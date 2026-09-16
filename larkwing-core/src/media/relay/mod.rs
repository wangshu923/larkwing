//! localhost 流转发:WebView 的 <audio>/<video> 不能直挂上游 CDN(防盗链要
//! Referer/UA,标签发不出去)→ 这里代发。两条路:
//!   /s/{token}  直转:上游头 + Range 透传(音频/单文件视频,拖进度条原生可用)
//!   /m/{token}  混流:B 站 DASH 音视频分离 → ffmpeg `-c copy` 拼成 fMP4 流(?t= 起播秒)
//! 只绑 127.0.0.1,token 随机不可猜;真相不落地 —— 注册表是纯瞬态,丢了重解析。
//!
//! 2026-09-16 按 §6.1 拆成兄弟文件(纯搬运,零行为改动):本文件留类型 / 注册表 / 常量(§4.11 单源在此)/
//! `Relay` 的起服务与 register_*;端点与机器按职责各归各家 —— `encoder`(H.264 编码器探测与参数)·
//! `upstream`(/s/ /dash/ 上游透传 + MPD 合成)· `file`(/f/ 本地文件 + Range)· `hls`(/hls/ 按需切片)·
//! `adaptive`(/la/ 音视频分离)· `ffmpeg`(段生成 / 命令构造 / 整段收集)· `progressive`(/m/ 渐进混流)·
//! `preview`(缩略图 / 雪碧图 / 封面)· `collect`(webrender 回传信箱)。兄弟之间互相看不见私有项,
//! 被跨文件调用的标 `pub(super)`、在下面汇一次;对外路径经 `pub use` 原样保留。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use axum::body::Body;
use axum::extract::{Path as AxPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use futures_util::TryStreamExt;
use sha2::Digest;
use tokio::io::AsyncReadExt;

use super::resolver::UpStream;
use super::SpriteSheet;
use crate::lockext::LockExt;

mod adaptive;
mod collect;
mod encoder;
mod ffmpeg;
mod file;
mod hls;
mod preview;
mod progressive;
mod upstream;

// 对外(media / 全 crate)的路径原样保留
pub use encoder::{video_encode_args, VideoEncoder};
pub use ffmpeg::{gen_video_init, gen_video_segment};
pub(crate) use ffmpeg::AUDIO_LOUDNESS_AF;
pub(crate) use preview::cover_jpeg;
// 兄弟文件之间的接缝:标了 pub(super) 的在这里汇一次,子文件 `use super::*` 即得(路由表照旧写裸名)
use adaptive::local_adaptive;
use collect::{collect, collect_preflight};
use encoder::{apply_video_encode, detect_video_encoder};
use ffmpeg::{build_audio_frag_cmd, build_frag_cmd, run_ffmpeg_collect};
use file::file;
use hls::hls;
use preview::{cover, thumb};
use progressive::remux;
use upstream::{build_mpd, dash, dash_preflight, direct, fetch_head};
// 搬过去的代码里写的是 `super::probe::…` / `super::no_console`(原先 relay 与它们同级):在这里把
// media 的这两个名字借进 relay,子文件的 `super::` 照旧解得出,搬运的行一字不改。
use super::{no_console, probe};

enum Entry {
    Direct(UpStream),
    Remux { video: UpStream, audio: UpStream, ffmpeg: PathBuf },
    /// 本地文件(含 NAS 挂载/UNC 路径):带 Range 的文件流,seek 白送。
    File(PathBuf),
    /// 本地文件但 WebView2 放不了(音轨 AC3/DTS、视频 HEVC、或容器是 mkv/avi;见 probe.rs):
    /// ffmpeg 单输入实时转封装/转码成 fMP4 —— **只转处理不了的那部分**:`transcode_audio` 真则
    /// 音轨转 AAC 否则 `-c:a copy`;`transcode_video` 真则视频转 H.264(吃 CPU)否则 `-c:v copy`
    /// (不掉画质/CPU 近零)。两者皆假时也仍跑(给 mkv 这类只需"转封装成 mp4"的容器用)。走 /m/
    /// 通道(渐进流、无原生 seek,前端按 ?t= 换 src 重启),与 B 站 DASH 混流同播放路径,前端零改。
    FileRemux {
        path: PathBuf,
        ffmpeg: PathBuf,
        transcode_video: bool,
        transcode_audio: bool,
        enc: VideoEncoder,
        /// 选中的音轨(0 起,`-map 0:a:{n}`;单音轨恒 0)。
        audio_track: usize,
    },
    /// B 站 DASH:两条独立自适应流(video.m4s + audio.m4s)。**不混流** —— 合成一份 DASH MPD,
    /// 前端 shaka 经 MSE 把两条喂给播放器、播放器自己管时间轴 → 原生 seek、天生同步(像 b 站网页)。
    /// `/dash/{token}/manifest.mpd` 返回 `mpd`;`/dash/{token}/v|a` 把 shaka 的 Range 请求带防盗链
    /// 头透传到对应上游(复用 proxy_upstream)。修「混流 + ?t= 重启 seek」的音画错位(那是固有缺陷)。
    Dash { mpd: String, video: UpStream, audio: UpStream },
    /// 本地不兼容文件(HEVC/AC3/mkv)走 **HLS 按需切片(fMP4 段)**(Stage 2,取代 FileRemux 的 /m/
    /// 渐进流):`/hls/{token}/index.m3u8` 由 `duration` 合成完整 VOD 播放列表(全片段都列出 + 共享
    /// `EXT-X-MAP:init.mp4` → shaka 知道完整时间轴、可任意 seek);`/hls/{token}/init.mp4` 切 0.1s 取
    /// ftyp+moov;`/hls/{token}/s{N}.m4s` 按需 ffmpeg `-ss N*SEG -t SEG` 出**单 moof 分片**、剔除尾部
    /// mfra、把 tfdt 改成累计起点(probe::patch_segment_tfdt)→ 标准 fMP4-HLS。**段走 fMP4 而非 mpegts**:
    /// 实锤 mpegts 段经 shaka 的 mux.js transmux 视频会失败(code 3015/3016)→ 黑屏;fMP4 = B 站 DASH
    /// 已验通的同路、MSE 直吃。无临时目录/无会话(每段无状态),seek = shaka 请求目标段 → 现切现回。
    /// 段一律转码视频 + 下混立体声 AAC(见 build_frag_cmd 三处实证),故无 transcode_* 旋钮。
    /// `enc` = 视频编码器(硬件/软件,注册时定死 → init 与各段同编码器,avcC 一致可拼)。
    FileHls { path: PathBuf, ffmpeg: PathBuf, duration: f64, enc: VideoEncoder, audio_track: usize },
    /// 本地不兼容文件的**音视频分离**自适应播放(0.2.6 治本):前端手写 MSE 两条 SourceBuffer —
    /// 视频按需分段(`copy_video` 决定 `-c:v copy` 省 CPU 还是转 H.264)、音频**离散段 + 左预卷**
    /// (前端 appendWindow 裁掉 priming → gapless 无漂移)。端点(`/la/{token}/…`):
    /// `desc`(JSON:两轨 mime + 视频段清单 + 音频网格/预卷 + 时长)/`vinit`+`v{N}`(视频 init/段)/
    /// `ainit`+`a{N}`(音频 init/段,离散完整响应——WebView2 收不下流式 body,故不流式)。`video_init`
    /// 在注册时生成一次并缓存(顺带解出 `video_mime` 的精确 codec 串);段无状态、现切现回。
    FileAdaptive {
        path: PathBuf,
        ffmpeg: PathBuf,
        copy_video: bool,
        /// 音频原样搬(P3「原声优先」):`-c:a copy`,不过响度滤镜、不重编 —— 原声道原动态。
        /// 只在「浏览器解得动 + 单/双声道 + AAC」三条全过时为真(见 capability::audio_track_plan)。
        copy_audio: bool,
        /// 视频编码器(转码段用;copy 段不理会)。与 `video_init` 同编码器,故段的 avcC 与 init 一致。
        enc: VideoEncoder,
        /// 完整 MSE type:`video/mp4; codecs="avc1.xxxxxx"`(从视频 init 的 avcC 解出)。
        video_mime: String,
        /// 缓存的视频 init(ftyp+moov),vinit 端点直接回它(跨段 codec 配置一致)。
        video_init: Vec<u8>,
        /// 视频段计划 `(start, dur)`:copy=关键帧对齐变长,转码=固定 6s。
        segments: Vec<(f64, f64)>,
        duration: f64,
        /// 选中的音轨(0 起;音频段/init 都按它 `-map`)。
        audio_track: usize,
        /// 字幕来源清单(P4):内嵌轨号或外挂文件,按需转 WebVTT 从 `/la/{token}/sub{N}.vtt` 出。
        subs: Vec<SubSource>,
    },
    /// 进度条 hover 预览缩略图:`/thumb/{token}?t=秒` 现抽现回一张 JPEG。**刻意与四条播放臂分开**
    /// 注册(不给 File/FileRemux/FileHls/FileAdaptive 各加一个字段):预览与播放是两件事,各自
    /// 一个 token 更好读、也让「这片有没有缩略图」只有一个信号(`NowPlaying.thumb_url` 有无)。
    /// 本地文件专用 —— 网络流手里只有远端 URL,没帧可抽,它们的预览走 `Sprites`。
    Thumb { path: PathBuf, ffmpeg: PathBuf },
    /// 网络源的进度条预览:平台**预生成的雪碧图**(B 站 `player/videoshot`,见 `MediaSource::sprites`)。
    /// **复用同一个 `/thumb/{token}?t=` 端点、同一个前端契约**(回一张 JPEG):按 t 在 `index` 里定帧
    /// → 下载所在那张大图(整图进有界缓存,一张图管几十格)→ 裁出那一格 → 缩到 THUMB_WIDTH → JPEG。
    /// 裁好的格照旧进 `thumbs` 缓存(键 = (token, 帧号))。任何一步不顺一律 404,前端照旧降级。
    Sprites { sheet: Arc<SpriteSheet> },
    /// 封面(专辑图 / 视频封面):`/cover/{token}` 现取现回一张 ≤ `COVER_MAX_EDGE` 的 JPEG。与播放条目
    /// 分开注册(缩略图同款理由):「有没有封面」就是 `NowPlaying.cover_url` 有没有值。三种来源见 `CoverSrc`;
    /// 取不出一律 404,前端回落 ♪ 占位。
    Cover(CoverSrc),
}

impl Entry {
    /// 这个 entry 常驻堆上多少字节(粗估,只为 `StreamRegistry` 的淘汰排序用,不求精确)。
    /// 绝大多数臂只有几个 PathBuf / String(几百字节,统一记 `BASE`);真占地方的只有
    /// `FileAdaptive.video_init`(整份 moov)、`Dash.mpd` 与雪碧图的帧索引。
    fn weight(&self) -> usize {
        /// 每个 entry 的固定开销(HashMap 槽 + token String + 几个 PathBuf)的量级。
        const BASE: usize = 256;
        BASE + match self {
            Entry::FileAdaptive { video_init, segments, subs, .. } => {
                video_init.len()
                    + segments.len() * std::mem::size_of::<(f64, f64)>()
                    + subs.len() * std::mem::size_of::<SubSource>()
            }
            Entry::Dash { mpd, .. } => mpd.len(),
            // sheet 是 Arc:多个 entry 可能共享同一份,重复记一点无害(宁可高估早淘汰)。
            Entry::Sprites { sheet } => {
                sheet.index.len() * std::mem::size_of::<f64>() + sheet.images.len() * 64
            }
            _ => 0,
        }
    }
}

/// 封面从哪来(三条路同一个端点、同一个前端契约)。
#[derive(Debug, Clone)]
pub enum CoverSrc {
    /// 音频文件内嵌图(mp3 APIC / flac PICTURE / m4a covr…):ffmpeg 把 attached_pic 流 `-c copy` 原字节抽出来。
    Embedded { path: PathBuf, ffmpeg: PathBuf },
    /// 同目录侧车图(cover / folder / front / album.*)。
    Sidecar(PathBuf),
    /// 远端图片(网络源封面,B 站视频封面):带防盗链头下载(雪碧图同款)。
    Remote { url: String, headers: Vec<(String, String)> },
}

/// 一条字幕的来源(P4):要么是文件内嵌的第 n 条字幕轨,要么是旁边的外挂文件。
/// 两者转 WebVTT 的命令只差输入,故合成一个类型、端点一视同仁。
#[derive(Debug, Clone)]
pub enum SubSource {
    /// 内嵌:`-map 0:s:{n}`。
    Embedded(usize),
    /// 外挂文件(`片子.chi.srt` 这类)。
    Sidecar(PathBuf),
}

/// HLS 切片时长(秒):段越短 seek 越细但请求越多;6s 是常见折中。自适应路(mod.rs)也用它当段目标。
pub(crate) const HLS_SEG: f64 = 6.0;
/// 自适应音频段时长(秒,固定网格 —— 音频无关键帧约束,任意帧可切)。
const AUDIO_SEG: f64 = 6.0;
/// 音频段左侧预卷(秒):切段时多切这么一段在前面,前端用 `appendWindowStart` 把它连同 AAC
/// 编码器 priming(~43ms 静音)一起裁掉 → 逐段独立编码也**无累计漂移**(gapless)。0.5s 足够盖住 priming。
const AUDIO_PREROLL: f64 = 0.5;
/// **copy 音频段的精切余量(秒,§4.11 单源在此)**:输入侧 `-ss` 是容器级 seek、落到前一个
/// **视频**关键帧,而 `-c:a copy` 不解码 → 早出来的那截不会被裁(见 `build_audio_frag_cmd`)。
/// 故粗切只切到 `ss − 这个值`,余下交给输出侧 `-ss` 按时间戳精确丢弃(音频每包都是同步点)。
/// 取小:丢弃量由关键帧间距决定、与本值无关,本值只影响粗切落点离目标多近。
const AUDIO_FINE_SEEK: f64 = 0.1;
/// ffmpeg stderr 的有界收集上限(只为拿错误尾巴记日志)。有界是因为 stderr 现在**并发排空**:
/// 排空是为了防挂死,但不能反过来让一个疯狂刷 stderr 的进程把内存吃掉。
const STDERR_TAIL_CAP: usize = 8 * 1024;
/// 上游流头(探 sidx 的前 96KB)的取回超时。relay 的 `net::Client` 刻意只设建连超时(它同时服务
/// 整片的 `/s/` 流,不能加总超时),所以这两趟小请求各自套一把 —— 不然 CDN 建连后不吐字节,
/// `register_dash` 永挂、`play()` 整个不返回(2026-09-16 体检修;§4.11 用户同日拍板按现值确认)。
const UPSTREAM_HEAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// HLS 播放列表的段数上限(≈ 33 小时 @6s)。存在的理由不是「片子不会更长」,而是 duration
/// 来自**文件自报**的元数据:坏文件能报出天文数字,而列表是照着它逐段拼字符串的。
const HLS_MAX_SEGMENTS: u64 = 20_000;

/* ——— 进度条 hover 缩略图的四道闸(单源在此;要调回来问,§4.11)———
 * 抽一帧本身很快(输入 seek 直落关键帧再解一帧),贵的是**次数** —— 光标横穿一条进度条能
 * 划过上千个像素。所以量化 + 缓存 + 串行 + 超时四道一起上,少一道就会在弱机上烧 CPU。 */
/// 时间量化格(秒):hover 时间先落格再取图 —— 同格不换 URL,光标在几十像素内抖动零请求。
/// 10s 与视频网站雪碧图的常见间隔同量级;副作用是缩略图最多"旧" 10s,预览够用。
const THUMB_GRID: f64 = 10.0;
/// 缩略图缓存上限(张)。**全局有界、不按 token 分**:挂在 token 下的缓存会随播放次数一起长;
/// 全局 FIFO 才封得住。48 张 × ~20KB ≈ 1MB。(`streams` 注册表自己的界见 `STREAMS_MAX_*`。)
const THUMB_CACHE_MAX: usize = 48;
/// 单张缩略图字节上限(抽出来的 JPEG 约 10–30KB;超这个数说明参数不对,别把它收进内存)。
const THUMB_MAX_BYTES: usize = 512 * 1024;
/// 单帧超时:4K HEVC 在弱机上解一帧也就几百毫秒,8s 是"这台机器抽不出来"的 backstop
/// (超时即放弃,child 由 kill_on_drop 收尸)。
const THUMB_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);
/// 缩略图宽度(像素,高度按比例):进度条上方的小图,192 在窄框里也够看清是哪一场戏。
/// 雪碧图路同一口径:原格比它宽才缩(HD 480 → 192),比它窄(SD 160)原样给、不放大。
pub(crate) const THUMB_WIDTH: u32 = 192;

