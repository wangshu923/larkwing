//! 播放路由的**唯一判定处**(P0 结构先行,2026-08-07)。
//!
//! **为什么要有这个文件**:「这条轨要不要转码 / 这片子走哪条路」原先散在四处各答一次 ——
//! `probe.rs` 的两张白名单(fourcc 与 ffmpeg 名两套词汇)、`play_local` 的分支(选路)、
//! `relay::build_frag_cmd` 的 `copy_video` 旗标(执行)。词汇不同、口径靠人脑对齐,历次事故
//! (mac 双矩阵 / 「探不出当兼容」黑屏 / 轨号被误用去 disable audioTracks)全从这条裂缝长出来。
//!
//! 约定:**这里只判定,不做 IO、不碰字节**。上游 `probe` 给事实(`Facts`),下游 `relay` 按结论
//! 搬字节。纯函数 → golden 可测,改判定策略(P1 换成问浏览器)只动这一个文件。
//!
//! ⚠️ P0 的铁律:**行为与 2026-08-07 之前逐条一致**(本文件的测试就是那份行为的书面版)。
//! 需要改变结论的事(能力探测、copy 快车道、字幕)都在后续期,改时测试会红给你看。

use super::probe::Container;

/// 探测方式:看容器决定「拿什么去问事实」(第一段判定,不涉及路由)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// 放歌(`audio_only`):本地音频常见格式浏览器都吃 → 直传,连文件头都不嗅。
    AudioDirect,
    /// 真 BMFF:读 moov 轻量探测(零子进程、不下 ffmpeg)。
    Bmff,
    /// 浏览器放不了的容器,以及「头是 BMFF 但 moov 读不出」的残缺件 → 交 ffmpeg 现探。
    Ffmpeg,
    /// 浏览器原生容器(webm/ogg)与认不出的 → 直传,交给浏览器。
    NativeDirect,
}

/// 一条轨的处置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Track {
    /// 原样搬运(不掉画质/音质,CPU 近零)。
    Copy,
    /// 重新编码(视频 → H.264;音频 → 立体声 AAC + 响度链)。
    Transcode,
}

/// 路由结论。`relay` 据此注册对应 Entry。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    /// 原样直传(`/f/` 或远端直链):Range 原生 seek,零成本。
    Direct,
    /// 音视频分离自适应(`/la/`,0.2.6 治本):前端手写 MSE 两条 SourceBuffer。
    Adaptive { video: Track, audio: Track },
    /// muxed fMP4-HLS(`/hls/`,经 shaka):**段一律转码视频 + 立体声 AAC**,故不带 Track。
    /// 兜底路 —— 自适应前提不满足时来这里;`force_software` = 前端兜底重放要求换软件编码。
    MuxedHls { force_software: bool },
    /// 渐进流(`/m/`):拿不到时长时的最后兜底,**已知无原生 seek**(前端 `?t=` 重启式)。
    Progressive { video: Track, audio: Track },
}

/// 判定所需的**事实**(来自 probe,不含任何策略)。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Facts {
    /// 时长;`None` 或 ≤0 = 建不了段清单(自适应/HLS 都要它)。
    pub duration: Option<f64>,
    /// 视频编码浏览器解不了(HEVC/AV1/杜比视界…)。
    pub video_incompatible: bool,
    /// **选中那条**音轨解不了(逐轨判定:多音轨片只看选中的)。
    pub sel_audio_incompatible: bool,
    /// 音轨条数:≥2 恒进管线(直传选不了轨 → 会全轨混播,2026-07-21 用户拍板)。
    pub audio_track_count: usize,
    /// 有关键帧表(copy 段的前提:段边界只能落真实 IDR)。
    pub has_keyframes: bool,
    /// 能定出精确的 H.264 codec 串(init 与各段 avcC 要一致才拼得上)。
    pub has_video_codec: bool,
    /// 选中音轨的编码名(fourcc 或 ffmpeg 名皆可,内部归一)。P3:copy 目前只放行 AAC。
    pub sel_audio_codec: Option<String>,
    /// 选中音轨的声道数;`None` = 探不出 → 按转码走(不赌)。
    pub sel_audio_channels: Option<u8>,
}

