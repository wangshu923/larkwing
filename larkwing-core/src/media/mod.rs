//! 影音运行时(PLAN §9):搜索(各源 API)→ 解析(yt-dlp)→ 转发/混流(relay)→
//! 事件推 UI。多源立场与 LLM 多供应商同构(宪法 §4):解析层 yt-dlp 天然多源,
//! 真正按源分化的只有**搜索**和**登录态**,接缝(`MediaSource` trait)就开在这;
//! 加源 = 加一个实现文件,工具面与模型无感知。MVP 只有 bilibili。

mod archive;
mod bilibili;
pub mod capability;
pub mod cookies;
mod download;
mod edit;
pub mod fingerprint;
mod introdetect;
mod lyrics;
mod probe;
mod relay;
mod resolver;
pub mod skip;
pub mod timeline;
mod torrent;
mod usage;
// 运行时按职责分家(2026-09-08;此前 `impl MediaRuntime` 是本文件里一个 1975 行的块)。
// 子模块能看见父模块的私有项 → 纯搬运;反过来不成立,故少数跨文件调用的 helper 标了 pub(super)。
mod control;
mod decode;
mod local;
mod play;
mod progress;
mod queue;
mod skipctl;

pub use archive::{ExtractOutcome, ZipOutcome};
pub use cookies::CookieRec;
pub use download::{DownloadOutcome, DownloadedAudio, TrackMeta};
pub use edit::{EditOutcome, EditRequest};
/// 「回合内等多久再转后台」单源(§4.11):ffmpeg/解压/扫盘/delegate 子回合共用一个 30。
pub(crate) use edit::IN_TURN_WAIT;
pub use torrent::{
    TorrentLink, TorrentOutcome, DEFAULT_ONLY_RE, MAX_CONCURRENT as TORRENT_MAX_CONCURRENT,
};
pub(crate) use lyrics::compose_batch_summary;
pub use lyrics::{LyricsBatchOutcome, LyricsFileResult, LyricsItem, LyricsResult};
pub(crate) use probe::probe_local; // fs_stat 白拿 BMFF 时长(免 ffmpeg 的轻量 moov 探测)
pub use usage::UsageOutcome;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Serialize;

use crate::bus::{AppEvent, Bus, MediaEvent, TaskRetry, Text};
use crate::components::{Component, Components, DEFAULT_GH_MIRRORS};
use crate::lockext::LockExt;
use crate::store::Store;
use crate::tasks::Tasks;

/// Windows 下给子进程加 CREATE_NO_WINDOW:主进程是 GUI 子系统(windows_subsystem="windows"),
/// 但它 spawn 的控制台程序(yt-dlp / ffmpeg)默认仍会弹一个黑框 —— 这里抑制掉。其它平台空操作。
/// 凡 media 里 spawn 子进程(resolver 的 yt-dlp、relay/edit/探测的 ffmpeg)都必须走这里。
fn no_console(cmd: &mut tokio::process::Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        // 注:CommandExt 作用于 std::process::Command,不是 tokio 的 ——
        // 经 as_std_mut() 拿底层 std command 设标志,spawn 时会被沿用。
        cmd.as_std_mut().creation_flags(CREATE_NO_WINDOW);
    }
    // macOS / Linux:GUI 进程 spawn 子进程不会凭空弹终端窗口,这里无事可做。
    // 钩子留着 —— 将来若要做别的平台级子进程加固(进程组 / niceness / 句柄继承收口),
    // 统一开在这个函数里,两处 spawn(resolver、relay)自动受益。
    #[cfg(not(windows))]
    let _ = cmd;
}

// ---------- 源接缝 ----------

/// 搜索命中(模型与播放卡片共用的形)。
#[derive(Debug, Clone, Serialize)]
pub struct MediaHit {
    pub url: String,
    pub title: String,
    pub author: String,
    pub duration_seconds: i64,
    pub source: String,
}

#[derive(Debug)]
pub enum SearchError {
    /// 412/403/-101 类风控:登录态能显著缓解 → UI 出扫码入口。
    RiskControl,
    Other(anyhow::Error),
}

/// 按源分化的两件事:搜索 + 登录态元数据。解析不在此 —— yt-dlp 统一吃页面 URL。
#[async_trait]
pub trait MediaSource: Send + Sync {
    fn id(&self) -> &'static str;
    /// 扫码登录页(壳层开窗口用)。
    fn login_url(&self) -> &'static str;
    /// 取 cookie 的域 URL(原生 CookieManager 按它查)。
    fn cookie_url(&self) -> &'static str;
    /// 判定"已登录"的关键 cookie 名。
    fn login_cookie(&self) -> &'static str;
    async fn search(
        &self,
        keyword: &str,
        limit: usize,
        cookie_header: Option<&str>,
    ) -> Result<Vec<MediaHit>, SearchError>;

    /// 发现「剧集队列」:给一个页面 URL,返回 `(series_key, 有序集列表)`;单个视频(非合集/分P)→ None。
    /// 按源分化(B 站走 view API 拿 分P/合集;别的源各自实现)。默认无 —— 未实现的源退化成单集。
    async fn episodes(
        &self,
        _page_url: &str,
        _cookie_header: Option<&str>,
    ) -> Result<Option<Series>> {
        Ok(None)
    }

    /// 进度条 hover 预览的**雪碧图**(平台预生成的缩略图拼版,B 站 = `player/videoshot`)。网络流
    /// 手里只有远端 URL、没帧可抽,预览图只能问平台要 —— 按源分化。**尽力件**:拿不到一律
    /// `Ok(None)`(= `NowPlaying.thumb_url` 为 None,前端只出时间气泡),绝不挡播放;调用方
    /// (`play_entry`)另套超时,不拖慢起播。默认无 —— 未实现的源没有预览图。
    async fn sprites(
        &self,
        _page_url: &str,
        _cookie_header: Option<&str>,
    ) -> Result<Option<SpriteSheet>> {
        Ok(None)
    }