/* ——— 雪碧图路(网络源预览)自己的两道闸(单源在此;**§4.11 待用户确认**)———
 * 雪碧图一张大图装 100 格,贵的不是格而是**整图下载 + 解码**(HD 4800×2700 JPEG ≈ 400KB,
 * 解码 ≈ 39MB 位图、百毫秒级);所以整图缓存 + 上面那套 thumbs 缓存 / 串行闸 / 超时一起用。 */
/// 雪碧图整图缓存上限(张,全局 FIFO,与 thumbs 同理不按 token 分)。一部两小时电影约 10 张
/// 大图;16 张够一部片来回拖,最坏(全 HD)≈ 16 × 400KB ≈ 6MB 编码字节(只存编码字节不存位图)。
const SPRITE_IMG_CACHE_MAX: usize = 16;
/// 单张雪碧图字节上限:HD 一张 ≈ 400KB,8MB 是「这不是雪碧图」的 backstop(边下边数,超了就弃)。
const SPRITE_IMG_MAX_BYTES: usize = 8 * 1024 * 1024;
/// 裁出来那一格的 JPEG 质量(小图,80 已看不出差别;ffmpeg 路的 mjpeg 默认质量同量级)。
const SPRITE_JPEG_QUALITY: u8 = 80;

/* ——— 封面(专辑图 / 视频封面)的几个数(§4.11 用户拍板 2026-09-07:512 / q85 / 缓存 16;原图上限沿雪碧图)——— */
/// 封面最长边(像素):播放条缩略 40px、「正在播放」大卡约 200px,512 留足 2× 屏,一张 ≈ 30–60KB。
pub(crate) const COVER_MAX_EDGE: u32 = 512;
/// 封面 JPEG 质量。
const COVER_JPEG_QUALITY: u8 = 85;
/// 封面缓存上限(张,全局 FIFO;键 = 来源身份〔文件路径 / 图 URL〕,同一首再放命中缓存不重抽)。
const COVER_CACHE_MAX: usize = 16;
/// 原图字节上限(内嵌 PNG 封面几 MB 常见;8MB 是「这不是封面」的 backstop,与雪碧图同数)。
const COVER_SRC_MAX_BYTES: usize = 8 * 1024 * 1024;
/// 原图边长上限(解码器限额;封面不该是几亿像素的图,超过不解)。
const COVER_SRC_MAX_EDGE: u32 = 8192;

