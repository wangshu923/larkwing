//! Core = 对话编排器(宪法 §5)。store 与 llm 的唯一合流点。

// 回合本体与后台脑力活
mod consolidate;
mod context;
pub(crate) mod diary; // pub(crate):eval 的 Drive::Diary 直调 run(评蒸馏质量)
mod title;
mod turn;
mod usage;
// `impl Engine` 按职责分家(2026-09-08;此前全挤在本文件一个 2473 行的 impl 块里)。
// 子模块能看见父模块的私有项 → 纯搬运:字段没改可见性、签名没动、调用点没动。
// 反过来不成立(父看不见子的私有项),所以少数被跨文件调用的私有 helper 标了 pub(super)。
mod background;
mod conversations;
mod dispatch;
mod library;
mod providers;
mod request;
mod settings;
mod snapshot;
mod subagent;
mod users;

// 搬到兄弟文件里的那些**对外**类型:再导出回来,调用方路径不变(`crate::engine::ProviderView` 照旧)。
pub use conversations::{TraceItem, TraceStep, TurnTrace};
pub use library::ReminderItem;
pub use providers::{ModelChoice, ModelGuess, ModelMeta, ProviderPatch, ProviderView};
pub use settings::SettingEntry;
pub use snapshot::{BootSnapshot, CareCandidate, FloatIdle, FloatReminder};
pub use usage::{DayUsage, MsgStats, UsageDigest};

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use serde::Serialize;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use serde::Deserialize;

use crate::llm::registry::{resolve_env, Protocol, ProviderRegistry, ProviderSpec, Strategy};
use crate::llm::{LlmError, LlmProvider, ToolCall, ToolDef};
use crate::lockext::{LockExt, RwLockExt};
use crate::scenes::{Scenes, DEFAULT_SCENE_ID};
use crate::store::{Briefing, Conversation, DiaryEntry, Memory, Message, SearchHit, Store, User};
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
    /// (core 不产文案铁规);绝不露工具/agent 概念,只露友好动词 —— 这是**折叠层**的口径。
    /// **展开层的技术细节也随流走(2026-08-16 改)**:`id`(call_id,配对用)+ `name`(原始
    /// 工具名)+ `args`(入参,Started 时带)+ `result`/`status`(Finished 时带,**截断**)。
    /// 原先「入参/结果故意不入流、回合收尾 hydrate 再补」——真机实锤:批量回合一跑几分钟,
    /// 这期间展开「想了想」里全是光秃秃的动词、点都点不开。截断后每步几百字节,流量可忽略;
    /// 收尾 hydrate 仍会用落库全量版整条覆盖(在飞看个大概,完了看全)。
    ToolUse {
        id: String,
        label: String,
        name: String,
        args: String,
        result: String,
        status: String,
        state: ToolUseState,
    },
    /// 记账灯带:本轮消耗 + 今日/会话累计快照(工具回合每轮各发一次;累计直接来自库,
    /// 前端只展示不记账)。provider 没回 usage(严格端点/假流)就不发 —— 不点没数据的灯。
    Usage { round: UsageDigest, today: DayUsage, conv: crate::store::UsageTotals },
    /// 插队(PLAN §9 B):回合在飞时注入的一条 user 消息已落库,之后的回复另起一段。
    /// 前端据此:收尾当前回复气泡 → 插用户气泡 → 开新回复气泡。
    Injected { message_id: i64, text: String, attachments: Vec<AttachmentRef> },
    /// show_image 亮图:工具把本机图片摆进聊天给**用户**看(图卡走 UI,不喂模型)。
    /// 前端把图卡插进当前在飞气泡组;同一批 refs 已随该 tool 行 payload 落库,
    /// 重开会话由行派生同一张卡(回执小票同构的「live 事件 + 落库派生」双路)。
    Shown { attachments: Vec<AttachmentRef> },
    /// 带文字的工具轮(PLAN §9):这一轮模型既说了话、又要继续调工具,它在落库里是一条独立
    /// assistant 内容行。前端据此把当前回复气泡封口(钉上 message_id 供「想了想」轨迹回挂)、
    /// 另起新泡接后续文字 —— 让在飞气泡结构 = 落库/重启结构(否则 trace 实时挂不上、重启才显)。
    Segment { message_id: i64 },
    /// 带落库 id:前端把流式文本与持久消息对账。
    /// end_session = 本回合模型调过 end_conversation(§7.5 会话收尾):唤醒回合据此走 wakeResume
    /// 收窗回待唤醒、**不开跟进窗**(免唤醒连续对话的显式结束);打字回合恒 false / 无意义。
    /// 加字段对前端是增量(tagged 编码,§6.8);旧前端不读它 = 维持开跟进窗的原行为。
    Done { message_id: i64, end_session: bool },
    Failed { kind: ErrorKind, message: String },
    Cancelled,
    /// 旁听临时回合(send_overheard):模型判「不是叫我」→ 整轮蒸发,什么都没落库
    /// (仅 tracing + usage 流水)。engine 内消费/测试用;前端无 Channel,收不到。
    Dismissed,
    /// 旁听转正:悬置的 user 行已落库(模型开口/干活了),此后事件与普通回合无异。
    Committed { message_id: i64 },
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
    /// show_image 亮的图(空 = 常态,序列化省略、老行反序列化补默认):
    /// 前端从工具行派生聊天图卡(重开会话图卡照在,与 live 的 TurnEvent::Shown 同一批 refs)。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<AttachmentRef>,
}

