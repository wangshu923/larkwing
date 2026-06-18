//! 影音运行时(PLAN §9):搜索(各源 API)→ 解析(yt-dlp)→ 转发/混流(relay)→
//! 事件推 UI。多源立场与 LLM 多供应商同构(宪法 §4):解析层 yt-dlp 天然多源,
//! 真正按源分化的只有**搜索**和**登录态**,接缝(`MediaSource` trait)就开在这;
//! 加源 = 加一个实现文件,工具面与模型无感知。MVP 只有 bilibili。

mod bilibili;
pub mod cookies;
mod relay;
mod resolver;

pub use cookies::CookieRec;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Serialize;

use crate::bus::{AppEvent, Bus, MediaEvent, TaskRetry, Text};
use crate::components::{Component, Components, DEFAULT_GH_MIRRORS};
use crate::store::Store;
use crate::tasks::Tasks;

/// Windows 下给子进程加 CREATE_NO_WINDOW:主进程是 GUI 子系统(windows_subsystem="windows"),
/// 但它 spawn 的控制台程序(yt-dlp / ffmpeg)默认仍会弹一个黑框 —— 这里抑制掉。其它平台空操作。
/// 出站只有 resolver(yt-dlp)和 relay(ffmpeg)两处 spawn,都必须走这里。
fn no_console(cmd: &mut tokio::process::Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        // 注:CommandExt 作用于 std::process::Command,不是 tokio 的 ——
        // 经 as_std_mut() 拿底层 std command 设标志,spawn 时会被沿用。
        cmd.as_std_mut().creation_flags(CREATE_NO_WINDOW);
    }
    // macOS / Linux:GUI 进程 spawn 子进程不会凭空弹终端窗口,这里无事可做。
    // 钩子留着 —— 将来若要做别的平台级子进程加固(进程组 / niceness / 句柄继承收口),
    // 统一开在这个函数里,两处 spawn(resolver、relay)自动受益。
    #[cfg(not(windows))]
    let _ = cmd;
}

// ---------- 源接缝 ----------

/// 搜索命中(模型与播放卡片共用的形)。
#[derive(Debug, Clone, Serialize)]
pub struct MediaHit {
    pub url: String,
    pub title: String,
    pub author: String,
    pub duration_seconds: i64,
    pub source: String,
}

#[derive(Debug)]
pub enum SearchError {
    /// 412/403/-101 类风控:登录态能显著缓解 → UI 出扫码入口。
    RiskControl,
    Other(anyhow::Error),
}

/// 按源分化的两件事:搜索 + 登录态元数据。解析不在此 —— yt-dlp 统一吃页面 URL。
#[async_trait]
pub trait MediaSource: Send + Sync {
    fn id(&self) -> &'static str;
    /// 扫码登录页(壳层开窗口用)。
    fn login_url(&self) -> &'static str;
    /// 取 cookie 的域 URL(原生 CookieManager 按它查)。
    fn cookie_url(&self) -> &'static str;
    /// 判定"已登录"的关键 cookie 名。
    fn login_cookie(&self) -> &'static str;
    async fn search(
        &self,
        keyword: &str,
        limit: usize,
        cookie_header: Option<&str>,
    ) -> Result<Vec<MediaHit>, SearchError>;
}

// ---------- 播放词汇(过桥给前端) ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Audio,
    Video,
}

/// 「正在播放」:前端拿 stream_url 挂播放元素;page_url 留诊断/以后“浏览器打开”。
#[derive(Debug, Clone, Serialize)]
pub struct NowPlaying {
    pub kind: MediaKind,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_seconds: Option<f64>,
    pub stream_url: String,
    pub page_url: String,
    pub source: String,
}

/// 登录窗口的三件套(壳层 media_login command 消费)。
#[derive(Debug, Clone, Serialize)]
pub struct LoginSpec {
    pub source: String,
    pub login_url: String,
    pub cookie_url: String,
    pub login_cookie: String,
}