/// 有界字节缓存:全局 FIFO(不做真 LRU —— 拖进度条是顺序扫过去的,FIFO 与 LRU 的命中率差异
/// 在这个场景可以忽略,换来零依赖零复杂度)。两处实例:裁好的缩略图(键 (token, 格/帧))与
/// 雪碧图整图(键 = 大图 URL),各自一个上限。
struct FifoCache<K> {
    cap: usize,
    map: HashMap<K, Arc<Vec<u8>>>,
    /// 插入顺序,超上限从头淘汰。
    order: std::collections::VecDeque<K>,
}

/* ——— `streams` 注册表的两道界(单源在此;§4.11 用户拍板 2026-09-08「合理就可以」= 现值确认)———
 * 从前这张表**只增不减**:每次点播留 1–3 个 entry(播放臂 + 缩略图/雪碧图 + 封面),切集、
 * 切音轨、重放各再留一份;连播一季或歌单循环几天就是几十上百个常驻。轻的 entry 只是几个
 * PathBuf 无所谓,真占地方的是 `FileAdaptive.video_init`(整份 ftyp+moov,4K 长片能到几 MB)。
 * 所以按**字节权重**淘汰而不是按条数:`attachment_url` 也在这张表里注册(聊天历史每张图
 * 一个 `Entry::File`,一条 ~100 字节),按条数一刀切会把老图的 URL 挤成 404、图卡破图。 */
