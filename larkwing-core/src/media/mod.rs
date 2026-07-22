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

/// 「怎么放的」——本次播放走了哪条链路(core 只发 key,前端按 locale 出短标签,§6.6)。
/// 给用户/开发者一个可见的「省 CPU 还是在转码」信号,也是 0.2.6 copy 切片真机验收的眼睛:
/// 同是本地不兼容片,看到 `HlsCopy`(视频没重编)还是 `HlsTranscode`(在转)一目了然。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaybackRoute {
    /// 原生直传:本地 `/f/` 全兼容文件 或 网络单流 `/s/` —— 零转码、原生 seek,最省。
    Direct,
    /// 自适应流(DASH,shaka/MSE):B 站音视频分离直供,播放器管时间轴 → 原生 seek + 同步。
    Dash,
    /// HLS 视频 `-c:v copy` 切片(视频已兼容 → 不重编码,仅音轨转 AAC):首播即省 CPU(0.2.6)。
    HlsCopy,
    /// HLS 重编码切片(视频 HEVC/AV1 等 WebView2 解不了 → 转 H.264):吃 CPU。
    HlsTranscode,
    /// ffmpeg 渐进混流(`/m/`):网络 DASH 回落 或 本地转封装,`?t=` 重启式 seek(无原生 seek)。
    Remux,
}

/// 从注册好的 relay URL 反推链路(relay 路径是稳定契约,前端也已按 `/m/` 判混流)。
/// 本地 `/hls/` 当前恒是重编码;0.2.6 copy 切片落地后由调用点显式传 `HlsCopy` 覆盖,不走这里。
fn derive_route(stream_url: &str, manifest_url: Option<&str>) -> PlaybackRoute {
    match manifest_url {
        Some(m) if m.contains("/dash/") => PlaybackRoute::Dash,
        Some(_) => PlaybackRoute::HlsTranscode, // `/hls/`(本地按需切片)
        None if stream_url.contains("/m/") => PlaybackRoute::Remux,
        None => PlaybackRoute::Direct,
    }
}

/// 固定 `target` 秒的段计划(转码用,不必关键帧对齐——转码每段从新 IDR 重编):
/// `(0,t),(t,t),…,(末段,余量)`。末段补到 `duration`。纯函数。
fn fixed_segments(duration: f64, target: f64) -> Vec<(f64, f64)> {
    if !(duration > 0.0) || !(target > 0.0) {
        return Vec::new();
    }
    let n = (duration / target).ceil().max(1.0) as usize;
    (0..n).map(|i| (i as f64 * target, (duration - i as f64 * target).min(target))).collect()
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
    /// 本次播放走的链路(见 `PlaybackRoute`):前端在播放条上出一枚「怎么放的」小徽章。
    pub route: PlaybackRoute,
    pub page_url: String,
    pub source: String,
    /// 多集续播位置:有值 = 这是一个 ≥2 集的剧集(B 站合集/分P、本地剧集文件夹)。
    /// 前端据 index/total 显示「第N/共M集」+ 上/下一集按钮;`ended` 时若非末集自动续播。
    /// None = 单个内容(电影/单曲),不出现集数 UI。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub playlist: Option<PlaylistPos>,
    /// 循环模式镜像:"off" / "one"(单曲)/ "all"(列表)。core 是唯一真相,每次 Play 事件
    /// 全量捎带 → 新播放的复位、切集时的延续,前端零猜测;"one" 由前端 `el.loop` 原生无缝循环。
    pub loop_mode: String,
    /// 随机播放镜像(仅多集队列可能为 true)。
    pub shuffle: bool,
    /// 全部音轨(本地探测;≥2 条 UI 才出切换钮,〔此刻〕才列清单)。网络流恒空(来源定音轨)。
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub audio_tracks: Vec<probe::AudioTrack>,
    /// 当前音轨(0 起下标;`-map 0:a:{n}` 的 n)。
    pub audio_track: usize,
    /// 有值 = 从这个位置(秒)接着播:切音轨重建管线时带上,前端加载完 seek 过去。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume_at: Option<f64>,
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
#[allow(clippy::large_enum_variant)] // NowPlaying 带音轨清单后变大;瞬态返回值,Box 徒增全部匹配点噪音
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
/// core 起播时乐观 seed,前端在生命周期切换(playing/paused/ended/stop)+ 音量/倍速/seek 调整
/// + 播放中低频心跳时经 `report_media_state` 命令回报校准。app 级瞬态(§6.4 派生可丢:
/// 丢了 = 按空闲算、不出错)。回合装配时读成一行「此刻」背景喂模型 → 修「歌放完了模型却
/// 以为还在播着」,并让模型知道当前音量/进度(才能「调到 50」「快进 5 分钟」这类绝对/相对操作)。
#[derive(Debug, Clone, Default)]
struct Playback {
    /// None = 空闲(没在播任何东西);Some = 正在放/暂停的标题。
    title: Option<String>,
    /// 仅当 title 为 Some 时有意义:true = 暂停,false = 正在播。
    paused: bool,
    /// 当前在播的剧集进度「第 index/共 total 集」(单集内容为 None);喂模型「此刻」背景用。
    pos: Option<(usize, usize)>,
    /// 基准音量 0–100(前端的用户意图值,不含唤醒避让折算;None = 尚无回报)。
    /// 跨播放粘住(seed/idle 都保留)——与前端「音量粘住」语义一致。
    volume_pct: Option<u8>,
    /// 播放位置/总长(秒)与倍速;`at` = 回报时刻,播放中按倍速外推出「此刻」位置
    /// (回报之间也准,不靠前端高频心跳)。
    position_secs: Option<f64>,
    duration_secs: Option<f64>,
    rate: Option<f64>,
    at: Option<std::time::Instant>,
}

/// 前端回报的播放器快照(`report_media_state` 命令载荷;新字段全可缺 —— 浏览器预览/旧路径兼容)。
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct PlaybackReport {
    /// playing | paused | idle | loading(其余按 playing)。
    pub status: String,
    #[serde(default)]
    pub title: Option<String>,
    /// 基准音量 0–100(用户意图,不含避让折算)。
    #[serde(default)]
    pub volume: Option<f64>,
    /// 播放位置 / 总长(秒)。
    #[serde(default)]
    pub position: Option<f64>,
    #[serde(default)]
    pub duration: Option<f64>,
    /// 倍速(缺省当 1)。
    #[serde(default)]
    pub rate: Option<f64>,
}

/// 切集目标:相对挪一格(上/下一集)或第 N 集绝对定位(1 起数;嘴控「看第五集」)。
#[derive(Debug, Clone, Copy)]
enum EpisodeTarget {
    Delta(i32),
    Nth(usize),
}

/// 循环模式(嘴控 loop_one/loop_all/loop_off;app 级,新 `play()` 请求复位 Off ——
/// 同「倍速每次复位、音量粘住」的粘性口径,切集/自动续播不复位)。
/// One 由前端 `el.loop` 原生无缝循环(ended 压根不触发);All 在 `auto_next` 里回卷队列
/// (没有队列时前端同样落到 `el.loop`,等价单曲循环)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoopMode {
    Off,
    One,
    All,
}

impl LoopMode {
    /// 过桥字符串(NowPlaying 镜像;前端按它对齐 el.loop 与按钮态)。
    fn as_str(self) -> &'static str {
        match self {
            LoopMode::Off => "off",
            LoopMode::One => "one",
            LoopMode::All => "all",
        }
    }
}

/// 当前剧集队列(app 级瞬态,§6.4 派生可丢:丢了 = 退化成单集,绝不出错)。来源无关 ——
/// B 站合集/分P 与本地剧集填的是同一个队列;`advance` 只挪 index、`play_entry` 现取现播。
#[derive(Debug, Clone)]
struct Playlist {
    /// 续播记忆的 key(B 站 season id/bvid;本地视频 `local:FNV(目录+骨架)`、
    /// 本地音频 `local:FNV(目录+audio)` —— 音频整夹一个队列,从哪首进都是同一个 key)。
    series_key: String,
    entries: Vec<EpisodeRef>,
    /// 当前集下标。
    index: usize,
    /// 整队列继承首集的音/画意图(放歌 vs 看视频),切集不变。
    audio_only: bool,
    /// 随机播放开关(嘴控 shuffle_on/off;随队列生灭)。开着时「下一首/自动续播」从这轮
    /// 没放过的里随机挑,「上一首」沿播放履历回退。
    shuffle: bool,
    /// 随机播放履历(这一轮已放过的队列下标,当前一首恒在末位;shuffle_on 时重置为 [当前])。
    played: Vec<usize>,
}

