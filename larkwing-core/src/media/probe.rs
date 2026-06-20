//! 本地媒体轻量探测(PLAN §9 / AGENT §8.1 WebView2 编解码坑的本地镜像)。
//!
//! 背景:网络播放经 yt-dlp 强制选 `avc 视频 + m4a/AAC 音频`(resolver.rs)绕开 WebView2
//! 的编解码短板;但**本地文件走 /f/ 直传、原样喂给 `<video>`**,文件里是什么编码就得
//! WebView2 自己啃。BD 国英双语压制片常用 **AC3 / E-AC3 / DTS** 音轨 —— Chromium(WebView2)
//! 解不了 → 画面有、声音没有(开发机 WKWebView 用系统解码器有声,故 Mac 漏网)。
//!
//! 这里在**不下载 ffmpeg、不跑子进程**的前提下读出本地 MP4(ISO BMFF)的音/视频编码标签,
//! 只有命中 WebView2 解不了的音轨时才让上层去转码(§7.1「按需」)——普通 AAC 文件零开销、
//! 仍走原生直传秒开。手法:定位 `moov` 盒子(可在文件尾,逐盒 seek 跳过 `mdat` 不读其内容),
//! 在其中按 4 字符编码标签(stsd 里的 sample entry fourcc)子串匹配。
//!
//! **为什么子串匹配而不是严格解析 stsd**:代价不对称 —— 漏判(把 AC3 当兼容)= 用户继续没声音
//! (真 bug);误判(把兼容当 AC3)= 多转一次码、能放、只是慢一点点。子串匹配几乎不漏判
//! (有 AC3 轨,"ac-3" 字节必在 moov 的 stsd 里),偏向安全的那侧。误判概率极低(moov 是结构化
//! 盒子,样本表里的二进制数恰好排成 "ac-3" 这种事可忽略),且后果良性。

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// 读 moov 的字节上限:stsd 编码标签必在 moov 前段(样本表 stsz/stco 这些巨块排在 stsd 之后),
/// 32MB 足以覆盖任意时长影片的全部 stsd;超大 moov 只读前段,不影响编码判定。
const MOOV_READ_CAP: u64 = 32 * 1024 * 1024;

/// WebView2(Chromium)**解不了**的音频 sample-entry fourcc:命中即需转码成 AAC。
/// AC3/E-AC3/AC-4(杜比)、DTS 全家、TrueHD(mlpa)、ALAC(苹果无损,Chromium 不解)。
const INCOMPATIBLE_AUDIO: &[&[u8]] = &[
    b"ac-3", b"ec-3", b"ac-4", b"dtsc", b"dtse", b"dtsh", b"dtsl", b"mlpa", b"alac",
];

/// WebView2 多半**放不出画面**的视频 fourcc:HEVC(hev1/hvc1/hvc2)、Dolby Vision(dvh1/dvhe)、
/// AV1(av01,Windows 需装扩展常缺)。本期只转音轨不转视频(§7.1 用户拍板),命中仅记日志诊断
/// —— 解释「为什么本地这片黑屏」,不据此动作。VP9(vp09)Chromium 能放,不列入。
const INCOMPATIBLE_VIDEO: &[&[u8]] =
    &[b"hev1", b"hvc1", b"hvc2", b"dvh1", b"dvhe", b"av01"];

/// 非 BMFF 容器(mkv/avi…)读不了 moov,改用 `ffmpeg -i` 探测,它在 stderr 打的是**编码名**
/// (`hevc`/`ac3`/`dts`…),与 fourcc 不同词汇 → 单独一张表。判据同上:只点名 WebView2
/// **确认解不了的**,认不出的默认当兼容(直拷/直传,不平白转码,§7.1「只转处理不了的」)。
/// AV1 与 fourcc 表一致仍列入(与 resolver 强制 avc 的保守一致;真机若证实 WebView2 能放再去掉省 CPU)。
const FFNAME_BAD_AUDIO: &[&str] = &[
    "ac3", "eac3", "ac4", "dts", "dca", "truehd", "mlp", "alac", "wmav1", "wmav2", "wmapro",
    "cook", "ralf",
];
const FFNAME_BAD_VIDEO: &[&str] = &[
    "hevc", "av1", "vc1", "mpeg2video", "mpeg4", "msmpeg4v1", "msmpeg4v2", "msmpeg4v3", "wmv1",
    "wmv2", "wmv3", "rv10", "rv20", "rv30", "rv40", "vp6", "vp6f",
];

