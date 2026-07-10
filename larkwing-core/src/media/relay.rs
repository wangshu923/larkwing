//! localhost 流转发:WebView 的 <audio>/<video> 不能直挂上游 CDN(防盗链要
//! Referer/UA,标签发不出去)→ 这里代发。两条路:
//!   /s/{token}  直转:上游头 + Range 透传(音频/单文件视频,拖进度条原生可用)
//!   /m/{token}  混流:B 站 DASH 音视频分离 → ffmpeg `-c copy` 拼成 fMP4 流(?t= 起播秒)
//! 只绑 127.0.0.1,token 随机不可猜;真相不落地 —— 注册表是纯瞬态,丢了重解析。

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

/// 视频转码用哪个 H.264 编码器。**探测出来的、每 entry 固定**(init 与各段必须同编码器,否则
/// avcC 配置不一致、MSE 拼不上)。有硬件编码器就用 GPU(省 CPU,§硬件加速),没有则回落软件
/// libx264 —— 回落时**逐字节等同旧行为、零回归**。选择由 `detect_video_encoder` 试编码探出、
/// `Relay` 进程级缓存(`Inner.hw_encoder`);"播放失败兜底重放"强制走 `Software`(最兼容)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoEncoder {
    /// libx264(软件,永远可用的回落)。
    Software,
    /// NVIDIA NVENC(`h264_nvenc`)。
    Nvenc,
    /// Intel Quick Sync(`h264_qsv`)。
    Qsv,
    /// AMD AMF(`h264_amf`)。
    Amf,
    /// Apple VideoToolbox(`h264_videotoolbox`,Mac 开发机)。
    VideoToolbox,
}

/// 追加视频编码参数到 ffmpeg 命令。**所有转码点共用这一处**(§4.8 单源):build_frag_cmd(HLS 段 /
/// 自适应视频段)、FileRemux(/m/)。质量目标 crf/cq≈23;硬件路加 `-profile:v high` 保 WebView2
/// 能解;`-pix_fmt yuv420p` 强制 8bit(10bit HEVC 一并压回,浏览器只认 8bit)。
/// **Software 分支保持与旧代码逐字节一致**(veryfast+crf23+yuv420p),不碰已验证的软件路。
fn apply_video_encode(cmd: &mut tokio::process::Command, enc: VideoEncoder) {
    match enc {
        VideoEncoder::Software => {
            cmd.args(["-c:v", "libx264", "-preset", "veryfast", "-crf", "23"]);
        }
        // p5 = 速度/画质折中(p1 最快~p7 最慢);vbr+cq 恒定质量(-b:v 0 让 cq 纯控质量不设码率)。
        VideoEncoder::Nvenc => {
            cmd.args([
                "-c:v", "h264_nvenc", "-preset", "p5", "-tune", "hq", "-rc", "vbr", "-cq", "23",
                "-b:v", "0", "-profile:v", "high",
            ]);
        }
        VideoEncoder::Qsv => {
            cmd.args([
                "-c:v", "h264_qsv", "-preset", "veryfast", "-global_quality", "23",
                "-profile:v", "high",
            ]);
        }
        VideoEncoder::Amf => {
            cmd.args([
                "-c:v", "h264_amf", "-rc", "cqp", "-qp_i", "23", "-qp_p", "23", "-profile:v", "high",
            ]);
        }
        VideoEncoder::VideoToolbox => {
            cmd.args(["-c:v", "h264_videotoolbox", "-q:v", "60", "-profile:v", "high"]);
        }
    }
    cmd.args(["-pix_fmt", "yuv420p"]);
}

/// 试编码一帧探这台机器能不能真用某编码器:编译进 ≠ 能用(如 h264_nvenc 编进了但没 N 卡 →
/// 运行时失败)。`color` 源出一帧喂给编码器 `-f null` 丢弃,退出码成功即可用。带 10s 超时防卡。
async fn probe_encoder(ffmpeg: &Path, name: &str) -> bool {
    let mut cmd = tokio::process::Command::new(ffmpeg);
    cmd.arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-nostdin")
        .arg("-f")
        .arg("lavfi")
        .arg("-i")
        .arg("color=c=black:s=256x256:r=5:d=0.4")
        .arg("-frames:v")
        .arg("1")
        .arg("-c:v")
        .arg(name)
        .arg("-f")
        .arg("null")
        .arg("-");
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    super::no_console(&mut cmd);
    matches!(
        tokio::time::timeout(std::time::Duration::from_secs(10), cmd.status()).await,
        Ok(Ok(s)) if s.success()
    )
}

/// 按平台优先级逐个试编码,取第一个真能用的硬件编码器;都不行回落 `Software`。
/// **探测不假设**(§4.11):从不硬认"有 GPU",全靠实测。整进程探一次(Relay 缓存)。
async fn detect_video_encoder(ffmpeg: &Path) -> VideoEncoder {
    let candidates: &[(&str, VideoEncoder)] = if cfg!(target_os = "windows") {
        &[
            ("h264_nvenc", VideoEncoder::Nvenc),
            ("h264_qsv", VideoEncoder::Qsv),
            ("h264_amf", VideoEncoder::Amf),
        ]
    } else if cfg!(target_os = "macos") {
        &[("h264_videotoolbox", VideoEncoder::VideoToolbox)]
    } else {
        &[]
    };
    for (name, enc) in candidates {
        if probe_encoder(ffmpeg, name).await {
            tracing::info!(encoder = name, "硬件视频编码器可用,转码走 GPU");
            return *enc;
        }
    }
    tracing::info!("无可用硬件视频编码器,转码回落 libx264(软件)");
    VideoEncoder::Software
}

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
    FileHls { path: PathBuf, ffmpeg: PathBuf, duration: f64, enc: VideoEncoder },
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
        /// 视频编码器(转码段用;copy 段不理会)。与 `video_init` 同编码器,故段的 avcC 与 init 一致。
        enc: VideoEncoder,
        /// 完整 MSE type:`video/mp4; codecs="avc1.xxxxxx"`(从视频 init 的 avcC 解出)。
        video_mime: String,
        /// 缓存的视频 init(ftyp+moov),vinit 端点直接回它(跨段 codec 配置一致)。
        video_init: Vec<u8>,
        /// 视频段计划 `(start, dur)`:copy=关键帧对齐变长,转码=固定 6s。
        segments: Vec<(f64, f64)>,
        duration: f64,
    },
}

/// HLS 切片时长(秒):段越短 seek 越细但请求越多;6s 是常见折中。自适应路(mod.rs)也用它当段目标。
pub(crate) const HLS_SEG: f64 = 6.0;
/// 自适应音频段时长(秒,固定网格 —— 音频无关键帧约束,任意帧可切)。
const AUDIO_SEG: f64 = 6.0;
/// 音频段左侧预卷(秒):切段时多切这么一段在前面,前端用 `appendWindowStart` 把它连同 AAC
/// 编码器 priming(~43ms 静音)一起裁掉 → 逐段独立编码也**无累计漂移**(gapless)。0.5s 足够盖住 priming。
const AUDIO_PREROLL: f64 = 0.5;

