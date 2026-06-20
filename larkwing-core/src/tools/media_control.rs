//! 能力轴:影音(控)。给"用户用嘴说暂停"用 —— 播放卡片上的按钮直连前端 VM,
//! 不经过模型,也不经过这里。

use async_trait::async_trait;

use super::{Tool, ToolCtx, ToolSpec};

pub(super) struct MediaControl {
    spec: ToolSpec,
}

impl MediaControl {
    pub(super) fn new() -> MediaControl {
        MediaControl {
            spec: ToolSpec {
                name: "media_control",
                description: "控制正在播放的内容:pause 暂停 / resume 继续 / stop 停止 / \
                              louder 大点声 / softer 小点声 / speed 倍速(value=0.25–3,\
                              如 1.5)/ seek 定位播放(value=秒,「跳到一分半」=90、\
                              「快进到十分钟」=600)/ next 下一集 / prev 上一集(多集合集/剧集时,\
                              用户说「下一集/上一集/换下一集/看上一集」)。用户说「暂停/接着放/别放了/\
                              大点声/1.5 倍速/跳到第 90 秒/下一集」时用。没有在放东西就别调。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": ["pause", "resume", "stop", "louder", "softer", "speed", "seek", "next", "prev"]
                        },
                        "value": {
                            "type": "number",
                            "description": "speed=倍速(0.25–3);seek=定位到第几秒;其它动作不传"
                        }
                    },
                    "required": ["action"]
                }),
                timeout: std::time::Duration::from_secs(5),
                ui_key: "tool.media_control",
            },
        }
    }
}

#[async_trait]
impl Tool for MediaControl {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> anyhow::Result<String> {
        let action = args
            .get("action")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("缺少 action 参数"))?;
        let value = args.get("value").and_then(serde_json::Value::as_f64);
        // next/prev = 队列推进(现取现播,异步):走 advance,不是发给前端的嘴控指令。
        match action {
            "next" | "prev" => {
                let delta = if action == "next" { 1 } else { -1 };
                match ctx.media.advance(ctx.user_id, delta).await? {
                    crate::media::PlayOutcome::Playing(np) => Ok(match &np.playlist {
                        Some(p) => format!("已切到第{}集《{}》(共{}集)", p.index + 1, np.title, p.total),
                        None => format!("已切到《{}》", np.title),
                    }),
                    crate::media::PlayOutcome::AwaitingLogin { detail } => Ok(format!(
                        "这一集需要登录才能播放,请提示用户扫码登录;登录后会自动接着放。(原因:{detail})"
                    )),
                }
            }
            _ => {
                ctx.media.control(action, value)?;
                Ok("ok".into())
            }
        }
    }
}
