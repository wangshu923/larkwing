//! 能力轴:分头办事(子回合委派)。把一件**自包含**的活派给一个新鲜上下文的子回合独立去跑
//! (同一份回合循环、同一套工具执行),跑完只把要点汇报带回主回合 —— 探查垃圾(几十轮
//! fs_list/web_fetch 原始结果)与错误重试噪音全关在子上下文里,主回合尾巴保持干净
//! (§6.5;长回合「越干越笨」的主因 = 上下文稀释 §4.5)。轮内 tool_calls 本就并发
//! (join_all),同一轮派几路 = 几路子回合真并行。
//!
//! 工具本体只做参数校验 + 转交 `ToolCtx::agent`(`SubAgent` trait,webrender 同款接缝:
//! tools 定义、engine 实现,tools 不反向依赖 engine §6.1)。子回合的装配/执行/30s 转后台
//! 全在 engine 侧 runner(engine/mod.rs)。ctx.agent = None(单测/未接线)→ 如实退回(§3.5)。

use async_trait::async_trait;

use super::{Tool, ToolCtx, ToolSpec};

/// 简报上限:失控 backstop(防把整页网页原文灌进简报),**不是工作上限**(§4.11 用户拍板
/// 2026-08-18:调研类简报要带背景材料,2000 掐脖子 → 10000)。大段材料该先 fs_write_text
/// 落成文件、简报里给路径。超限退回,绝不静默截断(§3.5)。
pub const SUB_TASK_MAX_CHARS: usize = 10_000;

/// 同时在跑的子回合路数上限(§4.11 用户拍板 4):满了如实退回不排队(bgtasks 口径)。
/// 计数盖全生命周期(同步等待段 + 转后台段),由 engine 侧 runner 的守卫维护。
pub const SUB_PARALLEL_MAX: usize = 4;

/// 子回合轮数上限(主回合 200 的收窄版):一路子任务该是一件自包含的活,40 轮还没完
/// 多半是简报圈得太大 —— 拆成几路再派。自检句/空转网照常生效(复用同一份回合循环)。
pub const SUB_MAX_ROUNDS: usize = 40;

/// 转后台收尾汇报里携带的汇报正文上限(report job 走 jobs.content,别无限膨胀;
/// 调研类长产出本就该落文件、汇报给路径 —— delegate_injection 里教了)。
pub const SUB_REPORT_MAX_CHARS: usize = 4_000;

/// 子回合工具面 = 场景白名单 − 本表(单源;`sub_whitelist_is_exactly_pinned` golden 枚举
/// **保留集**,新增任何工具都会撞测试、逼一次显式判定 —— 「新工具四件套」的第五件)。
/// 排除四类:
/// ① 会话/控制类 —— 计划槽是会话级、收尾信号是语音会话的、套娃锁死(深度 1 的白名单半边);
/// ② 持久知识写入类 —— 写「关于人/流程」的账需要对话语境,子回合没有(读类 recall/
///    briefing_lookup/skill_lookup 保留:查目录需知很有用);
/// ③ 跨回合/跨人调度类 —— 提醒/捎话/外发/后台任务管控留给拿得到全局语境的主回合;
/// ④ 现场交互类 —— 全局播放、聊天图卡(子回合不落库、图卡没有落点)、桌面开窗/音量/电源
///    是「对着眼前用户」的动作,不该从后台冒出来。
/// (enter_mode 属 ①,当前单场景未注册,注册之日照类入表。)
pub const SUB_EXCLUDED: &[&str] = &[
    // ① 会话/控制类
    "delegate",
    "plan_set",
    "end_conversation",
    // ② 持久知识写入类
    "remember",
    "briefing_write",
    "briefing_remove",
    "skill_write",
    "skill_remove",
    "note_todo",
    "finish_todo",
    // ③ 跨回合/跨人调度类
    "reminder_set",
    "reminder_list",
    "reminder_cancel",
    "watch_set",
    "send_file",
    "send_text",
    "task_status",
    "task_cancel",
    // fs_undo 撤的是「该用户全局最新的一批」(fsops.latest 不分回合)——并行子回合里
    // 一撤可能撤掉父回合/兄弟子回合刚落的批次(评审实锤)。撤哪一步需要对话语境,
    // 归主回合;子回合修自己的错走正向重做(fs_move 挪回去)。
    "fs_undo",
    // ④ 现场交互类
    "media_play",
    "media_control",
    "show_image",
    "open",
    "system_volume",
    "power",
];

/// 排除表的消费口(engine 侧 runner 过滤子集用;单源判定,别在别处再抄一份名单)。
pub fn allowed_in_sub(name: &str) -> bool {
    !SUB_EXCLUDED.contains(&name)
}

/// 子回合执行接缝(webrender::WebRenderer 同款):tools 定义、engine 实现并经
/// `ToolCtx::agent` 注入。engine → tools 单向,无环(§6.1 mod 边界)。
#[async_trait]
pub trait SubAgent: Send + Sync {
    /// 跑一路子回合:自包含任务简报 → 要点汇报。半分钟内跑完当场返回汇报;没跑完
    /// **不作废**,转 bgtasks 后台接着跑(返回「已转后台(编号 N)」,收尾经 report job
    /// 唤回合汇报)。Err = 这路活没成(观察喂回模型,主回合自行换路/重派)。
    async fn run(&self, ctx: &ToolCtx, task: &str) -> anyhow::Result<String>;
}

pub(super) struct Delegate {
    spec: ToolSpec,
}