/// 常驻字节上限:超了就从最老的开始淘汰(FIFO —— 播放是顺序往前走的,老 entry 播完就没人请求)。
const STREAMS_MAX_BYTES: usize = 64 * 1024 * 1024;
/// 条数 backstop:字节权重估不准的那类(全是小 entry)也不能无限堆。500 条小 entry ≈ 几十 KB,
/// 而一屏聊天最多 200 张图 —— 留足余量,正常用永远碰不到,只防「翻了几十个会话」的累积。
const STREAMS_MAX_ENTRIES: usize = 500;
/// 淘汰的保底:无论权重多大,最近这么多条一律留着。防「几个巨型 adaptive 就撑爆上限、
/// 把**正在播的那条**也淘汰掉」——正在播的永远在最近几条里。
/// **保底优先于上面两道界**:全是巨型 entry 时宁可字节略微超标,也不许淘汰最近这几条。
const STREAMS_MIN_KEEP: usize = 8;

/// token → entry 的注册表,带 FIFO 有界淘汰(界见上面三个常量)。
#[derive(Default)]
struct StreamRegistry {
    map: HashMap<String, Arc<Entry>>,
    /// 注册顺序,淘汰从头开始。
    order: std::collections::VecDeque<String>,
    /// `map` 里各 entry 的粗估权重之和(见 `Entry::weight`)。
    bytes: usize,
}

