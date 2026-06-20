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
    FileRemux { path: PathBuf, ffmpeg: PathBuf, transcode_video: bool, transcode_audio: bool },
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
    FileHls { path: PathBuf, ffmpeg: PathBuf, transcode_video: bool, transcode_audio: bool, duration: f64 },
}

/// HLS 切片时长(秒):段越短 seek 越细但请求越多;6s 是常见折中。
const HLS_SEG: f64 = 6.0;

struct Inner {
    port: u16,
    streams: Mutex<HashMap<String, Arc<Entry>>>,
    net: crate::net::Client,
    counter: AtomicU64,
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
        });
        let app = Router::new()
            .route("/s/{token}", get(direct))
            .route("/m/{token}", get(remux))
            .route("/f/{token}", get(file))
            // DASH:manifest + 两路段透传。shaka 用 fetch() 拉(跨源 → 需 CORS,见 dash 处理)。
            .route("/dash/{token}/{seg}", get(dash).options(dash_preflight))
            // 本地 HLS:m3u8 + 按需切片(同样 shaka fetch 跨源 → CORS)。
            .route("/hls/{token}/{seg}", get(hls).options(dash_preflight))
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

    /// 本地文件、ffmpeg 转封装/转码后的混流 URL(走 /m/ 通道,与 register_remux 同播放路径)。
    /// `transcode_video`/`transcode_audio` 各自决定该轨 copy 还是转码(按 probe 结论,只转不兼容的)。
    pub fn register_file_remux(
        &self,
        path: PathBuf,
        ffmpeg: PathBuf,
        transcode_video: bool,
        transcode_audio: bool,
    ) -> String {
        self.register(Entry::FileRemux { path, ffmpeg, transcode_video, transcode_audio }, "m")
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
    pub fn register_file_hls(
        &self,
        path: PathBuf,
        ffmpeg: PathBuf,
        transcode_video: bool,
        transcode_audio: bool,
        duration: f64,
    ) -> String {
        let token = self.token();
        self.inner.streams.lock().expect("relay streams lock poisoned").insert(
            token.clone(),
            Arc::new(Entry::FileHls { path, ffmpeg, transcode_video, transcode_audio, duration }),
        );
        format!("http://127.0.0.1:{}/hls/{token}/index.m3u8", self.inner.port)
    }
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

/// 本地 HLS:`index.m3u8` 合成完整 VOD 列表;`s{N}.ts` 按需切第 N 段(ffmpeg `-ss N*SEG -t SEG`
/// → mpegts,重置时间戳、单输入 → 段内音画同步)。无临时目录/无会话:每段现切现回、用完即弃。
async fn hls(State(state): State<Arc<Inner>>, AxPath((token, seg)): AxPath<(String, String)>) -> Response {
    let Some(entry) = lookup(&state, &token) else { return bad(StatusCode::NOT_FOUND) };
    let Entry::FileHls { path, ffmpeg, transcode_video, transcode_audio, duration } =
        entry.as_ref()
    else {
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
        let cmd = build_frag_cmd(ffmpeg, path, 0.0, 0.1, *transcode_video, *transcode_audio);
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
    let cmd = build_frag_cmd(ffmpeg, path, start, HLS_SEG, *transcode_video, *transcode_audio);
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

/// 构建「区间 [start, start+dur) 的自包含分片 mp4」命令(ftyp+moov+moof+mdat 吐 stdout)。
/// 单输入 → 段内音画同步;只转不兼容轨。HLS 的 init(取 0.1s)与各段(取 HLS_SEG)都用它。
fn build_frag_cmd(
    ffmpeg: &Path,
    path: &Path,
    start: f64,
    dur: f64,
    transcode_video: bool,
    transcode_audio: bool,
) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(ffmpeg);
    cmd.arg("-hide_banner").arg("-loglevel").arg("error").arg("-nostdin");
    if start > 0.0 {
        cmd.arg("-ss").arg(format!("{start:.3}")); // 输入 seek 到段首(copy 落最近关键帧)
    }
    cmd.arg("-i").arg(path).arg("-t").arg(format!("{dur:.3}"));
    cmd.arg("-map").arg("0:v:0?").arg("-map").arg("0:a:0?");
    if transcode_video {
        cmd.arg("-c:v").arg("libx264").arg("-preset").arg("veryfast").arg("-crf").arg("23")
            .arg("-pix_fmt").arg("yuv420p");
    } else {
        cmd.arg("-c:v").arg("copy");
    }
    if transcode_audio {
        cmd.arg("-c:a").arg("aac").arg("-b:a").arg("256k");
    } else {
        cmd.arg("-c:a").arg("copy");
    }
    // 单 moof/段:`-frag_duration` 给个远超段长(600s)的值 → ffmpeg 不在段内再分片,整段就一个
    // moof+mdat,便于把 tfdt 改成累计值(见 probe::patch_segment_tfdt)。default_base_moof 让 trun
    // 数据偏移相对 moof → 切掉 init 后偏移仍对。
    cmd.arg("-movflags").arg("empty_moov+default_base_moof")
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
        Entry::FileRemux { path, ffmpeg, transcode_video, transcode_audio } => {
            let mut cmd = tokio::process::Command::new(ffmpeg);
            cmd.arg("-hide_banner").arg("-loglevel").arg("error").arg("-nostdin");
            if q.t > 0.0 {
                cmd.arg("-ss").arg(format!("{:.3}", q.t)); // 输入 seek(对 copy 是关键帧对齐)
            }
            cmd.arg("-i").arg(path);
            // 首条视频可选(纯音频不报错)+ 首条音轨可选(无声轨不报错);字幕等不带。
            cmd.arg("-map").arg("0:v:0?").arg("-map").arg("0:a:0?");
            if *transcode_video {
                // HEVC/AV1 等转 H.264:veryfast 平衡速度/画质;yuv420p 把 10bit 压回 8bit(浏览器只认),
                // 否则 H.264 10bit 一样放不了。CPU 重,弱机可能跟不上 1x —— preset/硬件加速是真机调优项。
                cmd.arg("-c:v").arg("libx264").arg("-preset").arg("veryfast")
                    .arg("-crf").arg("23").arg("-pix_fmt").arg("yuv420p");
            } else {
                cmd.arg("-c:v").arg("copy"); // 视频兼容:原样搬,不掉画质、CPU 近零
            }
            if *transcode_audio {
                cmd.arg("-c:a").arg("aac").arg("-b:a").arg("256k");
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
    stream_ffmpeg(cmd, "video/mp4", false)
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

    #[test]
    fn tokens_are_unique_and_urls_local() {
        let inner = Arc::new(Inner {
            port: 12345,
            streams: Mutex::new(HashMap::new()),
            net: crate::net::Client::new(|b| b),
            counter: AtomicU64::new(1),
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
}
