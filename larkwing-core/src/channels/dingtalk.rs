//! 钉钉 bot 适配器:官方 **Stream 模式**(WebSocket,免公网,robot 同款)。
//! ① 开连接走 `net::Client`(HTTP)拿 endpoint+ticket;② WS 连上**只用来收**消息 + 回 ACK/pong;
//! ③ 回复 POST `sessionWebhook`(HTTP/`net::Client`)—— 回复不占 WS,故回合可异步 spawn、不阻塞收循环、不丢 ping。
//! 钉钉是国内服务:WS 直连(不经代理);TLS 走 rustls(进程级 aws-lc provider 已在壳层 lib.rs 装好)。

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

use super::{drive_turn, ChannelCtx};
use crate::net;

const CHANNEL: &str = "dingtalk";
const OPEN_URL: &str = "https://api.dingtalk.com/v1.0/gateway/connections/open";
const BOT_TOPIC: &str = "/v1.0/im/bot/messages/get";

// ⚠️ core 侧静态话术(§6.6 本应数据化,同 telegram):钉钉回合出错的兜底,不经模型。
const ERR_HINT: &str = "(出了点问题,稍后再试试)";
// 钉钉的图/语音下载链(downloadCode 换直链)是另一套,本批先如实告知(§3.5 不静默;TG 已全量)。
const UNSUPPORTED_HINT: &str = "在钉钉上我暂时只能看文字,图片和语音先打字告诉我哈。";

/// 渠道入口:建 net client,跑服务循环;断开/出错退避重连(不静默失败 §3.5)。
pub(super) async fn run(ctx: Arc<ChannelCtx>, ct: CancellationToken) {
    let net = Arc::new(net::Client::new(|b| b.timeout(Duration::from_secs(30))));
    while !ct.is_cancelled() {
        ctx.set_state(CHANNEL, true, None);
        if let Err(e) = serve(&ctx, &net, &ct).await {
            // 出错(含正常的网关轮换断开抛错):记错 + 状态行可见,5s 后重连
            tracing::warn!(err = %format!("{e:#}"), "钉钉渠道出错,5s 后重连");
            ctx.set_state(CHANNEL, false, Some(format!("{e:#}")));
        }
        if ct.is_cancelled() {
            break;
        }
        tokio::select! {
            _ = ct.cancelled() => break,
            _ = tokio::time::sleep(Duration::from_secs(5)) => {} // 网关轮换/断线 → 重连
        }
    }
    ctx.set_state(CHANNEL, false, None);
    tracing::info!("钉钉渠道已停");
}

/// 一次连接的生命周期:开连接 → WS 收发 →(取消 / 网关 disconnect / 断流 → 返回,由 run 重连)。
async fn serve(ctx: &Arc<ChannelCtx>, net: &Arc<net::Client>, ct: &CancellationToken) -> Result<()> {
    let app_key = ctx.secret("remote.dingtalk.app_key").context("没配钉钉 app_key")?;
    let app_secret = ctx.secret("remote.dingtalk.app_secret").context("没配钉钉 app_secret")?;
    let (endpoint, ticket) = open_connection(net, &app_key, &app_secret).await?;

    let url = format!("{endpoint}?ticket={ticket}");
    let (mut ws, _) = tokio_tungstenite::connect_async(url.as_str()).await.context("钉钉 WS 连接失败")?;
    tracing::info!("钉钉 Stream 在线");

    loop {
        let msg = tokio::select! {
            _ = ct.cancelled() => return Ok(()),
            m = ws.next() => match m {
                Some(Ok(m)) => m,
                Some(Err(e)) => return Err(e).context("钉钉 WS 读出错"),
                None => return Ok(()), // 断流 → run 重连
            },
        };
        let text = match msg {
            Message::Text(t) => t.as_str().to_string(),
            Message::Ping(p) => {
                let _ = ws.send(Message::Pong(p)).await; // 传输层 keepalive
                continue;
            }
            Message::Close(_) => return Ok(()), // 网关关连接 → run 重连
            _ => continue,
        };

        let frame: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let topic = frame.pointer("/headers/topic").and_then(Value::as_str).unwrap_or("");
        let message_id =
            frame.pointer("/headers/messageId").and_then(Value::as_str).unwrap_or("").to_string();

        match topic {
            // 应用层心跳:回 pong(echo data),保活
            "ping" => {
                let data = frame.get("data").and_then(Value::as_str).unwrap_or("{}");
                let _ = ws.send(Message::Text(ack_frame(&message_id, data).into())).await;
            }
            // 网关要求断开(连接轮换):收尾 → run 重连
            "disconnect" => return Ok(()),
            // bot 消息:先 ACK(钉钉要求及时回执),再异步处理(回复走 sessionWebhook,不占 WS)
            BOT_TOPIC => {
                let _ = ws.send(Message::Text(ack_frame(&message_id, "{}").into())).await;
                if let Some(m) = parse_bot_message(&frame) {
                    let (c, n) = (ctx.clone(), net.clone());
                    tokio::spawn(async move { handle_message(&c, &n, m).await });
                } else if let Some(webhook) = parse_unsupported(&frame) {
                    // 图/语音等非文本:已读必回(§3.5),别让家人对着空气等
                    let n = net.clone();
                    tokio::spawn(async move {
                        let _ = reply_webhook(&n, &webhook, UNSUPPORTED_HINT).await;
                    });
                }
            }
            _ => {}
        }
    }
}

