//! 影音运行时(PLAN §9):搜索(各源 API)→ 解析(yt-dlp)→ 转发/混流(relay)→
//! 事件推 UI。多源立场与 LLM 多供应商同构(宪法 §4):解析层 yt-dlp 天然多源,
//! 真正按源分化的只有**搜索**和**登录态**,接缝(`MediaSource` trait)就开在这;
//! 加源 = 加一个实现文件,工具面与模型无感知。MVP 只有 bilibili。

mod bilibili;
pub mod cookies;
mod probe;
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

    /// 发现「剧集队列」:给一个页面 URL,返回 `(series_key, 有序集列表)`;单个视频(非合集/分P)→ None。
    /// 按源分化(B 站走 view API 拿 分P/合集;别的源各自实现)。默认无 —— 未实现的源退化成单集。
    async fn episodes(
        &self,
        _page_url: &str,
        _cookie_header: Option<&str>,
    ) -> Result<Option<(String, Vec<EpisodeRef>)>> {
        Ok(None)
    }
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
    /// 有值 = 自适应流(DASH/HLS):前端用 shaka(MSE)播它,播放器自己管时间轴 → 原生 seek/同步。
    /// 否则前端用 `stream_url` 挂原生 `<video>/<audio>`(直传文件/单流,原生 seek)。(B 站 DASH 走这里。)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest_url: Option<String>,
    pub page_url: String,
    pub source: String,
    /// 多集续播位置:有值 = 这是一个 ≥2 集的剧集(B 站合集/分P、本地剧集文件夹)。
    /// 前端据 index/total 显示「第N/共M集」+ 上/下一集按钮;`ended` 时若非末集自动续播。
    /// None = 单个内容(电影/单曲),不出现集数 UI。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub playlist: Option<PlaylistPos>,
}

/// 「正在播放」里的队列位置(过桥给前端 + 给工具叙述)。
#[derive(Debug, Clone, Copy, Serialize)]
pub struct PlaylistPos {
    /// 当前集下标(0 起)。
    pub index: usize,
    /// 总集数(>1 才会带 PlaylistPos)。
    pub total: usize,
    /// 本次是否「接着上次续播」跳转而来(true → 工具叙述"接着上次第N集")。前端忽略。
    pub resumed: bool,
}

/// 队列里的一集:来源无关(B 站 / 本地共用同一队列机器,§多集续播)。
#[derive(Debug, Clone, Serialize)]
pub struct EpisodeRef {
    /// 集身份(续播记忆存的就是它):B 站 bvid / `p3`;本地**相对文件名**。
    /// 稳定、可跨会话比对、绝不含绝对路径(§6.2)。
    pub id: String,
    /// 可播地址:B 站 page_url;本地绝对路径。`advance` 直接喂回 `play_entry`。
    pub url: String,
    /// 显示标题(分P 的 part 名 / 合集集名 / 本地文件名)。
    pub title: String,
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
    user_id: i64,
    page_url: String,
    audio_only: bool,
    at: Instant,
}

/// 待重放有效期:超过即作废。
const PENDING_PLAY_TTL: Duration = Duration::from_secs(600);

/// 前端播放器的「此刻」状态快照。播放真相在前端 WebView(播放在那跑、放完只有它知道);
/// core 起播时乐观 seed,前端在生命周期切换(playing/paused/ended/stop)时经
/// `report_media_state` 命令回报校准。app 级瞬态(§6.4 派生可丢:丢了 = 按空闲算、不出错)。
/// 回合装配时读成一行「此刻」背景喂模型 → 修「歌放完了模型却以为还在播着」。
#[derive(Debug, Clone, Default)]
struct Playback {
    /// None = 空闲(没在播任何东西);Some = 正在放/暂停的标题。
    title: Option<String>,
    /// 仅当 title 为 Some 时有意义:true = 暂停,false = 正在播。
    paused: bool,
    /// 当前在播的剧集进度「第 index/共 total 集」(单集内容为 None);喂模型「此刻」背景用。
    pos: Option<(usize, usize)>,
}