    /// 平台标注的片头 / 片尾段(B 站番剧 playurl 的 clip_info_list,人工逐集标注、约六成集有)。
    /// **尽力件**:没有 / 拿不到一律 `Ok(vec![])`;调用方另套超时、与解析并行,不拖起播。
    /// 默认无 —— 未实现的源没有平台标注(靠章节 / 指纹 / 手标)。
    async fn skip_clips(
        &self,
        _page_url: &str,
        _cookie_header: Option<&str>,
    ) -> Result<Vec<skip::AutoSeg>> {
        Ok(Vec::new())
    }
}

/// 网络源的进度条预览素材:平台**预生成的雪碧图** —— 若干张大图,每张按行优先铺 `x_len × y_len`
/// 格,一格 = 一帧缩略图;`index` 是各帧的采样时刻。由 `MediaSource::sprites` 产出、relay 消费
/// (`Entry::Sprites`:`/thumb/{token}?t=秒` 定格 → 下载所在大图 → 裁一格 → JPEG),前端契约与
/// 本地 ffmpeg 抽帧完全一致(`thumb_url` + `?t=` 回一张 JPEG),前端零改。
#[derive(Debug, Clone, PartialEq)]
pub struct SpriteSheet {
    /// 每张大图横向格数。
    pub x_len: u32,
    /// 每张大图纵向格数。
    pub y_len: u32,
    /// 每格像素宽。
    pub tile_w: u32,
    /// 每格像素高。
    pub tile_h: u32,
    /// 大图地址(协议已补全);第 j 张装第 `j × x_len × y_len` 帧起的格。
    pub images: Vec<String>,
    /// 采样时刻表(秒):`index[k]` = 第 k 帧(全局帧号,跨大图连续)对应视频的第几秒,升序。
    /// 长度 = 帧数(源解析时已把平台自己的哨兵项剥掉,见 `bilibili::parse_videoshot`)。
    pub index: Vec<f64>,
    /// 下载大图要带的头(UA / Referer 防盗链),同 `UpStream.headers` 的用法。
    pub headers: Vec<(String, String)>,
}

// ---------- 播放词汇(过桥给前端) ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Audio,
    Video,
}

/// 「怎么放的」——本次播放走了哪条链路(core 只发 key,前端按 locale 出短标签,§6.6)。
/// 给用户/开发者一个可见的「省 CPU 还是在转码」信号,也是 0.2.6 copy 切片真机验收的眼睛:
/// 同是本地不兼容片,看到 `HlsCopy`(视频没重编)还是 `HlsTranscode`(在转)一目了然。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaybackRoute {
    /// 原生直传:本地 `/f/` 全兼容文件 或 网络单流 `/s/` —— 零转码、原生 seek,最省。
    Direct,
    /// 自适应流(DASH,shaka/MSE):B 站音视频分离直供,播放器管时间轴 → 原生 seek + 同步。
    Dash,
    /// HLS 视频 `-c:v copy` 切片(视频已兼容 → 不重编码,仅音轨转 AAC):首播即省 CPU(0.2.6)。
    HlsCopy,
    /// HLS 重编码切片(视频 HEVC/AV1 等 WebView2 解不了 → 转 H.264):吃 CPU。
    HlsTranscode,
    /// ffmpeg 渐进混流(`/m/`):网络 DASH 回落 或 本地转封装,`?t=` 重启式 seek(无原生 seek)。
    Remux,
}

/// 从注册好的 relay URL 反推链路(relay 路径是稳定契约,前端也已按 `/m/` 判混流)。
/// 本地 `/hls/` 当前恒是重编码;0.2.6 copy 切片落地后由调用点显式传 `HlsCopy` 覆盖,不走这里。
fn derive_route(stream_url: &str, manifest_url: Option<&str>) -> PlaybackRoute {
    match manifest_url {
        Some(m) if m.contains("/dash/") => PlaybackRoute::Dash,
        Some(_) => PlaybackRoute::HlsTranscode, // `/hls/`(本地按需切片)
        None if stream_url.contains("/m/") => PlaybackRoute::Remux,
        None => PlaybackRoute::Direct,
    }
}

/// 一条可显示的字幕(P4)。`url` 指向 relay 的 WebVTT 端点(现转现回、不落盘)。
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SubtitleRef {
    pub url: String,
    /// ISO-639-2 语言码;没标 = None,前端显示成「字幕 N」。core 不翻译(§6.6)。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,
    /// 来自外挂文件(vs 文件内嵌)—— UI 可据此提示来源。
    pub sidecar: bool,
}

