//! relay · /hls/ 本地 fMP4-HLS:合成完整 VOD 列表 + 按需现切 init / 段(shaka 经 MSE 播)。

use super::*;

/// 本地 HLS:`index.m3u8` 合成完整 VOD 列表;`init.mp4` 取 ftyp+moov;`s{N}.m4s` 按需切第 N 段
/// (fMP4 单 moof,一律转码视频 + 立体声 AAC,见 build_frag_cmd)。无临时目录/无会话:每段现切现回。
pub(super) async fn hls(State(state): State<Arc<Inner>>, AxPath((token, seg)): AxPath<(String, String)>) -> Response {
    let Some(entry) = lookup(&state, &token) else { return bad(StatusCode::NOT_FOUND) };
    let Entry::FileHls { path, ffmpeg, duration, enc, audio_track } = entry.as_ref() else {
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
        let cmd = build_frag_cmd(ffmpeg, path, 0.0, 0.1, false, false, *enc, *audio_track);
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
    let cmd = build_frag_cmd(ffmpeg, path, start, HLS_SEG, false, false, *enc, *audio_track);
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

/// 合成完整 VOD HLS 播放列表(纯函数,可测):**fMP4 段**——EXT-X-MAP 指共享 init,段为 `s{N}.m4s`。
/// 全段列出 → shaka 知道完整时长、可任意 seek。段数 = ceil(duration/seg);末段时长 = 余量。
fn build_hls_playlist(duration: f64, seg: f64) -> String {
    // 段数**必须有上限**(2026-08-22 审计):duration 来自文件自报的元数据(mvhd/ffmpeg),
    // 坏文件/被构造的文件可以报出天文数字 → 这个循环就照着它逐段拼字符串,直接把内存吃光。
    // 顺手把非有限值挡掉:`duration/seg` 出 inf/NaN 时 `as u64` 会饱和成 u64::MAX / 0。
    let bogus = !duration.is_finite() || duration <= 0.0 || !seg.is_finite() || seg <= 0.0;
    if bogus {
        // 拼不出可信的清单就给个最小合法清单,让上层的「没时长 → 回落 muxed」那条路接手
        return String::from(
            "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:6\n#EXT-X-MEDIA-SEQUENCE:0\n\
             #EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-ENDLIST\n",
        );
    }
    let want = (duration / seg).ceil().max(1.0);
    let n = if want > HLS_MAX_SEGMENTS as f64 {
        tracing::warn!(duration, seg, "文件自报时长离谱,HLS 段数按上限截断");
        HLS_MAX_SEGMENTS
    } else {
        want as u64
    };
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

#[cfg(test)]
mod tests {
    use super::*;

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

    /// **自报时长离谱时段数必须有上限(2026-08-22 审计)**:duration 来自文件自报的元数据,
    /// 坏文件能报出天文数字,而这个列表是照着它逐段拼字符串的 —— 没上限就是把内存吃光。
    /// 同时挡掉非有限值(`duration/seg` 出 inf/NaN 时 `as u64` 会饱和成 u64::MAX / 0)。
    #[test]
    fn hls_playlist_is_bounded_against_bogus_duration() {
        // 一个「1e12 秒」的自报时长:段数按上限截断,而不是拼出上亿行
        let m = build_hls_playlist(1e12, 6.0);
        let segs = m.matches(".m4s\n").count();
        assert_eq!(segs, HLS_MAX_SEGMENTS as usize, "段数应截到上限,实际 {segs}");
        assert!(m.len() < 2 * 1024 * 1024, "列表大小要有界,实际 {} 字节", m.len());
        // 非有限 / 非正:给最小合法清单,别拼出 u64::MAX 段
        for bad in [f64::INFINITY, f64::NAN, 0.0, -5.0] {
            let m = build_hls_playlist(bad, 6.0);
            assert!(m.starts_with("#EXTM3U") && m.contains("#EXT-X-ENDLIST"), "{bad} 应给最小清单");
            assert!(!m.contains(".m4s"), "{bad} 不该列出任何段");
        }
        // seg 非法(除零)同样不许炸
        let m = build_hls_playlist(20.0, 0.0);
        assert!(!m.contains(".m4s"), "seg=0 不该列段");
    }
}