/// 这条音轨能不能**原样 copy**(P3「原声优先」,用户拍板)。三个前提缺一不可:
/// ① 浏览器解得动 —— 否则没声音;
/// ② 单/双声道 —— **多声道 AAC 会被 MSE 拒 append**(0.2.6 实锤:整个 init 都进不去,
///    表现成"video 轨报错"的黑屏),所以多声道必须下混转码,这堵墙绕不过去;
/// ③ 编码是 AAC —— `/la/desc` 里的 audioMime 目前钉死 `mp4a.40.2`;copy 别的编码(opus/flac
///    在 mp4 里各有各的 mime、精确串要解 esds)会 mime 对不上,等有需求再放开。
///
/// 全过 = `Copy`:原声道、原动态一个字节不动;响度改由播放端 WebAudio 增益补(可关)。
fn audio_track_plan(f: &Facts) -> Track {
    let is_aac = f.sel_audio_codec.as_deref().and_then(normalize_codec) == Some("aac");
    let mono_or_stereo = f.sel_audio_channels.is_some_and(|c| c <= 2);
    if !f.sel_audio_incompatible && mono_or_stereo && is_aac {
        Track::Copy
    } else {
        Track::Transcode
    }
}

/// 看容器定探测方式。
pub fn plan_source(audio_only: bool, container: Container) -> Source {
    if audio_only {
        return Source::AudioDirect; // 放歌:不嗅文件头,直传交给浏览器
    }
    match container {
        Container::Bmff => Source::Bmff,
        Container::Foreign(_) => Source::Ffmpeg,
        Container::Native => Source::NativeDirect,
    }
}

/// 看事实定路由。`prefer_adaptive` = 常态 true;false = 前端手写 MSE 播放失败后的兜底重放
/// (强制走已验证的 muxed 老路 + 软件编码,会漂但不黑屏)。
pub fn plan_route(source: Source, f: &Facts, prefer_adaptive: bool) -> Plan {
    // 时长是段清单的前提(自适应与 HLS 都要);≤0 与缺失同义。
    let has_duration = f.duration.is_some_and(|d| d > 0.0);
    match source {
        Source::AudioDirect | Source::NativeDirect => Plan::Direct,
        Source::Bmff => {
            // 进不进管线三条判据(缺一不可少):视频解不了 / 选中音轨解不了 / 多音轨(选不了轨)。
            let need_pipeline =
                f.video_incompatible || f.sel_audio_incompatible || f.audio_track_count >= 2;
            if !need_pipeline {
                return Plan::Direct; // 全兼容单音轨:原生直传秒开
            }
            if !prefer_adaptive {
                return Plan::MuxedHls { force_software: true }; // 兜底重放
            }
            if !has_duration {
                return Plan::MuxedHls { force_software: false }; // 自适应建不了段清单
            }
            // copy 的两个硬前提:段边界要落真实 IDR、init 与各段 avcC 要一致。
            let video = if !f.video_incompatible && f.has_keyframes && f.has_video_codec {
                Track::Copy
            } else {
                Track::Transcode
            };
            // 音频:能 copy 就 copy(P3「原声优先」),否则离散段转码 + 响度链。
            Plan::Adaptive { video, audio: audio_track_plan(f) }
        }
        Source::Ffmpeg => {
            if has_duration {
                // P2:扫到关键帧表 → 与 BMFF 同一条自适应路(视频能 copy 就 copy,不再逐段重编)。
                // 表不可信 / 兜底重放 → muxed 老路。判据与 BMFF 臂**刻意保持同一套**,
                // 免得「容器不同、规则不同」又长出第二处判定。
                if prefer_adaptive && f.has_keyframes {
                    let video = if !f.video_incompatible && f.has_video_codec {
                        Track::Copy
                    } else {
                        Track::Transcode
                    };
                    return Plan::Adaptive { video, audio: audio_track_plan(f) };
                }
                Plan::MuxedHls { force_software: !prefer_adaptive }
            } else {
                let track = |bad: bool| if bad { Track::Transcode } else { Track::Copy };
                Plan::Progressive {
                    video: track(f.video_incompatible),
                    audio: track(f.sel_audio_incompatible),
                }
            }
        }
    }
}

