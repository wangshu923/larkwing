//! 远程交互渠道(PLAN 远程渠道):引擎边界适配器,**复用 turn loop**(AGENT §5 species ②)。
//! 不是工具、不碰 ToolCtx、不内嵌人格;一渠道一文件 + 这里注册。
//! 监督器按 settings 决定起哪些渠道;每渠道一个带 `CancellationToken` 的任务,崩溃退避重连
//! (不静默失败 §3.5,状态进 `ChannelStatus` 给设置页)。改配置/开关后由 `Supervisor::restart` 停旧起新。
//! 出站 HTTP 全走 `net::Client`(§4.6)。
//!
//! 手机端补全(v0.2.4):① 提醒推回手机 —— 复用全局事件车道(悬浮窗同款「又一个消费者」),
//! 订阅 `ConversationActivity{kind:"reminder"}`,渠道映射会话把回复推出去,engine 零改;
//! ② 语音/照片消息 —— 组合 `voice`(本地 ASR)与 `media`(ffmpeg 解码)两个 core 运行时,
//! channels 是边界适配器、只消费它们的公开 API(voice/media 均不反向依赖本模块)。

mod dingtalk;
/// 出站文件(send_file 工具的机器件;按人解析目标线程,§7.7)。pub(crate):工具层经
/// `crate::channels::outbound` 使用——tools 依赖 channels 的单向引用,不构成环(channels
/// 不认识 tools),也不破 §6.1(channels 仍不反向依赖 engine 之外的上层)。
pub(crate) mod outbound;
mod render;
mod telegram;
mod weixin;

/// 微信扫码登录(命令层用):起手拿二维码 + 轮询状态。QR 流程与协议在 `weixin` 模块内,
/// 这里给 pub 薄包装暴露给壳层(mod weixin 保持私有);storage 归 core(§6.6)。
pub use weixin::{QrPoll, QrStart};

/// 起手:POST 拿二维码,渲染 SVG 给设置 UI。
pub async fn weixin_qr_start() -> anyhow::Result<QrStart> {
    weixin::qr_start().await
}

/// 轮询扫码状态;confirmed 时账号(token/入口/身份)进绑定列表。`base_url` = 前端持有的
/// 当前轮询地址(IDC 重定向后回传;空 = 默认入口)。
pub async fn weixin_qr_poll(
    engine: &Engine,
    qrcode: &str,
    base_url: Option<&str>,
    verify_code: Option<&str>,
) -> anyhow::Result<QrPoll> {
    weixin::qr_poll_and_store(engine, qrcode, base_url, verify_code).await
}

/// 绑定列表(设置 UI 用):只给绑定者 user_id,**不含 token**(凭证不过桥 §7.7)。
/// 空串项 = 旧版迁移来的无身份绑定(UI 显示成「早期绑定」之类)。
pub fn weixin_accounts(engine: &Engine) -> Vec<String> {
    weixin::load_accounts(&engine.store().settings).into_iter().map(|a| a.user_id).collect()
}

/// 解绑一个微信账号(user_id 空串 = 解绑旧迁移绑定);调用方随后 reload_channels 停旧起新。
pub fn weixin_unbind(engine: &Engine, user_id: &str) -> anyhow::Result<()> {
    weixin::unbind_account(&engine.store().settings, user_id)
}

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use tokio_util::sync::CancellationToken;

use crate::engine::{Engine, InAttachment, TurnEvent, UserMeta};
use crate::net;

/// 攒批提示(§6.6 债:渠道操作性话术,不经模型;三渠道共用)。用户 2026-07-11 选定「极简功能」版。
/// 时机 = 纯文件消息到达、缓冲从空变满那一刻(防抖:连发多个只第一个提示,后续静默并入)。
pub(crate) const ATTACH_HINT: &str =
    "文件收到了。想让我做什么?说一句(可以接着发别的文件),我就连同文件一起处理。";
/// 攒批过期:攒着的文件超过这么久还没等到文字,下次文字到就不再算进去(隔太久的文件
/// 不该混进新意图)。宽松取 30min —— 人「发文件→看着发出去→再打字」十几秒很常见,别掐太紧。
const ATTACH_TTL: Duration = Duration::from_secs(30 * 60);

/// 攒批缓冲一项:等意图的附件 + 首个到达时刻(过期判定)。
struct PendingAttach {
    items: Vec<InAttachment>,
    first_at: Instant,
}

/// 附件攒批器(per (channel, ext_id)):无文字附件先攒、等文字一起处理。抽成独立结构 =
/// 可脱 ChannelCtx 单测(防抖 / 攒取 / 过期)。三渠道共用一个实例(ChannelCtx 持有)。
#[derive(Default)]
struct AttachBuffer {
    inner: Mutex<HashMap<(String, String), PendingAttach>>,
}

impl AttachBuffer {
    /// 收进缓冲;返回是否「本批第一个」(缓冲空→满)= 调用方要不要发一次提示。
    fn buffer(&self, channel: &str, ext_id: &str, atts: Vec<InAttachment>) -> bool {
        let mut m = self.inner.lock().expect("attach_buf poisoned");
        // 攒新批时顺手清过期僵尸(有人发了文件再没回来打字 → 别让 base64 bytes 长留内存)
        m.retain(|_, p| p.first_at.elapsed() < ATTACH_TTL);
        let key = (channel.to_string(), ext_id.to_string());
        match m.get_mut(&key) {
            Some(p) => {
                p.items.extend(atts);
                false
            }
            None => {
                m.insert(key, PendingAttach { items: atts, first_at: Instant::now() });
                true
            }
        }
    }