/// 「正在播放」:前端拿 stream_url 挂播放元素;page_url 留诊断/以后“浏览器打开”。
#[derive(Debug, Clone, Serialize)]
pub struct NowPlaying {
    pub kind: MediaKind,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_seconds: Option<f64>,
    pub stream_url: String,
    /// 有值 = 自适应流(DASH/HLS):前端用 shaka(MSE)播它,播放器自己管时间轴 → 原生 seek/同步。
    /// 否则前端用 `stream_url` 挂原生 `<video>/<audio>`(直传文件/单流,原生 seek)。(B 站 DASH 走这里。)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest_url: Option<String>,
    /// 字幕清单(P4;空 = 这片没有可显示的字幕)。`url` 指向 relay 的 WebVTT 端点,前端挂成
    /// `<track>`;**默认不开**,用户/模型选了才显示。lang 是 ISO 码,core 不翻译(§6.6)。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subtitles: Vec<SubtitleRef>,
    /// 本地音频旁挂 .lrc 的原文(有词才带,原样过桥——歌词是数据不是我们产的文案):
    /// 前端播放条上方滚当前句。视频不带(字幕走 subtitles);网络流没有词源,恒 None。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lyrics: Option<String>,
    /// 本次播放走的链路(见 `PlaybackRoute`):前端在播放条上出一枚「怎么放的」小徽章。
    pub route: PlaybackRoute,
    pub page_url: String,
    pub source: String,
    /// 多集续播位置:有值 = 这是一个 ≥2 集的剧集(B 站合集/分P、本地剧集文件夹)。
    /// 前端据 index/total 显示「第N/共M集」+ 上/下一集按钮;`ended` 时若非末集自动续播。
    /// None = 单个内容(电影/单曲),不出现集数 UI。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub playlist: Option<PlaylistPos>,
    /// 播放模式镜像:once(放完就停)/ loop_all(列表循环)/ loop_one(单曲循环)/ shuffle(随机)。
    /// core 是唯一真相,每次 Play 事件全量捎带 → 新播放的复位、切集时的延续,前端零猜测;
    /// loop_one 由前端 `el.loop` 原生无缝循环。中途改模式由 `MediaEvent::Mode` 增量对齐。
    pub play_mode: String,
    /// 封面地址(relay `/cover/{token}`,现取现回一张 ≤512px 的 JPEG)。**有值 = 有图可取**,
    /// None = 前端显 ♪ 占位(§3.5 不假装有图)。来源:本地音频文件内嵌图 > 同目录侧车图
    /// (cover / folder / front / album.*)> 网络源封面(B 站视频封面,视频也带 —— 当 poster 与系统媒体浮层图)。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cover_url: Option<String>,
    /// 专辑名(本地音频的全局标签;网络流 / 视频恒无)。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    /// 倍速镜像(0.5–3):新点播复位 1、切集 / 自动续播沿用;前端每条 Play 直接落到播放元素。
    pub rate: f64,
    /// 全部音轨(本地探测;≥2 条 UI 才出切换钮,〔此刻〕才列清单)。网络流恒空(来源定音轨)。
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub audio_tracks: Vec<probe::AudioTrack>,
    /// 当前音轨(0 起下标;`-map 0:a:{n}` 的 n)。
    pub audio_track: usize,
    /// 有值 = 从这个位置(秒)接着播:切音轨重建管线时带上,前端加载完 seek 过去。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume_at: Option<f64>,
    /// 进度条 hover 预览缩略图的基址(`…/thumb/{token}`,前端自己拼 `?t=秒`)。
    /// **有值 = 这片能出图**,None = 只出时间气泡(网络流没帧可抽、放歌没画面、ffmpeg 还没到手)。
    /// 前端只认这一个信号,不做别的判断(§3.5 不假装有图)。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumb_url: Option<String>,
    /// 本集的片头 / 片尾(手标 / B 站标注 / 章节 / 指纹检测汇成,见 `skip::resolve`)。None = 不跳。
    /// 前端:自然播进片头段 → 跳到段尾(OSD 可回看);自然越过片尾起点且有下一集 → 3 秒倒计时切集。
    /// 标记 / 检测结果变了由 `MediaEvent::Skip` 增量替换。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip: Option<skip::SkipInfo>,
}

/// 剧集列表面板要的整份队列(按需取,不塞进每条 Play 事件 —— 合集上百集,标题一次拉够)。
#[derive(Debug, Clone, Serialize)]
pub struct PlaylistView {
    pub index: usize,
    /// 剧名(拿不到 None,前端退回集数标题)。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub entries: Vec<PlaylistEntryView>,
}

/// 列表里的一集:只给显示用的标题(url / id 不过桥 —— 点第几集由 core 按下标定位)。
#[derive(Debug, Clone, Serialize)]
pub struct PlaylistEntryView {
    pub title: String,
}

/// 「正在播放」里的队列位置(过桥给前端 + 给工具叙述)。
#[derive(Debug, Clone, Copy, Serialize)]
pub struct PlaylistPos {
    /// 当前集下标(0 起)。
    pub index: usize,
    /// 总集数(>1 才会带 PlaylistPos)。
    pub total: usize,
    /// 本次是否「接着上次续播」跳转而来(true → 工具叙述"接着上次第N集")。前端忽略。
    pub resumed: bool,
}

/// 队列里的一集:来源无关(B 站 / 本地共用同一队列机器,§多集续播)。
#[derive(Debug, Clone, Serialize)]
pub struct EpisodeRef {
    /// 集身份(续播记忆存的就是它):B 站 bvid / `p3`;本地**相对文件名**。
    /// 稳定、可跨会话比对、绝不含绝对路径(§6.2)。
    pub id: String,
    /// 可播地址:B 站 page_url;本地绝对路径。`advance` 直接喂回 `play_entry`。
    pub url: String,
    /// 显示标题(分P 的 part 名 / 合集集名 / 本地文件名)。
    pub title: String,
}

/// 发现出来的一部剧集(B 站合集/分P/番剧、本地剧集文件夹、本地音频整夹共用):
/// `key` = 续播记忆 key(绝不含绝对路径,§6.2);`title` = 剧名(关怀条「继续看《X》」/ 家庭日记用,
/// 拿不到 None —— 不拿集名冒充);`entries` = 有序集列表(≥2 才成系列)。
#[derive(Debug, Clone)]
pub struct Series {
    pub key: String,
    pub title: Option<String>,
    pub entries: Vec<EpisodeRef>,
}

/// 登录窗口的三件套(壳层 media_login command 消费)。
#[derive(Debug, Clone, Serialize)]
pub struct LoginSpec {
    pub source: String,
    pub login_url: String,
    pub cookie_url: String,
    pub login_cookie: String,
}

/// `play()` 的结果:要么已开播,要么卡在「需要登录」。后者**不是失败**——已记下待重放,
/// 用户扫码登录成功(`set_cookies`)那一刻会带着新 cookie 自动续上,不再 `bail` 喂模型「放失败了」。
#[derive(Debug)]
#[allow(clippy::large_enum_variant)] // NowPlaying 带音轨清单后变大;瞬态返回值,Box 徒增全部匹配点噪音
pub enum PlayOutcome {
    /// 解析成功、已发 Play 事件,前端起播。
    Playing(NowPlaying),
    /// 需要登录:已发 AuthRequired(UI 出扫码气泡)+ 记下待重放。detail = 解析器给的原因。
    AwaitingLogin { detail: String },
}

