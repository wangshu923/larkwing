//! relay · /la/ 本地音视频分离自适应:desc / vinit / v{N} / ainit / a{N} / sub{N}(前端手写 MSE 两条 SourceBuffer)。

use super::*;

/// 本地自适应(音视频分离,手写 MSE):`desc`(JSON:两轨 mime + 视频段清单 + 时长)/
/// `vinit`(缓存视频 init)/`v{N}`(视频段 N:copy/转码 + tfdt 累积,video-only)/
/// `ainit`(音频 init)/`a{N}`(音频段 N:固定 6s 网格 + 左预卷,离散完整响应,前端 appendWindow 裁 priming)。
pub(super) async fn local_adaptive(
    State(state): State<Arc<Inner>>,
    AxPath((token, seg)): AxPath<(String, String)>,
) -> Response {
    let Some(entry) = lookup(&state, &token) else { return bad(StatusCode::NOT_FOUND) };
    let Entry::FileAdaptive {
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
            "{{\"videoMime\":{vm},\"audioMime\":\"audio/mp4; codecs=\\\"mp4a.40.2\\\"\",\"duration\":{dur:.6},\"copyVideo\":{cv},\"copyAudio\":{ca},\"audioSeg\":{aseg},\"audioPreroll\":{apre},\"segments\":[{segs}]}}",
            vm = json_string(video_mime),
            dur = duration,
            cv = copy_video,
            ca = copy_audio,
            aseg = AUDIO_SEG,
            apre = AUDIO_PREROLL,
        );
        return json_response(json);
    }
    if seg == "vinit" {
        return bytes_response(video_init.clone(), "video/mp4");
    }
    // 字幕(P4):`sub{N}.vtt` 按需把第 N 条来源转成 WebVTT(内嵌轨 `-map 0:s:{n}` / 外挂文件直接读)。
    // 现转现回、不落盘;转不出(图形轨混进来、编码坏了)→ 404,前端那条 track 不出现,绝不空挂。
    if let Some(n) = seg.strip_prefix("sub").and_then(|s| s.strip_suffix(".vtt")) {
        let Some(src) = n.parse::<usize>().ok().and_then(|i| subs.get(i)) else {
            return bad(StatusCode::NOT_FOUND);
        };
        let mut cmd = tokio::process::Command::new(ffmpeg);
        cmd.arg("-hide_banner").arg("-loglevel").arg("error").arg("-nostdin");
        match src {
            SubSource::Embedded(idx) => {
                cmd.arg("-i").arg(path).arg("-map").arg(format!("0:s:{idx}"));
            }
            SubSource::Sidecar(file) => {
                cmd.arg("-i").arg(file);
            }
        }
        cmd.arg("-f").arg("webvtt").arg("pipe:1");
        let Some(vtt) = run_ffmpeg_collect(cmd, 8 * 1024 * 1024).await else {
            tracing::warn!(seg, "字幕转 WebVTT 失败(可能是图形字幕或编码有问题)");
            return bad(StatusCode::NOT_FOUND);
        };
        // `<track>` 是跨源取的(app 源 ≠ relay 回环口)→ 必须带 CORS,否则前端拿不到。
        return bytes_response(vtt, "text/vtt");
    }
    // 音频改**离散段**(不再流式 —— WebView2 的 fetch 不吐流式 body,实锤 abuf=[空] 卡死;离散完整
    // 响应 WebView2 收得下,同视频段)。ainit=音频 init;a{N}=第 N 段(固定 6s 网格,带左预卷供前端
    // appendWindow 裁掉 priming → gapless 无漂移)。段内 tfdt=0,前端靠 timestampOffset+appendWindow 定位。
    if seg == "ainit" {
        let cmd = build_audio_frag_cmd(ffmpeg, path, 0.0, 0.1, *audio_track, *copy_audio);
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
        let cmd = build_audio_frag_cmd(ffmpeg, path, ss, cut, *audio_track, *copy_audio);
        let Some(full) = run_ffmpeg_collect(cmd, 16 * 1024 * 1024).await else {
            return bad(StatusCode::BAD_GATEWAY);
        };
        let Some(moof) = super::probe::first_moof_offset(&full) else {
            return bad(StatusCode::INTERNAL_SERVER_ERROR);
        };
        let end = super::probe::moof_segment_end(&full, moof);
        // 前端契约 = 段内 tfdt=0(时间轴由 timestampOffset+appendWindow 决定)。ffmpeg 对非默认
        // 音轨(-map 0:a:1)-ss 后可能残留非零 tfdt → 样本落到窗外被整段裁光(切英语轨无声,
        // 2026-07-22 真机)——这里**强制归零**,本就为 0 的段是 no-op;原值非零时记日志定案。
        let mut body = full[moof..end].to_vec();
        let tfdt_was = super::probe::zero_tfdt(&mut body);
        if tfdt_was != 0 {
            tracing::info!(seg = n, tfdt_was, bytes = body.len(), "音频段 tfdt 非零,已归零");
        }
        return bytes_response(body, "audio/mp4");
    }
    // v{N} → 视频段 N(video-only 分片,tfdt 改累计起点;与 HLS 段同款处理,只是无音轨)。
    let Some(n) = seg.strip_prefix('v').and_then(|s| s.parse::<usize>().ok()) else {
        return bad(StatusCode::NOT_FOUND);
    };
    let Some(&(start, dur)) = segments.get(n) else { return bad(StatusCode::NOT_FOUND) };
    tracing::info!(seg = n, start, dur, copy = copy_video, "自适应:现切视频段");
    let cmd = build_frag_cmd(ffmpeg, path, start, dur, *copy_video, true, *enc, 0);
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

#[cfg(test)]
mod tests {
    use super::*;

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
        let pr = super::super::probe::probe_local(&src).expect("ffmpeg 造的是合法 mp4");
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
            false, // copy_audio:这条老用例验的是视频 copy 链,音频照旧转码
            format!("video/mp4; codecs=\"{codec}\""),
            init,
            segments,
            dur,
            VideoEncoder::Software,
            0,
            Vec::new(), // 这条老用例不验字幕
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