/// 本地 MP4 探测结论。解析不出(非 MP4 / 畸形 / 读不到)→ 全 `false` + 无时长,上层据此
/// 退回原生直传(保当前行为,绝不因探测失败挡住播放)。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LocalProbe {
    /// 音轨是 AC3/DTS 等 WebView2 解不了的编码 → 需转码 AAC。
    pub audio_incompatible: bool,
    /// 视频是 HEVC/AV1/杜比视界等 WebView2 多半解不了的编码(仅诊断用)。
    pub video_incompatible: bool,
    /// 从 mvhd 读出的总时长(秒):/m/ 混流流 `<video>.duration` 不可靠,靠它喂进度条。
    pub duration_seconds: Option<f64>,
}

fn ext_lower(path: &Path) -> Option<String> {
    path.extension().and_then(|e| e.to_str()).map(|s| s.to_ascii_lowercase())
}

/// 是否 ISO BMFF 容器(只有这类有 moov 可读 → 走免 ffmpeg 的轻量探测,普通文件秒开不下 ffmpeg)。
pub fn is_isobmff_ext(path: &Path) -> bool {
    matches!(ext_lower(path).as_deref(), Some("mp4" | "m4v" | "mov"))
}

/// 是否视频文件(按扩展名)。本地多集续播扫文件夹时,按"同类"过滤(放视频只列视频)。
/// = ISO BMFF(mp4/m4v/mov)∪ 需转封装容器(mkv/avi…)∪ 浏览器原生(webm/ogv)。
pub fn is_video_ext(path: &Path) -> bool {
    is_isobmff_ext(path)
        || needs_ffmpeg_container(path)
        || matches!(ext_lower(path).as_deref(), Some("webm" | "ogv"))
}

/// 是否音频文件(按扩展名)。放歌/听评书的本地多集续播按此过滤(只列音频,不混进视频)。
pub fn is_audio_ext(path: &Path) -> bool {
    matches!(
        ext_lower(path).as_deref(),
        Some(
            "mp3" | "m4a" | "m4b" | "aac" | "flac" | "wav" | "ogg" | "oga" | "opus" | "wma"
                | "ape" | "alac" | "aiff" | "aif"
        )
    )
}

/// **WebView2 容器本身就放不了**、必须经 ffmpeg 转封装成 fMP4 才能播的视频容器(mkv/avi…)。
/// 这类没有"原生直传"的快车道(本就放不了),所以让它们一律走 ffmpeg:先确保 ffmpeg、再用
/// `ffmpeg -i` 探编码,兼容的轨 `-c copy`、不兼容的才转。webm / ogv 浏览器能放,不在此列。
pub fn needs_ffmpeg_container(path: &Path) -> bool {
    matches!(
        ext_lower(path).as_deref(),
        Some(
            "mkv" | "avi" | "ts" | "m2ts" | "mts" | "flv" | "wmv" | "mpg" | "mpeg" | "vob"
                | "rmvb" | "rm" | "asf" | "divx" | "f4v"
        )
    )
}

/// 解析 `ffmpeg -i <file>` 打到 stderr 的探测信息(纯函数、可测):取首条视频/音频流的编码名
/// 判兼容 + 总时长。`ffmpeg -i` 无输出会以非零码退出(“At least one output file…”),但流信息照样
/// 打在 stderr → 调用方不看退出码、只喂 stderr 进来。任何字段解析不出都按"兼容/无"降级,绝不挡播放。
pub fn parse_ffmpeg_stderr(stderr: &str) -> LocalProbe {
    let mut p = LocalProbe::default();
    for raw in stderr.lines() {
        let line = raw.trim();
        if p.duration_seconds.is_none() {
            p.duration_seconds = parse_duration_line(line);
        }
        // "Stream #0:0(eng): Video: hevc (Main 10), yuv420p10le, 1920x1080 …"
        if let Some(rest) = line.split("Video: ").nth(1) {
            let codec = rest.split([' ', ',', '(']).next().unwrap_or("");
            if FFNAME_BAD_VIDEO.contains(&codec) {
                p.video_incompatible = true;
            }
        }
        // "Stream #0:1(chi): Audio: dts (DTS-HD MA), 48000 Hz, 5.1 …"
        if let Some(rest) = line.split("Audio: ").nth(1) {
            let codec = rest.split([' ', ',', '(']).next().unwrap_or("");
            if FFNAME_BAD_AUDIO.contains(&codec) {
                p.audio_incompatible = true;
            }
        }
    }
    p
}