/// 当前剧集队列(app 级瞬态,§6.4 派生可丢:丢了 = 退化成单集,绝不出错)。来源无关 ——
/// B 站合集/分P 与本地剧集填的是同一个队列;`advance` 只挪 index、`play_entry` 现取现播。
#[derive(Debug, Clone)]
struct Playlist {
    /// 续播记忆的 key(B 站 season id/bvid;本地 `local:FNV(目录+骨架)`)。
    series_key: String,
    entries: Vec<EpisodeRef>,
    /// 当前集下标。
    index: usize,
    /// 整队列继承首集的音/画意图(放歌 vs 看视频),切集不变。
    audio_only: bool,
}

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
    /// ffmpeg 是否已发起后台预取(每进程至多一次;失败复位留给用时下载重试)。
    ffmpeg_prefetch_started: AtomicBool,
    /// 因「需登录」卡住、待登录后自动重放的播放(按源 id)。
    pending_play: Mutex<HashMap<String, PendingPlay>>,
    /// 前端播放器的当下状态(回合装配读它喂模型「此刻」背景;见 Playback 注释)。
    playback: Mutex<Playback>,
    /// 当前剧集队列(多集续播;None = 没在放剧集/单集内容)。
    playlist: Mutex<Option<Playlist>>,
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
                ffmpeg_prefetch_started: AtomicBool::new(false),
                pending_play: Mutex::new(HashMap::new()),
                playback: Mutex::new(Playback::default()),
                playlist: Mutex::new(None),
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
                    // 重放走完整 play(会重建队列):带新 cookie 重新发现合集/分P,resume 规则照常生效。
                    if let Err(e) = this.play(p.user_id, &p.page_url, p.audio_only, false).await {
                        tracing::warn!("登录后自动重放失败: {e:#}");
                    }
                });
            }
        }
        Ok(())
    }

    /// 记下一次「因需登录而卡住」的播放,待登录成功后自动重放。
    fn record_pending(&self, user_id: i64, source: &str, page_url: &str, audio_only: bool) {
        self.inner.pending_play.lock().unwrap().insert(
            source.to_string(),
            PendingPlay {
                user_id,
                page_url: page_url.to_string(),
                audio_only,
                at: Instant::now(),
            },
        );
    }

    /// 取走某源的待重放(取即消费,不重复);超过 TTL 的丢弃、返回 None。
    fn take_pending_play(&self, source: &str) -> Option<PendingPlay> {
        let p = self.inner.pending_play.lock().unwrap().remove(source)?;
        (p.at.elapsed() <= PENDING_PLAY_TTL).then_some(p)
    }

    /// 失败重下一个组件(前端「重试」按钮直连,§7.1 不绕 LLM)。按组件名找枚举,后台重跑
    /// `ensure`(自带 HUD 任务:成功 done、再失败仍 fail_retryable 冒新卡)。不阻塞调用方。
    pub fn retry_component(&self, name: &str) {
        let Some(c) = Component::from_name(name) else {
            tracing::warn!(component = name, "retry_download:未知组件名,忽略");
            return;
        };
        let this = self.clone();
        tokio::spawn(async move {
            let _ = this.inner.components.ensure(c, &this.mirrors()).await;
        });
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

    /// 首次播放**任何**媒体(含放歌)就后台预取 ffmpeg(fire-and-forget,不 await):视频迟早要它
    /// (网络 DASH 混流 / 本地 HEVC/AC3 转码),提前下好 → 真用到时零等待。下了不一定用、后台不阻塞
    /// 当前播放,所以放歌也预取(用户拍板,2026-06-19:预取的是工具不是转码,「下了不一定用、真用到不必等」)。
    /// 每进程至多触发一次;失败复位标记,留给后续重试。`ensure_component` 内有锁去重 + 已在磁盘即秒返回
    /// → 预取与用时下载只下一份、只冒一张卡,ffmpeg 已就绪时这步是即时 no-op。
    fn prefetch_ffmpeg(&self) {
        if self.inner.ffmpeg_prefetch_started.swap(true, Ordering::Relaxed) {
            return; // 本进程已预取过(或正在跑)
        }
        let this = self.clone();
        tokio::spawn(async move {
            match this.ensure_component(Component::Ffmpeg).await {
                Ok(_) => tracing::info!("ffmpeg 预取就绪"),
                Err(e) => {
                    tracing::warn!("ffmpeg 预取失败(用时会再试): {e:#}");
                    this.inner.ffmpeg_prefetch_started.store(false, Ordering::Relaxed);
                }
            }
        });
    }

    /// 播放(用户发起):先**发现剧集队列**(B 站合集/分P → view API;本地 → 同文件夹扫描),
    /// 套用**续播规则**定起播集,再把那一集交给 `play_entry` 现取现播。`restart=true`(用户说
    /// 「从头/重新看」)= 忽略续播存档、从第一集起。单个内容(电影/单曲)队列为空,退化成原行为。
    /// 错误向上抛(工具层转成喂模型的观察)。
    pub async fn play(
        &self,
        user_id: i64,
        page_url: &str,
        audio_only: bool,
        restart: bool,
    ) -> Result<PlayOutcome> {
        self.prefetch_ffmpeg(); // 后台预取(首次播放任何媒体即触发),不阻塞本次播放
        let (pos, target) = self.build_queue(user_id, page_url, audio_only, restart).await;
        self.play_entry(user_id, &target, audio_only, pos).await
    }

    /// 发现并装配剧集队列,返回 `(起播集的队列位置, 该集可播地址)`。
    /// **续播规则**:仅当请求落在「自然起点」(requested_index==0)且非 restart 时,才用存档跳到上次那集
    /// (`resumed=true`);用户点名某集(index>0)→ 就放那集、不跳。单集/发现失败 → 清队列、(None, 原 url)。
    async fn build_queue(
        &self,
        user_id: i64,
        page_url: &str,
        audio_only: bool,
        restart: bool,
    ) -> (Option<PlaylistPos>, String) {
        let discovered = if is_local_path(page_url) {
            local_episodes(std::path::Path::new(page_url))
        } else if let Some(source) = self.source_of_url(page_url) {
            let cookie =
                cookies::load(&self.inner.store, source.id()).map(|c| cookies::header_value(&c));
            match source.episodes(page_url, cookie.as_deref()).await {
                Ok(d) => d,
                Err(e) => {
                    tracing::info!("剧集发现失败,按单集处理: {e:#}");
                    None
                }
            }
        } else {
            None
        };

        // 不成系列(单集 / 发现失败 / <2 集)→ 清队列,退化成单集播放。
        let Some((key, entries)) = discovered.filter(|(_, e)| e.len() >= 2) else {
            *self.inner.playlist.lock().unwrap() = None;
            return (None, page_url.to_string());
        };

        // requested = 用户实际点的那集在队列里的位置(本地按绝对路径、B 站按 page_url 精确匹配;
        // 分P 的 P1 用裸 bvid url 对齐)。找不到 → 0(自然起点)。
        let requested = entries.iter().position(|e| e.url == page_url).unwrap_or(0);
        let mut index = requested;
        let mut resumed = false;
        if !restart && requested == 0 {
            if let Some(prog) = self.inner.store.media_progress.get(user_id, &key).ok().flatten() {
                if let Some(i) = entries.iter().position(|e| e.id == prog.episode_id) {
                    index = i;
                    resumed = i != 0; // 跳到非首集才算「接着上次」
                }
            }
        }
        let total = entries.len();
        let target = entries[index].url.clone();
        // 落进度(起播即记)。失败不挡播放 —— 续播是锦上添花。
        let _ = self.inner.store.media_progress.set(user_id, &key, &entries[index].id, 0.0);
        *self.inner.playlist.lock().unwrap() =
            Some(Playlist { series_key: key, entries, index, audio_only });
        (Some(PlaylistPos { index, total, resumed }), target)
    }

    /// 上/下一集(嘴控「下一集」、播放器按钮、`ended` 自动续播都汇到这):在**现有队列**里挪
    /// `delta`(±1),把那一集现取现播(不重建队列、流地址永不过期)。越界 = 报错喂回(到头/到顶)。
    pub async fn advance(&self, user_id: i64, delta: i32) -> Result<PlayOutcome> {
        let (target, audio_only, pos) = {
            let mut guard = self.inner.playlist.lock().unwrap();
            let pl = guard
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("现在没有在播放剧集,没有上一集/下一集可切"))?;
            let total = pl.entries.len() as i32;
            let new = pl.index as i32 + delta;
            anyhow::ensure!(new >= 0, "已经是第一集了");
            anyhow::ensure!(new < total, "已经是最后一集了,整季都放完啦");
            pl.index = new as usize;
            let e = &pl.entries[pl.index];
            // 切集即落进度(下次续播接得上)。
            let _ = self.inner.store.media_progress.set(user_id, &pl.series_key, &e.id, 0.0);
            (
                e.url.clone(),
                pl.audio_only,
                PlaylistPos { index: pl.index, total: pl.entries.len(), resumed: false },
            )
        };
        self.play_entry(user_id, &target, audio_only, Some(pos)).await
    }

    /// 放**一集**(队列已定;不碰队列):本地直走文件端点,网络走 yt-dlp 解析 → 注册转发。
    /// `pos` = 这一集在队列里的位置(None = 单集),会写进 `NowPlaying.playlist`。`play`/`advance` 共用。
    async fn play_entry(
        &self,
        user_id: i64,
        page_url: &str,
        audio_only: bool,
        pos: Option<PlaylistPos>,
    ) -> Result<PlayOutcome> {
        if is_local_path(page_url) {
            return self.play_local(page_url, audio_only, pos).await.map(PlayOutcome::Playing);
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
                        self.record_pending(user_id, id, page_url, audio_only);
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
        let mut manifest_url: Option<String> = None;
        let stream_url = if streams.len() == 2 {
            // 音视频分离(B 站 DASH 常态)。**优先 DASH 直供**:不混流 → 前端 shaka 经 MSE 播两条流、
            // 播放器自己管时间轴 → 原生 seek + 音画同步(像 b 站网页;治混流 ?t= 重启 seek 的错位)。
            // 合成 MPD 需时长 + 探到 sidx;任一不满足 → 回落 ffmpeg 混流(老路,seek 有错位但至少能放)。
            let audio = streams.pop().expect("len==2");
            let video = streams.pop().expect("len==2");
            let dash = match resolved.duration_seconds {
                Some(dur) => match relay.register_dash(video.clone(), audio.clone(), dur).await {
                    Ok(url) => Some(url),
                    Err(e) => {
                        tracing::info!("DASH 直供不可用,回落 ffmpeg 混流: {e:#}");
                        None
                    }
                },
                None => {
                    tracing::info!("解析无时长,DASH 直供跳过,走 ffmpeg 混流");
                    None
                }
            };
            if let Some(url) = dash {
                manifest_url = Some(url.clone());
                url // stream_url 也存 manifest(前端优先用 manifest_url 走 shaka;此处只为非空)
            } else {
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
                relay.register_remux(video, audio, ffmpeg)
            }
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
            manifest_url,
            page_url: page_url.into(),
            source: source_id.clone().unwrap_or_else(|| "web".into()),
            playlist: pos,
        };
        self.seed_playing(&np.title, pos.map(|p| (p.index, p.total)));
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
    async fn hls_or_fallback(
        &self,
        relay: &relay::Relay,
        path: &std::path::Path,
        transcode_video: bool,
        transcode_audio: bool,
        duration: Option<f64>,
    ) -> (String, Option<String>) {
        let Some(dur) = duration.filter(|d| *d > 0.0) else {
            tracing::warn!(path = %path.display(), "无时长,HLS VOD 列表建不了,回落 /m/ 渐进混流(seek 仍错)");
            return (self.remux_or_direct(relay, path, transcode_video, transcode_audio).await, None);
        };
        match self.ensure_component(Component::Ffmpeg).await {
            Ok(ffmpeg) => {
                // HLS 段一律转码视频 + 立体声 AAC(见 relay::build_frag_cmd 三处实证),
                // 故不再传 transcode_* —— 它们只在上面无时长回落 /m/ 时用。
                let url = relay.register_file_hls(path.to_path_buf(), ffmpeg, dur);
                (url.clone(), Some(url))
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), "ffmpeg 取不到,本地无法转码,退回直传(可能黑屏/无声): {e:#}");
                (relay.register_file(path.to_path_buf()), None)
            }
        }
    }

    /// 取 ffmpeg 注册转封装/转码 URL(走 /m/);ffmpeg 取不到则退回原生直传。HLS 无时长时的回落用。
    async fn remux_or_direct(
        &self,
        relay: &relay::Relay,
        path: &std::path::Path,
        transcode_video: bool,
        transcode_audio: bool,
    ) -> String {
        match self.ensure_component(Component::Ffmpeg).await {
            Ok(ffmpeg) => {
                relay.register_file_remux(path.to_path_buf(), ffmpeg, transcode_video, transcode_audio)
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), "ffmpeg 取不到,退回直传(可能黑屏/无声): {e:#}");
                relay.register_file(path.to_path_buf())
            }
        }
    }

    /// 用已就绪的 ffmpeg 探测非 BMFF 容器(mkv/avi…):跑 `ffmpeg -i` 读 stderr 拿编码/时长。
    /// `-i` 无输出会非零退出但信息照打 stderr → 不看退出码、只解析 stderr;探不出按全兼容降级。
    async fn probe_with_ffmpeg(
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

    /// 本地文件(含 NAS 挂载/UNC):跳过 yt-dlp,注册文件端点即播 —— 单文件免混流,
    /// Range 原生 seek 白送,秒级无任务进度可言,不上 HUD。
    ///
    /// 视频按编码/容器三分路(§8.1「WebView2 编解码坑」的本地补课,网络路径早强制了 avc+m4a,
    /// 本地直传此前全漏了):**只转 WebView2 处理不了的那部分**(§7.1 用户拍板「按需」)——
    ///   · BMFF(mp4/mov/m4v):读 moov 轻量探测(不下 ffmpeg),全兼容则原生直传秒开;音轨 AC3/DTS
    ///     或视频 HEVC 不兼容才取 ffmpeg 转(兼容轨 -c copy、不兼容轨才转码);
    ///   · mkv/avi 等容器(WebView2 本就放不了):必经 ffmpeg 转封装成 fMP4,先确保 ffmpeg、`ffmpeg -i`
    ///     探编码,再按需 copy/转码;
    ///   · webm / 未知 / 放歌(audio_only):直传,交给浏览器。
    ///
    /// `pos` = 这一集在本地剧集队列里的位置(None = 单文件);写进 `NowPlaying.playlist` 驱动续播 UI。
    async fn play_local(
        &self,
        path_str: &str,
        audio_only: bool,
        pos: Option<PlaylistPos>,
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

        // 三分路(详见上方 doc):放歌直传 / BMFF 轻量探测 / mkv 等容器走 ffmpeg。不兼容文件走 HLS
        //(manifest_url 有值 → 前端 shaka 播 → 原生 seek + 音画同步);兼容文件原生 /f/ 直传。
        let mut duration_seconds = None;
        let mut manifest_url: Option<String> = None;
        let stream_url = if audio_only {
            relay.register_file(path.clone()) // 放歌:本地音频常见格式浏览器都吃,直传
        } else if probe::is_isobmff_ext(&path) {
            // BMFF:读 moov 探测(同步 IO,挪 spawn_blocking),普通文件秒开不下 ffmpeg
            let p = path.clone();
            let pr = tokio::task::spawn_blocking(move || probe::probe_local(&p))
                .await
                .unwrap_or_default();
            duration_seconds = pr.duration_seconds;
            if pr.audio_incompatible || pr.video_incompatible {
                self.log_local_codec(&path, &pr);
                let (su, mu) = self
                    .hls_or_fallback(relay, &path, pr.video_incompatible, pr.audio_incompatible, pr.duration_seconds)
                    .await;
                manifest_url = mu;
                su
            } else {
                relay.register_file(path.clone()) // 全兼容:原生直传秒开
            }
        } else if probe::needs_ffmpeg_container(&path) {
            // mkv/avi 等容器 WebView2 放不了,必经 ffmpeg:先确保 ffmpeg、用它探编码,有时长走 HLS、否则 /m/
            match self.ensure_component(Component::Ffmpeg).await {
                Ok(ffmpeg) => {
                    let pr = self.probe_with_ffmpeg(&ffmpeg, &path).await;
                    duration_seconds = pr.duration_seconds;
                    self.log_local_codec(&path, &pr);
                    if let Some(dur) = pr.duration_seconds.filter(|d| *d > 0.0) {
                        // HLS 段一律转码视频 + 立体声 AAC(relay::build_frag_cmd),不传 transcode_*
                        let url = relay.register_file_hls(path.clone(), ffmpeg, dur);
                        manifest_url = Some(url.clone());
                        url
                    } else {
                        relay.register_file_remux(
                            path.clone(),
                            ffmpeg,
                            pr.video_incompatible,
                            pr.audio_incompatible,
                        )
                    }
                }
                Err(e) => {
                    tracing::warn!(path = %path.display(), "ffmpeg 取不到,容器无法转封装,退回直传(可能放不了): {e:#}");
                    relay.register_file(path.clone())
                }
            }
        } else {
            relay.register_file(path.clone()) // webm / 未知 → 直传,交给浏览器
        };

        let np = NowPlaying {
            kind: if audio_only { MediaKind::Audio } else { MediaKind::Video },
            title,
            author: None,
            duration_seconds,
            stream_url,
            manifest_url,
            page_url: path_str.into(),
            source: "local".into(),
            playlist: pos,
        };
        self.seed_playing(&np.title, pos.map(|p| (p.index, p.total)));
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

    /// 起播时乐观 seed「正在放」(前端随后经 report 校准;这步只是让模型立刻就知道在放什么)。
    /// `pos` = (index, total):在播剧集时把「第N/共M集」一并记下,喂模型「此刻」背景。
    fn seed_playing(&self, title: &str, pos: Option<(usize, usize)>) {
        *self.inner.playback.lock().unwrap() =
            Playback { title: Some(title.to_string()), paused: false, pos };
    }

    /// 前端回报播放态(`report_media_state` 命令 → 这里):前端是播放真相源,
    /// ended/stop/pause/resume 全经此校准 core 快照。status: playing|paused|idle(其余按 playing)。
    /// 集数位置(pos)由 core 起播/切集时 seed,前端回报不带 → 这里**保留**已有 pos(只更标题/暂停态),
    /// idle 才整体清空。
    pub fn set_playback(&self, status: &str, title: Option<String>) {
        let mut guard = self.inner.playback.lock().unwrap();
        *guard = match status {
            "idle" => Playback::default(),
            "paused" => Playback { title, paused: true, pos: guard.pos },
            // playing / loading / 其它 → 正在播
            _ => Playback { title, paused: false, pos: guard.pos },
        };
    }

    /// 「此刻」播放器状态的一行背景:回合装配追加到末条 user 喂模型,让它任何时候都拿得到
    /// 当下真相(修「歌放完了却以为还在播」)。总返回一行(含空闲),由提示词法条约束「只在
    /// 跟播放有关时才参考、平时别主动提」。在播剧集时带上「第N/共M集」。
    pub fn playback_summary(&self) -> Option<String> {
        let pb = self.inner.playback.lock().unwrap().clone();
        // 剧集补一段「(第N集/共M集)」,让模型知道进度(如被问"放到哪了""下一集")。
        let ep = pb
            .pos
            .map(|(i, n)| format!("(第{}集/共{n}集)", i + 1))
            .unwrap_or_default();
        Some(match (pb.title, pb.paused) {
            (None, _) => "播放器现在空闲,没有在播放任何内容".to_string(),
            (Some(t), false) => format!("播放器正在播放《{t}》{ep}"),
            (Some(t), true) => format!("播放器已暂停,停在《{t}》{ep}"),
        })
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

// ---------- 本地剧集发现(确定式:同文件夹 + 数字骨架分组 + 自然排序) ----------

/// 从一个本地文件推断出它所属的「剧集」队列:同文件夹、**同类**(视频/音频)、**数字骨架相同**
/// 的兄弟文件,自然排序。返回 `(series_key, 有序集列表)`;**不成系列**(平铺电影库 / 一文件夹一部 /
/// 未知类型)→ None,退回单集播放(现行为)。
///
/// 数字骨架分组防误判:把文件名里的数字段抹成 `#` 当骨架 —— `小猪佩奇E01/E02` 同骨架 `小猪佩奇E#`
/// = 一季;`肖申克的救赎 / 阿甘正传` 骨架各异 = 各自单独不续播。`series_key` = `local:FNV(小写父目录+骨架)`
/// **单向哈希,绝不落绝对路径**(§6.2);整棵目录搬走仍对得上(同相对结构 → 同 key)。
fn local_episodes(current: &std::path::Path) -> Option<(String, Vec<EpisodeRef>)> {
    let parent = current.parent()?;
    let cur_name = current.file_name()?.to_str()?;
    // 当前文件的桶:队列只收同桶文件(放视频不混进音频,反之亦然)。
    let want_video = probe::is_video_ext(current);
    if !want_video && !probe::is_audio_ext(current) {
        return None; // 未知类型,不组队列
    }
    let in_bucket = |p: &std::path::Path| {
        if want_video {
            probe::is_video_ext(p)
        } else {
            probe::is_audio_ext(p)
        }
    };

    let cur_skel = digit_skeleton(file_stem_str(cur_name));
    let mut group: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(parent).ok()? {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if !path.is_file() || !in_bucket(&path) {
            continue;
        }
        if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
            if digit_skeleton(file_stem_str(name)) == cur_skel {
                group.push(name.to_string());
            }
        }
    }
    if group.len() < 2 {
        return None; // 不成系列 → 单集
    }
    group.sort_by(|a, b| natural_cmp(a, b));

    let entries: Vec<EpisodeRef> = group
        .iter()
        .map(|name| EpisodeRef {
            id: name.clone(), // 相对文件名 = 集身份(续播记忆存它,不存绝对路径)
            url: parent.join(name).to_string_lossy().into_owned(),
            title: file_stem_str(name).to_string(),
        })
        .collect();
    let key_material = format!("{}\u{1f}{}", parent.to_string_lossy().to_lowercase(), cur_skel);
    Some((format!("local:{}", fnv1a_hex(&key_material)), entries))
}

/// 取文件名(无目录)的主名部分(去最后一段扩展名);无扩展名则原样。
fn file_stem_str(name: &str) -> &str {
    std::path::Path::new(name).file_stem().and_then(|s| s.to_str()).unwrap_or(name)
}

/// 把文件名里每一段连续 ASCII 数字抹成一个 `#`,得到「骨架」(分组用)。
fn digit_skeleton(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_digit = false;
    for c in s.chars() {
        if c.is_ascii_digit() {
            if !prev_digit {
                out.push('#');
            }
            prev_digit = true;
        } else {
            out.push(c);
            prev_digit = false;
        }
    }
    out
}

/// 自然排序:把数字段当数字比(`E2 < E10`、`第2集 < 第10集`),其余按字符比。
/// 同骨架文件的数字/非数字段天然对齐,逐段比即可;大小写仅作末位 tiebreak(稳定)。
fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (mut x, mut y) = (a, b);
    loop {
        match (x.is_empty(), y.is_empty()) {
            (true, true) => return Ordering::Equal,
            (true, false) => return Ordering::Less,
            (false, true) => return Ordering::Greater,
            _ => {}
        }
        let x_digit = x.as_bytes()[0].is_ascii_digit();
        let y_digit = y.as_bytes()[0].is_ascii_digit();
        if x_digit && y_digit {
            let xe = x.find(|c: char| !c.is_ascii_digit()).unwrap_or(x.len());
            let ye = y.find(|c: char| !c.is_ascii_digit()).unwrap_or(y.len());
            let (xn, xr) = x.split_at(xe);
            let (yn, yr) = y.split_at(ye);
            // 比数值:去前导零后按长度、再按字节(任意长度都对,不靠 parse 免溢出)。
            let (xt, yt) = (xn.trim_start_matches('0'), yn.trim_start_matches('0'));
            match xt.len().cmp(&yt.len()).then_with(|| xt.cmp(yt)) {
                Ordering::Equal => {
                    x = xr;
                    y = yr;
                }
                ord => return ord,
            }
        } else if !x_digit && !y_digit {
            let xe = x.find(|c: char| c.is_ascii_digit()).unwrap_or(x.len());
            let ye = y.find(|c: char| c.is_ascii_digit()).unwrap_or(y.len());
            let (xs, xr) = x.split_at(xe);
            let (ys, yr) = y.split_at(ye);
            match xs.to_lowercase().cmp(&ys.to_lowercase()).then_with(|| xs.cmp(ys)) {
                Ordering::Equal => {
                    x = xr;
                    y = yr;
                }
                ord => return ord,
            }
        } else {
            // 一边数字一边非数字(同骨架不会到此;泛用兜底):按首字节定序,确定即可。
            return x.as_bytes()[0].cmp(&y.as_bytes()[0]);
        }
    }
}

/// FNV-1a 64 位 → 16 位十六进制。**稳定**(跨版本不变,续播记忆 key 依赖它),
/// 且单向 —— 本地 series_key 用它把「父目录+骨架」哈希掉,不在 DB 落绝对路径(§6.2)。
fn fnv1a_hex(s: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
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

    #[test]
    fn natural_sort_orders_episodes_numerically() {
        let mut v = vec!["第10集.mp4", "第2集.mp4", "第1集.mp4", "第21集.mp4"];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(v, vec!["第1集.mp4", "第2集.mp4", "第10集.mp4", "第21集.mp4"]);
        // E2 < E10(字典序会把 E10 排前面,自然序不会)
        let mut e = vec!["S01E10.mkv", "S01E2.mkv", "S01E1.mkv"];
        e.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(e, vec!["S01E1.mkv", "S01E2.mkv", "S01E10.mkv"]);
        // 前导零等价:E01 == E1(数值相等),不影响相对顺序的稳定
        assert_eq!(natural_cmp("E01", "E1"), std::cmp::Ordering::Equal);
    }

    #[test]
    fn skeleton_groups_series_and_separates_movies() {
        assert_eq!(digit_skeleton("小猪佩奇E01"), "小猪佩奇E#");
        assert_eq!(digit_skeleton("小猪佩奇E02"), "小猪佩奇E#");
        assert_eq!(digit_skeleton("S01E02"), "S#E#");
        // 不同剧 / 电影:骨架不同 → 不会被归到一组
        assert_ne!(digit_skeleton("肖申克的救赎"), digit_skeleton("阿甘正传"));
    }

    fn touch(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, b"x").unwrap();
        p
    }

    #[test]
    fn local_episodes_builds_queue_for_a_series() {
        let dir = std::env::temp_dir().join(format!("lw-ep-series-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // 一季动画 + 一个字幕(应被过滤)+ 一个无关电影(骨架不同,应被排除)
        touch(&dir, "小猪佩奇 第1集.mp4");
        let e2 = touch(&dir, "小猪佩奇 第2集.mp4");
        touch(&dir, "小猪佩奇 第10集.mp4");
        touch(&dir, "小猪佩奇 第1集.srt"); // 非媒体,过滤
        touch(&dir, "无关电影.mp4"); // 骨架不同,排除

        let (key, eps) = local_episodes(&e2).expect("应识别为剧集");
        assert!(key.starts_with("local:"));
        assert_eq!(eps.len(), 3, "三集,排除字幕与无关电影");
        // 自然排序:1 < 2 < 10
        assert_eq!(eps[0].title, "小猪佩奇 第1集");
        assert_eq!(eps[1].title, "小猪佩奇 第2集");
        assert_eq!(eps[2].title, "小猪佩奇 第10集");
        // 集身份 = 相对文件名(不含绝对路径)
        assert_eq!(eps[0].id, "小猪佩奇 第1集.mp4");
        assert!(!eps[0].id.contains('/') && !eps[0].id.contains('\\'), "id 是相对名");
        // url 是可播的绝对路径
        assert!(is_local_path(&eps[1].url));
    }

    #[test]
    fn local_episodes_none_for_flat_movie_folder() {
        let dir = std::env::temp_dir().join(format!("lw-ep-movies-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let m = touch(&dir, "肖申克的救赎.mkv");
        touch(&dir, "阿甘正传.mkv");
        touch(&dir, "教父.mkv");
        // 平铺电影库:骨架各异 → 当前文件所在组只有 1 个 → 不续播
        assert!(local_episodes(&m).is_none(), "平铺电影不该误判成剧集");

        // 一文件夹一部电影也不续播
        let solo = std::env::temp_dir().join(format!("lw-ep-solo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&solo);
        std::fs::create_dir_all(&solo).unwrap();
        let only = touch(&solo, "某电影 (2024) 1080p.mp4");
        assert!(local_episodes(&only).is_none(), "单文件 → 单集");
    }

    #[tokio::test]
    async fn play_local_serves_file_through_relay() {
        let (rt, mut rx) = runtime("local");
        let dir = std::env::temp_dir().join(format!("lw-media-local-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("儿歌串烧.mp3");
        std::fs::write(&f, b"FAKE-MP3-BYTES").unwrap();

        let np = match rt.play(1, &f.to_string_lossy(), true, false).await.unwrap() {
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
        assert!(rt.play(1, "/no/such/file.mp4", false, false).await.is_err());
    }

    #[tokio::test]
    async fn local_series_autoadvance_and_resume_rule() {
        let (rt, _rx) = runtime("series");
        let dir = std::env::temp_dir().join(format!("lw-series-play-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mk = |n: &str| {
            let f = dir.join(n);
            std::fs::write(&f, b"x").unwrap();
            f.to_string_lossy().to_string()
        };
        let e1 = mk("剧 第1集.mp4");
        let _e2 = mk("剧 第2集.mp4");
        let e3 = mk("剧 第3集.mp4");
        let plist = |o: PlayOutcome| match o {
            PlayOutcome::Playing(np) => np.playlist.expect("应有队列位置"),
            other => panic!("应为 Playing,实际 {other:?}"),
        };

        // 起播第1集 → 三集队列,位置 0/3,非续播
        let pos = plist(rt.play(1, &e1, false, false).await.unwrap());
        assert_eq!((pos.index, pos.total, pos.resumed), (0, 3, false));

        // 自动/手动续播:下一集 → 1,再下一集 → 2
        assert_eq!(plist(rt.advance(1, 1).await.unwrap()).index, 1);
        assert_eq!(plist(rt.advance(1, 1).await.unwrap()).index, 2);
        // 末集再「下一集」= 越界报错(不重播)
        assert!(rt.advance(1, 1).await.is_err(), "末集再下一集应报错");
        // 上一集 → 回到 1
        assert_eq!(plist(rt.advance(1, -1).await.unwrap()).index, 1);

        // 进度此刻停在第2集 → 重放(点首集/没点集)续播跳回第2集
        let pos = plist(rt.play(1, &e1, false, false).await.unwrap());
        assert_eq!((pos.index, pos.resumed), (1, true), "应接着上次第2集");

        // restart=true → 回第1集、不续播
        let pos = plist(rt.play(1, &e1, false, true).await.unwrap());
        assert_eq!((pos.index, pos.resumed), (0, false));

        // 点名放第3集(index>0)→ 就放那集,不被续播带走
        assert_eq!(plist(rt.play(1, &e3, false, false).await.unwrap()).index, 2);

        // 没有队列时 advance 报错(没在放剧集)
        let (rt2, _rx2) = runtime("noqueue");
        assert!(rt2.advance(1, 1).await.is_err());
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
        rt.record_pending(1, "bilibili", "https://www.bilibili.com/video/BV1", true);
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
                    user_id: 1,
                    page_url: "https://www.bilibili.com/video/BVold".into(),
                    audio_only: false,
                    at: stale,
                },
            );
            assert!(rt.take_pending_play("bilibili").is_none(), "过期的待重放不返回");
        }
    }

    #[test]
    fn playback_snapshot_seed_report_and_summary() {
        let (rt, _rx) = runtime("playback");
        // 初始空闲(回合装配会据此告诉模型「没在播」)
        assert_eq!(
            rt.playback_summary().as_deref(),
            Some("播放器现在空闲,没有在播放任何内容")
        );
        // 起播乐观 seed → 正在播(单集:无集数)
        rt.seed_playing("天空之城", None);
        assert_eq!(rt.playback_summary().as_deref(), Some("播放器正在播放《天空之城》"));
        // 前端回报暂停(保留集数位置,这里无)
        rt.set_playback("paused", Some("天空之城".into()));
        assert_eq!(rt.playback_summary().as_deref(), Some("播放器已暂停,停在《天空之城》"));

        // 剧集:seed 带「第N/共M集」,且前端回报 playing 不丢集数位置
        rt.seed_playing("海底小纵队", Some((2, 12)));
        assert_eq!(
            rt.playback_summary().as_deref(),
            Some("播放器正在播放《海底小纵队》(第3集/共12集)")
        );
        rt.set_playback("playing", Some("海底小纵队".into()));
        assert_eq!(
            rt.playback_summary().as_deref(),
            Some("播放器正在播放《海底小纵队》(第3集/共12集)"),
            "前端回报不带集数 → 保留 seed 的位置"
        );

        // 前端回报 ended/stop → 空闲(修「歌放完了模型却以为还在播」),集数一并清
        rt.set_playback("idle", None);
        assert_eq!(
            rt.playback_summary().as_deref(),
            Some("播放器现在空闲,没有在播放任何内容")
        );
    }
}
