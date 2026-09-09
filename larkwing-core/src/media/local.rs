//! 本地文件怎么放:探测 → 只转处理不了的那部分 → 挑一条管线(直传 / 自适应 /
//! muxed HLS),外加封面侧车。
//!
//! 「这轨要不要转 / 走哪条路」的**判定**在 `capability.rs`(纯函数,golden 钉着);
//! 这里只负责按判定把管线搭起来、把兜底接上。

use super::*;

/// 固定 `target` 秒的段计划(转码用,不必关键帧对齐——转码每段从新 IDR 重编):
/// `(0,t),(t,t),…,(末段,余量)`。末段补到 `duration`。纯函数。
// `!(x > 0.0)` 是**故意的 NaN 防护,别按 lint 建议改**:探测/解析出来的时长可能是 NaN,
// 而 `x <= 0.0` 对 NaN 判 false → 会漏过闸门去切段;换成 `partial_cmp` 同理绕不开。
#[allow(clippy::neg_cmp_op_on_partial_ord)]
fn fixed_segments(duration: f64, target: f64) -> Vec<(f64, f64)> {
    if !(duration > 0.0) || !(target > 0.0) {
        return Vec::new();
    }
    let n = (duration / target).ceil().max(1.0) as usize;
    (0..n).map(|i| (i as f64 * target, (duration - i as f64 * target).min(target))).collect()
}

/// 放歌时顺手探出来的标签与封面(见 `audio_extras`);拿不到就是默认空,不挡播放。
#[derive(Debug, Default)]
struct AudioExtras {
    author: Option<String>,
    album: Option<String>,
    cover_url: Option<String>,
    duration: Option<f64>,
}

/// 同目录侧车封面的文件名(不含扩展名;§4.11 用户拍板 2026-09-07):Windows 媒体播放器写 `Folder.jpg`,
/// foobar2000 / MusicBee 默认 `cover.*` / `front.*`,再加 `album.*`;大小写不敏感,按此顺序取第一个。
const COVER_SIDECAR_NAMES: [&str; 4] = ["cover", "folder", "front", "album"];
/// 侧车封面认的图片扩展名(webp 要 image crate 解得动,已开 feature)。
const COVER_SIDECAR_EXTS: [&str; 4] = ["jpg", "jpeg", "png", "webp"];

/// 在音频文件所在目录找一张侧车封面:名字在 `COVER_SIDECAR_NAMES`、扩展名在 `COVER_SIDECAR_EXTS`
/// (都不分大小写)。同目录多张按名字顺序、再按扩展名顺序取第一张;目录读不了 = None。纯函数、可测。
fn find_sidecar_cover(audio: &std::path::Path) -> Option<PathBuf> {
    let dir = audio.parent()?;
    let mut best: Option<(usize, usize, PathBuf)> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let stem = path.file_stem().and_then(|s| s.to_str()).map(str::to_ascii_lowercase);
        let ext = path.extension().and_then(|s| s.to_str()).map(str::to_ascii_lowercase);
        let (Some(stem), Some(ext)) = (stem, ext) else { continue };
        let Some(ni) = COVER_SIDECAR_NAMES.iter().position(|n| *n == stem) else { continue };
        let Some(ei) = COVER_SIDECAR_EXTS.iter().position(|e| *e == ext) else { continue };
        // 显式 match 而非 `is_none_or`(Rust 1.82 才稳定,本项目 MSRV 1.77.2)
        let better = match &best {
            None => true,
            Some((bn, be, _)) => (ni, ei) < (*bn, *be),
        };
        if better {
            best = Some((ni, ei, path));
        }
    }
    best.map(|(_, _, p)| p)
}

impl MediaRuntime {
    /// 探测结论记一行诊断(解释「为什么这片要转 / 为什么可能黑屏」)。
    fn log_local_codec(&self, path: &std::path::Path, pr: &probe::LocalProbe) {
        if pr.video_incompatible {
            tracing::warn!(path = %path.display(),
                "视频编码 WebView2 解不了(HEVC/AV1/杜比视界等),转 H.264(吃 CPU,弱机可能跟不上)");
        }
        if pr.audio_incompatible {
            tracing::info!(path = %path.display(), "音轨 WebView2 解不了(AC3/DTS 等),转 AAC");
        }
    }