    /// 取走并清空;超 `ATTACH_TTL` 的丢弃当没攒过(隔太久的文件不混进新意图)。
    fn take(&self, channel: &str, ext_id: &str) -> Vec<InAttachment> {
        let mut m = self.inner.lock().expect("attach_buf poisoned");
        match m.remove(&(channel.to_string(), ext_id.to_string())) {
            Some(p) if p.first_at.elapsed() < ATTACH_TTL => p.items,
            _ => Vec::new(),
        }
    }
}

/// 渠道连接状态(给设置页状态行;不静默失败 §3.5)。
#[derive(Clone, Default, serde::Serialize)]
pub struct ChannelState {
    pub running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// 共享状态表:各渠道写,`remote_status` 命令读(channel → 状态)。
pub type ChannelStatus = Arc<Mutex<HashMap<String, ChannelState>>>;

// ⚠️ core 侧静态话术(§6.6 债,同 ONBOARD_HINT):确认闸的渠道回话三句,不经模型。
/// 确认请求推到手机的提示({action} = 按 kind 组的动作短语,见 `confirm_action_phrase`)。
pub(crate) const CONFIRM_PROMPT_SITE: &str = "要在 {host} {action},回「确认」就继续;回别的或不理,这步就不做。";
pub(crate) const CONFIRM_PROMPT_BARE: &str = "要{action},回「确认」就继续;回别的或不理,这步就不做。";

/// 确认卡的动作短语(kind + 目标原文 → 人话;桌面卡走前端字典组同款,这里是渠道静态话术半边)。
fn confirm_action_phrase(kind: &str, action: &str) -> String {
    match (kind, action.is_empty()) {
        ("submit", true) => "提交这个表单".to_string(),
        ("submit", false) => format!("提交『{action}』"),
        ("press", _) => format!("按 {action} 键"),
        _ => format!("点『{action}』"),
    }
}
/// 肯定回话的即时回执(用户回「确认」到模型接着说话之间有工具执行的空窗,别让人对着空气等)。
const CONFIRM_ACK: &str = "好,继续。";
/// 回「确认」时那张卡已收尾(超时/别处先点了):如实说,这句不进回合。
const CONFIRM_EXPIRED: &str = "这个确认已经过期了,那一步没有执行——还要继续的话再说一声。";

/// 渠道适配器运行上下文(引擎 + 语音/媒体运行时 + 状态回写)。pub(crate):仅 channels 内部构造。
pub(crate) struct ChannelCtx {
    pub engine: Arc<Engine>,
    /// 本地 ASR(手机语音消息转写);模块头注释:channels 组合它、它不认识 channels。
    pub voice: crate::voice::VoiceRuntime,
    /// ffmpeg 解码(语音消息 ogg/opus → PCM);同上,只消费公开 API。
    pub media: crate::media::MediaRuntime,
    pub status: ChannelStatus,
    /// 攒批缓冲(A,2026-07-11):手机发文件不能同时打字(微信铁律、钉钉文件同样),文件先
    /// 到、意图后到。无文字的附件消息先攒这里、不触发回合;等用户发来文字,连同攒的一起处理
    /// (§7.7)。带文字的附件(TG caption / 钉钉图文 richText)不进这。
    attach_buf: AttachBuffer,
    /// 确认闸的渠道等待表(§7.8):chat → 挂着的确认 id。outbound_loop 推确认提示时登记;
    /// 回话应答 / 卡片终态(超时/别处先点)时清。判定是**代码层严格词表**(`confirm::
    /// channel_reply_allows`),不交模型仲裁——页面注入玩不到这里。
    confirm_waits: Mutex<HashMap<(String, String), u64>>,
}

impl ChannelCtx {
    /// 攒批:无文字附件收进缓冲,等后续文字一起处理。返回是否「本批第一个」——true 时调用方
    /// 发一次 `ATTACH_HINT`;后续并入的返回 false(静默,防抖=连发多个文件只提示一次)。
    pub(crate) fn buffer_attachments(
        &self,
        channel: &str,
        ext_id: &str,
        atts: Vec<InAttachment>,
    ) -> bool {
        self.attach_buf.buffer(channel, ext_id, atts)
    }

    /// 取走并清空某人攒着的附件(用户发来文字时调,连同文字一起进回合)。超过 `ATTACH_TTL`
    /// 的丢弃当没攒过(隔太久的文件不该混进新意图)。没攒过 = 空 Vec(正常纯文本回合)。
    pub(crate) fn take_attachments(&self, channel: &str, ext_id: &str) -> Vec<InAttachment> {
        self.attach_buf.take(channel, ext_id)
    }

    /// 确认等待登记(outbound_loop 推完提示调)。同 chat 只挂一单(新单顶旧单——旧单会
    /// 因超时/取消自己收尾,这里不额外处理)。
    fn confirm_wait_set(&self, channel: &str, ext_id: &str, id: u64) {
        self.confirm_waits
            .lock()
            .expect("confirm_waits poisoned")
            .insert((channel.to_string(), ext_id.to_string()), id);
    }