/// 一条单文件 fMP4(B 站 DASH 的 video.m4s / audio.m4s)里两个关键字节范围,供合成 DASH MPD
/// 的 `<SegmentBase>` 用:Initialization(ftyp+moov,字节 `0..=init_last`)+ sidx 索引盒
/// (`index_first..=index_last`)。播放器(shaka)据此 Range 拉 init + index,再自行解析 sidx
/// 拿到各分片的时间/字节表 → 原生精确 seek。我们只**定位字节范围**,不解析 sidx 内容。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SidxRanges {
    /// Initialization 段末字节(含):`0..=init_last` = ftyp + moov。
    pub init_last: u64,
    /// sidx 盒首字节(含)。
    pub index_first: u64,
    /// sidx 盒末字节(含)。
    pub index_last: u64,
}

/// 在 fMP4 头部字节里逐盒前进,定位 `sidx`(在 ftyp/moov 之后、moof 之前)。返回 init 段与
/// sidx 的字节范围。`head` 取流前段(~64KB 足够:DASH 单表示流的 moov 很小)。找不到 sidx /
/// 头太短 / 畸形 → None(调用方回落:换方案或不走 DASH)。
pub fn probe_sidx(head: &[u8]) -> Option<SidxRanges> {
    let total = head.len() as u64;
    let mut off: u64 = 0;
    loop {
        if off + 8 > total {
            return None; // 头不够长,还没到 sidx
        }
        let o = off as usize;
        let size32 = u32::from_be_bytes([head[o], head[o + 1], head[o + 2], head[o + 3]]) as u64;
        let btype = &head[o + 4..o + 8];
        let box_size = match size32 {
            1 => {
                if off + 16 > total {
                    return None;
                }
                u64::from_be_bytes(head[o + 8..o + 16].try_into().ok()?)
            }
            0 => return None, // 延伸到 EOF 的盒不可能在 sidx 之前(畸形)
            n => n,
        };
        if box_size < 8 {
            return None; // 畸形
        }
        if btype == b"sidx" {
            if off == 0 {
                return None; // sidx 在最前 = 没有 init 段(ftyp/moov),不合我们用途
            }
            return Some(SidxRanges {
                init_last: off - 1,
                index_first: off,
                index_last: off + box_size - 1,
            });
        }
        off = off.checked_add(box_size)?;
    }
}

/// "Duration: 02:03:45.67, start: …" → 秒。N/A / 解析不出 → None。
fn parse_duration_line(line: &str) -> Option<f64> {
    let rest = line.strip_prefix("Duration:")?.trim_start();
    let hms = rest.split(',').next()?.trim();
    if hms.eq_ignore_ascii_case("N/A") {
        return None;
    }
    let mut it = hms.split(':');
    let h: f64 = it.next()?.trim().parse().ok()?;
    let m: f64 = it.next()?.parse().ok()?;
    let s: f64 = it.next()?.parse().ok()?;
    let secs = h * 3600.0 + m * 60.0 + s;
    (secs.is_finite() && secs > 0.0).then_some(secs)
}

/// 探测本地 MP4 文件的音/视频编码与时长(同步 IO;异步调用方用 `spawn_blocking` 包)。
pub fn probe_local(path: &Path) -> LocalProbe {
    let Some(moov) = read_moov(path) else { return LocalProbe::default() };
    LocalProbe {
        audio_incompatible: INCOMPATIBLE_AUDIO.iter().any(|tag| contains(&moov, tag)),
        video_incompatible: INCOMPATIBLE_VIDEO.iter().any(|tag| contains(&moov, tag)),
        duration_seconds: duration_from_moov(&moov),
    }
}

