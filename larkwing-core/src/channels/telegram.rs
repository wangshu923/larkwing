//! Telegram bot 适配器:raw Bot API over `net::Client`(**不用 teloxide**——它自带 reqwest 会绕过
//! net::Client / 破 §4.6 代理选路)。入站 `getUpdates` 长轮询(offset 防重放),出站 `sendMessage`
//! (**纯文本、不带 parse_mode** —— 绕开 MarkdownV2 转义地狱这个 robot 大坑)。免公网、免 SDK。
//! 国内 api.telegram.org 多半要代理:net 直连失败自动落代理(§4.6),无需本模块操心。
//!
//! 手机端补全(v0.2.4):语音消息(getFile 下载 → ffmpeg 解 16k PCM → 本地 ASR)与照片
//! (取最大尺寸 → 桌面同缝 InAttachment 当轮注入)都能收;其余类型如实回「看不了」(§3.5 不静默)。

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use base64::Engine as _;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::{drive_turn, split_message, ChannelCtx};
use crate::engine::InAttachment;
use crate::net;

const CHANNEL: &str = "telegram";
const API: &str = "https://api.telegram.org";
/// Telegram 单条上限 4096;留余量。
const TG_MAX: usize = 4000;
/// 长轮询服务端挂起窗口(秒);net client 超时须 > 它。
const POLL_TIMEOUT_S: u64 = 50;
/// 语音消息转写上限(秒):本地 ASR(SenseVoice/FireRed)超长段落精度掉、耗时涨,
/// 超过如实告知分段发;`-t` 双保险截到同值。
const VOICE_MSG_MAX_SECS: i64 = 60;
/// getFile 下载上限(Bot API 本身 20MB 封顶;语音/压缩图远小于此)。
const FILE_MAX_BYTES: i64 = 20 * 1024 * 1024;

// ⚠️ core 侧静态话术(§6.6 本应数据化,如语音的场景 JSON 话术)——MVP 先内联,记债待迁。
//    这些是渠道**操作性**提示(非人格),不经模型。
const ONBOARD_HINT: &str =
    "你好,我是旺财。你的 chat id 是 {id},把它加到设置·远程渠道的白名单里,我们就能聊啦。";
const ERR_HINT: &str = "(出了点问题,稍后再试试)";
const UNSUPPORTED_HINT: &str = "这个我还看不了~现在能收:文字、图片、语音。";
const VOICE_TOO_LONG_HINT: &str = "这条语音有点长,我一次只能听 60 秒内的,分开发我吧。";
const VOICE_PREPARING_HINT: &str =
    "我先去准备一下「听力」(第一次要在电脑上下载语音组件,可能要几分钟),弄好后你再发一遍哈。";
const VOICE_EMPTY_HINT: &str = "这条语音没听清,再说一遍试试?";

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
            let Some((chat_id, incoming, sender)) = parse_update(&upd) else { continue };
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

            match incoming {
                Incoming::Text(text) => {
                    reply_turn(ctx, net, &token, chat_id, &chat, text, sender.as_deref(), Vec::new(), None)
                        .await;
                }
                Incoming::Voice { file_id, duration } => {
                    handle_voice(ctx, net, &token, chat_id, &chat, sender.as_deref(), &file_id, duration)
                        .await;
                }
                Incoming::Photo { file_id, caption } => {
                    handle_photo(ctx, net, &token, chat_id, &chat, sender.as_deref(), &file_id, caption)
                        .await;
                }
                // 已读必回(§3.5 不静默):看不了的类型如实说,别让家人对着空气等
                Incoming::Unsupported => {
                    let _ = send_message(net, &token, chat_id, UNSUPPORTED_HINT).await;
                }
            }
        }
    }
}

/// 复用 turn loop:攒回复 → 按 4096 分片发回(文字 / 转写文字 / 图片说明共用的出口)。
#[allow(clippy::too_many_arguments)]
async fn reply_turn(
    ctx: &ChannelCtx,
    net: &net::Client,
    token: &str,
    chat_id: i64,
    chat: &str,
    text: String,
    sender: Option<&str>,
    attachments: Vec<InAttachment>,
    input: Option<&str>,
) {
    match drive_turn(&ctx.engine, CHANNEL, chat, text, sender, attachments, input).await {
        Ok(Some(reply)) => {
            for piece in split_message(&reply, TG_MAX) {
                if let Err(e) = send_message(net, token, chat_id, &piece).await {
                    tracing::warn!(err = %format!("{e:#}"), "Telegram 发送失败");
                }
            }
        }
        Ok(None) => {} // 折进在飞回合(inject),不单独回
        Err(e) => {
            tracing::warn!(err = %format!("{e:#}"), "Telegram 回合失败");
            let _ = send_message(net, token, chat_id, ERR_HINT).await;
        }
    }
}