/// 待登录重放:登录成功时把当初这次播放原样再跑一遍(带上新 cookie)。
/// 超过 TTL 视为过期(用户早不想看了)→ 丢弃不重放,免「登录后凭空冒出个老视频」。
#[derive(Debug, Clone)]
struct PendingPlay {
    user_id: i64,
    page_url: String,
    audio_only: bool,
    at: Instant,
}

/// 待重放有效期:超过即作废。
const PENDING_PLAY_TTL: Duration = Duration::from_secs(600);
/// 网络源雪碧图(进度条预览)抓取的总预算(**§4.11 待用户确认**)。它与 yt-dlp 解析**并行**跑:
/// 解析动辄一两秒,预览图的请求藏在它后面,常态下解析结束前就到手、零额外等待;这个数只兜
/// 「平台接口卡住」的坏情形 —— 起播最多被拖 3s 减去解析耗时。预览图是锦上添花,超时 = 没有
/// 缩略图(`thumb_url` None),不是失败、不进任务条。
const SPRITE_FETCH_TIMEOUT: Duration = Duration::from_secs(3);
/// 倍速允许范围(§4.11 用户拍板 2026-09-07)。下限 0.5:Chromium 的音频渲染器只在 0.5–4 倍之间
/// 做变速保调,更慢直接静音 —— 原先放行的 0.25 档在 Windows 上是无声的;上限 3 = 前端档位表的顶
/// (`useMedia.RATE_STEPS`,两处手工同步)。工具描述里的「0.5–3」照抄这里。
const SPEED_RANGE: std::ops::RangeInclusive<f64> = 0.5..=3.0;
/// 集内进度落盘节拍(用户拍板「30s 之类」,2026-09-07):前端 15s 一次心跳,core 每 30s 真写一次盘;
/// 暂停 / 停止 / 切集不受节拍限制,立刻落。断电最多丢半分钟。
const PROGRESS_PERSIST_EVERY: Duration = Duration::from_secs(30);
/// 续播读侧三道闸(单源;§4.11 过程默认,用户嫌不对回来改):
/// 看了不到 30 秒 = 还没开始看,下次从头;离结尾不到 90 秒 = 看完了(片尾字幕区),下次从下一集开头;
/// 总长不到 10 分钟的内容(歌 / 短片)不记集内位置 —— 重听从头是常识,记了反而怪。
const RESUME_HEAD_S: f64 = 30.0;
const RESUME_TAIL_S: f64 = 90.0;
const RESUME_MIN_DURATION_S: f64 = 600.0;

/// 前端播放器的「此刻」状态快照。播放真相在前端 WebView(播放在那跑、放完只有它知道);
/// core 起播时乐观 seed,前端在生命周期切换(playing/paused/ended/stop)+ 音量/倍速/seek 调整 +
/// 播放中低频心跳时经 `report_media_state` 命令回报校准。app 级瞬态(§6.4 派生可丢:
/// 丢了 = 按空闲算、不出错)。回合装配时读成一行「此刻」背景喂模型 → 修「歌放完了模型却
/// 以为还在播着」,并让模型知道当前音量/进度(才能「调到 50」「快进 5 分钟」这类绝对/相对操作)。
#[derive(Debug, Clone, Default)]
struct Playback {
    /// None = 空闲(没在播任何东西);Some = 正在放/暂停的标题。
    title: Option<String>,
    /// 仅当 title 为 Some 时有意义:true = 暂停,false = 正在播。
    paused: bool,
    /// 当前在播的剧集进度「第 index/共 total 集」(单集内容为 None);喂模型「此刻」背景用。
    pos: Option<(usize, usize)>,
    /// 基准音量 0–100(前端的用户意图值,不含唤醒避让折算;None = 尚无回报)。
    /// 跨播放粘住(seed/idle 都保留)——与前端「音量粘住」语义一致。
    volume_pct: Option<u8>,
    /// 播放位置/总长(秒)与倍速;`at` = 回报时刻,播放中按倍速外推出「此刻」位置
    /// (回报之间也准,不靠前端高频心跳)。
    position_secs: Option<f64>,
    duration_secs: Option<f64>,
    rate: Option<f64>,
    at: Option<std::time::Instant>,
}

/// 前端回报的播放器快照(`report_media_state` 命令载荷;新字段全可缺 —— 浏览器预览/旧路径兼容)。
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct PlaybackReport {
    /// playing | paused | idle | loading(其余按 playing)。
    pub status: String,
    #[serde(default)]
    pub title: Option<String>,
    /// 基准音量 0–100(用户意图,不含避让折算)。
    #[serde(default)]
    pub volume: Option<f64>,
    /// 播放位置 / 总长(秒)。
    #[serde(default)]
    pub position: Option<f64>,
    #[serde(default)]
    pub duration: Option<f64>,
    /// 倍速(缺省当 1)。
    #[serde(default)]
    pub rate: Option<f64>,
}

/// 播放模式(2026-09-07 用户拍板「一个模式钮三档」:列表循环 / 单曲循环 / 随机,取代原
/// 「循环三态 × 随机开关」两个正交状态)。app 级,**新 `play()` 请求复位**(同「倍速每次复位、
/// 音量粘住」的粘性口径,切集 / 自动续播不复位),默认见 `default_for`:
///   · 放歌且成队列 → `LoopAll`(音乐播放器口径:歌单默认循环着放);
///   · 单曲 / 视频剧集 → `Once`(放完就停;一季剧看完不该半夜自己从第一集重来)。
/// `Once` 是内部态:音频队列的模式钮只在后三档轮转,单曲上只有 `Once ↔ LoopOne` 两档,
/// 视频没有模式钮(嘴控仍可开循环 / 随机)。`LoopOne` 由前端 `el.loop` 原生无缝循环(ended
/// 压根不触发);`LoopAll` / `Shuffle` 在 `auto_next` 里回卷 / 挑歌,随机恒循环不停。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlayMode {
    Once,
    LoopAll,
    LoopOne,
    Shuffle,
}

