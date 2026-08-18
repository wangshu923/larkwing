//! 能力轴:一句话/一个链接 → 家里人的手机(渠道出站文字,机器件在 channels/outbound)。
//! send_file 的姊妹原语:那边送文件、这边送短内容,**原样递送**(用户要的是「这段内容
//! 落在手机上」)。与「捎话」分工:让它转述/到点提醒走 reminder_set(带 for),
//! 原文照发走这里。目标缺省 = 说话人(ToolCtx.user_id = 渠道归人后的 mem_user),同 send_file。

use std::time::Duration;

use async_trait::async_trait;

use crate::channels::outbound;

use super::{Tool, ToolCtx, ToolSpec};

/// 单条上限(字符):超长如实退回(§3.5 绝不静默截断)。与提醒物化内容同量级——
/// 这里送的是「一句话/链接」,成篇的内容该写成文件走 send_file。
const TEXT_MAX_CHARS: usize = 2000;

pub(super) struct SendText {
    spec: ToolSpec,
    net: crate::net::Client,
}

impl SendText {
    pub(super) fn new() -> SendText {
        SendText {
            spec: ToolSpec {
                name: "send_text",
                description: "把一句话/一个链接原样发到家里人的手机上(走已连接的 \
                              Telegram/钉钉/微信)。用户说「把这个链接发我手机」「把地址发给\
                              爸爸」就用它。不填 to = 发给说这句话的人;「发给妈妈」就把 to \
                              填成那位家人的名字。用户点名了渠道(「发我微信」)才填 channel,\
                              没点名就不填。发文件用 send_file;要对方到点才收到提醒的用 \
                              reminder_set。微信会话过期发不进去时会自动挂起、对方一开口就\
                              补送(结果里会说明),不要重发。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "text": {
                            "type": "string",
                            "description": "要发的内容,原样送达(链接/地址/一段话)"
                        },
                        "to": {
                            "type": "string",
                            "description": "发给哪位家人(名字要跟家人页一致);不填 = 说这句话的人自己"
                        },
                        "channel": {
                            "type": "string",
                            "enum": ["telegram", "dingtalk", "weixin"],
                            "description": "发到哪个渠道;只在用户点名时填(「发我微信」= weixin),不填 = 对方最近在用的那个"
                        }
                    },
                    "required": ["text"]
                }),
                timeout: Duration::from_secs(60),
                ui_key: "tool.send_text",
            },
            // 文字消息很小,超时给足一次往返即可;走 net 代理选路(§4.6)
            net: crate::net::Client::new(|b| {
                b.connect_timeout(Duration::from_secs(10)).timeout(Duration::from_secs(50))
            }),
        }
    }
}

#[async_trait]
impl Tool for SendText {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> anyhow::Result<String> {
        let text = args
            .get("text")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!("缺少 text 参数(要发的内容)"))?;
        let n = text.chars().count();
        anyhow::ensure!(
            n <= TEXT_MAX_CHARS,
            "内容太长({n} 字,上限 {TEXT_MAX_CHARS})——成篇的内容写成文件用 send_file 发,\
             或精简后再发"
        );

