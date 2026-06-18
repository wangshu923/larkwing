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
                description: "把关于用户/这个家、值得长期记住的事记进小本本,之后的对话你会一直记得。\
                              用 kind 标明类别(影响它有多「牢」):\
                              identity=身份/安全/情感,绝不能忘的(名字、家人、过敏、忌口、纪念);\
                              experience=这个家「怎么做事」、或被用户纠正后学到的习惯(如「整理音乐按歌手分」「放视频先翻本地」);\
                              fact=其它长期事实(默认,如喜好)。\
                              闲聊情绪、一次性安排、你自己的推测不要记。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "fact": {
                            "type": "string",
                            "description": "要记住的事实,第三人称简短陈述,如「用户对花生过敏」「整理音乐按歌手分」"
                        },
                        "kind": {
                            "type": "string",
                            "enum": ["fact", "experience", "identity"],
                            "description": "类别:identity=身份/安全(绝不忘)、experience=做事习惯/被纠正学到的、fact=其它(默认)"
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
        let fact = args
            .get("fact")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .context("缺少 fact 参数")?
            .to_string();
        // 超长不静默截断(§3.5):退回错误,让模型精简成一句话或拆成多条分开记。
        let n = fact.chars().count();
        if n > FACT_MAX_CHARS {
            anyhow::bail!(
                "这条记忆 {n} 字,超过 {FACT_MAX_CHARS} 字上限,没有记。\
                 请精简成一句话,或拆成多条分开记,再重试。"
            );
        }
        // 类别由模型分(§13.4):identity 受保护绝不被下沉、experience 是程序性经验、
        // 其余默认 fact;非法值一律落 fact(防御性收口)。
        use crate::store::memory::{KIND_EXPERIENCE, KIND_FACT, KIND_IDENTITY};
        let kind = match args.get("kind").and_then(serde_json::Value::as_str).map(str::trim) {
            Some("identity") => KIND_IDENTITY,
            Some("experience") => KIND_EXPERIENCE,
            _ => KIND_FACT,
        };
        let store = ctx.store.clone();
        let user_id = ctx.user_id;
        // 阻塞 IO 下沉线程池(与 engine 同款纪律)。
        let (_, resident) = tokio::task::spawn_blocking(move || {
            store.memory.add(user_id, kind, &fact, "explicit")
        })
        .await
        .context("记忆落库任务挂了")??;
        // 常驻区满了 → 这条进了按需层,如实告知(不静默,§3.5;镜像 briefing)
        Ok(if resident {
            "ok".into()
        } else {
            "ok(常驻区满了,这条记成了按需查询 —— 想起时我会先翻一下)".to_string()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::MediaRuntime;
    use crate::store::Store;
    use crate::tools::Tool;

    fn ctx(tag: &str) -> ToolCtx {
        let dir = std::env::temp_dir().join(format!("lw-remembertool-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _ = std::fs::remove_file(dir.join("t.db"));
        let store = Store::open(&dir.join("t.db")).unwrap();
        store.users.ensure_default_user().unwrap();
        ToolCtx { user_id: 1, conv_id: 1, media: MediaRuntime::detached(store.clone()), store }
    }

    #[tokio::test]
    async fn normal_fact_is_remembered() {
        let ctx = ctx("ok");
        let out =
            Remember::new().run(serde_json::json!({"fact": "用户不吃香菜"}), &ctx).await.unwrap();
        assert!(out.starts_with("ok"));
    }

    #[tokio::test]
    async fn over_limit_rejects_not_truncates() {
        let ctx = ctx("overlimit");
        let too_long = "九".repeat(FACT_MAX_CHARS + 50);
        // 超长 → 退回错误,不静默截断(§3.5)
        assert!(Remember::new()
            .run(serde_json::json!({"fact": too_long}), &ctx)
            .await
            .is_err());
    }
}