/// user 行:输入来源与朗读意向(PLAN §11 语音会话模式)。input: typed | mic | wake |
/// voice_msg(手机渠道语音消息转写,speak 恒 false——渠道回复是文字);
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
    /// 图片落盘的相对文件名(`attachments/` 下):重开会话仍能显缩略图(§9:图不喂 LLM 省钱,
    /// 但 UI 该看得见——bytes 走文件不进库 §6.2,前端按需经 relay /f/ 取)。doc / 旧数据 = None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
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

struct TurnHandle {
    token: CancellationToken,
    join: tokio::task::JoinHandle<()>,
}

/// 把新回合登记进会话槽,返回**该被取消的那一支**(None = 无人需要取消)。
///
/// 抽成独立函数只为一件事:这段判定住在 `launch` 的竞态窗口里(忙检与登记之间隔着 open_stream
/// 整次 HTTP 建连),真实竞态没法确定性复现,但**判定表本身**是会写错的地方 —— 单测钉住它。
///
/// 规则(2026-08-22 审计:原先是直接 `slot.inflight = Some(...)` 覆盖):
/// · `TurnHandle` 的 drop 既不 abort join 也不 cancel token → 被覆盖掉的回合会继续在同一会话里
///   跑,而**再没人持有它的 token**:停止按钮、delete/rollback 的 `cancel().await` 全够不到它,
///   它还会往已被截断的会话里接着插行。所以任何一支都不许无主地跑下去。
/// · 普通回合撞上在飞 → 新的赢(与 send_message 先 cancel 的语义一致),挤掉旧的;
/// · **旁听仲裁撞上在飞 → 旁听让路**(机会性的,真输入优先 §8.2)—— 取消刚起的自己,
///   绝不能反过来把用户的真回合杀掉。
/// · 旧句柄已经跑完(is_finished)= 不算在飞,既不算忙、也不需要取消。
fn register_inflight(
    slot: &mut SessionSlot,
    new_handle: TurnHandle,
    is_overheard: bool,
    inject: Arc<Mutex<InjectState>>,
) -> Option<TurnHandle> {
    let busy = slot.inflight.as_ref().is_some_and(|h| !h.join.is_finished());
    if is_overheard && busy {
        return Some(new_handle); // 让路:不登记,取消刚起的仲裁
    }
    let old = slot.inflight.replace(new_handle);
    // 旁听仲裁不收插队:用户此刻打字 = inject 落空 → 前端走普通发送 →
    // send_message 的 cancel 把未转正的仲裁整轮挤掉(真输入优先,零痕迹)。
    slot.inject = if is_overheard { None } else { Some(inject) };
    old.filter(|h| !h.join.is_finished())
}

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
    /// 「计划」(§6.5 会话内工作备忘):plan_set 全量替换写入(回合循环嗅探),
    /// inject_ambient 每回合背诵未完项。瞬态:丢槽 = 计划丢,用户再说一句即重建,
    /// 真相从不在槽里;持久化/跨会话明确不做(§9)。
    plan: Arc<Mutex<crate::tools::plan::Plan>>,
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
    /// 家庭日记(engine/diary.rs)后台补写的防重入 + 限流(app 级瞬态,丢了 = 下个节拍再试)。
    diary_inflight: Arc<AtomicBool>,
    diary_last_try: AtomicI64,
    /// 壳层网页渲染器(web_render 工具的机器件,webrender.rs 接缝):壳层 boot 注入;
    /// 没注入(core 单测/eval/headless)= None,工具如实说没有渲染组件。
    web_renderer: std::sync::OnceLock<Arc<dyn crate::webrender::WebRenderer>>,
    /// 语音运行时(read_audio 的耳朵;壳层 boot 注入,core 单测/eval 里为空)。
    voice: std::sync::OnceLock<crate::voice::VoiceRuntime>,
    /// 动作确认中枢(§7.8 确认闸):经 ToolCtx 给工具;前端命令 / 渠道回话 / 语音听音
    /// 都汇到它的 resolve,先到先得。
    confirmer: Arc<crate::confirm::Confirmer>,
    /// 自身弱引用(with_media 里 Arc 建好后回填):delegate 适配器(EngineSubAgent)拿它
    /// 在工具执行时回到 engine 跑子回合 —— Weak 不锁死生命周期,engine 没了就如实报错。
    weak_self: std::sync::OnceLock<std::sync::Weak<Engine>>,
    /// 同时在跑的子回合(delegate)计数:同步等待段 + 转后台段全算(§4.11 上限 4,
    /// 满了如实退回不排队);守卫式增减,panic/取消也不漏减。
    sub_active: Arc<std::sync::atomic::AtomicUsize>,
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
        // 出厂技能刷进库(内容以出厂为准、enabled 保留用户状态;§7 技能)。
        // 数据坏在 builtin_skills() 里 panic(编译者错误);写库失败只 warn 不挡开机。
        if let Err(e) = store.skills.sync_builtins(&crate::skills_builtin::builtin_skills()) {
            tracing::warn!(err = %e, "出厂技能同步失败(技能索引可能不全)");
        }
        // 代理总开关落全局(启动即生效;之后 set_setting 改了会刷新)。net 模块不碰
        // store/llm,故解析(读设置 + ${ENV} + env 回落)的合流放在 engine——唯一合流点。
        crate::net::set_proxy(Self::resolve_proxy(&store));
        let bus = media.bus().clone();
        let confirmer = crate::confirm::Confirmer::new(bus.clone(), store.clone());
        let engine = Arc::new(Engine {
            store,
            llm: RwLock::new(Vec::new()),
            scenes,
            tools,
            media,
            bus,
            sessions: Mutex::new(HashMap::new()),
            diary_inflight: Arc::new(AtomicBool::new(false)),
            diary_last_try: AtomicI64::new(0),
            web_renderer: std::sync::OnceLock::new(),
            voice: std::sync::OnceLock::new(),
            confirmer,
            weak_self: std::sync::OnceLock::new(),
            sub_active: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        });
        let _ = engine.weak_self.set(Arc::downgrade(&engine));
        engine
    }

    /// 壳层 boot 注入网页渲染器(webrender 接缝;重复注入忽略——boot 只跑一次)。
    pub fn set_web_renderer(&self, r: Arc<dyn crate::webrender::WebRenderer>) {
        let _ = self.web_renderer.set(r);
    }

    fn web_renderer(&self) -> Option<Arc<dyn crate::webrender::WebRenderer>> {
        self.web_renderer.get().cloned()
    }

    /// 壳层 boot 注入语音运行时(read_audio 的耳朵;set_web_renderer 同款接缝)。
    pub fn set_voice(&self, v: crate::voice::VoiceRuntime) {
        let _ = self.voice.set(v);
    }

    fn voice(&self) -> Option<crate::voice::VoiceRuntime> {
        self.voice.get().cloned()
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

    /// 全局事件车道(测试/调度器观测用)。
    pub fn bus(&self) -> &crate::bus::Bus {
        &self.bus
    }

    /// 动作确认中枢(壳层 confirm_action 命令 / 渠道回话 / 语音听音的应答入口)。
    pub fn confirmer(&self) -> &Arc<crate::confirm::Confirmer> {
        &self.confirmer
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    /// 助手显示名:主人的 `ui.pet_name`(空 = 出厂默认名 `context::DEFAULT_NAME`)。
    /// core 侧需要具名的用户可见文案时用(如渠道引导语);默认名回落单一真相源,
    /// 绝不再硬编「旺财 / 7274」(§4.1 名字 = 用户数据、§4.11 单源、§6.6 名字准则)。
    pub(crate) fn pet_name(&self) -> String {
        self.store
            .users
            .ensure_default_user()
            .ok()
            .and_then(|u| self.store.settings.get(Some(u.id), "ui.pet_name").ok().flatten())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| context::DEFAULT_NAME.to_string())
    }
}

