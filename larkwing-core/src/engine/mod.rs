//! Core = 对话编排器(宪法 §5)。store 与 llm 的唯一合流点。

mod consolidate;
mod context;
mod turn;
mod usage;

pub use usage::{DayUsage, MsgStats, UsageDigest};

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use serde::Serialize;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use serde::Deserialize;

use crate::llm::registry::{resolve_env, Protocol, ProviderRegistry, ProviderSpec, Strategy};
use crate::llm::{LlmError, LlmProvider, ToolCall, ToolDef};
use crate::scenes::{Scenes, DEFAULT_SCENE_ID};
use crate::store::{Briefing, Conversation, Memory, Message, SearchHit, Store, User};
use crate::tools::Tools;

// ---------- engine ↔ UI 的词汇表 ----------

/// ≠ llm::ChatEvent(那是 provider ↔ engine 的词汇)。
/// tagged 编码:加变体对前端是增量,未知变体可忽略(给工具进度等未来事件留路)。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum TurnEvent {
    Delta(String),
    Thinking(String),
    /// 工具状态泡:label 是 i18n 键(如 "tool.remember"),文案由前端字典选
    /// (core 不产文案铁规);绝不露工具/agent 概念,只露友好动词。
    ToolUse { label: String, state: ToolUseState },
    /// 记账灯带:本轮消耗 + 今日/会话累计快照(工具回合每轮各发一次;累计直接来自库,
    /// 前端只展示不记账)。provider 没回 usage(严格端点/假流)就不发 —— 不点没数据的灯。
    Usage { round: UsageDigest, today: DayUsage, conv: crate::store::UsageTotals },
    /// 插队(PLAN §9 B):回合在飞时注入的一条 user 消息已落库,之后的回复另起一段。
    /// 前端据此:收尾当前回复气泡 → 插用户气泡 → 开新回复气泡。
    Injected { message_id: i64, text: String, attachments: Vec<AttachmentRef> },
    /// 带文字的工具轮(PLAN §9):这一轮模型既说了话、又要继续调工具,它在落库里是一条独立
    /// assistant 内容行。前端据此把当前回复气泡封口(钉上 message_id 供「想了想」轨迹回挂)、
    /// 另起新泡接后续文字 —— 让在飞气泡结构 = 落库/重启结构(否则 trace 实时挂不上、重启才显)。
    Segment { message_id: i64 },
    /// 带落库 id:前端把流式文本与持久消息对账。
    Done { message_id: i64 },
    Failed { kind: ErrorKind, message: String },
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolUseState {
    Started,
    Finished,
}

// ---------- messages.payload 列的 JSON 形状(engine 私有词汇,store 只存 TEXT) ----------

/// assistant 行:工具轮的 tool_calls + 该轮 reasoning(坑 #4:回放历史时 DeepSeek
/// 要求工具轮附带 reasoning)。纯文本回合不写 payload。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct AssistantPayload {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    /// 不透明 reasoning 状态(原生方言用,逐字保真往返;兼容方言无此项)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_state: Option<serde_json::Value>,
}

/// 'tool' 行:配对主键 + 执行结局。status: ok | error | timeout | cancelled。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ToolRowPayload {
    pub call_id: String,
    pub name: String,
    pub status: String,
}

/// user 行:输入来源与朗读意向(PLAN §11 语音会话模式)。input: typed | mic | wake;
/// speak = 本回合按语音排版并朗读(发送瞬间由 来源×auto_speak 物化——真相在库,
/// 重启/重算都确定)。打字默认形(typed 不念)不写 payload,历史零膨胀。
/// pub:壳层 send_message command 直接反序列化它(IPC 词汇)。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UserMeta {
    #[serde(default)]
    pub input: String,
    #[serde(default)]
    pub speak: bool,
    /// 声纹识别出的说话人(PLAN §11 D):本回合记忆读写归到 TA(记忆归人,§6);
    /// None = 用会话归属者。会话归属与性格设定不受影响(保前缀稳定)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker_user: Option<i64>,
    /// 本回合带过的附件小票(媒体输入 PLAN §9):只存「📷/📄 名字」级指针给 UI 显历史,
    /// 附件本体当轮注入 LLM 后不持久(省 token/体积)。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<AttachmentRef>,
}

/// 持久小票:历史里标「这条带过图/文档」。kind: image | doc。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachmentRef {
    pub kind: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub mime: String,
}

/// 入站附件(IPC 词汇):前端把图/文档读成 base64 随消息送来。当轮处理——图走 image_url,
/// 文档抽文字注入当轮提示;不整体持久(只落 AttachmentRef 小票)。
#[derive(Debug, Clone, Deserialize)]
pub struct InAttachment {
    pub name: String,
    pub mime: String,
    /// 原始字节的 base64(无 data: 前缀)。
    pub data: String,
}

impl UserMeta {
    /// 默认形(打字、不念、无声纹、无附件)不落 payload。
    fn is_default(&self) -> bool {
        !self.speak
            && self.speaker_user.is_none()
            && self.attachments.is_empty()
            && (self.input.is_empty() || self.input == "typed")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    NoApiKey,
    BadApiKey,
    Network,
    Api,
    NotFound,
    Internal,
}

/// command 统一错误:kind 给前端选友好文案,message 进日志(不给普通人看)。
#[derive(Debug, Clone, Serialize)]
pub struct AppError {
    pub kind: ErrorKind,
    pub message: String,
}

/// 「想了想」轨迹的一步(PLAN §9 思考漏出·展开层):一次工具调用的技术细节。
/// ui_key 给折叠摘要兜底;name/args/result/status 是展开后给好奇/专业用户看的真东西。
#[derive(Debug, Clone, Serialize)]
pub struct TraceStep {
    pub name: String,
    pub ui_key: String,
    pub args: String,
    pub result: String,
    pub status: String,
}

/// 一回合的「想了想」轨迹:贴在该回合代表气泡上。折叠药丸只露「想了想 · N 步」(§3 干净默认);
/// 展开 = 工具名/入参/结果 + CoT 原文(用户拍板:展开是给好奇/专业用户的技术披露,
/// 非专业用户不必点开;§3 铁律2 在折叠层守住,展开层放开一格)。
#[derive(Debug, Clone, Serialize)]
pub struct TurnTrace {
    pub message_id: i64,
    pub steps: Vec<TraceStep>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
}

impl AppError {
    pub fn internal(message: impl ToString) -> Self {
        AppError { kind: ErrorKind::Internal, message: message.to_string() }
    }
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for AppError {}

impl From<LlmError> for AppError {
    fn from(e: LlmError) -> Self {
        let kind = match &e {
            LlmError::NoApiKey => ErrorKind::NoApiKey,
            LlmError::BadApiKey => ErrorKind::BadApiKey,
            LlmError::Network(_) => ErrorKind::Network,
            LlmError::Api { .. } => ErrorKind::Api,
        };
        AppError { kind, message: e.to_string() }
    }
}

impl From<anyhow::Error> for AppError {
    fn from(e: anyhow::Error) -> Self {
        AppError::internal(format!("{e:#}"))
    }
}

/// app 级设置里允许过桥给前端的 key —— 含钥匙的(llm.api_key / llm.providers)永不在列。
const APP_SETTING_KEYS: &[&str] = &[
    "llm.strategy",
    "llm.thinking",
    "voice.input_device",
    "voice.wake.enabled",
    "voice.wake.keywords",
    "voice.wake.sensitivity",
    "voice.asr.model",
    "voice.tts_backend",
    "weather.qweather.host",
    "weather.qweather.project_id",
    "weather.qweather.credential_id",
    "net.proxy",
    "net.proxy_enabled",
    "memory.auto_consolidate",
];

/// 语音的用户级设置(PLAN §11 逐键放行,不开 voice.* 通配——同前缀跨两个 scope)。
const VOICE_USER_KEYS: &[&str] =
    &["voice.speaker", "voice.auto_speak", "voice.rate", "voice.patience", "voice.volume"];

/// 内置预设:不可删、只可禁用;列表里永远露出(模板预填,用户按需改)。
const BUILTIN_PROVIDER_IDS: &[&str] = &["deepseek", "anthropic"];

/// 供应商卡片视图。钥匙永不明文过桥:`${ENV}` 引用原样展示(它不是秘密),
/// 明文只回尾 4 位掩码;key_set 看的是解析后的真值(引用挂空变量 = 没钥匙)。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderView {
    pub id: String,
    pub name: String,
    pub protocol: String,
    pub base_url: String,
    pub model: String,
    pub enabled: bool,
    pub builtin: bool,
    pub key_masked: String,
    pub key_set: bool,
}

impl ProviderView {
    fn from_spec(spec: &ProviderSpec) -> Self {
        let raw = spec.api_key.trim();
        let key_set = !resolve_env(raw).trim().is_empty();
        let key_masked = if raw.is_empty() {
            String::new()
        } else if raw.contains("${") {
            raw.to_string()
        } else {
            let tail: String = raw.chars().skip(raw.chars().count().saturating_sub(4)).collect();
            format!("····{tail}")
        };
        ProviderView {
            id: spec.id.clone(),
            name: spec.name.clone(),
            protocol: spec.protocol.as_str().into(),
            base_url: spec.base_url.clone(),
            model: spec.model.clone(),
            enabled: spec.enabled,
            builtin: BUILTIN_PROVIDER_IDS.contains(&spec.id.as_str()),
            key_masked,
            key_set,
        }
    }
}

/// 保存供应商卡的入参:None = 不动;api_key 空串视同 None(掩码回显防误存)。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderPatch {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub protocol: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub api_key: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SettingEntry {
    /// "app" | "user"
    pub scope: String,
    pub key: String,
    pub value: String,
}

