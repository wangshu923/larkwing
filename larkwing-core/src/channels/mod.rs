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

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use tokio_util::sync::CancellationToken;

use crate::engine::{Engine, InAttachment, TurnEvent, UserMeta};
use crate::net;

/// 渠道连接状态(给设置页状态行;不静默失败 §3.5)。
#[derive(Clone, Default, serde::Serialize)]
pub struct ChannelState {
    pub running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// 共享状态表:各渠道写,`remote_status` 命令读(channel → 状态)。
pub type ChannelStatus = Arc<Mutex<HashMap<String, ChannelState>>>;

/// 渠道适配器运行上下文(引擎 + 语音/媒体运行时 + 状态回写)。pub(crate):仅 channels 内部构造。
pub(crate) struct ChannelCtx {
    pub engine: Arc<Engine>,
    /// 本地 ASR(手机语音消息转写);模块头注释:channels 组合它、它不认识 channels。
    pub voice: crate::voice::VoiceRuntime,
    /// ffmpeg 解码(语音消息 ogg/opus → PCM);同上,只消费公开 API。
    pub media: crate::media::MediaRuntime,
    pub status: ChannelStatus,
}

impl ChannelCtx {
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
    let ctx = Arc::new(ChannelCtx { engine, voice, media, status });
    let mut tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();

    if enabled("remote.telegram.enabled") {
        let (ctx, ct) = (ctx.clone(), ct.clone());
        tasks.push(tokio::spawn(async move { telegram::run(ctx, ct).await }));
    }
    if enabled("remote.dingtalk.enabled") {
        let (ctx, ct) = (ctx.clone(), ct.clone());
        tasks.push(tokio::spawn(async move { dingtalk::run(ctx, ct).await }));
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
        let crate::bus::AppEvent::Conversation(act) = ev else { continue };
        // 只管自启回合(提醒/盯天气);渠道入站回合(kind="channel")已在 drive_turn 内回过了
        if act.kind != "reminder" {
            continue;
        }
        if let Err(e) = push_reminder(&ctx, &net, act.conv_id, act.outcome).await {
            tracing::warn!(err = %format!("{e:#}"), conv = act.conv_id, "提醒推回渠道失败");
        }
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
    let enabled = ctx.setting(&format!("remote.{}.enabled", thread.channel)).as_deref() == Some("1");
    if !enabled {
        return Ok(());
    }

    match thread.channel.as_str() {
        "telegram" => {
            let token = ctx.secret("remote.telegram.token").context("没配 Telegram token")?;
            let chat_id: i64 = thread.ext_id.parse().context("Telegram ext_id 非 chat_id")?;
            telegram::push(net, &token, chat_id, &text).await
        }
        "dingtalk" => {
            // 单聊入站时存了 senderStaffId;没有(群聊/老映射)= 推不了,如实跳过并留日志
            let Some(staff) = thread.push_id.as_deref() else {
                tracing::info!(conv = conv_id, "钉钉对话无推送地址(群聊/旧映射),提醒只留桌面");
                return Ok(());
            };
            let app_key = ctx.secret("remote.dingtalk.app_key").context("没配钉钉 app_key")?;
            let app_secret =
                ctx.secret("remote.dingtalk.app_secret").context("没配钉钉 app_secret")?;
            dingtalk::push(net, &app_key, &app_secret, staff, &text).await
        }
        other => {
            tracing::warn!(channel = other, "未知渠道,提醒不推");
            Ok(())
        }
    }
}

/// 把一条入站文本喂进引擎、攒出回复(**渠道无关**,复用 turn loop —— 这是"渠道复用回合循环"的兑现)。
/// `sender_label` = 平台昵称(有就顺手记进映射,给家人页认脸,不参与逻辑)。
/// 渠道归人:该 chat 若被指认给某家人(家人页设置),回合带 `speaker_user` —— 记忆/需知/提醒
/// 归 TA(与桌面声纹同一条 `UserMeta` 缝);未指认 = None,零行为变化。
/// `attachments` = 手机发来的图(桌面同缝:当轮注入、不落库);`input` = 输入形态
/// (`Some("voice")` = 语音消息转写,只作 payload 事实记录,不置 speak —— 渠道回复是文字)。
/// 返回 `None` = 已折进在飞回合(inject,沿用桌面前端语义,本条不单独回);
/// `Some(text)` = 完整回复,调用方按平台限长 `split_message` 后发出。
pub(crate) async fn drive_turn(
    engine: &Engine,
    channel: &str,
    ext_id: &str,
    text: String,
    sender_label: Option<&str>,
    attachments: Vec<InAttachment>,
    input: Option<&str>,
) -> anyhow::Result<Option<String>> {
    let store = engine.store().clone();
    // 会话映射:回访续接同一会话(send_message 自带历史回放),首次建专属会话并绑定
    let (conv_id, speaker) = match store.channels.thread_for(channel, ext_id)? {
        Some(t) => (t.conv_id, t.user_id),
        None => {
            let conv = engine.new_conversation(channel)?;
            store.channels.bind(channel, ext_id, conv.id)?;
            (conv.id, None)
        }
    };
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