/// 一条已解析的 bot 消息(ext_id 规则见 `parse_bot_message`;sender = 发言人昵称,给家人页认脸;
/// staff_id = 单聊发送者 staffId,存进映射当提醒主动推送的收件地址,群聊 None = 不推)。
struct BotMessage {
    ext_id: String,
    text: String,
    webhook: String,
    sender: Option<String>,
    staff_id: Option<String>,
}

/// 异步处理一条 bot 消息:复用 turn loop 攒回复 → POST sessionWebhook。spawn 出来跑,不阻塞 WS 收循环。
async fn handle_message(ctx: &ChannelCtx, net: &net::Client, m: BotMessage) {
    match drive_turn(&ctx.engine, CHANNEL, &m.ext_id, m.text, m.sender.as_deref(), Vec::new(), None)
        .await
    {
        Ok(Some(reply)) => {
            if let Err(e) = reply_webhook(net, &m.webhook, &reply).await {
                tracing::warn!(err = %format!("{e:#}"), "钉钉回复失败");
            }
        }
        Ok(None) => {} // 折进在飞回合
        Err(e) => {
            tracing::warn!(err = %format!("{e:#}"), "钉钉回合失败");
            let _ = reply_webhook(net, &m.webhook, ERR_HINT).await;
        }
    }
    // 推送收件地址(提醒推回手机):回合后写 —— 首次消息的映射行在 drive_turn 里才建出来。
    // 尽力件:失败只少了主动推送,不挡对话。
    if let Some(staff) = m.staff_id.as_deref() {
        let _ = ctx.engine.store().channels.set_push_id(CHANNEL, &m.ext_id, staff);
    }
}

/// 提醒推回手机(mod.rs outbound_loop 用):sessionWebhook 有时效撑不到「到点」,
/// 走企业机器人**单聊主动推送**(oToMessages/batchSend;robotCode = 企业内部应用的 appKey)。
pub(super) async fn push(
    net: &net::Client,
    app_key: &str,
    app_secret: &str,
    staff_id: &str,
    text: &str,
) -> Result<()> {
    let token = access_token(net, app_key, app_secret).await?;
    let url = "https://api.dingtalk.com/v1.0/robot/oToMessages/batchSend";
    let body = serde_json::json!({
        "robotCode": app_key,
        "userIds": [staff_id],
        "msgKey": "sampleText",
        "msgParam": serde_json::json!({ "content": text }).to_string(),
    });
    let resp = net
        .send(url, |c| c.post(url).header("x-acs-dingtalk-access-token", &token).json(&body))
        .await
        .context("钉钉 batchSend 请求失败")?;
    let status = resp.status();
    if !status.is_success() {
        let detail = resp.text().await.unwrap_or_default();
        anyhow::bail!("钉钉 batchSend HTTP {status}: {detail}");
    }
    Ok(())
}