impl StreamRegistry {
    fn insert(&mut self, token: String, entry: Arc<Entry>) {
        self.bytes = self.bytes.saturating_add(entry.weight());
        if let Some(old) = self.map.insert(token.clone(), entry) {
            // token 是 sha256 派生、实际不会撞;真撞了也要把旧权重扣回去,别让 bytes 漂。
            self.bytes = self.bytes.saturating_sub(old.weight());
        } else {
            self.order.push_back(token);
        }
        while self.map.len() > STREAMS_MIN_KEEP
            && (self.bytes > STREAMS_MAX_BYTES || self.map.len() > STREAMS_MAX_ENTRIES)
        {
            let Some(oldest) = self.order.pop_front() else { break };
            if let Some(dropped) = self.map.remove(&oldest) {
                self.bytes = self.bytes.saturating_sub(dropped.weight());
            }
        }
    }

    fn get(&self, token: &str) -> Option<Arc<Entry>> {
        self.map.get(token).cloned()
    }
}

/// 裁好的缩略图缓存(本地 ffmpeg 抽的帧 / 雪碧图裁的格,同一份)。
type ThumbCache = FifoCache<(String, u64)>;

impl Default for ThumbCache {
    fn default() -> Self {
        FifoCache::new(THUMB_CACHE_MAX)
    }
}

impl<K: std::hash::Hash + Eq + Clone> FifoCache<K> {
    fn new(cap: usize) -> Self {
        FifoCache { cap, map: HashMap::new(), order: std::collections::VecDeque::new() }
    }

    fn get(&self, key: &K) -> Option<Arc<Vec<u8>>> {
        self.map.get(key).cloned()
    }

    fn put(&mut self, key: K, bytes: Arc<Vec<u8>>) {
        if self.map.insert(key.clone(), bytes).is_none() {
            self.order.push_back(key);
        }
        while self.order.len() > self.cap {
            if let Some(old) = self.order.pop_front() {
                self.map.remove(&old);
            }
        }
    }
}

struct Inner {
    port: u16,
    /// token → entry;**有界**(见 `StreamRegistry` 与 `STREAMS_MAX_*`)。
    streams: Mutex<StreamRegistry>,
    net: crate::net::Client,
    counter: AtomicU64,
    /// 探出来的视频编码器(硬件优先),整进程探一次缓存;转码点复用,免每次试编码。
    hw_encoder: tokio::sync::OnceCell<VideoEncoder>,
    /// webrender 回传信箱(`POST /collect/{token}`,一次性):壳层隐藏窗里注入的脚本把
    /// 渲染后页面 POST 回来。token 用完即取走;窗超时收摊后残留项由发起方 drop Receiver 自清。
    collect: Mutex<HashMap<String, tokio::sync::oneshot::Sender<String>>>,
    /// hover 缩略图缓存(全局有界 FIFO,见 `ThumbCache`)。
    thumbs: Mutex<ThumbCache>,
    /// 雪碧图整图缓存(编码字节,键 = 大图 URL;全局有界 FIFO,见 `SPRITE_IMG_CACHE_MAX`)。
    sprites: Mutex<FifoCache<String>>,
    /// 封面缓存(归一后的 JPEG,键 = 来源身份;全局有界 FIFO,见 `COVER_CACHE_MAX`)。
    covers: Mutex<FifoCache<String>>,
    /// 缩略图串行闸:同时只抽一帧。拖一趟进度条会连着来好几格,并发起 ffmpeg 只是互相抢 CPU;
    /// 排队 + 前端只保留最新一次请求 = 最多积一个(拿到许可后还要再查一次缓存,别白抽)。
    /// 雪碧图路同闸:并发的两格多半落同一张大图,串行后第二个直接命中整图缓存、不重复下载。
    thumb_gate: tokio::sync::Semaphore,
}

#[derive(Clone)]
pub struct Relay {
    inner: Arc<Inner>,
}