/// 当前本地播放的现场(切音轨重建管线用;app 级瞬态,§6.4 派生可丢:丢了 = 切不了轨,不出错)。
#[derive(Debug, Clone)]
struct CurrentLocal {
    page_url: String,
    audio_only: bool,
    /// 探测出的音轨清单(顺序 = -map 轨号)。
    tracks: Vec<probe::AudioTrack>,
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
    /// 循环模式(见 LoopMode;新 `play()` 请求复位 Off)。
    loop_mode: Mutex<LoopMode>,
    /// 选中的音轨(0 起;新 `play()` 复位 0,切集粘住 —— 看英文轨的剧下一集还是英文)。
    audio_track: Mutex<usize>,
    /// 当前本地播放现场(切音轨用;None = 没在放本地内容)。
    current_local: Mutex<Option<CurrentLocal>>,
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
                loop_mode: Mutex::new(LoopMode::Off),
                audio_track: Mutex::new(0),
                current_local: Mutex::new(None),
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

    /// 聊天图片缩略图落盘目录(`<media>/attachments`;随数据根搬家)。engine 写、命令读回。
    pub fn attachments_dir(&self) -> PathBuf {
        self.inner.dir.join("attachments")
    }

    /// 收到的文件/文档「收件区」(`<media>/inbox`;随数据根搬家)。手机发来的文件落这里、
    /// 把本地路径交给模型,「把发来的文件存到电脑 / 整理」才成立(扫描件读不出文字也能存,
    /// 存文件不需要读内容,§9)。与缩略图分开:那是给 UI 看的,这是给模型 fs 操作的原件。
    pub fn inbox_dir(&self) -> PathBuf {
        self.inner.dir.join("inbox")
    }

    /// 历史图片小票(相对文件名)→ 可显缩略图的 localhost URL(重开会话回看图,§1/§9)。
    /// 文件名兜底防目录穿越:只取末段 file_name,拒绝 `..` / 路径分隔。
    pub async fn attachment_url(&self, file: &str) -> Result<String> {
        let name = std::path::Path::new(file)
            .file_name()
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty())
            .context("非法附件名")?;
        self.file_url(self.attachments_dir().join(name)).await
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

    /// pdf_to_png(tools/pdf.rs)用:pdfium 动态库就位(用时下载,§6.9;进度上 HUD)。
    pub async fn ensure_pdfium(&self) -> Result<PathBuf> {
        self.ensure_component(Component::Pdfium).await
    }

    /// webrender(壳层渲染窗)回传信箱:确保 relay 在线 → (POST 地址, 一次性接收端)。
    pub async fn webrender_collect(
        &self,
    ) -> Result<(String, tokio::sync::oneshot::Receiver<String>)> {
        let relay = self.inner.relay.get_or_try_init(relay::Relay::start).await?;
        Ok(relay.register_collect())
    }

    /// 进度总线句柄(壳层 webrender 上任务卡用;Tasks 本就是 Clone 的轻句柄)。
    pub fn tasks(&self) -> Tasks {
        self.inner.tasks.clone()
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
        // 新播放请求 = 新内容意图:循环/音轨都复位(同「倍速每次复位」口径;切集不经这里 —— 音轨跨集粘住)。
        *self.inner.loop_mode.lock().unwrap() = LoopMode::Off;
        *self.inner.audio_track.lock().unwrap() = 0;
        // 目录入参 = 音频文件夹:强制只出声;≥2 首由 build_queue 组队连播,恰 1 首退化成放
        // 那一首,一首没有如实退回(播放链吃不了目录,绝不喂它;§3.5 不静默)。
        let single_fallback;
        let (page_url, audio_only) = if is_dir_path(page_url) {
            let dir = std::path::Path::new(page_url);
            let files = audio_folder_files(dir);
            match files.len() {
                0 => anyhow::bail!("这个文件夹里没有能播放的音频文件"),
                1 => {
                    single_fallback = dir.join(&files[0]).to_string_lossy().into_owned();
                    (single_fallback.as_str(), true)
                }
                _ => (page_url, true),
            }
        } else {
            (page_url, audio_only)
        };
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
        let _ = self.inner.store.media_progress.set(user_id, &key, &entries[index].id, &entries[index].title, 0.0);
        *self.inner.playlist.lock().unwrap() = Some(Playlist {
            series_key: key,
            entries,
            index,
            audio_only,
            shuffle: false,
            played: Vec::new(),
        });
        (Some(PlaylistPos { index, total, resumed }), target)
    }

    /// 上/下一集(嘴控「下一集」、播放器按钮、`ended` 自动续播都汇到这):在**现有队列**里挪
    /// `delta`(±1),把那一集现取现播(不重建队列、流地址永不过期)。越界 = 报错喂回(到头/到顶)。
    pub async fn advance(&self, user_id: i64, delta: i32) -> Result<PlayOutcome> {
        self.switch_episode(user_id, EpisodeTarget::Delta(delta)).await
    }

    /// 跳到第 N 集(1 起数;嘴控「看第五集」):队列是 core 的单一真相(B 站合集/分P、本地剧集
    /// 同一套 entries,「第N/共M集」的 N/M 就来自它)—— 模型只说集数,不需要也不可能自己拼链接。
    /// 越界/没在放剧集 = 报错喂回;跳到当前集 = 从头重放该集(合「再放一遍这集」的口语义)。
    pub async fn jump_to_episode(&self, user_id: i64, episode: usize) -> Result<PlayOutcome> {
        self.switch_episode(user_id, EpisodeTarget::Nth(episode)).await
    }

