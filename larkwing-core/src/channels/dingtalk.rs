//! 钉钉 bot 适配器:官方 **Stream 模式**(WebSocket,免公网,robot 同款)。
//! ① 开连接走 `net::Client`(HTTP)拿 endpoint+ticket;② WS 连上**只用来收**消息 + 回 ACK/pong;
//! ③ 回复 POST `sessionWebhook`(HTTP/`net::Client`)—— 回复不占 WS,故回合可异步 spawn、不阻塞收循环、不丢 ping。
//! 钉钉是国内服务:WS 直连(不经代理);TLS 走 rustls(进程级 aws-lc provider 已在壳层 lib.rs 装好)。
//!
//! 手机端补齐(2026-07-07,对齐 TG):图(picture / richText 带图)与语音(audio)也能收——
//! downloadCode 经 `robot/messageFiles/download` 换直链下载;语音优先用钉钉自带的 `recognition`
//! 转写(有就不下载不占本地 ASR),没有才走 TG 同款 ffmpeg 解码 → 本地 ASR。其余类型如实回「看不了」。

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

use super::{drive_turn, ChannelCtx};
use crate::engine::InAttachment;
use crate::net;

const CHANNEL: &str = "dingtalk";
const OPEN_URL: &str = "https://api.dingtalk.com/v1.0/gateway/connections/open";
const BOT_TOPIC: &str = "/v1.0/im/bot/messages/get";
/// 语音消息转写上限(秒):本地 ASR 超长段落精度掉(TG 同值);钉钉客户端本身也是 60s 封顶。
const VOICE_MSG_MAX_SECS: i64 = 60;
/// 媒体下载上限(TG getFile 同值的 sanity cap;语音/压缩图远小于此)。
const FILE_MAX_BYTES: usize = 20 * 1024 * 1024;

// ⚠️ core 侧静态话术(§6.6 本应数据化,同 telegram):渠道操作性提示,不经模型。
const ERR_HINT: &str = "(出了点问题,稍后再试试)";
const UNSUPPORTED_HINT: &str = "这个我还看不了~在钉钉上现在能收:文字、图片、语音。";
const VOICE_TOO_LONG_HINT: &str = "这条语音有点长,我一次只能听 60 秒内的,分开发我吧。";
const VOICE_PREPARING_HINT: &str =
    "我先去准备一下「听力」(第一次要在电脑上下载语音组件,可能要几分钟),弄好后你再发一遍哈。";
const VOICE_EMPTY_HINT: &str = "这条语音没听清,再说一遍试试?";

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
            // bot 消息:先 ACK(钉钉要求及时回执),再异步处理(回复走 sessionWebhook,不占 WS)。
            // 凭证随消息带给 handler(图/语音换下载链要现取 accessToken)。
            BOT_TOPIC => {
                let _ = ws.send(Message::Text(ack_frame(&message_id, "{}").into())).await;
                if let Some(m) = parse_bot_message(&frame) {
                    let (c, n) = (ctx.clone(), net.clone());
                    let (ak, sk) = (app_key.clone(), app_secret.clone());
                    tokio::spawn(async move { handle_message(&c, &n, &ak, &sk, m).await });
                }
            }
            _ => {}
        }
    }
}

/// 消息载荷:文字之外,图 / 语音也能收(2026-07-07 补齐,对齐 TG);其余如实回「看不了」。
#[derive(Debug, PartialEq)]
enum Payload {
    Text(String),
    /// 图(picture,或 richText「文字+图」;caption = richText 的文字部分,纯图为空)。
    Picture { download_code: String, caption: String },
    /// 语音:钉钉自带 `recognition` 转写(有就直接用);否则 downloadCode 下载走本地 ASR。
    Audio { download_code: Option<String>, duration_ms: i64, recognition: Option<String> },
    /// 认得出是消息、但看不了的类型(视频/文件…)→ 回提示(§3.5 已读必回)。
    Unsupported,
}

/// 一条已解析的 bot 消息(ext_id 规则见 `parse_bot_message`;sender = 发言人昵称,给家人页认脸;
/// staff_id = 单聊发送者 staffId,存进映射当提醒主动推送的收件地址,群聊 None = 不推)。
struct BotMessage {
    ext_id: String,
    payload: Payload,
    webhook: String,
    sender: Option<String>,
    staff_id: Option<String>,
}

