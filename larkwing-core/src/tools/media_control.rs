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
                              「快进到十分钟」=600)。用户说「暂停/接着放/别放了/大点声/\
                              1.5 倍速/跳到第 90 秒」时用。没有在放东西就别调。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": ["pause", "resume", "stop", "louder", "softer", "speed", "seek"]
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
        ctx.media.control(action, value)?;
        Ok("ok".into())
    }
}