impl PlayMode {
    /// 过桥字符串(`NowPlaying.play_mode` / `MediaEvent::Mode`;前端按它对齐 el.loop 与按钮态)。
    fn as_str(self) -> &'static str {
        match self {
            PlayMode::Once => "once",
            PlayMode::LoopAll => "loop_all",
            PlayMode::LoopOne => "loop_one",
            PlayMode::Shuffle => "shuffle",
        }
    }

    /// 新点播的默认模式:放歌成队列 = 列表循环,其余(单曲 / 视频)= 放完就停。
    fn default_for(audio_only: bool, has_queue: bool) -> PlayMode {
        if audio_only && has_queue {
            PlayMode::LoopAll
        } else {
            PlayMode::Once
        }
    }

    /// 手动上 / 下一首到头时要不要回卷:三档都是「列表是环」,只有放完就停的 Once 到头报错。
    fn wraps(self) -> bool {
        self != PlayMode::Once
    }

    /// 〔此刻〕背景里的模式一句(默认的放完就停不啰嗦)。
    fn ambient(self) -> &'static str {
        match self {
            PlayMode::Once => "",
            PlayMode::LoopAll => ",列表循环中",
            PlayMode::LoopOne => ",单曲循环中",
            PlayMode::Shuffle => ",随机播放中",
        }
    }
}

/// 当前剧集队列(app 级瞬态,§6.4 派生可丢:丢了 = 退化成单集,绝不出错)。来源无关 ——
/// B 站合集/分P 与本地剧集填的是同一个队列;`advance` 只挪 index、`play_entry` 现取现播。
#[derive(Debug, Clone)]
struct Playlist {
    /// 续播记忆的 key(B 站 season id/bvid;本地视频 `local:FNV(目录+骨架)`、
    /// 本地音频 `local:FNV(目录+audio)` —— 音频整夹一个队列,从哪首进都是同一个 key)。
    series_key: String,
    /// 剧名(进度表 series_title / 关怀条用;拿不到 None)。
    series_title: Option<String>,
    entries: Vec<EpisodeRef>,
    /// 当前集下标。
    index: usize,
    /// 整队列继承首集的音/画意图(放歌 vs 看视频),切集不变。
    audio_only: bool,
    /// 随机播放履历(这一轮已放过的队列下标,当前一首恒在末位;进随机模式时重置为 [当前])。
    /// 随机开没开看 `Inner.mode == Shuffle`,履历随队列生灭。
    played: Vec<usize>,
}

/// 当前播放内容在续播表里的身份(起播成功时记下;前端心跳据此落集内位置;停播即清)。
/// `title` 用来核对心跳 —— 切集 / 换片瞬间迟到的旧心跳带着旧标题,不许写进新一集的行。
#[derive(Debug, Clone)]
struct ProgressTarget {
    key: String,
    episode_id: String,
    title: String,
}

/// 本集片头 / 片尾的解析现场:`auto` = 起播时拿到的自动段(B 站标注 / 章节),`duration` = 本集总长,
/// `current` = 最近一次解析结果(嘴控「跳过片头」读它)。手标 / 检测落库后用原料重算(`refresh_skip`)。
#[derive(Debug, Clone)]
struct SkipCtx {
    auto: Vec<skip::AutoSeg>,
    duration: Option<f64>,
    current: Option<skip::SkipInfo>,
}

/// 当前本地播放的现场(切音轨重建管线用;app 级瞬态,§6.4 派生可丢:丢了 = 切不了轨,不出错)。
#[derive(Debug, Clone)]
struct CurrentLocal {
    page_url: String,
    audio_only: bool,
    /// 探测出的音轨清单(顺序 = -map 轨号)。
    tracks: Vec<probe::AudioTrack>,
}

// ---------- 运行时 ----------

struct Inner {
    dir: PathBuf,
    store: Store,
    bus: Bus,
    tasks: Tasks,
    /// 后台差事登记处(模型可见性:此刻/status/取消/收尾汇报;批量下载与批量配词注册进来)。
    bg: crate::bgtasks::BgTasks,
    components: Components,
    relay: tokio::sync::OnceCell<relay::Relay>,
    sources: Vec<Arc<dyn MediaSource>>,
    login_hint_sent: AtomicBool,
    /// ffmpeg 是否已发起后台预取(每进程至多一次;失败复位留给用时下载重试)。
    ffmpeg_prefetch_started: AtomicBool,
    /// 因「需登录」卡住、待登录后自动重放的播放(按源 id)。
    pending_play: Mutex<HashMap<String, PendingPlay>>,
    /// 前端播放器的当下状态(回合装配读它喂模型「此刻」背景;见 Playback 注释)。
    playback: Mutex<Playback>,
    /// 当前剧集队列(多集续播;None = 没在放剧集/单集内容)。
    playlist: Mutex<Option<Playlist>>,
    /// 播放模式(见 PlayMode;新 `play()` 请求按内容复位:歌单列表循环、其余放完就停)。
    mode: Mutex<PlayMode>,
    /// 选中的音轨(0 起;新 `play()` 复位 0,切集粘住 —— 看英文轨的剧下一集还是英文)。
    audio_track: Mutex<usize>,
    /// 选中音轨的语言码(切轨时从清单抄下;切集按**语言**对号、不按轨号 —— 两集轨序不同时
    /// 「第 2 条」可能换了语言;没标语言的文件退回按轨号)。None = 没显式选过。
    audio_track_lang: Mutex<Option<String>>,
    /// 倍速(0.5–3;单源见 `SPEED_RANGE`)。**队列级粘住**:新 `play()` 请求复位 1(mpv 时代
    /// 教训:放完电影再放歌还是 2 倍),切集 / 自动续播沿用 —— 1.5 倍看剧不该每集掉回 1.0。
    /// 前端每条 Play 事件直接应用 `NowPlaying.rate`,零猜测(与 loop_mode 同款镜像)。
    rate: Mutex<f64>,
    /// 当前本地播放现场(切音轨用;None = 没在放本地内容)。
    current_local: Mutex<Option<CurrentLocal>>,
    /// 当前内容的续播身份 + 上次落盘时刻(节拍见 PROGRESS_PERSIST_EVERY)。None = 不记进度。
    progress: Mutex<Option<ProgressTarget>>,
    progress_at: Mutex<Option<std::time::Instant>>,
    /// 当前这一集的片头 / 片尾解析原料 + 结果(用户标记 / 检测跑完后据此重算再广播)。None = 单部内容。
    skip_ctx: Mutex<Option<SkipCtx>>,
    /// 指纹检测任务在跑(一次只跑一个;新一集开播时上一个还没完就先不起,下次开播再来)。
    detect_busy: AtomicBool,
    /// BT 下载引擎(懒建,同 relay):不用 BT 的用户零成本,且**不会平白发 DHT 包**。
    torrent: tokio::sync::OnceCell<torrent::TorrentEngine>,
}