/// 异步处理一条 bot 消息:按载荷分派。spawn 出来跑,不阻塞 WS 收循环。
async fn handle_message(
    ctx: &ChannelCtx,
    net: &net::Client,
    app_key: &str,
    app_secret: &str,
    m: BotMessage,
) {
    match &m.payload {
        Payload::Text(text) => {
            let text = text.clone();
            run_reply(ctx, net, &m, text, Vec::new(), None).await;
        }
        Payload::Picture { download_code, caption } => {
            let (code, caption) = (download_code.clone(), caption.clone());
            handle_picture(ctx, net, app_key, app_secret, &m, &code, caption).await;
        }
        Payload::Audio { download_code, duration_ms, recognition } => {
            let (code, dur, rec) = (download_code.clone(), *duration_ms, recognition.clone());
            handle_audio(ctx, net, app_key, app_secret, &m, code.as_deref(), dur, rec).await;
        }
        Payload::Unsupported => {
            let _ = reply_webhook(net, &m.webhook, UNSUPPORTED_HINT).await;
        }
    }
}

/// 复用 turn loop:攒回复 → POST sessionWebhook(文字 / 转写 / 图说明共用出口);
/// 回合后写推送收件地址(首次消息的映射行在 drive_turn 里才建出来;尽力件,失败只少推送)。
async fn run_reply(
    ctx: &ChannelCtx,
    net: &net::Client,
    m: &BotMessage,
    text: String,
    attachments: Vec<InAttachment>,
    input: Option<&str>,
) {
    match drive_turn(&ctx.engine, CHANNEL, &m.ext_id, text, m.sender.as_deref(), attachments, input)
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
    if let Some(staff) = m.staff_id.as_deref() {
        let _ = ctx.engine.store().channels.set_push_id(CHANNEL, &m.ext_id, staff);
    }
}

/// 图:downloadCode 换直链下载 → 桌面同缝 `InAttachment`(图当轮注入、不落库,§9);
/// caption(richText 的文字)当消息文字。与 TG handle_photo 同构。
async fn handle_picture(
    ctx: &ChannelCtx,
    net: &net::Client,
    app_key: &str,
    app_secret: &str,
    m: &BotMessage,
    download_code: &str,
    caption: String,
) {
    let bytes = match download_media(net, app_key, app_secret, download_code).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(err = %format!("{e:#}"), "钉钉图片下载失败");
            let _ = reply_webhook(net, &m.webhook, ERR_HINT).await;
            return;
        }
    };
    let mime = sniff_image_mime(&bytes);
    let att = InAttachment {
        name: format!("photo.{}", ext_of(mime)),
        mime: mime.into(),
        data: base64::engine::general_purpose::STANDARD.encode(&bytes),
    };
    run_reply(ctx, net, m, caption, vec![att], None).await;
}

/// 语音:钉钉自带 `recognition` 转写优先(不下载不占本地 ASR);没有才下载 → ffmpeg 解
/// 16k PCM → 本地 ASR(TG handle_voice 同构:超长 / 模型没就绪 / 转写为空,全部如实回话)。
#[allow(clippy::too_many_arguments)]
async fn handle_audio(
    ctx: &ChannelCtx,
    net: &net::Client,
    app_key: &str,
    app_secret: &str,
    m: &BotMessage,
    download_code: Option<&str>,
    duration_ms: i64,
    recognition: Option<String>,
) {
    if let Some(text) = recognition {
        run_reply(ctx, net, m, text, Vec::new(), Some("voice_msg")).await;
        return;
    }
    let Some(code) = download_code else {
        // 防御:parse 已把「无码无转写」归 Unsupported,这里兜底不静默
        let _ = reply_webhook(net, &m.webhook, UNSUPPORTED_HINT).await;
        return;
    };
    if duration_ms > VOICE_MSG_MAX_SECS * 1000 {
        let _ = reply_webhook(net, &m.webhook, VOICE_TOO_LONG_HINT).await;
        return;
    }
    if !ctx.voice.asr_ready() {
        ctx.voice.prefetch_asr(); // 后台下齐(桌面 HUD 可见),好了再发就能听
        let _ = reply_webhook(net, &m.webhook, VOICE_PREPARING_HINT).await;
        return;
    }
    let text = match audio_to_text(ctx, net, app_key, app_secret, code).await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(err = %format!("{e:#}"), "钉钉语音转写失败");
            let _ = reply_webhook(net, &m.webhook, ERR_HINT).await;
            return;
        }
    };
    if text.trim().is_empty() {
        let _ = reply_webhook(net, &m.webhook, VOICE_EMPTY_HINT).await;
        return;
    }
    run_reply(ctx, net, m, text, Vec::new(), Some("voice_msg")).await;
}

