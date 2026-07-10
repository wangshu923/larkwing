//! 能力轴:未了的事(记一笔 + 了结)。★主动关怀里程碑 切片2·B —— 让旺财跨会话记住用户没做完的
//! 打算,日后顺口关心(跟进**倾向**在 `engine/context::CARE_FOLLOWUP`,受 care.enabled 收口;
//! **本工具只管存取**)。独立于记忆系统 —— §13「宁缺毋滥」记事准则不管这里(见 store/todos)。

use anyhow::Context;
use async_trait::async_trait;

use super::{Tool, ToolCtx, ToolSpec};

/// 单条上限(防塞长段;与 remember 同量级)。
const TODO_MAX_CHARS: usize = 120;

pub(super) struct NoteTodo {
    spec: ToolSpec,
}

impl NoteTodo {
    pub(super) fn new() -> NoteTodo {
        NoteTodo {
            spec: ToolSpec {
                name: "note_todo",
                description: "用户提到想做 / 想买 / 想去、但**还没做**的事(未了的打算)时,记一笔留着 —— \
                              日后你可以在合适的时候顺口关心一句进展。\
                              只记真有个「以后要办」的打算;一次性闲话、已经做完的、纯情绪、你自己的推测都别记。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "content": {
                            "type": "string",
                            "description": "那件没了结的事,简短一句,如「想给妈妈买生日礼物」「打算周末收拾书房」"
                        }
                    },
                    "required": ["content"]
                }),
                timeout: std::time::Duration::from_secs(5),
                ui_key: "tool.note_todo",
            },
        }
    }
}

#[async_trait]
impl Tool for NoteTodo {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> anyhow::Result<String> {
        let content = args
            .get("content")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .context("缺少 content 参数")?
            .to_string();
        let n = content.chars().count();
        if n > TODO_MAX_CHARS {
            // 超长退回、不静默截断(§3.5)
            anyhow::bail!("这条 {n} 字,超过 {TODO_MAX_CHARS} 字上限,没记。请精简成一句话再试。");
        }
        let store = ctx.store.clone();
        let user_id = ctx.user_id;
        tokio::task::spawn_blocking(move || store.todos.add(user_id, &content))
            .await
            .context("记待办任务挂了")??;
        Ok("ok".into())
    }
}

pub(super) struct FinishTodo {
    spec: ToolSpec,
}

impl FinishTodo {
    pub(super) fn new() -> FinishTodo {
        FinishTodo {
            spec: ToolSpec {
                name: "finish_todo",
                description: "用户说某件之前记下、还没了结的事**已经做了 / 不打算做了** → 了结它,以后别再提。\
                              content 传那件事(照你在上文看到的原文最准,记不全给个能对上的片段也行)。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "content": {
                            "type": "string",
                            "description": "要了结的那件事(原文或能对上的片段)"
                        }
                    },
                    "required": ["content"]
                }),
                timeout: std::time::Duration::from_secs(5),
                ui_key: "tool.finish_todo",
            },
        }
    }
}

#[async_trait]
impl Tool for FinishTodo {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> anyhow::Result<String> {
        let content = args
            .get("content")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .context("缺少 content 参数")?
            .to_string();
        let store = ctx.store.clone();
        let user_id = ctx.user_id;
        let hit = tokio::task::spawn_blocking(move || store.todos.mark_done(user_id, &content))
            .await
            .context("了结待办任务挂了")??;
        // 没命中如实告知(§3.5),不静默
        Ok(if hit {
            "ok".into()
        } else {
            "没找到对应的事(可能已经了结过了)".to_string()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::MediaRuntime;
    use crate::store::Store;

    fn ctx(tag: &str) -> ToolCtx {
        let dir = std::env::temp_dir().join(format!("lw-todotool-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _ = std::fs::remove_file(dir.join("t.db"));
        let store = Store::open(&dir.join("t.db")).unwrap();
        store.users.ensure_default_user().unwrap();
        ToolCtx { user_id: 1, conv_id: 1, media: MediaRuntime::detached(store.clone()), store, web: None }
    }

    #[tokio::test]
    async fn note_then_finish_roundtrip() {
        let ctx = ctx("rt");
        NoteTodo::new()
            .run(serde_json::json!({"content": "想给妈妈买生日礼物"}), &ctx)
            .await
            .unwrap();
        assert_eq!(ctx.store.todos.list_open(1, 10).unwrap().len(), 1);
        // 子串了结
        let out = FinishTodo::new()
            .run(serde_json::json!({"content": "买生日礼物"}), &ctx)
            .await
            .unwrap();
        assert_eq!(out, "ok");
        assert!(ctx.store.todos.list_open(1, 10).unwrap().is_empty());
        // 再了结 = 没命中,如实告知(不静默)
        let miss = FinishTodo::new()
            .run(serde_json::json!({"content": "买生日礼物"}), &ctx)
            .await
            .unwrap();
        assert!(miss.starts_with("没找到"));
    }

    #[tokio::test]
    async fn over_limit_rejects() {
        let ctx = ctx("over");
        let long = "事".repeat(TODO_MAX_CHARS + 10);
        assert!(NoteTodo::new()
            .run(serde_json::json!({"content": long}), &ctx)
            .await
            .is_err());
    }
}