    /// 切集共用体(相对挪 / 第 N 集绝对定位):算目标 index → 越界报错 → 切集即落续播进度 →
    /// 那一集现取现播(不重建队列、流地址永不过期)。
    async fn switch_episode(&self, user_id: i64, target: EpisodeTarget) -> Result<PlayOutcome> {
        let loop_all = *self.inner.loop_mode.lock().unwrap() == LoopMode::All;
        let (target_url, audio_only, pos) = {
            let mut guard = self.inner.playlist.lock().unwrap();
            let pl = guard
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("现在没有在播放剧集,没有可切换的集"))?;
            let total = pl.entries.len();
            let new = match target {
                // 随机播放中的「下一首」= 这轮没放过的里随机挑(用户点名要,放完一轮也接着挑,
                // 恒有下一首);「上一首」= 沿履历回退(现场心智 = 回到刚才那首)。
                EpisodeTarget::Delta(d) if pl.shuffle => {
                    if d > 0 {
                        shuffle_advance(pl, true, shuffle_seed()).expect("wrap=true 恒有下一首")
                    } else {
                        shuffle_back(pl)
                            .ok_or_else(|| anyhow::anyhow!("随机播放刚开始,还没有可回退的上一首"))?
                    }
                }
                EpisodeTarget::Delta(d) => {
                    let n = pl.index as i32 + d;
                    if loop_all {
                        // 列表循环开着:到头/到顶都回卷(开着循环点「下一首」不该被「已是最后」拦下)
                        n.rem_euclid(total as i32) as usize
                    } else {
                        anyhow::ensure!(n >= 0, "已经是第一集了");
                        anyhow::ensure!((n as usize) < total, "已经是最后一集了,整季都放完啦");
                        n as usize
                    }
                }
                EpisodeTarget::Nth(n) => {
                    anyhow::ensure!(
                        (1..=total).contains(&n),
                        "这部一共 {total} 集,没有第 {n} 集"
                    );
                    let idx = n - 1;
                    // 点名跳集也记进随机履历(「上一首」能回来;auto_next 已推过的恰在末位,不重复)。
                    if pl.shuffle && pl.played.last() != Some(&idx) {
                        pl.played.push(idx);
                    }
                    idx
                }
            };
            pl.index = new;
            let e = &pl.entries[pl.index];
            // 切集即落进度(下次续播接得上)。
            let _ = self.inner.store.media_progress.set(user_id, &pl.series_key, &e.id, &e.title, 0.0);
            (
                e.url.clone(),
                pl.audio_only,
                PlaylistPos { index: pl.index, total, resumed: false },
            )
        };
        self.play_entry(user_id, &target_url, audio_only, Some(pos)).await
    }

    /// 一集自然放完(前端 `ended` 的唯一 core 入口):按循环/随机决定接下来放什么。
    /// Some = core 已接管(切下一首现取现播,Play 事件接力);None = 没有下一首,前端正常收尾。
    /// 只服务自动续播路;用户嘴控 next/prev 仍走 `advance`(到头报错的反馈是对的)。
    /// (单曲循环由前端 `el.loop` 原生循环,ended 压根不触发,不经这里。)
    pub async fn auto_next(&self, user_id: i64) -> Result<Option<PlayOutcome>> {
        let loop_all = *self.inner.loop_mode.lock().unwrap() == LoopMode::All;
        let target = {
            let mut guard = self.inner.playlist.lock().unwrap();
            let Some(pl) = guard.as_mut() else { return Ok(None) }; // 单集:交回前端收尾
            if pl.shuffle {
                // 随机:这轮没放过的里挑;都放过 → 列表循环重开一轮,否则收尾。
                shuffle_advance(pl, loop_all, shuffle_seed())
            } else if pl.index + 1 < pl.entries.len() {
                Some(pl.index + 1)
            } else if loop_all {
                Some(0) // 列表循环:末集放完回卷到第一集
            } else {
                None
            }
        };
        match target {
            Some(i) => self.switch_episode(user_id, EpisodeTarget::Nth(i + 1)).await.map(Some),
            None => Ok(None),
        }
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
            return self
                .play_local(page_url, audio_only, pos, true, None)
                .await
                .map(PlayOutcome::Playing);
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

        let (loop_mode, shuffle) = self.mode_flags();
        // 网络流没有本地音轨概念(来源已定轨)→ 清掉本地现场,切音轨会如实退回
        *self.inner.current_local.lock().unwrap() = None;
        let np = NowPlaying {
            kind: if audio_only { MediaKind::Audio } else { MediaKind::Video },
            title: resolved.title,
            author: resolved.uploader,
            duration_seconds: resolved.duration_seconds,
            route: derive_route(&stream_url, manifest_url.as_deref()),
            stream_url,
            manifest_url,
            page_url: page_url.into(),
            source: source_id.clone().unwrap_or_else(|| "web".into()),
            playlist: pos,
            loop_mode,
            shuffle,
            audio_tracks: Vec::new(),
            audio_track: 0,
            resume_at: None,
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
    async fn adaptive_or_fallback(
        &self,
        relay: &relay::Relay,
        path: &std::path::Path,
        pr: &probe::LocalProbe,
        sel_audio_bad: bool,
        audio_track: usize,
    ) -> (String, Option<String>, PlaybackRoute) {
        let (vi, ai) = (pr.video_incompatible, sel_audio_bad);
        let Some(dur) = pr.duration_seconds.filter(|d| *d > 0.0) else {
            tracing::warn!(path = %path.display(), "无时长,自适应建不了段清单,回落 muxed HLS");
            let (su, mu) =
                self.hls_or_fallback(relay, path, vi, ai, pr.duration_seconds, false, audio_track).await;
            return (su, mu, PlaybackRoute::HlsTranscode);
        };
        let ffmpeg = match self.ensure_component(Component::Ffmpeg).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(path = %path.display(), "ffmpeg 取不到,自适应走不了,退直传: {e:#}");
                return (relay.register_file(path.to_path_buf()), None, PlaybackRoute::Direct);
            }
        };
        // 视频兼容 + 有关键帧 + 能定 H.264 codec → copy 切片(省 CPU);否则转码(仍分离音频治漂移)。
        let copy_video =
            !pr.video_incompatible && !pr.video_keyframes.is_empty() && pr.video_codec.is_some();
        let segments = if copy_video {
            probe::plan_copy_segments(&pr.video_keyframes, dur, relay::HLS_SEG)
        } else {
            fixed_segments(dur, relay::HLS_SEG)
        };
        if segments.is_empty() {
            tracing::warn!(path = %path.display(), "自适应段计划为空,回落 muxed HLS");
            let (su, mu) = self.hls_or_fallback(relay, path, vi, ai, Some(dur), false, audio_track).await;
            return (su, mu, PlaybackRoute::HlsTranscode);
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
            return (su, mu, PlaybackRoute::HlsTranscode);
        };
        let Some(codec) = probe::video_h264_codec(&init) else {
            tracing::warn!(path = %path.display(), "自适应 init 解不出 H.264 codec,回落 muxed HLS");
            let (su, mu) = self.hls_or_fallback(relay, path, vi, ai, Some(dur), false, audio_track).await;
            return (su, mu, PlaybackRoute::HlsTranscode);
        };
        let video_mime = format!("video/mp4; codecs=\"{codec}\"");
        let route = if copy_video { PlaybackRoute::HlsCopy } else { PlaybackRoute::HlsTranscode };
        tracing::info!(path = %path.display(), copy = copy_video, segs = segments.len(),
            "自适应播放(音视频分离,连续音频治漂移)");
        let url = relay.register_file_adaptive(
            path.to_path_buf(), ffmpeg, copy_video, video_mime, init, segments, dur, enc, audio_track,
        );
        (url.clone(), Some(url), route)
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

    /// 音频字节(手机语音消息的 ogg/opus 等)→ 16k 单声道 f32 PCM(喂本地 ASR)。
    /// ffmpeg 组件解码:落临时文件(解封装要可 seek 输入更稳)→ `-f f32le` 读 stdout;
    /// `-t max_secs` 双保险截断(调用方已按时长挡长语音)。用完即删,失败如实报错。
    pub async fn decode_audio_pcm16k(&self, bytes: Vec<u8>, max_secs: u32) -> Result<Vec<f32>> {
        use std::sync::atomic::{AtomicU64, Ordering};
        let ffmpeg = self.ensure_component(Component::Ffmpeg).await?;
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let tmp = std::env::temp_dir().join(format!(
            "lw-voicemsg-{}-{}.bin",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        tokio::fs::write(&tmp, &bytes).await.context("语音临时文件写入失败")?;
        let mut cmd = tokio::process::Command::new(&ffmpeg);
        cmd.arg("-hide_banner")
            .arg("-i")
            .arg(&tmp)
            .arg("-t")
            .arg(max_secs.to_string())
            .arg("-f")
            .arg("f32le")
            .arg("-ar")
            .arg("16000")
            .arg("-ac")
            .arg("1")
            .arg("pipe:1");
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        no_console(&mut cmd);
        let out = tokio::time::timeout(std::time::Duration::from_secs(30), cmd.output()).await;
        let _ = tokio::fs::remove_file(&tmp).await;
        let out = out.context("ffmpeg 解码超时")?.context("ffmpeg 起不来")?;
        anyhow::ensure!(out.status.success(), "ffmpeg 解码失败(退出码 {:?})", out.status.code());
        anyhow::ensure!(!out.stdout.is_empty(), "解码出的音频为空");
        Ok(out
            .stdout
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect())
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
    /// 视频按编码/容器三分路(§8.1「WebView2 编解码坑」的本地补课,网络路径早强制了 avc+m4a,
    /// 本地直传此前全漏了):**只转 WebView2 处理不了的那部分**(§7.1 用户拍板「按需」)——
    ///   · BMFF(mp4/mov/m4v):读 moov 轻量探测(不下 ffmpeg),全兼容则原生直传秒开;音轨 AC3/DTS
    ///     或视频 HEVC 不兼容才取 ffmpeg 转(兼容轨 -c copy、不兼容轨才转码);
    ///   · mkv/avi 等容器(WebView2 本就放不了):必经 ffmpeg 转封装成 fMP4,先确保 ffmpeg、`ffmpeg -i`
    ///     探编码,再按需 copy/转码;
    ///   · webm / 未知 / 放歌(audio_only):直传,交给浏览器。
    ///
    /// `pos` = 这一集在本地剧集队列里的位置(None = 单文件);写进 `NowPlaying.playlist` 驱动续播 UI。
    /// `prefer_adaptive`:true(常态)= 不兼容 BMFF 走音视频分离自适应(治漂移 + copy 省 CPU);
    /// false = 前端手写 MSE 播放失败后的**兜底重放**,强制回落 muxed HLS(能放的老路)。
    async fn play_local(
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

        // 三分路(详见上方 doc):放歌直传 / BMFF 轻量探测 / mkv 等容器走 ffmpeg。不兼容文件走 HLS
        //(manifest_url 有值 → 前端 shaka 播 → 原生 seek + 音画同步);兼容文件原生 /f/ 直传。
        let mut duration_seconds = None;
        let mut manifest_url: Option<String> = None;
        let mut route: Option<PlaybackRoute> = None; // Some = 显式(自适应路 URL 反推不出 copy/转码)
        let mut tracks: Vec<probe::AudioTrack> = Vec::new(); // 音轨清单(BMFF/容器探测才有)
        let mut sel_track = 0usize; // 选中音轨(越界已钳回 0)
        let stream_url = if audio_only {
            relay.register_file(path.clone()) // 放歌:本地音频常见格式浏览器都吃,直传
        } else if probe::is_isobmff_ext(&path) {
            // BMFF:读 moov 探测(同步 IO,挪 spawn_blocking),普通文件秒开不下 ffmpeg
            let p = path.clone();
            let pr = tokio::task::spawn_blocking(move || probe::probe_local(&p))
                .await
                .unwrap_or_default();
            duration_seconds = pr.duration_seconds;
            tracks = pr.audio_tracks.clone();
            sel_track = self.clamp_audio_track(tracks.len());
            // 逐轨判定:多音轨片只看**选中那条**要不要转(选中 AAC 轨 = 音频可 copy)。
            let sel_audio_bad = tracks
                .get(sel_track)
                .map(|t| probe::audio_codec_needs_transcode(&t.codec))
                .unwrap_or(pr.audio_incompatible);
            // 多音轨**恒进管线**(即使全兼容):直传选不了轨——WKWebView 的 audioTracks API
            // 真机证伪(起播收敛/播放中启停都不生效 → 全轨混播),Chromium 干脆没有;管线
            // `-map` 只出选中那条,混播物理上不可能,响度链顺带统一(2026-07-21 用户拍板)。
            let need_pipeline =
                pr.video_incompatible || sel_audio_bad || tracks.len() >= 2;
            if need_pipeline {
                self.log_local_codec(&path, &pr);
                if prefer_adaptive {
                    // 0.2.6 治本:音视频分离自适应 —— 视频兼容→`copy` 省 CPU、否则转码;音频一整条连续
                    // 编码(治「逐段音频 priming 累积漂移」)。前提不满足自动回落 muxed HLS(绝不阻断)。
                    let (su, mu, r) =
                        self.adaptive_or_fallback(relay, &path, &pr, sel_audio_bad, sel_track).await;
                    manifest_url = mu;
                    route = Some(r);
                    su
                } else {
                    // 兜底重放:前端手写 MSE 播放失败 → 强制走 muxed HLS + **软件编码**(最兼容的老路,
                    // 会漂但不黑屏;硬件编码万一在这台机上花屏/解不了,回落这一步就换回软件)。
                    let (su, mu) = self
                        .hls_or_fallback(relay, &path, pr.video_incompatible, sel_audio_bad, pr.duration_seconds, true, sel_track)
                        .await;
                    manifest_url = mu;
                    su
                }
            } else {
                relay.register_file(path.clone()) // 全兼容:原生直传秒开
            }
        } else if probe::needs_ffmpeg_container(&path) {
            // mkv/avi 等容器 WebView2 放不了,必经 ffmpeg:先确保 ffmpeg、用它探编码。
            // 有时长走 HLS(shaka 原生 seek)、否则 /m/。(C2 整文件 remux 已删,见上。)
            match self.ensure_component(Component::Ffmpeg).await {
                Ok(ffmpeg) => {
                    let pr = self.probe_with_ffmpeg(&ffmpeg, &path).await;
                    duration_seconds = pr.duration_seconds;
                    tracks = pr.audio_tracks.clone();
                    sel_track = self.clamp_audio_track(tracks.len());
                    let sel_audio_bad = tracks
                        .get(sel_track)
                        .map(|t| probe::audio_codec_needs_transcode(&t.codec))
                        .unwrap_or(pr.audio_incompatible);
                    self.log_local_codec(&path, &pr);
                    if let Some(dur) = pr.duration_seconds.filter(|d| *d > 0.0) {
                        // HLS 段一律转码视频 + 立体声 AAC(relay::build_frag_cmd),不传 transcode_*。
                        // 硬件优先(force_software = 兜底重放时才置真)。
                        let url = relay
                            .register_file_hls(path.clone(), ffmpeg, dur, !prefer_adaptive, sel_track)
                            .await;
                        manifest_url = Some(url.clone());
                        url
                    } else {
                        relay
                            .register_file_remux(
                                path.clone(),
                                ffmpeg,
                                pr.video_incompatible,
                                sel_audio_bad,
                                !prefer_adaptive,
                                sel_track,
                            )
                            .await
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

        let (loop_mode, shuffle) = self.mode_flags();
        let route = route.unwrap_or_else(|| derive_route(&stream_url, manifest_url.as_deref()));
        // 记下本地现场:切音轨据此重建管线。多音轨时把清单打进日志(真机对「音轨没名字」
        // 一眼定案:是文件本身没标语言〔und〕还是解析漏了)。
        if tracks.len() >= 2 {
            tracing::info!(path = %path.display(), tracks = ?tracks, track = sel_track, "音轨清单");
        }
        *self.inner.current_local.lock().unwrap() = Some(CurrentLocal {
            page_url: path_str.to_string(),
            audio_only,
            tracks: tracks.clone(),
        });
        let np = NowPlaying {
            kind: if audio_only { MediaKind::Audio } else { MediaKind::Video },
            title,
            author: None,
            duration_seconds,
            // 自适应路显式给 route(URL 是 /la/,反推不出 copy/转码);其余路由由 URL 反推。
            route,
            stream_url,
            manifest_url,
            page_url: path_str.into(),
            source: "local".into(),
            playlist: pos,
            loop_mode,
            shuffle,
            audio_tracks: tracks,
            audio_track: sel_track,
            resume_at,
        };
        self.seed_playing(&np.title, pos.map(|p| (p.index, p.total)));
        self.publish(MediaEvent::Play(np.clone()));
        Ok(np)
    }

    /// 模型侧播放控制(用户用嘴说"暂停/大点声/倍速/跳到 90 秒");播放条的循环/随机按钮
    /// 经壳层命令也汇到这(同一校验/执行口)。speed/seek 带 value,其余不带;
    /// 词表和校验收口在这,前端只执行不判断。循环/随机先落 core 状态(auto_next/「此刻」背景
    /// 读它),再随 Control 事件让前端对齐 el.loop/按钮态。
    pub fn control(&self, action: &str, value: Option<f64>) -> Result<()> {
        match action {
            "pause" | "resume" | "stop" | "louder" | "softer" => {}
            "loop_one" | "loop_all" | "loop_off" => {
                *self.inner.loop_mode.lock().unwrap() = match action {
                    "loop_one" => LoopMode::One,
                    "loop_all" => LoopMode::All,
                    _ => LoopMode::Off,
                };
            }
            "shuffle_on" | "shuffle_off" => {
                let on = action == "shuffle_on";
                let mut guard = self.inner.playlist.lock().unwrap();
                match guard.as_mut() {
                    Some(pl) => {
                        pl.shuffle = on;
                        pl.played = vec![pl.index]; // 新一轮履历从当前这首起算
                    }
                    // 单曲/没在放列表:开随机没意义,如实退回(§3.5);关随机幂等、不吵。
                    None if on => anyhow::bail!("现在没有在放多首的列表,没法随机播放"),
                    None => {}
                }
            }
            "volume" => {
                let v = value.context("volume 需要 value(0–100)")?;
                anyhow::ensure!((0.0..=100.0).contains(&v), "音量范围 0–100,收到 {v}");
            }
            "speed" => {
                let v = value.context("speed 需要 value(倍速)")?;
                anyhow::ensure!((0.25..=3.0).contains(&v), "倍速范围 0.25–3,收到 {v}");
            }
            "seek" => {
                let v = value.context("seek 需要 value(秒)")?;
                anyhow::ensure!(v >= 0.0, "定位秒数不能为负");
            }
            other => anyhow::bail!(
                "未知动作 {other},可用: pause/resume/stop/louder/softer/volume/speed/seek/\
                 loop_one/loop_all/loop_off/shuffle_on/shuffle_off"
            ),
        }
        self.publish(MediaEvent::Control { action: action.into(), value });
        Ok(())
    }

    /// 读选中音轨并钳到有效范围(切集后新一集音轨数可能变少;越界回 0 并回写,状态别悬空)。
    fn clamp_audio_track(&self, total: usize) -> usize {
        let mut guard = self.inner.audio_track.lock().unwrap();
        if *guard >= total.max(1) {
            *guard = 0;
        }
        *guard
    }

    /// 播放器「此刻」位置(秒):最近回报值 + 播放中按倍速外推(与 playback_summary 同口径)。
    fn current_position(&self) -> Option<f64> {
        let pb = self.inner.playback.lock().unwrap().clone();
        let mut cur = pb.position_secs?;
        if !pb.paused {
            if let Some(at) = pb.at {
                cur += at.elapsed().as_secs_f64() * pb.rate.unwrap_or(1.0);
            }
        }
        if let Some(d) = pb.duration_secs {
            cur = cur.min(d);
        }
        Some(cur.max(0.0))
    }

    /// 切音轨(n 从 1 数;嘴控 media_control 的 audio_track 与播放条按钮汇到同一口)。
    /// mac + 原生直传:发 Control 事件让前端 `audioTracks` 就地启停(无缝、不重载);
    /// 其余管线:按选中轨重建(ffmpeg `-map`),`NowPlaying.resume_at` 带上当前位置接着放。
    /// 返回给模型的观察文本(§3.5 各种没得切都如实说)。
    pub async fn set_audio_track(&self, n: usize) -> Result<String> {
        let Some(cur) = self.inner.current_local.lock().unwrap().clone() else {
            anyhow::bail!("现在没有在放本地内容,切换不了音轨(网络流的音轨由来源决定)");
        };
        anyhow::ensure!(
            self.inner.playback.lock().unwrap().title.is_some(),
            "现在没有在播放,切换不了音轨"
        );
        let total = cur.tracks.len();
        anyhow::ensure!(total >= 2, "这个文件没有可切换的音轨(读到 {total} 条)");
        anyhow::ensure!(
            (1..=total).contains(&n),
            "一共 {total} 条音轨({}),没有第 {n} 条",
            track_menu(&cur.tracks)
        );
        let idx = n - 1;
        let prev = *self.inner.audio_track.lock().unwrap();
        if idx == prev {
            return Ok(format!("已经在第 {n} 条音轨({})了", track_desc(&cur.tracks[idx], n)));
        }
        *self.inner.audio_track.lock().unwrap() = idx;
        // 统一走「重建 + 原位续播」(mac 直传也一样):真机实锤 WKWebView **播放中**改
        // audioTracks.enabled 不重新路由音频(静音且切回也不恢复);loadedmetadata 时的收敛
        // 有效 → 重载后由起播收敛把新轨启起来,本地文件重载亚秒级,与 Windows 同一条路。
        let resume = self.current_position();
        let pos = self.inner.playlist.lock().unwrap().as_ref().map(|pl| PlaylistPos {
            index: pl.index,
            total: pl.entries.len(),
            resumed: false,
        });
        if let Err(e) = self.play_local(&cur.page_url, cur.audio_only, pos, true, resume).await {
            // 重建失败:选择回滚(老管线还在播旧轨,状态别悬空指向没生效的轨)
            *self.inner.audio_track.lock().unwrap() = prev;
            return Err(e);
        }
        Ok(format!(
            "已切到第 {n} 条音轨({}),从刚才的位置接着放",
            track_desc(&cur.tracks[idx], n)
        ))
    }

    /// 循环/随机模式镜像(NowPlaying 每次捎带全量,前端以此对齐 el.loop/按钮态,零猜测)。
    fn mode_flags(&self) -> (String, bool) {
        let loop_mode = self.inner.loop_mode.lock().unwrap().as_str().to_string();
        let shuffle = self.inner.playlist.lock().unwrap().as_ref().is_some_and(|p| p.shuffle);
        (loop_mode, shuffle)
    }

    /// 起播时乐观 seed「正在放」(前端随后经 report 校准;这步只是让模型立刻就知道在放什么)。
    /// `pos` = (index, total):在播剧集时把「第N/共M集」一并记下,喂模型「此刻」背景。
    /// 音量跨播放粘住(前端基准如此)→ seed 保留旧值;进度/倍速是新内容的事,清零等回报。
    fn seed_playing(&self, title: &str, pos: Option<(usize, usize)>) {
        let mut guard = self.inner.playback.lock().unwrap();
        let volume_pct = guard.volume_pct;
        *guard = Playback {
            title: Some(title.to_string()),
            paused: false,
            pos,
            volume_pct,
            ..Playback::default()
        };
    }

    /// 前端回报播放态(`report_media_state` 命令 → 这里):前端是播放真相源,
    /// ended/stop/pause/resume/音量/倍速/seek/心跳全经此校准 core 快照。
    /// 集数位置(pos)由 core 起播/切集时 seed,前端回报不带 → 这里**保留**已有 pos;
    /// 音量也粘住(idle 不清,与前端「跨播放粘住」一致);其余 idle 清空。
    pub fn set_playback(&self, r: PlaybackReport) {
        let mut guard = self.inner.playback.lock().unwrap();
        let volume_pct =
            r.volume.map(|v| v.clamp(0.0, 100.0).round() as u8).or(guard.volume_pct);
        *guard = match r.status.as_str() {
            "idle" => Playback { volume_pct, ..Playback::default() },
            // paused 只认显式;playing / loading / 其它 → 正在播
            s => Playback {
                title: r.title,
                paused: s == "paused",
                pos: guard.pos,
                volume_pct,
                position_secs: r.position,
                duration_secs: r.duration.filter(|d| *d > 0.0),
                rate: r.rate,
                at: Some(std::time::Instant::now()),
            },
        };
    }

    /// 「此刻」播放器状态的一行背景:回合装配追加到末条 user 喂模型,让它任何时候都拿得到
    /// 当下真相(修「歌放完了却以为还在播」)。总返回一行(含空闲),由提示词法条约束「只在
    /// 跟播放有关时才参考、平时别主动提」。在播剧集时带「第N/共M集」;有回报时带**进度/音量/
    /// 倍速** —— 模型据此才能「音量调到 50」「快进 5 分钟」(相对操作 = 自己按当前值算绝对值)。
    pub fn playback_summary(&self) -> Option<String> {
        let pb = self.inner.playback.lock().unwrap().clone();
        // 剧集补一段「(第N集/共M集)」,让模型知道进度(如被问"放到哪了""下一集")。
        let ep = pb
            .pos
            .map(|(i, n)| format!("(第{}集/共{n}集)", i + 1))
            .unwrap_or_default();
        // 进度 = 最近回报值,播放中按倍速外推到「此刻」(回报之间也准);夹到总长。
        let progress = pb
            .position_secs
            .map(|p| {
                let mut cur = p;
                if !pb.paused {
                    if let Some(at) = pb.at {
                        cur += at.elapsed().as_secs_f64() * pb.rate.unwrap_or(1.0);
                    }
                }
                match pb.duration_secs {
                    Some(d) => format!(",进度 {}/{}", fmt_clock(cur.min(d)), fmt_clock(d)),
                    None => format!(",已播到 {}", fmt_clock(cur)),
                }
            })
            .unwrap_or_default();
        let vol = pb.volume_pct.map(|v| format!(",音量 {v}%")).unwrap_or_default();
        let rate = pb
            .rate
            .filter(|r| (*r - 1.0).abs() > 0.011)
            .map(|r| format!(",{r} 倍速"))
            .unwrap_or_default();
        // 多音轨清单:模型据此把「换英文/国语」对到轨号(media_control 的 audio_track)。
        let audio = {
            let sel = *self.inner.audio_track.lock().unwrap();
            self.inner
                .current_local
                .lock()
                .unwrap()
                .as_ref()
                .filter(|c| c.tracks.len() >= 2)
                .map(|c| {
                    format!(",音轨 {}/{}(可选: {})", sel + 1, c.tracks.len(), track_menu(&c.tracks))
                })
                .unwrap_or_default()
        };
        // 循环/随机标记:模型据此答「现在是循环吗」、对「别循环了/换随机」给对动作。
        let mode = {
            let lm = *self.inner.loop_mode.lock().unwrap();
            let sh = self.inner.playlist.lock().unwrap().as_ref().is_some_and(|p| p.shuffle);
            format!(
                "{}{}",
                match lm {
                    LoopMode::One => ",单曲循环中",
                    LoopMode::All => ",列表循环中",
                    LoopMode::Off => "",
                },
                if sh { ",随机播放中" } else { "" }
            )
        };
        Some(match (pb.title, pb.paused) {
            (None, _) => "播放器现在空闲,没有在播放任何内容".to_string(),
            (Some(t), false) => format!("播放器正在播放《{t}》{ep}{progress}{vol}{rate}{mode}{audio}"),
            (Some(t), true) => format!("播放器已暂停,停在《{t}》{ep}{progress}{vol}{rate}{mode}{audio}"),
        })
    }
}

/// 一条音轨的短描述(喂模型/观察文本):标题 > 语言码 > 「音轨N」。
fn track_desc(t: &probe::AudioTrack, n: usize) -> String {
    match (&t.title, &t.lang) {
        (Some(ti), _) => ti.clone(),
        (None, Some(l)) => l.clone(),
        _ => format!("音轨{n}"),
    }
}

/// 音轨清单话术:`1=chi 2=eng`(〔此刻〕与报错共用;模型按语言挑轨号)。
fn track_menu(tracks: &[probe::AudioTrack]) -> String {
    tracks
        .iter()
        .enumerate()
        .map(|(i, t)| format!("{}={}", i + 1, track_desc(t, i + 1)))
        .collect::<Vec<_>>()
        .join(" ")
}

/// 秒 → 「M:SS」/「H:MM:SS」钟表格式(喂模型的进度表示;比裸秒数好读好算)。
fn fmt_clock(secs: f64) -> String {
    let s = secs.max(0.0).round() as u64;
    let (h, m, sec) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{sec:02}")
    } else {
        format!("{m}:{sec:02}")
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
    // 目录入参(音频文件夹,`play()` 已归一):整夹音频即队列,起点 = 排序后第一首
    //(build_queue 按 url 匹配不到目录 → requested=0 = 自然起点,续播规则照常适用)。
    if current.is_dir() {
        return audio_folder_queue(current);
    }
    let parent = current.parent()?;
    let cur_name = current.file_name()?.to_str()?;
    // 当前文件的桶:队列只收同桶文件(放视频不混进音频,反之亦然)。
    let want_video = probe::is_video_ext(current);
    if !want_video && !probe::is_audio_ext(current) {
        return None; // 未知类型,不组队列
    }
    // 音频:同文件夹全部音频 = 一个队列(歌单/专辑/故事集心智,连播是常识)。下面的数字骨架
    // 闸只适合视频(防平铺电影库误触)——歌名天然各异,套用它 = 永远单曲放完就停(2026-07-21 修)。
    if !want_video {
        return audio_folder_queue(parent);
    }

    let cur_skel = digit_skeleton(file_stem_str(cur_name));
    let mut group: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(parent).ok()? {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if !path.is_file() || !probe::is_video_ext(&path) {
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
    let key_material = format!("{}\u{1f}{}", parent.to_string_lossy().to_lowercase(), cur_skel);
    Some((format!("local:{}", fnv1a_hex(&key_material)), folder_entries(parent, &group)))
}

/// 目录判定:本地路径且真是目录(非本地/不存在都算否;fs 探一次,亚毫秒)。
fn is_dir_path(s: &str) -> bool {
    is_local_path(s) && std::path::Path::new(s).is_dir()
}

/// 随机播放的下一首:从「这轮没放过的」里挑,命中即记进履历。都放过时,
/// wrap = true(列表循环 / 用户点名「下一首」)→ 重开一轮(排除当前,免紧挨着重复;
/// 队列只剩一首时退化为重放),否则 None(这一轮放完了)。seed 注入可测(§4.11 免不了随机,
/// 但挑选逻辑本身是纯函数)。
fn shuffle_advance(pl: &mut Playlist, wrap: bool, seed: u64) -> Option<usize> {
    let total = pl.entries.len();
    let mut candidates: Vec<usize> = (0..total).filter(|i| !pl.played.contains(i)).collect();
    if candidates.is_empty() {
        if !wrap {
            return None;
        }
        pl.played = vec![pl.index]; // 重开一轮:当前这首不马上重复
        candidates = (0..total).filter(|&i| i != pl.index).collect();
        if candidates.is_empty() {
            candidates.push(pl.index); // 队列只有一首:只能重放它
        }
    }
    let pick = candidates[(xorshift64(seed) as usize) % candidates.len()];
    pl.played.push(pick);
    Some(pick)
}

/// 随机播放的「上一首」:弹掉当前、回到履历上一首。没有更早的 = None(如实报,不瞎跳)。
fn shuffle_back(pl: &mut Playlist) -> Option<usize> {
    if pl.played.len() < 2 {
        return None;
    }
    pl.played.pop();
    pl.played.last().copied()
}

/// 挑歌用的种子(时间戳;无需密码学质量)。
fn shuffle_seed() -> u64 {
    crate::store::now_ms() as u64
}

/// 一步 xorshift 伪随机(0 种子防呆)。
fn xorshift64(seed: u64) -> u64 {
    let mut s = seed | 1;
    s ^= s << 13;
    s ^= s >> 7;
    s ^= s << 17;
    s
}

/// 文件夹里全部音频文件名(natural sort;子目录/非音频跳过)。读不了 = 空。
fn audio_folder_files(dir: &std::path::Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut group: Vec<String> = Vec::new();
    for entry in rd.flatten() {
        let path = entry.path();
        if !path.is_file() || !probe::is_audio_ext(&path) {
            continue;
        }
        if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
            group.push(name.to_string());
        }
    }
    group.sort_by(|a, b| natural_cmp(a, b));
    group
}

/// 音频整夹队列:<2 首不成队列(单曲不出集数 UI)。series_key 只认「哪个文件夹」
/// (目录 + audio 桶标记)——从任一首进、或直接给文件夹,都是同一个 key → 续播记录共享。
/// (原音频骨架 key 的老续播记录〔有声书章节类〕一次性失联,之后照常;拍板可接受。)
fn audio_folder_queue(dir: &std::path::Path) -> Option<(String, Vec<EpisodeRef>)> {
    let group = audio_folder_files(dir);
    if group.len() < 2 {
        return None;
    }
    let key_material = format!("{}\u{1f}audio", dir.to_string_lossy().to_lowercase());
    Some((format!("local:{}", fnv1a_hex(&key_material)), folder_entries(dir, &group)))
}

/// 文件名列表 → 队列条目(id = 相对文件名〔续播记忆存它,绝不落绝对路径 §6.2〕,url = 绝对路径)。
fn folder_entries(dir: &std::path::Path, names: &[String]) -> Vec<EpisodeRef> {
    names
        .iter()
        .map(|name| EpisodeRef {
            id: name.clone(),
            url: dir.join(name).to_string_lossy().into_owned(),
            title: file_stem_str(name).to_string(),
        })
        .collect()
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
        rt.control("volume", Some(50.0)).unwrap();
        assert!(rt.control("blast_off", None).is_err(), "未知动作被拒");
        assert!(rt.control("speed", None).is_err(), "speed 缺 value 被拒");
        assert!(rt.control("speed", Some(9.0)).is_err(), "倍速超界被拒");
        assert!(rt.control("seek", Some(-3.0)).is_err(), "负秒数被拒");
        assert!(rt.control("volume", None).is_err(), "volume 缺 value 被拒");
        assert!(rt.control("volume", Some(120.0)).is_err(), "音量超 0–100 被拒");
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

    #[test]
    fn audio_folder_groups_whole_folder_and_dir_input() {
        let dir = std::env::temp_dir().join(format!("lw-ep-audio-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // 歌名各异(骨架各不同)也要整夹组队 —— 这正是音频不吃视频骨架闸的原因
        let a = touch(&dir, "小星星.mp3");
        let b = touch(&dir, "两只老虎.flac");
        touch(&dir, "说明.txt"); // 非媒体,过滤
        touch(&dir, "短片.mp4"); // 视频不混进音频桶

        let (key_a, eps) = local_episodes(&a).expect("整夹音频应组队");
        assert_eq!(eps.len(), 2, "只收音频");
        assert!(eps.iter().all(|e| e.id.ends_with(".mp3") || e.id.ends_with(".flac")));
        assert!(!eps[0].id.contains('/') && !eps[0].id.contains('\\'), "id 是相对名");
        // 从另一首进、或直接给文件夹:同一个 key + 同一份队列(续播记录共享)
        let (key_b, _) = local_episodes(&b).unwrap();
        let (key_dir, eps_dir) = local_episodes(&dir).unwrap();
        assert_eq!(key_a, key_b);
        assert_eq!(key_a, key_dir);
        assert_eq!(eps.len(), eps_dir.len());

        // 只有一首:不成队列(文件与目录入口一致)
        let solo = std::env::temp_dir().join(format!("lw-ep-audio-solo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&solo);
        std::fs::create_dir_all(&solo).unwrap();
        let only = touch(&solo, "独一首.mp3");
        assert!(local_episodes(&only).is_none(), "单曲 → 不出队列");
        assert!(local_episodes(&solo).is_none());
    }

    fn mk_shuffle_playlist(n: usize) -> Playlist {
        Playlist {
            series_key: "local:test".into(),
            entries: (0..n)
                .map(|i| EpisodeRef {
                    id: format!("{i}.mp3"),
                    url: format!("/x/{i}.mp3"),
                    title: format!("{i}"),
                })
                .collect(),
            index: 0,
            audio_only: true,
            shuffle: true,
            played: vec![0],
        }
    }

    #[test]
    fn shuffle_advance_picks_unplayed_then_round_ends() {
        let mut pl = mk_shuffle_playlist(4);
        for seed in 1..=3u64 {
            let pick = shuffle_advance(&mut pl, false, seed).expect("这轮还有没放过的");
            assert_eq!(pl.played.iter().filter(|&&i| i == pick).count(), 1, "一轮内不重复");
        }
        assert_eq!(pl.played.len(), 4, "一轮放完");
        // 都放过:不循环 → 收尾;列表循环 → 重开一轮且不紧挨着重复当前
        assert!(shuffle_advance(&mut pl, false, 7).is_none());
        pl.index = 2;
        let again = shuffle_advance(&mut pl, true, 7).expect("循环重开一轮");
        assert_ne!(again, 2, "重开一轮不紧挨着重复当前那首");
        // 队列只有一首:重开只能重放它
        let mut one = mk_shuffle_playlist(1);
        assert_eq!(shuffle_advance(&mut one, true, 3), Some(0));
    }

    #[test]
    fn shuffle_back_walks_history() {
        let mut pl = mk_shuffle_playlist(3);
        pl.played = vec![2, 0, 1]; // 放过 2→0→1,当前 1
        assert_eq!(shuffle_back(&mut pl), Some(0));
        assert_eq!(shuffle_back(&mut pl), Some(2));
        assert_eq!(shuffle_back(&mut pl), None, "没有更早的了");
    }

    #[test]
    fn mode_controls_need_right_context_and_show_in_summary() {
        let (rt, _rx) = runtime("mode-ctl");
        assert!(rt.control("shuffle_on", None).is_err(), "没队列不能开随机");
        rt.control("shuffle_off", None).unwrap(); // 关随机幂等不吵
        rt.control("loop_one", None).unwrap(); // 循环不需要队列(单曲循环)
        rt.set_playback(PlaybackReport {
            status: "playing".into(),
            title: Some("小星星".into()),
            ..Default::default()
        });
        assert!(rt.playback_summary().unwrap().contains("单曲循环中"));
        rt.control("loop_all", None).unwrap();
        assert!(rt.playback_summary().unwrap().contains("列表循环中"));
        rt.control("loop_off", None).unwrap();
        assert!(!rt.playback_summary().unwrap().contains("循环中"));
    }

    #[tokio::test]
    async fn set_audio_track_validation_and_rollback() {
        let (rt, mut rx) = runtime("atrack");
        assert!(rt.set_audio_track(2).await.is_err(), "没在放本地内容如实退回");
        // 注入现场:直传双音轨(chi/eng)
        *rt.inner.current_local.lock().unwrap() = Some(CurrentLocal {
            page_url: "/x/双语片.mp4".into(),
            audio_only: false,
            tracks: vec![
                probe::AudioTrack { codec: "ac-3".into(), lang: Some("chi".into()), title: None },
                probe::AudioTrack { codec: "ac-3".into(), lang: Some("eng".into()), title: None },
            ],
        });
        assert!(rt.set_audio_track(2).await.is_err(), "没在播放(playback 空闲)也退回");
        rt.set_playback(PlaybackReport {
            status: "playing".into(),
            title: Some("双语片".into()),
            ..Default::default()
        });
        let err = rt.set_audio_track(9).await.unwrap_err().to_string();
        assert!(err.contains("一共 2 条"), "越界报错列清单: {err}");
        assert!(rt.set_audio_track(1).await.unwrap().contains("已经在"), "同轨幂等");
        // 「此刻」背景带音轨清单
        assert!(rt.playback_summary().unwrap().contains("音轨 1/2"), "{:?}", rt.playback_summary());
        // 切轨一律重建管线(mac 直传也不例外 —— WKWebView 播放中启停音轨不重路由,真机实锤);
        // 这里文件不存在 → 如实报错且选择回滚(真切换在 e2e/真机验)
        let _ = &mut rx;
        assert!(rt.set_audio_track(2).await.is_err());
        assert_eq!(*rt.inner.audio_track.lock().unwrap(), 0, "失败回滚选择");
    }

    #[tokio::test]
    async fn play_dir_input_edges() {
        let (rt, _rx) = runtime("audio-dir-edge");
        // 没有音频的文件夹:如实退回
        let dir = std::env::temp_dir().join(format!("lw-audio-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        touch(&dir, "唯一.mp4");
        let err =
            rt.play(1, &dir.to_string_lossy(), false, false).await.unwrap_err().to_string();
        assert!(err.contains("没有能播放的音频"), "空音频文件夹如实退回: {err}");
        // 恰一首:退化成放那一首(单曲,无队列,仍强制只出声)
        let solo = std::env::temp_dir().join(format!("lw-audio-one-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&solo);
        std::fs::create_dir_all(&solo).unwrap();
        touch(&solo, "独一首.mp3");
        match rt.play(1, &solo.to_string_lossy(), false, false).await.unwrap() {
            PlayOutcome::Playing(np) => {
                assert!(matches!(np.kind, MediaKind::Audio), "目录入参强制只出声");
                assert!(np.playlist.is_none(), "单曲不出队列");
                assert_eq!(np.title, "独一首");
            }
            other => panic!("应为 Playing,实际 {other:?}"),
        }
    }

    #[tokio::test]
    async fn audio_autonext_loop_and_shuffle_flow() {
        let (rt, _rx) = runtime("audio-queue");
        let dir = std::env::temp_dir().join(format!("lw-audio-play-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for n in ["a.mp3", "b.mp3", "c.mp3"] {
            touch(&dir, n);
        }
        let plist = |o: PlayOutcome| match o {
            PlayOutcome::Playing(np) => np,
            other => panic!("应为 Playing,实际 {other:?}"),
        };

        // 目录入参:整夹组队、强制只出声、从第一首起;循环镜像随 Play 捎带
        let np = plist(rt.play(1, &dir.to_string_lossy(), false, false).await.unwrap());
        assert!(matches!(np.kind, MediaKind::Audio));
        let pos = np.playlist.expect("整夹应组队");
        assert_eq!((pos.index, pos.total), (0, 3));
        assert_eq!(np.loop_mode, "off");
        assert!(!np.shuffle);

        // 顺序自动续播:0→1→2;末首放完且不循环 → 交回前端收尾
        let np = plist(rt.auto_next(1).await.unwrap().expect("有下一首"));
        assert_eq!(np.playlist.unwrap().index, 1);
        assert_eq!(plist(rt.auto_next(1).await.unwrap().unwrap()).playlist.unwrap().index, 2);
        assert!(rt.auto_next(1).await.unwrap().is_none(), "末首且不循环 → 收尾");

        // 列表循环:auto_next 回卷到第一首;嘴控到顶/到头也回卷不报错
        rt.control("loop_all", None).unwrap();
        let np = plist(rt.auto_next(1).await.unwrap().expect("列表循环回卷"));
        assert_eq!(np.playlist.unwrap().index, 0);
        assert_eq!(np.loop_mode, "all", "镜像随 Play 捎带");
        let np = plist(rt.advance(1, -1).await.unwrap());
        assert_eq!(np.playlist.unwrap().index, 2, "循环开着,到顶回卷");

        // 随机:开启后 auto_next 挑「这轮没放过的」,一轮内不重复
        rt.control("shuffle_on", None).unwrap();
        let mut seen = vec![2usize]; // 当前第 3 首(index 2),新一轮履历从它起算
        for _ in 0..2 {
            let np = plist(rt.auto_next(1).await.unwrap().expect("随机还有没放过的"));
            let i = np.playlist.unwrap().index;
            assert!(!seen.contains(&i), "随机一轮内不重复,已放 {seen:?} 又放 {i}");
            assert!(np.shuffle, "随机镜像随 Play 捎带");
            seen.push(i);
        }
        // 一轮放完:列表循环开着 → 重开一轮接着放;关掉循环把余下放完 → 收尾
        assert!(rt.auto_next(1).await.unwrap().is_some(), "循环+随机:放完一轮重开");
        rt.control("loop_off", None).unwrap();
        assert!(rt.auto_next(1).await.unwrap().is_some(), "这轮还剩一首");
        assert!(rt.auto_next(1).await.unwrap().is_none(), "随机放完一轮且不循环 → 收尾");

        // 新播放请求复位循环(音量粘住、循环不粘)
        let np = plist(rt.play(1, &dir.to_string_lossy(), false, true).await.unwrap());
        assert_eq!(np.loop_mode, "off", "新 play() 复位循环");
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

        // 第 N 集绝对定位(嘴控「看第一集」= 1 起数):跳到第1集 → index 0
        let pos = plist(rt.jump_to_episode(1, 1).await.unwrap());
        assert_eq!((pos.index, pos.total), (0, 3));
        // 跳到当前集 = 从头重放该集(不报错);越界/0 集如实报错
        assert_eq!(plist(rt.jump_to_episode(1, 1).await.unwrap()).index, 0);
        let err = rt.jump_to_episode(1, 9).await.unwrap_err().to_string();
        assert!(err.contains("一共 3 集"), "越界报错要说清共几集: {err}");
        assert!(rt.jump_to_episode(1, 0).await.is_err(), "第 0 集(1 起数)被拒");
        // 跳集也落续播进度:重放(自然起点)接到刚跳的第1集 → resumed=false(本就是首集)
        let pos = plist(rt.play(1, &e1, false, false).await.unwrap());
        assert_eq!((pos.index, pos.resumed), (0, false), "进度已被 jump 更新到第1集");

        // 没有队列时 advance / jump 都报错(没在放剧集)
        let (rt2, _rx2) = runtime("noqueue");
        assert!(rt2.advance(1, 1).await.is_err());
        assert!(rt2.jump_to_episode(1, 2).await.is_err());
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

    /// 造一份前端回报(测试便捷形;新字段默认缺 = 老形回报)。
    fn report(status: &str, title: Option<&str>) -> PlaybackReport {
        PlaybackReport {
            status: status.into(),
            title: title.map(str::to_string),
            ..PlaybackReport::default()
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
        rt.set_playback(report("paused", Some("天空之城")));
        assert_eq!(rt.playback_summary().as_deref(), Some("播放器已暂停,停在《天空之城》"));

        // 剧集:seed 带「第N/共M集」,且前端回报 playing 不丢集数位置
        rt.seed_playing("海底小纵队", Some((2, 12)));
        assert_eq!(
            rt.playback_summary().as_deref(),
            Some("播放器正在播放《海底小纵队》(第3集/共12集)")
        );
        rt.set_playback(report("playing", Some("海底小纵队")));
        assert_eq!(
            rt.playback_summary().as_deref(),
            Some("播放器正在播放《海底小纵队》(第3集/共12集)"),
            "前端回报不带集数 → 保留 seed 的位置"
        );

        // 前端回报 ended/stop → 空闲(修「歌放完了模型却以为还在播」),集数一并清
        rt.set_playback(report("idle", None));
        assert_eq!(
            rt.playback_summary().as_deref(),
            Some("播放器现在空闲,没有在播放任何内容")
        );
    }

    /// 富回报(音量/进度/倍速)进「此刻」摘要;暂停不外推、播放外推;音量跨播放/空闲粘住。
    #[test]
    fn playback_summary_carries_volume_progress_and_rate() {
        let (rt, _rx) = runtime("playback-rich");
        // 暂停态:进度用回报原值(不外推),音量/倍速如实标注
        rt.set_playback(PlaybackReport {
            status: "paused".into(),
            title: Some("天空之城".into()),
            volume: Some(40.0),
            position: Some(83.0),
            duration: Some(7083.0),
            rate: Some(1.5),
        });
        assert_eq!(
            rt.playback_summary().as_deref(),
            Some("播放器已暂停,停在《天空之城》,进度 1:23/1:58:03,音量 40%,1.5 倍速")
        );
        // 播放态:外推(elapsed≈0,仍是 1:23 量级);1 倍速不标注
        rt.set_playback(PlaybackReport {
            status: "playing".into(),
            title: Some("天空之城".into()),
            volume: Some(40.0),
            position: Some(83.0),
            duration: Some(7083.0),
            rate: Some(1.0),
        });
        let s = rt.playback_summary().unwrap();
        assert!(s.contains("进度 1:23/1:58:03"), "刚回报完外推≈0: {s}");
        assert!(s.contains("音量 40%") && !s.contains("倍速"), "1 倍速不标注: {s}");
        // 音量粘住:idle 清进度不清音量;下次 seed(新播放)也保留
        rt.set_playback(report("idle", None));
        assert_eq!(
            rt.playback_summary().as_deref(),
            Some("播放器现在空闲,没有在播放任何内容")
        );
        rt.seed_playing("新歌", None);
        assert_eq!(
            rt.playback_summary().as_deref(),
            Some("播放器正在播放《新歌》,音量 40%"),
            "音量跨播放粘住(前端基准语义),进度等回报"
        );
    }

    #[test]
    fn fmt_clock_formats() {
        assert_eq!(fmt_clock(0.0), "0:00");
        assert_eq!(fmt_clock(83.4), "1:23");
        assert_eq!(fmt_clock(3600.0), "1:00:00");
        assert_eq!(fmt_clock(7083.0), "1:58:03");
        assert_eq!(fmt_clock(-5.0), "0:00", "负数夹到 0");
    }
}