/// `play()` 的结果:要么已开播,要么卡在「需要登录」。后者**不是失败**——已记下待重放,
/// 用户扫码登录成功(`set_cookies`)那一刻会带着新 cookie 自动续上,不再 `bail` 喂模型「放失败了」。
#[derive(Debug)]
pub enum PlayOutcome {
    /// 解析成功、已发 Play 事件,前端起播。
    Playing(NowPlaying),
    /// 需要登录:已发 AuthRequired(UI 出扫码气泡)+ 记下待重放。detail = 解析器给的原因。
    AwaitingLogin { detail: String },
}

/// 待登录重放:登录成功时把当初这次播放原样再跑一遍(带上新 cookie)。
/// 超过 TTL 视为过期(用户早不想看了)→ 丢弃不重放,免「登录后凭空冒出个老视频」。
#[derive(Debug, Clone)]
struct PendingPlay {
    page_url: String,
    audio_only: bool,
    at: Instant,
}

/// 待重放有效期:超过即作废。
const PENDING_PLAY_TTL: Duration = Duration::from_secs(600);

// ---------- 运行时 ----------

struct Inner {
    dir: PathBuf,
    store: Store,
    bus: Bus,
    tasks: Tasks,
    components: Components,
    relay: tokio::sync::OnceCell<relay::Relay>,
    sources: Vec<Arc<dyn MediaSource>>,
    login_hint_sent: AtomicBool,
    /// 因「需登录」卡住、待登录后自动重放的播放(按源 id)。
    pending_play: Mutex<HashMap<String, PendingPlay>>,
}

#[derive(Clone)]
pub struct MediaRuntime {
    inner: Arc<Inner>,
}