// ---------- 首屏快照 ----------

/// §7「开窗秒显」的落点:一个 IPC 来回画出首屏。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BootSnapshot {
    pub user: User,
    pub conversation: Conversation,
    pub messages: Vec<Message>,
    pub has_api_key: bool,
    /// 会话还没消息时的场景开场白(引导式上手)。
    pub opening_line: Option<String>,
    /// 用户级语言设置(与皮肤同款,settings scope=user);文案由前端按它选,core 不产文案。
    pub locale: String,
}

/// 悬浮窗待机轮播的"环境信息"(PLAN §12,只读):时间归 OS、余额/今日花费复用现成命令,
/// 这里只补 OS 给不了的两项 —— 下个提醒、最近一句旺财说的话。字段 snake_case(同 DayUsage)。
#[derive(Debug, Clone, Serialize)]
pub struct FloatIdle {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_reminder: Option<FloatReminder>,
    /// 最近一句旺财说的话(已过滤工具轮空串 / __IGNORE__);None = 还没说过。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_line: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FloatReminder {
    pub content: String,
    /// unix 毫秒(本地时区);前端按它显示"还有多久 / 几点"。
    pub due_at: i64,
}

// ---------- 瞬态状态分层:session 槽 ----------

struct TurnHandle {
    token: CancellationToken,
    join: tokio::task::JoinHandle<()>,
}

/// 每隔这么多用户回合,后台自动提炼一次记忆(PLAN §13 Phase 3 自动触发)。
/// 偏稀:蒸馏 = 花钱的 LLM 调用,且 lookback 50 条本就覆盖多轮 → 稀一点、重叠靠去重兜。
const CONSOLIDATE_EVERY_TURNS: u32 = 12;

/// 会话槽:只装派生/瞬态状态。真相永远在库;丢槽 = 重算,绝不 = 出错。
#[derive(Default)]
struct SessionSlot {
    inflight: Option<TurnHandle>,
    /// 插队(PLAN §9 B):在飞回合的注入队列;回合在轮间/收尾前排空它。
    inject: Option<Arc<Mutex<InjectState>>>,
    /// 自上次记忆提炼以来的用户回合数(§13 Phase 3);到阈值归零并后台蒸馏。
    /// 瞬态:丢槽 = 从 0 重数,顶多晚一次提炼(尽力件 + 去重兜底),绝不出错。
    turns_since_consolidate: u32,
    /// 提炼在飞标志(防并发重复落库);spawn 出去的任务持同一 Arc,跑完置回 false。
    consolidating: Arc<AtomicBool>,
    // 以后的会话级住户:工具已读标记、稳定前缀缓存、会话内统计(PLAN §4)
}

/// 插队队列状态:回合在飞时被注入的 user 消息缓冲 + 收尾闸(原子防丢)。
#[derive(Default)]
pub(crate) struct InjectState {
    pub buffer: Vec<InjectReady>,
    /// 回合已进入收尾(原子置位):此后 inject 一律拒绝,改由前端起新回合。
    pub finishing: bool,
}

/// 一条注入消息的就绪形(命令侧已处理好附件):display 落库 / llm_content 进 request /
/// parts 是图 / refs 给 Injected 事件 / payload 是落库 UserMeta JSON(默认形 None)。
pub(crate) struct InjectReady {
    pub display: String,
    pub llm_content: String,
    pub parts: Vec<crate::llm::ContentPart>,
    pub refs: Vec<AttachmentRef>,
    pub payload: Option<String>,
}

/// 入站附件 → (图 image_url parts, 文档抽出的文字, 落库小票)。send_message 与插队共用。
fn process_attachments(
    attachments: &[InAttachment],
) -> (Vec<crate::llm::ContentPart>, String, Vec<AttachmentRef>) {
    use base64::Engine as _;
    let mut image_parts = Vec::new();
    let mut doc_text = String::new();
    let mut refs = Vec::new();
    for a in attachments {
        if crate::attach::is_image(&a.mime) {
            image_parts.push(crate::llm::ContentPart::ImageUrl {
                url: format!("data:{};base64,{}", a.mime, a.data),
            });
            refs.push(AttachmentRef { kind: "image".into(), name: a.name.clone(), mime: a.mime.clone() });
            continue;
        }
        let extracted = base64::engine::general_purpose::STANDARD
            .decode(a.data.as_bytes())
            .ok()
            .and_then(|bytes| crate::attach::extract_doc_text(&a.name, &a.mime, &bytes));
        match extracted {
            Some(t) => doc_text.push_str(&format!("\n\n〔附件:{}〕\n{t}", a.name)),
            None => doc_text.push_str(&format!("\n\n〔附件:{}(暂时读不出内容)〕", a.name)),
        }
        refs.push(AttachmentRef { kind: "doc".into(), name: a.name.clone(), mime: a.mime.clone() });
    }
    (image_parts, doc_text, refs)
}

/// 命令侧构造一条注入的就绪形(处理附件 + 物化 meta + 拼 LLM 文本)。
fn build_inject_ready(
    text: String,
    meta: Option<UserMeta>,
    attachments: Vec<InAttachment>,
) -> InjectReady {
    let (parts, doc_text, refs) = process_attachments(&attachments);
    let mut eff_meta = meta.unwrap_or_default();
    eff_meta.attachments = refs.clone();
    let payload = (!eff_meta.is_default()).then(|| serde_json::to_string(&eff_meta).ok()).flatten();
    let llm_content = format!("{text}{doc_text}");
    InjectReady { display: text, llm_content, parts, refs, payload }
}

// ---------- Engine ----------

pub struct Engine {
    store: Store,
    /// 候选供应商,(id, provider),首位主选、其余为建连失败的切换顺序;空 = 未配 key(首跑态)。
    /// Arc<dyn>:静态组合约束的是不动态加载,不禁 dyn 调度。
    llm: RwLock<Vec<(String, Arc<dyn LlmProvider>)>>,
    scenes: Scenes,
    /// 工具注册表(静态组合);场景白名单在 send_message 时筛子集。
    tools: Tools,
    /// 影音运行时(经 ToolCtx 进工具)。
    media: crate::media::MediaRuntime,
    /// 全局事件车道(与 media 同一条):自启回合完成时喊"会话有动静"。
    bus: crate::bus::Bus,
    sessions: Mutex<HashMap<i64, SessionSlot>>,
}

impl Engine {
    /// 测试/示例入口:影音运行时取 detached(事件无人听,组件落临时目录)。
    pub fn new(store: Store, scenes: Scenes) -> Arc<Engine> {
        let media = crate::media::MediaRuntime::detached(store.clone());
        Engine::with_media(store, scenes, media)
    }

    /// 壳层装配入口:Bus 由壳创建并订阅,经 MediaRuntime 带入。
    pub fn with_media(
        store: Store,
        scenes: Scenes,
        media: crate::media::MediaRuntime,
    ) -> Arc<Engine> {
        let tools = Tools::builtin();
        // 内置场景的 few-shot/白名单坏掉 = 编译者错误,开机即炸好过线上 400
        for scene in scenes.all() {
            scene.validate(&tools).expect("内置场景未通过工具校验");
        }
        // 代理总开关落全局(启动即生效;之后 set_setting 改了会刷新)。net 模块不碰
        // store/llm,故解析(读设置 + ${ENV} + env 回落)的合流放在 engine——唯一合流点。
        crate::net::set_proxy(Self::resolve_proxy(&store));
        let bus = media.bus().clone();
        Arc::new(Engine {
            store,
            llm: RwLock::new(Vec::new()),
            scenes,
            tools,
            media,
            bus,
            sessions: Mutex::new(HashMap::new()),
        })
    }

    /// 解析代理:总开关 `net.proxy_enabled` 关 ⇒ 一律直连(连 env 也不读,与界面开关一致);
    /// 开 ⇒ 用 `net.proxy` 地址(过 `${ENV}`),地址空则回落环境变量(net::env_proxy)。
    /// net 模块刻意不碰 store/llm;此处是 store×llm 合流点,故解析放这。
    fn resolve_proxy(store: &Store) -> Option<String> {
        // 总开关:默认关。关 = 直连,地址虽存着也不用(铁律:开关一关一律直连)。
        let enabled =
            store.settings.get(None, "net.proxy_enabled").ok().flatten().as_deref() == Some("1");
        if !enabled {
            return None;
        }
        store
            .settings
            .get(None, "net.proxy")
            .ok()
            .flatten()
            .map(|v| crate::llm::registry::resolve_env(&v).trim().to_string())
            .filter(|v| !v.is_empty())
            .or_else(crate::net::env_proxy)
    }

    /// 回合开始时把「此刻」背景状态追加给模型(通用缝):各源各贡献一行,拼成一条不落库的
    /// 〔此刻 · …〕注记挂到末条 user 消息(持久前缀字节不动 → 前缀缓存不破)。目前只有 media
    /// 播放态(修「歌放完了模型却以为还在播」);以后的进行中任务 / 待触发提醒等在此各 push 一条
    /// 即可,缝不再动。在 build_context **之后**调,因装配闭包在线程池里拿不到 &self。
    fn inject_ambient(&self, request: &mut crate::llm::ChatRequest) {
        let mut lines: Vec<String> = Vec::new();
        if let Some(s) = self.media.playback_summary() {
            lines.push(s);
        }
        if !lines.is_empty() {
            context::attach_ambient(request, &lines.join(";"));
        }
    }