/// 逐顶层盒子前进,定位并读出 `moov`(可能在文件尾):用每个盒子的 size 字段 seek 跳过
/// (尤其不读巨大的 mdat 内容),只把 moov 拉进内存(封顶 MOOV_READ_CAP)。
fn read_moov(path: &Path) -> Option<Vec<u8>> {
    let mut f = File::open(path).ok()?;
    let total = f.metadata().ok()?.len();
    let mut offset: u64 = 0;
    loop {
        if offset + 8 > total {
            return None;
        }
        f.seek(SeekFrom::Start(offset)).ok()?;
        let mut hdr = [0u8; 8];
        f.read_exact(&mut hdr).ok()?;
        let size32 = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as u64;
        let btype = &hdr[4..8];
        // size==1 → 紧随其后的 64 位 largesize;size==0 → 该盒延伸到文件尾(合法仅末盒)。
        let (box_size, header_len) = match size32 {
            1 => {
                let mut ext = [0u8; 8];
                f.read_exact(&mut ext).ok()?;
                (u64::from_be_bytes(ext), 16u64)
            }
            0 => (total - offset, 8u64),
            n => (n, 8u64),
        };
        if box_size < header_len {
            return None; // 畸形:盒子比头还小
        }
        if btype == b"moov" {
            let payload = box_size - header_len;
            let take = payload.min(MOOV_READ_CAP) as usize;
            f.seek(SeekFrom::Start(offset + header_len)).ok()?;
            let mut buf = vec![0u8; take];
            f.read_exact(&mut buf).ok()?;
            return Some(buf);
        }
        offset = offset.checked_add(box_size)?;
        if offset >= total {
            return None; // 走到尾也没 moov
        }
    }
}