    /// 卡片终态(超时/桌面先点了):按 id 把等待摘掉(不知道 chat,按值扫)。
    fn confirm_wait_clear(&self, id: u64) {
        self.confirm_waits.lock().expect("confirm_waits poisoned").retain(|_, v| *v != id);
    }

    /// 渠道回话先过确认闸(§7.8):该 chat 挂着确认时,这条**纯文本**回话先做应答判定。
    /// 返回 Some(回执) = 消息被确认流程消费(回执直接发,不进回合);None = 没挂确认,
    /// 或已按「拒」处理(拒的那句照常进回合——用户说「先别,改成 X」,在飞回合正等着它)。
    pub(crate) fn confirm_reply(
        &self,
        channel: &str,
        ext_id: &str,
        text: &str,
    ) -> Option<&'static str> {
        let key = (channel.to_string(), ext_id.to_string());
        let id = *self.confirm_waits.lock().expect("confirm_waits poisoned").get(&key)?;
        self.confirm_waits.lock().expect("confirm_waits poisoned").remove(&key);
        if crate::confirm::channel_reply_allows(text) {
            if self.engine.confirmer().resolve(id, true, "channel") {
                Some(CONFIRM_ACK)
            } else {
                Some(CONFIRM_EXPIRED) // 已超时/别处先答:这句只是「确认」,不值得进回合
            }
        } else {
            // 其他任何回复 = 拒;resolve 失败(已收尾)也无妨——消息照常进回合
            let _ = self.engine.confirmer().resolve(id, false, "channel");
            None
        }
    }

    pub(crate) fn set_state(&self, channel: &str, running: bool, err: Option<String>) {
        if let Ok(mut m) = self.status.lock() {
            m.insert(channel.into(), ChannelState { running, last_error: err });
        }
    }