/// 浏览器解码能力快照(P1):前端 boot 探来的**事实**,取代「我们在 Rust 里猜哪些编码不行」。
///
/// **为什么分两路**:直传(`<video src>`)问的是 `HTMLMediaElement.canPlayType`,管线里能不能
/// `copy` 问的是 `MediaSource.isTypeSupported` —— **两者真的会不一样**(mac 的 WKWebView 直传能放
/// AC3/HEVC〔系统解码器〕,MSE 未必;这正是 `probe::mac_native_*` 那套 `cfg!(target_os)` 分叉的由来)。
/// 分开记,那套编译期分叉才有真正的替代品:同一台机器上两条路各按各的事实判。
///
/// **绝对不做的事:把「矩阵里没探过」当成「不支持」。** 探测矩阵是有限的,没覆盖到的编码一律
/// 回落白名单(`None` = 我不知道),宁可多转一次码,也不能因为没探过就把能放的片判成放不了 ——
/// 与「认不出的默认当兼容」同一条保守偏向(§7.1)。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(default)]
pub struct Codecs {
    /// **矩阵覆盖到的**归一编码名 —— 有它才分得清「探过但不支持」与「压根没探过」。
    /// 少了这一层,两种情况都长成「不在集合里」,后者会被误答成 false(= 把能放的片判成放不了)。
    pub probed: std::collections::BTreeSet<String>,
    /// 直传放得了的归一编码名(`canPlayType` 说 probably/maybe)。
    pub direct: std::collections::BTreeSet<String>,
    /// MSE 吃得下的归一编码名(`isTypeSupported` 说 true)= 管线里可 copy 的前提。
    pub mse: std::collections::BTreeSet<String>,
}

/// 把两套词汇归一成一个名字:BMFF 的 **fourcc**(`hev1`/`ac-3`…)与 **ffmpeg 编码名**
/// (`hevc`/`ac3`…)说的是同一件事,却一直是两张表两套字符串(诊断里那条「四处各答一次」的一半)。
/// 认不出 → `None`(上层回落白名单,不猜)。
pub fn normalize_codec(raw: &str) -> Option<&'static str> {
    // fourcc 在 moov 里是定长字段(可能带空格),ffmpeg 名大小写不保证 → 先削平再查。
    let k = raw.trim().to_ascii_lowercase();
    Some(match k.as_str() {
        // ── 视频 ──
        "avc1" | "avc3" | "h264" => "h264",
        "hev1" | "hvc1" | "hvc2" | "hevc" | "h265" => "hevc",
        "dvh1" | "dvhe" => "dolbyvision",
        "av01" | "av1" => "av1",
        "vp09" | "vp9" => "vp9",
        "vp08" | "vp8" => "vp8",
        "vc1" | "vc-1" | "wvc1" => "vc1",
        "mpeg2video" | "mp2v" => "mpeg2video",
        "mpeg4" | "mp4v" => "mpeg4",
        "msmpeg4v1" | "msmpeg4v2" | "msmpeg4v3" => "msmpeg4",
        "wmv1" | "wmv2" | "wmv3" => "wmv",
        "rv10" | "rv20" | "rv30" | "rv40" => "realvideo",
        "vp6" | "vp6f" => "vp6",
        // ── 音频 ──
        "mp4a" | "aac" => "aac",
        "ac-3" | "ac3" => "ac3",
        "ec-3" | "eac3" => "eac3",
        "ac-4" | "ac4" => "ac4",
        "dtsc" | "dtse" | "dtsh" | "dtsl" | "dts" | "dca" => "dts",
        "mlpa" | "truehd" | "mlp" => "truehd",
        "alac" => "alac",
        "opus" => "opus",
        "flac" => "flac",
        "vorbis" => "vorbis",
        "mp3" => "mp3",
        "wmav1" | "wmav2" | "wmapro" => "wma",
        "cook" => "cook",
        "ralf" => "ralf",
        _ => return None, // 认不出:上层回落白名单,不猜
    })
}

/// 这条轨在**管线**里能不能原样 copy。`None` = 快照缺席或矩阵没覆盖 → 调用方回落白名单。
pub fn mse_supports(caps: Option<&Codecs>, raw_codec: &str) -> Option<bool> {
    lookup(caps, raw_codec, |c| &c.mse)
}

