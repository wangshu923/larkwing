//! 能力轴:记忆(写)。记忆归人、跨场景(宪法 §6);这是「关了再开,它还记得你」
//! 闭环的写入端 —— 下个回合起,新记忆随画像层进稳定前缀。

use anyhow::Context;
use async_trait::async_trait;

use super::{Tool, ToolCtx, ToolSpec};

/// 单条记忆长度上限(防模型把整段对话塞进来撑爆前缀)。
const FACT_MAX_CHARS: usize = 200;

pub(super) struct Remember {
    spec: ToolSpec,
}

impl Remember {
    pub(super) fn new() -> Remember {
        Remember {
            spec: ToolSpec {
                name: "remember",
                description: "把关于用户的重要、长期有效的事实记进小本本,之后的对话你会一直记得。\
                              只记用户主动透露、值得长期记住的事:名字、家人、喜好、忌口、过敏、纪念日等。\
                              闲聊情绪、一次性安排、你自己的推测不要记。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "fact": {
                            "type": "string",
                            "description": "要记住的事实,第三人称简短陈述,如「女儿叫朵朵,生日在十月」"
                        }
                    },
                    "required": ["fact"]
                }),
                timeout: std::time::Duration::from_secs(5),
                ui_key: "tool.remember",
            },
        }
    }
}

#[async_trait]
impl Tool for Remember {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> anyhow::Result<String> {
        let fact: String = args
            .get("fact")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .context("缺少 fact 参数")?
            .chars()
            .take(FACT_MAX_CHARS)
            .collect();
        let store = ctx.store.clone();
        let user_id = ctx.user_id;
        // 阻塞 IO 下沉线程池(与 engine 同款纪律)
        tokio::task::spawn_blocking(move || store.memory.add(user_id, "fact", &fact))
            .await
            .context("记忆落库任务挂了")??;
        Ok("ok".into())
    }
}