#[derive(Clone)]
pub struct MediaRuntime {
    inner: Arc<Inner>,
}

impl MediaRuntime {
    pub fn new(dir: PathBuf, store: Store, bus: Bus) -> MediaRuntime {
        let tasks = Tasks::new(bus.clone());
        let components = Components::new(dir.join("components"), tasks.clone());
        let bg = crate::bgtasks::BgTasks::new(store.clone());
        MediaRuntime {
            inner: Arc::new(Inner {
                dir,
                store,
                bus,
                tasks,
                bg,
                components,
                relay: tokio::sync::OnceCell::new(),
                sources: vec![Arc::new(bilibili::Bilibili::new())],
                login_hint_sent: AtomicBool::new(false),
                ffmpeg_prefetch_started: AtomicBool::new(false),
                pending_play: Mutex::new(HashMap::new()),
                playback: Mutex::new(Playback::default()),
                playlist: Mutex::new(None),
                mode: Mutex::new(PlayMode::Once),
                audio_track: Mutex::new(0),
                audio_track_lang: Mutex::new(None),
                rate: Mutex::new(1.0),
                current_local: Mutex::new(None),
                progress: Mutex::new(None),
                progress_at: Mutex::new(None),
                skip_ctx: Mutex::new(None),
                detect_busy: AtomicBool::new(false),
                torrent: tokio::sync::OnceCell::new(),
            }),
        }
    }

    /// 测试/无壳跑法:事件无人听、组件落系统临时目录。功能完整,只是安静。
    pub fn detached(store: Store) -> MediaRuntime {
        MediaRuntime::new(std::env::temp_dir().join("larkwing-media"), store, Bus::new())
    }

    pub fn bus(&self) -> &Bus {
        &self.inner.bus
    }

    /// 后台差事登记处(task_status/task_cancel 工具与〔此刻〕背景经此读写)。
    pub fn bg(&self) -> &crate::bgtasks::BgTasks {
        &self.inner.bg
    }

    fn publish(&self, ev: MediaEvent) {
        self.inner.bus.publish(AppEvent::Media(ev));
    }

    /// 镜像列表 = 数据(settings 可覆盖,坏 JSON 回默认)。
    fn mirrors(&self) -> Vec<String> {
        self.inner
            .store
            .settings
            .get(None, "media.gh_mirrors")
            .ok()
            .flatten()
            .and_then(|json| serde_json::from_str::<Vec<String>>(&json).ok())
            .unwrap_or_else(|| DEFAULT_GH_MIRRORS.iter().map(|s| s.to_string()).collect())
    }

    /// 把一个本地文件注册成 localhost 播放地址(/f/ 通道,带 Range)。
    /// TTS 音频(PLAN §11)与本地媒体共用这条路;voice 不依赖 media,壳层缝合。
    pub async fn file_url(&self, path: PathBuf) -> Result<String> {
        let relay = self
            .inner
            .relay
            .get_or_try_init(relay::Relay::start)
            .await
            .context("转发服务起不来")?;
        Ok(relay.register_file(path))
    }

    /// 聊天图片缩略图落盘目录(`<media>/attachments`;随数据根搬家)。engine 写、命令读回。
    pub fn attachments_dir(&self) -> PathBuf {
        self.inner.dir.join("attachments")
    }

    /// 收到的文件/文档「收件区」(`<media>/inbox`;随数据根搬家)。手机发来的文件落这里、
    /// 把本地路径交给模型,「把发来的文件存到电脑 / 整理」才成立(扫描件读不出文字也能存,
    /// 存文件不需要读内容,§9)。与缩略图分开:那是给 UI 看的,这是给模型 fs 操作的原件。
    pub fn inbox_dir(&self) -> PathBuf {
        self.inner.dir.join("inbox")
    }