    /// settings 取非空值(渠道**非秘密**配置:开关 / 白名单)。
    pub(crate) fn setting(&self, key: &str) -> Option<String> {
        self.engine
            .store()
            .settings
            .get(None, key)
            .ok()
            .flatten()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    /// 取渠道**秘密**(token / app_secret 等):走 keyring(回落 settings),不读 SQLite 明文(§6.3)。
    pub(crate) fn secret(&self, key: &str) -> Option<String> {
        crate::secrets::get(&self.engine.store().settings, key)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }
}

/// 按 settings 起各启用渠道,等它们收尾(被 `ct` 取消时返回)。
/// 顶层 spawn 由壳层用 `tauri::async_runtime::spawn` 发起(core 不依赖 tauri,§6.1);
/// "停旧起新"= 壳层取消旧 `ct`、重新 spawn 本函数(shell-side supervisor)。
/// 本函数内的 per-channel `tokio::spawn` 安全 —— 它已在 runtime worker 线程上跑。
pub async fn run(
    engine: Arc<Engine>,
    voice: crate::voice::VoiceRuntime,
    media: crate::media::MediaRuntime,
    status: ChannelStatus,
    ct: CancellationToken,
) {
    let store = engine.store().clone();
    let enabled = |k: &str| store.settings.get(None, k).ok().flatten().as_deref() == Some("1");
    let ctx = Arc::new(ChannelCtx {
        engine,
        voice,
        media,
        status,
        attach_buf: AttachBuffer::default(),
        confirm_waits: Mutex::new(HashMap::new()),
    });
    let mut tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();

    if enabled("remote.telegram.enabled") {
        let (ctx, ct) = (ctx.clone(), ct.clone());
        tasks.push(tokio::spawn(async move { telegram::run(ctx, ct).await }));
    }
    if enabled("remote.dingtalk.enabled") {
        let (ctx, ct) = (ctx.clone(), ct.clone());
        tasks.push(tokio::spawn(async move { dingtalk::run(ctx, ct).await }));
    }
    if enabled("remote.weixin.enabled") {
        let (ctx, ct) = (ctx.clone(), ct.clone());
        tasks.push(tokio::spawn(async move { weixin::run(ctx, ct).await }));
    }

    if tasks.is_empty() {
        tracing::info!("远程渠道:无启用项");
        return;
    }
    // 提醒推回手机:有渠道在跑才值得听动静(渠道全关 = 本函数早退,推送随之不在)。
    {
        let (ctx, ct) = (ctx.clone(), ct.clone());
        tasks.push(tokio::spawn(async move { outbound_loop(ctx, ct).await }));
    }
    tracing::info!(count = tasks.len() - 1, "远程渠道已拉起");
    for t in tasks {
        let _ = t.await;
    }
}

/// 提醒推回手机(A1):听全局事件车道,自启回合(提醒/盯天气)收尾且目标是渠道映射会话时,
/// 把回复主动推到平台。Done 推最后一条 assistant 文本;Failed 推提醒原文保底(§3.5 不静默——
/// 到点了人必须收到动静,哪怕回合没跑成)。推送失败只 warn(渠道断线时提醒仍在桌面)。
async fn outbound_loop(ctx: Arc<ChannelCtx>, ct: CancellationToken) {
    let mut rx = ctx.engine.bus().subscribe();
    let net = net::Client::new(|b| b.timeout(std::time::Duration::from_secs(30)));
    loop {
        let ev = tokio::select! {
            _ = ct.cancelled() => return,
            r = rx.recv() => match r {
                Ok(ev) => ev,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            },
        };
        match ev {
            crate::bus::AppEvent::Conversation(act) => {
                // 只管自启回合(提醒/盯天气);渠道入站回合(kind="channel")drive_turn 内回过了
                if act.kind != "reminder" {
                    continue;
                }
                if let Err(e) = push_reminder(&ctx, &net, act.conv_id, act.outcome).await {
                    tracing::warn!(err = %format!("{e:#}"), conv = act.conv_id, "提醒推回渠道失败");
                }
            }
            // 确认闸(§7.8):渠道来源的确认请求推回发起的那个 chat 等回话;
            // 终态卡(超时/桌面先点)清等待表。桌面来源的卡归前端,这里不管。
            crate::bus::AppEvent::Confirm(card) => handle_confirm_card(&ctx, &net, card).await,
            _ => continue,
        }
    }
}

/// 确认卡的渠道半边:pending → 反查映射、把「要点『X』,回『确认』继续」推到发起 chat、
/// 登记等待;推不出去(渠道断/没收件地址)→ **立即按「送达失败」拒**,别让工具白等 120s
/// (via="unreachable",工具层据此如实说「没送到」而不是「用户拒了」)。终态卡 → 清等待。
async fn handle_confirm_card(ctx: &Arc<ChannelCtx>, net: &net::Client, card: crate::confirm::ConfirmCard) {
    if !matches!(card.origin.as_str(), "telegram" | "dingtalk" | "weixin") {
        return; // 桌面来源(ui/system)归前端卡
    }
    if card.state != "pending" {
        ctx.confirm_wait_clear(card.id);
        return;
    }
    let store = ctx.engine.store().clone();
    let conv_id = card.conv_id;
    let thread = tokio::task::spawn_blocking(move || store.channels.thread_by_conv(conv_id))
        .await
        .ok()
        .and_then(|r| r.ok())
        .flatten();
    let Some(thread) = thread else {
        tracing::warn!(conv = conv_id, "确认请求反查不到渠道映射,按送达失败收");
        let _ = ctx.engine.confirmer().resolve(card.id, false, "unreachable");
        return;
    };
    let phrase = confirm_action_phrase(&card.kind, &card.action);
    let prompt = if card.host.is_empty() {
        CONFIRM_PROMPT_BARE.replace("{action}", &phrase)
    } else {
        CONFIRM_PROMPT_SITE.replace("{host}", &card.host).replace("{action}", &phrase)
    };
    // 先登记再推(推到与用户回话之间没有空窗);推失败立刻摘掉
    ctx.confirm_wait_set(&thread.channel, &thread.ext_id, card.id);
    if let Err(e) = push_text_to_thread(ctx, net, &thread, &prompt).await {
        tracing::warn!(err = %format!("{e:#}"), conv = conv_id, "确认请求推不到手机,按送达失败收");
        ctx.confirm_wait_clear(card.id);
        let _ = ctx.engine.confirmer().resolve(card.id, false, "unreachable");
    }
}

// ⚠️ core 侧静态话术(§6.6 债,同 ONBOARD_HINT):Failed 保底推送的前缀,不经模型。
const REMIND_FALLBACK_PREFIX: &str = "提醒到啦:";
/// Failed 保底推的提醒原文截断上限(event 行是物化任务语境,可能到 2000 字;手机上给个头就够)。
const REMIND_FALLBACK_MAX: usize = 200;

/// 推送取料(可单测的纯查库部分):反查映射 + 按终态选文本。
/// None = 非渠道会话(桌面提醒本就不推);文本 None = 库里没得可推(如实放弃)。
/// Done 推最后一条 assistant 回复;Failed 推提醒原文(event 行)截断保底。
fn reminder_push_payload(
    store: &crate::store::Store,
    conv_id: i64,
    outcome: crate::bus::TurnOutcome,
) -> anyhow::Result<Option<(crate::store::ChannelThread, Option<String>)>> {
    let Some(thread) = store.channels.thread_by_conv(conv_id)? else { return Ok(None) };
    let text = match outcome {
        crate::bus::TurnOutcome::Done => store.chat.latest_assistant_line(conv_id)?,
        crate::bus::TurnOutcome::Failed => store.chat.latest_event_line(conv_id)?.map(|c| {
            let head: String = c.chars().take(REMIND_FALLBACK_MAX).collect();
            format!("{REMIND_FALLBACK_PREFIX}{head}")
        }),
    };
    Ok(Some((thread, text)))
}

/// 单条提醒的推送:反查映射 → 取该推的文本 → 按渠道发。非渠道会话 / 渠道未启用 / 没凭证 → 静静跳过
/// (桌面提醒本就不推;渠道被关掉时也别硬推)。
async fn push_reminder(
    ctx: &ChannelCtx,
    net: &net::Client,
    conv_id: i64,
    outcome: crate::bus::TurnOutcome,
) -> anyhow::Result<()> {
    let store = ctx.engine.store().clone();
    let looked =
        tokio::task::spawn_blocking(move || reminder_push_payload(&store, conv_id, outcome))
            .await??;
    let Some((thread, Some(text))) = looked else { return Ok(()) };
    push_text_to_thread(ctx, net, &thread, &text).await
}

/// 往一条渠道映射主动推一段文字(提醒推送与确认请求共用的出口):渠道未启用 / 缺凭证 /
/// 无收件地址 → Err(调用方按语义处理:提醒静静跳过 warn、确认按送达失败收)。
async fn push_text_to_thread(
    ctx: &ChannelCtx,
    net: &net::Client,
    thread: &crate::store::ChannelThread,
    text: &str,
) -> anyhow::Result<()> {
    let enabled = ctx.setting(&format!("remote.{}.enabled", thread.channel)).as_deref() == Some("1");
    anyhow::ensure!(enabled, "渠道 {} 未启用", thread.channel);

    match thread.channel.as_str() {
        "telegram" => {
            let token = ctx.secret("remote.telegram.token").context("没配 Telegram token")?;
            let chat_id: i64 = thread.ext_id.parse().context("Telegram ext_id 非 chat_id")?;
            telegram::push(net, &token, chat_id, text).await
        }
        "dingtalk" => {
            // 单聊入站时存了 senderStaffId;没有(群聊/老映射)= 推不了
            let staff = thread
                .push_id
                .as_deref()
                .context("钉钉对话无推送地址(群聊/旧映射)")?;
            let app_key = ctx.secret("remote.dingtalk.app_key").context("没配钉钉 app_key")?;
            let app_secret =
                ctx.secret("remote.dingtalk.app_secret").context("没配钉钉 app_secret")?;
            dingtalk::push(net, &app_key, &app_secret, staff, text).await
        }
        "weixin" => {
            // 微信主动推送要回显上次的 context_token(存在 push_id 列);没有就推不了
            let ctx_token =
                thread.push_id.as_deref().context("微信对话无 context_token")?;
            // 多绑定:按线程 ext_id(= 绑定者)选对应账号的 token/入口
            let acc = weixin::account_for(&ctx.engine.store().settings, &thread.ext_id)?;
            weixin::push(net, acc.base(), &acc.token, &thread.ext_id, ctx_token, text).await
        }
        other => anyhow::bail!("未知渠道 {other},推不了"),
    }
}

/// 单聊闲置轮换阈值(2026-07-13 用户拍板 12h):同一个人超过这么久没动静,下一条消息
/// 开新会话(老会话留在桌面当历史)。判据 = 会话 `updated_at`(任意角色的最后一条消息,
/// 含提醒到点的推送——刚推过提醒会话就还「热」,用户回「收到」要接得上文,不按
/// 「用户上一条」算);群聊不轮换(支持群聊时再议)。
const FRESH_CONV_IDLE_MS: i64 = 12 * 60 * 60 * 1000;

/// 会话解析(轮换 + 悬空自愈):返回 (conv_id, 指认的家人)。规则 = **chat 的会话 = 名下
/// 最近有动静的那个**;单聊全部凉透(> 12h)才开新会话;名下会话被桌面删光 = 自愈重建
/// (修「删会话后手机永久报错」)。「最近动静的不一定是现行会话」:提醒在轮换走的老会话
/// 到点 → 用户回话接回老会话(上下文都在那);改绑经 `bind` 落新历史行,老行留档
/// (`thread_by_conv` 反查:提醒推送/发起人标签不断链),指认/推送地址随行继承。
/// `fresh` = 建新会话(注入可测);`now` = 毫秒时钟(注入可测,同 memory::maintain)。
fn resolve_conv(
    store: &crate::store::Store,
    channel: &str,
    ext_id: &str,
    single: bool,
    now: i64,
    fresh: impl FnOnce() -> anyhow::Result<crate::store::Conversation>,
) -> anyhow::Result<(i64, Option<i64>)> {
    let Some(t) = store.channels.thread_for(channel, ext_id)? else {
        // 首次来消息:建专属会话并绑定
        let conv = fresh()?;
        store.channels.bind(channel, ext_id, conv.id)?;
        return Ok((conv.id, None));
    };
    let active = store.channels.latest_active_conv(channel, ext_id)?;
    match active {
        Some((conv_id, updated_at))
            if !single || now.saturating_sub(updated_at) <= FRESH_CONV_IDLE_MS =>
        {
            if conv_id != t.conv_id {
                // 动静在老会话(提醒刚到点):接回它,现行指针跟着挪
                store.channels.bind(channel, ext_id, conv_id)?;
            }
            Ok((conv_id, t.user_id))
        }
        // 全凉透(单聊 12h 轮换)/ 名下会话全被删(悬空自愈)→ 开新会话
        _ => {
            let conv = fresh()?;
            store.channels.bind(channel, ext_id, conv.id)?; // 新行继承指认/昵称/推送地址
            Ok((conv.id, t.user_id))
        }
    }
}

/// 把一条入站文本喂进引擎、攒出回复(**渠道无关**,复用 turn loop —— 这是"渠道复用回合循环"的兑现)。
/// `sender_label` = 平台昵称(有就顺手记进映射,给家人页认脸,不参与逻辑)。
/// 渠道归人:该 chat 若被指认给某家人(家人页设置),回合带 `speaker_user` —— 记忆/需知/提醒
/// 归 TA(与桌面声纹同一条 `UserMeta` 缝);未指认 = None,零行为变化。
/// `attachments` = 手机发来的图(桌面同缝:当轮注入、不落库);`input` = 输入形态
/// (`Some("voice")` = 语音消息转写,只作 payload 事实记录,不置 speak —— 渠道回复是文字)。
/// `single` = 单聊(12h 闲置轮换只对单聊;群聊维持永久续接,见 `resolve_conv`)。
/// 返回 `None` = 已折进在飞回合(inject,沿用桌面前端语义,本条不单独回);
/// `Some(text)` = 完整回复,调用方按平台限长 `split_message` 后发出。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn drive_turn(
    engine: &Engine,
    channel: &str,
    ext_id: &str,
    text: String,
    sender_label: Option<&str>,
    attachments: Vec<InAttachment>,
    input: Option<&str>,
    single: bool,
) -> anyhow::Result<Option<String>> {
    let store = engine.store().clone();
    // 会话映射:续接名下最近有动静的会话;单聊闲置 12h 轮换、映射悬空自愈(resolve_conv)
    let (conv_id, speaker) =
        resolve_conv(&store, channel, ext_id, single, crate::store::now_ms(), || {
            Ok(engine.new_conversation(channel)?)
        })?;
    if let Some(label) = sender_label {
        let _ = store.channels.set_label(channel, ext_id, label); // 尽力件,失败不挡回合
    }
    // 指认过的家人才带 speaker(悬空 id 由 send_message 的存在性校验兜底 → 回落会话用户);
    // 语音消息带 input 形态(payload 物化,speak 恒 false —— 〔语音〕说话守则只对要念的回合生效)
    let meta = (speaker.is_some() || input.is_some()).then(|| UserMeta {
        speaker_user: speaker,
        input: input.unwrap_or_default().to_string(),
        ..Default::default()
    });

    // 在飞 → 插队(折进当前回合);否则起新回合。与桌面前端 inject-or-send 同语义。
    if engine.inject(conv_id, text.clone(), meta.clone(), attachments.clone()).await {
        return Ok(None);
    }
    let mut rx = engine.send_message(conv_id, text, meta, attachments).await?;
    let mut buf = String::new();
    while let Some(ev) = rx.recv().await {
        match ev {
            TurnEvent::Delta(t) => buf.push_str(&t),
            TurnEvent::Failed { message, .. } => anyhow::bail!("回合失败: {message}"),
            TurnEvent::Done { .. } => break,
            // Thinking/ToolUse/Usage/Segment/Injected/Cancelled:远程纯文本不需要,忽略
            _ => {}
        }
    }
    // 渠道回合收尾 → 经全局事件车道喊一声(同 wake_turn 自启回合):前端据此刷新「最近」列表,
    // 否则渠道新建的会话要重启 app 才出现(用户 2026-06-19 实测)。
    engine.bus().publish(crate::bus::AppEvent::Conversation(crate::bus::ConversationActivity {
        conv_id,
        kind: "channel".into(),
        outcome: crate::bus::TurnOutcome::Done,
    }));
    Ok(Some(buf))
}