struct Inner {
    port: u16,
    streams: Mutex<HashMap<String, Arc<Entry>>>,
    net: crate::net::Client,
    counter: AtomicU64,
    /// 探出来的视频编码器(硬件优先),整进程探一次缓存;转码点复用,免每次试编码。
    hw_encoder: tokio::sync::OnceCell<VideoEncoder>,
    /// webrender 回传信箱(`POST /collect/{token}`,一次性):壳层隐藏窗里注入的脚本把
    /// 渲染后页面 POST 回来。token 用完即取走;窗超时收摊后残留项由发起方 drop Receiver 自清。
    collect: Mutex<HashMap<String, tokio::sync::oneshot::Sender<String>>>,
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
            streams: Mutex::new(HashMap::new()),
            // 上游是大文件流:只设建连超时,不设整体超时(空闲保护靠链路自身断流)。
            // 走统一 net::Client(CLAUDE.md §5):墙内 CDN 直连优先永不代理,未来墙外源(YouTube 等)直连失败自动落代理。
            net: crate::net::Client::new(|b| b.connect_timeout(std::time::Duration::from_secs(10))),
            counter: AtomicU64::new(1),
            hw_encoder: tokio::sync::OnceCell::new(),
            collect: Mutex::new(HashMap::new()),
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
            .lock()
            .expect("relay streams lock poisoned")
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
            let mut map = self.inner.collect.lock().expect("relay collect lock poisoned");
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
    ) -> String {
        let enc = if force_software {
            VideoEncoder::Software
        } else {
            self.video_encoder(&ffmpeg).await
        };
        self.register(Entry::FileRemux { path, ffmpeg, transcode_video, transcode_audio, enc }, "m")
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
        let vhead = fetch_head(&self.inner.net, &video, HEAD).await.context("取视频流头失败")?;
        let ahead = fetch_head(&self.inner.net, &audio, HEAD).await.context("取音频流头失败")?;
        let vsidx = super::probe::probe_sidx(&vhead)
            .context("视频流头里没有 sidx(非 SegmentBase 单文件 DASH)")?;
        let asidx = super::probe::probe_sidx(&ahead).context("音频流头里没有 sidx")?;
        let mpd = build_mpd(duration, &video, vsidx, &audio, asidx);
        let token = self.token();
        self.inner
            .streams
            .lock()
            .expect("relay streams lock poisoned")
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
    ) -> String {
        let enc = if force_software {
            VideoEncoder::Software
        } else {
            self.video_encoder(&ffmpeg).await
        };
        let token = self.token();
        self.inner.streams.lock().expect("relay streams lock poisoned").insert(
            token.clone(),
            Arc::new(Entry::FileHls { path, ffmpeg, duration, enc }),
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
        video_mime: String,
        video_init: Vec<u8>,
        segments: Vec<(f64, f64)>,
        duration: f64,
        enc: VideoEncoder,
    ) -> String {
        let token = self.token();
        self.inner.streams.lock().expect("relay streams lock poisoned").insert(
            token.clone(),
            Arc::new(Entry::FileAdaptive {
                path,
                ffmpeg,
                copy_video,
                enc,
                video_mime,
                video_init,
                segments,
                duration,
            }),
        );
        format!("http://127.0.0.1:{}/la/{token}/desc", self.inner.port)
    }
}

/// 生成视频 init(ftyp+moov):切 0.1s 分片、取首个 moof 之前的部分。注册自适应播放时调一次,
/// 既拿到 init 字节缓存(vinit 端点直接回),又能从中解出精确 codec 串(avcC)。失败 → None
/// (调用方回落 muxed HLS,不硬走分离路)。`copy_video` 与后续各段一致 → init 与段的 avcC 匹配。
pub async fn gen_video_init(
    ffmpeg: &Path,
    path: &Path,
    copy_video: bool,
    enc: VideoEncoder,
) -> Option<Vec<u8>> {
    let cmd = build_frag_cmd(ffmpeg, path, 0.0, 0.1, copy_video, true, enc);
    let full = run_ffmpeg_collect(cmd, 8 * 1024 * 1024).await?;
    let moof = super::probe::first_moof_offset(&full)?;
    Some(full[..moof].to_vec())
}

/// 取一条上游流的前段(≤cap 字节)用于探 sidx:带防盗链头 + Range;上游若忽略 Range 回 200 全量,
/// 也只读到 cap 就停(绝不把整片拉进内存)。失败 → None。
async fn fetch_head(net: &crate::net::Client, up: &UpStream, cap: u64) -> Option<Vec<u8>> {
    let mut resp = net
        .send(&up.url, |c| {
            let mut req = c.get(&up.url);
            for (k, v) in &up.headers {
                req = req.header(k, v);
            }
            req.header(axum::http::header::RANGE, format!("bytes=0-{}", cap - 1))
        })
        .await
        .ok()?;
    let mut buf: Vec<u8> = Vec::new();
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                buf.extend_from_slice(&chunk);
                if buf.len() as u64 >= cap {
                    buf.truncate(cap as usize);
                    break;
                }
            }
            Ok(None) => break,
            Err(_) => return None,
        }
    }
    Some(buf)
}

/// 合成一份 on-demand DASH MPD(纯函数,可测):两条单文件流各一个 Representation,用 SegmentBase
/// + indexRange(sidx)+ Initialization range。shaka 据此 Range 拉 init/index/段、自己管时间轴 →
/// 原生精确 seek + 音画同步。codecs/bandwidth 来自 yt-dlp(缺则给保守默认);音频采样率/声道 shaka
/// 会从 init 段读真值,故 MPD 不写、免对不上。段地址用相对 `v`/`a`(相对 manifest URL → /dash/{token}/v|a)。
fn build_mpd(
    duration: f64,
    video: &UpStream,
    vsidx: super::probe::SidxRanges,
    audio: &UpStream,
    asidx: super::probe::SidxRanges,
) -> String {
    let vcodec = video.vcodec.as_deref().unwrap_or("avc1.640028");
    let acodec = audio.acodec.as_deref().unwrap_or("mp4a.40.2");
    let vbw = video.bandwidth.unwrap_or(2_000_000);
    let abw = audio.bandwidth.unwrap_or(128_000);
    let w = video.width.unwrap_or(1920);
    let h = video.height.unwrap_or(1080);
    format!(
        concat!(
            r#"<?xml version="1.0" encoding="UTF-8"?>"#,
            "\n",
            r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" profiles="urn:mpeg:dash:profile:isoff-on-demand:2011" type="static" minBufferTime="PT2S" mediaPresentationDuration="PT{dur:.3}S">"#,
            "\n  <Period>\n",
            r#"    <AdaptationSet contentType="video" mimeType="video/mp4" segmentAlignment="true" startWithSAP="1">"#,
            "\n",
            r#"      <Representation id="v" bandwidth="{vbw}" codecs="{vcodec}" width="{w}" height="{h}">"#,
            "\n        <BaseURL>v</BaseURL>\n",
            r#"        <SegmentBase indexRange="{vif}-{vil}"><Initialization range="0-{vinit}"/></SegmentBase>"#,
            "\n      </Representation>\n    </AdaptationSet>\n",
            r#"    <AdaptationSet contentType="audio" mimeType="audio/mp4" segmentAlignment="true" startWithSAP="1">"#,
            "\n",
            r#"      <Representation id="a" bandwidth="{abw}" codecs="{acodec}">"#,
            "\n        <BaseURL>a</BaseURL>\n",
            r#"        <SegmentBase indexRange="{aif}-{ail}"><Initialization range="0-{ainit}"/></SegmentBase>"#,
            "\n      </Representation>\n    </AdaptationSet>\n  </Period>\n</MPD>\n",
        ),
        dur = duration,
        vbw = vbw,
        vcodec = vcodec,
        w = w,
        h = h,
        vif = vsidx.index_first,
        vil = vsidx.index_last,
        vinit = vsidx.init_last,
        abw = abw,
        acodec = acodec,
        aif = asidx.index_first,
        ail = asidx.index_last,
        ainit = asidx.init_last,
    )
}

/// 按扩展名给 Content-Type(WebView 解码选型用;不认识的交给 Chromium 嗅探)。
fn content_type_of(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).map(str::to_lowercase).as_deref() {
        Some("mp4") | Some("m4v") => "video/mp4",
        Some("mov") => "video/quicktime",
        Some("mkv") => "video/x-matroska",
        Some("webm") => "video/webm",
        Some("avi") => "video/x-msvideo",
        Some("m4a") => "audio/mp4",
        Some("mp3") => "audio/mpeg",
        Some("flac") => "audio/flac",
        Some("wav") => "audio/wav",
        Some("aac") => "audio/aac",
        Some("ogg") | Some("opus") => "audio/ogg",
        _ => "application/octet-stream",
    }
}

/// Range 头解析(只支持媒体元素实际会发的 `bytes=a-` / `bytes=a-b`;歪的当没有)。
enum RangeSpec {
    None,
    Span(u64, u64),
    Unsatisfiable,
}

