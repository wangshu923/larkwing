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
}

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

    let upstream = match state
        .net
        .send(&up.url, |c| {
            let mut req = c.get(&up.url);
            for (k, v) in &up.headers {
                req = req.header(k, v);
            }
            if let Some(range) = headers.get(axum::http::header::RANGE) {
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

    let mut builder = Response::builder().status(upstream.status().as_u16());
    for key in ["content-type", "content-length", "content-range", "accept-ranges"] {
        if let Some(v) = upstream.headers().get(key) {
            builder = builder.header(key, v);
        }
    }
    let stream = upstream.bytes_stream().map_err(std::io::Error::other);
    builder.body(Body::from_stream(stream)).unwrap_or_else(|_| bad(StatusCode::INTERNAL_SERVER_ERROR))
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
    let cmd = match entry.as_ref() {
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
                cmd.arg("-ss").arg(format!("{:.3}", q.t)); // 输入 seek(对 copy 是关键帧对齐,够用)
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
            cmd
        }
        _ => return bad(StatusCode::NOT_FOUND),
    };
    stream_ffmpeg(cmd)
}

/// 给一个已配好程序+参数的 ffmpeg 命令收口 stdio、起进程、把 stdout 搬成 HTTP 流。
/// 两条混流路径(网络 DASH / 本地转码)共用,fMP4 输出参数也在此统一(别两处各写一遍)。
fn stream_ffmpeg(mut cmd: tokio::process::Command) -> Response {
    cmd.arg("-movflags").arg("frag_keyframe+empty_moov+default_base_moof")
        .arg("-f").arg("mp4")
        .arg("pipe:1");
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

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "video/mp4")
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
        let a = relay.register_direct(UpStream { url: "u1".into(), headers: vec![] });
        let b = relay.register_direct(UpStream { url: "u2".into(), headers: vec![] });
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