/// 从 moov 里的 mvhd 盒读总时长。锚在 4 字节类型标签 "mvhd" 上(忽略前面的 size 字段,更鲁棒),
/// 其后是 version(1)+flags(3)+creation+modification+timescale(4)+duration。v1 的时间字段是 64 位。
fn duration_from_moov(moov: &[u8]) -> Option<f64> {
    let tag = find_subslice(moov, b"mvhd")?;
    let body = moov.get(tag + 4..)?; // 跳过 "mvhd" 类型标签,到 version
    let version = *body.first()?;
    // 字段偏移(自 version 起算)
    let (ts_off, dur_off, dur_u64) = if version == 1 {
        (4 + 8 + 8, 4 + 8 + 8 + 4, true) // flags + creation(8) + mod(8) + timescale(4) + duration(8)
    } else {
        (4 + 4 + 4, 4 + 4 + 4 + 4, false) // flags + creation(4) + mod(4) + timescale(4) + duration(4)
    };
    let timescale = u32::from_be_bytes(body.get(ts_off..ts_off + 4)?.try_into().ok()?);
    if timescale == 0 {
        return None;
    }
    let duration = if dur_u64 {
        u64::from_be_bytes(body.get(dur_off..dur_off + 8)?.try_into().ok()?) as f64
    } else {
        u32::from_be_bytes(body.get(dur_off..dur_off + 4)?.try_into().ok()?) as f64
    };
    let secs = duration / timescale as f64;
    (secs.is_finite() && secs > 0.0).then_some(secs)
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    find_subslice(hay, needle).is_some()
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 拼一个盒子:[size:u32 大端][4 字节类型][payload]。
    fn mp4_box(typ: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let size = (8 + payload.len()) as u32;
        let mut v = size.to_be_bytes().to_vec();
        v.extend_from_slice(typ);
        v.extend_from_slice(payload);
        v
    }

    /// 造一个 v0 mvhd payload:timescale=1000,duration=5000 → 5.0s。
    fn mvhd_v0(timescale: u32, duration: u32) -> Vec<u8> {
        let mut p = vec![0u8; 4]; // version(0) + flags
        p.extend_from_slice(&0u32.to_be_bytes()); // creation
        p.extend_from_slice(&0u32.to_be_bytes()); // modification
        p.extend_from_slice(&timescale.to_be_bytes());
        p.extend_from_slice(&duration.to_be_bytes());
        p.extend_from_slice(&[0u8; 8]); // rate/volume 等(本解析读到 duration 即止,余可有可无)
        p
    }

    fn write_temp(tag: &str, bytes: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("lw-probe-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{tag}.mp4"));
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn detects_ac3_audio_with_moov_after_mdat() {
        // 非 faststart 排布:ftyp + mdat(大,内容里没有编码标签)+ moov(在尾)。
        let mut moov_payload = mp4_box(b"mvhd", &mvhd_v0(1000, 5000));
        // 模拟 trak/stbl/stsd 里的 ac-3 sample entry(子串足矣)
        moov_payload.extend_from_slice(&mp4_box(b"stsd", b"\x00\x00\x00\x01\x00\x00\x00\x20ac-3xxxx"));
        let mut file = mp4_box(b"ftyp", b"isomiso2avc1mp41");
        file.extend_from_slice(&mp4_box(b"mdat", b"some media payload bytes, no codec tags here"));
        file.extend_from_slice(&mp4_box(b"moov", &moov_payload));

        let path = write_temp("ac3", &file);
        let probe = probe_local(&path);
        assert!(probe.audio_incompatible, "应识别出 ac-3 音轨需转码");
        assert!(!probe.video_incompatible);
        assert_eq!(probe.duration_seconds, Some(5.0), "mvhd 时长 5000/1000=5.0s");
    }

    #[test]
    fn aac_audio_is_compatible_no_transcode() {
        let mut moov_payload = mp4_box(b"mvhd", &mvhd_v0(600, 1800)); // 3.0s
        moov_payload.extend_from_slice(&mp4_box(b"stsd", b"\x00\x00\x00\x01\x00\x00\x00\x20mp4axxxx"));
        let mut file = mp4_box(b"ftyp", b"isomiso2avc1mp41");
        file.extend_from_slice(&mp4_box(b"moov", &moov_payload)); // faststart:moov 在前
        file.extend_from_slice(&mp4_box(b"mdat", b"media"));

        let probe = probe_local(&write_temp("aac", &file));
        assert!(!probe.audio_incompatible, "AAC(mp4a)兼容,不该转码");
        assert_eq!(probe.duration_seconds, Some(3.0));
    }

    #[test]
    fn flags_hevc_video_for_diagnostics() {
        let mut moov_payload = mp4_box(b"mvhd", &mvhd_v0(1000, 1000));
        moov_payload.extend_from_slice(&mp4_box(b"stsd", b"....hvc1....mp4a...."));
        let mut file = mp4_box(b"ftyp", b"isomhvc1");
        file.extend_from_slice(&mp4_box(b"moov", &moov_payload));
        let probe = probe_local(&write_temp("hevc", &file));
        assert!(probe.video_incompatible, "hvc1 → 视频不兼容(诊断)");
        assert!(!probe.audio_incompatible, "音轨是 mp4a,不需转码");
    }

    #[test]
    fn garbage_or_non_mp4_yields_default() {
        let probe = probe_local(&write_temp("junk", b"this is definitely not an MP4 file at all"));
        assert_eq!(probe, LocalProbe::default(), "解析不出 → 默认(放行直传,不挡播放)");
    }

    #[test]
    fn ext_gate() {
        assert!(is_isobmff_ext(Path::new("/x/movie.mp4")));
        assert!(is_isobmff_ext(Path::new("/x/movie.MOV")));
        assert!(is_isobmff_ext(Path::new("/x/clip.m4v")));
        assert!(!is_isobmff_ext(Path::new("/x/movie.mkv")), "mkv 不是 ISO BMFF,走 ffmpeg 探测");
        assert!(!is_isobmff_ext(Path::new("/x/song.flac")));
        // 需 ffmpeg 转封装的容器
        assert!(needs_ffmpeg_container(Path::new("/x/m.mkv")));
        assert!(needs_ffmpeg_container(Path::new("/x/m.AVI")));
        assert!(needs_ffmpeg_container(Path::new("/x/m.ts")));
        assert!(!needs_ffmpeg_container(Path::new("/x/m.mp4")), "mp4 走 BMFF 快车道");
        assert!(!needs_ffmpeg_container(Path::new("/x/m.webm")), "webm 浏览器原生能放");
    }

    #[test]
    fn media_kind_buckets() {
        // 视频桶:BMFF + 转封装容器 + webm
        assert!(is_video_ext(Path::new("/x/a.mp4")));
        assert!(is_video_ext(Path::new("/x/a.MKV")));
        assert!(is_video_ext(Path::new("/x/a.webm")));
        assert!(!is_video_ext(Path::new("/x/a.mp3")), "mp3 不是视频");
        // 音频桶
        assert!(is_audio_ext(Path::new("/x/a.mp3")));
        assert!(is_audio_ext(Path::new("/x/a.FLAC")));
        assert!(is_audio_ext(Path::new("/x/a.m4a")));
        assert!(!is_audio_ext(Path::new("/x/a.mp4")), "mp4 是视频不是音频");
        // 字幕/杂项两桶都不收
        assert!(!is_video_ext(Path::new("/x/a.srt")) && !is_audio_ext(Path::new("/x/a.srt")));
    }

    #[test]
    fn ffmpeg_stderr_hevc_dts_mkv() {
        // 典型 BD mkv:HEVC 10bit 视频 + DTS 5.1 音轨 → 两样都要转
        let stderr = "\
Input #0, matroska,webm, from 'movie.mkv':
  Duration: 02:10:33.40, start: 0.000000, bitrate: 18234 kb/s
    Stream #0:0(eng): Video: hevc (Main 10), yuv420p10le(tv), 1920x1080, 23.98 fps
    Stream #0:1(chi): Audio: dts (DTS-HD MA), 48000 Hz, 5.1(side), s32p
    Stream #0:2(chi): Subtitle: subrip";
        let p = parse_ffmpeg_stderr(stderr);
        assert!(p.video_incompatible, "hevc → 转视频");
        assert!(p.audio_incompatible, "dts → 转音轨");
        assert_eq!(p.duration_seconds, Some(2.0 * 3600.0 + 10.0 * 60.0 + 33.4));
    }

    #[test]
    fn ffmpeg_stderr_h264_aac_all_copy() {
        // H.264 + AAC 的 mkv:容器要转封装,但两条流都 -c copy,不转码
        let stderr = "\
  Duration: 00:21:05.00, start: 0.000000, bitrate: 2500 kb/s
    Stream #0:0: Video: h264 (High), yuv420p, 1280x720
    Stream #0:1: Audio: aac (LC), 44100 Hz, stereo";
        let p = parse_ffmpeg_stderr(stderr);
        assert!(!p.video_incompatible, "h264 兼容,copy");
        assert!(!p.audio_incompatible, "aac 兼容,copy");
        assert_eq!(p.duration_seconds, Some(21.0 * 60.0 + 5.0));
    }

    #[test]
    fn probe_sidx_locates_init_and_index() {
        // 典型 DASH 单文件:ftyp + moov + sidx + moof(后者只是占位)
        let ftyp = mp4_box(b"ftyp", b"dashiso6");
        let moov = mp4_box(b"mvhd", &mvhd_v0(1000, 1000)); // 当 moov 内容占位,够长即可
        let moov = mp4_box(b"moov", &moov);
        let sidx = mp4_box(b"sidx", &[0u8; 40]); // sidx 盒(内容不解析,只定位)
        let mut file = Vec::new();
        file.extend_from_slice(&ftyp);
        file.extend_from_slice(&moov);
        let sidx_start = file.len() as u64;
        file.extend_from_slice(&sidx);
        file.extend_from_slice(&mp4_box(b"moof", &[0u8; 16]));

        let r = probe_sidx(&file).expect("应定位到 sidx");
        assert_eq!(r.init_last, sidx_start - 1, "init 段 = ftyp+moov,到 sidx 前一字节");
        assert_eq!(r.index_first, sidx_start, "index 从 sidx 盒首字节起");
        assert_eq!(r.index_last, sidx_start + sidx.len() as u64 - 1, "index 到 sidx 盒末字节");
    }

    #[test]
    fn probe_sidx_none_when_absent_or_short() {
        // 没有 sidx(纯 ftyp+moov+mdat)→ None
        let mut no_sidx = mp4_box(b"ftyp", b"isom");
        no_sidx.extend_from_slice(&mp4_box(b"moov", &mp4_box(b"mvhd", &mvhd_v0(1000, 1000))));
        no_sidx.extend_from_slice(&mp4_box(b"mdat", b"data"));
        assert_eq!(probe_sidx(&no_sidx), None, "无 sidx → None");
        // 头太短(只有半个盒头)→ None,不 panic
        assert_eq!(probe_sidx(b"\x00\x00"), None);
        assert_eq!(probe_sidx(b""), None);
    }

    #[test]
    fn ffmpeg_stderr_unknown_and_na_duration() {
        // 认不出的编码默认兼容(不平白转码);Duration N/A → 无时长
        let stderr = "  Duration: N/A, start: 0.000000\n    Stream #0:0: Video: theora\n    Stream #0:1: Audio: vorbis";
        let p = parse_ffmpeg_stderr(stderr);
        assert!(!p.video_incompatible && !p.audio_incompatible, "theora/vorbis 当兼容");
        assert_eq!(p.duration_seconds, None);
    }
}