fn parse_range(header: Option<&axum::http::HeaderValue>, len: u64) -> RangeSpec {
    let Some(raw) = header.and_then(|h| h.to_str().ok()) else { return RangeSpec::None };
    let Some(spec) = raw.strip_prefix("bytes=") else { return RangeSpec::None };
    let Some((a, b)) = spec.split_once('-') else { return RangeSpec::None };
    let Ok(start) = a.trim().parse::<u64>() else { return RangeSpec::None };
    if start >= len {
        return RangeSpec::Unsatisfiable;
    }
    let end = match b.trim() {
        "" => len - 1,
        s => match s.parse::<u64>() {
            Ok(e) => e.min(len - 1),
            Err(_) => return RangeSpec::None,
        },
    };
    if end < start {
        return RangeSpec::None;
    }
    RangeSpec::Span(start, end)
}

fn lookup(state: &Inner, token: &str) -> Option<Arc<Entry>> {
    state.streams.lock().expect("relay streams lock poisoned").get(token).cloned()
}

fn bad(status: StatusCode) -> Response {
    Response::builder().status(status).body(Body::empty()).expect("static response")
}

/// 直转:上游必需头 + 客户端 Range 透传,响应头/状态镜像回去。
/// WebView 首次请求就带 Range: bytes=0-,上游 206 + 总长 → 原生 seek 直接可用。
async fn direct(
    State(state): State<Arc<Inner>>,
    AxPath(token): AxPath<String>,
    headers: HeaderMap,
) -> Response {
    let Some(entry) = lookup(&state, &token) else { return bad(StatusCode::NOT_FOUND) };
    let Entry::Direct(up) = entry.as_ref() else { return bad(StatusCode::NOT_FOUND) };
    proxy_upstream(&state, up, &headers).await
}

/// 把一条上游流透传给客户端:带防盗链头、透传客户端 Range、镜像响应头/状态。`/s/`(`<video src>`)
/// 与 `/dash/…/v|a`(shaka `fetch`,跨源)共用。**带 CORS**:shaka 用 fetch 拉段是跨源请求(app 源
/// ≠ relay 回环端口),`<video src>` 不查 CORS 但 fetch 查 → 必须放行 + 暴露 Range 相关响应头。
async fn proxy_upstream(state: &Inner, up: &UpStream, client_headers: &HeaderMap) -> Response {
    let upstream = match state
        .net
        .send(&up.url, |c| {
            let mut req = c.get(&up.url);
            for (k, v) in &up.headers {
                req = req.header(k, v);
            }
            if let Some(range) = client_headers.get(axum::http::header::RANGE) {
                req = req.header(axum::http::header::RANGE, range);
            }
            req
        })
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("上游拉流失败: {e}");
            return bad(StatusCode::BAD_GATEWAY);
        }
    };

    let mut builder = Response::builder()
        .status(upstream.status().as_u16())
        .header("access-control-allow-origin", "*")
        .header("access-control-expose-headers", "Content-Length, Content-Range, Accept-Ranges");
    for key in ["content-type", "content-length", "content-range", "accept-ranges"] {
        if let Some(v) = upstream.headers().get(key) {
            builder = builder.header(key, v);
        }
    }
    let stream = upstream.bytes_stream().map_err(std::io::Error::other);
    builder.body(Body::from_stream(stream)).unwrap_or_else(|_| bad(StatusCode::INTERNAL_SERVER_ERROR))
}

/// DASH:`manifest.mpd` 返回合成的 MPD(带 CORS);`v`/`a` 把 shaka 的 Range 请求透传到对应上游。
async fn dash(
    State(state): State<Arc<Inner>>,
    AxPath((token, seg)): AxPath<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let Some(entry) = lookup(&state, &token) else { return bad(StatusCode::NOT_FOUND) };
    let Entry::Dash { mpd, video, audio } = entry.as_ref() else { return bad(StatusCode::NOT_FOUND) };
    match seg.as_str() {
        "manifest.mpd" => Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/dash+xml")
            .header("access-control-allow-origin", "*")
            .body(Body::from(mpd.clone()))
            .unwrap_or_else(|_| bad(StatusCode::INTERNAL_SERVER_ERROR)),
        "v" => proxy_upstream(&state, video, &headers).await,
        "a" => proxy_upstream(&state, audio, &headers).await,
        _ => bad(StatusCode::NOT_FOUND),
    }
}

/// CORS 预检(shaka 带 Range 的 fetch 可能先发 OPTIONS):放行 GET + Range。
async fn dash_preflight() -> Response {
    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .header("access-control-allow-origin", "*")
        .header("access-control-allow-methods", "GET, OPTIONS")
        .header("access-control-allow-headers", "Range")
        .header("access-control-max-age", "86400")
        .body(Body::empty())
        .unwrap_or_else(|_| bad(StatusCode::INTERNAL_SERVER_ERROR))
}

/// webrender 回传:一次性 token → 取走信箱、投递页面 JSON。未知/已用 token 一律 404
/// (页面世界不可信,不给探测面);载荷缓冲上限防恶意页灌爆(注入脚本自身只发 ~10KB)。
const COLLECT_MAX_BYTES: usize = 512 * 1024;

async fn collect(
    State(state): State<Arc<Inner>>,
    AxPath(token): AxPath<String>,
    body: String,
) -> Response {
    if body.len() > COLLECT_MAX_BYTES {
        return bad(StatusCode::PAYLOAD_TOO_LARGE);
    }
    let sender = state.collect.lock().expect("relay collect lock poisoned").remove(&token);
    let Some(tx) = sender else { return bad(StatusCode::NOT_FOUND) };
    let _ = tx.send(body); // 发起方已放弃(超时收摊)= 静默丢,无人可通知
    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .header("access-control-allow-origin", "*")
        .body(Body::empty())
        .unwrap_or_else(|_| bad(StatusCode::INTERNAL_SERVER_ERROR))
}

async fn collect_preflight() -> Response {
    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .header("access-control-allow-origin", "*")
        .header("access-control-allow-methods", "POST, OPTIONS")
        .header("access-control-allow-headers", "Content-Type")
        .header("access-control-max-age", "86400")
        .body(Body::empty())
        .unwrap_or_else(|_| bad(StatusCode::INTERNAL_SERVER_ERROR))
}

/// 本地 HLS:`index.m3u8` 合成完整 VOD 列表;`init.mp4` 取 ftyp+moov;`s{N}.m4s` 按需切第 N 段
/// (fMP4 单 moof,一律转码视频 + 立体声 AAC,见 build_frag_cmd)。无临时目录/无会话:每段现切现回。
async fn hls(State(state): State<Arc<Inner>>, AxPath((token, seg)): AxPath<(String, String)>) -> Response {
    let Some(entry) = lookup(&state, &token) else { return bad(StatusCode::NOT_FOUND) };
    let Entry::FileHls { path, ffmpeg, duration, enc } = entry.as_ref() else {
        return bad(StatusCode::NOT_FOUND);
    };
    // 三种请求:清单 / 共享 init / moof 段。段走 **fMP4(非 mpegts)** —— MSE 直接吃,绕开 shaka 的
    // mux.js transmux(实锤 2026-06-20:mpegts 视频段 append 到 MSE 失败 code=3015/3016 → 黑屏;
    // 音频没事就视频炸,正是 transmux 那步)。fMP4 = B 站 DASH 已验通的同路。
    if seg == "index.m3u8" {
        tracing::info!(duration = *duration, "HLS:发 manifest");
        return bytes_response(
            build_hls_playlist(*duration, HLS_SEG).into_bytes(),
            "application/vnd.apple.mpegurl",
        );
    }
    if seg == "init.mp4" {
        // 共享 init(ftyp+moov):切一小段、取首个 moof 之前的部分。codec 配置与各 moof 段一致
        //(同输入 + 同 copy/转码 flag → ffmpeg 产出确定、跨调用兼容,Mac 已验 init+moof 可拼)。
        tracing::info!("HLS:发 init");
        let cmd = build_frag_cmd(ffmpeg, path, 0.0, 0.1, false, false, *enc);
        let Some(full) = run_ffmpeg_collect(cmd, 8 * 1024 * 1024).await else {
            return bad(StatusCode::BAD_GATEWAY);
        };
        let Some(moof) = super::probe::first_moof_offset(&full) else {
            tracing::warn!("HLS init:没找到 moof");
            return bad(StatusCode::INTERNAL_SERVER_ERROR);
        };
        return bytes_response(full[..moof].to_vec(), "video/mp4");
    }
    // s{N}.m4s → 第 N 段 [N*SEG, N*SEG+SEG):自包含分片 mp4,切掉 init、只回 moof+mdat。
    let Some(n) = seg
        .strip_prefix('s')
        .and_then(|s| s.strip_suffix(".m4s"))
        .and_then(|s| s.parse::<u64>().ok())
    else {
        return bad(StatusCode::NOT_FOUND);
    };
    let start = n as f64 * HLS_SEG;
    if start >= *duration {
        return bad(StatusCode::NOT_FOUND);
    }
    tracing::info!(seg = n, start, "HLS:现切一段");
    let cmd = build_frag_cmd(ffmpeg, path, start, HLS_SEG, false, false, *enc);
    let Some(full) = run_ffmpeg_collect(cmd, 256 * 1024 * 1024).await else {
        return bad(StatusCode::BAD_GATEWAY);
    };
    let Some(moof) = super::probe::first_moof_offset(&full) else {
        tracing::warn!(seg = n, "HLS 段:没找到 moof");
        return bad(StatusCode::INTERNAL_SERVER_ERROR);
    };
    // 段体 = moof+mdat(剔除 -f mp4 在尾部写的 mfra),并把被重置为 0 的 tfdt 改回累计起点
    //(start×timescale)→ 标准累计 tfdt fMP4-HLS,shaka 直接按 tfdt 拼接、各段落到正确时间轴。
    let end = super::probe::moof_segment_end(&full, moof);
    let mut body = full[moof..end].to_vec();
    let ts = super::probe::init_timescales(&full[..moof]);
    super::probe::patch_segment_tfdt(&mut body, &ts, start);
    bytes_response(body, "video/mp4")
}