// 注:直传那一路(`canPlayType`)的查询函数**故意先不写** —— 它的消费者是「全兼容 → 直传」那个
// 判定,而那处今天与 copy 判定共用同一个 `video_incompatible` 旗子(两个问题一个答案,诊断里那条
// 「四处各答一次」的残余)。P2 重建 copy 判定时把这对事实拆开,那时它才有真调用点。
// 快照里 `direct` 集照常收(前端探一次两路都拿得到,不额外花钱),只是暂时没人读。

/// 进程级快照(app 级瞬态,§6.4「派生的、可丢的」:丢了 = 回落白名单,不是出错)。
/// 前端 boot 探完经 IPC 灌进来;`None` = 还没探到(boot 前的第一次播放 / headless / 单测)。
/// 形态照 `catalog` 的 overlay 先例:全局 overlay + 纯函数 `_with` 接缝,消费点零改动。
static CODECS: std::sync::RwLock<Option<Codecs>> = std::sync::RwLock::new(None);

/// 前端探完灌进来(换 WebView 版本会重探 → 允许覆盖)。
pub fn set_codecs(c: Codecs) {
    if let Ok(mut w) = CODECS.write() {
        *w = Some(c);
    }
}

/// 取当前快照(集合很小,克隆比端着锁进判定安全)。锁中毒 → `None` = 回落白名单,绝不 panic。
pub fn codecs() -> Option<Codecs> {
    CODECS.read().ok().and_then(|r| r.clone())
}

