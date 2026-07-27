//! 音频下载(media_download 的机器件):把网络页面的音轨存成本地文件。
//! 解析与播放同一条链(yt-dlp / cookies / AuthRequired 分类全复用),只换下载专用
//! 格式串(质量优先:源里有无损拿无损,见 resolver::DOWNLOAD_AUDIO_FORMAT);落盘走
//! net::Client 流式(web_download 同款 `.part` 临时件 + dedupe 永不覆盖);最后 ffmpeg
//! `-c copy` 整理成标准容器(fMP4/.m4s → .m4a、Hi-Res FLAC → .flac)并写歌名/作者
//! metadata —— **全程不转码**,拿到什么音质存什么音质。ffmpeg 缺席不阻断:原样保存
//! + 如实告知(§3.5 不静默降质也不静默失败)。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};

use crate::bus::{MediaEvent, Text};
use crate::components::Component;
use crate::files;

use super::lyrics::{self, LyricsResult};
use super::resolver::{self, ResolveError, Resolved, UpStream};
use super::{cookies, MediaRuntime};

/// 单文件闸:音频为主、放宽到 500MB(两小时高码率无损也装得下)。web_download 的
/// 50MB 是"任意网页文件"的通用口径,不适用用户点名的曲目。
pub const AUDIO_MAX_BYTES: u64 = 500 * 1024 * 1024;
/// 批量封顶(失控 backstop):一次合集最多这么多首,超了如实退回让用户挑一段。
pub const BATCH_MAX: usize = 100;

/// 下载结果(工具层据此组话)。
pub enum DownloadOutcome {
    Done(DownloadedAudio),
    /// 需要登录 ≠ 失败(§7.1 播放同口径):已弹扫码;下载没有自动重放,登录后再调一次。
    AwaitingLogin { detail: String },
    /// 批量已开工(后台 job):工具立即返回,进度在任务条。
    BatchStarted { total: usize, dir: PathBuf },
}

pub struct DownloadedAudio {
    pub path: PathBuf,
    /// 展示名(= 模型给的干净歌名,没给则视频标题)。
    pub title: String,
    pub bytes: u64,
    /// 存成的扩展名("flac" / "m4a" / "opus" / "ogg";原样保存的合并流为 "mp4")。
    pub ext: &'static str,
    /// 无损(FLAC/ALAC)—— 工具话术据此如实说音质。
    pub lossless: bool,
    /// false = ffmpeg 缺席/失败,原始流原样保存(没整理容器、没写标签)。
    pub remuxed: bool,
    /// 歌词结论(下载后自动配 .lrc;fetch 阶段占位 NotFound,由调用方回填)。
    pub lyrics: LyricsResult,
}

/// 模型给的干净「歌名 / 歌手」(信息抽取归模型:B 站标题是标题党,代码不猜)。
/// title → 文件名与标签的歌名;artist → 文件名前缀「歌手 - 」与 artist 标签。
/// 都没给 = 回落视频标题、不写 artist(UP 主是搬运号不是歌手,只进 comment 留档)。
#[derive(Debug, Clone, Default)]
pub struct TrackMeta {
    pub title: Option<String>,
    pub artist: Option<String>,
}

/// 解析的三分结论:成 / 要登录 / 真失败。批量循环里"要登录"不能当 Err 抛(要点名剩余几首)。
enum ResolvedOrAuth {
    Ok(Resolved),
    Auth(String),
}