/// 本地自适应(音视频分离,手写 MSE):`desc`(JSON:两轨 mime + 视频段清单 + 时长)/
/// `vinit`(缓存视频 init)/`v{N}`(视频段 N:copy/转码 + tfdt 累积,video-only)/
/// `ainit`(音频 init)/`a{N}`(音频段 N:固定 6s 网格 + 左预卷,离散完整响应,前端 appendWindow 裁 priming)。
async fn local_adaptive(
    State(state): State<Arc<Inner>>,
    AxPath((token, seg)): AxPath<(String, String)>,
) -> Response {
    let Some(entry) = lookup(&state, &token) else { return bad(StatusCode::NOT_FOUND) };
    let Entry::FileAdaptive {
        path,
        ffmpeg,
        copy_video,
        enc,
        video_mime,
        video_init,
        segments,
        duration,
    } = entry.as_ref()
    else {
        return bad(StatusCode::NOT_FOUND);
    };

    if seg == "desc" {
        let segs: String = segments
            .iter()
            .enumerate()
            .map(|(i, (s, d))| {
                format!("{}{{\"start\":{s:.6},\"dur\":{d:.6}}}", if i == 0 { "" } else { "," })
            })
            .collect();
        let json = format!(
            "{{\"videoMime\":{vm},\"audioMime\":\"audio/mp4; codecs=\\\"mp4a.40.2\\\"\",\"duration\":{dur:.6},\"copyVideo\":{cv},\"audioSeg\":{aseg},\"audioPreroll\":{apre},\"segments\":[{segs}]}}",
            vm = json_string(video_mime),
            dur = duration,
            cv = copy_video,
            aseg = AUDIO_SEG,
            apre = AUDIO_PREROLL,
        );
        return json_response(json);
    }
    if seg == "vinit" {
        return bytes_response(video_init.clone(), "video/mp4");
    }
    // 音频改**离散段**(不再流式 —— WebView2 的 fetch 不吐流式 body,实锤 abuf=[空] 卡死;离散完整
    // 响应 WebView2 收得下,同视频段)。ainit=音频 init;a{N}=第 N 段(固定 6s 网格,带左预卷供前端
    // appendWindow 裁掉 priming → gapless 无漂移)。段内 tfdt=0,前端靠 timestampOffset+appendWindow 定位。
    if seg == "ainit" {
        let cmd = build_audio_frag_cmd(ffmpeg, path, 0.0, 0.1);
        let Some(full) = run_ffmpeg_collect(cmd, 4 * 1024 * 1024).await else {
            return bad(StatusCode::BAD_GATEWAY);
        };
        let Some(moof) = super::probe::first_moof_offset(&full) else {
            return bad(StatusCode::INTERNAL_SERVER_ERROR);
        };
        return bytes_response(full[..moof].to_vec(), "audio/mp4");
    }
    if let Some(n) = seg.strip_prefix('a').and_then(|s| s.parse::<usize>().ok()) {
        let grid = n as f64 * AUDIO_SEG;
        if grid >= *duration {
            return bad(StatusCode::NOT_FOUND);
        }
        let seg_dur = (duration - grid).min(AUDIO_SEG);
        // N>0 左移 preroll 多切一段(前端 appendWindow 裁掉);N=0 从头切(起点无 priming 可裁,留着即可)。
        let (ss, cut) =
            if n > 0 { (grid - AUDIO_PREROLL, seg_dur + AUDIO_PREROLL) } else { (0.0, seg_dur) };
        tracing::info!(seg = n, grid, "自适应:现切音频段");
        let cmd = build_audio_frag_cmd(ffmpeg, path, ss, cut);
        let Some(full) = run_ffmpeg_collect(cmd, 16 * 1024 * 1024).await else {
            return bad(StatusCode::BAD_GATEWAY);
        };
        let Some(moof) = super::probe::first_moof_offset(&full) else {
            return bad(StatusCode::INTERNAL_SERVER_ERROR);
        };
        let end = super::probe::moof_segment_end(&full, moof);
        // tfdt 不改(=0):前端用 timestampOffset=grid-preroll 放到真时间轴,appendWindow 裁到 [grid, grid+dur]。
        return bytes_response(full[moof..end].to_vec(), "audio/mp4");
    }
    // v{N} → 视频段 N(video-only 分片,tfdt 改累计起点;与 HLS 段同款处理,只是无音轨)。
    let Some(n) = seg.strip_prefix('v').and_then(|s| s.parse::<usize>().ok()) else {
        return bad(StatusCode::NOT_FOUND);
    };
    let Some(&(start, dur)) = segments.get(n) else { return bad(StatusCode::NOT_FOUND) };
    tracing::info!(seg = n, start, dur, copy = copy_video, "自适应:现切视频段");
    let cmd = build_frag_cmd(ffmpeg, path, start, dur, *copy_video, true, *enc);
    let Some(full) = run_ffmpeg_collect(cmd, 256 * 1024 * 1024).await else {
        return bad(StatusCode::BAD_GATEWAY);
    };
    let Some(moof) = super::probe::first_moof_offset(&full) else {
        tracing::warn!(seg = n, "自适应视频段:没找到 moof");
        return bad(StatusCode::INTERNAL_SERVER_ERROR);
    };
    let end = super::probe::moof_segment_end(&full, moof);
    let mut body = full[moof..end].to_vec();
    // 用缓存的 video_init 解 timescale(段本身无 moov)→ tfdt 改成累计起点 start×ts。
    let ts = super::probe::init_timescales(video_init);
    super::probe::patch_segment_tfdt(&mut body, &ts, start);
    bytes_response(body, "video/mp4")
}

/// 转义成 JSON 字符串字面量(video_mime 含 `codecs="…"` 的双引号,必须转义)。
fn json_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// 带 CORS 的 JSON 响应(desc 被前端跨源 fetch)。
fn json_response(body: String) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .header("access-control-allow-origin", "*")
        .body(Body::from(body))
        .unwrap_or_else(|_| bad(StatusCode::INTERNAL_SERVER_ERROR))
}

