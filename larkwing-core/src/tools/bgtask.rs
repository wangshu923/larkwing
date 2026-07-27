//! 能力轴:后台差事(查 / 停)。长活(批量下载、批量配歌词这类)在后台跑,这两个原语
//! 给模型「看到哪了」与「叫停」的手脚;收尾汇报由登记处自动唤回合,不用轮询等结果。
//! 「按时查岗」刻意不内建 —— 模型自己 reminder_set + task_status 组合(§5 正交原语)。

use async_trait::async_trait;

use super::{Tool, ToolCtx, ToolRisk, ToolSpec};

pub(super) struct TaskStatus {
    spec: ToolSpec,
}

impl TaskStatus {
    pub(super) fn new() -> TaskStatus {
        TaskStatus {
            spec: ToolSpec {
                name: "task_status",
                description: "看后台长活(批量下载/批量配歌词这类)跑到哪了:列出运行中的\
                              (编号、进度、正在处理哪个、累计没成的点名)和刚结束的。用户问\
                              「到哪了/完了吗/哪些没成」时用;〔此刻〕背景里已有一行简报,要\
                              细节才调这个。想按时查岗可以配合 reminder_set(比如设 5 分钟后\
                              「查一下进度并汇报」)。后台任务跑完本来就会自动回来一条汇报,\
                              不需要为等结果反复轮询。",
                parameters: serde_json::json!({ "type": "object", "properties": {} }),
                timeout: std::time::Duration::from_secs(10),
                ui_key: "tool.task_status",
            },
        }
    }
}

#[async_trait]
impl Tool for TaskStatus {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, _args: serde_json::Value, ctx: &ToolCtx) -> anyhow::Result<String> {
        Ok(ctx.media.bg().status_report())
    }
}

pub(super) struct TaskCancel {
    spec: ToolSpec,
}

impl TaskCancel {
    pub(super) fn new() -> TaskCancel {
        TaskCancel {
            spec: ToolSpec {
                name: "task_cancel",
                description: "叫停一个正在跑的后台任务(编号来自〔此刻〕背景或 task_status)。\
                              温和收尾:正在处理的那一项做完就停、不撕一半;停完会自动回来一条\
                              收尾汇报(已完成多少如实报)。用户说「停下/别下了/先别配了」时用。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "id": {
                            "type": "integer",
                            "description": "任务编号(〔此刻〕背景或 task_status 里带着)"
                        }
                    },
                    "required": ["id"]
                }),
                timeout: std::time::Duration::from_secs(10),
                ui_key: "tool.task_cancel",
            },
        }
    }
}

#[async_trait]
impl Tool for TaskCancel {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn risk(&self) -> ToolRisk {
        ToolRisk::Mutating
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> anyhow::Result<String> {
        let id = super::arg_u64(&args, "id", 0);
        anyhow::ensure!(id > 0, "缺少任务编号 id(从〔此刻〕背景或 task_status 里拿)");
        Ok(match ctx.media.bg().cancel(id) {
            Some(title) => format!(
                "已让「{title}」停下——正在处理的那一项做完就收尾,收尾后会自动回来一条汇报\
                 (已完成多少如实报)。"
            ),
            None => format!(
                "没有编号 {id} 的运行中任务(可能已经跑完了)。用 task_status 看看现状。"
            ),
        })
    }
}