/// 企业内部应用 access token(v1.0 oauth2;提醒推送每次现取——低频事件,不值得养缓存)。
async fn access_token(net: &net::Client, app_key: &str, app_secret: &str) -> Result<String> {
    let url = "https://api.dingtalk.com/v1.0/oauth2/accessToken";
    let body = serde_json::json!({ "appKey": app_key, "appSecret": app_secret });
    let resp =
        net.send(url, |c| c.post(url).json(&body)).await.context("钉钉取 accessToken 请求失败")?;
    let status = resp.status();
    let v: Value = resp.json().await.context("钉钉 accessToken 响应非 JSON")?;
    v.get("accessToken")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("钉钉 accessToken 取不到(HTTP {status}):{v}"))
}

/// 开 Stream 连接:POST clientId/Secret + 订阅 bot 消息 → {endpoint, ticket}。
async fn open_connection(net: &net::Client, app_key: &str, app_secret: &str) -> Result<(String, String)> {
    let body = serde_json::json!({
        "clientId": app_key,
        "clientSecret": app_secret,
        "subscriptions": [ { "type": "CALLBACK", "topic": BOT_TOPIC } ],
        "ua": "larkwing",
        "localIp": "127.0.0.1"
    });
    let resp = net.send(OPEN_URL, |c| c.post(OPEN_URL).json(&body)).await.context("钉钉开连接请求失败")?;
    let status = resp.status();
    let v: Value = resp.json().await.context("钉钉开连接响应非 JSON")?;
    let endpoint = v
        .get("endpoint")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("钉钉开连接无 endpoint(HTTP {status}):{v}"))?;
    let ticket = v.get("ticket").and_then(Value::as_str).ok_or_else(|| anyhow!("钉钉开连接无 ticket"))?;
    Ok((endpoint.to_string(), ticket.to_string()))
}

/// 回复:POST sessionWebhook 发文本(钉钉的回复入口,临时有效期)。
async fn reply_webhook(net: &net::Client, webhook: &str, text: &str) -> Result<()> {
    let body = serde_json::json!({ "msgtype": "text", "text": { "content": text } });
    let resp = net.send(webhook, |c| c.post(webhook).json(&body)).await.context("钉钉 sessionWebhook 请求失败")?;
    if !resp.status().is_success() {
        anyhow::bail!("钉钉 sessionWebhook HTTP {}", resp.status());
    }
    Ok(())
}

/// Stream 回执帧:网关要求对每条 ping/callback 回 code=200 + 同 messageId。
fn ack_frame(message_id: &str, data: &str) -> String {
    serde_json::json!({
        "code": 200,
        "headers": { "contentType": "application/json", "messageId": message_id },
        "message": "OK",
        "data": data
    })
    .to_string()
}

/// 从 bot 帧解析一条消息。data 是 JSON 字符串(二次解析)。
/// 单聊按 conversationId 续接;群聊按 (conversationId, 发言人) 隔离 + strip 开头 @mention;
/// senderNick = 发言人昵称(给家人页认脸,取不到无妨);单聊顺手带 senderStaffId(推送地址)。
fn parse_bot_message(frame: &Value) -> Option<BotMessage> {
    let data_str = frame.get("data").and_then(Value::as_str)?;
    let data: Value = serde_json::from_str(data_str).ok()?;
    let raw = data.pointer("/text/content").and_then(Value::as_str)?;
    let text = strip_at_mention(raw.trim());
    if text.is_empty() {
        return None;
    }
    let webhook = data.get("sessionWebhook").and_then(Value::as_str)?.to_string();
    let conv_id = data.get("conversationId").and_then(Value::as_str).unwrap_or("");
    let is_group = data.get("conversationType").and_then(Value::as_str) == Some("2");
    let staff = data
        .get("senderStaffId")
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.is_empty());
    let ext_id = if is_group {
        // 群聊:按发言人隔离(robot 坑#2),否则同群多人互串会话
        format!("{conv_id}:{}", staff.as_deref().unwrap_or(""))
    } else {
        conv_id.to_string()
    };
    let sender = data
        .get("senderNick")
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    // 推送地址只记单聊(群聊的提醒主动推是另一套群 API,本批不做 → None = 不推)
    let staff_id = if is_group { None } else { staff };
    Some(BotMessage { ext_id, text, webhook, sender, staff_id })
}