/// 音频统一响度:先下混到立体声、整段统一提量、再限幅防削顶 —— 修「转码后整段偏小(人声+音效一起小)」。
/// 做的是「整段一起抬」,**不挑人声**(与用户讨论:不是对白被埋,是整体衰减 —— 5.1→立体声下混归一化
/// 约 −8dB + AC3/DTS 的 DRC 被 ffmpeg 套上)。`volume` 必须在下混**之后**提量才有效(故都塞进 `-af`,
/// 别用 `-ac 2`——那是输出选项、在滤镜之后跑,会把提过的量又除回去);`alimiter` 只削最尖的峰值
/// (防「系统音量开太大被吓到」),音效整体力度保留。**值是真机可调项**:实际增益/限幅只能
/// Windows 真机 + 真 5.1 片验(§8.1)。改这一处即改全部转码音频(build_frag_cmd 的 HLS 段 /
/// FileRemux 的 `/m/` 流式混流共用)。
/// **增益 2026-07-04 由 +8dB 降到 +5dB**:真机反馈响的时候「偶尔轻微破音」—— +8dB 把响段推进
/// alimiter 太狠,重限幅出失真。−3dB 后仍明显提量(从下混/DRC 的偏小里拉回来),破音余量更足。
/// 还破就继续调小这一个数;想更响调回去。
pub(crate) const AUDIO_LOUDNESS_AF: &str =
    "aformat=channel_layouts=stereo,volume=5dB,alimiter=limit=0.95";

/// 构建「区间 [start, start+dur) 的自包含分片 mp4」命令(ftyp+moov+moof+mdat 吐 stdout)。
/// HLS 的 init(取 0.1s)与各段(取 HLS_SEG)都用它。
///
/// **视频、音频一律转码(不 copy),这是按需 fMP4-HLS 在 WebView2/Chromium MSE 上稳的前提**
/// (三处实证,2026-06-20 Mac Chromium MSE 复现):
/// ① **视频转码**:`-ss` + `-c:v copy` 只能落到关键帧、切不准 → 段时长漂(实测请求 6s 出 8s)、
///    段间重叠/错位;转码则每段从干净 IDR 起、恰好 dur 秒、编码配置(avcC)跨段一致 → MSE 拼得上。
/// ② **音频必转码**:视频转码 + 音频 `copy` 时,fragmented muxer 把样本时长写成 2×(段被拉长一倍,
///    实测),两轨都转码即消失。
/// ③ **下混立体声 + 统一响度(`-af AUDIO_LOUDNESS_AF`)**:多声道 AAC 声道布局不明确会被 MSE **拒绝 append**
///    整个 init(报在 video 轨上,正是用户「video:2 code=3014 黑屏」)→ aformat 下混立体声永远能 append;
///    顺带整段提量 + 限幅,修「转码后整段偏小」(替掉原来的 `-ac 2`,见 AUDIO_LOUDNESS_AF 说明)。
/// 代价 = 已是 H.264 的片子(仅因容器 mkv / 音轨 AC3 才进 HLS)也被重编视频,弱机吃 CPU;
/// 但这些片子当前本就黑屏(mpegts 链路),不是回退。**0.2.6 起「视频已兼容」的片走音视频分离
/// 的 `FileAdaptive` 路(视频 `-c:v copy`、音频一整条连续编码),省 CPU + 治漂移**(见下);
/// 本函数的「muxed HLS(视频转码 + 逐段音频)」只兜「视频轨也不兼容(HEVC/AV1)且不走分离路」的老链路。
///
/// `copy_video`:true = `-c:v copy`(视频已兼容,零重编码,须段界落关键帧);false = 转 H.264。
/// `video_only`:true = `-an`(分离路的视频段,音频另走连续流);false = 带音轨(老 muxed HLS)。
fn build_frag_cmd(
    ffmpeg: &Path,
    path: &Path,
    start: f64,
    dur: f64,
    copy_video: bool,
    video_only: bool,
    enc: VideoEncoder,
) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(ffmpeg);
    cmd.arg("-hide_banner").arg("-loglevel").arg("error").arg("-nostdin");
    if start > 0.0 {
        // copy 必须精确落在关键帧上:高精度 + 微正 margin(COPY_SS_EPS),防浮点舍入落到**前一个**
        // 关键帧(关键帧间距 ≫ margin,不会误跳到下一个)。转码不挑关键帧(从新 IDR 重编),用 .3 即可。
        let ss = if copy_video {
            format!("{:.6}", start + COPY_SS_EPS)
        } else {
            format!("{start:.3}")
        };
        cmd.arg("-ss").arg(ss);
    }
    cmd.arg("-i").arg(path).arg("-t").arg(format!("{dur:.6}"));
    cmd.arg("-map").arg("0:v:0?");
    if !video_only {
        cmd.arg("-map").arg("0:a:0?");
    }
    if copy_video {
        cmd.arg("-c:v").arg("copy"); // 视频已兼容:原样搬,不掉画质、CPU 近零
    } else {
        apply_video_encode(&mut cmd, enc); // 硬件优先(省 CPU),回落 libx264 与旧行为一致
    }
    if !video_only {
        cmd.arg("-c:a").arg("aac").arg("-af").arg(AUDIO_LOUDNESS_AF).arg("-b:a").arg("256k");
    }
    // 单 moof/段:`-frag_duration` 给个远超段长(600s)的值 → ffmpeg 不在段内再分片,整段就一个
    // moof+mdat,便于把 tfdt 改成累计值(见 probe::patch_segment_tfdt)。default_base_moof 让 trun
    // 数据偏移相对 moof → 切掉 init 后偏移仍对。
    cmd.arg("-movflags").arg("empty_moov+default_base_moof")
        .arg("-frag_duration").arg("600000000")
        .arg("-f").arg("mp4").arg("pipe:1");
    cmd
}

/// copy 段 `-ss` 的微正 margin(秒):够跨过浮点舍入、又远小于任何关键帧间距(≥ 一帧 ~16ms)。
const COPY_SS_EPS: f64 = 0.001;

/// 构建音频段命令(`-vn` 纯音频 → AAC 立体声 + 响度,单 moof 分片吐 stdout)。`ss>0` 从该秒输入 seek
/// (段带左预卷时用),`dur` = 要切的时长(含预卷)。init 段取 `ss=0,dur=0.1`。tfdt 由 ffmpeg 归零,
/// 前端 timestampOffset + appendWindow 定位/裁剪。
fn build_audio_frag_cmd(ffmpeg: &Path, path: &Path, ss: f64, dur: f64) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(ffmpeg);
    cmd.arg("-hide_banner").arg("-loglevel").arg("error").arg("-nostdin");
    if ss > 0.0 {
        cmd.arg("-ss").arg(format!("{ss:.6}"));
    }
    cmd.arg("-i").arg(path).arg("-t").arg(format!("{dur:.6}")).arg("-vn")
        .arg("-c:a").arg("aac").arg("-af").arg(AUDIO_LOUDNESS_AF).arg("-b:a").arg("256k")
        .arg("-movflags").arg("empty_moov+default_base_moof")
        .arg("-frag_duration").arg("600000000")
        .arg("-f").arg("mp4").arg("pipe:1");
    cmd
}

/// 跑 ffmpeg、把 stdout 整段收进内存(封顶 cap;分片就几 MB~几十 MB,稳)。失败/空 → None(并记 stderr)。
async fn run_ffmpeg_collect(mut cmd: tokio::process::Command, cap: usize) -> Option<Vec<u8>> {
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    super::no_console(&mut cmd);
    let mut child = cmd.spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let mut stderr = child.stderr.take()?;
    let mut buf = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        match stdout.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(k) => {
                buf.extend_from_slice(&chunk[..k]);
                if buf.len() > cap {
                    tracing::warn!("HLS 段超上限 {cap} 字节,截断");
                    break;
                }
            }
        }
    }
    let _ = child.wait().await;
    if buf.is_empty() {
        let mut err = String::new();
        let _ = stderr.read_to_string(&mut err).await;
        if !err.trim().is_empty() {
            tracing::warn!("HLS ffmpeg stderr: {}", err.trim());
        }
        return None;
    }
    Some(buf)
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