/// 单测夹具:拆分后各主题文件的 `mod tests` 共用(`crate::engine::testkit::engine`)。
/// 住在 mod.rs 是因为它得被所有兄弟文件看见 —— 父模块的 `pub(super)` 项对全部后代可见。
#[cfg(test)]
mod testkit {
    use super::*;

    pub(super) fn engine(tag: &str) -> Arc<Engine> {
        let dir = std::env::temp_dir().join(format!("lw-engine-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _ = std::fs::remove_file(dir.join("t.db"));
        let store = Store::open(&dir.join("t.db")).unwrap();
        Engine::new(store, crate::scenes::Scenes::builtin())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 登记在飞句柄的判定表(2026-08-22 审计:原先直接覆盖 → 被挤掉的回合无人持 token、
    /// 停止按钮够不到它,还会往已截断的会话里接着插行)。真实竞态窗口不可确定性复现,
    /// 但判定表能钉:**任何一支都不许无主地跑下去,且旁听绝不许杀掉真回合。**
    #[tokio::test]
    async fn register_inflight_never_leaves_an_unowned_turn() {
        // 造句柄:running = 永不结束;done = 已跑完(is_finished 为真)
        fn running() -> TurnHandle {
            TurnHandle {
                token: CancellationToken::new(),
                join: tokio::spawn(async { std::future::pending::<()>().await }),
            }
        }
        async fn done() -> TurnHandle {
            let h = TurnHandle { token: CancellationToken::new(), join: tokio::spawn(async {}) };
            for _ in 0..100 {
                if h.join.is_finished() {
                    break;
                }
                tokio::task::yield_now().await;
            }
            assert!(h.join.is_finished(), "夹具:这支应已跑完");
            h
        }
        let q = || Arc::new(Mutex::new(InjectState::default()));

        // ① 空槽 + 普通:登记,无人被取消
        let mut slot = SessionSlot::default();
        assert!(register_inflight(&mut slot, running(), false, q()).is_none(), "空槽无 loser");
        assert!(slot.inflight.is_some() && slot.inject.is_some(), "登记上了,且收插队");

        // ② 在飞 + 普通:新的赢,**旧的必须被交出来取消**(否则它无主地跑)
        let old_tok = slot.inflight.as_ref().unwrap().token.clone();
        let loser = register_inflight(&mut slot, running(), false, q()).expect("旧的要被交出来");
        loser.token.cancel();
        assert!(old_tok.is_cancelled(), "被挤掉的那支必须有人取消它");

        // ③ 在飞 + **旁听**:旁听让路 —— 交出来的是「新来的」,槽里仍是原来那支
        let kept = slot.inflight.as_ref().unwrap().token.clone();
        let new_tok = CancellationToken::new();
        let newcomer =
            TurnHandle { token: new_tok.clone(), join: tokio::spawn(async { std::future::pending::<()>().await }) };
        let loser = register_inflight(&mut slot, newcomer, true, q()).expect("旁听自己让路");
        loser.token.cancel();
        assert!(new_tok.is_cancelled(), "让路的是刚起的仲裁");
        assert!(!kept.is_cancelled(), "真回合绝不能被旁听杀掉");
        // 槽里还是原来那支:token 是克隆、共享取消状态 —— 取消槽里的那个,kept 必须跟着变
        slot.inflight.as_ref().unwrap().token.cancel();
        assert!(kept.is_cancelled(), "槽里留的必须是原来那支(不是新来的)");

        // ④ 旧句柄已跑完:不算忙 → 旁听也能登记,且没有谁需要取消
        let mut slot = SessionSlot { inflight: Some(done().await), ..Default::default() };
        assert!(register_inflight(&mut slot, running(), true, q()).is_none(), "跑完的不需取消");
        assert!(slot.inject.is_none(), "旁听回合不收插队");
        // ⑤ 普通回合同理:跑完的旧句柄不当 loser 交出去
        let mut slot = SessionSlot { inflight: Some(done().await), ..Default::default() };
        assert!(register_inflight(&mut slot, running(), false, q()).is_none());
    }
}