/// 语音消息的转写链(错误统一冒泡给上面兜 ERR_HINT)。
async fn audio_to_text(
    ctx: &ChannelCtx,
    net: &net::Client,
    app_key: &str,
    app_secret: &str,
    download_code: &str,
) -> Result<String> {
    let bytes = download_media(net, app_key, app_secret, download_code).await?;
    let pcm = ctx.media.decode_audio_pcm16k(bytes, VOICE_MSG_MAX_SECS as u32).await?;
    ctx.voice.transcribe_pcm(pcm).await
}

/// downloadCode 换直链(`robot/messageFiles/download`,robotCode = 企业内部应用 appKey)→ 拉字节。
/// 图 / 语音共用;直链是钉钉 CDN 的临时地址,现换现下。
async fn download_media(
    net: &net::Client,
    app_key: &str,
    app_secret: &str,
    download_code: &str,
) -> Result<Vec<u8>> {
    let token = access_token(net, app_key, app_secret).await?;
    let url = "https://api.dingtalk.com/v1.0/robot/messageFiles/download";
    let body = serde_json::json!({ "downloadCode": download_code, "robotCode": app_key });
    let resp = net
        .send(url, |c| c.post(url).header("x-acs-dingtalk-access-token", &token).json(&body))
        .await
        .context("钉钉换下载链请求失败")?;
    let status = resp.status();
    let v: Value = resp.json().await.context("钉钉换下载链响应非 JSON")?;
    let dl = v
        .get("downloadUrl")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("钉钉换不到 downloadUrl(HTTP {status}):{v}"))?;
    let resp = net.send(dl, |c| c.get(dl)).await.context("钉钉媒体下载失败")?;
    anyhow::ensure!(resp.status().is_success(), "钉钉媒体下载 HTTP {}", resp.status());
    let bytes = resp.bytes().await.context("读媒体字节失败")?;
    anyhow::ensure!(bytes.len() <= FILE_MAX_BYTES, "文件太大({} 字节)", bytes.len());
    Ok(bytes.to_vec())
}

/// 图片字节嗅探 MIME(钉钉不回 content-type 语义;认不出按 jpeg —— 手机拍照绝大多数是它)。
fn sniff_image_mime(b: &[u8]) -> &'static str {
    if b.starts_with(&[0x89, b'P', b'N', b'G']) {
        "image/png"
    } else if b.starts_with(b"GIF8") {
        "image/gif"
    } else if b.len() >= 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        "image/webp"
    } else {
        "image/jpeg"
    }
}