impl MediaRuntime {
    /// 单曲下载(回合内阻塞,秒级到十秒级):解析 → 拉流落盘 → remux 整理 → 配歌词。
    pub async fn download_audio(
        &self,
        page_url: &str,
        dir: &Path,
        meta: &TrackMeta,
    ) -> Result<DownloadOutcome> {
        let ytdlp = self.ensure_component(Component::YtDlp).await?;
        let cookies_file = self.export_cookies(page_url).await?;
        let task = self.inner.tasks.start("media_download", Text::new("task.media_download"));
        task.step("step.audio_source", serde_json::Value::Null);
        let resolved =
            match resolve_for_download(&ytdlp, cookies_file.as_deref(), page_url).await {
                Ok(ResolvedOrAuth::Ok(r)) => r,
                Ok(ResolvedOrAuth::Auth(detail)) => {
                    // 需要登录 ≠ 失败:弹扫码气泡,任务正常收尾不标红(§7.1 同口径)。
                    if let Some(s) = self.source_of_url(page_url) {
                        self.publish(MediaEvent::AuthRequired { source: s.id().into() });
                    }
                    task.done();
                    return Ok(DownloadOutcome::AwaitingLogin { detail });
                }
                Err(e) => {
                    task.fail("task.err.resolve", serde_json::Value::Null);
                    return Err(e);
                }
            };
        task.step("step.audio_fetch", serde_json::json!({ "t": resolved.title }));
        // ffmpeg 缺席不阻断(原样保存);首次用时下载有自己的进度卡。
        let ffmpeg = self.ensure_component(Component::Ffmpeg).await.ok();
        let client = download_client();
        match fetch_audio_file(&client, &resolved, dir, ffmpeg.as_deref(), meta, page_url).await
        {
            Ok(mut file) => {
                // 歌词跟在下载后面配(找不到/失败绝不影响下载成败)。
                task.step("step.audio_lyrics", serde_json::Value::Null);
                file.lyrics = lyrics::lyrics_for_download(
                    &client,
                    &resolved.subtitles,
                    sub_headers(&resolved),
                    &file.title,
                    meta.artist.as_deref(),
                    resolved.duration_seconds,
                    &file.path,
                )
                .await;
                task.done();
                Ok(DownloadOutcome::Done(file))
            }
            Err(e) => {
                task.fail("task.err.download", serde_json::Value::Null);
                Err(e)
            }
        }
    }

    /// 整个合集/分P 下载(用户确认过"要全部"才走到这):发现剧集与播放同一条
    /// (`MediaSource::episodes`),不成系列退化成单曲;成系列 = 分离 job 后台逐首下,
    /// 立即返回。进度在任务条;有没下成的按 fail 收尾点名数目(§3.5 不静默)。
    /// `artist` 对整批生效(「某歌手精选」场景);每首的歌名用各集自己的标题。
    pub async fn download_all(
        &self,
        page_url: &str,
        dir: &Path,
        artist: Option<String>,
    ) -> Result<DownloadOutcome> {
        let meta = TrackMeta { title: None, artist };
        let Some(source) = self.source_of_url(page_url) else {
            return self.download_audio(page_url, dir, &meta).await;
        };
        let source_id = source.id().to_string();
        let cookie_header =
            cookies::load(&self.inner.store, &source_id).map(|c| cookies::header_value(&c));
        let discovered = match source.episodes(page_url, cookie_header.as_deref()).await {
            Ok(d) => d,
            Err(e) => {
                tracing::info!("下载:剧集发现失败,按单曲处理: {e:#}");
                None
            }
        };
        let Some((_key, entries)) = discovered.filter(|(_, e)| e.len() >= 2) else {
            return self.download_audio(page_url, dir, &meta).await;
        };
        anyhow::ensure!(
            entries.len() <= BATCH_MAX,
            "这个合集有 {} 首,一次最多下 {BATCH_MAX} 首——请让用户挑一部分(可以按第几首到第几首分批)",
            entries.len()
        );
        // 组件与登录态在答应之前备齐(答应了就要真开工;组件下载失败在这儿如实报错)。
        let ytdlp = self.ensure_component(Component::YtDlp).await?;
        let cookies_file = self.export_cookies(page_url).await?;
        let ffmpeg = self.ensure_component(Component::Ffmpeg).await.ok();

        let total = entries.len();
        let this = self.clone();
        let dir_owned = dir.to_path_buf();
        tokio::spawn(async move {
            let client = download_client();
            let task =
                this.inner.tasks.start("media_download", Text::new("task.media_download"));
            let mut failed = 0usize;
            for (i, e) in entries.iter().enumerate() {
                task.step_progress(
                    "step.audio_batch",
                    serde_json::json!({ "t": e.title, "i": i + 1, "n": total }),
                    i as f32 / total as f32,
                );
                match resolve_for_download(&ytdlp, cookies_file.as_deref(), &e.url).await {
                    Ok(ResolvedOrAuth::Ok(r)) => {
                        match fetch_audio_file(
                            &client,
                            &r,
                            &dir_owned,
                            ffmpeg.as_deref(),
                            &meta,
                            &e.url,
                        )
                        .await
                        {
                            Ok(file) => {
                                // 逐首配歌词(静默:结论进日志;配不上不算下载失败)。
                                let got = lyrics::lyrics_for_download(
                                    &client,
                                    &r.subtitles,
                                    sub_headers(&r),
                                    &file.title,
                                    meta.artist.as_deref(),
                                    r.duration_seconds,
                                    &file.path,
                                )
                                .await;
                                if got == LyricsResult::NotFound {
                                    tracing::info!(title = %file.title, "批量下载:这首没找到歌词");
                                }
                            }
                            Err(err) => {
                                failed += 1;
                                tracing::warn!(title = %e.title, "批量下载:这首没下成: {err:#}");
                            }
                        }
                    }
                    Ok(ResolvedOrAuth::Auth(detail)) => {
                        // 登录态半路失效:后面的大概率同样命运——弹扫码、剩余算没下成,别空转。
                        this.publish(MediaEvent::AuthRequired { source: source_id.clone() });
                        tracing::warn!(
                            "批量下载:需要登录,剩余 {} 首搁置: {detail}",
                            total - i
                        );
                        failed += total - i;
                        break;
                    }
                    Err(err) => {
                        failed += 1;
                        tracing::warn!(title = %e.title, "批量下载:这首没解析出来: {err:#}");
                    }
                }
            }
            if failed == 0 {
                task.done();
            } else {
                task.fail(
                    "task.err.audio_batch",
                    serde_json::json!({ "fail": failed, "total": total }),
                );
            }
        });
        Ok(DownloadOutcome::BatchStarted { total, dir: dir.to_path_buf() })
    }