/// 合成完整 VOD HLS 播放列表(纯函数,可测):**fMP4 段**——EXT-X-MAP 指共享 init,段为 `s{N}.m4s`。
/// 全段列出 → shaka 知道完整时长、可任意 seek。段数 = ceil(duration/seg);末段时长 = 余量。
fn build_hls_playlist(duration: f64, seg: f64) -> String {
    let n = (duration / seg).ceil().max(1.0) as u64;
    let mut s = String::from("#EXTM3U\n#EXT-X-VERSION:7\n");
    s.push_str(&format!("#EXT-X-TARGETDURATION:{}\n", seg.ceil() as u64));
    s.push_str("#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-PLAYLIST-TYPE:VOD\n");
    s.push_str("#EXT-X-MAP:URI=\"init.mp4\"\n");
    for i in 0..n {
        let dur = (duration - i as f64 * seg).clamp(0.0, seg);
        s.push_str(&format!("#EXTINF:{dur:.3},\ns{i}.m4s\n"));
    }
    s.push_str("#EXT-X-ENDLIST\n");
    s
}

/// 本地文件:Range 透传的文件流(原生 seek 白送);UNC/挂载盘符就是普通路径。
async fn file(
    State(state): State<Arc<Inner>>,
    AxPath(token): AxPath<String>,
    headers: HeaderMap,
) -> Response {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};

    let Some(entry) = lookup(&state, &token) else { return bad(StatusCode::NOT_FOUND) };
    let Entry::File(path) = entry.as_ref() else { return bad(StatusCode::NOT_FOUND) };
    let mut f = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(path = %path.display(), "本地文件打不开: {e}");
            return bad(StatusCode::NOT_FOUND);
        }
    };
    let len = match f.metadata().await {
        Ok(m) => m.len(),
        Err(_) => return bad(StatusCode::INTERNAL_SERVER_ERROR),
    };
    let ctype = content_type_of(path);

    match parse_range(headers.get(axum::http::header::RANGE), len) {
        RangeSpec::Unsatisfiable => Response::builder()
            .status(StatusCode::RANGE_NOT_SATISFIABLE)
            .header("content-range", format!("bytes */{len}"))
            .body(Body::empty())
            .unwrap_or_else(|_| bad(StatusCode::INTERNAL_SERVER_ERROR)),
        RangeSpec::None => Response::builder()
            .status(StatusCode::OK)
            .header("content-type", ctype)
            .header("content-length", len)
            .header("accept-ranges", "bytes")
            // CORS:响度均衡以 crossorigin 接管 <audio>/<video> 时 /f/ 也要放行,否则设了 crossOrigin
            // 的元素会加载失败(与 /dash/ /s/ 一致)。本地文件/TTS/试听都走 /f/。
            .header("access-control-allow-origin", "*")
            .header("access-control-expose-headers", "Content-Length, Content-Range, Accept-Ranges")
            .body(Body::from_stream(tokio_util::io::ReaderStream::new(f)))
            .unwrap_or_else(|_| bad(StatusCode::INTERNAL_SERVER_ERROR)),
        RangeSpec::Span(start, end) => {
            if f.seek(std::io::SeekFrom::Start(start)).await.is_err() {
                return bad(StatusCode::INTERNAL_SERVER_ERROR);
            }
            let take = f.take(end - start + 1);
            Response::builder()
                .status(StatusCode::PARTIAL_CONTENT)
                .header("content-type", ctype)
                .header("content-length", end - start + 1)
                .header("content-range", format!("bytes {start}-{end}/{len}"))
                .header("accept-ranges", "bytes")
                // CORS:同上(媒体元素首个 `Range: bytes=0-` 请求走这条 206,crossorigin 下必须放行)。
                .header("access-control-allow-origin", "*")
                .header("access-control-expose-headers", "Content-Length, Content-Range, Accept-Ranges")
                .body(Body::from_stream(tokio_util::io::ReaderStream::new(take)))
                .unwrap_or_else(|_| bad(StatusCode::INTERNAL_SERVER_ERROR))
        }
    }
}

#[derive(serde::Deserialize, Default)]
struct RemuxQuery {
    /// 起播秒(seek = 换 src 重启混流,前端自己记位移)。
    #[serde(default)]
    t: f64,
}

/// 混流:经 ffmpeg 拼 fMP4 吐 stdout。无总长、不可 Range —— <video> 按渐进流播;两种来源:
///   Remux      两路网络上游 `-c copy`(B 站 DASH);
///   FileRemux  单个本地文件,视频 `-c:v copy` + 音轨转 AAC(AC3/DTS 本地片,见 probe.rs)。
/// 共用 stream_ffmpeg 起进程吐流。child 的生死跟着搬运任务走(响应体被 drop → 搬运 send
/// 失败 → 任务退出 → child drop → kill_on_drop 收尸),与 llm 取消同一个所有权手法。
async fn remux(
    State(state): State<Arc<Inner>>,
    AxPath(token): AxPath<String>,
    Query(q): Query<RemuxQuery>,
) -> Response {
    let Some(entry) = lookup(&state, &token) else { return bad(StatusCode::NOT_FOUND) };
    let mut cmd = match entry.as_ref() {
        Entry::Remux { video, audio, ffmpeg } => {
            let mut cmd = tokio::process::Command::new(ffmpeg);
            cmd.arg("-hide_banner").arg("-loglevel").arg("error").arg("-nostdin");
            for up in [video, audio] {
                if q.t > 0.0 {
                    cmd.arg("-ss").arg(format!("{:.3}", q.t));
                }
                if !up.headers.is_empty() {
                    let joined: String =
                        up.headers.iter().map(|(k, v)| format!("{k}: {v}\r\n")).collect();
                    cmd.arg("-headers").arg(joined);
                }
                cmd.arg("-i").arg(&up.url);
            }
            cmd.arg("-map").arg("0:v:0").arg("-map").arg("1:a:0")
                .arg("-c").arg("copy"); // 纯复制不转码:CPU 几乎零开销
            cmd
        }
        Entry::FileRemux { path, ffmpeg, transcode_video, transcode_audio, enc } => {
            let mut cmd = tokio::process::Command::new(ffmpeg);
            cmd.arg("-hide_banner").arg("-loglevel").arg("error").arg("-nostdin");
            if q.t > 0.0 {
                cmd.arg("-ss").arg(format!("{:.3}", q.t)); // 输入 seek(对 copy 是关键帧对齐)
            }
            cmd.arg("-i").arg(path);
            // 首条视频可选(纯音频不报错)+ 首条音轨可选(无声轨不报错);字幕等不带。
            cmd.arg("-map").arg("0:v:0?").arg("-map").arg("0:a:0?");
            if *transcode_video {
                // HEVC/AV1 等转 H.264:硬件优先(省 CPU),回落 libx264;yuv420p 把 10bit 压回 8bit
                //(浏览器只认),否则 H.264 10bit 一样放不了。参数收口 apply_video_encode(§4.8 单源)。
                apply_video_encode(&mut cmd, *enc);
            } else {
                cmd.arg("-c:v").arg("copy"); // 视频兼容:原样搬,不掉画质、CPU 近零
            }
            if *transcode_audio {
                // 下混立体声(源可能 5.1,直出多声道 AAC 浏览器放不了)+ 统一响度(修转码后整段偏小)。
                cmd.arg("-c:a").arg("aac").arg("-af").arg(AUDIO_LOUDNESS_AF).arg("-b:a").arg("256k");
            } else {
                cmd.arg("-c:a").arg("copy");
            }
            // 注:拖动 seek 后的音画同步是 /m/ 重启式 seek 的固有难题(copy 视频回退关键帧),
            // 网络 DASH 两路输入同样存在;修法是方向决策(见与用户的讨论),不在此处堆 flag。
            cmd
        }
        _ => return bad(StatusCode::NOT_FOUND),
    };
    // 两条路都吐流式 fMP4(渐进播);HLS 段走另一条(mpegts,见 hls_segment)。
    cmd.arg("-movflags")
        .arg("frag_keyframe+empty_moov+default_base_moof")
        .arg("-f")
        .arg("mp4")
        .arg("pipe:1");
    // cors=true:crossorigin 接管 <video> 做响度均衡时,/m/(本地混流/网络 DASH 混流)也要放行。
    stream_ffmpeg(cmd, "video/mp4", true)
}

