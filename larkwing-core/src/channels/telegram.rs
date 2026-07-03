//! Telegram bot 适配器:raw Bot API over `net::Client`(**不用 teloxide**——它自带 reqwest 会绕过
//! net::Client / 破 §4.6 代理选路)。入站 `getUpdates` 长轮询(offset 防重放),出站 `sendMessage`
//! (**纯文本、不带 parse_mode** —— 绕开 MarkdownV2 转义地狱这个 robot 大坑)。免公网、免 SDK。
//! 国内 api.telegram.org 多半要代理:net 直连失败自动落代理(§4.6),无需本模块操心。

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::{drive_turn, split_message, ChannelCtx};
use crate::net;

const CHANNEL: &str = "telegram";
const API: &str = "https://api.telegram.org";
/// Telegram 单条上限 4096;留余量。
const TG_MAX: usize = 4000;
/// 长轮询服务端挂起窗口(秒);net client 超时须 > 它。
const POLL_TIMEOUT_S: u64 = 50;

// ⚠️ core 侧静态话术(§6.6 本应数据化,如语音的场景 JSON 话术)——MVP 先内联,记债待迁。
//    这两条是渠道**操作性**提示(非人格),不经模型。
const ONBOARD_HINT: &str =
    "你好,我是旺财。你的 chat id 是 {id},把它加到设置·远程渠道的白名单里,我们就能聊啦。";
const ERR_HINT: &str = "(出了点问题,稍后再试试)";

/// 渠道入口:建长超时 net client,跑服务循环;出错退避重连(不静默失败 §3.5)。
pub(super) async fn run(ctx: Arc<ChannelCtx>, ct: CancellationToken) {
    let net = net::Client::new(|b| b.timeout(Duration::from_secs(POLL_TIMEOUT_S + 20)));
    while !ct.is_cancelled() {
        ctx.set_state(CHANNEL, true, None);
        match serve(&ctx, &net, &ct).await {
            Ok(()) => break, // 正常返回 = 被取消
            Err(e) => {
                let msg = format!("{e:#}");
                tracing::warn!(err = %msg, "Telegram 渠道出错,5s 后重连");
                ctx.set_state(CHANNEL, false, Some(msg));
                tokio::select! {
                    _ = ct.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                }
            }
        }
    }
    ctx.set_state(CHANNEL, false, None);
    tracing::info!("Telegram 渠道已停");
}

async fn serve(ctx: &ChannelCtx, net: &net::Client, ct: &CancellationToken) -> Result<()> {
    let token = ctx.secret("remote.telegram.token").context("没配 Telegram token")?;
    let allowed = allowed_chats(ctx);
    // 启动丢弃积压(等价 drop_pending_updates):从最后一条之后开始
    let mut offset = latest_offset(net, &token).await;
    tracing::info!(offset, allow = allowed.len(), "Telegram 渠道在线");

    loop {
        if ct.is_cancelled() {
            return Ok(());
        }
        // 长轮询期间也能被取消(否则要等满 50s)
        let updates = tokio::select! {
            _ = ct.cancelled() => return Ok(()),
            r = get_updates(net, &token, offset, POLL_TIMEOUT_S) => r?,
        };
        for upd in updates {
            if let Some(id) = upd.get("update_id").and_then(Value::as_i64) {
                offset = offset.max(id + 1);
            }
            let Some((chat_id, text, sender)) = parse_message(&upd) else { continue };
            let chat = chat_id.to_string();

            // 访问控制(非风控 §9):配了白名单就只放行名单内;空白名单 = 谁来都先发 onboarding
            if !allowed.is_empty() {
                if !allowed.contains(&chat) {
                    continue; // 已设名单的陌生 chat:静默忽略
                }
            } else {
                let _ = send_message(net, &token, chat_id, &ONBOARD_HINT.replace("{id}", &chat)).await;
                continue;
            }

            // 复用 turn loop:攒回复 → 按 4096 分片发回
            match drive_turn(&ctx.engine, CHANNEL, &chat, text, sender.as_deref()).await {
                Ok(Some(reply)) => {
                    for piece in split_message(&reply, TG_MAX) {
                        if let Err(e) = send_message(net, &token, chat_id, &piece).await {
                            tracing::warn!(err = %format!("{e:#}"), "Telegram 发送失败");
                        }
                    }
                }
                Ok(None) => {} // 折进在飞回合(inject),不单独回
                Err(e) => {
                    tracing::warn!(err = %format!("{e:#}"), "Telegram 回合失败");
                    let _ = send_message(net, &token, chat_id, ERR_HINT).await;
                }
            }
        }
    }
}