    /// 登录态导出成 yt-dlp 的 cookies 文件(play_entry 同逻辑;没登录 = None,匿名照下)。
    async fn export_cookies(&self, page_url: &str) -> Result<Option<PathBuf>> {
        let source_id = self.source_of_url(page_url).map(|s| s.id().to_string());
        match source_id
            .and_then(|id| cookies::load(&self.inner.store, &id).map(|c| (id, c)))
        {
            Some((id, recs)) => {
                Ok(Some(cookies::export_file(&self.inner.dir, &id, &recs).await?))
            }
            None => Ok(None),
        }
    }
}

async fn resolve_for_download(
    ytdlp: &Path,
    cookies_file: Option<&Path>,
    page_url: &str,
) -> Result<ResolvedOrAuth> {
    match resolver::resolve_download(ytdlp, page_url, cookies_file).await {
        Ok(r) => Ok(ResolvedOrAuth::Ok(r)),
        Err(ResolveError::AuthRequired(d)) => Ok(ResolvedOrAuth::Auth(d)),
        Err(ResolveError::Other(e)) => Err(e),
    }
}

/// 下载客户端:与页面抓取(15s 总超时)分家,大文件要时间。§4.6 统一走 net::Client
/// (墙内 CDN 直连优先);防盗链 Referer/UA 由 yt-dlp 给、请求时原样带上,不在客户端层写死。
fn download_client() -> crate::net::Client {
    crate::net::Client::new(|b| {
        b.connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(280))
    })
}

/// 字幕拉取用的防盗链头(首路流的头;字幕与流同站,同一套 Referer/UA 无害够用)。
fn sub_headers(resolved: &Resolved) -> &[(String, String)] {
    resolved.streams.first().map(|s| s.headers.as_slice()).unwrap_or(&[])
}