/// 非文本消息(picture/audio/video/file…):取不出 text 但有回复口 → 交给调用方回「看不了」。
/// 返回 sessionWebhook;连 webhook 都没有的帧(非消息回调)→ None 静默。
fn parse_unsupported(frame: &Value) -> Option<String> {
    let data_str = frame.get("data").and_then(Value::as_str)?;
    let data: Value = serde_json::from_str(data_str).ok()?;
    if data.pointer("/text/content").and_then(Value::as_str).is_some() {
        return None; // 文本消息(哪怕内容空白)不算不支持
    }
    data.get("msgtype").and_then(Value::as_str)?; // 得像一条消息(有类型),不是别的回调
    data.get("sessionWebhook").and_then(Value::as_str).map(str::to_string)
}

/// 群聊里开头的 @机器人 前缀去掉(robot 坑#7),只去开头一个。
fn strip_at_mention(text: &str) -> String {
    let t = text.trim_start();
    if let Some(rest) = t.strip_prefix('@') {
        // 跳过 @ 后到第一个空白
        if let Some(sp) = rest.find(char::is_whitespace) {
            return rest[sp..].trim_start().to_string();
        }
    }
    t.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_single_chat_uses_conversation_id() {
        let data = serde_json::json!({
            "conversationType": "1",
            "conversationId": "cidAAA",
            "senderNick": " 妈妈 ",
            "senderStaffId": "staff1",
            "sessionWebhook": "https://oapi.dingtalk.com/robot/x",
            "text": { "content": "  在吗  " }
        })
        .to_string();
        let frame = serde_json::json!({ "headers": { "topic": BOT_TOPIC, "messageId": "m1" }, "data": data });
        let m = parse_bot_message(&frame).unwrap();
        assert_eq!(m.ext_id, "cidAAA");
        assert_eq!(m.text, "在吗");
        assert!(m.webhook.contains("oapi.dingtalk.com"));
        assert_eq!(m.sender.as_deref(), Some("妈妈"), "昵称去空白");
        assert_eq!(m.staff_id.as_deref(), Some("staff1"), "单聊记推送地址");
    }

    #[test]
    fn parse_group_chat_isolates_by_sender_and_strips_at() {
        let data = serde_json::json!({
            "conversationType": "2",
            "conversationId": "grpBBB",
            "senderStaffId": "staff9",
            "sessionWebhook": "https://oapi.dingtalk.com/robot/y",
            "text": { "content": "@旺财 今天天气" }
        })
        .to_string();
        let frame = serde_json::json!({ "data": data });
        let m = parse_bot_message(&frame).unwrap();
        assert_eq!(m.ext_id, "grpBBB:staff9", "群聊按发言人隔离");
        assert_eq!(m.text, "今天天气", "开头 @机器人 被去掉");
        assert_eq!(m.sender, None, "无 senderNick → None");
        assert_eq!(m.staff_id, None, "群聊不记推送地址(群提醒主动推本批不做)");
    }

    #[test]
    fn parse_unsupported_picks_webhook_for_non_text_only() {
        // 图片消息:无 text、有 msgtype + webhook → 该回「看不了」
        let pic = serde_json::json!({
            "msgtype": "picture",
            "conversationType": "1",
            "sessionWebhook": "https://oapi.dingtalk.com/robot/z"
        })
        .to_string();
        let frame = serde_json::json!({ "data": pic });
        assert_eq!(parse_unsupported(&frame).as_deref(), Some("https://oapi.dingtalk.com/robot/z"));
        // 文本消息(哪怕空白)不算不支持;非消息回调(无 msgtype)静默
        let txt = serde_json::json!({ "msgtype": "text", "text": { "content": " " },
            "sessionWebhook": "https://x" }).to_string();
        assert_eq!(parse_unsupported(&serde_json::json!({ "data": txt })), None);
        let other = serde_json::json!({ "sessionWebhook": "https://x" }).to_string();
        assert_eq!(parse_unsupported(&serde_json::json!({ "data": other })), None);
    }

    #[test]
    fn ack_frame_carries_code_and_message_id() {
        let f: Value = serde_json::from_str(&ack_frame("mid42", "{}")).unwrap();
        assert_eq!(f["code"], 200);
        assert_eq!(f["headers"]["messageId"], "mid42");
    }
}