impl MediaRuntime {
    pub fn new(dir: PathBuf, store: Store, bus: Bus) -> MediaRuntime {
        let tasks = Tasks::new(bus.clone());
        let components = Components::new(dir.join("components"), tasks.clone());
        MediaRuntime {
            inner: Arc::new(Inner {
                dir,
                store,
                bus,
                tasks,
                components,
                relay: tokio::sync::OnceCell::new(),
                sources: vec![Arc::new(bilibili::Bilibili::new())],
                login_hint_sent: AtomicBool::new(false),
                pending_play: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// 测试/无壳跑法:事件无人听、组件落系统临时目录。功能完整,只是安静。
    pub fn detached(store: Store) -> MediaRuntime {
        MediaRuntime::new(std::env::temp_dir().join("larkwing-media"), store, Bus::new())
    }

    pub fn bus(&self) -> &Bus {
        &self.inner.bus
    }

    fn publish(&self, ev: MediaEvent) {
        self.inner.bus.publish(AppEvent::Media(ev));
    }

    /// 镜像列表 = 数据(settings 可覆盖,坏 JSON 回默认)。
    fn mirrors(&self) -> Vec<String> {
        self.inner
            .store
            .settings
            .get(None, "media.gh_mirrors")
            .ok()
            .flatten()
            .and_then(|json| serde_json::from_str::<Vec<String>>(&json).ok())
            .unwrap_or_else(|| DEFAULT_GH_MIRRORS.iter().map(|s| s.to_string()).collect())
    }

    /// 把一个本地文件注册成 localhost 播放地址(/f/ 通道,带 Range)。
    /// TTS 音频(PLAN §11)与本地媒体共用这条路;voice 不依赖 media,壳层缝合。
    pub async fn file_url(&self, path: PathBuf) -> Result<String> {
        let relay = self
            .inner
            .relay
            .get_or_try_init(relay::Relay::start)
            .await
            .context("转发服务起不来")?;
        Ok(relay.register_file(path))
    }

    fn default_source(&self) -> &Arc<dyn MediaSource> {
        &self.inner.sources[0]
    }

    fn source_of_url(&self, url: &str) -> Option<&Arc<dyn MediaSource>> {
        // 接缝够用即可:按源 id 出现在域名里判断(bilibili.com / b23.tv 短链交给 yt-dlp)
        self.inner.sources.iter().find(|s| url.contains(s.id()))
    }

    pub fn login_spec(&self, source_id: &str) -> Option<LoginSpec> {
        let s = self.inner.sources.iter().find(|s| s.id() == source_id)?;
        Some(LoginSpec {
            source: s.id().into(),
            login_url: s.login_url().into(),
            cookie_url: s.cookie_url().into(),
            login_cookie: s.login_cookie().into(),
        })
    }

    /// 登录窗口收割的 cookie 入库 + 导出 + 广播(UI 撤登录提示,下次解析自动带上)。
    /// 若此前有播放因「需登录」卡住 → 带着新 cookie 自动重放(不绕模型,同嘴控哲学 §7.1)。
    pub fn set_cookies(&self, source_id: &str, recs: Vec<CookieRec>) -> Result<()> {
        cookies::save(&self.inner.store, source_id, &recs)?;
        self.publish(MediaEvent::LoggedIn { source: source_id.into() });
        tracing::info!(source = source_id, n = recs.len(), "登录态已入库");
        // 需 tokio 运行时(生产里 set_cookies 来自异步轮询);无运行时的同步调用方保留待重放、不丢。
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            if let Some(p) = self.take_pending_play(source_id) {
                tracing::info!(source = source_id, url = %p.page_url, "登录成功,自动重放待播内容");
                let this = self.clone();
                handle.spawn(async move {
                    if let Err(e) = this.play(&p.page_url, p.audio_only).await {
                        tracing::warn!("登录后自动重放失败: {e:#}");
                    }
                });
            }
        }
        Ok(())
    }

    /// 记下一次「因需登录而卡住」的播放,待登录成功后自动重放。
    fn record_pending(&self, source: &str, page_url: &str, audio_only: bool) {
        self.inner.pending_play.lock().unwrap().insert(
            source.to_string(),
            PendingPlay { page_url: page_url.to_string(), audio_only, at: Instant::now() },
        );
    }

    /// 取走某源的待重放(取即消费,不重复);超过 TTL 的丢弃、返回 None。
    fn take_pending_play(&self, source: &str) -> Option<PendingPlay> {
        let p = self.inner.pending_play.lock().unwrap().remove(source)?;
        (p.at.elapsed() <= PENDING_PLAY_TTL).then_some(p)
    }

    /// 组件就绪(分离 spawn:回合被取消/超时,下载在 HUD 里继续走完,下次直接命中)。
    async fn ensure_component(&self, c: Component) -> Result<PathBuf> {
        let this = self.clone();
        let mirrors = self.mirrors();
        tokio::spawn(async move { this.inner.components.ensure(c, &mirrors).await })
            .await
            .context("组件下载任务挂了")?
    }

    /// 搜索(默认源)。风控错误在此转事件,文字留给工具层喂模型。
    pub async fn search(&self, keyword: &str, limit: usize) -> Result<Vec<MediaHit>, SearchError> {
        let source = self.default_source();
        let cookie = cookies::load(&self.inner.store, source.id()).map(|c| cookies::header_value(&c));
        let result = source.search(keyword, limit, cookie.as_deref()).await;
        if matches!(result, Err(SearchError::RiskControl)) {
            self.publish(MediaEvent::AuthRequired { source: source.id().into() });
        }
        result
    }

    /// 播放:本地路径直走文件端点(免解析、即时);网络页面走 yt-dlp 解析 → 注册转发。
    /// 错误向上抛(工具层转成喂模型的观察)。
    pub async fn play(&self, page_url: &str, audio_only: bool) -> Result<PlayOutcome> {
        if is_local_path(page_url) {
            return self.play_local(page_url, audio_only).await.map(PlayOutcome::Playing);
        }
        let ytdlp = self.ensure_component(Component::YtDlp).await?;

        let source_id = self.source_of_url(page_url).map(|s| s.id().to_string());
        let cookies_file = match source_id
            .as_deref()
            .and_then(|id| cookies::load(&self.inner.store, id).map(|c| (id.to_string(), c)))
        {
            Some((id, recs)) => {
                Some(cookies::export_file(&self.inner.dir, &id, &recs).await?)
            }
            None => None,
        };

        let task = self.inner.tasks.start("resolve", Text::new("task.resolve"));
        task.step("step.resolve", serde_json::Value::Null);
        let resolved =
            match resolver::resolve(&ytdlp, page_url, cookies_file.as_deref(), audio_only).await {
                Ok(r) => r,
                Err(resolver::ResolveError::AuthRequired(detail)) => {
                    // 需要登录 ≠ 失败:记下待重放 + 弹扫码气泡,登录成功后自动续上(见 set_cookies)。
                    if let Some(id) = &source_id {
                        self.record_pending(id, page_url, audio_only);
                        self.publish(MediaEvent::AuthRequired { source: id.clone() });
                        // 解析已得结论(需登录),不是失败:正常收尾,不标红 HUD、不喂模型「放失败了」。
                        task.done();
                        return Ok(PlayOutcome::AwaitingLogin { detail });
                    }
                    // 未知来源没有登录通道,只能如实退回(MVP 仅 bilibili,此分支基本走不到)。
                    task.fail("task.err.auth", serde_json::Value::Null);
                    anyhow::bail!("这个内容需要登录才能播放({detail})");
                }
                Err(e) => {
                    // 解析失败多为网络/瞬时:给重放口(UI 显「重试」,点击直连重播同一 url)
                    task.fail_retryable(
                        "task.err.resolve",
                        serde_json::Value::Null,
                        TaskRetry::MediaPlay { page_url: page_url.to_string(), audio_only },
                    );
                    anyhow::bail!("解析失败: {e}");
                }
            };

        let relay = self
            .inner
            .relay
            .get_or_try_init(relay::Relay::start)
            .await
            .context("转发服务起不来")?;

        let mut streams = resolved.streams.clone();
        let stream_url = if streams.len() == 2 {
            // 音视频分离(B 站 DASH 常态):要 ffmpeg 混流
            let ffmpeg = match self.ensure_component(Component::Ffmpeg).await {
                Ok(p) => p,
                Err(e) => {
                    // 组件(ffmpeg)下载失败同属可重试:同样给重放口
                    task.fail_retryable(
                        "task.err.download",
                        serde_json::Value::Null,
                        TaskRetry::MediaPlay { page_url: page_url.to_string(), audio_only },
                    );
                    return Err(e);
                }
            };
            let audio = streams.pop().expect("len==2");
            let video = streams.pop().expect("len==2");
            relay.register_remux(video, audio, ffmpeg)
        } else {
            relay.register_direct(streams.pop().expect("resolver 保证非空"))
        };
        task.done();

        let np = NowPlaying {
            kind: if audio_only { MediaKind::Audio } else { MediaKind::Video },
            title: resolved.title,
            author: resolved.uploader,
            duration_seconds: resolved.duration_seconds,
            stream_url,
            page_url: page_url.into(),
            source: source_id.clone().unwrap_or_else(|| "web".into()),
        };
        self.publish(MediaEvent::Play(np.clone()));

        // 建议气泡素材:还没登录 → 每次启动至多提示一次"登录画质更清晰"
        if let Some(id) = source_id {
            if cookies_file.is_none() && !self.inner.login_hint_sent.swap(true, Ordering::Relaxed)
            {
                self.publish(MediaEvent::LoginHint { source: id });
            }
        }
        Ok(PlayOutcome::Playing(np))
    }

    /// 本地文件(含 NAS 挂载/UNC):跳过 yt-dlp,注册文件端点即播 —— 单文件免混流,
    /// Range 原生 seek 白送,秒级无任务进度可言,不上 HUD。
    async fn play_local(&self, path_str: &str, audio_only: bool) -> Result<NowPlaying> {
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
        let np = NowPlaying {
            kind: if audio_only { MediaKind::Audio } else { MediaKind::Video },
            title,
            author: None,
            duration_seconds: None,
            stream_url: relay.register_file(path),
            page_url: path_str.into(),
            source: "local".into(),
        };
        self.publish(MediaEvent::Play(np.clone()));
        Ok(np)
    }

    /// 模型侧播放控制(用户用嘴说"暂停/大点声/倍速/跳到 90 秒");按钮不走这,直连前端 VM。
    /// speed/seek 带 value,其余不带;词表和校验收口在这,前端只执行不判断。
    pub fn control(&self, action: &str, value: Option<f64>) -> Result<()> {
        match action {
            "pause" | "resume" | "stop" | "louder" | "softer" => {}
            "speed" => {
                let v = value.context("speed 需要 value(倍速)")?;
                anyhow::ensure!((0.25..=3.0).contains(&v), "倍速范围 0.25–3,收到 {v}");
            }
            "seek" => {
                let v = value.context("seek 需要 value(秒)")?;
                anyhow::ensure!(v >= 0.0, "定位秒数不能为负");
            }
            other => anyhow::bail!(
                "未知动作 {other},可用: pause/resume/stop/louder/softer/speed/seek"
            ),
        }
        self.publish(MediaEvent::Control { action: action.into(), value });
        Ok(())
    }
}

/// 本地路径判定:unix 绝对路径 / Windows 盘符(C:\ 或 C:/)/ UNC(\\nas\share)。
/// 排除 protocol-relative 的 `//host`(那是网络);相对路径一律拒(工具层报错引导)。
pub fn is_local_path(s: &str) -> bool {
    let b = s.as_bytes();
    (s.starts_with('/') && !s.starts_with("//"))
        || s.starts_with("\\\\")
        || (b.len() >= 3
            && b[0].is_ascii_alphabetic()
            && b[1] == b':'
            && (b[2] == b'\\' || b[2] == b'/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime(tag: &str) -> (MediaRuntime, tokio::sync::broadcast::Receiver<AppEvent>) {
        let dir = std::env::temp_dir().join(format!("lw-media-test-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::open(&dir.join("t.db")).unwrap();
        let bus = Bus::new();
        let rx = bus.subscribe();
        (MediaRuntime::new(dir, store, bus), rx)
    }

    #[tokio::test]
    async fn control_validates_action_and_publishes() {
        let (rt, mut rx) = runtime("ctl");
        rt.control("pause", None).unwrap();
        rt.control("speed", Some(1.5)).unwrap();
        rt.control("seek", Some(90.0)).unwrap();
        assert!(rt.control("blast_off", None).is_err(), "未知动作被拒");
        assert!(rt.control("speed", None).is_err(), "speed 缺 value 被拒");
        assert!(rt.control("speed", Some(9.0)).is_err(), "倍速超界被拒");
        assert!(rt.control("seek", Some(-3.0)).is_err(), "负秒数被拒");
        match rx.try_recv().unwrap() {
            AppEvent::Media(MediaEvent::Control { action, value }) => {
                assert_eq!(action, "pause");
                assert!(value.is_none());
            }
            other => panic!("应是 Control,实际 {other:?}"),
        }
    }

    #[test]
    fn local_path_detection() {
        assert!(is_local_path("/Users/me/Movies/a.mp4"));
        assert!(is_local_path("C:\\Movies\\a.mp4"));
        assert!(is_local_path("d:/film/b.mkv"));
        assert!(is_local_path("\\\\nas\\film\\c.mp4"), "UNC 路径");
        assert!(!is_local_path("https://www.bilibili.com/video/BV1"));
        assert!(!is_local_path("//cdn.example/x"), "protocol-relative 是网络");
        assert!(!is_local_path("movies/a.mp4"), "相对路径拒收");
    }

    #[tokio::test]
    async fn play_local_serves_file_through_relay() {
        let (rt, mut rx) = runtime("local");
        let dir = std::env::temp_dir().join(format!("lw-media-local-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("儿歌串烧.mp3");
        std::fs::write(&f, b"FAKE-MP3-BYTES").unwrap();

        let np = match rt.play(&f.to_string_lossy(), true).await.unwrap() {
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
        assert!(matches!(rx.try_recv().unwrap(), AppEvent::Media(MediaEvent::Play(_))));

        // 不存在的文件 = 错误观察
        assert!(rt.play("/no/such/file.mp4", false).await.is_err());
    }

    #[test]
    fn login_spec_known_and_unknown_sources() {
        let (rt, _rx) = runtime("login");
        let spec = rt.login_spec("bilibili").unwrap();
        assert!(spec.login_url.contains("passport.bilibili.com"));
        assert_eq!(spec.login_cookie, "SESSDATA");
        assert!(rt.login_spec("nope").is_none());
    }

    #[test]
    fn cookies_roundtrip_and_logged_in_event() {
        let (rt, mut rx) = runtime("ck");
        rt.set_cookies(
            "bilibili",
            vec![CookieRec {
                name: "SESSDATA".into(),
                value: "v".into(),
                domain: ".bilibili.com".into(),
                path: "/".into(),
            }],
        )
        .unwrap();
        let loaded = cookies::load(&rt.inner.store, "bilibili").unwrap();
        assert_eq!(loaded[0].name, "SESSDATA");
        assert!(matches!(
            rx.try_recv().unwrap(),
            AppEvent::Media(MediaEvent::LoggedIn { .. })
        ));
    }

    #[test]
    fn mirrors_default_then_overridable() {
        let (rt, _rx) = runtime("mir");
        assert_eq!(rt.mirrors().len(), DEFAULT_GH_MIRRORS.len());
        rt.inner.store.settings.set(None, "media.gh_mirrors", r#"["https://my.mirror/"]"#).unwrap();
        assert_eq!(rt.mirrors(), vec!["https://my.mirror/".to_string()]);
        rt.inner.store.settings.set(None, "media.gh_mirrors", "not json").unwrap();
        assert_eq!(rt.mirrors().len(), DEFAULT_GH_MIRRORS.len(), "坏 JSON 回默认");
    }

    #[test]
    fn source_of_url_matches_by_id() {
        let (rt, _rx) = runtime("src");
        assert!(rt.source_of_url("https://www.bilibili.com/video/BV1").is_some());
        assert!(rt.source_of_url("https://example.com/v").is_none());
    }

    #[test]
    fn pending_play_record_take_roundtrip() {
        let (rt, _rx) = runtime("pending");
        assert!(rt.take_pending_play("bilibili").is_none(), "初始无待重放");
        rt.record_pending("bilibili", "https://www.bilibili.com/video/BV1", true);
        let p = rt.take_pending_play("bilibili").expect("记下后应能取到");
        assert_eq!(p.page_url, "https://www.bilibili.com/video/BV1");
        assert!(p.audio_only);
        assert!(rt.take_pending_play("bilibili").is_none(), "取走即消费,不重复重放");
    }

    #[test]
    fn pending_play_expires_after_ttl() {
        let (rt, _rx) = runtime("pending-exp");
        // 直接塞一个「过期」条目(at 早于 TTL);checked_sub 在极早期 Instant 上可能为 None,跳过即可
        if let Some(stale) = Instant::now().checked_sub(PENDING_PLAY_TTL + Duration::from_secs(1)) {
            rt.inner.pending_play.lock().unwrap().insert(
                "bilibili".into(),
                PendingPlay {
                    page_url: "https://www.bilibili.com/video/BVold".into(),
                    audio_only: false,
                    at: stale,
                },
            );
            assert!(rt.take_pending_play("bilibili").is_none(), "过期的待重放不返回");
        }
    }
}
