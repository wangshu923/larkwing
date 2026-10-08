//! 能力轴:分批干活(batch)。把同一批要干的活打包成**一次**工具调用:`calls` 里每项是一次
//! 内层工具调用(tool + args),程序按 `parallel` 的路数并行跑完,**整批只回一次结果 / 一次汇报**
//! —— 一张票据 = 一张 HUD 卡 = 一次回执(§7.1 bgtasks「一票一报」不变)。治 2026-10-08 用户
//! 100+ 集去广告实战的两件实锤:十几件 ffmpeg 各开票据各报一次、模型每条都「等全部完成」;
//! 显卡编码器一次只吃得下几路,模型手工切批撞错。组由 agent **自己打包**(显式),不按回合隐式捆
//! —— DAG 式规划里互不相干的几条线各成一组、各报各的,不互相拖(用户拍板)。
//!
//! 工具本体只做参数校验 + 转交 `ToolCtx::batch`(`BatchRunner` trait,delegate::SubAgent 同款接缝:
//! tools 定义、engine 实现,tools 不反向依赖 engine §6.1)。内层工具跑在「已经在后台」的语境里
//! (`ToolCtx::in_batch` = 整批的进度落点 `bgtasks::Beat`):会转后台的工具据此直接跑到底、
//! 不另开票据不另起卡,进度打到组上。ctx.batch = None(单测 / 子回合)→ 如实退回(§3.5)。

use std::sync::LazyLock;

use async_trait::async_trait;

use super::{Tool, ToolCtx, ToolSpec};

/// 每批件数上限(§4.11 用户拍板 30):一次调用的 JSON 会很长(十几条 ffmpeg 参数约两千 token),
/// 流式截断就整批作废 —— 超了让模型分几批。
pub const BATCH_MAX_CALLS: usize = 30;
/// 并行路数缺省(用户拍板 4):显卡编码器一次吃得下的量级;纯网络的活模型可以多开(上限 = 件数)。
pub const BATCH_DEFAULT_PARALLEL: usize = 4;
/// 标题上限(与计划卡标题同 30 字):HUD 卡标题 + 汇报标题。
pub const BATCH_TITLE_MAX_CHARS: usize = 30;
/// 逐件结果进汇报各截多少字(用户拍板 200):成品路径一般在首句里。
pub const BATCH_ITEM_RESULT_MAX_CHARS: usize = 200;
/// 整份汇报上限(与 delegate 的 `SUB_REPORT_MAX_CHARS` 同口径)。
pub const BATCH_REPORT_MAX_CHARS: usize = 4_000;

/// 内层不许放的工具(单源;golden `batch_whitelist_is_exactly_pinned` 枚举**保留集**,新增任何工具
/// 都会撞测试、逼一次「进不进 batch」的显式判定 —— 新工具五件套的同款)。排除四类:
/// ① 控制类 / 不嵌套 —— 计划槽、子回合、收尾信号、后台任务管控都是主回合对着对话做的事;batch 不套 batch;
/// ② 现场交互类 —— 对着眼前用户的动作(播放 / 图卡 / 开窗 / 音量 / 电源 / 打印 / 启动项)不该成批从后台
///    冒出来;web_render 带可见窗与确认闸的人机接力,也不进;
/// ③ 自带后台 job 与自己的并发闸(torrent §7.1 MAX_CONCURRENT 3),内联不了;
/// ④ fs_undo 撤的是「全局最新一批」,并行里一撤可能撤掉兄弟项刚落的(delegate 同款判定)。
/// 其余全放行:加工 / 下载 / 解压 / 读写文件 / 查网页 / 记事 —— 批里跑在父回合同一份语境里。
pub const BATCH_EXCLUDED: &[&str] = &[
    // ① 控制类 / 不嵌套
    "batch",
    "plan_set",
    "delegate",
    "end_conversation",
    "task_status",
    "task_cancel",
    // ② 现场交互类
    "media_play",
    "media_control",
    "show_image",
    "open",
    "system_volume",
    "power",
    "print_file",
    "startup_toggle",
    "web_render",
    // ③ 自带后台 job + 并发闸
    "torrent_download",
    // ④ 撤销需要对话语境
    "fs_undo",
];

/// 排除表的消费口(engine 侧 runner 过滤工具面用;单源判定)。
pub fn allowed_in_batch(name: &str) -> bool {
    !BATCH_EXCLUDED.contains(&name)
}

/// 一批里的一件:内层工具名 + 它的参数(原样转交,内层工具自己校验)。
#[derive(Debug, Clone)]
pub struct BatchCall {
    pub tool: String,
    pub args: serde_json::Value,
}

/// 校验归一后的一批。
#[derive(Debug, Clone)]
pub struct BatchSpec {
    pub title: String,
    /// 同时跑几件(1..=件数)。
    pub parallel: usize,
    pub calls: Vec<BatchCall>,
}