impl Relay {
    /// 起服务(随机端口,只绑回环)。app 生命周期内常驻,不做优雅停机 —— 进程没了它就没了。
    pub async fn start() -> Result<Relay> {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .context("转发服务绑不上回环端口")?;
        let port = listener.local_addr()?.port();
        let inner = Arc::new(Inner {
            port,
            streams: Mutex::new(StreamRegistry::default()),
            // 上游是大文件流:只设建连超时,不设整体超时(空闲保护靠链路自身断流)。
            // 走统一 net::Client(CLAUDE.md §5):墙内 CDN 直连优先永不代理,未来墙外源(YouTube 等)直连失败自动落代理。
            net: crate::net::Client::new(|b| b.connect_timeout(std::time::Duration::from_secs(10))),
            counter: AtomicU64::new(1),
            hw_encoder: tokio::sync::OnceCell::new(),
            collect: Mutex::new(HashMap::new()),
            thumbs: Mutex::new(ThumbCache::default()),
            sprites: Mutex::new(FifoCache::new(SPRITE_IMG_CACHE_MAX)),
            covers: Mutex::new(FifoCache::new(COVER_CACHE_MAX)),
            thumb_gate: tokio::sync::Semaphore::new(1),
        });
        let app = Router::new()
            .route("/s/{token}", get(direct))
            .route("/m/{token}", get(remux))
            .route("/f/{token}", get(file))
            // DASH:manifest + 两路段透传。shaka 用 fetch() 拉(跨源 → 需 CORS,见 dash 处理)。
            .route("/dash/{token}/{seg}", get(dash).options(dash_preflight))
            // 本地 HLS:m3u8 + 按需切片(同样 shaka fetch 跨源 → CORS)。
            .route("/hls/{token}/{seg}", get(hls).options(dash_preflight))
            // 本地自适应(音视频分离,手写 MSE):desc/vinit/v{N}/audio(前端 fetch 跨源 → CORS)。
            .route("/la/{token}/{seg}", get(local_adaptive).options(dash_preflight))
            // 进度条 hover 预览缩略图:`?t=秒` 现抽现回一张 JPEG(前端 <img>,不查 CORS → 不用放行)。
            .route("/thumb/{token}", get(thumb))
            // 封面(专辑图 / 视频封面):现取现回一张 ≤512px 的 JPEG(同样 <img> 用,不查 CORS)。
            .route("/cover/{token}", get(cover))
            // webrender 回传(壳层隐藏窗注入脚本 → 任意外源页面 fetch 过来 → 需 CORS;
            // 脚本用 text/plain 发 = 简单请求免预检,OPTIONS 只是兜底)。
            .route("/collect/{token}", axum::routing::post(collect).options(collect_preflight))
            .with_state(inner.clone());
        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                tracing::error!("媒体转发服务挂了: {e}");
            }
        });
        tracing::info!(port, "媒体转发服务在线");
        Ok(Relay { inner })
    }

    fn token(&self) -> String {
        // 不可猜即可(只绑回环):纳秒 + 自增量过一遍 sha256
        let n = self.inner.counter.fetch_add(1, Ordering::Relaxed);
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let digest = sha2::Sha256::digest(format!("{n}:{t}:larkwing-relay").as_bytes());
        digest.iter().take(12).map(|b| format!("{b:02x}")).collect()
    }

    fn register(&self, entry: Entry, path: &str) -> String {
        let token = self.token();
        self.inner
            .streams
            .lk()
            .insert(token.clone(), Arc::new(entry));
        format!("http://127.0.0.1:{}/{path}/{token}", self.inner.port)
    }

    /// webrender 回传信箱:一次性 token → (POST 地址, 接收端)。壳层把地址嵌进注入脚本,
    /// 页面 fetch POST 回来即投递。发起方超时放弃 = drop Receiver;这类死项(页面一直没 POST)
    /// 在下次 register 时按 `is_closed` 清扫 —— 不随时间堆积。
    pub fn register_collect(&self) -> (String, tokio::sync::oneshot::Receiver<String>) {
        let token = self.token();
        let (tx, rx) = tokio::sync::oneshot::channel();
        {
            let mut map = self.inner.collect.lk();
            map.retain(|_, s| !s.is_closed());
            map.insert(token.clone(), tx);
        }
        (format!("http://127.0.0.1:{}/collect/{token}", self.inner.port), rx)
    }

    /// 单流直转 URL。
    pub fn register_direct(&self, up: UpStream) -> String {
        self.register(Entry::Direct(up), "s")
    }

    /// 双流混流 URL(视频在前音频在后,resolver 的约定顺序)。
    pub fn register_remux(&self, video: UpStream, audio: UpStream, ffmpeg: PathBuf) -> String {
        self.register(Entry::Remux { video, audio, ffmpeg }, "m")
    }

    /// 本地文件 URL(原生 Range 直传)。
    pub fn register_file(&self, path: PathBuf) -> String {
        self.register(Entry::File(path), "f")
    }

    /// 进度条 hover 缩略图的**基址**(`…/thumb/{token}`):前端自己拼 `?t=秒`。与播放条目分开注册,
    /// 播放走哪条路(直传/自适应/HLS/渐进)都是同一套预览。ffmpeg 必须**已经在手**(调用方用
    /// `Components::ready` 拿,不为预览图拉下载)—— 所以「有没有缩略图」就是 `thumb_url` 有没有值。
    pub fn register_thumbs(&self, path: PathBuf, ffmpeg: PathBuf) -> String {
        self.register(Entry::Thumb { path, ffmpeg }, "thumb")
    }

    /// 网络源雪碧图的预览基址(同样是 `…/thumb/{token}`,前端拼 `?t=秒`,与本地路一个契约)。
    /// 大图不在这里下 —— 首次 hover 到某张图时才下、进 `sprites` 缓存;注册本身零 IO。
    pub fn register_sprites(&self, sheet: Arc<SpriteSheet>) -> String {
        self.register(Entry::Sprites { sheet }, "thumb")
    }

    /// 封面地址(`…/cover/{token}`):注册零 IO,首次请求才抽 / 读 / 下,归一成 ≤512px JPEG 进缓存。
    /// 有没有封面由调用方决定(内嵌图探到了 / 侧车图在 / 源给了封面 URL 才注册),这里不猜。
    pub fn register_cover(&self, src: CoverSrc) -> String {
        self.register(Entry::Cover(src), "cover")
    }

    /// 探/取该机器可用的视频编码器(硬件优先,整进程探一次缓存)。转码前调,拿来定 entry 的 `enc`。
    pub async fn video_encoder(&self, ffmpeg: &Path) -> VideoEncoder {
        *self.inner.hw_encoder.get_or_init(|| detect_video_encoder(ffmpeg)).await
    }

    /// 本地文件、ffmpeg 转封装/转码后的混流 URL(走 /m/ 通道,与 register_remux 同播放路径)。
    /// `transcode_video`/`transcode_audio` 各自决定该轨 copy 还是转码(按 probe 结论,只转不兼容的)。
    pub async fn register_file_remux(
        &self,
        path: PathBuf,
        ffmpeg: PathBuf,
        transcode_video: bool,
        transcode_audio: bool,
        force_software: bool,
        audio_track: usize,
    ) -> String {
        let enc = if force_software {
            VideoEncoder::Software
        } else {
            self.video_encoder(&ffmpeg).await
        };
        self.register(
            Entry::FileRemux { path, ffmpeg, transcode_video, transcode_audio, enc, audio_track },
            "m",
        )
    }

    /// B 站 DASH:**不混流**。探两条流的 sidx → 合成 MPD → 注册,返回 `…/dash/{token}/manifest.mpd`。
    /// 前端 shaka 经 MSE 播这份 manifest → 播放器管时间轴、原生 seek、音画同步(治混流重启 seek 的错位)。
    /// 探不到 sidx(流不是 SegmentBase 单文件 DASH,头里没有)→ Err,调用方回落混流。
    pub async fn register_dash(
        &self,
        video: UpStream,
        audio: UpStream,
        duration: f64,
    ) -> Result<String> {
        // sidx 在 ftyp+moov 之后;DASH 单表示流的 moov 很小,96KB 头足够覆盖。
        const HEAD: u64 = 96 * 1024;
        // 两条流头**并行**取、各带超时:串行白等一趟 RTT;不带超时则 CDN 建连后不吐字节时
        // `play()` 整个不返回(relay 的 net::Client 刻意只设建连超时,见 UPSTREAM_HEAD_TIMEOUT)。
        let (vhead, ahead) = tokio::join!(
            tokio::time::timeout(UPSTREAM_HEAD_TIMEOUT, fetch_head(&self.inner.net, &video, HEAD)),
            tokio::time::timeout(UPSTREAM_HEAD_TIMEOUT, fetch_head(&self.inner.net, &audio, HEAD)),
        );
        let vhead = vhead.ok().flatten().context("取视频流头失败或超时")?;
        let ahead = ahead.ok().flatten().context("取音频流头失败或超时")?;
        let vsidx = super::probe::probe_sidx(&vhead)
            .context("视频流头里没有 sidx(非 SegmentBase 单文件 DASH)")?;
        let asidx = super::probe::probe_sidx(&ahead).context("音频流头里没有 sidx")?;
        let mpd = build_mpd(duration, &video, vsidx, &audio, asidx);
        let token = self.token();
        self.inner
            .streams
            .lk()
            .insert(token.clone(), Arc::new(Entry::Dash { mpd, video, audio }));
        Ok(format!("http://127.0.0.1:{}/dash/{token}/manifest.mpd", self.inner.port))
    }

    /// 本地不兼容文件走 HLS 按需切片(Stage 2)。注册返回 `…/hls/{token}/index.m3u8`(前端 shaka
    /// 经 manifest_url 播,自动认 HLS)。duration 来自 probe(mvhd / ffmpeg -i);切片按需现切(见 hls)。
    pub async fn register_file_hls(
        &self,
        path: PathBuf,
        ffmpeg: PathBuf,
        duration: f64,
        force_software: bool,
        audio_track: usize,
    ) -> String {
        let enc = if force_software {
            VideoEncoder::Software
        } else {
            self.video_encoder(&ffmpeg).await
        };
        let token = self.token();
        self.inner.streams.lk().insert(
            token.clone(),
            Arc::new(Entry::FileHls { path, ffmpeg, duration, enc, audio_track }),
        );
        format!("http://127.0.0.1:{}/hls/{token}/index.m3u8", self.inner.port)
    }

    /// 注册音视频分离自适应播放,返回 `…/la/{token}/desc`(前端手写 MSE 据此起播)。
    /// `video_init` 由 `gen_video_init` 预生成(顺带解出 codec 串填 `video_mime`)。
    #[allow(clippy::too_many_arguments)]
    pub fn register_file_adaptive(
        &self,
        path: PathBuf,
        ffmpeg: PathBuf,
        copy_video: bool,
        copy_audio: bool,
        video_mime: String,
        video_init: Vec<u8>,
        segments: Vec<(f64, f64)>,
        duration: f64,
        enc: VideoEncoder,
        audio_track: usize,
        subs: Vec<SubSource>,
    ) -> String {
        let token = self.token();
        self.inner.streams.lk().insert(
            token.clone(),
            Arc::new(Entry::FileAdaptive {
                path,
                ffmpeg,
                copy_video,
                copy_audio,
                enc,
                video_mime,
                video_init,
                segments,
                duration,
                audio_track,
                subs,
            }),
        );
        format!("http://127.0.0.1:{}/la/{token}/desc", self.inner.port)
    }
}