/// 启动时取"最后一条积压之后"的 offset(offset=-1 拿最后一条;+1 = 跳过积压)。
async fn latest_offset(net: &net::Client, token: &str) -> i64 {
    match get_updates(net, token, -1, 0).await {
        Ok(updates) => updates
            .last()
            .and_then(|u| u.get("update_id").and_then(Value::as_i64))
            .map(|id| id + 1)
            .unwrap_or(0),
        Err(e) => {
            tracing::warn!(err = %format!("{e:#}"), "Telegram 取初始 offset 失败,从 0 开始");
            0
        }
    }
}

async fn get_updates(net: &net::Client, token: &str, offset: i64, timeout_s: u64) -> Result<Vec<Value>> {
    let url = format!("{API}/bot{token}/getUpdates?offset={offset}&timeout={timeout_s}");
    let resp = net.send(&url, |c| c.get(&url)).await.context("getUpdates 请求失败")?;
    let body: Value = resp.json().await.context("getUpdates 响应非 JSON")?;
    if body.get("ok").and_then(Value::as_bool) != Some(true) {
        let desc = body.get("description").and_then(Value::as_str).unwrap_or("unknown");
        anyhow::bail!("Telegram getUpdates 失败: {desc}");
    }
    Ok(body.get("result").and_then(Value::as_array).cloned().unwrap_or_default())
}

async fn send_message(net: &net::Client, token: &str, chat_id: i64, text: &str) -> Result<()> {
    let url = format!("{API}/bot{token}/sendMessage");
    let body = serde_json::json!({ "chat_id": chat_id, "text": text }); // 无 parse_mode = 字面文本
    let resp = net.send(&url, |c| c.post(&url).json(&body)).await.context("sendMessage 请求失败")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let detail = resp.text().await.unwrap_or_default();
        anyhow::bail!("sendMessage HTTP {status}: {detail}");
    }
    Ok(())
}

/// 从 update 取 (chat_id, text, 发送者昵称);非文本消息 → None。
/// 昵称 = from.first_name(+ last_name),只给家人页认脸,取不到无妨。
fn parse_message(upd: &Value) -> Option<(i64, String, Option<String>)> {
    let msg = upd.get("message")?;
    let text = msg.get("text").and_then(Value::as_str)?.trim().to_string();
    if text.is_empty() {
        return None;
    }
    let chat_id = msg.get("chat")?.get("id").and_then(Value::as_i64)?;
    let sender = msg.get("from").map(|f| {
        let first = f.get("first_name").and_then(Value::as_str).unwrap_or("");
        let last = f.get("last_name").and_then(Value::as_str).unwrap_or("");
        format!("{first} {last}").trim().to_string()
    });
    Some((chat_id, text, sender.filter(|s| !s.is_empty())))
}

/// 白名单(逗号/空格/分号/换行分隔的 chat id);空 = 未配置。
fn allowed_chats(ctx: &ChannelCtx) -> Vec<String> {
    ctx.setting("remote.telegram.allowed_chats")
        .map(|s| {
            s.split([',', '，', ' ', ';', '；', '\n'])
                .map(|x| x.trim().to_string())
                .filter(|x| !x.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_message_extracts_text_chat_and_sender() {
        let upd = serde_json::json!({
            "update_id": 10,
            "message": {
                "text": "  在吗  ",
                "chat": { "id": 12345 },
                "from": { "first_name": "豆豆", "last_name": "" }
            }
        });
        assert_eq!(parse_message(&upd), Some((12345, "在吗".to_string(), Some("豆豆".into()))));
        // 无 from(频道帖等):昵称 None,消息照收
        let bare = serde_json::json!({ "message": { "text": "hi", "chat": { "id": 1 } } });
        assert_eq!(parse_message(&bare), Some((1, "hi".to_string(), None)));
    }

    #[test]
    fn parse_message_skips_non_text() {
        let photo = serde_json::json!({ "message": { "chat": { "id": 1 }, "photo": [] } });
        assert_eq!(parse_message(&photo), None);
        let empty = serde_json::json!({ "message": { "text": "   ", "chat": { "id": 1 } } });
        assert_eq!(parse_message(&empty), None);
    }
}