        // 目标解析与 send_file 同一套:缺省说话人,to = 家人,channel = 用户点名
        let recipient = args
            .get("to")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|name| outbound::find_member(&ctx.store, name))
            .transpose()?;
        let target_user = recipient.as_ref().map_or(ctx.user_id, |u| u.id);
        let channel = args
            .get("channel")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if let Some(ch) = channel {
            anyhow::ensure!(
                matches!(ch, "telegram" | "dingtalk" | "weixin"),
                "channel 只能是 telegram/dingtalk/weixin,收到「{ch}」"
            );
        }
        let target = outbound::resolve_target(&ctx.store, target_user, channel)?;

        let whose = recipient.as_ref().map(|u| format!("{}的", u.name)).unwrap_or_default();
        let who = recipient.as_ref().map(|u| u.name.clone()).unwrap_or_else(|| "你".to_string());
        match outbound::send_text(&self.net, &target, text).await {
            Ok(()) => Ok(format!("已经 {} 发到{whose}手机", target.channel_name())),
            // 微信会话窗口关死(平台限制,非故障)→ 挂起补发,不算失败(send_file 同款,§7.7)
            Err(e) if outbound::is_stale_weixin(&e) => {
                outbound::queue_weixin_pending_text(&ctx.store, &target, text)
                    .await
                    .map_err(|qe| anyhow::anyhow!("没发出去({e:#});挂起也失败:{qe:#}"))?;
                Ok(format!(
                    "没能马上送到、已挂起——微信限制 bot 只能在对方最近说过话的会话窗口内\
                     主动发消息,现在窗口过期了;{who}在微信上随便发来一句话就会自动补送\
                     ({} 小时内有效),不用重发。想立刻送到,就让{who}先给我发句话。",
                    outbound::weixin_pending_ttl_hours()
                ))
            }
            Err(e) => Err(e.context(format!(
                "没发出去(经 {},发往{whose}手机)",
                target.channel_name()
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::MediaRuntime;
    use crate::store::Store;

    fn ctx(tag: &str) -> ToolCtx {
        let dir = std::env::temp_dir().join(format!("lw-sendtext-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _ = std::fs::remove_file(dir.join("t.db"));
        let store = Store::open(&dir.join("t.db")).unwrap();
        let me = store.users.ensure_default_user().unwrap();
        ToolCtx {
            user_id: me.id,
            conv_id: 1,
            media: MediaRuntime::detached(store.clone()),
            store,
            web: None,
            voice: None,
            confirm: None,
            grants: Default::default(),
            agent: None,
        }
    }

    #[tokio::test]
    async fn rejects_bad_args_and_reports_unlinked_phone() {
        let ctx = ctx("args");
        let tool = SendText::new();
        assert!(tool.run(serde_json::json!({}), &ctx).await.is_err(), "缺 text");
        assert!(tool.run(serde_json::json!({"text": "  "}), &ctx).await.is_err(), "空 text");
        let long = "长".repeat(TEXT_MAX_CHARS + 1);
        let err = tool.run(serde_json::json!({"text": long}), &ctx).await.unwrap_err();
        assert!(err.to_string().contains("太长"), "{err:#}");
        // 没绑手机 → 明白话观察(§3.5)
        let err = tool.run(serde_json::json!({"text": "一个链接"}), &ctx).await.unwrap_err();
        assert!(err.to_string().contains("还没连上手机"), "{err:#}");
        // 查无此人 → 带现有名单的明白话
        let err = tool
            .run(serde_json::json!({"text": "hi", "to": "二舅"}), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("没有叫"), "{err:#}");
    }

    /// 微信会话窗口关死 → 文字挂起、结果如实说明(Ok 不是 Err),挂起件落 KV。
    #[tokio::test]
    async fn weixin_stale_context_queues_pending_text() {
        use axum::{routing::post, Router};
        async fn deny(_b: axum::body::Bytes) -> &'static str {
            r#"{"ret":-2,"errmsg":"prepare failed"}"#
        }
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, Router::new().route("/ilink/bot/sendmessage", post(deny)))
                .await
                .ok();
        });

        let ctx = ctx("wxpend");
        crate::secrets::set(&ctx.store.settings, "remote.weixin.token", "tok").unwrap();
        ctx.store
            .settings
            .set(None, "remote.weixin.base_url", &format!("http://127.0.0.1:{port}"))
            .unwrap();
        let owner = ctx.store.users.ensure_default_user().unwrap();
        let conv =
            ctx.store.chat.create_conversation_full(owner.id, "companion", "weixin").unwrap();
        ctx.store.channels.bind("weixin", "wxid_1", conv.id).unwrap();
        ctx.store.channels.set_push_id("weixin", "wxid_1", "stale-ctx").unwrap();

        let tool = SendText::new();
        let out = tool
            .run(
                serde_json::json!({"text": "https://example.com/菜谱", "channel": "weixin"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.contains("挂起") && out.contains("不用重发"), "如实说明挂起:{out}");
        let raw = ctx.store.settings.get(None, "remote.weixin.pending_sends").unwrap().unwrap();
        assert!(raw.contains("菜谱"), "文字挂起件落 KV:{raw}");
    }
}