    /// 历史图片小票(相对文件名)→ 可显缩略图的 localhost URL(重开会话回看图,§1/§9)。
    /// 文件名兜底防目录穿越:只取末段 file_name,拒绝 `..` / 路径分隔。
    pub async fn attachment_url(&self, file: &str) -> Result<String> {
        let name = std::path::Path::new(file)
            .file_name()
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty())
            .context("非法附件名")?;
        self.file_url(self.attachments_dir().join(name)).await
    }

    fn default_source(&self) -> &Arc<dyn MediaSource> {
        &self.inner.sources[0]
    }

    fn source_of_url(&self, url: &str) -> Option<&Arc<dyn MediaSource>> {
        // 接缝够用即可:按源 id 出现在域名里判断(bilibili.com / b23.tv 短链交给 yt-dlp)
        self.inner.sources.iter().find(|s| url.contains(s.id()))
    }

    pub fn login_spec(&self, source_id: &str) -> Option<LoginSpec> {
        let s = self.inner.sources.iter().find(|s| s.id() == source_id)?;
        Some(LoginSpec {
            source: s.id().into(),
            login_url: s.login_url().into(),
            cookie_url: s.cookie_url().into(),
            login_cookie: s.login_cookie().into(),
        })
    }

    /// 登录窗口收割的 cookie 入库 + 导出 + 广播(UI 撤登录提示,下次解析自动带上)。
    /// 若此前有播放因「需登录」卡住 → 带着新 cookie 自动重放(不绕模型,同嘴控哲学 §7.1)。
    pub fn set_cookies(&self, source_id: &str, recs: Vec<CookieRec>) -> Result<()> {
        cookies::save(&self.inner.store, source_id, &recs)?;
        self.publish(MediaEvent::LoggedIn { source: source_id.into() });
        tracing::info!(source = source_id, n = recs.len(), "登录态已入库");
        // 需 tokio 运行时(生产里 set_cookies 来自异步轮询);无运行时的同步调用方保留待重放、不丢。
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            if let Some(p) = self.take_pending_play(source_id) {
                tracing::info!(source = source_id, url = %p.page_url, "登录成功,自动重放待播内容");
                let this = self.clone();
                handle.spawn(async move {
                    // 重放走完整 play(会重建队列):带新 cookie 重新发现合集/分P,resume 规则照常生效。
                    if let Err(e) = this.play(p.user_id, &p.page_url, p.audio_only, false, None).await {
                        tracing::warn!("登录后自动重放失败: {e:#}");
                    }
                });
            }
        }
        Ok(())
    }

    /// 记下一次「因需登录而卡住」的播放,待登录成功后自动重放。
    fn record_pending(&self, user_id: i64, source: &str, page_url: &str, audio_only: bool) {
        self.inner.pending_play.lk().insert(
            source.to_string(),
            PendingPlay {
                user_id,
                page_url: page_url.to_string(),
                audio_only,
                at: Instant::now(),
            },
        );
    }

    /// 取走某源的待重放(取即消费,不重复);超过 TTL 的丢弃、返回 None。
    fn take_pending_play(&self, source: &str) -> Option<PendingPlay> {
        let p = self.inner.pending_play.lk().remove(source)?;
        (p.at.elapsed() <= PENDING_PLAY_TTL).then_some(p)
    }

    /// 失败重下一个组件(前端「重试」按钮直连,§7.1 不绕 LLM)。按组件名找枚举,后台重跑
    /// `ensure`(自带 HUD 任务:成功 done、再失败仍 fail_retryable 冒新卡)。不阻塞调用方;
    /// **须在 tokio 上下文内调用**(裸 `tokio::spawn`,壳层同步命令直调会 panic)。
    pub fn retry_component(&self, name: &str) {
        let Some(c) = Component::from_name(name) else {
            tracing::warn!(component = name, "retry_download:未知组件名,忽略");
            return;
        };
        let this = self.clone();
        tokio::spawn(async move {
            let _ = this.inner.components.ensure(c, &this.mirrors()).await;
        });
    }

    /// 组件就绪(分离 spawn:回合被取消/超时,下载在 HUD 里继续走完,下次直接命中)。
    async fn ensure_component(&self, c: Component) -> Result<PathBuf> {
        let this = self.clone();
        let mirrors = self.mirrors();
        tokio::spawn(async move { this.inner.components.ensure(c, &mirrors).await })
            .await
            .context("组件下载任务挂了")?
    }

    /// pdf_to_png(tools/pdf.rs)用:pdfium 动态库就位(用时下载,§6.9;进度上 HUD)。
    pub async fn ensure_pdfium(&self) -> Result<PathBuf> {
        self.ensure_component(Component::Pdfium).await
    }

    /// webrender(壳层渲染窗)回传信箱:确保 relay 在线 → (POST 地址, 一次性接收端)。
    pub async fn webrender_collect(
        &self,
    ) -> Result<(String, tokio::sync::oneshot::Receiver<String>)> {
        let relay = self.inner.relay.get_or_try_init(relay::Relay::start).await?;
        Ok(relay.register_collect())
    }

    /// 进度总线句柄(壳层 webrender 上任务卡用;Tasks 本就是 Clone 的轻句柄)。
    pub fn tasks(&self) -> Tasks {
        self.inner.tasks.clone()
    }

    /// 搜索(默认源)。风控错误在此转事件,文字留给工具层喂模型。
    pub async fn search(&self, keyword: &str, limit: usize) -> Result<Vec<MediaHit>, SearchError> {
        let source = self.default_source();
        let cookie = cookies::load(&self.inner.store, source.id()).map(|c| cookies::header_value(&c));
        let result = source.search(keyword, limit, cookie.as_deref()).await;
        if matches!(result, Err(SearchError::RiskControl)) {
            self.publish(MediaEvent::AuthRequired { source: source.id().into() });
        }
        result
    }

    /// 首次播放**任何**媒体(含放歌)就后台预取 ffmpeg(fire-and-forget,不 await):视频迟早要它
    /// (网络 DASH 混流 / 本地 HEVC/AC3 转码),提前下好 → 真用到时零等待。下了不一定用、后台不阻塞
    /// 当前播放,所以放歌也预取(用户拍板,2026-06-19:预取的是工具不是转码,「下了不一定用、真用到不必等」)。
    /// 每进程至多触发一次;失败复位标记,留给后续重试。`ensure_component` 内有锁去重 + 已在磁盘即秒返回
    /// → 预取与用时下载只下一份、只冒一张卡,ffmpeg 已就绪时这步是即时 no-op。
    fn prefetch_ffmpeg(&self) {
        if self.inner.ffmpeg_prefetch_started.swap(true, Ordering::Relaxed) {
            return; // 本进程已预取过(或正在跑)
        }
        let this = self.clone();
        tokio::spawn(async move {
            match this.ensure_component(Component::Ffmpeg).await {
                Ok(_) => tracing::info!("ffmpeg 预取就绪"),
                Err(e) => {
                    tracing::warn!("ffmpeg 预取失败(用时会再试): {e:#}");
                    this.inner.ffmpeg_prefetch_started.store(false, Ordering::Relaxed);
                }
            }
        });
    }

}

/// 本地路径判定:unix 绝对路径 / Windows 盘符(C:\ 或 C:/)/ UNC(\\nas\share)。
/// 排除 protocol-relative 的 `//host`(那是网络);相对路径一律拒(工具层报错引导)。
pub fn is_local_path(s: &str) -> bool {
    let b = s.as_bytes();
    (s.starts_with('/') && !s.starts_with("//"))
        || s.starts_with("\\\\")
        || (b.len() >= 3
            && b[0].is_ascii_alphabetic()
            && b[1] == b':'
            && (b[2] == b'\\' || b[2] == b'/'))
}