impl Delegate {
    pub(super) fn new() -> Delegate {
        Delegate {
            spec: ToolSpec {
                name: "delegate",
                description: "把一件查证或处理的活分派给一个帮手独立去干,你只拿回结果要点。\
                              帮手看不到你们的对话,task 必须自包含一次写全:目标、范围、已知的路径或线索、\
                              要回报什么;大段背景材料先用 fs_write_text 落成文件,task 里给路径。\
                              要翻很多文件或网页才能弄清的探查,交给帮手比自己一步步翻省得多;\
                              互不依赖的几路活可以在同一轮里各派一个,并行跑。\
                              半分钟内干完的当场拿到汇报;更久的自动转后台接着跑,跑完会自动回来汇报。\
                              一两步就能办完的小事别派,自己直接调工具;帮手的汇报不够细,\
                              可以带更具体的问题再派一次。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "task": {
                            "type": "string",
                            "description": "自包含的任务简报:目标、范围、已知路径/线索、要回报什么(不超过 10000 字;大段材料落文件给路径)"
                        }
                    },
                    "required": ["task"]
                }),
                // 同步段只等 30s(media::IN_TURN_WAIT),之后要么已回汇报、要么已转后台
                // 秒回 —— 60s 是兜底(建连/装配的余量),不是等活干完的预算。
                timeout: std::time::Duration::from_secs(60),
                ui_key: "tool.delegate",
            },
        }
    }
}

#[async_trait]
impl Tool for Delegate {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> anyhow::Result<String> {
        let task = args.get("task").and_then(|v| v.as_str()).map(str::trim).unwrap_or("");
        if task.is_empty() {
            anyhow::bail!(
                "缺 task:写一份自包含的任务简报(目标、范围、已知路径或线索、要回报什么),\
                 帮手看不到对话,指代要全部展开。"
            );
        }
        let n = task.chars().count();
        if n > SUB_TASK_MAX_CHARS {
            anyhow::bail!(
                "简报太长({n} 字,上限 {SUB_TASK_MAX_CHARS} 字):大段材料先用 fs_write_text \
                 落成文件,简报里给路径让帮手自己读;简报只留目标、范围和要回报什么。"
            );
        }
        let Some(agent) = ctx.agent.clone() else {
            anyhow::bail!("没有分派通道(子回合执行器未接线),这件事自己动手做。");
        };
        agent.run(ctx, task).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx(tag: &str) -> ToolCtx {
        let dir = std::env::temp_dir().join(format!("lw-delegate-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _ = std::fs::remove_file(dir.join("t.db"));
        let store = crate::store::Store::open(&dir.join("t.db")).unwrap();
        ToolCtx {
            user_id: 1,
            conv_id: 1,
            media: crate::media::MediaRuntime::detached(store.clone()),
            store,
            web: None,
            voice: None,
            confirm: None,
            grants: Default::default(),
            agent: None,
        }
    }

    #[tokio::test]
    async fn missing_or_oversized_task_bails_with_guidance() {
        let tool = Delegate::new();
        let c = ctx("args");
        let err = tool.run(json!({}), &c).await.unwrap_err().to_string();
        assert!(err.contains("task") && err.contains("自包含"), "缺参要教怎么写:{err}");

        let long = "长".repeat(SUB_TASK_MAX_CHARS + 1);
        let err = tool.run(json!({ "task": long }), &c).await.unwrap_err().to_string();
        assert!(err.contains("10000") && err.contains("fs_write_text"), "超长要指路落文件:{err}");
    }

    #[tokio::test]
    async fn no_runner_bails_honestly() {
        let tool = Delegate::new();
        let err = tool.run(json!({ "task": "查一下" }), &ctx("norunner")).await.unwrap_err().to_string();
        assert!(err.contains("分派通道"), "未接线要如实说(§3.5):{err}");
    }

    /// 白名单 golden:枚举**保留集**(不是排除集)—— 以后新增任何工具都会撞这条,
    /// 逼一次「进不进子回合」的显式判定(「新工具四件套」的第五件,§6.5)。
    #[test]
    fn sub_whitelist_is_exactly_pinned() {
        let reg = crate::tools::Tools::builtin();
        // 排除表里的名字必须真实存在(防拼错 = 静默放行)
        for name in SUB_EXCLUDED {
            assert!(reg.get(name).is_some(), "SUB_EXCLUDED 里的 {name} 不在注册表(拼错了?)");
        }
        let mut kept: Vec<&str> = reg.names().into_iter().filter(|n| allowed_in_sub(n)).collect();
        kept.sort_unstable();
        let want = vec![
            "briefing_lookup",
            "ffmpeg_run",
            "fs_append",
            "fs_copy",
            "fs_edit",
            "fs_find",
            "fs_list",
            "fs_mkdir",
            "fs_move",
            "fs_read_text",
            "fs_stat",
            "fs_trash",
            "fs_unzip",
            "fs_usage",
            "fs_write_text",
            "fs_zip",
            "lyrics_fetch",
            "media_download",
            "media_search",
            "now",
            "pdf_to_png",
            "qr_decode",
            "qr_encode",
            "read_audio",
            "read_image",
            "recall",
            "skill_lookup",
            "torrent_download",
            "weather",
            "web_download",
            "web_fetch",
            "web_render",
            "web_search",
        ];
        assert_eq!(kept, want, "子回合保留集变了:新工具要显式判定进不进子回合(§6.5 五件套)");
    }
}