/// 参数校验(纯函数,单测钉着):标题 / 件数 / 每件形状 / 排除表 / 并行度归一。
/// 问题全部退回报错让模型改(§3.5),绝不静默截断或静默丢件。
pub fn parse_args(args: &serde_json::Value) -> anyhow::Result<BatchSpec> {
    let title = super::arg_str(args, "title")?;
    let title_len = title.chars().count();
    anyhow::ensure!(
        title_len <= BATCH_TITLE_MAX_CHARS,
        "title 太长({title_len} 字,上限 {BATCH_TITLE_MAX_CHARS} 字):一句话说清这批是什么就行"
    );
    let calls_v = args.get("calls").and_then(serde_json::Value::as_array).ok_or_else(|| {
        anyhow::anyhow!("缺 calls:数组,每项是 {{\"tool\": 工具名, \"args\": 该工具的参数对象}}")
    })?;
    anyhow::ensure!(
        !calls_v.is_empty(),
        "calls 是空的:至少放一件活;一两件的小事直接调工具,别用 batch"
    );
    anyhow::ensure!(
        calls_v.len() <= BATCH_MAX_CALLS,
        "一批最多 {BATCH_MAX_CALLS} 件,收到 {} 件:分几批来,每批跑完会回来一次汇报",
        calls_v.len()
    );
    let mut calls = Vec::with_capacity(calls_v.len());
    let mut excluded: Vec<String> = Vec::new();
    for (i, c) in calls_v.iter().enumerate() {
        let tool = c
            .get("tool")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!("第 {} 件缺 tool(工具名)", i + 1))?;
        let a = c.get("args").cloned().unwrap_or_else(|| serde_json::json!({}));
        anyhow::ensure!(a.is_object(), "第 {} 件的 args 得是对象(该工具的参数)", i + 1);
        if !allowed_in_batch(tool) {
            excluded.push(tool.to_string());
        }
        calls.push(BatchCall { tool: tool.to_string(), args: a });
    }
    if !excluded.is_empty() {
        excluded.sort();
        excluded.dedup();
        anyhow::bail!(
            "这些工具不能放进 batch:{}。batch 里只放直接干活的工具(加工 / 下载 / 解压 / 读写文件 / \
             查网页这类);计划、分头办事、播放控制、打开网页、种子下载各自单独调。",
            excluded.join("、")
        );
    }
    let parallel = super::arg_u64(args, "parallel", BATCH_DEFAULT_PARALLEL as u64) as usize;
    let parallel = parallel.clamp(1, calls.len());
    Ok(BatchSpec { title, parallel, calls })
}

/// 分批执行接缝(delegate::SubAgent 同款):tools 定义、engine 实现并经 `ToolCtx::batch` 注入。
#[async_trait]
pub trait BatchRunner: Send + Sync {
    /// 跑一批:并行执行、整批只回一次结果。半分钟内跑完当场返回汇总;没跑完转后台,返回
    /// 「已转后台(编号 N)」,收尾经 report job 唤回合一次性汇报。Err = 这批没派出去
    /// (观察喂回模型,主回合自行修正重派)。
    async fn run(&self, ctx: &ToolCtx, spec: BatchSpec) -> anyhow::Result<String>;
}

/// 描述里的数字与常量单源(§4.11):别在文案里再写一份数字。
static DESCRIPTION: LazyLock<String> = LazyLock::new(|| {
    format!(
        "把同一批要干的活打包成一次调用:calls 里每项 = 一次工具调用(tool 名 + 该工具的 args),\
         程序按 parallel 路并行跑完,**整批只回一次结果**(逐件成没成、产物在哪)。适合逐集剪 / \
         逐首配 / 逐个转 / 逐个下这类同类重复活 —— 别一条条单独调,也别一次塞太多(每批最多 \
         {BATCH_MAX_CALLS} 件,多了分几批,每批跑完会回来一次汇报)。半分钟内跑完当场回;更久自动转后台\
         接着跑(任务条一张卡、能叫停),跑完一次性回来汇报。parallel 按活的性质给:显卡编码一次吃不下\
         几路(缺省 {BATCH_DEFAULT_PARALLEL}),纯网络的活可以多开。calls 里只放直接干活的工具(加工 / \
         下载 / 解压 / 读写文件 / 查网页),不能再套 batch、分头办事、播放控制、打开网页、种子下载。"
    )
});

pub(super) struct Batch {
    spec: ToolSpec,
}

impl Batch {
    pub(super) fn new() -> Batch {
        Batch {
            spec: ToolSpec {
                name: "batch",
                description: DESCRIPTION.as_str(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "title": {
                            "type": "string",
                            "description": format!("这批活一句话(不超过 {BATCH_TITLE_MAX_CHARS} 字),任务卡与汇报都用它")
                        },
                        "parallel": {
                            "type": "integer",
                            "description": format!("同时跑几件;缺省 {BATCH_DEFAULT_PARALLEL}(显卡编码的量级),纯网络活可以多开;不超过件数")
                        },
                        "calls": {
                            "type": "array",
                            "description": format!("每项一次工具调用;最多 {BATCH_MAX_CALLS} 件"),
                            "items": {
                                "type": "object",
                                "properties": {
                                    "tool": { "type": "string", "description": "工具名" },
                                    "args": { "type": "object", "description": "该工具的参数" }
                                },
                                "required": ["tool", "args"]
                            }
                        }
                    },
                    "required": ["title", "calls"]
                }),
                // 同步段只等 30s(media::IN_TURN_WAIT),之后要么已回汇总、要么已转后台秒回 ——
                // 60s 是兜底(装配 / 工具面校验的余量),不是等活干完的预算(delegate 同款)。
                timeout: std::time::Duration::from_secs(60),
                ui_key: "tool.batch",
            },
        }
    }
}

