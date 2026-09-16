//! relay · 上游透传:/s/ 直转(头 + Range 透传)、/dash/ 合成 MPD 与段透传(CORS)、`fetch_head` 探 sidx、`build_mpd`。

use super::*;

/// 取一条上游流的前段(≤cap 字节)用于探 sidx:带防盗链头 + Range;上游若忽略 Range 回 200 全量,
/// 也只读到 cap 就停(绝不把整片拉进内存)。失败 → None。
pub(super) async fn fetch_head(net: &crate::net::Client, up: &UpStream, cap: u64) -> Option<Vec<u8>> {
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

/// 合成一份 on-demand DASH MPD(纯函数,可测):两条单文件流各一个 Representation,用 SegmentBase +
/// indexRange(sidx)+ Initialization range。shaka 据此 Range 拉 init/index/段、自己管时间轴 →
/// 原生精确 seek + 音画同步。codecs/bandwidth 来自 yt-dlp(缺则给保守默认);音频采样率/声道 shaka
/// 会从 init 段读真值,故 MPD 不写、免对不上。段地址用相对 `v`/`a`(相对 manifest URL → /dash/{token}/v|a)。
pub(super) fn build_mpd(
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

/// 直转:上游必需头 + 客户端 Range 透传,响应头/状态镜像回去。
/// WebView 首次请求就带 Range: bytes=0-,上游 206 + 总长 → 原生 seek 直接可用。
pub(super) async fn direct(
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
pub(super) async fn dash(
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
pub(super) async fn dash_preflight() -> Response {
    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .header("access-control-allow-origin", "*")
        .header("access-control-allow-methods", "GET, OPTIONS")
        .header("access-control-allow-headers", "Range")
        .header("access-control-max-age", "86400")
        .body(Body::empty())
        .unwrap_or_else(|_| bad(StatusCode::INTERNAL_SERVER_ERROR))
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
