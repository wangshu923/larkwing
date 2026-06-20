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

/// fMP4 里第一个 `moof` 盒的偏移(= `ftyp`+`moov` 之后)。把 ffmpeg 出的「自包含分片 mp4」
/// (ftyp+moov+moof+mdat)切成 HLS fMP4 要的两块:**共享 init**(`0..first_moof`,即 ftyp+moov)
/// 与 **moof 段**(`first_moof..`,moof+mdat)。逐顶层盒前进找 moof;找不到/畸形 → None。
pub fn first_moof_offset(b: &[u8]) -> Option<usize> {
    let total = b.len() as u64;
    let mut off: u64 = 0;
    loop {
        if off + 8 > total {
            return None;
        }
        let o = off as usize;
        let size32 = u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]) as u64;
        let btype = &b[o + 4..o + 8];
        let box_size = match size32 {
            1 => {
                if off + 16 > total {
                    return None;
                }
                u64::from_be_bytes(b[o + 8..o + 16].try_into().ok()?)
            }
            0 => return None,
            n => n,
        };
        if box_size < 8 {
            return None;
        }
        if btype == b"moof" {
            return Some(o);
        }
        off = off.checked_add(box_size)?;
    }
}

/// 在字节区间 `[start, end)` 内逐顶层盒前进,对每个盒回调 `f(type4, header_len, box_start, box_end)`
/// (payload = `[box_start+header_len, box_end)`)。处理 32 位 size、size==1 的 64 位 largesize、
/// size==0(延伸到 end)。畸形即停。不下钻 —— 进容器盒靠对其 payload 再调一次。
fn for_each_box(b: &[u8], start: usize, end: usize, mut f: impl FnMut(&[u8], usize, usize, usize)) {
    let end = end.min(b.len());
    let mut o = start;
    while o + 8 <= end {
        let size32 = u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]) as usize;
        let typ = &b[o + 4..o + 8];
        let (box_end, hdr) = match size32 {
            1 => {
                if o + 16 > end {
                    break;
                }
                let sz = u64::from_be_bytes(b[o + 8..o + 16].try_into().unwrap()) as usize;
                (o.saturating_add(sz).min(end), 16usize)
            }
            0 => (end, 8usize),
            n => (o.saturating_add(n).min(end), 8usize),
        };
        if box_end < o + hdr {
            break; // 畸形:盒比头还小
        }
        f(typ, hdr, o, box_end);
        if size32 == 0 || box_end <= o {
            break;
        }
        o = box_end;
    }
}

/// 找直接子盒 `typ`,返回其 **payload** 范围 `[start, end)`(已跳过盒头)。
fn find_box(b: &[u8], typ: &[u8; 4], start: usize, end: usize) -> Option<(usize, usize)> {
    let mut found = None;
    for_each_box(b, start, end, |t, hdr, bs, be| {
        if found.is_none() && t == &typ[..] {
            found = Some((bs + hdr, be));
        }
    });
    found
}

/// 从 fMP4 的 init(ftyp+moov)解析每条轨道的 `(track_id, timescale)`。用于把按需切出的分片段
/// 里被重置为 0 的 `tfdt.baseMediaDecodeTime` 改回累计值(段在时间轴上的正确起点 = start×timescale)。
/// 走 moov → trak →(tkhd 取 track_id、mdia/mdhd 取 timescale)。解析不出某轨就跳过(降级:不患的轨
/// 不动 tfdt,等价于回落 0 起点,绝不 panic)。
pub fn init_timescales(init: &[u8]) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    let Some((mo_s, mo_e)) = find_box(init, b"moov", 0, init.len()) else { return out };
    for_each_box(init, mo_s, mo_e, |t, hdr, bs, be| {
        if t != b"trak" {
            return;
        }
        let (ps, pe) = (bs + hdr, be);
        // tkhd: fullbox version/flags(4) + creation/mod(v1:8+8 / v0:4+4) + track_ID(4)
        let Some((tk_s, _)) = find_box(init, b"tkhd", ps, pe) else { return };
        let tkv = init.get(tk_s).copied().unwrap_or(0);
        let tid_off = tk_s + 4 + if tkv == 1 { 16 } else { 8 };
        let Some(tid) =
            init.get(tid_off..tid_off + 4).and_then(|s| s.try_into().ok()).map(u32::from_be_bytes)
        else {
            return;
        };
        // mdia → mdhd: version/flags(4) + creation/mod(v1:8+8 / v0:4+4) + timescale(4)
        let Some((md_s, md_e)) = find_box(init, b"mdia", ps, pe) else { return };
        let Some((mh_s, _)) = find_box(init, b"mdhd", md_s, md_e) else { return };
        let mhv = init.get(mh_s).copied().unwrap_or(0);
        let ts_off = mh_s + 4 + if mhv == 1 { 16 } else { 8 };
        let Some(ts) =
            init.get(ts_off..ts_off + 4).and_then(|s| s.try_into().ok()).map(u32::from_be_bytes)
        else {
            return;
        };
        if ts != 0 {
            out.push((tid, ts));
        }
    });
    out
}