/// 给一个**已配好程序+参数+输出格式(`-f … pipe:1`)**的 ffmpeg 命令收口 stdio、起进程、把
/// stdout 搬成 HTTP 流。三条路共用:网络 DASH 混流 / 本地 fMP4 混流(content_type=video/mp4)、
/// 本地 HLS 按需切片(content_type=video/mp2t,cors=true 给 shaka fetch)。child 生死跟搬运任务走
/// (响应体被 drop → send 失败 → 任务退出 → kill_on_drop 收尸)。
fn stream_ffmpeg(mut cmd: tokio::process::Command, content_type: &'static str, cors: bool) -> Response {
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    super::no_console(&mut cmd); // Windows 下不弹控制台黑框

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("ffmpeg 起不来: {e}");
            return bad(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(8);
    tokio::spawn(async move {
        let _child = &mut child; // 搬运任务持有 child:任务退出才轮到 kill_on_drop
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match stdout.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(Ok(bytes::Bytes::copy_from_slice(&buf[..n]))).await.is_err() {
                        break; // 客户端不要了(暂停换台/关窗):停止搬运,child 随后被杀
                    }
                }
            }
        }
        // 正常收尾或被弃读:收掉 stderr 留诊断,然后让 child drop
        let mut err = String::new();
        let _ = stderr.read_to_string(&mut err).await;
        if !err.trim().is_empty() {
            tracing::warn!("ffmpeg stderr: {}", err.trim());
        }
    });

    let mut builder = Response::builder().status(StatusCode::OK).header("content-type", content_type);
    if cors {
        // HLS 段被 shaka 用 fetch 拉(跨源:app 源 ≠ relay 回环口)→ 必须放行。
        builder = builder.header("access-control-allow-origin", "*");
    }
    builder
        .body(Body::from_stream(tokio_stream_from(rx)))
        .unwrap_or_else(|_| bad(StatusCode::INTERNAL_SERVER_ERROR))
}

