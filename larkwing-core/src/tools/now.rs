//! 能力轴:时间(读)。模型没有钟,这是它"看一眼现在"的原语。

use async_trait::async_trait;

use super::{Tool, ToolCtx, ToolSpec};

pub(super) struct Now {
    spec: ToolSpec,
}

impl Now {
    pub(super) fn new() -> Now {
        Now {
            spec: ToolSpec {
                name: "now",
                description: "查看现在的日期、时间和星期。当用户问今天几号/星期几/几点了,\
                              或你需要当前时间来推算日期(比如「下周三」是哪天)时使用。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
                timeout: std::time::Duration::from_secs(5),
                ui_key: "tool.now",
            },
        }
    }
}

#[async_trait]
impl Tool for Now {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, _args: serde_json::Value, _ctx: &ToolCtx) -> anyhow::Result<String> {
        let now = chrono::Local::now();
        let weekday = ["周一", "周二", "周三", "周四", "周五", "周六", "周日"]
            [chrono::Datelike::weekday(&now).num_days_from_monday() as usize];
        // 结果是喂给模型的观察(不是 UI 文案),用模型当前人格的语言最顺
        Ok(serde_json::json!({
            "now": now.format("%Y-%m-%d %H:%M").to_string(),
            "weekday": weekday,
        })
        .to_string())
    }
}
