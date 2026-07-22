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

/// 一条音轨的探测结论(顺序 = 文件里的音轨顺序,也就是 ffmpeg `-map 0:a:{n}` 的 n)。
/// 过桥进 `NowPlaying.audio_tracks`(UI 出切换钮、〔此刻〕喂模型)。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct AudioTrack {
    /// 编码名(BMFF = stsd fourcc,如 "ac-3"/"mp4a";容器 = ffmpeg 编码名,如 "ac3"/"aac";
    /// 解析不出 = "?"——占位保序,顺序错了 -map 就切错轨)。
    pub codec: String,
    /// ISO-639-2 三字语言码("chi"/"eng"…;"und"/读不出 = None)。core 不翻译(§6.6),
    /// 前端字典映射常见码 → 「国语/英语」,模型直接认得 ISO 码。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,
    /// 元数据标题(mkv 常见「国语 DD5.1」这类;BMFF 少见)。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// 本地 MP4 探测结论。解析不出(非 MP4 / 畸形 / 读不到)→ 全 `false` + 无时长,上层据此
/// 退回原生直传(保当前行为,绝不因探测失败挡住播放)。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LocalProbe {
    /// 音轨是 AC3/DTS 等 WebView2 解不了的编码 → 需转码 AAC。
    /// (多音轨文件 = 任一轨不兼容即 true;选轨播放时上层按 `audio_tracks[选中].codec`
    /// 用 `audio_codec_needs_transcode` 逐轨判,这个粗粒度旗子只当回落。)
    pub audio_incompatible: bool,
    /// 视频是 HEVC/AV1/杜比视界等 WebView2 多半解不了的编码(仅诊断用)。
    pub video_incompatible: bool,
    /// 从 mvhd 读出的总时长(秒):/m/ 混流流 `<video>.duration` 不可靠,靠它喂进度条。
    pub duration_seconds: Option<f64>,
    /// 视频轨关键帧时刻(秒,升序);空 = 无 stss/非视频/解析不出 → 不能 copy 切片(§0.2.6)。
    pub video_keyframes: Vec<f64>,
    /// 视频轨 H.264 codec 串(`avc1.xxxxxx`);None = 非 H.264 或解不出 → copy 路走不了,回落转码。
    pub video_codec: Option<String>,
    /// 全部音轨(文件顺序 = `-map 0:a:{n}` 的 n)。空 = 解析不出/无音轨(选轨功能不出现)。
    pub audio_tracks: Vec<AudioTrack>,
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
    parse_ffmpeg_stderr_with(stderr, cfg!(target_os = "macos"))
}