    /// 全局事件车道(测试/调度器观测用)。
    pub fn bus(&self) -> &crate::bus::Bus {
        &self.bus
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    /// 直接注入单 provider(测试 / FakeLlm);None = 清空回首跑态。
    pub fn set_provider(&self, p: Option<Arc<dyn LlmProvider>>) {
        *self.llm.write().expect("llm lock poisoned") = match p {
            Some(p) => vec![("custom".into(), p)],
            None => Vec::new(),
        };
    }

    pub fn has_provider(&self) -> bool {
        !self.llm.read().expect("llm lock poisoned").is_empty()
    }

    /// 从 settings 重建供应商候选(开机装配 / 任何 llm.* 配置变更后调用)。
    /// 解析顺序:LARKWING_FAKE_LLM → llm.providers JSON → 单 DeepSeek 兜底
    /// (key: env DEEPSEEK_API_KEY → llm.api_key)。坏 JSON 走兜底并记日志,绝不让 app 哑掉。
    pub fn reload_providers(&self) -> Result<(), AppError> {
        if std::env::var("LARKWING_FAKE_LLM").ok().as_deref() == Some("1") {
            tracing::info!("LARKWING_FAKE_LLM=1,使用假 provider");
            self.set_provider(Some(Arc::new(crate::llm::fake::FakeLlm::default())));
            return Ok(());
        }
        let registry = self.load_registry()?;
        let strategy = Strategy::parse(
            self.store.settings.get(None, "llm.strategy")?.unwrap_or_default().as_str(),
        );
        let candidates: Vec<(String, Arc<dyn LlmProvider>)> = registry
            .candidates(strategy)
            .into_iter()
            .map(|spec| (spec.id.clone(), spec.build()))
            .collect();
        tracing::info!(
            n = candidates.len(),
            order = ?candidates.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
            "供应商候选已重建"
        );
        *self.llm.write().expect("llm lock poisoned") = candidates;
        Ok(())
    }

    fn load_registry(&self) -> Result<ProviderRegistry, AppError> {
        // 秘密走 keyring(回落 settings),不落 SQLite 明文(§6.3)
        if let Some(json) = crate::secrets::get(&self.store.settings, "llm.providers") {
            match ProviderRegistry::from_json(&json) {
                Ok(reg) => return Ok(reg),
                // 容错铁律:配置坏了降级跑,不让 7274 哑掉
                Err(e) => tracing::error!(err = %e, "llm.providers 解析失败,回落单 DeepSeek"),
            }
        }
        // 单 key 兜底:用户填过的钥匙优先;没填过则挂 ${DEEPSEEK_API_KEY} 引用 ——
        // env 兜底就此变成数据(resolve_env 取值时解析),落盘也不会泄明文。
        let key = crate::secrets::get(&self.store.settings, "llm.api_key")
            .filter(|k| !k.trim().is_empty())
            .unwrap_or_else(|| "${DEEPSEEK_API_KEY}".into());
        Ok(ProviderRegistry::deepseek_only(key))
    }

    /// 写 key 入库并重建候选(不搞热更新魔法)。
    /// 多供应商配置存在时,同步更新其中的 deepseek 条目 —— 两处永远说同一把钥匙。
    pub fn set_api_key(&self, key: &str) -> Result<(), AppError> {
        let key = key.trim();
        if key.is_empty() {
            return Err(AppError { kind: ErrorKind::BadApiKey, message: "key 为空".into() });
        }
        crate::secrets::set(&self.store.settings, "llm.api_key", key).map_err(AppError::internal)?;
        if let Some(json) = crate::secrets::get(&self.store.settings, "llm.providers") {
            if let Ok(reg) = ProviderRegistry::from_json(&json) {
                let mut specs = reg.specs().to_vec();
                match specs.iter_mut().find(|s| s.id == "deepseek") {
                    Some(ds) => {
                        ds.api_key = key.to_string();
                        ds.enabled = true; // 贴钥匙 = 要用它;条目曾被禁用也就地复活
                    }
                    None => specs.push(ProviderSpec::deepseek(key.to_string())),
                }
                crate::secrets::set(
                    &self.store.settings,
                    "llm.providers",
                    &ProviderRegistry::new(specs).to_json(),
                )
                .map_err(AppError::internal)?;
            }
        }
        self.reload_providers()
    }

    pub fn boot(&self) -> Result<BootSnapshot, AppError> {
        let user = self.store.users.ensure_default_user()?;
        let conversation = match self.store.chat.latest_conversation(user.id)? {
            Some(c) => c,
            None => self.store.chat.create_conversation(user.id, DEFAULT_SCENE_ID)?,
        };
        let messages = self.store.chat.recent_messages(conversation.id, 50)?;
        let locale = self
            .store
            .settings
            .get(Some(user.id), "ui.locale")?
            .unwrap_or_else(|| "zh-CN".into());
        let opening_line = if messages.is_empty() {
            self.scenes.get(&conversation.scene_id).map(|s| s.opening_for(&locale))
        } else {
            None
        };
        Ok(BootSnapshot {
            user,
            conversation,
            messages,
            has_api_key: self.has_provider(),
            opening_line,
            locale,
        })
    }

    /// 悬浮窗待机轮播(PLAN §12):只给 OS 没有的东西 —— 下个提醒 + 最近一句旺财说的话。
    /// 只读、轻量(一次取列表首条 + 一句话);余额/今日花费由前端复用 llm_balance/usage_today。
    pub fn float_idle(&self) -> Result<FloatIdle, AppError> {
        let user = self.store.users.ensure_default_user()?;
        let next_reminder = self
            .store
            .jobs
            .list_pending(user.id)?
            .into_iter()
            .next()
            .map(|j| FloatReminder { content: j.content, due_at: j.due_at });
        let latest_line = match self.store.chat.latest_conversation(user.id)? {
            Some(c) => self.store.chat.latest_assistant_line(c.id)?,
            None => None,
        };
        Ok(FloatIdle { next_reminder, latest_line })
    }

    pub fn new_conversation(&self, channel: &str) -> Result<Conversation, AppError> {
        let user = self.store.users.ensure_default_user()?;
        Ok(self.store.chat.create_conversation_full(user.id, DEFAULT_SCENE_ID, channel)?)
    }

    pub fn list_conversations(&self) -> Result<Vec<Conversation>, AppError> {
        let user = self.store.users.ensure_default_user()?;
        Ok(self.store.chat.list_conversations(user.id)?)
    }

    pub fn load_conversation(&self, conv_id: i64) -> Result<Vec<Message>, AppError> {
        Ok(self.store.chat.recent_messages(conv_id, 200)?)
    }

    /// 跨会话搜索当前用户的聊天记录(排除工具 / 系统事件内部行)。最近命中在前。
    pub fn search_messages(&self, query: &str, limit: i64) -> Result<Vec<SearchHit>, AppError> {
        let user = self.store.users.ensure_default_user()?;
        Ok(self.store.chat.search_messages(user.id, query, limit)?)
    }

    /// 先取消在飞 → 级联删消息 → 清会话槽。
    pub async fn delete_conversation(&self, conv_id: i64) -> Result<(), AppError> {
        self.cancel(conv_id).await;
        self.store.chat.delete_conversation(conv_id)?;
        self.sessions.lock().expect("sessions lock poisoned").remove(&conv_id);
        Ok(())
    }

    /// 用户右键重命名会话(无条件覆盖标题;空串交给前端拦,这里只落库)。
    pub fn rename_conversation(&self, conv_id: i64, title: &str) -> Result<(), AppError> {
        Ok(self.store.chat.set_title(conv_id, title)?)
    }

    /// 钉住 / 取消钉住会话(列表排最前 + 📌)。
    pub fn set_conversation_pinned(&self, conv_id: i64, pinned: bool) -> Result<(), AppError> {
        Ok(self.store.chat.set_pinned(conv_id, pinned)?)
    }

    pub fn set_skin(&self, skin_id: &str) -> Result<(), AppError> {
        let user = self.store.users.ensure_default_user()?;
        self.store.users.set_skin(user.id, skin_id)?;
        Ok(())
    }

    /// 当前用户皮肤。给无 boot 快照的窗口(悬浮窗)拉取初值用;主窗已从 boot 拿到。
    pub fn skin(&self) -> Result<String, AppError> {
        Ok(self.store.users.ensure_default_user()?.skin_id)
    }

    /// 确保**全局** Ed25519 身份密钥对存在(没有就生成并落库),返回公钥 PEM 给前端展示/复制。
    /// 幂等:已有直接回存量公钥。私钥(`crypto.ed25519.private_key`)是秘密、永不过桥。所有走
    /// Ed25519-JWT 的服务(和风是首个消费者)共用这一把 ——「整个程序对外一对」。
    pub fn ensure_app_keypair(&self) -> Result<String, AppError> {
        use crate::crypto::{generate_keypair, KEY_ED25519_PRIVATE, KEY_ED25519_PUBLIC};
        if let Some(pubkey) = self.store.settings.get(None, KEY_ED25519_PUBLIC)? {
            if !pubkey.trim().is_empty() {
                return Ok(pubkey);
            }
        }
        let (private_pem, public_pem) = generate_keypair().map_err(|e| AppError {
            kind: ErrorKind::Internal,
            message: format!("生成应用密钥失败:{e}"),
        })?;
        // 私钥进 keyring(秘密、永不过桥);公钥留 settings(非秘密,给用户复制)
        crate::secrets::set(&self.store.settings, KEY_ED25519_PRIVATE, &private_pem)
            .map_err(AppError::internal)?;
        self.store.settings.set(None, KEY_ED25519_PUBLIC, &public_pem)?;
        Ok(public_pem)
    }

    /// 设置页快照:app 级只暴露白名单内的 key(llm.api_key / llm.providers
    /// 这类含钥匙的永不过桥),用户级只暴露 ui.* 前缀。
    pub fn list_settings(&self) -> Result<Vec<SettingEntry>, AppError> {
        let user = self.store.users.ensure_default_user()?;
        let mut out = Vec::new();
        for (key, value) in self.store.settings.list(None)? {
            if APP_SETTING_KEYS.contains(&key.as_str()) {
                out.push(SettingEntry { scope: "app".into(), key, value });
            }
        }
        for (key, value) in self.store.settings.list(Some(user.id))? {
            if key.starts_with("ui.")
                || key == "persona.style"
                || VOICE_USER_KEYS.contains(&key.as_str())
            {
                out.push(SettingEntry { scope: "user".into(), key, value });
            }
        }
        Ok(out)
    }

    /// 写设置:key 决定归属与合法值,白名单之外一律拒绝(PLAN:不开无类型后门)。
    pub fn set_setting(&self, key: &str, value: &str) -> Result<(), AppError> {
        let invalid = |msg: &str| AppError { kind: ErrorKind::Internal, message: msg.into() };
        if value.chars().count() > 200 {
            return Err(invalid("设置值过长"));
        }
        match key {
            "llm.strategy" => {
                if !["thrifty", "balanced", "smart_first"].contains(&value) {
                    return Err(invalid("未知的用脑策略"));
                }
                self.store.settings.set(None, key, value)?;
                self.reload_providers() // 策略变了 = 候选顺序变了
            }
            "llm.thinking" => {
                if !["off", "light", "medium", "heavy"].contains(&value) {
                    return Err(invalid("未知的反应模式档位"));
                }
                self.store.settings.set(None, key, value)?;
                Ok(()) // 每回合取值,无需重建
            }
            // 一句话性格设定(用户级):进稳定前缀的人格覆盖层,改动即生效(下一回合重装配)
            "persona.style" => {
                if value.chars().count() > 500 {
                    return Err(invalid("性格设定最多 500 字"));
                }
                let user = self.store.users.ensure_default_user()?;
                self.store.settings.set(Some(user.id), key, value)?;
                Ok(())
            }
            k if k.starts_with("ui.") => {
                let user = self.store.users.ensure_default_user()?;
                self.store.settings.set(Some(user.id), k, value)?;
                Ok(())
            }
            // 语音(PLAN §11):有枚举的逐键校验;user 级跟人走,app 级是机器属性
            "voice.patience" => {
                if !["snappy", "standard", "relaxed"].contains(&value) {
                    return Err(invalid("未知的耐心档位"));
                }
                let user = self.store.users.ensure_default_user()?;
                self.store.settings.set(Some(user.id), key, value)?;
                Ok(())
            }
            "voice.auto_speak" => {
                if !["follow", "always", "off"].contains(&value) {
                    return Err(invalid("未知的自动朗读档位"));
                }
                let user = self.store.users.ensure_default_user()?;
                self.store.settings.set(Some(user.id), key, value)?;
                Ok(())
            }
            "voice.speaker" | "voice.rate" | "voice.volume" => {
                let user = self.store.users.ensure_default_user()?;
                self.store.settings.set(Some(user.id), key, value)?;
                Ok(())
            }
            "voice.input_device" => {
                self.store.settings.set(None, key, value)?;
                Ok(())
            }
            // 唤醒词(app 级,机器属性):写库即可;开着唤醒时前端会调 voice_wake_set
            // 重启循环让新词生效。voice.wake.enabled 不走这里——开关 = voice_wake_set
            // 一体化入口(写库 + 起停),绕过会出现"库说开着、循环没在跑"的分叉。
            "voice.wake.keywords" => {
                self.store.settings.set(None, key, value)?;
                Ok(())
            }
            // 唤醒灵敏度(app 级,机器属性):0~100 整数 → wake_threshold 映射成 KWS 阈值。
            // 漏了这条 → 写被白名单拒 → 前端乐观写回滚,滑块"一闪一闪"且从不落库(灵敏度其实没生效)。
            // 开着唤醒时前端 saveSensitivity 会重启循环让新阈值生效。
            "voice.wake.sensitivity" => match value.parse::<u32>() {
                Ok(n) if n <= 100 => {
                    self.store.settings.set(None, key, value)?;
                    Ok(())
                }
                _ => Err(invalid("唤醒灵敏度需为 0~100 的整数")),
            },
            "voice.tts_backend" => {
                if !["online", "offline"].contains(&value) {
                    return Err(invalid("未知的语音合成档"));
                }
                self.store.settings.set(None, key, value)?;
                Ok(())
            }
            // 中文 ASR 模型档(app 级,机器属性,2026-06 用户要求放出来选,AGENT §7.5):
            // sense-voice(快,默认)/ whisper-small(对孩子/口音更稳,稍慢,~370MB)/
            // firered-ctc(小红书,中文最准,~740MB)。模型用时下载;开着唤醒时前端会重启
            // 循环让新模型生效(同 sensitivity)。漏了这条 → 写被白名单拒 → 前端乐观写回滚。
            "voice.asr.model" => {
                if !["sense-voice", "whisper-small", "firered-ctc"].contains(&value) {
                    return Err(invalid("未知的识别模型档"));
                }
                self.store.settings.set(None, key, value)?;
                Ok(())
            }
            // 和风 JWT 接入(app 级,非秘密):项目 ID(JWT 的 sub)/凭据 ID(JWT 的 kid),空串 = 清空。
            // 三件套(含 host)齐 + 全局私钥已生成 → 下回合工具现读即切和风源,否则回落 Open-Meteo。
            "weather.qweather.project_id" | "weather.qweather.credential_id" => {
                self.store.settings.set(None, key, value.trim())?;
                Ok(())
            }
            // 和风专属 API Host(app 级,非秘密):空 = 不走和风(老公共域名已停服,无免 host 兜底)。
            // 控制台给的是**裸域名**(xxx.qweatherapi.com),用户多半不带 scheme —— 缺 scheme 自动补
            // https://(和风一律 https),绝不因「没写 http」就拒,否则乐观写回滚,用户「填不进去」。
            "weather.qweather.host" => {
                let v = value.trim();
                let v = if v.is_empty() || v.starts_with("http://") || v.starts_with("https://") {
                    v.to_string()
                } else {
                    format!("https://{v}")
                };
                self.store.settings.set(None, key, &v)?;
                Ok(())
            }
            // 全局代理地址(app 级,PLAN §代理):单独保存、始终保留(用不用看总开关 net.proxy_enabled);
            // 取值 http(s)/socks5(h) 或 ${ENV}(空也允许 = 占位,开关开时回落系统 env);写库后刷新全局 net。
            "net.proxy" => {
                let v = value.trim();
                let ok = v.is_empty()
                    || v.contains("${")
                    || ["http://", "https://", "socks5://", "socks5h://"]
                        .iter()
                        .any(|p| v.starts_with(p));
                if !ok {
                    return Err(invalid("代理地址要以 http(s):// 或 socks5(h):// 开头(或留空)"));
                }
                self.store.settings.set(None, key, v)?;
                crate::net::set_proxy(Self::resolve_proxy(&self.store));
                Ok(())
            }
            // 全局代理总开关(app 级):0/1。关 = 一律直连(地址照存不丢);开 = 用上面的地址。
            // 写库后立即刷新全局 net(现读即生效,无需重启)。地址与开关分家 = 关掉不丢地址。
            "net.proxy_enabled" => {
                if !["0", "1"].contains(&value) {
                    return Err(invalid("开关需为 0 或 1"));
                }
                self.store.settings.set(None, key, value)?;
                crate::net::set_proxy(Self::resolve_proxy(&self.store));
                Ok(())
            }
            // 记忆自动提炼总开关(app 级,PLAN §13 Phase 3):0/1,默认开(缺省 = 开,见 spawn_consolidate)。
            // 关 = 不再后台蒸馏(手动 consolidate_conversation 入口不受影响);现读即生效,无需重启。
            "memory.auto_consolidate" => {
                if !["0", "1"].contains(&value) {
                    return Err(invalid("开关需为 0 或 1"));
                }
                self.store.settings.set(None, key, value)?;
                Ok(())
            }
            // 远程渠道配置(app 级,PLAN 远程渠道):enabled 校验 0/1;凭证/白名单原样写(trim)。
            // token/app_secret 等**不进 APP_SETTING_KEYS** → 写得进、读不回(钥匙永不过桥,§4)。
            // 改完由前端调 reload_channels 命令停旧起新(类比 provider 保存即重建)。
            k if k.starts_with("remote.") => {
                if k.ends_with(".enabled") && !["0", "1"].contains(&value) {
                    return Err(invalid("开关需为 0 或 1"));
                }
                // token/app_secret 等是秘密 → keyring(写得进读不回);开关/白名单非秘密走 settings
                if crate::secrets::is_secret(k) {
                    crate::secrets::set(&self.store.settings, k, value.trim())
                        .map_err(AppError::internal)?;
                } else {
                    self.store.settings.set(None, k, value.trim())?;
                }
                Ok(())
            }
            _ => Err(invalid("不在设置白名单内")),
        }
    }

    /// 供应商卡片列表 = 生效中的注册表 + 还没配置的内置预设模板(全部预填,用户按需改)。
    pub fn list_providers(&self) -> Result<Vec<ProviderView>, AppError> {
        Ok(self.effective_specs()?.iter().map(ProviderView::from_spec).collect())
    }

    /// 按 id upsert 一张卡:None 字段不动;api_key 只在非空时替换(掩码回显防误存)。
    /// 保存即物化整张 llm.providers JSON(兜底注册表从此显式化)并重建候选。
    pub fn save_provider(&self, patch: ProviderPatch) -> Result<Vec<ProviderView>, AppError> {
        let invalid = |msg: &str| AppError { kind: ErrorKind::Internal, message: msg.into() };
        let id = patch.id.trim();
        if id.is_empty() || id.chars().count() > 32 {
            return Err(invalid("供应商 id 为空或过长"));
        }
        let mut specs = self.effective_specs()?;
        let spec = match specs.iter_mut().find(|s| s.id == id) {
            Some(s) => s,
            None => {
                specs.push(ProviderSpec { id: id.into(), name: id.into(), ..Default::default() });
                specs.last_mut().expect("刚 push 过")
            }
        };
        if let Some(name) = patch.name {
            let name = name.trim();
            if !name.is_empty() {
                spec.name = name.into();
            }
        }
        if let Some(p) = patch.protocol {
            spec.protocol = Protocol::parse(&p).ok_or_else(|| invalid("未知协议"))?;
        }
        if let Some(u) = patch.base_url {
            spec.base_url = u.trim().trim_end_matches('/').into();
        }
        if let Some(m) = patch.model {
            spec.model = m.trim().into();
        }
        if let Some(en) = patch.enabled {
            spec.enabled = en;
        }
        if let Some(k) = patch.api_key {
            let k = k.trim();
            if !k.is_empty() {
                spec.api_key = k.into();
            }
        }
        if spec.base_url.trim().is_empty() || spec.model.trim().is_empty() {
            return Err(invalid("接入点和模型不能为空"));
        }
        self.persist_specs(&specs)?;
        Ok(specs.iter().map(ProviderView::from_spec).collect())
    }

    /// 内置预设只可禁用不可删;自定义卡可删。
    pub fn remove_provider(&self, id: &str) -> Result<Vec<ProviderView>, AppError> {
        if BUILTIN_PROVIDER_IDS.contains(&id) {
            return Err(AppError {
                kind: ErrorKind::Internal,
                message: "内置供应商不可删除,可以禁用".into(),
            });
        }
        let mut specs = self.effective_specs()?;
        specs.retain(|s| s.id != id);
        self.persist_specs(&specs)?;
        Ok(specs.iter().map(ProviderView::from_spec).collect())
    }

    /// 生效注册表 + 缺席的内置预设(模板形态,钥匙空、其余预填)。
    fn effective_specs(&self) -> Result<Vec<ProviderSpec>, AppError> {
        let mut specs = self.load_registry()?.specs().to_vec();
        if !specs.iter().any(|s| s.id == "anthropic") {
            specs.push(ProviderSpec::anthropic(String::new()));
        }
        Ok(specs)
    }

    fn persist_specs(&self, specs: &[ProviderSpec]) -> Result<(), AppError> {
        crate::secrets::set(
            &self.store.settings,
            "llm.providers",
            &ProviderRegistry::new(specs.to_vec()).to_json(),
        )
        .map_err(AppError::internal)?;
        self.reload_providers()
    }

    // ---- 多用户 / 家人(PLAN §11 D;会话管理类一等公民,§4 永不委托可插拔层) ----

    /// 家人列表(设置·家人 tab);附"是否已录声纹"标记。
    pub fn list_users(&self) -> Result<Vec<(User, bool)>, AppError> {
        let users = self.store.users.list()?;
        let enrolled = self.store.voiceprints.enrolled_ids()?;
        Ok(users.into_iter().map(|u| { let on = enrolled.contains(&u.id); (u, on) }).collect())
    }

    /// 添加家人。
    pub fn create_user(&self, name: &str) -> Result<User, AppError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(AppError { kind: ErrorKind::Internal, message: "名字不能为空".into() });
        }
        Ok(self.store.users.create(name)?)
    }

    /// 给某家人改名(家人 tab 列表行内改;rename_user 改的是默认用户,这条按 id)。
    pub fn rename_family(&self, id: i64, name: &str) -> Result<(), AppError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(AppError { kind: ErrorKind::Internal, message: "名字不能为空".into() });
        }
        self.store.users.rename(id, name)?;
        Ok(())
    }

    /// 删除家人:守住"至少留一人"+ 编排跨域清理(记忆/声纹随人走,隐私)。
    /// 会话不删(历史可能混着别人,归属悬空无害;boot 取最近活跃用户兜底)。
    pub fn delete_user(&self, id: i64) -> Result<(), AppError> {
        if self.store.users.count()? <= 1 {
            return Err(AppError { kind: ErrorKind::Internal, message: "至少得留一个人".into() });
        }
        self.store.memory.delete_for_user(id)?;
        self.store.voiceprints.remove(id)?;
        self.store.users.delete(id)?;
        Ok(())
    }

    /// 小本本(回忆页):看 7274 记住了什么。
    pub fn list_memories(&self) -> Result<Vec<Memory>, AppError> {
        let user = self.store.users.ensure_default_user()?;
        Ok(self.store.memory.list(user.id)?)
    }

    /// 记忆提炼 / 反思(PLAN §13 Phase 3):把一段会话蒸馏成耐久记忆。**保守**——只增不删、
    /// 提炼条目进按需层(不污染前缀)、近重复跳过(详见 `consolidate`)。后台尽力件:没配
    /// provider / 会话不存在 = 返回 0,不报错。返回新增条数。
    /// 这是**手动 / 命令入口**(按 conv_user 提炼);**自动触发**走 `spawn_consolidate`
    /// (`send_message` 每 `CONSOLIDATE_EVERY_TURNS` 个用户回合后台跑、按说话人提炼,2026-06-18 接上)。
    pub async fn consolidate_conversation(&self, conv_id: i64) -> Result<usize, AppError> {
        // 锁内只取主选 provider 的 Arc 快照,await 在锁外(RwLock guard 不跨 await)
        let provider = {
            let candidates = self.llm.read().expect("llm lock poisoned");
            candidates.first().map(|(_, p)| p.clone())
        };
        let Some(provider) = provider else { return Ok(0) };
        let Some(conv) = self.store.chat.get_conversation(conv_id)? else { return Ok(0) };
        let added = consolidate::run(&provider, &self.store, conv.user_id, conv_id, 50).await?;
        Ok(added)
    }

    /// 累加该会话「自上次提炼以来的用户回合数」,到阈值则归零并返回 true(该后台提炼了)。
    /// 纯计数(只改 SessionSlot 瞬态、无 IO),便于单测;真正的提炼由 `spawn_consolidate` 起。
    fn bump_consolidate_due(&self, conv_id: i64) -> bool {
        let mut sessions = self.sessions.lock().expect("sessions lock poisoned");
        let slot = sessions.entry(conv_id).or_default();
        slot.turns_since_consolidate += 1;
        if slot.turns_since_consolidate >= CONSOLIDATE_EVERY_TURNS {
            slot.turns_since_consolidate = 0;
            true
        } else {
            false
        }
    }

    /// 记忆自动提炼总开关(`memory.auto_consolidate`,app 级):缺省 = 开;只有显式 "0" 才关。
    fn auto_consolidate_enabled(&self) -> bool {
        self.store
            .settings
            .get(None, "memory.auto_consolidate")
            .ok()
            .flatten()
            .map(|v| v != "0")
            .unwrap_or(true)
    }

    /// 后台提炼一次该会话(PLAN §13 Phase 3 自动触发):尽力件 —— 开关关 / 没 provider / 上次还在跑
    /// 则跳过,错误只记日志,绝不影响主对话。写到说话人 `user_id`(记忆归人 §6)。
    /// cheap-model 路由仍后置(用主 provider);触发频率见 `CONSOLIDATE_EVERY_TURNS`。
    fn spawn_consolidate(&self, conv_id: i64, user_id: i64) {
        // 用户在设置关掉了自动提炼 = 直接不跑(手动入口不受影响)
        if !self.auto_consolidate_enabled() {
            return;
        }
        // 上次提炼还没跑完 = 跳过这轮(防并发重复落库;flag 持在会话槽,spawn 任务跑完清)
        let flag = {
            let mut sessions = self.sessions.lock().expect("sessions lock poisoned");
            sessions.entry(conv_id).or_default().consolidating.clone()
        };
        if flag.swap(true, Ordering::AcqRel) {
            return;
        }
        let provider = self.llm.read().expect("llm lock poisoned").first().map(|(_, p)| p.clone());
        let Some(provider) = provider else {
            flag.store(false, Ordering::Release);
            return;
        };
        let store = self.store.clone();
        tokio::spawn(async move {
            match consolidate::run(&provider, &store, user_id, conv_id, 50).await {
                Ok(n) if n > 0 => {
                    tracing::info!(target: "larkwing::memory", conv = conv_id, added = n, "记忆自动提炼:+{n} 条")
                }
                Ok(_) => {
                    tracing::debug!(target: "larkwing::memory", conv = conv_id, "记忆自动提炼:无新增")
                }
                Err(e) => {
                    tracing::warn!(target: "larkwing::memory", conv = conv_id, "记忆自动提炼失败(尽力件): {e:#}")
                }
            }
            flag.store(false, Ordering::Release);
        });
    }

    /// 记错了点掉(记忆卫生 = 信任感);按当前用户限定。
    pub fn delete_memory(&self, id: i64) -> Result<(), AppError> {
        let user = self.store.users.ensure_default_user()?;
        if !self.store.memory.delete(user.id, id)? {
            return Err(AppError {
                kind: ErrorKind::NotFound,
                message: format!("记忆 {id} 不存在"),
            });
        }
        Ok(())
    }

    /// 回忆页「家里的事」分组:当前用户视角的家庭备忘(home + 个人 scope)。
    pub fn list_briefings(&self) -> Result<Vec<Briefing>, AppError> {
        let user = self.store.users.ensure_default_user()?;
        Ok(self.store.briefings.list_for(user.id)?)
    }

    pub fn delete_briefing(&self, id: i64) -> Result<(), AppError> {
        if !self.store.briefings.remove_by_id(id)? {
            return Err(AppError {
                kind: ErrorKind::NotFound,
                message: format!("备忘 {id} 不存在"),
            });
        }
        Ok(())
    }

    /// 提醒页:当前用户待触发的提醒(定时任务,按 due_at 升序;真相在库、回合无状态)。
    pub fn list_reminders(&self) -> Result<Vec<crate::store::Job>, AppError> {
        let user = self.store.users.ensure_default_user()?;
        Ok(self.store.jobs.list_pending(user.id)?)
    }

    /// 提醒页「取消」:撤掉一条提醒(按当前用户限定,防串号)。
    pub fn cancel_reminder(&self, id: i64) -> Result<(), AppError> {
        let user = self.store.users.ensure_default_user()?;
        if !self.store.jobs.cancel(user.id, id)? {
            return Err(AppError {
                kind: ErrorKind::NotFound,
                message: format!("提醒 {id} 不存在"),
            });
        }
        Ok(())
    }

    // ---- 文件操作记录(PLAN §9 文件能力):操作记录页 + 撤销/重做 ----

    /// 操作记录页:当前用户最近的文件操作批次(最近在前)。
    pub fn list_fsops(&self) -> Result<Vec<crate::store::FsOpRow>, AppError> {
        let user = self.store.users.ensure_default_user()?;
        Ok(self.store.fsops.list_for(user.id, 100)?)
    }

    /// 撤销一批(操作记录页「撤销」按钮;模型侧另有 fs_undo 工具)。
    pub fn fsops_undo(&self, id: i64) -> Result<(), AppError> {
        self.apply_fsops(id, "applied", "undone", true)
    }

    /// 重做一批(「重做」按钮)。功能性,非安全承诺。
    pub fn fsops_redo(&self, id: i64) -> Result<(), AppError> {
        self.apply_fsops(id, "undone", "applied", false)
    }

    /// 撤销/重做共用:校归属 + 校当前状态(已是目标态 = 幂等返回)→ 执行 → 翻状态。
    /// 文件 I/O 直接在此(同 delete_* 等阻塞域方法,Tauri 在工作线程跑同步 command)。
    fn apply_fsops(&self, id: i64, from: &str, to: &str, undo: bool) -> Result<(), AppError> {
        let user = self.store.users.ensure_default_user()?;
        let row = self.store.fsops.get(id)?.ok_or_else(|| AppError {
            kind: ErrorKind::NotFound,
            message: format!("操作记录 {id} 不存在"),
        })?;
        if row.user_id != user.id {
            return Err(AppError { kind: ErrorKind::NotFound, message: "不是你的操作记录".into() });
        }
        if row.state != from {
            return Ok(()); // 已是目标状态 → 幂等(前端刷新即可)
        }
        let items: Vec<crate::files::FsOpItem> =
            serde_json::from_str(&row.ops).map_err(AppError::internal)?;
        if undo {
            crate::files::undo_batch(&items);
        } else {
            crate::files::redo_batch(&items);
        }
        self.store.fsops.set_state(id, to)?;
        Ok(())
    }

    pub fn rename_user(&self, name: &str) -> Result<User, AppError> {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 24 {
            return Err(AppError { kind: ErrorKind::Internal, message: "名字为空或过长".into() });
        }
        let user = self.store.users.ensure_default_user()?;
        self.store.users.rename(user.id, name)?;
        Ok(User { name: name.into(), ..user })
    }

    /// 今日用量快照(灯带初值;之后的增量由 TurnEvent::Usage 推送)。
    pub fn usage_today(&self) -> DayUsage {
        usage::usage_today(&self.store)
    }

    /// 会话累计快照(灯带"话题"段初值:开机/切话题时取;之后随 TurnEvent::Usage 推送)。
    pub fn usage_conversation(&self, conv_id: i64) -> crate::store::UsageTotals {
        usage::usage_conversation(&self.store, conv_id)
    }

    /// 历史/提醒气泡的 hover 读数(PLAN §11 D):把库里每回合的用量映射到对应的
    /// assistant 气泡 id —— 前端 load 会话后回填,让自启回合/历史消息也能 hover 看读数
    /// (在飞回合仍由 TurnEvent::Usage 实时常显,不走这条)。
    pub fn conversation_stats(&self, conv_id: i64) -> Result<Vec<MsgStats>, AppError> {
        let rollups = self.store.usage.rounds_by_turn(conv_id)?;
        if rollups.is_empty() {
            return Ok(vec![]);
        }
        // 回合锚点(user/event 行 id)→ 该回合"代表气泡"= 其后最后一条有内容的 assistant
        // (跨过中途的纯 tool_call 空 assistant 行;event 行是自启回合锚点,与 round.user_msg_id 对齐)
        let msgs = self.store.chat.recent_messages(conv_id, 200)?;
        let mut key_to_assistant: HashMap<i64, i64> = HashMap::new();
        let mut cur: Option<i64> = None;
        for m in &msgs {
            match m.role.as_str() {
                "user" | "event" => cur = Some(m.id),
                "assistant" if !m.content.trim().is_empty() => {
                    if let Some(k) = cur {
                        key_to_assistant.insert(k, m.id);
                    }
                }
                _ => {}
            }
        }
        Ok(rollups
            .into_iter()
            .filter_map(|r| {
                key_to_assistant.get(&r.user_msg_id).map(|&aid| MsgStats {
                    message_id: aid,
                    ms: r.elapsed_ms,
                    input_tokens: r.input_tokens,
                    output_tokens: r.output_tokens,
                    cache_hit_tokens: r.cache_hit_tokens,
                    cost_usd: r.cost_usd,
                })
            })
            .collect())
    }

    /// 历史回放的「想了想」轨迹:把每回合中途的工具调用(名/入参/结果/状态)+ CoT 原文,
    /// 归到该回合代表气泡(其后最后一条有内容的 assistant)。全从落库 payload 重建。
    /// live 回合落库后由前端补拉这条(不在 TurnEvent 里塞 args/result,免破流式词汇)。
    pub fn conversation_trace(&self, conv_id: i64) -> Result<Vec<TurnTrace>, AppError> {
        let msgs = self.store.chat.recent_messages(conv_id, 200)?;
        let mut out = Vec::new();
        let mut steps: Vec<TraceStep> = Vec::new();
        let mut reasoning: Vec<String> = Vec::new();
        let mut idx_by_call: HashMap<String, usize> = HashMap::new();
        let reset = |steps: &mut Vec<TraceStep>, r: &mut Vec<String>, idx: &mut HashMap<String, usize>| {
            steps.clear();
            r.clear();
            idx.clear();
        };
        for m in &msgs {
            match m.role.as_str() {
                "user" | "event" => reset(&mut steps, &mut reasoning, &mut idx_by_call),
                "assistant" => {
                    if let Some(p) = m
                        .payload
                        .as_deref()
                        .and_then(|p| serde_json::from_str::<AssistantPayload>(p).ok())
                    {
                        for c in &p.tool_calls {
                            let ui_key = self
                                .tools
                                .get(&c.name)
                                .map(|t| t.spec().ui_key.to_string())
                                .unwrap_or_else(|| "tool.unknown".into());
                            idx_by_call.insert(c.id.clone(), steps.len());
                            steps.push(TraceStep {
                                name: c.name.clone(),
                                ui_key,
                                args: c.args.to_string(),
                                result: String::new(),
                                status: String::new(),
                            });
                        }
                        if let Some(r) =
                            p.reasoning.as_deref().map(str::trim).filter(|s| !s.is_empty())
                        {
                            reasoning.push(r.to_string());
                        }
                    }
                    // 有内容 = 这回合代表气泡(可见回复):结算并复位
                    if !m.content.trim().is_empty() {
                        if !steps.is_empty() || !reasoning.is_empty() {
                            out.push(TurnTrace {
                                message_id: m.id,
                                steps: std::mem::take(&mut steps),
                                reasoning: (!reasoning.is_empty()).then(|| reasoning.join("\n\n")),
                            });
                        }
                        reset(&mut steps, &mut reasoning, &mut idx_by_call);
                    }
                }
                // tool 行:按 call_id 回填结果/状态到对应步骤
                "tool" => {
                    if let Some(tp) = m
                        .payload
                        .as_deref()
                        .and_then(|p| serde_json::from_str::<ToolRowPayload>(p).ok())
                    {
                        if let Some(step) = idx_by_call.get(&tp.call_id).and_then(|&i| steps.get_mut(i)) {
                            step.result = m.content.clone();
                            step.status = tp.status;
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(out)
    }

    /// 主选供应商的账户余额。None = 没配供应商/不支持/查不到 —— 锦上添花,失败静默。
    /// 查到的值顺手落快照(变了才记):余额差值 = 供应商账面的真实花费,给分析对账用。
    pub async fn llm_balance(&self) -> Option<crate::llm::AccountBalance> {
        // 锁内只取 Arc 快照,await 在锁外(RwLock guard 不能跨 await)
        let (provider_id, provider) = {
            let candidates = self.llm.read().expect("llm lock poisoned");
            candidates.first().map(|(id, p)| (id.clone(), p.clone()))
        }?;
        let balance = provider.balance().await?;
        let (store, b) = (self.store.clone(), balance.clone());
        let _ = tokio::task::spawn_blocking(move || {
            if let Err(e) = store.usage.add_balance_snapshot(&provider_id, &b.currency, &b.amount)
            {
                tracing::warn!("余额快照落库失败: {e:#}");
            }
        })
        .await;
        Some(balance)
    }

    /// 幂等取消:没在飞 = no-op。await 旧回合收尾(partial 落库完成)后才返回。
    pub async fn cancel(&self, conv_id: i64) {
        let handle = {
            let mut sessions = self.sessions.lock().expect("sessions lock poisoned");
            sessions.get_mut(&conv_id).and_then(|slot| slot.inflight.take())
        };
        if let Some(h) = handle {
            h.token.cancel();
            let _ = h.join.await;
        }
    }

    /// 回合入口。同会话已有在飞 → 自动取消旧的再开新的(会话管控)。
    /// 前置错误走 Err(镜像 llm 两阶段);开流后走 TurnEvent。
    pub async fn send_message(
        &self,
        conv_id: i64,
        text: String,
        meta: Option<UserMeta>,
        attachments: Vec<InAttachment>,
    ) -> Result<mpsc::Receiver<TurnEvent>, AppError> {
        // 1. 会话管控:必须 await 旧回合收尾,partial 先落库,新回合拼历史才完整
        self.cancel(conv_id).await;

        // 2. 前置检查:候选快照(失序读快照,reload 不阻塞在飞回合)
        let candidates = self.llm.read().expect("llm lock poisoned").clone();
        if candidates.is_empty() {
            return Err(AppError { kind: ErrorKind::NoApiKey, message: "还没有配置 API key".into() });
        }
        let conversation = self.store.chat.get_conversation(conv_id)?.ok_or(AppError {
            kind: ErrorKind::NotFound,
            message: format!("会话 {conv_id} 不存在"),
        })?;
        let scene = self
            .scenes
            .get(&conversation.scene_id)
            .unwrap_or_else(|| self.scenes.default_scene())
            .clone();

        // 3. 白名单工具子集(场景声明顺序,会话内稳定 → 前缀不抖)
        let tool_subset = self.tools.subset(&scene.tools);
        let tool_defs: Vec<ToolDef> = tool_subset.iter().map(|t| t.spec().def()).collect();

        // 落用户消息 + 取上下文原料 + 单一装配权出 ChatRequest(阻塞 IO 下沉线程池)
        let store = self.store.clone();
        let conv_user = conversation.user_id;
        let (mut request, user_msg_id, mem_user) = tokio::task::spawn_blocking(
            move || -> anyhow::Result<(crate::llm::ChatRequest, i64, i64)> {
            // 入站附件(媒体输入 PLAN §9):图 → image_url 当轮注入;文档 → 抽文字当轮注入;
            // 只把小票(AttachmentRef)落 payload,附件本体不进持久前缀(省 token/体积)。与插队共用
            let (image_parts, doc_text, att_refs) = process_attachments(&attachments);

            // 语音会话模式(PLAN §11)+ 附件小票:非默认形态物化进 payload(真相在库)
            let mut eff_meta = meta.unwrap_or_default();
            eff_meta.attachments = att_refs;
            let payload = (!eff_meta.is_default())
                .then(|| serde_json::to_string(&eff_meta))
                .transpose()?;
            let user_msg =
                store.chat.append_message_full(conv_id, "user", &text, payload.as_deref())?;
            // 记忆归人(§6):声纹识别出且确属真实用户 → 本回合用 TA;否则会话归属者
            // (访客/电视声识别不出 → fallback,绝不误记到家人名下,robot 同款立场)
            let mem_user = match eff_meta.speaker_user {
                Some(sid) if sid != conv_user && store.users.get(sid)?.is_some() => sid,
                _ => conv_user,
            };
            store.users.touch(mem_user)?;
            // 记忆只取常驻·画像层进前缀(§13.3 ②;按需层靠 recall 工具取),写时已执法预算
            // → 前缀有界、字节稳定,记得再多也不胀前缀(修掉「全量进前缀」雷,§13.1)
            let memories = store.memory.list_resident(mem_user)?;
            // 观测:这回合带进前缀的常驻记忆(测「用到了记忆吗」—— recall 不一定触发,
            // 大多数记忆是从这里被动进上下文的;§4.4 进库前的轻量日志版)
            tracing::info!(
                target: "larkwing::memory",
                conv = conv_id, resident = memories.len(),
                "turn ctx → {}",
                memories.iter().map(|m| m.content.as_str()).collect::<Vec<_>>().join(" | ")
            );
            // 任务需知:只有常驻条目进前缀(预算在写入时执法,这里无条件全装);
            // 非常驻的归 briefing_lookup 工具按需取
            let briefings: Vec<crate::store::Briefing> = store
                .briefings
                .list_for(mem_user)?
                .into_iter()
                .filter(|b| b.resident)
                .collect();
            // 性格设定用**会话归属者**(家给 7274 的人设,跟说话人无关 → 前缀字节稳定,
            // 一家人轮流说话不会让缓存失效);没设过=出厂默认句,空串=纯出厂人设
            let style = store
                .settings
                .get(Some(conv_user), "persona.style")?
                .unwrap_or_else(|| context::DEFAULT_PERSONA_STYLE.into());
            // 用户给助手起的名字(ui.pet_name):与性格设定同走会话归属者 → 前缀字节稳定;
            // 没设过/空 = 出厂名,build_context 不注入
            let pet_name = store.settings.get(Some(conv_user), "ui.pet_name")?;
            let total = store.chat.count_messages(conv_id)? as usize;
            let start = context::anchored_start(total);
            let history =
                store.chat.messages_page(conv_id, start as i64, (total - start) as i64)?;
            let mut request = context::build_context(
                &scene,
                pet_name.as_deref(),
                Some(&style),
                &memories,
                &briefings,
                &history,
                &tool_defs,
            );
            // 当轮注入附件(图 parts + 文档文字):挂到最后一条 user 消息上,持久前缀
            // (few-shot/历史)字节不动 → 缓存不破,也不为历史里的旧图反复付 vision 费
            if !image_parts.is_empty() || !doc_text.is_empty() {
                if let Some(crate::llm::ChatMessage::User { content, parts }) = request
                    .messages
                    .iter_mut()
                    .rev()
                    .find(|m| matches!(m, crate::llm::ChatMessage::User { .. }))
                {
                    content.push_str(&doc_text);
                    parts.extend(image_parts);
                }
            }
            // 反应模式(最快/轻度/中度/重度):每回合取值,改完下一句话就生效,无需重建 provider
            let thinking = match store.settings.get(None, "llm.thinking")?.as_deref() {
                Some("off") => crate::llm::Thinking::Off,
                Some("light") => crate::llm::Thinking::Light,
                Some("heavy") => crate::llm::Thinking::Heavy,
                // 缺省/"medium"/旧值"on"/未知 → 中度(默认反应模式)
                _ => crate::llm::Thinking::Medium,
            };
            if thinking != crate::llm::Thinking::Off {
                request.options.thinking = Some(thinking);
            }
            Ok((request, user_msg.id, mem_user))
        })
        .await
        .map_err(AppError::internal)??;

        // 记忆自动提炼(PLAN §13 Phase 3):每 N 个用户回合后台蒸馏一次(尽力件,不阻塞本回合)。
        // 写到说话人(mem_user,记忆归人 §6);用户消息已落库,蒸馏读得到这段历史。
        if self.bump_consolidate_due(conv_id) {
            self.spawn_consolidate(conv_id, mem_user);
        }

        // 「此刻」背景状态(播放器在不在放…)挂到末条 user,喂模型当下真相(不落库、不破缓存)
        self.inject_ambient(&mut request);

        // 4+5. 开流 + spawn 回合(与 wake_turn 共用尾段)。ToolCtx.user_id = mem_user:
        // remember 写到说话人(记忆归人);会话归属仍是 conv_user。
        self.launch(conv_id, mem_user, candidates, request, tool_subset, user_msg_id).await
    }

    /// 插队(PLAN §9 B):把一条消息塞进**正在跑的回合**,它在下一次 LLM 调用就带上(不打断)。
    /// 返回 false = 没有在飞回合 / 回合正收尾 —— 调用方(前端)改用普通发送起新回合。
    pub async fn inject(
        &self,
        conv_id: i64,
        text: String,
        meta: Option<UserMeta>,
        attachments: Vec<InAttachment>,
    ) -> bool {
        // 取在飞回合的注入句柄(锁内只 clone Arc)
        let inject = {
            let sessions = self.sessions.lock().expect("sessions lock poisoned");
            sessions.get(&conv_id).and_then(|slot| slot.inject.clone())
        };
        let Some(inject) = inject else { return false };
        // 提前拒:已在收尾就别处理了
        if inject.lock().expect("inject lock poisoned").finishing {
            return false;
        }
        // 处理附件(阻塞下沉线程池)→ 就绪形
        let ready = match tokio::task::spawn_blocking(move || {
            build_inject_ready(text, meta, attachments)
        })
        .await
        {
            Ok(r) => r,
            Err(_) => return false,
        };
        // 入队(再查一次 finishing:处理期间回合可能已收尾,原子防丢)
        let mut st = inject.lock().expect("inject lock poisoned");
        if st.finishing {
            return false;
        }
        st.buffer.push(ready);
        true
    }

    /// 共用尾段:开流(建连失败按候选顺序切换)→ spawn 回合 → 登记在飞。
    /// 全军覆没报主选的错误(最有代表性)。开流之后的失败不切换 —— 半截话已经
    /// 流向用户,静默换供应商重说会精神分裂,走既有 Failed 友好兜底。
    /// 工具轮的 2+ 次开流粘住本次选中的 provider(Turn 持有它)。
    async fn launch(
        &self,
        conv_id: i64,
        user_id: i64,
        candidates: Vec<(String, Arc<dyn LlmProvider>)>,
        request: crate::llm::ChatRequest,
        tool_subset: Vec<Arc<dyn crate::tools::Tool>>,
        user_msg_id: i64,
    ) -> Result<mpsc::Receiver<TurnEvent>, AppError> {
        let mut opened = None;
        let mut first_err: Option<LlmError> = None;
        for (id, provider) in &candidates {
            // 计时从"这一家"的建连起(供应商延迟归属干净;切换浪费的时间不算在赢家头上)
            let started = std::time::Instant::now();
            match provider.chat_stream(request.clone()).await {
                Ok(rx) => {
                    if first_err.is_some() {
                        tracing::warn!(provider = %id, "主选供应商建连失败,已切换备用");
                    }
                    opened = Some((rx, id.clone(), provider.clone(), started));
                    break;
                }
                Err(e) => {
                    tracing::warn!(provider = %id, err = %e, "建连失败,尝试下一个候选");
                    first_err.get_or_insert(e);
                }
            }
        }
        let (rx_llm, provider_id, provider, first_round_start) = match opened {
            Some(quad) => quad,
            None => return Err(first_err.expect("candidates 非空,必有错误").into()),
        };

        let (tx, rx) = mpsc::channel::<TurnEvent>(64);
        let token = CancellationToken::new();
        // 记账用的模型 id:单轮覆盖优先,否则取选中 provider 的默认模型
        let model =
            request.options.model.clone().unwrap_or_else(|| provider.model_id().to_string());
        let inject = Arc::new(Mutex::new(InjectState::default())); // 插队队列:Turn 与 inject 命令共用
        let join = tokio::spawn(
            turn::Turn {
                store: self.store.clone(),
                conv_id,
                user_id,
                token: token.clone(),
                tx,
                provider,
                provider_id,
                model,
                user_msg_id,
                first_round_start,
                request,
                tools: tool_subset,
                media: self.media.clone(),
                rx: rx_llm,
                inject: inject.clone(),
            }
            .run(),
        );
        {
            let mut sessions = self.sessions.lock().expect("sessions lock poisoned");
            let slot = sessions.entry(conv_id).or_default();
            slot.inflight = Some(TurnHandle { token, join });
            slot.inject = Some(inject);
        }
        Ok(rx)
    }

    /// 自启回合(调度器到点调用;PLAN §8「分离 job 型」的兑现):
    /// 执行一律**新鲜上下文** —— 稳定前缀与聊天回合字节级相同(共享缓存),不回放历史;
    /// 任务语境靠创建时物化进 content。无前端 Channel,engine 自己消费事件流,
    /// 完成后经全局事件车道喊"会话有动静"。
    /// 返回 false = 目标会话正有在飞回合,本次不打扰(调度器下个 tick 重试)。
    pub async fn wake_turn(&self, job: &crate::store::Job) -> Result<bool, AppError> {
        let candidates = self.llm.read().expect("llm lock poisoned").clone();
        if candidates.is_empty() {
            return Err(AppError { kind: ErrorKind::NoApiKey, message: "还没有配置 API key".into() });
        }

        // 会话兜底:原会话被删 → 该用户最新会话 → 新建(boot 同款链)
        let store = self.store.clone();
        let (job_conv, job_user) = (job.conv_id, job.user_id);
        let conversation = tokio::task::spawn_blocking(
            move || -> anyhow::Result<crate::store::Conversation> {
                if let Some(c) = store.chat.get_conversation(job_conv)? {
                    return Ok(c);
                }
                if let Some(c) = store.chat.latest_conversation(job_user)? {
                    return Ok(c);
                }
                // 自启回合兜底新建 = 系统渠道(原会话被删、用户也无任何会话时才走到)
                Ok(store.chat.create_conversation_full(
                    job_user,
                    DEFAULT_SCENE_ID,
                    crate::store::chat::CHANNEL_SYSTEM,
                )?)
            },
        )
        .await
        .map_err(AppError::internal)??;
        let conv_id = conversation.id;

        // 忙检:绝不打断用户正在进行的对话;调度器会重试
        {
            let sessions = self.sessions.lock().expect("sessions lock poisoned");
            if sessions.get(&conv_id).is_some_and(|s| s.inflight.is_some()) {
                return Ok(false);
            }
        }

        let scene = self
            .scenes
            .get(&conversation.scene_id)
            .unwrap_or_else(|| self.scenes.default_scene())
            .clone();
        let tool_subset = self.tools.subset(&scene.tools);
        let tool_defs: Vec<ToolDef> = tool_subset.iter().map(|t| t.spec().def()).collect();

        // 落 event 行(UI 不渲染;回放时经同一翻译进上下文)+ 拼新鲜请求
        let store = self.store.clone();
        let user_id = job.user_id;
        let content = job.content.clone();
        let (mut request, event_msg_id) = tokio::task::spawn_blocking(
            move || -> anyhow::Result<(crate::llm::ChatRequest, i64)> {
                let event_msg = store.chat.append_message(conv_id, "event", &content)?;
                // 只取常驻·画像层(§13.3 ②);任务回合与聊天回合共用同款前缀
                let memories = store.memory.list_resident(user_id)?;
                let briefings: Vec<crate::store::Briefing> = store
                    .briefings
                    .list_for(user_id)?
                    .into_iter()
                    .filter(|b| b.resident)
                    .collect();
                let style = store
                    .settings
                    .get(Some(user_id), "persona.style")?
                    .unwrap_or_else(|| context::DEFAULT_PERSONA_STYLE.into());
                let pet_name = store.settings.get(Some(user_id), "ui.pet_name")?;
                // 历史 = 空(新鲜上下文);注入消息与回放翻译同一字节形
                let mut request = context::build_context(
                    &scene,
                    pet_name.as_deref(),
                    Some(&style),
                    &memories,
                    &briefings,
                    &[],
                    &tool_defs,
                );
                request
                    .messages
                    .push(crate::llm::ChatMessage::user(context::event_injection(&content)));
                let thinking = match store.settings.get(None, "llm.thinking")?.as_deref() {
                    Some("off") => crate::llm::Thinking::Off,
                    Some("light") => crate::llm::Thinking::Light,
                    Some("heavy") => crate::llm::Thinking::Heavy,
                    // 缺省/"medium"/旧值"on"/未知 → 中度(默认反应模式)
                    _ => crate::llm::Thinking::Medium,
                };
                if thinking != crate::llm::Thinking::Off {
                    request.options.thinking = Some(thinking);
                }
                Ok((request, event_msg.id))
            },
        )
        .await
        .map_err(AppError::internal)??;

        // 自启回合也带「此刻」背景(任务到点时音乐可能正放着);不落库、不破缓存
        self.inject_ambient(&mut request);
        let mut rx =
            self.launch(conv_id, user_id, candidates, request, tool_subset, event_msg_id).await?;

        // 无人挂流:自己消费到收尾,记下终态,然后经全局事件车道喊一声
        // (UI 据此刷新列表;用户不在该会话时按 outcome 在列表项打标)
        let bus = self.bus.clone();
        tokio::spawn(async move {
            let mut outcome = crate::bus::TurnOutcome::Done;
            while let Some(ev) = rx.recv().await {
                if matches!(ev, TurnEvent::Failed { .. }) {
                    outcome = crate::bus::TurnOutcome::Failed;
                }
            }
            bus.publish(crate::bus::AppEvent::Conversation(crate::bus::ConversationActivity {
                conv_id,
                kind: "reminder".into(),
                outcome,
            }));
        });
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine(tag: &str) -> Arc<Engine> {
        let dir = std::env::temp_dir().join(format!("lw-engine-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _ = std::fs::remove_file(dir.join("t.db"));
        let store = Store::open(&dir.join("t.db")).unwrap();
        Engine::new(store, crate::scenes::Scenes::builtin())
    }

    /// 自动提炼计数:前 N-1 轮不触发、第 N 轮触发并归零、不同会话各自独立。
    #[test]
    fn consolidate_due_fires_every_n_turns_and_resets() {
        let eng = engine("consol-due");
        let conv = 1i64;
        for i in 1..CONSOLIDATE_EVERY_TURNS {
            assert!(!eng.bump_consolidate_due(conv), "第 {i} 轮不该触发");
        }
        assert!(eng.bump_consolidate_due(conv), "第 N 轮该触发");
        assert!(!eng.bump_consolidate_due(conv), "触发后计数归零、又从头数");
        assert!(!eng.bump_consolidate_due(2), "另一会话独立计数,不受影响");
    }

    /// 没配 provider 时后台提炼直接放弃(不 panic、不卡住 in-flight 标志)。
    #[test]
    fn spawn_consolidate_noops_without_provider() {
        let eng = engine("consol-noprov");
        eng.spawn_consolidate(1, 1); // 无 provider → 早退;flag 复位,可再次进入
        eng.spawn_consolidate(1, 1);
    }

    /// 自动提炼总开关:缺省 = 开;设 0 关、设回 1 开;非 0/1 被拒(顺带验白名单 + 校验臂)。
    #[test]
    fn auto_consolidate_setting_defaults_on_and_respects_off() {
        let eng = engine("consol-toggle");
        assert!(eng.auto_consolidate_enabled(), "缺省即开");
        eng.set_setting("memory.auto_consolidate", "0").unwrap();
        assert!(!eng.auto_consolidate_enabled(), "设 0 = 关");
        eng.set_setting("memory.auto_consolidate", "1").unwrap();
        assert!(eng.auto_consolidate_enabled(), "设回 1 = 开");
        assert!(eng.set_setting("memory.auto_consolidate", "2").is_err(), "非 0/1 被拒");
    }
}