/// 长消息按平台上限切片(优先在换行处断,放不下才硬切;robot Telegram 4096 同款)。
/// 按字符计数(中英安全近似);返回去空白后的非空片。
pub(crate) fn split_message(text: &str, max: usize) -> Vec<String> {
    let text = text.trim();
    if text.is_empty() {
        return Vec::new();
    }
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        return vec![text.to_string()];
    }
    let mut out = Vec::new();
    let mut start = 0;
    while start < chars.len() {
        let end = (start + max).min(chars.len());
        // 窗口内最后一个换行作断点(留给下一片);没有则硬切到 end
        let cut = if end < chars.len() {
            chars[start..end]
                .iter()
                .rposition(|&c| c == '\n')
                .map(|p| start + p)
                .filter(|&p| p > start)
                .unwrap_or(end)
        } else {
            end
        };
        let piece: String = chars[start..cut].iter().collect();
        let piece = piece.trim().to_string();
        if !piece.is_empty() {
            out.push(piece);
        }
        start = cut;
        while start < chars.len() && chars[start] == '\n' {
            start += 1; // 跳过分界换行
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_keeps_short_intact() {
        assert_eq!(split_message("你好", 100), vec!["你好"]);
        assert!(split_message("   ", 100).is_empty());
    }

    fn test_ctx(tag: &str) -> (Arc<ChannelCtx>, Arc<Engine>) {
        let dir = std::env::temp_dir().join(format!("lw-chconfirm-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _ = std::fs::remove_file(dir.join("t.db"));
        let store = crate::store::Store::open(&dir.join("t.db")).unwrap();
        store.users.ensure_default_user().unwrap();
        let engine = Engine::new(store.clone(), crate::scenes::Scenes::builtin());
        let ctx = Arc::new(ChannelCtx {
            engine: engine.clone(),
            voice: crate::voice::VoiceRuntime::new(
                dir,
                store.clone(),
                crate::bus::Bus::new(),
                crate::scenes::Scenes::builtin(),
            ),
            media: crate::media::MediaRuntime::detached(store),
            status: ChannelStatus::default(),
            attach_buf: AttachBuffer::default(),
            confirm_waits: Mutex::new(HashMap::new()),
        });
        (ctx, engine)
    }

    fn channel_ask(conv_id: i64) -> crate::confirm::ConfirmAsk {
        crate::confirm::ConfirmAsk {
            user_id: 1,
            conv_id,
            origin: "weixin".into(),
            host: "x.example.com".into(),
            action: "确认支付 ¥128.00".into(),
            kind: "click".into(),
        }
    }

    #[test]
    fn confirm_action_phrase_by_kind() {
        assert_eq!(confirm_action_phrase("click", "确认支付"), "点『确认支付』");
        assert_eq!(confirm_action_phrase("submit", "立即购买"), "提交『立即购买』");
        assert_eq!(confirm_action_phrase("submit", ""), "提交这个表单");
        assert_eq!(confirm_action_phrase("press", "Enter"), "按 Enter 键");
    }

    /// 渠道回话拦截:挂着确认时,非肯定回话 = 拒 + 照常进回合;别的 chat 不受影响;
    /// 应答后等待表即清。
    #[tokio::test]
    async fn confirm_reply_deny_falls_through_to_turn() {
        let (ctx, engine) = test_ctx("deny");
        // 没挂确认:不拦(照常进回合)
        assert!(ctx.confirm_reply("weixin", "u1", "确认").is_none());

        let _sub = engine.bus().subscribe(); // 有订阅者 ask 才不走 NoUi
        let confirmer = engine.confirmer().clone();
        let c2 = confirmer.clone();
        let task = tokio::spawn(async move {
            c2.ask(channel_ask(7), std::time::Duration::from_secs(10)).await
        });
        while confirmer.pending_for_conv(7).is_none() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let id = confirmer.pending_for_conv(7).unwrap().id;
        ctx.confirm_wait_set("weixin", "u1", id);

        // 别的 chat / 别的渠道:不拦
        assert!(ctx.confirm_reply("weixin", "u2", "确认").is_none());
        assert!(ctx.confirm_reply("telegram", "u1", "确认").is_none());
        // 非肯定回话 = 拒(resolve deny)+ None(这句照常进回合让模型接)
        assert!(ctx.confirm_reply("weixin", "u1", "先别,改成到店自提").is_none());
        assert_eq!(
            task.await.unwrap(),
            crate::confirm::ConfirmDecision::Denied { via: "channel".into() }
        );
        // 等待表已清:同 chat 再说「确认」不再拦
        assert!(ctx.confirm_reply("weixin", "u1", "确认").is_none());
    }

    /// 肯定回话 = 回执即回、不进回合;卡已收尾时回「确认」= 如实说过期。
    #[tokio::test]
    async fn confirm_reply_allow_receipt_and_expired_honesty() {
        let (ctx, engine) = test_ctx("allow");
        let _sub = engine.bus().subscribe();
        let confirmer = engine.confirmer().clone();
        let c2 = confirmer.clone();
        let task = tokio::spawn(async move {
            c2.ask(channel_ask(9), std::time::Duration::from_secs(10)).await
        });
        while confirmer.pending_for_conv(9).is_none() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let id = confirmer.pending_for_conv(9).unwrap().id;
        ctx.confirm_wait_set("weixin", "u1", id);
        assert_eq!(ctx.confirm_reply("weixin", "u1", "确认"), Some(CONFIRM_ACK));
        assert_eq!(
            task.await.unwrap(),
            crate::confirm::ConfirmDecision::Allowed { via: "channel".into() }
        );
        // 挂一个已经收尾的 id(超时/桌面先点):回「确认」如实说过期,不进回合
        ctx.confirm_wait_set("weixin", "u1", id);
        assert_eq!(ctx.confirm_reply("weixin", "u1", "确认"), Some(CONFIRM_EXPIRED));
        // 终态清扫入口:按 id 摘(outbound_loop 终态卡路径)
        ctx.confirm_wait_set("weixin", "u3", 424242);
        ctx.confirm_wait_clear(424242);
        assert!(ctx.confirm_reply("weixin", "u3", "确认").is_none());
    }

    #[test]
    fn attach_buffer_debounces_and_merges() {
        let att = |n: &str| InAttachment { name: n.into(), mime: "application/pdf".into(), data: String::new() };
        let buf = AttachBuffer::default();

        // 连发 3 个文件:只第一个「本批第一个」= 要提示,后两个静默(防抖:不提示三次)
        assert!(buf.buffer("weixin", "u1", vec![att("a.pdf")]), "第一个要提示");
        assert!(!buf.buffer("weixin", "u1", vec![att("b.pdf")]), "第二个静默");
        assert!(!buf.buffer("weixin", "u1", vec![att("c.pdf")]), "第三个静默");

        // 用户发文字 → 取走全部攒的(连同文字一起进回合),缓冲清空
        let taken = buf.take("weixin", "u1");
        assert_eq!(taken.len(), 3, "三个文件一起交出");
        assert!(buf.take("weixin", "u1").is_empty(), "取过即空");

        // 取空后下一个文件又算「本批第一个」(重新提示);不同人 / 不同渠道各自独立
        assert!(buf.buffer("weixin", "u1", vec![att("d.pdf")]), "新一批重新提示");
        assert!(buf.buffer("weixin", "u2", vec![att("x.pdf")]), "另一个人独立成批");
        assert!(buf.buffer("dingtalk", "u1", vec![att("y.pdf")]), "另一渠道独立成批");
        assert!(buf.take("telegram", "nobody").is_empty(), "没攒过 = 空");
    }

    /// 会话解析全链:首建 → 续接 → 12h 轮换(继承指认)→ 提醒唤醒老会话接回 → 删光自愈 → 群聊不轮换。
    #[test]
    fn resolve_conv_rotates_heals_and_keeps_groups() {
        let p = std::env::temp_dir().join(format!("lw-chan-resolve-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&p);
        let store = crate::store::Store::open(&p).unwrap();
        let user = store.users.ensure_default_user().unwrap();
        let kid = store.users.create("小朋友").unwrap();
        let fresh =
            || Ok(store.chat.create_conversation_full(user.id, "companion", "telegram").unwrap());
        let now = crate::store::now_ms();
        const H12: i64 = FRESH_CONV_IDLE_MS;

        // 首次来消息:建会话并绑定
        let (c1, sp) = resolve_conv(&store, "telegram", "42", true, now, fresh).unwrap();
        assert_eq!(sp, None);
        assert_eq!(store.channels.conv_for("telegram", "42").unwrap(), Some(c1));

        // 12h 内回访:续接同一会话;指认给家人后说话人跟着带
        let t = store.channels.thread_for("telegram", "42").unwrap().unwrap();
        store.channels.bind_user(t.id, Some(kid.id)).unwrap();
        let (again, sp) = resolve_conv(&store, "telegram", "42", true, now + H12, fresh).unwrap();
        assert_eq!((again, sp), (c1, Some(kid.id)), "12h 整还没过线,续接");

        // 超 12h 没动静:轮换新会话,指认随历史行继承、说话人不丢
        let (c2, sp) = resolve_conv(&store, "telegram", "42", true, now + H12 + 60_000, fresh).unwrap();
        assert_ne!(c2, c1, "闲置超时开新会话");
        assert_eq!(sp, Some(kid.id));
        let t = store.channels.thread_for("telegram", "42").unwrap().unwrap();
        assert_eq!((t.conv_id, t.user_id), (c2, Some(kid.id)), "现行指针挪到新会话,指认继承");
        assert!(store.channels.thread_by_conv(c1).unwrap().is_some(), "老会话反查不断链");

        // 提醒在老会话到点(updated_at 被叫醒)→ 回话接回老会话,现行指针跟着挪
        std::thread::sleep(std::time::Duration::from_millis(2)); // 毫秒时间戳拉开平局
        store.chat.append_message(c1, "event", "提醒用户:三点吃药").unwrap();
        let (back, _) =
            resolve_conv(&store, "telegram", "42", true, crate::store::now_ms(), fresh).unwrap();
        assert_eq!(back, c1, "动静在老会话:接回去(用户回「收到」有上文)");
        assert_eq!(store.channels.conv_for("telegram", "42").unwrap(), Some(c1));

        // 名下会话全被桌面删光:自愈重建,不再永久报错
        // (SQLite rowid 会复用刚删的 id,判「新」看存在性不看 id 值)
        store.chat.delete_conversation(c1).unwrap();
        store.chat.delete_conversation(c2).unwrap();
        let (c3, sp) = resolve_conv(&store, "telegram", "42", true, now, fresh).unwrap();
        assert!(store.chat.get_conversation(c3).unwrap().is_some(), "悬空自愈 = 重建了活会话");
        assert_eq!(sp, Some(kid.id), "自愈也不丢指认");
        assert_eq!(store.channels.conv_for("telegram", "42").unwrap(), Some(c3));

        // 群聊(single=false):再久也不轮换
        let (g, _) =
            resolve_conv(&store, "telegram", "42", false, now + 400 * 24 * H12, fresh).unwrap();
        assert_eq!(g, c3, "群聊维持永久续接");
    }

    #[test]
    fn reminder_push_payload_selects_text_by_outcome() {
        let p = std::env::temp_dir()
            .join(format!("lw-chan-push-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&p);
        let store = crate::store::Store::open(&p).unwrap();
        let user = store.users.ensure_default_user().unwrap();
        let conv =
            store.chat.create_conversation_full(user.id, "companion", "telegram").unwrap();

        // 桌面会话(无映射)→ None,不推
        assert!(reminder_push_payload(&store, conv.id, crate::bus::TurnOutcome::Done)
            .unwrap()
            .is_none());

        store.channels.bind("telegram", "12345", conv.id).unwrap();
        store.chat.append_message(conv.id, "event", "提醒用户:三点吃药").unwrap();
        store.chat.append_message(conv.id, "assistant", "到点啦,该吃药了哦").unwrap();

        // Done → 推最后一条 assistant 回复
        let (t, text) = reminder_push_payload(&store, conv.id, crate::bus::TurnOutcome::Done)
            .unwrap()
            .unwrap();
        assert_eq!(t.ext_id, "12345");
        assert_eq!(text.as_deref(), Some("到点啦,该吃药了哦"));

        // Failed → 推 event 行原文(带前缀,截断保底)
        let (_, text) = reminder_push_payload(&store, conv.id, crate::bus::TurnOutcome::Failed)
            .unwrap()
            .unwrap();
        let text = text.unwrap();
        assert!(text.starts_with(REMIND_FALLBACK_PREFIX));
        assert!(text.contains("三点吃药"));
    }

    #[test]
    fn split_prefers_newline_then_hard_cuts() {
        // 三行,每行 4 字;max=5 → 应在换行处断,不硬切到行中
        let text = "AAAA\nBBBB\nCCCC";
        let parts = split_message(text, 5);
        assert!(parts.iter().all(|p| p.chars().count() <= 5), "每片不超上限");
        assert_eq!(parts.concat().replace('\n', ""), "AAAABBBBCCCC");
        // 无换行的超长行:硬切
        let long = "x".repeat(12);
        let parts = split_message(&long, 5);
        assert_eq!(parts, vec!["xxxxx", "xxxxx", "xx"]);
    }
}