/// `mac_native` 注入可测(同 `probe_local_with`;mac 放宽白名单见 `mac_native_ffname`)。
fn parse_ffmpeg_stderr_with(stderr: &str, mac_native: bool) -> LocalProbe {
    let mut p = LocalProbe::default();
    // 紧跟在某条音频流后面的 `Metadata: title` 归它(mkv 常见「国语 DD5.1」);
    // 新的 Stream 行一出现就收口,别把视频/字幕的 title 错挂到音轨上。
    let mut title_pending = false;
    for raw in stderr.lines() {
        let line = raw.trim();
        if p.duration_seconds.is_none() {
            p.duration_seconds = parse_duration_line(line);
        }
        if line.starts_with("Stream #") {
            title_pending = false;
        }
        // "Stream #0:0(eng): Video: hevc (Main 10), yuv420p10le, 1920x1080 …"
        if let Some(rest) = line.split("Video: ").nth(1) {
            let codec = rest.split([' ', ',', '(']).next().unwrap_or("");
            if FFNAME_BAD_VIDEO.contains(&codec) && !mac_native_ffname(codec, mac_native) {
                p.video_incompatible = true;
            }
        }
        // "Stream #0:1(chi): Audio: dts (DTS-HD MA), 48000 Hz, 5.1 …" —— 括号里是语言码
        if let Some(rest) = line.split("Audio: ").nth(1) {
            let codec = rest.split([' ', ',', '(']).next().unwrap_or("");
            if FFNAME_BAD_AUDIO.contains(&codec) && !mac_native_ffname(codec, mac_native) {
                p.audio_incompatible = true;
            }
            let lang = line
                .split(": Audio:")
                .next()
                .and_then(|head| head.rsplit_once('(').map(|(_, r)| r))
                .and_then(|r| r.split(')').next())
                .map(str::trim)
                .filter(|s| s.len() == 3 && s.chars().all(|c| c.is_ascii_lowercase()) && *s != "und")
                .map(str::to_string);
            p.audio_tracks.push(AudioTrack {
                codec: if codec.is_empty() { "?".into() } else { codec.to_string() },
                lang,
                title: None,
            });
            title_pending = true;
        } else if title_pending && line.starts_with("title") {
            if let Some((_, v)) = line.split_once(':') {
                let v = v.trim();
                if !v.is_empty() {
                    if let Some(last) = p.audio_tracks.last_mut() {
                        last.title = Some(v.to_string());
                    }
                }
            }
            title_pending = false; // 一条 title 就够,后续 Metadata 行不再收
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

/// 开发壳 macOS(WKWebView = AVFoundation 系统解码)**原生就能解、无需转码**的编码白名单。
/// **只剩视频(HEVC)**——音频(AC3/E-AC3/ALAC)的放宽 2026-07-21 当天回滚(用户拍板
/// 「音频都转」):直传省下的那点转码换来三亏——多音轨全轨混播(WKWebView 的 audioTracks
/// API 起播收敛/播放中启停都不生效)、切轨只能重建、浏览器自行下混响度不受控;统一走管线
/// 转 AAC + 响度链(`AUDIO_LOUDNESS_AF`),两平台音频判定同一张矩阵。视频保留:HEVC 在
/// mac 直传/管线 copy 都能原生解,与响度无关。DTS/TrueHD/AC-4/AV1/杜比视界两平台都转
/// (宁可多转,不可无声/花屏 —— §8.1「有画没声」教训)。Windows 恒走完整表,行为零变化。
fn mac_native_fourcc(tag: &[u8], mac_native: bool) -> bool {
    mac_native && matches!(tag, b"hev1" | b"hvc1" | b"hvc2")
}

/// 同上,ffmpeg 编码名词汇(mkv 等容器路:容器仍要转封装,HEVC 流在 mac 可 `-c copy`)。
fn mac_native_ffname(name: &str, mac_native: bool) -> bool {
    mac_native && matches!(name, "hevc")
}

/// **这一条音轨**要不要转码(选轨播放的逐轨判定;fourcc 与 ffmpeg 名两套词汇都认,
/// mac 白名单同口径)。认不出的编码默认兼容(§7.1「只转处理不了的」)。
pub fn audio_codec_needs_transcode(codec: &str) -> bool {
    audio_codec_needs_transcode_with(codec, cfg!(target_os = "macos"))
}

fn audio_codec_needs_transcode_with(codec: &str, mac_native: bool) -> bool {
    let c = codec.trim();
    let bad = INCOMPATIBLE_AUDIO.contains(&c.as_bytes()) || FFNAME_BAD_AUDIO.contains(&c);
    bad && !(mac_native_fourcc(c.as_bytes(), mac_native) || mac_native_ffname(c, mac_native))
}

/// 探测本地 MP4 文件的音/视频编码与时长(同步 IO;异步调用方用 `spawn_blocking` 包)。
/// 兼容判定按**当前编译目标**(mac 开发壳放宽白名单内的编码,见 `mac_native_fourcc`)。
pub fn probe_local(path: &Path) -> LocalProbe {
    probe_local_with(path, cfg!(target_os = "macos"))
}

/// `mac_native` 注入可测(Windows/mac 两套矩阵都有测试钉住;运行时由 `probe_local` 按编译目标传)。
fn probe_local_with(path: &Path, mac_native: bool) -> LocalProbe {
    let Some(moov) = read_moov(path) else { return LocalProbe::default() };
    LocalProbe {
        audio_incompatible: INCOMPATIBLE_AUDIO
            .iter()
            .any(|tag| !mac_native_fourcc(tag, mac_native) && contains(&moov, tag)),
        video_incompatible: INCOMPATIBLE_VIDEO
            .iter()
            .any(|tag| !mac_native_fourcc(tag, mac_native) && contains(&moov, tag)),
        duration_seconds: duration_from_moov(&moov),
        video_keyframes: video_keyframes(&moov).unwrap_or_default(),
        video_codec: video_h264_codec(&moov),
        audio_tracks: audio_tracks_of_moov(&moov),
    }
}

/// 列出全部音轨(hdlr='soun' 的 trak,按文件顺序 = `-map 0:a:{n}` 的 n):
/// stsd 首个 sample-entry 的 fourcc + mdhd 语言。解析不了细节的轨给 "?" 占位**保序**。
fn audio_tracks_of_moov(moov: &[u8]) -> Vec<AudioTrack> {
    let (mo_s, mo_e) = find_box(moov, b"moov", 0, moov.len())
        // probe_local 传进来的通常已是 moov payload(不含盒头):没套 moov 盒时直接在整块上找 trak。
        .unwrap_or((0, moov.len()));
    let mut out = Vec::new();
    for_each_box(moov, mo_s, mo_e, |t, hdr, bs, be| {
        if t == b"trak" {
            if let Some(track) = audio_track_of_trak(moov, bs + hdr, be) {
                out.push(track);
            }
        }
    });
    out
}

/// 单条 trak:是音轨(hdlr='soun')则读出编码 fourcc + mdhd 语言;非音轨/结构缺失 → None。
fn audio_track_of_trak(moov: &[u8], tr_s: usize, tr_e: usize) -> Option<AudioTrack> {
    let (md_s, md_e) = find_box(moov, b"mdia", tr_s, tr_e)?;
    let (hd_s, _) = find_box(moov, b"hdlr", md_s, md_e)?;
    if moov.get(hd_s + 8..hd_s + 12)? != b"soun" {
        return None;
    }
    // stsd payload:version/flags(4) + entry_count(4) + 首个 sample entry(size(4)+fourcc(4)…)
    let codec = (|| {
        let (mn_s, mn_e) = find_box(moov, b"minf", md_s, md_e)?;
        let (st_s, st_e) = find_box(moov, b"stbl", mn_s, mn_e)?;
        let (sd_s, _) = find_box(moov, b"stsd", st_s, st_e)?;
        let fourcc = moov.get(sd_s + 12..sd_s + 16)?;
        let s = String::from_utf8_lossy(fourcc).trim().to_string();
        (!s.is_empty()).then_some(s)
    })()
    .unwrap_or_else(|| "?".into());
    // mdhd:version/flags(4) + creation/mod(v1:8+8 / v0:4+4) + timescale(4) + duration(v1:8 / v0:4)
    // 之后 2 字节 = pad(1bit) + 3×5bit 打包语言(各 +0x60 = ISO-639-2 小写字母)。
    let lang = (|| {
        let (mh_s, _) = find_box(moov, b"mdhd", md_s, md_e)?;
        let v = *moov.get(mh_s)?;
        let lang_off = mh_s + 4 + if v == 1 { 16 + 4 + 8 } else { 8 + 4 + 4 };
        let raw = u16::from_be_bytes(moov.get(lang_off..lang_off + 2)?.try_into().ok()?);
        decode_mdhd_lang(raw)
    })();
    // trak 级 udta→name:部分 remux 工具把轨道名(「国语 5.1」类)写在这;没有 = None。
    let title = (|| {
        let (ud_s, ud_e) = find_box(moov, b"udta", tr_s, tr_e)?;
        let (nm_s, nm_e) = find_box(moov, b"name", ud_s, ud_e)?;
        let raw = moov.get(nm_s..nm_e)?;
        let text = String::from_utf8_lossy(raw).trim_matches(char::from(0)).trim().to_string();
        (!text.is_empty() && text.chars().count() <= 60).then_some(text)
    })();
    Some(AudioTrack { codec, lang, title })
}

/// mdhd 打包语言 → ISO-639-2 三字码;全零/"und"/解出非小写字母 → None(未标注)。
fn decode_mdhd_lang(raw: u16) -> Option<String> {
    if raw == 0 {
        return None;
    }
    let ch = |shift: u16| (((raw >> shift) & 0x1F) as u8 + 0x60) as char;
    let s: String = [ch(10), ch(5), ch(0)].into_iter().collect();
    (s.chars().all(|c| c.is_ascii_lowercase()) && s != "und").then_some(s)
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

/// 从 moov 里取**视频轨**的关键帧时间戳(秒,升序),供 HLS `-c:v copy` 切片按关键帧对齐切段
/// —— copy 不能任意切,段界只能落在真实 IDR 上(变长段),见 PLAN ★0.2.6。路径:
/// trak →(hdlr 认出 `vide` + mdhd 取 timescale)→ minf → stbl →(stss 同步样本号 + stts 样本时长)
/// → 累积 DTS ÷ timescale。无 stss(全帧同步/异常)、解析越界(moov 被 32MB 截断等)、无视频轨
/// → **None**,调用方回落逐段重编码(安全,能放只是费 CPU)。首个同步样本恒是第 1 帧(时间 ≈0)。
pub fn video_keyframes(moov: &[u8]) -> Option<Vec<f64>> {
    let (mo_s, mo_e) = find_box(moov, b"moov", 0, moov.len())
        // probe_local 传进来的通常已是 moov **payload**(不含盒头):没套 moov 盒时直接在整块上找 trak。
        .unwrap_or((0, moov.len()));
    let mut found: Option<Vec<f64>> = None;
    for_each_box(moov, mo_s, mo_e, |t, hdr, bs, be| {
        if t == b"trak" && found.is_none() {
            found = keyframes_of_trak(moov, bs + hdr, be);
        }
    });
    found.filter(|k| !k.is_empty())
}

/// 单条 trak:是视频轨则返回其关键帧秒数;非视频轨 / 缺表 / 越界 → None(调用方跳过换下一条)。
fn keyframes_of_trak(moov: &[u8], tr_s: usize, tr_e: usize) -> Option<Vec<f64>> {
    let (md_s, md_e) = find_box(moov, b"mdia", tr_s, tr_e)?;
    // hdlr payload:version/flags(4) + pre_defined(4) + handler_type(4) → 只认 'vide'。
    let (hd_s, _) = find_box(moov, b"hdlr", md_s, md_e)?;
    if moov.get(hd_s + 8..hd_s + 12)? != b"vide" {
        return None;
    }
    // mdhd:version/flags(4) + creation/mod(v1:8+8 / v0:4+4) + timescale(4)。
    let (mh_s, _) = find_box(moov, b"mdhd", md_s, md_e)?;
    let mhv = *moov.get(mh_s)?;
    let ts_off = mh_s + 4 + if mhv == 1 { 16 } else { 8 };
    let timescale = u32::from_be_bytes(moov.get(ts_off..ts_off + 4)?.try_into().ok()?);
    if timescale == 0 {
        return None;
    }
    let (mn_s, mn_e) = find_box(moov, b"minf", md_s, md_e)?;
    let (st_s, st_e) = find_box(moov, b"stbl", mn_s, mn_e)?;
    let (ss_s, ss_e) = find_box(moov, b"stss", st_s, st_e)?; // 缺 stss = 无关键帧表 → None
    let (tt_s, tt_e) = find_box(moov, b"stts", st_s, st_e)?;
    let sync = parse_stss(moov.get(ss_s..ss_e)?)?;
    let deltas = parse_stts(moov.get(tt_s..tt_e)?)?;
    Some(keyframe_secs(&sync, &deltas, timescale))
}

/// stss payload → 同步样本号列表(1 起)。version/flags(4) + entry_count(4) + entry_count×u32。
fn parse_stss(p: &[u8]) -> Option<Vec<u32>> {
    let n = u32::from_be_bytes(p.get(4..8)?.try_into().ok()?) as usize;
    let mut out = Vec::with_capacity(n.min(1 << 20));
    for i in 0..n {
        let o = 8 + i * 4;
        out.push(u32::from_be_bytes(p.get(o..o + 4)?.try_into().ok()?));
    }
    Some(out)
}

/// stts payload →(样本数, 每样本时长)列表。version/flags(4) + entry_count(4) + entry_count×(u32,u32)。
fn parse_stts(p: &[u8]) -> Option<Vec<(u32, u32)>> {
    let n = u32::from_be_bytes(p.get(4..8)?.try_into().ok()?) as usize;
    let mut out = Vec::with_capacity(n.min(1 << 20));
    for i in 0..n {
        let o = 8 + i * 8;
        let count = u32::from_be_bytes(p.get(o..o + 4)?.try_into().ok()?);
        let delta = u32::from_be_bytes(p.get(o + 4..o + 8)?.try_into().ok()?);
        out.push((count, delta));
    }
    Some(out)
}

/// 合并遍历 stss(同步样本号)与 stts(样本时长游程),算每个关键帧的解码时刻(秒)。
/// stts 是「count 个样本各 delta 时长」的游程压缩;样本 s(1 起)的 DTS = 它前面所有样本的 delta 之和。
/// sync 升序、stts 顺序 → 单趟归并,O(sync + stts)。
fn keyframe_secs(sync: &[u32], deltas: &[(u32, u32)], timescale: u32) -> Vec<f64> {
    let mut out = Vec::with_capacity(sync.len());
    let mut di = 0usize; // 当前游程下标
    let mut run_first: u64 = 1; // 当前游程首样本号(1 起)
    let mut run_dts: u64 = 0; // run_first 处的累积 DTS
    for &s in sync {
        let s = s as u64;
        if s == 0 {
            continue; // 样本号 1 起,0 非法
        }
        while di < deltas.len() {
            let (count, delta) = (deltas[di].0 as u64, deltas[di].1 as u64);
            if s < run_first + count {
                let dts = run_dts + (s - run_first) * delta;
                out.push(dts as f64 / timescale as f64);
                break;
            }
            run_dts += count * delta;
            run_first += count;
            di += 1;
        }
    }
    out
}

/// 从 moov 解视频轨的 H.264 codec 串(`avc1.PPCCLL`,MSE 建 SourceBuffer 必需精确编码串)。
/// 只认 H.264(avc1/avc3 → 读 avcC 的 profile/compat/level 三字节);其它编码(HEVC/VP9/…)→ None,
/// 调用方据此**不走 copy**(copy 只对能确证 H.264 的做,否则回落转码,安全)。子串定位 `avcC` 盒够用
/// (moov 结构化,样本表里恰好排出 "avcC" 4 字节的概率可忽略),避免全量下钻 stsd。
pub fn video_h264_codec(moov: &[u8]) -> Option<String> {
    let tag = find_subslice(moov, b"avcC")?;
    // avcC payload 紧随类型标签:configurationVersion(1) + profile(1) + compat(1) + level(1)…
    let p = moov.get(tag + 4..tag + 8)?;
    Some(format!("avc1.{:02X}{:02X}{:02X}", p[1], p[2], p[3]))
}

/// 按关键帧把整片切成**变长段**的计划:每段从一个关键帧起,尽量凑够 `target` 秒后落到**下一个
/// 关键帧**收尾(copy 只能在关键帧断开)。返回 `(start_secs, dur_secs)` 列表:start 恒是真实关键帧
/// 时刻,末段补到 `duration`。纯函数、可测。keyframes 为空 → 空列表(调用方回落固定 6s 重编码)。
/// 关键帧稀疏时段会长于 target(正确,只是段大);过密则 ≈target。
pub fn plan_copy_segments(keyframes: &[f64], duration: f64, target: f64) -> Vec<(f64, f64)> {
    if keyframes.is_empty() || !(duration > 0.0) || !(target > 0.0) {
        return Vec::new();
    }
    let mut segs = Vec::new();
    let mut start = keyframes[0].max(0.0); // 首关键帧通常 ≈0
    let mut ki = 0usize;
    loop {
        let want = start + target;
        // 找第一个时刻 ≥ want 的关键帧当段尾(必须严格 > start,避免零长段)。
        while ki < keyframes.len() && (keyframes[ki] <= start || keyframes[ki] < want) {
            ki += 1;
        }
        if ki >= keyframes.len() {
            // 没有更靠后的关键帧了 → 末段补到片尾。
            if duration > start {
                segs.push((start, duration - start));
            }
            break;
        }
        let end = keyframes[ki];
        segs.push((start, end - start));
        start = end;
    }
    segs
}

/// fMP4 里第一个 `moof` 盒的偏移(= `ftyp`+`moov` 之后)。把 ffmpeg 出的「自包含分片 mp4」
/// 把段内全部 `tfdt` 的 baseMediaDecodeTime **归零**,返回改前的最大原值(诊断用)。
/// 自适应音频段的前端契约 = 「段内 tfdt=0,时间轴由 timestampOffset+appendWindow 决定」;
/// ffmpeg 对**非默认音轨**(`-map 0:a:1`)`-ss` 后可能残留非零 tfdt(2026-07-22 真机:切英语轨
/// 段被 append 但样本全落在 appendWindow 外被静默裁光 → 无声)——归零 = 按构造满足契约,
/// 对本就为 0 的段是 no-op。找不到 tfdt(不是分片 mp4)= 0,不动。
pub fn zero_tfdt(seg: &mut [u8]) -> u64 {
    let mut max_was: u64 = 0;
    let mut from = 0usize;
    while let Some(rel) = find_subslice(&seg[from..], b"tfdt") {
        let tag = from + rel;
        from = tag + 4;
        let Some(&version) = seg.get(tag + 4) else { break };
        let off = tag + 8; // version(1)+flags(3) 之后是 baseMediaDecodeTime
        if version == 1 {
            let Some(b8) = seg.get(off..off + 8) else { continue };
            let was = u64::from_be_bytes(b8.try_into().unwrap());
            max_was = max_was.max(was);
            seg[off..off + 8].fill(0);
        } else {
            let Some(b4) = seg.get(off..off + 4) else { continue };
            let was = u32::from_be_bytes(b4.try_into().unwrap()) as u64;
            max_was = max_was.max(was);
            seg[off..off + 4].fill(0);
        }
    }
    max_was
}

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
        let probe = probe_local_with(&path, false); // Windows(目标平台)矩阵
        assert!(probe.audio_incompatible, "应识别出 ac-3 音轨需转码");
        assert!(!probe.video_incompatible);
        assert_eq!(probe.duration_seconds, Some(5.0), "mvhd 时长 5000/1000=5.0s");
        // mac 音频放宽已回滚(混播/切轨/响度三亏,「音频都转」):AC3 在 mac 也转
        assert!(probe_local_with(&path, true).audio_incompatible, "mac 对 AC3 同样转码");
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
        let path = write_temp("hevc", &file);
        let probe = probe_local_with(&path, false); // Windows(目标平台)矩阵
        assert!(probe.video_incompatible, "hvc1 → 视频不兼容(诊断)");
        assert!(!probe.audio_incompatible, "音轨是 mp4a,不需转码");
        assert!(!probe_local_with(&path, true).video_incompatible, "mac 原生解 HEVC,不标");
    }

    #[test]
    fn mac_whitelist_is_video_only() {
        // 音频放宽已回滚:AC3/DTS 两套矩阵都转(混播/切轨/响度三亏,「音频都转」拍板)
        let mut moov_payload = mp4_box(b"mvhd", &mvhd_v0(1000, 5000));
        moov_payload
            .extend_from_slice(&mp4_box(b"stsd", b"\x00\x00\x00\x01\x00\x00\x00\x20dtscxxxx"));
        let mut file = mp4_box(b"ftyp", b"isomiso2avc1mp41");
        file.extend_from_slice(&mp4_box(b"moov", &moov_payload));
        let path = write_temp("dts-mac", &file);
        assert!(probe_local_with(&path, false).audio_incompatible);
        assert!(probe_local_with(&path, true).audio_incompatible, "DTS 在 mac 也要转");

        // ffmpeg 名字表同口径(mkv 路):视频 hevc mac 放行 -c copy;音频 ac3 两边都转
        let s = "  Stream #0:0: Video: hevc (Main 10)\n  Stream #0:1: Audio: ac3, 48000 Hz";
        assert!(parse_ffmpeg_stderr_with(s, false).video_incompatible);
        assert!(parse_ffmpeg_stderr_with(s, false).audio_incompatible);
        let mac = parse_ffmpeg_stderr_with(s, true);
        assert!(!mac.video_incompatible, "mac:hevc 视频可 copy");
        assert!(mac.audio_incompatible, "mac:ac3 音频同样转(回滚后)");
        let s2 = "  Stream #0:0: Video: av1\n  Stream #0:1: Audio: truehd";
        let mac2 = parse_ffmpeg_stderr_with(s2, true);
        assert!(mac2.video_incompatible && mac2.audio_incompatible, "av1/truehd mac 也不放");
    }

    /// 造一条音轨 trak(hdlr=soun + mdhd v0 带打包语言 + stsd 首 sample-entry fourcc)。
    fn audio_trak(fourcc: &[u8; 4], lang: u16) -> Vec<u8> {
        let hdlr = mp4_box(b"hdlr", &{
            let mut p = vec![0u8; 8]; // version/flags + pre_defined
            p.extend_from_slice(b"soun");
            p.extend_from_slice(&[0u8; 12]);
            p
        });
        let mdhd = mp4_box(b"mdhd", &{
            let mut p = vec![0u8; 4 + 4 + 4 + 4 + 4]; // v0: vf + creation + mod + timescale + duration
            p.extend_from_slice(&lang.to_be_bytes());
            p.extend_from_slice(&[0u8; 2]); // pre_defined
            p
        });
        let stsd = mp4_box(b"stsd", &{
            let mut p = vec![0, 0, 0, 0, 0, 0, 0, 1]; // vf + entry_count=1
            p.extend_from_slice(&[0, 0, 0, 0x20]); // entry size
            p.extend_from_slice(fourcc);
            p
        });
        let stbl = mp4_box(b"stbl", &stsd);
        let minf = mp4_box(b"minf", &stbl);
        let mdia = mp4_box(b"mdia", &[hdlr, mdhd, minf].concat());
        mp4_box(b"trak", &mdia)
    }

    #[test]
    fn audio_tracks_enumerated_with_lang_and_per_track_compat() {
        // chi = (3<<10)|(8<<5)|9 = 0x0D09;eng = (5<<10)|(14<<5)|7 = 0x15C7(mdhd 打包 ISO-639-2)
        let mut moov_payload = mp4_box(b"mvhd", &mvhd_v0(1000, 5000));
        moov_payload.extend_from_slice(&audio_trak(b"ac-3", 0x0D09));
        moov_payload.extend_from_slice(&audio_trak(b"mp4a", 0x15C7));
        let mut file = mp4_box(b"ftyp", b"isomiso2avc1mp41");
        file.extend_from_slice(&mp4_box(b"moov", &moov_payload));
        let path = write_temp("atracks", &file);
        let p = probe_local_with(&path, false);
        assert_eq!(p.audio_tracks.len(), 2, "两条音轨按文件顺序列出");
        assert_eq!(p.audio_tracks[0].codec, "ac-3");
        assert_eq!(p.audio_tracks[0].lang.as_deref(), Some("chi"));
        assert_eq!(p.audio_tracks[1].codec, "mp4a");
        assert_eq!(p.audio_tracks[1].lang.as_deref(), Some("eng"));
        // 逐轨判定:ac-3 两平台都转(mac 音频放宽已回滚);mp4a(AAC)两边都不转
        assert!(audio_codec_needs_transcode_with("ac-3", false));
        assert!(audio_codec_needs_transcode_with("ac-3", true), "mac 音频同一张矩阵");
        assert!(!audio_codec_needs_transcode_with("mp4a", false));
        assert!(audio_codec_needs_transcode_with("dtsc", true));
        assert!(audio_codec_needs_transcode_with("dts", true), "ffmpeg 名同口径");
    }

    #[test]
    fn audio_trak_udta_name_becomes_title() {
        // trak 里带 udta→name(remux 工具写的轨道名)→ 当 title;lang 未标(0)→ None
        let mut trak_payload = Vec::new();
        // 复用 audio_trak 的 mdia(剥掉外层 trak 盒头 8 字节取 payload)
        trak_payload.extend_from_slice(&audio_trak(b"ac-3", 0)[8..]);
        let name = mp4_box(b"name", "\u{56fd}\u{8bed} 5.1".as_bytes());
        trak_payload.extend_from_slice(&mp4_box(b"udta", &name));
        let mut moov_payload = mp4_box(b"mvhd", &mvhd_v0(1000, 5000));
        moov_payload.extend_from_slice(&mp4_box(b"trak", &trak_payload));
        let mut file = mp4_box(b"ftyp", b"isomiso2avc1mp41");
        file.extend_from_slice(&mp4_box(b"moov", &moov_payload));
        let p = probe_local_with(&write_temp("udta-name", &file), false);
        assert_eq!(p.audio_tracks.len(), 1);
        assert_eq!(p.audio_tracks[0].title.as_deref(), Some("国语 5.1"));
        assert!(p.audio_tracks[0].lang.is_none(), "语言 0 = 未标注");
    }

    #[test]
    fn ffmpeg_stderr_lists_audio_tracks_with_lang_and_title() {
        let stderr = "\
  Duration: 02:10:33.40, start: 0.000000, bitrate: 18234 kb/s
    Stream #0:0(eng): Video: hevc (Main 10), yuv420p10le(tv), 1920x1080, 23.98 fps
    Stream #0:1(chi): Audio: ac3, 48000 Hz, 5.1(side)
    Metadata:
      title           : 国语 DD5.1
    Stream #0:2(eng): Audio: dts (DTS-HD MA), 48000 Hz
    Stream #0:3: Subtitle: subrip";
        let p = parse_ffmpeg_stderr_with(stderr, false);
        assert_eq!(p.audio_tracks.len(), 2, "字幕/视频不混进音轨清单");
        assert_eq!(p.audio_tracks[0].codec, "ac3");
        assert_eq!(p.audio_tracks[0].lang.as_deref(), Some("chi"));
        assert_eq!(p.audio_tracks[0].title.as_deref(), Some("国语 DD5.1"));
        assert_eq!(p.audio_tracks[1].codec, "dts");
        assert_eq!(p.audio_tracks[1].lang.as_deref(), Some("eng"));
        assert!(p.audio_tracks[1].title.is_none(), "title 只归紧跟的那条音轨");
    }

    #[test]
    fn zero_tfdt_rewrites_and_reports_original() {
        let v0 = {
            let mut p = vec![0u8, 0, 0, 0]; // version 0 + flags
            p.extend_from_slice(&12_345u32.to_be_bytes());
            mp4_box(b"tfdt", &p)
        };
        let v1 = {
            let mut p = vec![1u8, 0, 0, 0]; // version 1 + flags
            p.extend_from_slice(&17_580_000u64.to_be_bytes());
            mp4_box(b"tfdt", &p)
        };
        let mut seg = mp4_box(b"moof", &[v0, v1].concat());
        assert_eq!(zero_tfdt(&mut seg), 17_580_000, "报告改前最大原值");
        assert_eq!(zero_tfdt(&mut seg), 0, "二次归零 = no-op(已全 0)");
        // 没有 tfdt 的数据:不动、报 0
        let mut plain = mp4_box(b"ftyp", b"isom");
        assert_eq!(zero_tfdt(&mut plain), 0);
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
        let p = parse_ffmpeg_stderr_with(stderr, false); // Windows(目标平台)矩阵
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

    // ── 关键帧提取 + copy 切片计划(PLAN ★0.2.6) ──────────────────────────

    fn hdlr(kind: &[u8; 4]) -> Vec<u8> {
        let mut p = fullbox(0);
        p.extend_from_slice(&0u32.to_be_bytes()); // pre_defined
        p.extend_from_slice(kind); // handler_type('vide'/'soun')
        p.extend_from_slice(&[0u8; 12]); // reserved
        p.extend_from_slice(b"x\0"); // name(任意)
        p
    }
    fn stss_box(samples: &[u32]) -> Vec<u8> {
        let mut p = fullbox(0);
        p.extend_from_slice(&(samples.len() as u32).to_be_bytes());
        for &s in samples {
            p.extend_from_slice(&s.to_be_bytes());
        }
        mp4_box(b"stss", &p)
    }
    fn stts_box(entries: &[(u32, u32)]) -> Vec<u8> {
        let mut p = fullbox(0);
        p.extend_from_slice(&(entries.len() as u32).to_be_bytes());
        for &(c, d) in entries {
            p.extend_from_slice(&c.to_be_bytes());
            p.extend_from_slice(&d.to_be_bytes());
        }
        mp4_box(b"stts", &p)
    }
    /// 造一条带 hdlr/mdhd/stbl(stss+stts)的 trak。handler='vide' 才会被 video_keyframes 采用。
    fn media_trak(handler: &[u8; 4], timescale: u32, sync: &[u32], stts: &[(u32, u32)]) -> Vec<u8> {
        let mut stbl = stss_box(sync);
        stbl.extend_from_slice(&stts_box(stts));
        let minf = mp4_box(b"minf", &mp4_box(b"stbl", &stbl));
        let mut mdia = mp4_box(b"hdlr", &hdlr(handler));
        mdia.extend_from_slice(&mp4_box(b"mdhd", &mdhd_v0(timescale)));
        mdia.extend_from_slice(&minf);
        let mut payload = mp4_box(b"tkhd", &tkhd_v0(1));
        payload.extend_from_slice(&mp4_box(b"mdia", &mdia));
        mp4_box(b"trak", &payload)
    }

    #[test]
    fn video_keyframes_from_stss_stts() {
        // ts=1000,250 帧各 40 单位(=40ms,25fps,10s);关键帧每 50 帧一个 → 时刻 0,2,4,6,8。
        let mut moov = mp4_box(b"mvhd", &mvhd_v0(1000, 10_000));
        // 先放一条音频轨(handler='soun')确认被跳过,再放视频轨。
        moov.extend_from_slice(&media_trak(b"soun", 48_000, &[1], &[(1, 1024)]));
        moov.extend_from_slice(&media_trak(b"vide", 1000, &[1, 51, 101, 151, 201], &[(250, 40)]));
        let kf = video_keyframes(&moov).expect("应从视频轨解析出关键帧");
        assert_eq!(kf, vec![0.0, 2.0, 4.0, 6.0, 8.0]);
    }

    #[test]
    fn video_keyframes_handles_stts_runs() {
        // 变帧率:前 100 帧 delta=40(0..4s),后 100 帧 delta=20(4s 起,50fps)。
        // 关键帧样本 1(0s)、101(前 100 帧 = 100×40=4000 → 4.0s)、151(4.0 + 50×20/1000=5.0s)。
        let mut moov = mp4_box(b"mvhd", &mvhd_v0(1000, 6000));
        moov.extend_from_slice(&media_trak(b"vide", 1000, &[1, 101, 151], &[(100, 40), (100, 20)]));
        let kf = video_keyframes(&moov).expect("应解析");
        assert_eq!(kf, vec![0.0, 4.0, 5.0]);
    }

    #[test]
    fn video_keyframes_none_without_stss() {
        // 无 stss(全帧同步 / 异常)→ None,调用方回落重编码。
        let minf = mp4_box(b"minf", &mp4_box(b"stbl", &stts_box(&[(100, 40)])));
        let mut mdia = mp4_box(b"hdlr", &hdlr(b"vide"));
        mdia.extend_from_slice(&mp4_box(b"mdhd", &mdhd_v0(1000)));
        mdia.extend_from_slice(&minf);
        let mut payload = mp4_box(b"tkhd", &tkhd_v0(1));
        payload.extend_from_slice(&mp4_box(b"mdia", &mdia));
        let mut moov = mp4_box(b"mvhd", &mvhd_v0(1000, 4000));
        moov.extend_from_slice(&mp4_box(b"trak", &payload));
        assert_eq!(video_keyframes(&moov), None);
        // 完全没有视频轨(只有音频)→ None
        let mut audio_only = mp4_box(b"mvhd", &mvhd_v0(1000, 4000));
        audio_only.extend_from_slice(&media_trak(b"soun", 48_000, &[1], &[(200, 1024)]));
        assert_eq!(video_keyframes(&audio_only), None);
    }

    #[test]
    fn plan_copy_segments_dense_keyframes() {
        // 关键帧每 2s,target 6s → 每段凑到 ≥6s 的下一个关键帧(6),末段补到片尾。
        let kf: Vec<f64> = (0..=6).map(|i| i as f64 * 2.0).collect(); // 0,2,4,6,8,10,12
        let segs = plan_copy_segments(&kf, 13.0, 6.0);
        assert_eq!(segs, vec![(0.0, 6.0), (6.0, 6.0), (12.0, 1.0)]);
        // 段界恒落关键帧、首尾相接、总长=片长
        assert!(segs.iter().all(|&(s, _)| kf.contains(&s)), "每段起点都是关键帧");
        let total: f64 = segs.iter().map(|&(_, d)| d).sum();
        assert!((total - 13.0).abs() < 1e-9, "覆盖到片尾");
    }

    #[test]
    fn plan_copy_segments_sparse_keyframes_make_long_segments() {
        // 关键帧稀疏(10s 一个)→ 段会长于 target(正确,copy 只能在关键帧断)。
        let segs = plan_copy_segments(&[0.0, 10.0, 20.0], 25.0, 6.0);
        assert_eq!(segs, vec![(0.0, 10.0), (10.0, 10.0), (20.0, 5.0)]);
    }

    #[test]
    fn plan_copy_segments_edges() {
        assert_eq!(plan_copy_segments(&[0.0], 5.0, 6.0), vec![(0.0, 5.0)], "短于一段 → 单段到片尾");
        assert!(plan_copy_segments(&[], 10.0, 6.0).is_empty(), "无关键帧 → 空(回落重编码)");
        assert!(plan_copy_segments(&[0.0], 0.0, 6.0).is_empty(), "无时长 → 空");
        // 相接性 + 无零长段
        let kf: Vec<f64> = (0..30).map(|i| i as f64 * 1.001).collect();
        let segs = plan_copy_segments(&kf, 30.03, 6.0);
        for w in segs.windows(2) {
            assert!((w[0].0 + w[0].1 - w[1].0).abs() < 1e-9, "段首尾相接");
        }
        assert!(segs.iter().all(|&(_, d)| d > 0.0), "无零长段");
    }

    /// 端到端(需 PATH 有 ffmpeg,平时 #[ignore]):生成关键帧每 2s 的真片,读 moov 解关键帧,
    /// 断言与 ffmpeg 实际排布吻合;再验 copy 切段首尾相接。`cargo test -- --ignored keyframe` 手跑。
    #[test]
    #[ignore]
    fn real_ffmpeg_keyframes_match_and_copy_segments_align() {
        use std::process::Command;
        let dir = std::env::temp_dir().join(format!("lw-kf-e2e-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("src.mp4");
        // 20s,25fps,关键帧每 50 帧(=2s):-g 50 -keyint_min 50 -sc_threshold 0(禁场景切额外关键帧)。
        let ok = Command::new("ffmpeg")
            .args([
                "-y", "-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i",
                "testsrc=size=320x240:rate=25:duration=20", "-c:v", "libx264", "-preset",
                "ultrafast", "-pix_fmt", "yuv420p", "-g", "50", "-keyint_min", "50",
                "-sc_threshold", "0",
            ])
            .arg(&src)
            .status()
            .expect("run ffmpeg")
            .success();
        assert!(ok, "生成测试源失败");

        let moov = read_moov(&src).expect("读 moov");
        let kf = video_keyframes(&moov).expect("解关键帧");
        // 期望 0,2,4,…,18(10 个);允许 ±1 帧(0.04s)误差。
        let want: Vec<f64> = (0..10).map(|i| i as f64 * 2.0).collect();
        assert_eq!(kf.len(), want.len(), "关键帧个数应为 10,实得 {kf:?}");
        for (g, w) in kf.iter().zip(&want) {
            assert!((g - w).abs() < 0.05, "关键帧 {g} 应≈{w}(全部:{kf:?})");
        }
        // 切片计划:段界落关键帧、首尾相接、覆盖到 20s。
        let segs = plan_copy_segments(&kf, 20.0, 6.0);
        assert!(segs.iter().all(|&(s, _)| kf.iter().any(|k| (k - s).abs() < 1e-6)));
        let total: f64 = segs.iter().map(|&(_, d)| d).sum();
        assert!((total - 20.0).abs() < 0.05, "覆盖到片尾,段:{segs:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