fn lookup(state: &Inner, token: &str) -> Option<Arc<Entry>> {
    state.streams.lk().get(token)
}

fn bad(status: StatusCode) -> Response {
    Response::builder().status(status).body(Body::empty()).expect("static response")
}

/// 带 CORS 的整块字节响应(HLS 的 manifest/init/段都被 shaka 跨源 fetch,需放行)。
fn bytes_response(body: Vec<u8>, content_type: &'static str) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", content_type)
        .header("access-control-allow-origin", "*")
        .body(Body::from(body))
        .unwrap_or_else(|_| bad(StatusCode::INTERNAL_SERVER_ERROR))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_unique_and_urls_local() {
        let inner = Arc::new(Inner {
            port: 12345,
            streams: Mutex::new(StreamRegistry::default()),
            net: crate::net::Client::new(|b| b),
            counter: AtomicU64::new(1),
            hw_encoder: tokio::sync::OnceCell::new(),
            collect: Mutex::new(HashMap::new()),
            thumbs: Mutex::new(ThumbCache::default()),
            sprites: Mutex::new(FifoCache::new(SPRITE_IMG_CACHE_MAX)),
            covers: Mutex::new(FifoCache::new(COVER_CACHE_MAX)),
            thumb_gate: tokio::sync::Semaphore::new(1),
        });
        let relay = Relay { inner };
        let a = relay.register_direct(UpStream { url: "u1".into(), ..Default::default() });
        let b = relay.register_direct(UpStream { url: "u2".into(), ..Default::default() });
        assert_ne!(a, b);
        assert!(a.starts_with("http://127.0.0.1:12345/s/"));
        assert_eq!(relay.inner.streams.lk().map.len(), 2);
    }

    /// 注册表淘汰:轻 entry 按条数 backstop、重 entry 按字节,且**永不淘汰最近 MIN_KEEP 条**
    /// (正在播的那条恒在其中)。纯内存、不起服务。
    #[test]
    fn stream_registry_evicts_by_bytes_and_keeps_recent() {
        // ① 一堆轻 entry(聊天历史每张图一个 Entry::File):远不到字节上限,只受条数 backstop 管。
        //    这一条是「按条数一刀切会把老图挤成 404」的回归守卫:500 条以内一个都不许掉。
        let mut reg = StreamRegistry::default();
        for i in 0..STREAMS_MAX_ENTRIES {
            reg.insert(format!("t{i}"), Arc::new(Entry::File(PathBuf::from(format!("/x/{i}.png")))));
        }
        assert_eq!(reg.map.len(), STREAMS_MAX_ENTRIES, "没到条数上限,一条都不该淘汰");
        assert!(reg.get("t0").is_some(), "最老的图 URL 仍然有效");
        assert!(reg.bytes < STREAMS_MAX_BYTES, "轻 entry 的权重远够不上字节上限");
        // 再插一条 → 只淘汰最老的那一条
        reg.insert("extra".into(), Arc::new(Entry::File(PathBuf::from("/x/extra.png"))));
        assert_eq!(reg.map.len(), STREAMS_MAX_ENTRIES);
        assert!(reg.get("t0").is_none(), "超出条数上限,最老的被淘汰");
        assert!(reg.get("extra").is_some());

        // ② 重 entry(FileAdaptive 缓存整份 moov):字节上限先到,老的被淘汰、最新的必须活着。
        let mut reg = StreamRegistry::default();
        let heavy = |mb: usize| {
            Arc::new(Entry::FileAdaptive {
                path: PathBuf::from("/x/m.mkv"),
                ffmpeg: PathBuf::from("ffmpeg"),
                copy_video: true,
                copy_audio: true,
                enc: VideoEncoder::Software,
                video_mime: "video/mp4".into(),
                video_init: vec![0u8; mb * 1024 * 1024],
                segments: vec![(0.0, 6.0); 100],
                duration: 600.0,
                audio_track: 0,
                subs: Vec::new(),
            })
        };
        for i in 0..40 {
            reg.insert(format!("h{i}"), heavy(8)); // 40 × 8MB = 320MB 远超 64MB 上限
        }
        // 压到保底条数为止(**保底优先于字节上限**:全是巨型 entry 时宁可略微超字节,
        // 也不能把最近这几条淘汰掉 —— 正在播的那条就在里面)。320MB → 8 条 ≈ 64MB。
        assert_eq!(reg.map.len(), STREAMS_MIN_KEEP, "重 entry 被压到保底条数");
        assert!(reg.get("h39").is_some(), "最新注册的(= 正在播的)必须还在");
        assert!(reg.get("h0").is_none(), "最老的重 entry 已被淘汰");

        // ③ MIN_KEEP 保底:单条就超上限时也不许把自己淘汰掉(否则正在播的当场 404)。
        let mut reg = StreamRegistry::default();
        reg.insert("only".into(), heavy(128)); // 一条就 128MB > 64MB 上限
        assert!(reg.get("only").is_some(), "最近 MIN_KEEP 条恒留,哪怕单条超限");
    }

    #[test]
    fn thumb_cache_evicts_oldest_beyond_cap() {
        let mut c = ThumbCache::default();
        for i in 0..(THUMB_CACHE_MAX as u64 + 3) {
            c.put(("tok".into(), i), Arc::new(vec![i as u8]));
        }
        assert_eq!(c.map.len(), THUMB_CACHE_MAX, "总量封在上限");
        assert!(c.get(&("tok".into(), 0)).is_none(), "最早的被淘汰");
        assert!(c.get(&("tok".into(), THUMB_CACHE_MAX as u64 + 2)).is_some(), "最新的还在");
        // 重复 put 同一格不该把队列撑长(否则反复 hover 同一处会把别的挤掉)
        let key = ("tok".to_string(), THUMB_CACHE_MAX as u64 + 2);
        c.put(key.clone(), Arc::new(vec![9]));
        assert_eq!(c.order.len(), THUMB_CACHE_MAX);
    }

    /// 雪碧图整图缓存:同一套 FIFO、自己的上限(16 张),键是大图 URL。
    #[test]
    fn sprite_image_cache_evicts_oldest_beyond_cap() {
        let mut c: FifoCache<String> = FifoCache::new(SPRITE_IMG_CACHE_MAX);
        for i in 0..(SPRITE_IMG_CACHE_MAX + 3) {
            c.put(format!("https://cdn/sheet-{i}.jpg"), Arc::new(vec![i as u8]));
        }
        assert_eq!(c.map.len(), SPRITE_IMG_CACHE_MAX, "总量封在上限");
        assert!(c.get(&"https://cdn/sheet-0.jpg".to_string()).is_none(), "最早的被淘汰");
        assert!(c.get(&format!("https://cdn/sheet-{}.jpg", SPRITE_IMG_CACHE_MAX + 2)).is_some());
        c.put(format!("https://cdn/sheet-{}.jpg", SPRITE_IMG_CACHE_MAX + 2), Arc::new(vec![7]));
        assert_eq!(c.order.len(), SPRITE_IMG_CACHE_MAX, "重复 put 不撑长队列");
    }
}