    /// 本地不兼容文件:优先 **HLS 按需切片**(/hls/,前端 shaka 播 → 原生 seek + 音画同步,Stage 2)。
    /// 返回 `(stream_url, manifest_url)`:HLS 时 manifest_url 有值(前端走 shaka)。无时长(建不了 VOD
    /// 列表)→ 回落老 /m/ 渐进混流(能放、seek 仍错);ffmpeg 取不到 → 直传。绝不阻断播放。
    #[allow(clippy::too_many_arguments)]
    async fn hls_or_fallback(
        &self,
        relay: &relay::Relay,
        path: &std::path::Path,
        transcode_video: bool,
        transcode_audio: bool,
        duration: Option<f64>,
        force_software: bool,
        audio_track: usize,
    ) -> (String, Option<String>) {
        let Some(dur) = duration.filter(|d| *d > 0.0) else {
            tracing::warn!(path = %path.display(), "无时长,HLS VOD 列表建不了,回落 /m/ 渐进混流(seek 仍错)");
            return (
                self.remux_or_direct(relay, path, transcode_video, transcode_audio, force_software, audio_track)
                    .await,
                None,
            );
        };
        match self.ensure_component(Component::Ffmpeg).await {
            Ok(ffmpeg) => {
                // HLS 段一律转码视频 + 立体声 AAC(见 relay::build_frag_cmd 三处实证),
                // 故不再传 transcode_* —— 它们只在上面无时长回落 /m/ 时用。
                let url = relay
                    .register_file_hls(path.to_path_buf(), ffmpeg, dur, force_software, audio_track)
                    .await;
                (url.clone(), Some(url))
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), "ffmpeg 取不到,本地无法转码,退回直传(可能黑屏/无声): {e:#}");
                (relay.register_file(path.to_path_buf()), None)
            }
        }
    }

    /// 本地 BMFF 不兼容文件的**音视频分离自适应**(0.2.6 治本):视频按需分段(兼容→`copy` 省 CPU、
    /// 否则转 H.264)、音频**一整条连续编码** → 前端手写 MSE 播,治「逐段音频 priming 漂移」+ 省 CPU。
    /// 返回 `(stream_url, manifest_url, route)`。任一前提不满足(无时长 / ffmpeg 取不到 / init 生成失败 /
    /// 解不出 H.264 codec)→ **回落现有 muxed HLS**(能放、只是老样子),绝不阻断播放(§兜底)。
    ///
    /// 参数多(8 个)是有意的:每一个都是 `capability::plan_route` 定完后**照着执行**的开关,
    /// 攒成参数结构体只是把同样的字段搬个地方,还得连带改 `hls_or_fallback` 那一族的调用链 ——
    /// 不值。故 `#[allow(clippy::too_many_arguments)]`,别按 lint 拆。
    #[allow(clippy::too_many_arguments)]
    async fn adaptive_or_fallback(
        &self,
        relay: &relay::Relay,
        path: &std::path::Path,
        pr: &probe::LocalProbe,
        sel_audio_bad: bool,
        audio_track: usize,
        // 视频 copy 还是转码:**由 `capability::plan_route` 定**(判定单点),这里只执行。
        copy_video: bool,
        // 音频同上(P3「原声优先」):true = `-c:a copy` 原样搬。
        copy_audio: bool,
    ) -> (String, Option<String>, PlaybackRoute, Vec<SubtitleRef>) {
        let (vi, ai) = (pr.video_incompatible, sel_audio_bad);
        let Some(dur) = pr.duration_seconds.filter(|d| *d > 0.0) else {
            tracing::warn!(path = %path.display(), "无时长,自适应建不了段清单,回落 muxed HLS");
            let (su, mu) =
                self.hls_or_fallback(relay, path, vi, ai, pr.duration_seconds, false, audio_track).await;
            return (su, mu, PlaybackRoute::HlsTranscode, Vec::new());
        };
        let ffmpeg = match self.ensure_component(Component::Ffmpeg).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(path = %path.display(), "ffmpeg 取不到,自适应走不了,退直传: {e:#}");
                return (relay.register_file(path.to_path_buf()), None, PlaybackRoute::Direct, Vec::new());
            }
        };
        let segments = if copy_video {
            probe::plan_copy_segments(&pr.video_keyframes, dur, relay::HLS_SEG)
        } else {
            fixed_segments(dur, relay::HLS_SEG)
        };
        if segments.is_empty() {
            tracing::warn!(path = %path.display(), "自适应段计划为空,回落 muxed HLS");
            let (su, mu) = self.hls_or_fallback(relay, path, vi, ai, Some(dur), false, audio_track).await;
            return (su, mu, PlaybackRoute::HlsTranscode, Vec::new());
        }
        // 视频编码器:copy 路不转码(enc 用不到,免探测);转码路探硬件优先(省 CPU)。init 与各段
        // 必须同 enc → 这里定一次,gen_video_init 与 register_file_adaptive 共用(avcC 才一致可拼)。
        let enc = if copy_video {
            relay::VideoEncoder::Software
        } else {
            relay.video_encoder(&ffmpeg).await
        };
        // 生成视频 init(顺带据其 avcC 定精确 codec 串);任一失败 → 回落 muxed HLS。
        let Some(init) = relay::gen_video_init(&ffmpeg, path, copy_video, enc).await else {
            tracing::warn!(path = %path.display(), "自适应 init 生成失败,回落 muxed HLS");
            let (su, mu) = self.hls_or_fallback(relay, path, vi, ai, Some(dur), false, audio_track).await;
            return (su, mu, PlaybackRoute::HlsTranscode, Vec::new());
        };
        let Some(codec) = probe::video_h264_codec(&init) else {
            tracing::warn!(path = %path.display(), "自适应 init 解不出 H.264 codec,回落 muxed HLS");
            let (su, mu) = self.hls_or_fallback(relay, path, vi, ai, Some(dur), false, audio_track).await;
            return (su, mu, PlaybackRoute::HlsTranscode, Vec::new());
        };
        // 字幕来源(P4):内嵌的文本轨(图形轨如 PGS 转不了,不收 —— 挂上去只会一片空白)+
        // 同目录的外挂文件。**默认不开**,前端把它们挂成 `<track>` 由用户选。
        let subs: Vec<relay::SubSource> = pr
            .subtitles
            .iter()
            .filter(|s| s.text)
            .map(|s| relay::SubSource::Embedded(s.index))
            .chain(probe::sidecar_subtitles(path).into_iter().map(|(p, _)| relay::SubSource::Sidecar(p)))
            .collect();
        // **接缝抽查(契约③的运行时半边)**:拿第一段量一下真实时长,与计划对不上就整条降级到
        // 已验证的 muxed 老路 —— 段边界错位 = 音画不同步,宁可多转码也不让它上台。开发侧那条
        // e2e(`copy_segments_have_no_cumulative_drift`)量全片,这里只抽一段:代价 = 起播多切
        // 一段(copy 段近乎白拿),换「这台机器上这个片子确实切得准」的现场证据。
        // 量不出来(解析不出 timescale / 段拿不到)= 不作数,照原路走(闸只拦确凿的错)。
        if let Some((_, ts)) = probe::init_timescales(&init).first().copied() {
            if let Some((seg_start, seg_dur)) = segments.first().copied() {
                if let Some(bytes) =
                    relay::gen_video_segment(&ffmpeg, path, seg_start, seg_dur, copy_video, enc).await
                {
                    if let Some(real) = probe::fragment_duration(&bytes, ts) {
                        if let Some(d) = timeline::cumulative_drift(
                            &[seg_dur],
                            &[real],
                            timeline::SEAM_TOL,
                        ) {
                            tracing::warn!(path = %path.display(), planned = seg_dur, real,
                                drift = d.secs, "首段时长与计划对不上,回落 muxed HLS(防音画错位)");
                            let (su, mu) =
                                self.hls_or_fallback(relay, path, vi, ai, Some(dur), false, audio_track).await;
                            return (su, mu, PlaybackRoute::HlsTranscode, Vec::new());
                        }
                        tracing::info!(path = %path.display(), planned = seg_dur, real, "首段接缝抽查通过");
                    }
                }
            }
        }
        let video_mime = format!("video/mp4; codecs=\"{codec}\"");
        let route = if copy_video { PlaybackRoute::HlsCopy } else { PlaybackRoute::HlsTranscode };
        tracing::info!(path = %path.display(), copy = copy_video, segs = segments.len(),
            "自适应播放(音视频分离,连续音频治漂移)");
        // 字幕地址与 desc 同一个 token,故在这里(拿到 url 之后)组清单;外挂的标出来源、内嵌的不标。
        let sidecar_from = pr.subtitles.iter().filter(|s| s.text).count();
        let langs: Vec<Option<String>> = pr
            .subtitles
            .iter()
            .filter(|s| s.text)
            .map(|s| s.lang.clone())
            .chain(probe::sidecar_subtitles(path).into_iter().map(|(_, l)| l))
            .collect();
        let url = relay.register_file_adaptive(
            path.to_path_buf(), ffmpeg, copy_video, copy_audio, video_mime, init, segments, dur, enc, audio_track, subs,
        );
        let sub_refs = langs
            .into_iter()
            .enumerate()
            .map(|(i, lang)| SubtitleRef {
                url: url.replace("desc", &format!("sub{i}.vtt")),
                lang,
                sidecar: i >= sidecar_from,
            })
            .collect();
        (url.clone(), Some(url), route, sub_refs)
    }

    /// 取 ffmpeg 注册转封装/转码 URL(走 /m/);ffmpeg 取不到则退回原生直传。HLS 无时长时的回落用。
    async fn remux_or_direct(
        &self,
        relay: &relay::Relay,
        path: &std::path::Path,
        transcode_video: bool,
        transcode_audio: bool,
        force_software: bool,
        audio_track: usize,
    ) -> String {
        match self.ensure_component(Component::Ffmpeg).await {
            Ok(ffmpeg) => {
                relay
                    .register_file_remux(
                        path.to_path_buf(),
                        ffmpeg,
                        transcode_video,
                        transcode_audio,
                        force_software,
                        audio_track,
                    )
                    .await
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), "ffmpeg 取不到,退回直传(可能黑屏/无声): {e:#}");
                relay.register_file(path.to_path_buf())
            }
        }
    }

    /// 放歌的「锦上添花」件:歌手 / 专辑标签 + 封面来源。**只用已到手的 ffmpeg**(`Components::ready`,
    /// 不为它下载 —— 播放本身早已后台预取,新装机器至多第一首没有),没有 ffmpeg 就只找侧车图。
    /// 封面优先级:文件内嵌图(mp3 APIC / flac PICTURE / m4a covr,`ffmpeg -i` 的 attached pic 流)>
    /// 同目录侧车图(`find_sidecar_cover`);都没有 = None,前端显 ♪ 占位(§3.5 不假装有图)。
    /// 任何一步不顺都不挡播放。
    async fn audio_extras(&self, relay: &relay::Relay, path: &std::path::Path) -> AudioExtras {
        let mut out = AudioExtras::default();
        if let Some(ff) = self.inner.components.ready(Component::Ffmpeg) {
            let pr = self.probe_with_ffmpeg(&ff, path).await;
            out.author = pr.tag_artist;
            out.album = pr.tag_album;
            out.duration = pr.duration_seconds;
            if pr.attached_pic {
                out.cover_url = Some(relay.register_cover(relay::CoverSrc::Embedded {
                    path: path.to_path_buf(),
                    ffmpeg: ff,
                }));
                return out;
            }
        }
        let p = path.to_path_buf();
        let side = tokio::task::spawn_blocking(move || find_sidecar_cover(&p)).await.ok().flatten();
        if let Some(side) = side {
            out.cover_url = Some(relay.register_cover(relay::CoverSrc::Sidecar(side)));
        }
        out
    }

    /// 用已就绪的 ffmpeg 探测非 BMFF 容器(mkv/avi…):跑 `ffmpeg -i` 读 stderr 拿编码/时长。
    /// `-i` 无输出会非零退出但信息照打 stderr → 不看退出码、只解析 stderr;探不出按全兼容降级。
    pub(super) async fn probe_with_ffmpeg(
        &self,
        ffmpeg: &std::path::Path,
        path: &std::path::Path,
    ) -> probe::LocalProbe {
        let mut cmd = tokio::process::Command::new(ffmpeg);
        cmd.arg("-hide_banner").arg("-i").arg(path);
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        no_console(&mut cmd);
        match tokio::time::timeout(std::time::Duration::from_secs(20), cmd.output()).await {
            Ok(Ok(out)) => probe::parse_ffmpeg_stderr(&String::from_utf8_lossy(&out.stderr)),
            _ => {
                tracing::warn!(path = %path.display(), "ffmpeg 探测失败/超时,按全兼容降级");
                probe::LocalProbe::default()
            }
        }
    }

    /// 扫关键帧(P2):**copy 快车道对所有容器开放**的前提 —— 段边界只能落真实 IDR,而 mkv/ts
    /// 没有 moov 的 stss 表可读。`-skip_frame nokey` 让解码器只解关键帧(demux 全程、不解 P/B 帧);
    /// **`ffprobe` 没随包**(components 只解出 `ffmpeg`),所以用 ffmpeg 自己。本机实测 10 分钟
    /// 720p mkv:300 个关键帧 / 0.31s。
    ///
    /// 返回空 = 这张表不能信(扫不出 / 超时 / 没过 `validate_keyframes` 的闸)→ 调用方走转码路。
    /// **异常一律退回不猜**:拿半张错表去 copy 切段 = 段边界错位 = 音画事故(§7.1 保守偏向)。
    /// 超时给到 120s:大片扫描是 IO 密集的,比 `-i` 探测久,但仍远短于一次整片转码。
    async fn scan_keyframes(
        &self,
        ffmpeg: &std::path::Path,
        path: &std::path::Path,
        duration: f64,
    ) -> Vec<f64> {
        let mut cmd = tokio::process::Command::new(ffmpeg);
        cmd.arg("-hide_banner")
            .arg("-loglevel")
            .arg("info") // showinfo 打在 info 级 —— 压成 error 就一行都收不到
            .arg("-nostdin")
            .arg("-skip_frame")
            .arg("nokey")
            .arg("-i")
            .arg(path)
            .arg("-map")
            .arg("0:v:0")
            .arg("-vf")
            .arg("showinfo")
            .arg("-f")
            .arg("null")
            .arg("-");
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        no_console(&mut cmd);
        let out = match tokio::time::timeout(std::time::Duration::from_secs(120), cmd.output()).await
        {
            Ok(Ok(out)) => out,
            _ => {
                tracing::warn!(path = %path.display(), "关键帧扫描失败/超时,这片走转码路");
                return Vec::new();
            }
        };
        let kf = probe::parse_showinfo_keyframes(&String::from_utf8_lossy(&out.stderr));
        match probe::validate_keyframes(&kf, duration) {
            Ok(()) => {
                tracing::info!(path = %path.display(), n = kf.len(), "关键帧表可用,copy 快车道开");
                kf
            }
            Err(why) => {
                tracing::warn!(path = %path.display(), n = kf.len(), why, "关键帧表不可信,这片走转码路");
                Vec::new()
            }
        }
    }

    /// 兜底重放(前端手写 MSE〔本地自适应〕播放失败时调):对同一本地文件**强制走 muxed HLS**
    /// (能放的老路,会漂但不黑屏,§3.5 不静默失败)。`play_local` 内部已发 Play 事件替换当前播放。
    /// pos=None:兜底不重建剧集队列(先保能放;切集另说)。仅本地文件有意义(网络路本就 shaka)。
    pub async fn replay_local_compat(&self, page_url: &str, audio_only: bool) -> Result<()> {
        self.play_local(page_url, audio_only, None, false, None).await?;
        Ok(())
    }

    /// 本地文件(含 NAS 挂载/UNC):跳过 yt-dlp,注册文件端点即播 —— 单文件免混流,
    /// Range 原生 seek 白送,秒级无任务进度可言,不上 HUD。
    ///
    /// 视频按**真实容器 + 编码**三分路(§8.1「WebView2 编解码坑」的本地补课,网络路径早强制了
    /// avc+m4a,本地直传此前全漏了):**只转 WebView2 处理不了的那部分**(§7.1 用户拍板「按需」)。
    /// 容器一律**嗅文件头认**(`probe::sniff_container`)、**不看扩展名** —— 扩展名会骗人,
    /// 2026-08-07 真机实锤 `.mp4` 里装 mpegts,按名字分流直传成黑屏;VLC 这类播放器从来嗅头,
    /// 所以「播放器都没问题」:
    ///   · 真 BMFF(能读出 moov):轻量探测(不下 ffmpeg),全兼容则原生直传秒开;音轨 AC3/DTS
    ///     或视频 HEVC 不兼容才取 ffmpeg 转(兼容轨 -c copy、不兼容轨才转码);
    ///   · 认出来是浏览器放不了的容器(mkv/mpegts/avi/flv/wmv/rm…),或头是 BMFF 但 moov 读不出
    ///     (残缺文件):必经 ffmpeg 转封装成 fMP4,先确保 ffmpeg、`ffmpeg -i` 探编码,再按需 copy/转码;
    ///   · 浏览器原生容器(webm/ogg)/ 认不出的 / 放歌(audio_only):直传,交给浏览器。
    ///
    /// `pos` = 这一集在本地剧集队列里的位置(None = 单文件);写进 `NowPlaying.playlist` 驱动续播 UI。
    /// `prefer_adaptive`:true(常态)= 不兼容 BMFF 走音视频分离自适应(治漂移 + copy 省 CPU);
    /// false = 前端手写 MSE 播放失败后的**兜底重放**,强制回落 muxed HLS(能放的老路)。
    pub(super) async fn play_local(
        &self,
        path_str: &str,
        audio_only: bool,
        pos: Option<PlaylistPos>,
        prefer_adaptive: bool,
        resume_at: Option<f64>,
    ) -> Result<NowPlaying> {
        let path = std::path::PathBuf::from(path_str);
        let meta = tokio::fs::metadata(&path)
            .await
            .with_context(|| format!("文件不存在或读不到: {path_str}"))?;
        anyhow::ensure!(meta.is_file(), "{path_str} 不是文件");
        let title = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| path_str.to_string());
        let relay = self
            .inner
            .relay
            .get_or_try_init(relay::Relay::start)
            .await
            .context("转发服务起不来")?;

        // 三分路(详见上方 doc):放歌直传 / 真 BMFF 轻量探测 / 其余容器走 ffmpeg。不兼容文件走 HLS
        //(manifest_url 有值 → 前端 shaka 播 → 原生 seek + 音画同步);兼容文件原生 /f/ 直传。
        let mut duration_seconds = None;
        let mut manifest_url: Option<String> = None;
        let mut route: Option<PlaybackRoute> = None;
        let mut subtitle_refs: Vec<SubtitleRef> = Vec::new(); // P4:字幕清单(自适应路才有) // Some = 显式(自适应路 URL 反推不出 copy/转码)
        let mut chapters: Vec<probe::Chapter> = Vec::new(); // 章节(ffmpeg 探测路才有;OP/ED 章节 → 自动跳)
        let mut tracks: Vec<probe::AudioTrack> = Vec::new(); // 音轨清单(BMFF/容器探测才有)
        let mut sel_track = 0usize; // 选中音轨(越界已钳回 0)
        // 分流看**文件内容**、不看扩展名(2026-08-07 实锤:`.mp4` 里装的是 mpegts,
        // h264+aac 浏览器明明吃得下,卡在容器上 —— 按扩展名分流就直传成黑屏)。
        let sniffed = if audio_only {
            probe::Container::Native // 放歌走直传快车道,不为它开文件嗅探
        } else {
            let p = path.clone();
            tokio::task::spawn_blocking(move || probe::sniff_container(&p))
                .await
                .unwrap_or(probe::Container::Native)
        };
        // 真 BMFF 才读 moov(同步 IO 挪 spawn_blocking);读不出 = 残缺/头对内容不对 → 也交 ffmpeg。
        let bmff = if sniffed == probe::Container::Bmff {
            let p = path.clone();
            tokio::task::spawn_blocking(move || probe::probe_local(&p)).await.unwrap_or_default()
        } else {
            None
        };
        // 判定收口(P0):`capability::plan_source` 定怎么探、`plan_route` 定走哪条路,本函数只执行。
        // 「这条轨要不要转」从此只有那一个文件回答得了 —— 原先散在两张白名单 + 这里的分支 +
        // relay 的 copy 旗标四处,词汇还各不相同(历次黑屏/混播事故的裂缝)。
        let source = capability::plan_source(audio_only, sniffed);
        let stream_url = match (source, bmff) {
            (capability::Source::AudioDirect | capability::Source::NativeDirect, _) => {
                // 放歌 / webm·ogg 等浏览器原生容器 / 认不出的 → 直传,交给浏览器
                relay.register_file(path.clone())
            }
            (capability::Source::Bmff, Some(pr)) => {
                duration_seconds = pr.duration_seconds;
                tracks = pr.audio_tracks.clone();
                sel_track = self.pick_audio_track(&tracks);
                // 逐轨判定:多音轨片只看**选中那条**要不要转(选中 AAC 轨 = 音频可 copy)。
                let sel_audio_bad = tracks
                    .get(sel_track)
                    .map(|t| probe::audio_codec_needs_transcode(&t.codec))
                    .unwrap_or(pr.audio_incompatible);
                let facts = capability::Facts {
                    duration: pr.duration_seconds,
                    video_incompatible: pr.video_incompatible,
                    sel_audio_incompatible: sel_audio_bad,
                    audio_track_count: tracks.len(),
                    has_keyframes: !pr.video_keyframes.is_empty(),
                    has_video_codec: pr.video_codec.is_some(),
                    sel_audio_codec: tracks.get(sel_track).map(|t| t.codec.clone()),
                    sel_audio_channels: tracks.get(sel_track).and_then(|t| t.channels),
                };
                let plan = capability::plan_route(source, &facts, prefer_adaptive);
                if !matches!(plan, capability::Plan::Direct) {
                    self.log_local_codec(&path, &pr);
                }
                match plan {
                    capability::Plan::Direct => relay.register_file(path.clone()), // 全兼容:秒开
                    capability::Plan::Adaptive { video, audio } => {
                        // 0.2.6 治本:音视频分离 —— 视频 copy 省 CPU / 转码,音频一整条连续编码
                        //(治逐段 priming 漂移)。执行期前提不满足(ffmpeg 缺 / init 生不出 /
                        // 段计划空)自动回落 muxed HLS,绝不阻断。
                        let (su, mu, r, sr) = self
                            .adaptive_or_fallback(
                                relay,
                                &path,
                                &pr,
                                sel_audio_bad,
                                sel_track,
                                video == capability::Track::Copy,
                                audio == capability::Track::Copy,
                            )
                            .await;
                        manifest_url = mu;
                        route = Some(r);
                        subtitle_refs = sr;
                        su
                    }
                    capability::Plan::MuxedHls { force_software } => {
                        if !force_software {
                            // 常态却落 muxed = 自适应前提不满足(今天唯一的因:没时长,段清单建不了)
                            tracing::warn!(path = %path.display(), "无时长,自适应建不了段清单,回落 muxed HLS");
                            route = Some(PlaybackRoute::HlsTranscode);
                        }
                        // force_software = 前端手写 MSE 播放失败后的兜底重放:走已验证的老路 +
                        // 软件编码(硬件编码万一在这台机上花屏/解不了,这一步换回来)。
                        let (su, mu) = self
                            .hls_or_fallback(relay, &path, pr.video_incompatible, sel_audio_bad, pr.duration_seconds, force_software, sel_track)
                            .await;
                        manifest_url = mu;
                        su
                    }
                    // BMFF 路不会出渐进流(plan_route 的 Bmff 臂不产它);真出了按直传兜底不 panic
                    capability::Plan::Progressive { .. } => relay.register_file(path.clone()),
                }
            }
            // 浏览器放不了的容器(mkv/mpegts/avi/flv/wmv/rm…),以及**「头是 BMFF 但 moov 读不出」
            // 的残缺件**(上面那条 `Some(pr)` 没匹配上 = 探不出)—— 一律交 ffmpeg 现探现转:
            // 「探不出」绝不能再被当成「全兼容」直传(2026-08-07 黑屏修法)。
            _ => {
                if let probe::Container::Foreign(kind) = sniffed {
                    if probe::is_isobmff_ext(&path) {
                        // 扩展名骗人:留一行现场,免得下次又要从库里刨(§3.5 不静默)
                        tracing::info!(path = %path.display(), kind, "扩展名说是 mp4,实际容器不是,交给 ffmpeg 转封装");
                    }
                }
                match self.ensure_component(Component::Ffmpeg).await {
                    Ok(ffmpeg) => {
                        let pr = self.probe_with_ffmpeg(&ffmpeg, &path).await;
                        duration_seconds = pr.duration_seconds;
                        tracks = pr.audio_tracks.clone();
                        chapters = pr.chapters.clone();
                        sel_track = self.pick_audio_track(&tracks);
                        let sel_audio_bad = tracks
                            .get(sel_track)
                            .map(|t| probe::audio_codec_needs_transcode(&t.codec))
                            .unwrap_or(pr.audio_incompatible);
                        self.log_local_codec(&path, &pr);
                        // P2:mkv/ts 没有 moov 的 stss 表 → 用 ffmpeg 扫一遍关键帧,拿到了才敢开
                        // copy 快车道(扫不出/不可信 = 空表 → 判定自然回落转码路,绝不半张表切段)。
                        let mut pr = pr;
                        if prefer_adaptive {
                            if let Some(dur) = pr.duration_seconds.filter(|d| *d > 0.0) {
                                pr.video_keyframes = self.scan_keyframes(&ffmpeg, &path, dur).await;
                            }
                        }
                        let facts = capability::Facts {
                            duration: pr.duration_seconds,
                            video_incompatible: pr.video_incompatible,
                            sel_audio_incompatible: sel_audio_bad,
                            audio_track_count: tracks.len(),
                            has_keyframes: !pr.video_keyframes.is_empty(),
                            // 容器路的 `avc1.xxxxxx` 串要等 init 生成后才解得出(`gen_video_init`
                            // 会做,解不出自动回落 muxed)→ 判定这里乐观放行,由执行侧兜底。
                            has_video_codec: true,
                            sel_audio_codec: tracks.get(sel_track).map(|t| t.codec.clone()),
                            sel_audio_channels: tracks.get(sel_track).and_then(|t| t.channels),
                        };
                        let plan =
                            capability::plan_route(capability::Source::Ffmpeg, &facts, prefer_adaptive);
                        // 逐轨结论(渐进流用);plan 给不出时按事实兜底 —— 二者同源,实际不分叉。
                        let (tv, ta) = match plan {
                            capability::Plan::Progressive { video, audio } => (
                                video == capability::Track::Transcode,
                                audio == capability::Track::Transcode,
                            ),
                            _ => (pr.video_incompatible, sel_audio_bad),
                        };
                        match (plan, pr.duration_seconds.filter(|d| *d > 0.0)) {
                            (capability::Plan::Adaptive { video, audio }, Some(_)) => {
                                // 与 BMFF 同一条自适应路:视频 copy 省 CPU / 转码,音频离散段治漂移。
                                let (su, mu, r, sr) = self
                                    .adaptive_or_fallback(
                                        relay,
                                        &path,
                                        &pr,
                                        sel_audio_bad,
                                        sel_track,
                                        video == capability::Track::Copy,
                                        audio == capability::Track::Copy,
                                    )
                                    .await;
                                manifest_url = mu;
                                route = Some(r);
                                subtitle_refs = sr;
                                su
                            }
                            (capability::Plan::MuxedHls { force_software }, Some(dur)) => {
                                // HLS 段一律转码视频 + 立体声 AAC(relay::build_frag_cmd),不传 transcode_*。
                                let url = relay
                                    .register_file_hls(path.clone(), ffmpeg, dur, force_software, sel_track)
                                    .await;
                                manifest_url = Some(url.clone());
                                url
                            }
                            // 拿不到时长 → `/m/` 渐进流(已知无原生 seek),逐轨按需转。
                            _ => {
                                relay
                                    .register_file_remux(
                                        path.clone(),
                                        ffmpeg,
                                        tv,
                                        ta,
                                        !prefer_adaptive,
                                        sel_track,
                                    )
                                    .await
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(path = %path.display(), "ffmpeg 取不到,容器无法转封装,退回直传(可能放不了): {e:#}");
                        relay.register_file(path.clone())
                    }
                }
            }
        };

        // 放歌的锦上添花:歌手 / 专辑标签 + 封面(内嵌图 > 侧车图);只用已到手的 ffmpeg,拿不到不挡播放。
        let extras =
            if audio_only { self.audio_extras(relay, &path).await } else { AudioExtras::default() };
        if duration_seconds.is_none() {
            duration_seconds = extras.duration; // 直传路原本不探时长,标签探测顺手带回来(前端元数据到了会再校准)
        }
        // 进度条 hover 预览:放歌没画面不出图;视频**只在 ffmpeg 已经在手时**注册 —— 用
        // `Components::ready`(绝不下载):预览图这种锦上添花的事不值得为它拉一次组件下载,
        // 而播放本身早已在后台预取 ffmpeg(`prefetch_ffmpeg`),所以新装机器至多是第一次看片
        // 没有预览图,之后都有。拿不到 = thumb_url None = 前端只出时间气泡,不发无用请求。
        let thumb_url = if audio_only {
            None
        } else {
            self.inner
                .components
                .ready(Component::Ffmpeg)
                .map(|ffmpeg| relay.register_thumbs(path.clone(), ffmpeg))
        };
        let route = route.unwrap_or_else(|| derive_route(&stream_url, manifest_url.as_deref()));
        // 记下本地现场:切音轨据此重建管线。多音轨时把清单打进日志(真机对「音轨没名字」
        // 一眼定案:是文件本身没标语言〔und〕还是解析漏了)。
        if tracks.len() >= 2 {
            tracing::info!(path = %path.display(), tracks = ?tracks, track = sel_track, "音轨清单");
        }
        *self.inner.current_local.lk() = Some(CurrentLocal {
            page_url: path_str.to_string(),
            audio_only,
            tracks: tracks.clone(),
        });
        // 片头 / 片尾:章节里名为 OP/ED 的段 + 库里的手标 / 检测结果汇成本集怎么跳(放歌不跳)
        let skip_info = if audio_only {
            None
        } else {
            self.compute_skip(skip::chapters_to_auto(&chapters), duration_seconds)
        };
        let np = NowPlaying {
            subtitles: subtitle_refs.clone(),
            // 放歌带旁挂歌词;切集/连播每次重进这里 → 换曲自动换词
            lyrics: if audio_only { lyrics::sidecar_lyrics(&path) } else { None },
            kind: if audio_only { MediaKind::Audio } else { MediaKind::Video },
            title,
            author: extras.author,
            album: extras.album,
            cover_url: extras.cover_url,
            duration_seconds,
            // 自适应路显式给 route(URL 是 /la/,反推不出 copy/转码);其余路由由 URL 反推。
            route,
            stream_url,
            manifest_url,
            page_url: path_str.into(),
            source: "local".into(),
            playlist: pos,
            play_mode: self.play_mode_str(),
            rate: self.rate(),
            audio_tracks: tracks,
            audio_track: sel_track,
            resume_at,
            thumb_url,
            skip: skip_info,
        };
        self.seed_playing(&np.title, pos.map(|p| (p.index, p.total)));
        self.publish(MediaEvent::Play(np.clone()));
        // 本地视频剧集:后台起指纹检测(本集还没测过才跑;ffmpeg 不在手 / 上一个还在跑就先不起)
        if !audio_only && pos.is_some() {
            self.maybe_spawn_detect();
        }
        Ok(np)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::testkit::*;

    #[test]
    fn sidecar_cover_prefers_cover_over_folder_case_insensitive() {
        let dir = std::env::temp_dir().join(format!("lw-sidecar-cover-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let song = touch(&dir, "歌.flac");
        assert!(find_sidecar_cover(&song).is_none(), "没有侧车图");
        touch(&dir, "Folder.JPG");
        assert_eq!(
            find_sidecar_cover(&song).unwrap().file_name().unwrap().to_str().unwrap(),
            "Folder.JPG",
            "大小写不敏感"
        );
        touch(&dir, "cover.png");
        assert_eq!(
            find_sidecar_cover(&song).unwrap().file_name().unwrap().to_str().unwrap(),
            "cover.png",
            "名字顺序 cover > folder"
        );
        touch(&dir, "cover.jpg");
        assert_eq!(
            find_sidecar_cover(&song).unwrap().file_name().unwrap().to_str().unwrap(),
            "cover.jpg",
            "同名按扩展名顺序 jpg > png"
        );
        touch(&dir, "back.jpg"); // 不在清单里的名字不算
        assert_eq!(find_sidecar_cover(&song).unwrap().file_name().unwrap(), "cover.jpg");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn play_local_serves_file_through_relay() {
        let (rt, mut rx) = runtime("local");
        let dir = std::env::temp_dir().join(format!("lw-media-local-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("儿歌串烧.mp3");
        std::fs::write(&f, b"FAKE-MP3-BYTES").unwrap();

        let np = match rt.play(1, &f.to_string_lossy(), true, false, None).await.unwrap() {
            PlayOutcome::Playing(np) => np,
            other => panic!("本地文件应为 Playing,实际 {other:?}"),
        };
        assert_eq!(np.source, "local");
        assert_eq!(np.title, "儿歌串烧");
        assert!(matches!(np.kind, MediaKind::Audio));
        assert!(np.stream_url.contains("/f/"));
        // 流真的能拉到字节
        let body = reqwest::get(&np.stream_url).await.unwrap().bytes().await.unwrap();
        assert_eq!(&body[..], b"FAKE-MP3-BYTES");
        // 全局车道上跑的不只有播放事件(组件下载的任务卡等杂事同走这里),所以断言「Play 发出来了」
        // 而不是「第一条就是 Play」—— 顺序不作数。
        let saw_play = std::iter::from_fn(|| rx.try_recv().ok())
            .any(|e| matches!(e, AppEvent::Media(MediaEvent::Play(_))));
        assert!(saw_play, "起播该发 Play 事件");

        // 不存在的文件 = 错误观察
        assert!(rt.play(1, "/no/such/file.mp4", false, false, None).await.is_err());
    }

    /// 分流按内容不按扩展名的端到端守卫(2026-08-07 黑屏回归):`.mp4` 里装 MPEG-TS —— 编码
    /// (h264+aac)浏览器完全吃得下,卡的是容器,所以**必须**经 ffmpeg 转封装、绝不能直传。
    /// 从前按扩展名走 BMFF 快车道 → 探不出 → 当「全兼容」直传 → Mac/Windows 一起黑屏 0:00。
    /// 需 PATH 有 ffmpeg,故 `#[ignore]`:
    /// `cargo test -p larkwing-core --lib media::mod -- --ignored mislabeled --nocapture`
    #[tokio::test]
    #[ignore]
    async fn mislabeled_container_routes_to_ffmpeg_not_direct() {
        let (rt, _rx) = runtime("mislabel");
        let dir = std::env::temp_dir().join(format!("lw-mislabel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("动画片第1集.mp4"); // 名字是 mp4,内容是 mpegts
        let ok = std::process::Command::new("ffmpeg")
            .args([
                "-y", "-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i",
                "testsrc=size=320x240:rate=25:duration=8", "-f", "lavfi", "-i",
                "sine=frequency=440:sample_rate=48000:duration=8", "-c:v", "libx264", "-preset",
                "ultrafast", "-pix_fmt", "yuv420p", "-c:a", "aac", "-f", "mpegts",
            ])
            .arg(&fake)
            .status()
            .expect("run ffmpeg")
            .success();
        assert!(ok, "生成 mpegts 夹具失败");
        assert_eq!(probe::sniff_container(&fake), probe::Container::Foreign("mpegts"));

        let np = match rt.play(1, &fake.to_string_lossy(), false, false, None).await.unwrap() {
            PlayOutcome::Playing(np) => np,
            other => panic!("应为 Playing,实际 {other:?}"),
        };
        assert!(
            np.manifest_url.is_some(),
            "扩展名骗人的 TS 必须经 ffmpeg,不能直传,实际 stream_url={}",
            np.stream_url
        );
        assert!(np.duration_seconds.unwrap_or(0.0) > 7.0, "时长该由 ffmpeg 探出来");
        let body = reqwest::get(np.manifest_url.as_ref().unwrap())
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        // **P2 起这里的预期变了**:容器路扫到关键帧表后与 BMFF 同权 → 走音视频分离自适应
        // (`/la/desc` JSON)且**视频 copy**;这片是标准 H.264,只是容器不对,本就不该重编。
        // 扫不出关键帧时会回落 muxed HLS(`#EXTM3U`),那也是合法结局 —— 但对这个夹具应当是 copy,
        // 掉回 muxed 就说明关键帧扫描坏了,正是要它红给我们看。
        assert!(
            body.contains("\"copyVideo\":true"),
            "标准 H.264 的 TS 应走自适应 copy 快车道(不重编),实际: {body}"
        );
        assert!(body.contains("\"segments\""), "自适应 desc 应带段清单: {body}");
    }

    /// **契约③的实弹:机器抓漂移,不靠耳朵**(P2,2026-08-07)。
    ///
    /// 真 ffmpeg 端到端:造一个关键帧每 2s 的 mkv → 走完整 `play_local` 链 → 把**每一段视频**
    /// 取回来、拼上 init、用 ffmpeg 量**真实时长** → 与段清单里的计划时长逐段对账
    /// (`timeline::cumulative_drift` 累加,容差 `SEAM_TOL`)。
    ///
    /// 它能抓住的正是这块历史上反复踩的那类事故:段实际长度与计划不符 → 前端按计划起点摆放
    /// (tfdt)→ 每段错一点、累计成音画不同步。**单段容差放得下的偏移,累计放不下。**
    /// 也顺带回答一个悬着的问题:`COPY_SS_EPS`(+1ms)在容器路到底该不该沿用 —— 真漂了它就红。
    ///
    /// 需 PATH 有 ffmpeg:`cargo test -p larkwing-core --lib media::tests::copy_segments -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn copy_segments_have_no_cumulative_drift() {
        // 两种容器各跑一遍。**mpegts 是关键的第二例**:它的起始 PTS 天生非 0(实测 1.4),
        // 而段清单是 0 基的 —— 这一遍过了就证明「ffmpeg 已把时间轴归一到最早那条轨」,
        // 我们**不该再自己减一次 t0**(双重修正反而造出偏移)。
        for container in ["matroska", "mpegts"] {
            drift_free_copy_segments(container).await;
        }
    }

    async fn drift_free_copy_segments(container: &str) {
        let (rt, _rx) = runtime("drift");
        let dir = std::env::temp_dir()
            .join(format!("lw-drift-{}-{container}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("片子.mkv"); // 名字一律 .mkv:分流看文件头不看扩展名
        // 40s、25fps、关键帧每 2s(-g 50 + 关掉场景切额外关键帧)→ 段按 6s 目标凑到关键帧边界。
        let ok = std::process::Command::new("ffmpeg")
            .args([
                "-y", "-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i",
                "testsrc=size=320x240:rate=25:duration=40", "-f", "lavfi", "-i",
                "sine=frequency=440:sample_rate=48000:duration=40", "-c:v", "libx264", "-preset",
                "ultrafast", "-pix_fmt", "yuv420p", "-g", "50", "-keyint_min", "50",
                "-sc_threshold", "0", "-c:a", "ac3", "-f", container,
            ])
            .arg(&src)
            .status()
            .expect("run ffmpeg")
            .success();
        assert!(ok, "生成 {container} 夹具失败");

        let np = match rt.play(1, &src.to_string_lossy(), false, false, None).await.unwrap() {
            PlayOutcome::Playing(np) => np,
            other => panic!("应为 Playing,实际 {other:?}"),
        };
        let base = np.manifest_url.as_ref().expect("mkv 应走自适应").trim_end_matches("desc");
        let desc: serde_json::Value =
            reqwest::get(format!("{base}desc")).await.unwrap().json().await.unwrap();
        assert_eq!(desc["copyVideo"], true, "H.264 的 {container} 该走 copy 快车道,实际 desc={desc}");
        let segs = desc["segments"].as_array().expect("段清单");
        let planned: Vec<f64> = segs.iter().map(|s| s["dur"].as_f64().expect("dur")).collect();
        // 首段起点**未必是 0**(mkv/ts 的首关键帧常带偏移,实测 0.023)—— 差分的起点要用它,
        // 拿 0 当起点会把这份偏移误算成第一段变长(我第一版就这么错的)。
        let first_start = segs[0]["start"].as_f64().expect("start");
        assert!(planned.len() >= 4, "40s / 6s 目标该切出好几段,实际 {}", planned.len());

        // init + 段 拼起来才是一个能被 ffmpeg 读的完整 fMP4(MSE 也是这么吃的)。
        let init = reqwest::get(format!("{base}vinit")).await.unwrap().bytes().await.unwrap();
        // 运行时抽查用的量具(`probe::fragment_duration`,纯解析零进程)必须与「用 ffmpeg 量」
        // 对得上 —— 合成夹具再像也不等于真 ffmpeg 的排布,这里拿真段交叉验证一次。
        let ts = probe::init_timescales(&init).first().copied().expect("init 该解出 timescale").1;
        let mut measured = Vec::new();
        for (i, &want) in planned.iter().enumerate() {
            let seg = reqwest::get(format!("{base}v{i}")).await.unwrap().bytes().await.unwrap();
            let parsed = probe::fragment_duration(&seg, ts).expect("真段该量得出时长");
            assert!(
                (parsed - want).abs() < timeline::SEAM_TOL,
                "[{container}] 第 {i} 段:解析量得 {parsed}s,计划 {want}s —— 运行时抽查的量具与计划对不上",
            );
            let tmp = dir.join(format!("seg{i}.mp4"));
            let mut bytes = init.to_vec();
            bytes.extend_from_slice(&seg);
            std::fs::write(&tmp, &bytes).unwrap();
            let out = std::process::Command::new("ffmpeg")
                .arg("-hide_banner")
                .arg("-i")
                .arg(&tmp)
                .output()
                .expect("run ffmpeg");
            let d = probe::parse_ffmpeg_stderr(&String::from_utf8_lossy(&out.stderr))
                .duration_seconds
                .unwrap_or_else(|| panic!("第 {i} 段量不出时长"));
            measured.push(d);
        }
        // ⚠️ ffmpeg 量出来的是**段的终点**不是段长 —— 因为每段的 tfdt 已被改成累计起点
        //(契约②:段时间戳由我们写),读到的 fMP4 自带绝对位置。这本身就是一条端到端证据:
        // 各段确实落在自己的绝对位置上,没有「全堆在 0 秒」。真实段长 = 终点逐段差分。
        let ends = measured.clone();
        assert!(ends.windows(2).all(|w| w[1] > w[0]), "段终点必须递增: {ends:?}");
        let mut real_durs = Vec::with_capacity(ends.len());
        let mut prev = first_start;
        for e in &ends {
            real_durs.push(e - prev);
            prev = *e;
        }
        println!("[{container}] 计划段长: {planned:?}\n实测终点: {ends:?}\n实测段长: {real_durs:?}");
        assert_eq!(
            timeline::cumulative_drift(&planned, &real_durs, timeline::SEAM_TOL),
            None,
            "段实际时长与计划**累计**对不上 —— 这正是音画不同步的来源\n计划 {planned:?}\n实测 {real_durs:?}"
        );
    }

    /// **P3 音频 copy 的对齐验收(真 ffmpeg e2e)**:copy 段落在 AAC 帧边界(~21ms 粒度),
    /// 而前端 `appendWindow`/`timestampOffset` 那套是按「转码段起点精确」设计的 —— 中间会不会
    /// 留缝只能量,不能推理(这正是这块反复踩的那类事)。
    ///
    /// 判据 = **每段都必须完整盖住它那 6s 窗口**:盖不住就是缝,前端裁完会空一块 → 卡顿/漂移。
    /// 夹具:H.264 + **两条 AAC 立体声轨**(多音轨 → 恒进管线,才轮得到 copy 判定)。
    #[tokio::test]
    #[ignore]
    async fn audio_copy_segments_cover_their_window() {
        let (rt, _rx) = runtime("acopy");
        let dir = std::env::temp_dir().join(format!("lw-acopy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("双语片.mkv");
        let ok = std::process::Command::new("ffmpeg")
            .args([
                "-y", "-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i",
                "testsrc=size=320x240:rate=25:duration=30", "-f", "lavfi", "-i",
                "sine=frequency=440:sample_rate=48000:duration=30", "-map", "0:v", "-map", "1:a",
                "-map", "1:a", "-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p",
                "-g", "50", "-keyint_min", "50", "-sc_threshold", "0", "-c:a", "aac", "-ac", "2",
                "-f", "matroska",
            ])
            .arg(&src)
            .status()
            .expect("run ffmpeg")
            .success();
        assert!(ok, "生成双音轨夹具失败");

        let np = match rt.play(1, &src.to_string_lossy(), false, false, None).await.unwrap() {
            PlayOutcome::Playing(np) => np,
            other => panic!("应为 Playing,实际 {other:?}"),
        };
        let base = np.manifest_url.as_ref().expect("多音轨该进管线").trim_end_matches("desc");
        let desc: serde_json::Value =
            reqwest::get(format!("{base}desc")).await.unwrap().json().await.unwrap();
        assert_eq!(desc["copyAudio"], true, "AAC 立体声该原样搬,实际 desc={desc}");
        let seg_len = desc["audioSeg"].as_f64().unwrap();
        let duration = desc["duration"].as_f64().unwrap();

        let ainit = reqwest::get(format!("{base}ainit")).await.unwrap().bytes().await.unwrap();
        let ts = probe::init_timescales(&ainit).first().copied().expect("音频 init 该有 timescale").1;
        let n = (duration / seg_len).ceil() as usize;
        for i in 0..n {
            let seg = reqwest::get(format!("{base}a{i}")).await.unwrap().bytes().await.unwrap();
            let real = probe::fragment_duration(&seg, ts).expect("copy 的音频段该量得出时长");
            // 末段只剩零头,按剩余长度要求;其余每段必须盖满窗口(段自带左预卷,通常还更长)。
            let need = (duration - i as f64 * seg_len).min(seg_len);
            assert!(
                real + 0.05 >= need,
                "第 {i} 段只盖了 {real}s、需要 {need}s —— copy 段落在 AAC 帧边界上留了缝"
            );
        }
    }

    /// **P4 字幕端到端(真 ffmpeg)**:内嵌字幕轨 + 外挂 `.srt` 都要能变成前端挂得上的 WebVTT。
    /// 判据 = 取回来的东西确实是 WebVTT(`WEBVTT` 头 + 时间轴 + 那句台词),而不是空文件/报错页。
    #[tokio::test]
    #[ignore]
    async fn subtitles_convert_to_webvtt_from_embedded_and_sidecar() {
        let (rt, _rx) = runtime("subs");
        let dir = std::env::temp_dir().join(format!("lw-subs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // 外挂 srt(顺便验中文不乱码)
        let srt = dir.join("带字幕的片.chi.srt");
        std::fs::write(&srt, "1\n00:00:01,000 --> 00:00:03,000\n外挂字幕这一句\n\n").unwrap();
        // 内嵌一条 srt 轨的 mkv(视频 H.264、音频 AAC 立体声 → 顺带走 copy/copy)
        let emb = dir.join("emb.srt");
        std::fs::write(&emb, "1\n00:00:02,000 --> 00:00:04,000\n内嵌字幕这一句\n\n").unwrap();
        let src = dir.join("带字幕的片.mkv");
        let ok = std::process::Command::new("ffmpeg")
            .args([
                "-y", "-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i",
                "testsrc=size=320x240:rate=25:duration=10", "-f", "lavfi", "-i",
                "sine=frequency=440:sample_rate=48000:duration=10",
            ])
            .arg("-i")
            .arg(&emb)
            .args([
                "-map", "0:v", "-map", "1:a", "-map", "2:s", "-c:v", "libx264", "-preset",
                "ultrafast", "-pix_fmt", "yuv420p", "-g", "50", "-c:a", "aac", "-ac", "2", "-c:s",
                "srt", "-metadata:s:s:0", "language=chi", "-f", "matroska",
            ])
            .arg(&src)
            .status()
            .expect("run ffmpeg")
            .success();
        assert!(ok, "生成带内嵌字幕的夹具失败");

        let np = match rt.play(1, &src.to_string_lossy(), false, false, None).await.unwrap() {
            PlayOutcome::Playing(np) => np,
            other => panic!("应为 Playing,实际 {other:?}"),
        };
        assert_eq!(np.subtitles.len(), 2, "一条内嵌 + 一条外挂,实际 {:?}", np.subtitles);
        assert!(!np.subtitles[0].sidecar, "内嵌的排前面");
        assert_eq!(np.subtitles[0].lang.as_deref(), Some("chi"), "内嵌轨的语言要认出来");
        assert!(np.subtitles[1].sidecar, "外挂的标出来源");
        assert_eq!(np.subtitles[1].lang.as_deref(), Some("chi"), "文件名后缀里的语言");

        for (i, want) in [(0usize, "内嵌字幕这一句"), (1, "外挂字幕这一句")] {
            let vtt = reqwest::get(&np.subtitles[i].url).await.unwrap().text().await.unwrap();
            assert!(vtt.starts_with("WEBVTT"), "第 {i} 条不是 WebVTT: {vtt}");
            assert!(vtt.contains("-->"), "第 {i} 条没有时间轴: {vtt}");
            assert!(vtt.contains(want), "第 {i} 条内容对不上(中文可能乱码): {vtt}");
        }
    }
}