/// 语音消息:下载 → ffmpeg 解 16k 单声道 PCM → 本地 ASR → 转写文字进正常回合。
/// 模型没就绪先如实说「准备中」并后台预取(下载几百 MB,绝不卡在收循环里干等);
/// 转写为空两段式兜底的渠道版 = 一句「没听清」(§7.5 有声兜底同哲学,这里是文字渠道)。
#[allow(clippy::too_many_arguments)]
async fn handle_voice(
    ctx: &ChannelCtx,
    net: &net::Client,
    token: &str,
    chat_id: i64,
    chat: &str,
    sender: Option<&str>,
    file_id: &str,
    duration: i64,
) {
    if duration > VOICE_MSG_MAX_SECS {
        let _ = send_message(net, token, chat_id, VOICE_TOO_LONG_HINT).await;
        return;
    }
    if !ctx.voice.asr_ready() {
        ctx.voice.prefetch_asr(); // 后台下齐(桌面 HUD 可见),好了再发就能听
        let _ = send_message(net, token, chat_id, VOICE_PREPARING_HINT).await;
        return;
    }
    let text = match voice_to_text(ctx, net, token, file_id).await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(err = %format!("{e:#}"), "Telegram 语音转写失败");
            let _ = send_message(net, token, chat_id, ERR_HINT).await;
            return;
        }
    };
    if text.trim().is_empty() {
        let _ = send_message(net, token, chat_id, VOICE_EMPTY_HINT).await;
        return;
    }
    reply_turn(ctx, net, token, chat_id, chat, text, sender, Vec::new(), Some("voice_msg")).await;
}

/// 语音消息的转写链(错误统一冒泡给上面兜 ERR_HINT)。
async fn voice_to_text(
    ctx: &ChannelCtx,
    net: &net::Client,
    token: &str,
    file_id: &str,
) -> Result<String> {
    let bytes = download_file(net, token, file_id).await?;
    let pcm = ctx.media.decode_audio_pcm16k(bytes, VOICE_MSG_MAX_SECS as u32).await?;
    ctx.voice.transcribe_pcm(pcm).await
}

/// 照片:取最大尺寸下载 → 桌面同缝 `InAttachment`(图当轮注入、不落库,§9);caption 当消息文字。
#[allow(clippy::too_many_arguments)]
async fn handle_photo(
    ctx: &ChannelCtx,
    net: &net::Client,
    token: &str,
    chat_id: i64,
    chat: &str,
    sender: Option<&str>,
    file_id: &str,
    caption: String,
) {
    let bytes = match download_file(net, token, file_id).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(err = %format!("{e:#}"), "Telegram 照片下载失败");
            let _ = send_message(net, token, chat_id, ERR_HINT).await;
            return;
        }
    };
    let att = InAttachment {
        name: "photo.jpg".into(),
        mime: "image/jpeg".into(), // Bot API 的 photo 恒为服务端压缩 JPEG
        data: base64::engine::general_purpose::STANDARD.encode(&bytes),
    };
    reply_turn(ctx, net, token, chat_id, chat, caption, sender, vec![att], None).await;
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

/// 提醒推回手机(mod.rs outbound_loop 用):主动往 chat 发一段文本,长文分片。
pub(super) async fn push(net: &net::Client, token: &str, chat_id: i64, text: &str) -> Result<()> {
    for piece in split_message(text, TG_MAX) {
        send_message(net, token, chat_id, &piece).await?;
    }
    Ok(())
}

/// getFile 取下载路径 → 拉字节(语音/照片共用);Bot API 20MB 封顶,超限如实报错。
async fn download_file(net: &net::Client, token: &str, file_id: &str) -> Result<Vec<u8>> {
    let url = format!("{API}/bot{token}/getFile?file_id={file_id}");
    let resp = net.send(&url, |c| c.get(&url)).await.context("getFile 请求失败")?;
    let body: Value = resp.json().await.context("getFile 响应非 JSON")?;
    if body.get("ok").and_then(Value::as_bool) != Some(true) {
        let desc = body.get("description").and_then(Value::as_str).unwrap_or("unknown");
        anyhow::bail!("Telegram getFile 失败: {desc}");
    }
    let size = body.pointer("/result/file_size").and_then(Value::as_i64).unwrap_or(0);
    anyhow::ensure!(size <= FILE_MAX_BYTES, "文件太大({size} 字节)");
    let path = body
        .pointer("/result/file_path")
        .and_then(Value::as_str)
        .context("getFile 无 file_path")?;
    let dl = format!("{API}/file/bot{token}/{path}");
    let resp = net.send(&dl, |c| c.get(&dl)).await.context("下载文件失败")?;
    anyhow::ensure!(resp.status().is_success(), "下载 HTTP {}", resp.status());
    Ok(resp.bytes().await.context("读文件字节失败")?.to_vec())
}

/// 一条入站消息的形态(手机端补全:文字之外,语音/照片能收,其余如实告知)。
#[derive(Debug, PartialEq)]
enum Incoming {
    Text(String),
    Voice { file_id: String, duration: i64 },
    Photo { file_id: String, caption: String },
    /// 认得出是媒体/内容消息、但看不了的类型(贴纸/视频/文件…)→ 回提示。
    /// 服务性消息(入群/置顶等)不在此列,照旧静默跳过。
    Unsupported,
}

