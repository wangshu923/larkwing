//! 能力轴:记忆(按需取)。§13.3 ③ —— 常驻·画像层每轮自带进前缀,这里取**沉在按需层**
//! 的情节/经验记忆(以前聊到过的习惯、被纠正的解读、低频的事)。命中即强化(salience+,
//! §13.3 ④)。镜像 briefing_lookup;与 remember(写)同域分工。常驻基础工具。

use anyhow::Context;
use async_trait::async_trait;

use super::{Tool, ToolCtx, ToolSpec};

pub(super) struct Recall {
    spec: ToolSpec,
}

impl Recall {
    pub(super) fn new() -> Recall {
        Recall {
            spec: ToolSpec {
                name: "recall",
                description: "翻你对用户/这个家的记忆:系统提示里「你记得关于用户的这些事」没写、\
                              但像是以前聊到过的(某个习惯、某次说过的偏好、家人或宠物的事),\
                              先用它查一遍再说「不记得」。关于环境/资源(目录、设备)的事用 briefing_lookup。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "关键词(匹配记忆内容),如「香菜」「猫」「放轻松的」"
                        }
                    },
                    "required": ["query"]
                }),
                timeout: std::time::Duration::from_secs(5),
                ui_key: "tool.recall",
            },
        }
    }
}

#[async_trait]
impl Tool for Recall {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> anyhow::Result<String> {
        let query: String = args
            .get("query")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .context("缺少 query 参数")?
            .to_string();
        let store = ctx.store.clone();
        let user_id = ctx.user_id;
        let q_log = query.clone();
        let hits = tokio::task::spawn_blocking(move || store.memory.recall(user_id, &query))
            .await
            .context("记忆检索任务挂了")??;
        // 观测「被召回」(§4.4 进库前的轻量版):测试时 tail 日志即知模型查了啥、命中哪几条。
        tracing::info!(
            target: "larkwing::memory",
            user = user_id, query = %q_log, hits = hits.len(),
            "recall → {}",
            hits.iter().map(|m| m.content.as_str()).collect::<Vec<_>>().join(" | ")
        );
        if hits.is_empty() {
            Ok("没翻到相关的记忆".into())
        } else {
            Ok(hits.iter().map(|m| format!("- {}", m.content)).collect::<Vec<_>>().join("\n"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::MediaRuntime;
    use crate::store::Store;
    use crate::store::memory::{KIND_EPISODIC, KIND_FACT};

    fn ctx(tag: &str) -> ToolCtx {
        let dir = std::env::temp_dir().join(format!("lw-recall-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _ = std::fs::remove_file(dir.join("t.db"));
        let store = Store::open(&dir.join("t.db")).unwrap();
        // memories FK 到 users → 得先有这个人(briefing 无 FK 才能省这步)
        let me = store.users.ensure_default_user().unwrap();
        ToolCtx { user_id: me.id, conv_id: 1, media: MediaRuntime::detached(store.clone()), store, web: None, confirm: None }
    }

    #[tokio::test]
    async fn recall_returns_hits_and_reinforces() {
        let ctx = ctx("hit");
        let uid = ctx.user_id;
        ctx.store
            .memory
            .add(uid, KIND_EPISODIC, "上次说放轻松的指纯音乐歌单", "correction")
            .unwrap();
        ctx.store.memory.add(uid, KIND_FACT, "用户养了只猫叫咪咪", "explicit").unwrap();

        let r = Recall::new();
        let out = r.run(serde_json::json!({"query": "轻松"}), &ctx).await.unwrap();
        assert!(out.contains("纯音乐"), "命中按需层: {out}");
        assert!(!out.contains("咪咪"), "不相关的不返回");

        let miss = r.run(serde_json::json!({"query": "火星"}), &ctx).await.unwrap();
        assert!(miss.contains("没翻到"), "查不到给友好空话: {miss}");

        // 缺 query 报错(走两阶段错误的建连前那侧,模型据此改调)
        assert!(r.run(serde_json::json!({}), &ctx).await.is_err());
    }
}
