//! relay · /f/ 本地文件端点:手写 Range(只认媒体元素会发的形)+ 按扩展名定 content-type。

use super::*;

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

/// 本地文件:Range 透传的文件流(原生 seek 白送);UNC/挂载盘符就是普通路径。
pub(super) async fn file(
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

#[cfg(test)]
mod tests {
    use super::*;

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

    /// `parse_range` 只认媒体元素会发的 `bytes=a-` / `bytes=a-b`;其余歪形一律当没有(回 200 全量),
    /// 越界 416,**绝不 panic**(header 是客户端给的)。此前只经 e2e 三形覆盖。
    #[test]
    fn parse_range_edge_forms_never_panic() {
        let hv = |s: &str| axum::http::HeaderValue::from_str(s).unwrap();
        let none = |s: &str| matches!(parse_range(Some(&hv(s)), 100), RangeSpec::None);
        assert!(none("bytes=-5"), "后缀形不认 → 200 全量");
        assert!(none("bytes=5-2"), "倒置 → 当没有");
        assert!(none("bytes=abc"));
        assert!(none("bytes=0-10,20-30"), "多段不认");
        assert!(none("items=0-1"), "非 bytes 单位");
        assert!(none(""), "空串");
        assert!(matches!(parse_range(Some(&hv("bytes=100-")), 100), RangeSpec::Unsatisfiable));
        assert!(
            matches!(parse_range(Some(&hv("bytes=0-")), 0), RangeSpec::Unsatisfiable),
            "0 字节文件任何 Range 都越界"
        );
        assert!(
            matches!(parse_range(Some(&hv("bytes=10-999")), 100), RangeSpec::Span(10, 99)),
            "end 夹到 len-1"
        );
        assert!(matches!(parse_range(Some(&hv("bytes=0-")), 100), RangeSpec::Span(0, 99)));
        assert!(matches!(parse_range(None, 100), RangeSpec::None));
    }
}