fn tokio_stream_from(
    rx: tokio::sync::mpsc::Receiver<Result<bytes::Bytes, std::io::Error>>,
) -> impl futures_util::Stream<Item = Result<bytes::Bytes, std::io::Error>> {
    futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// webrender 回传信箱:注册 → 页面 POST → 接收端拿到原文;token 一次性(二投 404);
    /// 发起方放弃(drop rx)的死项被下次注册清扫。
    #[tokio::test]
    async fn collect_mailbox_roundtrip_once_and_sweeps_dead() {
        let relay = Relay::start().await.unwrap();
        let (url, rx) = relay.register_collect();
        let http = reqwest::Client::new();
        let resp = http.post(&url).body("{\"title\":\"页\"}").send().await.unwrap();
        assert!(resp.status().is_success());
        assert_eq!(rx.await.unwrap(), "{\"title\":\"页\"}");
        // 同 token 再投 = 404(一次性)
        let resp = http.post(&url).body("x").send().await.unwrap();
        assert_eq!(resp.status().as_u16(), 404);
        // 放弃的信箱:drop rx → 下次注册清扫,不堆积
        let (_url2, rx2) = relay.register_collect();
        drop(rx2);
        let _ = relay.register_collect();
        assert_eq!(relay.inner.collect.lock().unwrap().len(), 1, "死项被清扫,只剩新注册的");
    }

    #[test]
    fn tokens_are_unique_and_urls_local() {
        let inner = Arc::new(Inner {
            port: 12345,
            streams: Mutex::new(HashMap::new()),
            net: crate::net::Client::new(|b| b),
            counter: AtomicU64::new(1),
            hw_encoder: tokio::sync::OnceCell::new(),
            collect: Mutex::new(HashMap::new()),
        });
        let relay = Relay { inner };
        let a = relay.register_direct(UpStream { url: "u1".into(), ..Default::default() });
        let b = relay.register_direct(UpStream { url: "u2".into(), ..Default::default() });
        assert_ne!(a, b);
        assert!(a.starts_with("http://127.0.0.1:12345/s/"));
        assert_eq!(relay.inner.streams.lock().unwrap().len(), 2);
    }

    /// 直转端到端:本地起一个假上游,断言防盗链头与 Range 都透传、响应镜像。
    #[tokio::test]
    async fn direct_passes_headers_and_range_through() {
        use axum::routing::get as aget;

        // 假上游:校验 Referer + Range,回 206
        async fn upstream(headers: HeaderMap) -> Response {
            assert_eq!(headers.get("referer").unwrap(), "https://www.bilibili.com/");
            assert_eq!(headers.get("range").unwrap(), "bytes=3-");
            Response::builder()
                .status(206)
                .header("content-type", "audio/mp4")
                .header("content-range", "bytes 3-9/10")
                .body(Body::from("3456789"))
                .unwrap()
        }
        let up_listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let up_port = up_listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(up_listener, Router::new().route("/a.m4a", aget(upstream))).await.ok();
        });

        let relay = Relay::start().await.unwrap();
        let url = relay.register_direct(UpStream {
            url: format!("http://127.0.0.1:{up_port}/a.m4a"),
            headers: vec![("Referer".into(), "https://www.bilibili.com/".into())],
            ..Default::default()
        });

        let resp = reqwest::Client::new()
            .get(&url)
            .header("Range", "bytes=3-")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 206);
        assert_eq!(resp.headers().get("content-range").unwrap(), "bytes 3-9/10");
        assert_eq!(resp.text().await.unwrap(), "3456789");

        // 未注册 token = 404
        let nf = reqwest::get(format!(
            "http://127.0.0.1:{}/s/deadbeef",
            relay.inner.port
        ))
        .await
        .unwrap();
        assert_eq!(nf.status().as_u16(), 404);
    }

    #[test]
    fn hls_playlist_lists_all_segments_vod() {
        let m = build_hls_playlist(20.0, 6.0); // 20s/6 → 4 段(6,6,6,2)
        assert!(m.starts_with("#EXTM3U"));
        assert!(m.contains("#EXT-X-VERSION:7"), "fMP4 段需 v7");
        assert!(m.contains("#EXT-X-MAP:URI=\"init.mp4\""), "fMP4 共享 init");
        assert!(m.contains("#EXT-X-PLAYLIST-TYPE:VOD") && m.contains("#EXT-X-ENDLIST"), "完整 VOD → 可任意 seek");
        for seg in ["s0.m4s", "s1.m4s", "s2.m4s", "s3.m4s"] {
            assert!(m.contains(seg), "应列出 {seg}");
        }
        assert!(!m.contains("s4.m4s"), "只 4 段");
        assert!(m.contains("#EXTINF:6.000,") && m.contains("#EXTINF:2.000,"), "首段 6s、末段余量 2s");
        // 短于一段的片子也至少出一段
        assert!(build_hls_playlist(3.0, 6.0).contains("s0.m4s"));
    }

    #[test]
    fn apply_video_encode_maps_each_encoder() {
        let args_for = |enc| {
            let mut c = tokio::process::Command::new("ffmpeg");
            apply_video_encode(&mut c, enc);
            c.as_std().get_args().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<String>>()
        };
        // 软件路 = 旧行为逐字节一致(libx264 veryfast crf23 + yuv420p),防回归。
        let sw = args_for(VideoEncoder::Software);
        assert_eq!(
            sw.iter().map(String::as_str).collect::<Vec<_>>(),
            ["-c:v", "libx264", "-preset", "veryfast", "-crf", "23", "-pix_fmt", "yuv420p"]
        );
        // 各硬件路:首个是 -c:v <对应编码器> + high profile + 末尾 -pix_fmt yuv420p。
        for (enc, name) in [
            (VideoEncoder::Nvenc, "h264_nvenc"),
            (VideoEncoder::Qsv, "h264_qsv"),
            (VideoEncoder::Amf, "h264_amf"),
            (VideoEncoder::VideoToolbox, "h264_videotoolbox"),
        ] {
            let a = args_for(enc);
            assert_eq!(a[0].as_str(), "-c:v");
            assert_eq!(a[1].as_str(), name);
            assert!(a.iter().any(|s| s == "high"), "{name} 应带 -profile:v high");
            assert_eq!(a.last().unwrap().as_str(), "yuv420p", "{name} 末尾应 -pix_fmt yuv420p");
        }
    }

    #[test]
    fn build_mpd_embeds_codecs_ranges_duration() {
        use super::super::probe::SidxRanges;
        let video = UpStream {
            vcodec: Some("avc1.640028".into()),
            width: Some(1920),
            height: Some(1080),
            bandwidth: Some(3_000_000),
            ..Default::default()
        };
        let audio = UpStream {
            acodec: Some("mp4a.40.2".into()),
            bandwidth: Some(128_000),
            ..Default::default()
        };
        let mpd = build_mpd(
            3600.5,
            &video,
            SidxRanges { init_last: 799, index_first: 800, index_last: 1199 },
            &audio,
            SidxRanges { init_last: 599, index_first: 600, index_last: 699 },
        );
        // 编码、码率、时长、两路 SegmentBase 的 init/index range 都进 MPD
        assert!(mpd.contains(r#"codecs="avc1.640028""#) && mpd.contains(r#"codecs="mp4a.40.2""#));
        assert!(mpd.contains(r#"bandwidth="3000000""#) && mpd.contains(r#"bandwidth="128000""#));
        assert!(mpd.contains("PT3600.500S"), "时长 ISO8601");
        assert!(mpd.contains(r#"indexRange="800-1199""#) && mpd.contains(r#"range="0-799""#), "视频 init/index");
        assert!(mpd.contains(r#"indexRange="600-699""#) && mpd.contains(r#"range="0-599""#), "音频 init/index");
        assert!(mpd.contains("<BaseURL>v</BaseURL>") && mpd.contains("<BaseURL>a</BaseURL>"), "段相对地址");
        assert!(mpd.contains(r#"width="1920""#) && mpd.contains(r#"height="1080""#));
    }

    #[test]
    fn content_types_by_extension() {
        assert_eq!(content_type_of(Path::new("a.MP4")), "video/mp4");
        assert_eq!(content_type_of(Path::new("b.m4a")), "audio/mp4");
        assert_eq!(content_type_of(Path::new("c.mkv")), "video/x-matroska");
        assert_eq!(content_type_of(Path::new("d.unknown")), "application/octet-stream");
    }

    /// 本地文件端点:无 Range 全量 200,Range 给 206 + 正确切片,越界 416。
    #[tokio::test]
    async fn file_endpoint_serves_ranges() {
        let dir = std::env::temp_dir().join(format!("lw-relay-file-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("clip.mp3");
        std::fs::write(&path, b"0123456789").unwrap();

        let relay = Relay::start().await.unwrap();
        let url = relay.register_file(path);
        let http = reqwest::Client::new();

        let full = http.get(&url).send().await.unwrap();
        assert_eq!(full.status().as_u16(), 200);
        assert_eq!(full.headers()["content-type"], "audio/mpeg");
        assert_eq!(full.headers()["accept-ranges"], "bytes");
        assert_eq!(full.text().await.unwrap(), "0123456789");

        let tail = http.get(&url).header("Range", "bytes=3-").send().await.unwrap();
        assert_eq!(tail.status().as_u16(), 206);
        assert_eq!(tail.headers()["content-range"], "bytes 3-9/10");
        assert_eq!(tail.text().await.unwrap(), "3456789");

        let span = http.get(&url).header("Range", "bytes=2-4").send().await.unwrap();
        assert_eq!(span.status().as_u16(), 206);
        assert_eq!(span.text().await.unwrap(), "234");

        let over = http.get(&url).header("Range", "bytes=99-").send().await.unwrap();
        assert_eq!(over.status().as_u16(), 416);
        assert_eq!(over.headers()["content-range"], "bytes */10");
    }

    /// 端到端(需 PATH 有 ffmpeg,平时 #[ignore]):生成真片 → 注册 FileAdaptive(copy)→ 用 reqwest
    /// 打全部端点,断言字节形态对(desc JSON / vinit 含 moov / v0 含 moof+mdat / audio 连续流有料)。
    /// `cargo test -p larkwing-core --lib media::relay -- --ignored adaptive` 手跑。
    #[tokio::test]
    #[ignore]
    async fn real_ffmpeg_adaptive_endpoints() {
        use std::process::Command;
        let has = |h: &[u8], n: &[u8]| h.windows(n.len()).any(|w| w == n);
        let dir = std::env::temp_dir().join(format!("lw-adaptive-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("src.mp4");
        // 20s,25fps,关键帧每 2s,H.264 + AAC(视频兼容 → 走 copy)。
        let ok = Command::new("ffmpeg")
            .args([
                "-y", "-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i",
                "testsrc=size=320x240:rate=25:duration=20", "-f", "lavfi", "-i",
                "sine=frequency=440:sample_rate=48000:duration=20", "-c:v", "libx264", "-preset",
                "ultrafast", "-pix_fmt", "yuv420p", "-g", "50", "-keyint_min", "50",
                "-sc_threshold", "0", "-c:a", "aac",
            ])
            .arg(&src)
            .status()
            .expect("run ffmpeg")
            .success();
        assert!(ok, "生成测试源失败");

        let ffmpeg = PathBuf::from("ffmpeg");
        let pr = super::super::probe::probe_local(&src);
        let dur = pr.duration_seconds.expect("时长");
        let codec = pr.video_codec.clone().expect("H.264 codec");
        assert!(!pr.video_keyframes.is_empty(), "应有关键帧");
        let init =
            gen_video_init(&ffmpeg, &src, true, VideoEncoder::Software).await.expect("生成 init");
        assert!(has(&init, b"moov") && has(&init, b"ftyp"), "vinit 应含 ftyp+moov");
        let segments = super::super::probe::plan_copy_segments(&pr.video_keyframes, dur, HLS_SEG);
        assert!(!segments.is_empty());

        let relay = Relay::start().await.unwrap();
        let desc_url = relay.register_file_adaptive(
            src.clone(),
            ffmpeg,
            true,
            format!("video/mp4; codecs=\"{codec}\""),
            init,
            segments,
            dur,
            VideoEncoder::Software,
        );
        let base = desc_url.strip_suffix("/desc").unwrap().to_string();
        let http = reqwest::Client::new();

        // desc:JSON 有两轨 mime + 段清单。
        let desc = http.get(&desc_url).send().await.unwrap();
        assert_eq!(desc.status().as_u16(), 200);
        let body = desc.text().await.unwrap();
        assert!(body.contains("videoMime") && body.contains("avc1."), "desc 带视频 codec: {body}");
        assert!(body.contains("mp4a.40.2") && body.contains("\"segments\""), "desc 带音频+段: {body}");

        // vinit:ftyp+moov。
        let vinit = http.get(format!("{base}/vinit")).send().await.unwrap();
        assert_eq!(vinit.status().as_u16(), 200);
        let vb = vinit.bytes().await.unwrap();
        assert!(has(&vb, b"moov"), "vinit 应含 moov");

        // v0:moof+mdat(段体)。
        let v0 = http.get(format!("{base}/v0")).send().await.unwrap();
        assert_eq!(v0.status().as_u16(), 200);
        let v0b = v0.bytes().await.unwrap();
        assert!(has(&v0b, b"moof") && has(&v0b, b"mdat"), "v0 应是 moof+mdat 段");

        // ainit:音频 init(ftyp+moov)。
        let ainit = http.get(format!("{base}/ainit")).send().await.unwrap();
        assert_eq!(ainit.status().as_u16(), 200);
        let ab = ainit.bytes().await.unwrap();
        assert!(has(&ab, b"moov"), "ainit 应含 moov");
        // a0:音频段 0(moof+mdat)。
        let a0 = http.get(format!("{base}/a0")).send().await.unwrap();
        assert_eq!(a0.status().as_u16(), 200);
        let a0b = a0.bytes().await.unwrap();
        assert!(has(&a0b, b"moof") && has(&a0b, b"mdat"), "a0 应是 moof+mdat 段");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