/// 段体末偏移 = 第一个 `mdat` 盒的结尾。`ffmpeg -f mp4` 会在文件尾再写一个 `mfra`(随机访问索引),
/// 普通 HLS 媒体段没有它、且其偏移在切掉 init 后已失效 —— 截到 mdat 结尾,产出干净的「moof+mdat」段。
/// 找不到 mdat → `full.len()`(原样不截,绝不挡播放)。`moof` = `first_moof_offset` 的返回值。
pub fn moof_segment_end(full: &[u8], moof: usize) -> usize {
    let mut end = full.len();
    let mut found = false;
    for_each_box(full, moof, full.len(), |t, _hdr, _bs, be| {
        if !found && t == b"mdat" {
            end = be;
            found = true;
        }
    });
    end
}

/// 把按需切出的分片段(`moof+mdat`,单 moof、每轨一个 traf)里被重置为 0 的
/// `tfdt.baseMediaDecodeTime` 改写成累计值,使该段在 MSE 时间轴上落到正确起点 `start_secs`。
/// 这样产出的是**标准「累计 tfdt」fMP4-HLS 段**,shaka 直接按 tfdt 拼接 —— 不依赖播放器对 HLS
/// 分片额外算 timestampOffset 的实现细节(实测 ffmpeg 输入 seek 出的分片 tfdt 恒为 0,不修则各段
/// 全堆在 0 秒 → 黑屏/错乱)。`track_ts` = `init_timescales` 的结果。某轨找不到则跳过(不动那条 tfdt)。
pub fn patch_segment_tfdt(seg: &mut [u8], track_ts: &[(u32, u32)], start_secs: f64) {
    let Some((mf_s, mf_e)) = find_box(seg, b"moof", 0, seg.len()) else { return };
    // 先只读扫描收集补丁((tfdt payload 偏移, version, 新 base)),再统一写回(避免可变借用穿插)。
    let mut patches: Vec<(usize, u8, u64)> = Vec::new();
    for_each_box(seg, mf_s, mf_e, |t, hdr, bs, be| {
        if t != b"traf" {
            return;
        }
        let (ps, pe) = (bs + hdr, be);
        // tfhd: version/flags(4) + track_ID(4)(track_ID 恒在最前,与可选 flag 字段无关)
        let Some((th_s, _)) = find_box(seg, b"tfhd", ps, pe) else { return };
        let Some(tid) =
            seg.get(th_s + 4..th_s + 8).and_then(|s| s.try_into().ok()).map(u32::from_be_bytes)
        else {
            return;
        };
        let Some(&(_, ts)) = track_ts.iter().find(|(id, _)| *id == tid) else { return };
        let Some((td_s, _)) = find_box(seg, b"tfdt", ps, pe) else { return };
        let ver = seg.get(td_s).copied().unwrap_or(0);
        let base = (start_secs * ts as f64).round().max(0.0) as u64;
        patches.push((td_s, ver, base));
    });
    for (td_s, ver, base) in patches {
        let off = td_s + 4; // 跳过 version/flags
        if ver == 1 {
            if let Some(slot) = seg.get_mut(off..off + 8) {
                slot.copy_from_slice(&base.to_be_bytes());
            }
        } else if let Some(slot) = seg.get_mut(off..off + 4) {
            slot.copy_from_slice(&(base as u32).to_be_bytes());
        }
    }
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

    /// fullbox payload 头:version + 3 字节 flags(0)。
    fn fullbox(version: u8) -> Vec<u8> {
        vec![version, 0, 0, 0]
    }

    /// tkhd v0(只到 track_id 即可,后续字段填零):flags + creation(4)+mod(4)+track_ID(4)。
    fn tkhd_v0(track_id: u32) -> Vec<u8> {
        let mut p = fullbox(0);
        p.extend_from_slice(&0u32.to_be_bytes()); // creation
        p.extend_from_slice(&0u32.to_be_bytes()); // modification
        p.extend_from_slice(&track_id.to_be_bytes());
        p.extend_from_slice(&[0u8; 60]); // 余下字段(reserved/duration/matrix…),解析读不到此处
        p
    }

    /// mdhd v0:flags + creation(4)+mod(4)+timescale(4)+duration(4)。
    fn mdhd_v0(timescale: u32) -> Vec<u8> {
        let mut p = fullbox(0);
        p.extend_from_slice(&0u32.to_be_bytes());
        p.extend_from_slice(&0u32.to_be_bytes());
        p.extend_from_slice(&timescale.to_be_bytes());
        p.extend_from_slice(&0u32.to_be_bytes()); // duration
        p
    }

    fn trak(track_id: u32, timescale: u32) -> Vec<u8> {
        let tkhd = mp4_box(b"tkhd", &tkhd_v0(track_id));
        let mdhd = mp4_box(b"mdhd", &mdhd_v0(timescale));
        let mdia = mp4_box(b"mdia", &mdhd);
        let mut payload = tkhd;
        payload.extend_from_slice(&mdia);
        mp4_box(b"trak", &payload)
    }

    /// tfhd:flags + track_ID(4)。
    fn tfhd(track_id: u32) -> Vec<u8> {
        let mut p = fullbox(0);
        p.extend_from_slice(&track_id.to_be_bytes());
        p
    }

    /// tfdt v1:flags + baseMediaDecodeTime(u64)。
    fn tfdt_v1(base: u64) -> Vec<u8> {
        let mut p = fullbox(1);
        p.extend_from_slice(&base.to_be_bytes());
        p
    }

    fn traf(track_id: u32, base: u64) -> Vec<u8> {
        let mut payload = mp4_box(b"tfhd", &tfhd(track_id));
        payload.extend_from_slice(&mp4_box(b"tfdt", &tfdt_v1(base)));
        mp4_box(b"traf", &payload)
    }

    /// 读某 traf 的 tfdt(v1)base —— 测试断言用。
    fn read_tfdt_base(seg: &[u8], track_id: u32) -> Option<u64> {
        let (mf_s, mf_e) = find_box(seg, b"moof", 0, seg.len())?;
        let mut got = None;
        for_each_box(seg, mf_s, mf_e, |t, hdr, bs, be| {
            if t != b"traf" || got.is_some() {
                return;
            }
            let (ps, pe) = (bs + hdr, be);
            let Some((th_s, _)) = find_box(seg, b"tfhd", ps, pe) else { return };
            let tid = u32::from_be_bytes(seg[th_s + 4..th_s + 8].try_into().unwrap());
            if tid != track_id {
                return;
            }
            let Some((td_s, _)) = find_box(seg, b"tfdt", ps, pe) else { return };
            got = Some(u64::from_be_bytes(seg[td_s + 4..td_s + 12].try_into().unwrap()));
        });
        got
    }

    /// 端到端:用 HLS 段的真实 ffmpeg flag 切一段,跑真·probe 函数链(first_moof_offset →
    /// init_timescales → moof_segment_end → patch_segment_tfdt),断言 tfdt 被改成累计起点。
    /// 需要 PATH 里有 ffmpeg → 平时 `#[ignore]`,本机手动 `cargo test -- --ignored real_ffmpeg` 跑。
    #[test]
    #[ignore]
    fn real_ffmpeg_segment_patches_to_cumulative_tfdt() {
        use std::process::Command;
        let dir = std::env::temp_dir().join(format!("lw-hls-e2e-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("src.mp4");
        // 20s testsrc + sine,H.264 + AAC
        let ok = Command::new("ffmpeg")
            .args(["-y", "-hide_banner", "-loglevel", "error",
                "-f", "lavfi", "-i", "testsrc=size=320x240:rate=25:duration=20",
                "-f", "lavfi", "-i", "sine=frequency=440:duration=20",
                "-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p", "-g", "50",
                "-c:a", "aac"])
            .arg(&src)
            .status().expect("run ffmpeg").success();
        assert!(ok, "生成测试源失败");

        // 切第 2 段 [6,12)(对应 build_frag_cmd 的 flag:单 moof、empty_moov+default_base_moof)
        let out = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-ss", "6.000"])
            .arg("-i").arg(&src)
            .args(["-t", "6.000", "-map", "0:v:0?", "-map", "0:a:0?", "-c:v", "copy", "-c:a", "copy",
                "-movflags", "empty_moov+default_base_moof", "-frag_duration", "600000000",
                "-f", "mp4", "pipe:1"])
            .output().expect("切段");
        let full = out.stdout;
        assert!(!full.is_empty(), "ffmpeg 无输出");

        let moof = first_moof_offset(&full).expect("找 moof");
        let ts = init_timescales(&full[..moof]);
        assert!(ts.iter().any(|(_, t)| *t > 0), "应解析出 timescale: {ts:?}");
        let end = moof_segment_end(&full, moof);
        let mut body = full[moof..end].to_vec();
        // 改前 tfdt 应 ≈ 0
        for (tid, _) in &ts {
            assert_eq!(read_tfdt_base(&body, *tid), Some(0), "输入 seek 出的段 tfdt 改前应为 0");
        }
        patch_segment_tfdt(&mut body, &ts, 6.0);
        for (tid, scale) in &ts {
            let want = (6.0 * *scale as f64).round() as u64;
            assert_eq!(read_tfdt_base(&body, *tid), Some(want), "轨 {tid} tfdt 应改为累计 6s");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn init_timescales_reads_each_track() {
        // moov = mvhd + trak(id1,ts12800) + trak(id2,ts44100)
        let mut moov = mp4_box(b"mvhd", &mvhd_v0(1000, 1000));
        moov.extend_from_slice(&trak(1, 12800));
        moov.extend_from_slice(&trak(2, 44100));
        let init = {
            let mut v = mp4_box(b"ftyp", b"isom");
            v.extend_from_slice(&mp4_box(b"moov", &moov));
            v
        };
        assert_eq!(init_timescales(&init), vec![(1, 12800), (2, 44100)]);
    }

    #[test]
    fn patch_tfdt_sets_cumulative_base_and_strips_mfra() {
        // 自包含分片:ftyp + moov(2 轨)+ moof(2 traf,base=0)+ mdat + mfra(末尾,应被剔除)
        let mut moov = mp4_box(b"mvhd", &mvhd_v0(1000, 1000));
        moov.extend_from_slice(&trak(1, 12800));
        moov.extend_from_slice(&trak(2, 44100));
        let mut moof_payload = mp4_box(b"mfhd", &[0u8; 8]);
        moof_payload.extend_from_slice(&traf(1, 0));
        moof_payload.extend_from_slice(&traf(2, 0));
        let mut full = mp4_box(b"ftyp", b"isom");
        full.extend_from_slice(&mp4_box(b"moov", &moov));
        full.extend_from_slice(&mp4_box(b"moof", &moof_payload));
        full.extend_from_slice(&mp4_box(b"mdat", &[0u8; 32]));
        let with_mfra_len = {
            full.extend_from_slice(&mp4_box(b"mfra", &[0u8; 16]));
            full.len()
        };

        let moof = first_moof_offset(&full).expect("应找到 moof");
        let ts = init_timescales(&full[..moof]);
        assert_eq!(ts, vec![(1, 12800), (2, 44100)]);

        let end = moof_segment_end(&full, moof);
        assert!(end < with_mfra_len, "段体应截到 mdat 末尾、剔除尾部 mfra");

        let mut body = full[moof..end].to_vec();
        patch_segment_tfdt(&mut body, &ts, 6.0);
        // 6.0s × timescale:视频 6×12800=76800,音频 6×44100=264600
        assert_eq!(read_tfdt_base(&body, 1), Some(76_800), "视频轨 tfdt 应改为累计 6s");
        assert_eq!(read_tfdt_base(&body, 2), Some(264_600), "音频轨 tfdt 应改为累计 6s");
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