#[async_trait]
impl Tool for Batch {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> anyhow::Result<String> {
        let spec = parse_args(&args)?;
        let Some(runner) = ctx.batch.clone() else {
            anyhow::bail!("这里没有分批通道(子回合里 / 未接线):一条条直接调工具就行。");
        };
        runner.run(ctx, spec).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx(tag: &str) -> ToolCtx {
        let dir = std::env::temp_dir().join(format!("lw-batch-{}-{tag}", std::process::id()));
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
            batch: None,
            in_batch: None,
        }
    }

    #[test]
    fn parse_validates_shape_limits_and_exclusions() {
        let err = parse_args(&json!({ "calls": [] })).unwrap_err().to_string();
        assert!(err.contains("title"), "缺标题要点名:{err}");

        let err = parse_args(&json!({ "title": "x", "calls": [] })).unwrap_err().to_string();
        assert!(err.contains("空的"), "空批退回:{err}");

        let long = "长".repeat(BATCH_TITLE_MAX_CHARS + 1);
        let err = parse_args(&json!({ "title": long, "calls": [{ "tool": "now", "args": {} }] }))
            .unwrap_err()
            .to_string();
        assert!(err.contains("太长"), "{err}");

        let many: Vec<_> = (0..=BATCH_MAX_CALLS).map(|_| json!({ "tool": "now", "args": {} })).collect();
        let err = parse_args(&json!({ "title": "多", "calls": many })).unwrap_err().to_string();
        assert!(err.contains("最多") && err.contains("分几批"), "超件数要指路分批:{err}");

        let err = parse_args(&json!({
            "title": "套娃",
            "calls": [
                { "tool": "plan_set", "args": {} },
                { "tool": "batch", "args": {} },
                { "tool": "now", "args": {} }
            ]
        }))
        .unwrap_err()
        .to_string();
        assert!(err.contains("batch、plan_set") && err.contains("不能放进"), "排除表要点名排序去重:{err}");

        // 缺 args 当空对象;parallel 缺省 4 并夹到件数;字符串数字也认(arg_u64 宽容)
        let spec = parse_args(&json!({ "title": "看时间", "calls": [{ "tool": "now" }, { "tool": "now", "args": {} }] }))
            .unwrap();
        assert_eq!(spec.calls.len(), 2);
        assert_eq!(spec.parallel, 2, "缺省 {BATCH_DEFAULT_PARALLEL} 夹到件数 2");
        let spec = parse_args(&json!({ "title": "看时间", "parallel": "0", "calls": [{ "tool": "now", "args": {} }] }))
            .unwrap();
        assert_eq!(spec.parallel, 1, "0 夹到 1");
    }

    #[tokio::test]
    async fn no_runner_bails_honestly() {
        let tool = Batch::new();
        let err = tool
            .run(json!({ "title": "看时间", "calls": [{ "tool": "now", "args": {} }] }), &ctx("norunner"))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("分批通道"), "未接线 / 子回合要如实说(§3.5):{err}");
    }

    /// 保留集 golden(枚举**保留集**不是排除集):以后新增任何工具都会撞这条,逼一次
    /// 「进不进 batch」的显式判定(新工具五件套同款,§6.5)。
    #[test]
    fn batch_whitelist_is_exactly_pinned() {
        let reg = crate::tools::Tools::builtin();
        for name in BATCH_EXCLUDED {
            assert!(reg.get(name).is_some(), "BATCH_EXCLUDED 里的 {name} 不在注册表(拼错了?)");
        }
        let mut kept: Vec<&str> = reg.names().into_iter().filter(|n| allowed_in_batch(n)).collect();
        kept.sort_unstable();
        let want = vec![
            "briefing_lookup",
            "briefing_remove",
            "briefing_write",
            "ffmpeg_run",
            "finish_todo",
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
            "note_todo",
            "now",
            "pdf_to_png",
            "qr_decode",
            "qr_encode",
            "read_audio",
            "read_image",
            "recall",
            "remember",
            "reminder_cancel",
            "reminder_list",
            "reminder_set",
            "send_file",
            "send_text",
            "skill_lookup",
            "skill_remove",
            "skill_write",
            "speak_to_file",
            "startup_list",
            "system_status",
            "watch_set",
            "weather",
            "web_download",
            "web_fetch",
            "web_search",
        ];
        assert_eq!(kept, want, "batch 保留集变了:新工具要显式判定进不进 batch(§6.5 五件套)");
    }
}