/// 拉流落盘 + remux 的可测核心(不碰 MediaRuntime):选音频流 → `.part` 流式落盘(硬闸)
/// → ffmpeg `-c copy` 整理($ffmpeg=None$ 或失败 = 原样改名保存,不阻断)。
/// 命名 = `歌手 - 歌名.ext`(meta 给了歌手)/ `歌名.ext`;歌词由调用方随后配(lyrics 占位)。
async fn fetch_audio_file(
    net: &crate::net::Client,
    resolved: &Resolved,
    dir: &Path,
    ffmpeg: Option<&Path>,
    meta: &TrackMeta,
    page_url: &str,
) -> Result<DownloadedAudio> {
    let up = pick_audio_stream(&resolved.streams);
    std::fs::create_dir_all(dir)
        .with_context(|| format!("建不了目标文件夹 {}", dir.display()))?;

    let resp = net
        .send(&up.url, |c| {
            let mut req = c.get(&up.url);
            for (k, v) in &up.headers {
                req = req.header(k, v); // 防盗链头(Referer/UA)原样带上,relay 同款
            }
            req
        })
        .await
        .context("下载请求失败")?;
    let status = resp.status();
    anyhow::ensure!(status.is_success(), "下载失败 HTTP {status}");
    if let Some(len) = resp.content_length() {
        anyhow::ensure!(
            len <= AUDIO_MAX_BYTES,
            "音频文件 {} 超过 {} 上限,不下了",
            crate::files::human_size(len),
            crate::files::human_size(AUDIO_MAX_BYTES)
        );
    }

    // 先写临时件再改名:半截下载绝不顶着正式名躺在文件夹里(web_download 同款)。
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let part = dir.join(format!(
        ".lw-audio-{}-{}.part",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let bytes = match stream_to_part(resp, &part, AUDIO_MAX_BYTES).await {
        Ok(n) => n,
        Err(e) => {
            let _ = std::fs::remove_file(&part);
            return Err(e);
        }
    };

    let (ext, lossless) = audio_ext(up);
    // 展示名/文件名:模型给的干净歌名优先(空串当没给);歌手给了就用「歌手 - 歌名」
    // 音乐库通用模板。都没给 = 视频标题原样(信息抽取归模型,代码不猜标题党)。
    let display_title = meta
        .title
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(&resolved.title)
        .to_string();
    let base = match meta.artist.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(artist) => format!("{artist} - {display_title}"),
        None => display_title.clone(),
    };
    let name = files::sanitize_filename(&base);
    // 来源留档进 comment(UP 主是搬运号不是歌手,不进 artist 标签)。
    let comment = match &resolved.uploader {
        Some(u) => format!("来源: {page_url}(UP主: {u})"),
        None => format!("来源: {page_url}"),
    };
    if let Some(ff) = ffmpeg {
        let dest = files::dedupe_path(&dir.join(format!("{name}.{ext}")));
        match remux_audio(
            ff,
            &part,
            &dest,
            ext,
            &display_title,
            meta.artist.as_deref(),
            &comment,
        )
        .await
        {
            Ok(out_bytes) => {
                let _ = std::fs::remove_file(&part);
                return Ok(DownloadedAudio {
                    path: dest,
                    title: display_title,
                    bytes: out_bytes,
                    ext,
                    lossless,
                    remuxed: true,
                    lyrics: LyricsResult::NotFound, // 占位:歌词由调用方随后配并回填
                });
            }
            Err(e) => {
                // remux 只是整理,失败不吞下载成果:原样保存 + 如实告知(remuxed=false)。
                tracing::warn!(title = %display_title, "音频整理失败,原样保存: {e:#}");
            }
        }
    }
    // 原样保存:字节就是源流(fMP4)。合并单文件(带视频轨)按 .mp4,纯音频按 .m4a
    // (B 站音频流即 fMP4,MP4 家族扩展名如实;FLAC-in-fMP4 顶 .m4a 也比假 .flac 诚实)。
    let raw_ext: &'static str = if up.vcodec.is_some() { "mp4" } else { "m4a" };
    let dest = files::dedupe_path(&dir.join(format!("{name}.{raw_ext}")));
    std::fs::rename(&part, &dest).or_else(|_| {
        // 跨卷 rename 失败转拷贝(dir 可能在别的盘;§7.2 同款)
        std::fs::copy(&part, &dest).map(|_| ()).and_then(|()| std::fs::remove_file(&part))
    })?;
    Ok(DownloadedAudio {
        path: dest,
        title: display_title,
        bytes,
        ext: raw_ext,
        lossless,
        remuxed: false,
        lyrics: LyricsResult::NotFound, // 占位:歌词由调用方随后配并回填
    })
}

/// 两路(音视频分离)选音频那路;单路(纯音频或合并单文件)就它。下载格式串不带
/// `+` 合并,常态单路 —— 这里宽容兜住没见过的形状。
fn pick_audio_stream(streams: &[UpStream]) -> &UpStream {
    streams.iter().find(|s| s.acodec.is_some() && s.vcodec.is_none()).unwrap_or(&streams[0])
}

/// 目标扩展名与"无损"判定:按选中流的音频编码(RFC6381)定容器。
/// flac → .flac(无损);alac → .m4a(无损,ALAC 的家就是 MP4);opus → .opus;
/// vorbis → .ogg;其余(mp4a/aac/未知)→ .m4a。
fn audio_ext(up: &UpStream) -> (&'static str, bool) {
    let a = up.acodec.as_deref().unwrap_or("").to_ascii_lowercase();
    if a.contains("flac") {
        ("flac", true)
    } else if a.contains("alac") {
        ("m4a", true)
    } else if a.contains("opus") {
        ("opus", false)
    } else if a.contains("vorbis") {
        ("ogg", false)
    } else {
        ("m4a", false)
    }
}

/// 流式落盘(硬闸按实际字节数,不信服务器自报)。
async fn stream_to_part(
    mut resp: reqwest::Response,
    dest: &Path,
    cap: u64,
) -> Result<u64> {
    use std::io::Write;
    let mut f = std::fs::File::create(dest)
        .with_context(|| format!("建不了文件 {}", dest.display()))?;
    let mut total: u64 = 0;
    while let Some(chunk) = resp.chunk().await.context("下载中断")? {
        total += chunk.len() as u64;
        anyhow::ensure!(
            total <= cap,
            "音频文件超过 {} 上限,已停止",
            crate::files::human_size(cap)
        );
        f.write_all(&chunk)?;
    }
    f.flush()?;
    Ok(total)
}

/// ffmpeg `-c copy` 整理:抽音轨(`-vn` 兜合并流)、换标准容器、写歌名/歌手/来源标签。
/// 不转码 —— 纯 I/O,秒级。输出先落同目录临时名(ffmpeg 按扩展名认格式)再改名。
async fn remux_audio(
    ffmpeg: &Path,
    src: &Path,
    dest: &Path,
    ext: &str,
    title: &str,
    artist: Option<&str>,
    comment: &str,
) -> Result<u64> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let tmp = dest.with_file_name(format!(
        ".lw-remux-{}-{}.{ext}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let mut cmd = tokio::process::Command::new(ffmpeg);
    cmd.arg("-hide_banner").arg("-y").arg("-i").arg(src).arg("-vn").arg("-c:a").arg("copy");
    if ext == "m4a" {
        cmd.arg("-movflags").arg("+faststart"); // moov 提前,顺序读的播放器/网盘预览友好
    }
    cmd.arg("-metadata").arg(format!("title={title}"));
    if let Some(a) = artist {
        cmd.arg("-metadata").arg(format!("artist={a}"));
    }
    cmd.arg("-metadata").arg(format!("comment={comment}"));
    cmd.arg(&tmp);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    super::no_console(&mut cmd);
    let out = tokio::time::timeout(std::time::Duration::from_secs(120), cmd.output())
        .await
        .context("音频整理超时")?
        .context("ffmpeg 起不来")?;
    if !out.status.success() {
        let _ = std::fs::remove_file(&tmp);
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("ffmpeg 整理失败: {}", stderr.trim().chars().rev().take(200).collect::<Vec<_>>().into_iter().rev().collect::<String>());
    }
    let bytes = std::fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0);
    std::fs::rename(&tmp, dest).or_else(|_| {
        std::fs::copy(&tmp, dest).map(|_| ()).and_then(|()| std::fs::remove_file(&tmp))
    })?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn up(acodec: Option<&str>, vcodec: Option<&str>) -> UpStream {
        UpStream {
            url: "http://example.invalid/a".into(),
            acodec: acodec.map(str::to_string),
            vcodec: vcodec.map(str::to_string),
            ..UpStream::default()
        }
    }

    #[test]
    fn pick_prefers_pure_audio_stream() {
        let streams = vec![up(None, Some("avc1.64")), up(Some("mp4a.40.2"), None)];
        assert!(pick_audio_stream(&streams).acodec.is_some(), "两路选纯音频那路");
        let combined = vec![up(Some("mp4a.40.2"), Some("avc1.64"))];
        assert!(pick_audio_stream(&combined).vcodec.is_some(), "单路合并流就它");
    }

    #[test]
    fn ext_follows_source_codec() {
        assert_eq!(audio_ext(&up(Some("flac"), None)), ("flac", true), "无损存 .flac");
        assert_eq!(audio_ext(&up(Some("alac"), None)), ("m4a", true), "ALAC 的家是 MP4");
        assert_eq!(audio_ext(&up(Some("mp4a.40.2"), None)), ("m4a", false));
        assert_eq!(audio_ext(&up(Some("opus"), None)), ("opus", false));
        assert_eq!(audio_ext(&up(None, None)), ("m4a", false), "未知按 m4a");
    }

    /// 端到端(无 ffmpeg 路):防盗链头带上才 200 → `.part` 落盘 → 原样改名保存,
    /// 同名去重永不覆盖,不留临时件。
    #[tokio::test]
    async fn fetch_carries_antileech_headers_and_never_overwrites() {
        use axum::{http::HeaderMap, http::StatusCode, routing::get, Router};
        async fn track(headers: HeaderMap) -> (StatusCode, &'static [u8]) {
            match headers.get("referer").and_then(|v| v.to_str().ok()) {
                Some(r) if r.contains("example-source") => (StatusCode::OK, b"fake-audio-bytes"),
                _ => (StatusCode::FORBIDDEN, b""),
            }
        }
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, Router::new().route("/a", get(track))).await.ok();
        });

        let resolved = Resolved {
            title: "《测试曲目》某某录音棚大声听".into(),
            uploader: Some("某搬运号".into()),
            duration_seconds: Some(3.0),
            streams: vec![UpStream {
                url: format!("http://127.0.0.1:{port}/a"),
                headers: vec![("Referer".into(), "https://example-source.test/".into())],
                acodec: Some("mp4a.40.2".into()),
                ..UpStream::default()
            }],
            subtitles: Vec::new(),
        };
        let dir = std::env::temp_dir().join(format!("lw-audiodl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let net = download_client();
        let page = "https://example-source.test/v/1";

        // 模型给了干净歌名/歌手 → 「歌手 - 歌名.ext」;展示名跟着干净歌名走。
        let meta = TrackMeta {
            title: Some("测试曲目".into()),
            artist: Some("某演唱者".into()),
        };
        let f1 = fetch_audio_file(&net, &resolved, &dir, None, &meta, page).await.unwrap();
        assert_eq!(f1.path.file_name().unwrap().to_str().unwrap(), "某演唱者 - 测试曲目.m4a");
        assert_eq!(f1.title, "测试曲目");
        assert!(!f1.remuxed && !f1.lossless);
        assert_eq!(std::fs::read(&f1.path).unwrap(), b"fake-audio-bytes");

        let f2 = fetch_audio_file(&net, &resolved, &dir, None, &meta, page).await.unwrap();
        assert_eq!(
            f2.path.file_name().unwrap().to_str().unwrap(),
            "某演唱者 - 测试曲目 (2).m4a",
            "同名去重永不覆盖"
        );

        // 没给 meta → 回落视频标题原样(代码不猜标题党)。
        let f3 = fetch_audio_file(&net, &resolved, &dir, None, &TrackMeta::default(), page)
            .await
            .unwrap();
        assert_eq!(
            f3.path.file_name().unwrap().to_str().unwrap(),
            "《测试曲目》某某录音棚大声听.m4a"
        );
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".part"))
            .collect();
        assert!(leftovers.is_empty(), "不留 .part 残件");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 硬闸按实际字节数(不信 Content-Length):超了立停报错。
    #[tokio::test]
    async fn stream_cap_stops_oversize_body() {
        use axum::{routing::get, Router};
        async fn big() -> &'static [u8] {
            b"0123456789abcdef" // 16 字节
        }
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, Router::new().route("/big", get(big))).await.ok();
        });
        let resp = reqwest::Client::new()
            .get(format!("http://127.0.0.1:{port}/big"))
            .send()
            .await
            .unwrap();
        let dest = std::env::temp_dir().join(format!("lw-cap-{}.part", std::process::id()));
        let err = stream_to_part(resp, &dest, 8).await.unwrap_err();
        assert!(err.to_string().contains("上限"), "{err:#}");
        let _ = std::fs::remove_file(&dest);
    }
}
