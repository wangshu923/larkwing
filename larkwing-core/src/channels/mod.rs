//! 远程交互渠道(PLAN 远程渠道):引擎边界适配器,**复用 turn loop**(AGENT §5 species ②)。
//! 不是工具、不碰 ToolCtx、不内嵌人格;一渠道一文件 + 这里注册。
//! 监督器按 settings 决定起哪些渠道;每渠道一个带 `CancellationToken` 的任务,崩溃退避重连
//! (不静默失败 §3.5,状态进 `ChannelStatus` 给设置页)。改配置/开关后由 `Supervisor::restart` 停旧起新。
//! 出站 HTTP 全走 `net::Client`(§4.6)。

mod dingtalk;
mod telegram;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;

use crate::engine::{Engine, TurnEvent};

/// 渠道连接状态(给设置页状态行;不静默失败 §3.5)。
#[derive(Clone, Default, serde::Serialize)]
pub struct ChannelState {
    pub running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// 共享状态表:各渠道写,`remote_status` 命令读(channel → 状态)。
pub type ChannelStatus = Arc<Mutex<HashMap<String, ChannelState>>>;

/// 渠道适配器运行上下文(引擎 + 状态回写)。pub(crate):仅 channels 内部构造。
pub(crate) struct ChannelCtx {
    pub engine: Arc<Engine>,
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
pub async fn run(engine: Arc<Engine>, status: ChannelStatus, ct: CancellationToken) {
    let store = engine.store().clone();
    let enabled = |k: &str| store.settings.get(None, k).ok().flatten().as_deref() == Some("1");
    let ctx = Arc::new(ChannelCtx { engine, status });
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
    tracing::info!(count = tasks.len(), "远程渠道已拉起");
    for t in tasks {
        let _ = t.await;
    }
}

/// 把一条入站文本喂进引擎、攒出回复(**渠道无关**,复用 turn loop —— 这是"渠道复用回合循环"的兑现)。
/// `sender_label` = 平台昵称(有就顺手记进映射,给家人页认脸,不参与逻辑)。
/// 渠道归人:该 chat 若被指认给某家人(家人页设置),回合带 `speaker_user` —— 记忆/需知/提醒
/// 归 TA(与桌面声纹同一条 `UserMeta` 缝);未指认 = None,零行为变化。
/// 返回 `None` = 已折进在飞回合(inject,沿用桌面前端语义,本条不单独回);
/// `Some(text)` = 完整回复,调用方按平台限长 `split_message` 后发出。
pub(crate) async fn drive_turn(
    engine: &Engine,
    channel: &str,
    ext_id: &str,
    text: String,
    sender_label: Option<&str>,
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
    // 指认过的家人才带 speaker(悬空 id 由 send_message 的存在性校验兜底 → 回落会话用户)
    let meta = speaker.map(|uid| crate::engine::UserMeta {
        speaker_user: Some(uid),
        ..Default::default()
    });

    // 在飞 → 插队(折进当前回合);否则起新回合。与桌面前端 inject-or-send 同语义。
    if engine.inject(conv_id, text.clone(), meta.clone(), Vec::new()).await {
        return Ok(None);
    }
    let mut rx = engine.send_message(conv_id, text, meta, Vec::new()).await?;
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