/// 明确「看不了但该回一句」的消息键(服务性消息不算,别对入群通知喊话)。
const UNSUPPORTED_KEYS: &[&str] =
    &["sticker", "video", "document", "audio", "video_note", "animation", "contact", "location", "venue", "poll"];

/// 从 update 取 (chat_id, 形态, 发送者昵称);非消息帧(edited/回执等)→ None。
/// 昵称 = from.first_name(+ last_name),只给家人页认脸,取不到无妨。
fn parse_update(upd: &Value) -> Option<(i64, Incoming, Option<String>)> {
    let msg = upd.get("message")?;
    let chat_id = msg.get("chat")?.get("id").and_then(Value::as_i64)?;
    let sender = msg
        .get("from")
        .map(|f| {
            let first = f.get("first_name").and_then(Value::as_str).unwrap_or("");
            let last = f.get("last_name").and_then(Value::as_str).unwrap_or("");
            format!("{first} {last}").trim().to_string()
        })
        .filter(|s| !s.is_empty());

    let incoming = if let Some(text) = msg.get("text").and_then(Value::as_str) {
        let t = text.trim();
        if t.is_empty() {
            return None;
        }
        Incoming::Text(t.to_string())
    } else if let Some(v) = msg.get("voice") {
        Incoming::Voice {
            file_id: v.get("file_id").and_then(Value::as_str)?.to_string(),
            duration: v.get("duration").and_then(Value::as_i64).unwrap_or(0),
        }
    } else if let Some(sizes) = msg.get("photo").and_then(Value::as_array) {
        // photo 是从小到大的多档尺寸,取最后(最大)那档
        Incoming::Photo {
            file_id: sizes.last()?.get("file_id").and_then(Value::as_str)?.to_string(),
            caption: msg
                .get("caption")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string(),
        }
    } else if UNSUPPORTED_KEYS.iter().any(|k| msg.get(k).is_some()) {
        Incoming::Unsupported
    } else {
        return None; // 服务性消息(入群/置顶…):照旧静默
    };
    Some((chat_id, incoming, sender))
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
    fn parse_update_extracts_text_chat_and_sender() {
        let upd = serde_json::json!({
            "update_id": 10,
            "message": {
                "text": "  在吗  ",
                "chat": { "id": 12345 },
                "from": { "first_name": "豆豆", "last_name": "" }
            }
        });
        assert_eq!(
            parse_update(&upd),
            Some((12345, Incoming::Text("在吗".into()), Some("豆豆".into())))
        );
        // 无 from(频道帖等):昵称 None,消息照收
        let bare = serde_json::json!({ "message": { "text": "hi", "chat": { "id": 1 } } });
        assert_eq!(parse_update(&bare), Some((1, Incoming::Text("hi".into()), None)));
        // 空白文本 → 跳过
        let empty = serde_json::json!({ "message": { "text": "   ", "chat": { "id": 1 } } });
        assert_eq!(parse_update(&empty), None);
    }

    #[test]
    fn parse_update_voice_and_photo() {
        let voice = serde_json::json!({ "message": {
            "chat": { "id": 5 },
            "voice": { "file_id": "vf1", "duration": 7 }
        }});
        assert_eq!(
            parse_update(&voice),
            Some((5, Incoming::Voice { file_id: "vf1".into(), duration: 7 }, None))
        );
        // photo 取最大档(数组最后一个);caption 去空白
        let photo = serde_json::json!({ "message": {
            "chat": { "id": 6 },
            "photo": [ { "file_id": "small" }, { "file_id": "big" } ],
            "caption": " 这是什么 "
        }});
        assert_eq!(
            parse_update(&photo),
            Some((6, Incoming::Photo { file_id: "big".into(), caption: "这是什么".into() }, None))
        );
        // 无 caption → 空串(引擎收空文本 + 图,与桌面同缝)
        let bare_photo = serde_json::json!({ "message": {
            "chat": { "id": 7 }, "photo": [ { "file_id": "p" } ]
        }});
        assert_eq!(
            parse_update(&bare_photo),
            Some((7, Incoming::Photo { file_id: "p".into(), caption: String::new() }, None))
        );
    }

    #[test]
    fn parse_update_unsupported_vs_service() {
        // 贴纸/文件 → Unsupported(要回「看不了」,§3.5 不静默)
        let sticker = serde_json::json!({ "message": { "chat": { "id": 1 }, "sticker": {} } });
        assert_eq!(parse_update(&sticker), Some((1, Incoming::Unsupported, None)));
        let doc = serde_json::json!({ "message": { "chat": { "id": 1 }, "document": {} } });
        assert_eq!(parse_update(&doc), Some((1, Incoming::Unsupported, None)));
        // 服务性消息(入群通知等)→ 静默跳过,别对着系统事件喊话
        let service =
            serde_json::json!({ "message": { "chat": { "id": 1 }, "new_chat_members": [{}] } });
        assert_eq!(parse_update(&service), None);
        // 空 photo 数组:形不完整 → 跳过
        let broken = serde_json::json!({ "message": { "chat": { "id": 1 }, "photo": [] } });
        assert_eq!(parse_update(&broken), None);
    }
}