// ---------- 本地剧集发现(确定式:同文件夹 + 数字骨架分组 + 自然排序) ----------

/// 单测夹具:拆分后各主题文件的 `mod tests` 共用(`crate::media::testkit::*`)。
/// 住在 mod.rs 是因为它得被所有兄弟文件看见 —— 父模块的 pub(super) 项对全部后代可见。
#[cfg(test)]
mod testkit {
    use super::*;

    pub(super) fn runtime(tag: &str) -> (MediaRuntime, tokio::sync::broadcast::Receiver<AppEvent>) {
        let dir = std::env::temp_dir().join(format!("lw-media-test-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::open(&dir.join("t.db")).unwrap();
        let bus = Bus::new();
        let rx = bus.subscribe();
        let rt = MediaRuntime::new(dir, store, bus);
        // 夹具**预先扣上「已预取」的闩** = 单测一律不为 ffmpeg 联网。`play()` 一进来就
        // fire-and-forget `prefetch_ffmpeg()`(§6.9 用时下载),开发机 PATH 上有 ffmpeg 所以
        // 无声无息;**CI 机器上没有** → 每个调 play 的用例都真的去 GitHub 拉几十 MB,还先往
        // 同一条总线插一条 `AppEvent::Task`(download 开始)—— 断言「收到的第一条事件是 Play」
        // 就这么被顶掉(2026-09-09 CI 首红,本机 `PATH=/usr/bin:/bin` 可逐字复现)。
        // 真需要 ffmpeg 的用例走 `ensure_component`(PATH 命中),不受这个闩影响。
        rt.inner.ffmpeg_prefetch_started.store(true, Ordering::Relaxed);
        (rt, rx)
    }

    pub(super) fn touch(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, b"x").unwrap();
        p
    }

    pub(super) fn mk_shuffle_playlist(n: usize) -> Playlist {
        Playlist {
            series_key: "local:test".into(),
            series_title: None,
            entries: (0..n)
                .map(|i| EpisodeRef {
                    id: format!("{i}.mp3"),
                    url: format!("/x/{i}.mp3"),
                    title: format!("{i}"),
                })
                .collect(),
            index: 0,
            audio_only: true,
            played: vec![0],
        }
    }

    /// 造一份前端回报(测试便捷形;新字段默认缺 = 老形回报)。
    pub(super) fn report(status: &str, title: Option<&str>) -> PlaybackReport {
        PlaybackReport {
            status: status.into(),
            title: title.map(str::to_string),
            ..PlaybackReport::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testkit::*;
    use super::*;

    #[test]
    fn login_spec_known_and_unknown_sources() {
        let (rt, _rx) = runtime("login");
        let spec = rt.login_spec("bilibili").unwrap();
        assert!(spec.login_url.contains("passport.bilibili.com"));
        assert_eq!(spec.login_cookie, "SESSDATA");
        assert!(rt.login_spec("nope").is_none());
    }

    #[test]
    fn cookies_roundtrip_and_logged_in_event() {
        let (rt, mut rx) = runtime("ck");
        rt.set_cookies(
            "bilibili",
            vec![CookieRec {
                name: "SESSDATA".into(),
                value: "v".into(),
                domain: ".bilibili.com".into(),
                path: "/".into(),
            }],
        )
        .unwrap();
        let loaded = cookies::load(&rt.inner.store, "bilibili").unwrap();
        assert_eq!(loaded[0].name, "SESSDATA");
        assert!(matches!(
            rx.try_recv().unwrap(),
            AppEvent::Media(MediaEvent::LoggedIn { .. })
        ));
    }

    #[test]
    fn mirrors_default_then_overridable() {
        let (rt, _rx) = runtime("mir");
        assert_eq!(rt.mirrors().len(), DEFAULT_GH_MIRRORS.len());
        rt.inner.store.settings.set(None, "media.gh_mirrors", r#"["https://my.mirror/"]"#).unwrap();
        assert_eq!(rt.mirrors(), vec!["https://my.mirror/".to_string()]);
        rt.inner.store.settings.set(None, "media.gh_mirrors", "not json").unwrap();
        assert_eq!(rt.mirrors().len(), DEFAULT_GH_MIRRORS.len(), "坏 JSON 回默认");
    }

    #[test]
    fn source_of_url_matches_by_id() {
        let (rt, _rx) = runtime("src");
        assert!(rt.source_of_url("https://www.bilibili.com/video/BV1").is_some());
        assert!(rt.source_of_url("https://example.com/v").is_none());
    }

    #[test]
    fn pending_play_record_take_roundtrip() {
        let (rt, _rx) = runtime("pending");
        assert!(rt.take_pending_play("bilibili").is_none(), "初始无待重放");
        rt.record_pending(1, "bilibili", "https://www.bilibili.com/video/BV1", true);
        let p = rt.take_pending_play("bilibili").expect("记下后应能取到");
        assert_eq!(p.page_url, "https://www.bilibili.com/video/BV1");
        assert!(p.audio_only);
        assert!(rt.take_pending_play("bilibili").is_none(), "取走即消费,不重复重放");
    }

    #[test]
    fn pending_play_expires_after_ttl() {
        let (rt, _rx) = runtime("pending-exp");
        // 直接塞一个「过期」条目(at 早于 TTL);checked_sub 在极早期 Instant 上可能为 None,跳过即可
        if let Some(stale) = Instant::now().checked_sub(PENDING_PLAY_TTL + Duration::from_secs(1)) {
            rt.inner.pending_play.lk().insert(
                "bilibili".into(),
                PendingPlay {
                    user_id: 1,
                    page_url: "https://www.bilibili.com/video/BVold".into(),
                    audio_only: false,
                    at: stale,
                },
            );
            assert!(rt.take_pending_play("bilibili").is_none(), "过期的待重放不返回");
        }
    }

}