fn ext_of(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "jpg",
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
/// 信封(webhook / 会话 / 发言人)对所有类型通用;载荷按 msgtype 分:text / picture / audio /
/// richText(文字+图)能收,其余 = Unsupported(回「看不了」);连 msgtype 或 webhook 都没有的
/// 帧(非消息回调)→ None 静默。单聊按 conversationId 续接;群聊按 (conversationId, 发言人)
/// 隔离(robot 坑#2)+ strip 开头 @mention;senderNick 给家人页认脸;单聊带 senderStaffId(推送地址)。
fn parse_bot_message(frame: &Value) -> Option<BotMessage> {
    let data_str = frame.get("data").and_then(Value::as_str)?;
    let data: Value = serde_json::from_str(data_str).ok()?;
    let webhook = data.get("sessionWebhook").and_then(Value::as_str)?.to_string();
    let msgtype = data.get("msgtype").and_then(Value::as_str)?; // 得像一条消息,不是别的回调

    let payload = match msgtype {
        "text" => {
            let raw = data.pointer("/text/content").and_then(Value::as_str)?;
            let text = strip_at_mention(raw.trim());
            if text.is_empty() {
                return None; // 空白文本照旧静默跳过(不算不支持)
            }
            Payload::Text(text)
        }
        "picture" => match content_download_code(&data) {
            Some(code) => Payload::Picture { download_code: code, caption: String::new() },
            None => Payload::Unsupported, // 形不完整:如实回提示,别静默
        },
        "audio" => {
            let download_code = content_download_code(&data);
            let recognition = data
                .pointer("/content/recognition")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            if download_code.is_none() && recognition.is_none() {
                Payload::Unsupported
            } else {
                Payload::Audio {
                    download_code,
                    duration_ms: int_of(data.pointer("/content/duration")),
                    recognition,
                }
            }
        }
        "richText" => parse_rich_text(&data),
        _ => Payload::Unsupported, // video / file / …:已读必回(§3.5)
    };

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
    Some(BotMessage { ext_id, payload, webhook, sender, staff_id })
}

/// picture 消息的下载码:钉钉两种字段名都见过(downloadCode / pictureDownloadCode),都认。
fn content_download_code(data: &Value) -> Option<String> {
    ["downloadCode", "pictureDownloadCode"].iter().find_map(|k| {
        data.pointer(&format!("/content/{k}"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|s| !s.is_empty())
    })
}

/// richText(文字+图混排):content.richText 数组,文字段并接成 caption、取第一张图;
/// 没图有字 = 当纯文本;两样都没有 = Unsupported。
fn parse_rich_text(data: &Value) -> Payload {
    let Some(items) = data.pointer("/content/richText").and_then(Value::as_array) else {
        return Payload::Unsupported;
    };
    let mut text = String::new();
    let mut code: Option<String> = None;
    for it in items {
        if let Some(t) = it.get("text").and_then(Value::as_str) {
            text.push_str(t);
        }
        if code.is_none() {
            code = ["downloadCode", "pictureDownloadCode"].iter().find_map(|k| {
                it.get(*k).and_then(Value::as_str).map(str::to_string).filter(|s| !s.is_empty())
            });
        }
    }
    let text = strip_at_mention(text.trim());
    match (code, text.is_empty()) {
        (Some(download_code), _) => Payload::Picture { download_code, caption: text },
        (None, false) => Payload::Text(text),
        (None, true) => Payload::Unsupported,
    }
}

/// 宽容取整(钉钉的 duration 有的载荷发数字、有的发字符串)。
fn int_of(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0),
        _ => 0,
    }
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

    /// 把 data(JSON 值)包成 Stream 帧。
    fn frame_of(data: serde_json::Value) -> Value {
        serde_json::json!({ "headers": { "topic": BOT_TOPIC, "messageId": "m1" }, "data": data.to_string() })
    }

    #[test]
    fn parse_single_chat_uses_conversation_id() {
        let frame = frame_of(serde_json::json!({
            "msgtype": "text",
            "conversationType": "1",
            "conversationId": "cidAAA",
            "senderNick": " 妈妈 ",
            "senderStaffId": "staff1",
            "sessionWebhook": "https://oapi.dingtalk.com/robot/x",
            "text": { "content": "  在吗  " }
        }));
        let m = parse_bot_message(&frame).unwrap();
        assert_eq!(m.ext_id, "cidAAA");
        assert_eq!(m.payload, Payload::Text("在吗".into()));
        assert!(m.webhook.contains("oapi.dingtalk.com"));
        assert_eq!(m.sender.as_deref(), Some("妈妈"), "昵称去空白");
        assert_eq!(m.staff_id.as_deref(), Some("staff1"), "单聊记推送地址");
    }

    #[test]
    fn parse_group_chat_isolates_by_sender_and_strips_at() {
        let frame = frame_of(serde_json::json!({
            "msgtype": "text",
            "conversationType": "2",
            "conversationId": "grpBBB",
            "senderStaffId": "staff9",
            "sessionWebhook": "https://oapi.dingtalk.com/robot/y",
            "text": { "content": "@旺财 今天天气" }
        }));
        let m = parse_bot_message(&frame).unwrap();
        assert_eq!(m.ext_id, "grpBBB:staff9", "群聊按发言人隔离");
        assert_eq!(m.payload, Payload::Text("今天天气".into()), "开头 @机器人 被去掉");
        assert_eq!(m.sender, None, "无 senderNick → None");
        assert_eq!(m.staff_id, None, "群聊不记推送地址(群提醒主动推本批不做)");
    }

    #[test]
    fn parse_picture_audio_and_unsupported() {
        // 图:downloadCode 进载荷(pictureDownloadCode 字段名也认)
        let m = parse_bot_message(&frame_of(serde_json::json!({
            "msgtype": "picture", "conversationType": "1", "conversationId": "c1",
            "sessionWebhook": "https://x",
            "content": { "pictureDownloadCode": "dc-pic" }
        })))
        .unwrap();
        assert_eq!(m.payload, Payload::Picture { download_code: "dc-pic".into(), caption: String::new() });

        // 语音:钉钉自带转写 recognition 带上;duration 字符串也认
        let m = parse_bot_message(&frame_of(serde_json::json!({
            "msgtype": "audio", "conversationType": "1", "conversationId": "c1",
            "sessionWebhook": "https://x",
            "content": { "downloadCode": "dc-voice", "duration": "3000", "recognition": " 在吗 " }
        })))
        .unwrap();
        assert_eq!(
            m.payload,
            Payload::Audio {
                download_code: Some("dc-voice".into()),
                duration_ms: 3000,
                recognition: Some("在吗".into())
            }
        );

        // 视频等其它类型 → Unsupported(回「看不了」);缺 downloadCode 的图同理
        let m = parse_bot_message(&frame_of(serde_json::json!({
            "msgtype": "video", "sessionWebhook": "https://x" })))
        .unwrap();
        assert_eq!(m.payload, Payload::Unsupported);
        let m = parse_bot_message(&frame_of(serde_json::json!({
            "msgtype": "picture", "sessionWebhook": "https://x" })))
        .unwrap();
        assert_eq!(m.payload, Payload::Unsupported);

        // 空白文本照旧静默;非消息回调(无 msgtype / 无 webhook)静默
        let txt = serde_json::json!({ "msgtype": "text", "text": { "content": " " }, "sessionWebhook": "https://x" });
        assert!(parse_bot_message(&frame_of(txt)).is_none());
        let other = serde_json::json!({ "sessionWebhook": "https://x" });
        assert!(parse_bot_message(&frame_of(other)).is_none());
    }

    #[test]
    fn parse_rich_text_merges_text_and_first_picture() {
        // 文字 + 图混排 → Picture{caption};只有文字 → Text;都没有 → Unsupported
        let m = parse_bot_message(&frame_of(serde_json::json!({
            "msgtype": "richText", "conversationType": "1", "conversationId": "c1",
            "sessionWebhook": "https://x",
            "content": { "richText": [ { "text": "这个菜" }, { "downloadCode": "dc-rt", "type": "picture" }, { "text": "怎么做?" } ] }
        })))
        .unwrap();
        assert_eq!(
            m.payload,
            Payload::Picture { download_code: "dc-rt".into(), caption: "这个菜怎么做?".into() }
        );

        let m = parse_bot_message(&frame_of(serde_json::json!({
            "msgtype": "richText", "sessionWebhook": "https://x",
            "content": { "richText": [ { "text": "纯文字" } ] }
        })))
        .unwrap();
        assert_eq!(m.payload, Payload::Text("纯文字".into()));
    }

    #[test]
    fn sniff_image_mime_by_magic_bytes() {
        assert_eq!(sniff_image_mime(&[0x89, b'P', b'N', b'G', 0x0D]), "image/png");
        assert_eq!(sniff_image_mime(b"GIF89a..."), "image/gif");
        assert_eq!(sniff_image_mime(b"RIFF\x00\x00\x00\x00WEBPVP8 "), "image/webp");
        assert_eq!(sniff_image_mime(&[0xFF, 0xD8, 0xFF]), "image/jpeg");
        assert_eq!(sniff_image_mime(b""), "image/jpeg", "认不出按 jpeg");
    }

    #[test]
    fn ack_frame_carries_code_and_message_id() {
        let f: Value = serde_json::from_str(&ack_frame("mid42", "{}")).unwrap();
        assert_eq!(f["code"], 200);
        assert_eq!(f["headers"]["messageId"], "mid42");
    }
}