/// 两路共用的查法:先归一,再看**探过没有**,最后才答支持与否。
fn lookup(
    caps: Option<&Codecs>,
    raw: &str,
    pick: impl Fn(&Codecs) -> &std::collections::BTreeSet<String>,
) -> Option<bool> {
    let caps = caps?;
    let name = normalize_codec(raw)?;
    // 「没探过」必须与「探过且不支持」分开答,否则矩阵漏一个编码就把能放的片判成放不了。
    if !caps.probed.contains(name) {
        return None;
    }
    Some(pick(caps).contains(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 事实的便捷构造:默认 = 有时长、全兼容、单音轨、关键帧与 codec 齐 → 最"顺"的片子。
    fn facts() -> Facts {
        Facts {
            duration: Some(600.0),
            video_incompatible: false,
            sel_audio_incompatible: false,
            audio_track_count: 1,
            has_keyframes: true,
            has_video_codec: true,
            // 默认给一条**能 copy 的** AAC 立体声:上面那些视频侧的用例才不会被音频判定牵着走。
            sel_audio_codec: Some("mp4a".into()),
            sel_audio_channels: Some(2),
        }
    }

    /// 快照便捷构造:探过 `probed`,其中 `mse` / `direct` 各自能放的。
    fn caps(probed: &[&str], mse: &[&str], direct: &[&str]) -> Codecs {
        let set = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect();
        Codecs { probed: set(probed), mse: set(mse), direct: set(direct) }
    }

    #[test]
    fn codecs_snapshot_roundtrips() {
        // 用**行为中性**的快照(probed 空 = 一切回落白名单):全局态被并行跑的其它测试读到也无害。
        let neutral = Codecs::default();
        set_codecs(neutral.clone());
        assert_eq!(codecs(), Some(neutral));
        assert_eq!(mse_supports(codecs().as_ref(), "ac-3"), None, "空矩阵 = 什么都没探过");
    }

    #[test]
    fn normalize_maps_both_vocabularies_to_one_name() {
        // fourcc 与 ffmpeg 名说的是同一件事 → 必须归到同一个名字,否则又是两张表。
        for (raw, want) in [
            ("hev1", "hevc"), ("hvc1", "hevc"), ("hvc2", "hevc"), ("hevc", "hevc"),
            ("avc1", "h264"), ("h264", "h264"),
            ("av01", "av1"), ("av1", "av1"),
            ("vp09", "vp9"), ("vp9", "vp9"),
            ("dvh1", "dolbyvision"), ("dvhe", "dolbyvision"),
            ("mp4a", "aac"), ("aac", "aac"),
            ("ac-3", "ac3"), ("ac3", "ac3"),
            ("ec-3", "eac3"), ("eac3", "eac3"),
            ("dtsc", "dts"), ("dtse", "dts"), ("dtsh", "dts"), ("dtsl", "dts"), ("dca", "dts"),
            ("mlpa", "truehd"), ("truehd", "truehd"), ("mlp", "truehd"),
            ("alac", "alac"), ("opus", "opus"), ("flac", "flac"),
        ] {
            assert_eq!(normalize_codec(raw), Some(want), "{raw}");
        }
        // 大小写与前后空白不该影响(ffmpeg/moov 两边的串都不保证干净)
        assert_eq!(normalize_codec("HEVC"), Some("hevc"));
        assert_eq!(normalize_codec(" ac-3 "), Some("ac3"));
        // 认不出的一律 None —— 让调用方回落白名单,绝不瞎猜
        assert_eq!(normalize_codec("某种没见过的编码"), None);
        assert_eq!(normalize_codec(""), None);
    }

    #[test]
    fn support_answers_from_snapshot_per_route() {
        // 同一台机器上两路可以不同:MSE 吃不下 AC3、直传却放得了(mac WKWebView 实况)。
        let c = caps(
            &["h264", "aac", "ac3", "hevc"],   // 这四个都探过
            &["h264", "aac"],                   // MSE 只吃得下这俩
            &["h264", "aac", "ac3", "hevc"],    // 直传四个都能放
        );
        assert_eq!(mse_supports(Some(&c), "avc1"), Some(true));
        assert_eq!(mse_supports(Some(&c), "ac-3"), Some(false), "探过且不支持 → 敢答 false");
        // 直传那一路的事实照常收在快照里(`c.direct` 说 ac3/hevc 能直传),等 P2 拆开两个问题时才读。
        assert!(c.direct.contains("ac3") && !c.mse.contains("ac3"), "两路结论确实可以不同");
    }

    #[test]
    fn unknown_or_absent_snapshot_returns_none_not_false() {
        // ① 没快照(boot 前第一次播放 / headless / 单测)→ None,调用方回落白名单 = 今天的行为。
        assert_eq!(mse_supports(None, "ac-3"), None);
        assert_eq!(mse_supports(None, "avc1"), None);
        // ② 有快照但矩阵没覆盖这个编码 → 也是 None。**「没探过」≠「不支持」** ——
        //    否则探测矩阵漏一个编码,就会把能放的片判成放不了(比白名单还糟)。
        let c = caps(&["h264"], &["h264"], &["h264"]);
        assert_eq!(normalize_codec("cook"), Some("cook"), "认得出这个名字");
        assert_eq!(mse_supports(Some(&c), "cook"), None, "但矩阵没探过它 → 不知道");
        assert_eq!(mse_supports(Some(&c), "hevc"), None, "同上:没探过 ≠ 不支持");
    }

    #[test]
    fn source_follows_container_not_extension() {
        // 放歌不嗅文件头(省一次 IO),直接直传。
        assert_eq!(plan_source(true, Container::Bmff), Source::AudioDirect);
        assert_eq!(plan_source(true, Container::Foreign("mpegts")), Source::AudioDirect);
        // 视频:真 BMFF 走轻量探测,其余容器交 ffmpeg,浏览器原生/认不出的直传。
        assert_eq!(plan_source(false, Container::Bmff), Source::Bmff);
        assert_eq!(plan_source(false, Container::Foreign("matroska")), Source::Ffmpeg);
        assert_eq!(plan_source(false, Container::Foreign("mpegts")), Source::Ffmpeg);
        assert_eq!(plan_source(false, Container::Native), Source::NativeDirect);
    }

    #[test]
    fn direct_sources_always_direct() {
        assert_eq!(plan_route(Source::AudioDirect, &facts(), true), Plan::Direct);
        assert_eq!(plan_route(Source::NativeDirect, &facts(), true), Plan::Direct);
        // 直传路不受任何事实影响(放歌片子有没有时长都直传)。
        let f = Facts { duration: None, video_incompatible: true, ..facts() };
        assert_eq!(plan_route(Source::AudioDirect, &f, true), Plan::Direct);
    }

    #[test]
    fn fully_compatible_bmff_streams_directly() {
        assert_eq!(plan_route(Source::Bmff, &facts(), true), Plan::Direct);
    }

    /// P3「原声优先」(用户拍板):音频**能 copy 就 copy**,响度改到播放端做。
    /// 三个前提缺一不可 —— 少一个就老老实实转码,宁可多转不可无声/拒播。
    #[test]
    fn audio_copies_only_when_all_three_preconditions_hold() {
        let aac_stereo = Facts {
            sel_audio_codec: Some("mp4a".into()),
            sel_audio_channels: Some(2),
            video_incompatible: true, // 视频不兼容 → 进管线,才轮得到音频判定
            ..facts()
        };
        assert_eq!(
            plan_route(Source::Bmff, &aac_stereo, true),
            Plan::Adaptive { video: Track::Transcode, audio: Track::Copy },
            "AAC 立体声 + 浏览器解得动 → 原样搬,保住原声动态"
        );

        // ① 浏览器解不了(AC3)→ 必须转,否则没声音
        let ac3 = Facts {
            sel_audio_codec: Some("ac-3".into()),
            sel_audio_incompatible: true,
            ..aac_stereo.clone()
        };
        assert_eq!(
            plan_route(Source::Bmff, &ac3, true),
            Plan::Adaptive { video: Track::Transcode, audio: Track::Transcode }
        );
        // ② 多声道 —— **MSE 拒 append 多声道 AAC 是硬墙**(0.2.6 实锤,整个 init 进不去)
        let aac_51 = Facts { sel_audio_channels: Some(6), ..aac_stereo.clone() };
        assert_eq!(
            plan_route(Source::Bmff, &aac_51, true),
            Plan::Adaptive { video: Track::Transcode, audio: Track::Transcode }
        );
        // ③ 不是 AAC(如 FLAC):浏览器虽解得动,但 desc 的 audioMime 目前钉死 mp4a.40.2,
        //    copy 过去 mime 对不上 → 先转码,等 mime 能精确推导再放开
        let flac = Facts { sel_audio_codec: Some("flac".into()), ..aac_stereo.clone() };
        assert_eq!(
            plan_route(Source::Bmff, &flac, true),
            Plan::Adaptive { video: Track::Transcode, audio: Track::Transcode }
        );
        // 声道数拿不准(探不出)→ 按转码走,不赌
        let unknown = Facts { sel_audio_channels: None, ..aac_stereo.clone() };
        assert_eq!(
            plan_route(Source::Bmff, &unknown, true),
            Plan::Adaptive { video: Track::Transcode, audio: Track::Transcode }
        );
        // 容器路同一套规则(mkv 里的 AAC 立体声照样 copy)
        let mkv_aac = Facts { sel_audio_codec: Some("aac".into()), ..aac_stereo };
        assert_eq!(
            plan_route(Source::Ffmpeg, &mkv_aac, true),
            Plan::Adaptive { video: Track::Transcode, audio: Track::Copy }
        );
    }

    #[test]
    fn multi_audio_bmff_always_enters_pipeline_even_when_compatible() {
        // 2026-07-21 用户拍板:直传选不了轨(WKWebView audioTracks 真机证伪、Chromium 没这 API)
        // → 多音轨恒进管线,`-map` 只出选中那条,混播物理上不可能。
        let f = Facts { audio_track_count: 2, ..facts() };
        // 音频是 AAC 立体声 → P3 起原样 copy(进管线是为了「选得了轨」,不是为了重编音频)
        assert_eq!(
            plan_route(Source::Bmff, &f, true),
            Plan::Adaptive { video: Track::Copy, audio: Track::Copy }
        );
    }

    #[test]
    fn bmff_incompatible_audio_keeps_video_copy() {
        // 「视频本就是 H.264、只是音轨是 AC3」的 BD 压制片 = 最常见情形:视频 copy 省 CPU。
        let f = Facts { sel_audio_incompatible: true, ..facts() };
        assert_eq!(
            plan_route(Source::Bmff, &f, true),
            Plan::Adaptive { video: Track::Copy, audio: Track::Transcode }
        );
    }

    #[test]
    fn bmff_incompatible_video_transcodes_video() {
        let f = Facts { video_incompatible: true, ..facts() };
        // 视频要转,但音频(AAC 立体声)照样原样 copy —— 两条轨各判各的,P3 起不再一起转。
        assert_eq!(
            plan_route(Source::Bmff, &f, true),
            Plan::Adaptive { video: Track::Transcode, audio: Track::Copy }
        );
    }

    #[test]
    fn bmff_copy_needs_keyframes_and_codec() {
        // copy 段的两个硬前提缺任一 → 视频转码(仍走自适应,音频分离治漂移)。
        let no_kf = Facts { sel_audio_incompatible: true, has_keyframes: false, ..facts() };
        assert_eq!(
            plan_route(Source::Bmff, &no_kf, true),
            Plan::Adaptive { video: Track::Transcode, audio: Track::Transcode }
        );
        let no_codec = Facts { sel_audio_incompatible: true, has_video_codec: false, ..facts() };
        assert_eq!(
            plan_route(Source::Bmff, &no_codec, true),
            Plan::Adaptive { video: Track::Transcode, audio: Track::Transcode }
        );
    }

    #[test]
    fn bmff_without_duration_falls_back_to_muxed() {
        // 自适应建不了段清单 → muxed HLS(不是 Progressive:BMFF 路的兜底是 HLS)。
        for d in [None, Some(0.0)] {
            let f = Facts { duration: d, sel_audio_incompatible: true, ..facts() };
            assert_eq!(
                plan_route(Source::Bmff, &f, true),
                Plan::MuxedHls { force_software: false },
                "duration={d:?}"
            );
        }
    }

    #[test]
    fn replay_fallback_forces_muxed_with_software_encoder() {
        // 前端手写 MSE 播放失败的兜底重放:走已验证的 muxed 老路 + 软件编码(硬件万一花屏)。
        let f = Facts { sel_audio_incompatible: true, ..facts() };
        assert_eq!(
            plan_route(Source::Bmff, &f, false),
            Plan::MuxedHls { force_software: true }
        );
    }

    #[test]
    fn foreign_container_takes_the_copy_fast_lane_once_keyframes_are_known() {
        // P2:扫到关键帧表后 mkv/ts 与 BMFF 同权 —— 标准 H.264 的片子**不再逐段重编**。
        // (这正是「.mp4 里装 mpegts」那次修法留下的已知代价,到此还清。)
        assert_eq!(
            plan_route(Source::Ffmpeg, &facts(), true),
            Plan::Adaptive { video: Track::Copy, audio: Track::Copy }
        );
        // 扫不出 / 表不可信 → 退回 muxed 重编。**绝不拿半张表去切段**(段边界错位 = 音画事故)。
        let no_kf = Facts { has_keyframes: false, ..facts() };
        assert_eq!(
            plan_route(Source::Ffmpeg, &no_kf, true),
            Plan::MuxedHls { force_software: false }
        );
        // 视频编码浏览器解不了(HEVC/AV1 且这台机确实不支持)→ 仍走自适应,但视频得转。
        let bad_v = Facts { video_incompatible: true, ..facts() };
        assert_eq!(
            plan_route(Source::Ffmpeg, &bad_v, true),
            Plan::Adaptive { video: Track::Transcode, audio: Track::Copy }
        );
        // 兜底重放照旧:换软件编码的 muxed 老路(最兼容,会漂但不黑屏)。
        assert_eq!(
            plan_route(Source::Ffmpeg, &facts(), false),
            Plan::MuxedHls { force_software: true }
        );
    }

    #[test]
    fn foreign_container_without_duration_goes_progressive_with_per_track_flags() {
        // 拿不到时长 → `/m/` 渐进流(已知无原生 seek),逐轨按需转:兼容的仍 copy。
        let f = Facts { duration: None, sel_audio_incompatible: true, ..facts() };
        assert_eq!(
            plan_route(Source::Ffmpeg, &f, true),
            Plan::Progressive { video: Track::Copy, audio: Track::Transcode }
        );
        let both = Facts {
            duration: None,
            video_incompatible: true,
            sel_audio_incompatible: true,
            ..facts()
        };
        assert_eq!(
            plan_route(Source::Ffmpeg, &both, true),
            Plan::Progressive { video: Track::Transcode, audio: Track::Transcode }
        );
        // 两轨都兼容但容器不行(mkv+H.264+AAC 且没时长)→ 纯转封装,两轨都 copy。
        let remux_only = Facts { duration: None, ..facts() };
        assert_eq!(
            plan_route(Source::Ffmpeg, &remux_only, true),
            Plan::Progressive { video: Track::Copy, audio: Track::Copy }
        );
    }
}
