//! 能力轴:干长活的工作备忘(控制原语,end_conversation 同族)。多步骤/多批次任务先把
//! 步骤列成清单,每完成一批全量替换更新 —— 治「批量干一半停下来等人踢」三成因(AGENT §6.5):
//! 自检句纯刹车 / 跨回合断链(job 收尾唤回合不知道还剩哪些)/ 超长回合截断丢任务原话。
//!
//! 工具本体**无状态**:只解析校验 + 回显清单当观察;真正写会话槽(SessionSlot.plan)+ 发
//! `AppEvent::Plan` 快照由回合循环嗅探完成(end_conversation 同构,turn.rs)。计划是会话级
//! 瞬态、派生可丢、不落库(§6.4 入槽资格);持久化/优先级/依赖关系明确不做(§9)。
//! 解析必须与嗅探共用本文件的 `parse_args` 单源 —— 两处各解一遍就会漂。

use async_trait::async_trait;

use super::{arg_bool, Tool, ToolCtx, ToolSpec};

/// §4.11 用户拍板(2026-08-18「稍微大些」终值)。超限一律退回报错让模型精简/拆条,
/// 绝不静默截断(§3.5)。
pub const PLAN_MAX_ITEMS: usize = 30;
pub const PLAN_ITEM_CHARS: usize = 120;
pub const PLAN_TITLE_CHARS: usize = 30;

/// 一份工作计划:标题(可选短名)+ 步骤清单。空 items = 无计划(清空态)。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Plan {
    pub title: Option<String>,
    pub items: Vec<PlanItem>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlanItem {
    pub text: String,
    pub done: bool,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn done_count(&self) -> usize {
        self.items.iter().filter(|i| i.done).count()
    }

    /// 「此刻」背景行(engine::inject_ambient 消费):无计划 None;有未完项则列出
    /// (那就是接下来要干的),已完成只计数;全完成提示收尾/清空。行内用「、」分隔,
    /// 别用「;」—— 多条 ambient 源之间是拿「;」拼的。
    pub fn ambient_line(&self) -> Option<String> {
        if self.items.is_empty() {
            return None;
        }
        let (done, total) = (self.done_count(), self.items.len());
        let name = self.title.as_deref().map(|t| format!("「{t}」")).unwrap_or_default();
        if done == total {
            return Some(format!("计划{name}:{done}/{total} 全部完成(该向用户收尾汇报,或用 plan_set 清空)"));
        }
        // 末尾那句提示(2026-09-17 用户「除了进度是不是该再给个提示」):进度本就每回合在这儿,
        // 模型不动手的是「更新」这一步 —— 把怎么更新、什么时候清空一并摆在眼前。
        Some(format!(
            "计划{name}:完成 {done}/{total},接下来:{}(做完一批就 plan_set 传 done=[序号] 标完成,\
             用户换了别的事就传空 items 清空)",
            self.undone_texts().join("、")
        ))
    }

    /// 自检句用的未完项一串(「a、b、c」);没有未完项 = None。
    pub fn remaining_brief(&self) -> Option<String> {
        let undone = self.undone_texts();
        if undone.is_empty() { None } else { Some(undone.join("、")) }
    }

    fn undone_texts(&self) -> Vec<&str> {
        self.items.iter().filter(|i| !i.done).map(|i| i.text.as_str()).collect()
    }
}

/// 解析 + 校验(单源:工具 run 与回合循环嗅探共用)。宽容(§4.4 Quirks):items 里的
/// 裸字符串当 `{text, done:false}`;done 吃 arg_bool(字符串 "true"/"1" 都认);title
/// 空白当没给。items 传空数组 = 清空计划,合法。超限/空文本 bail,话术引导精简或拆条。
pub fn parse_args(args: &serde_json::Value) -> anyhow::Result<Plan> {
    let title = match args.get("title").and_then(|v| v.as_str()).map(str::trim) {
        Some("") | None => None,
        Some(t) => {
            let n = t.chars().count();
            if n > PLAN_TITLE_CHARS {
                anyhow::bail!("title 太长({n} 字,上限 {PLAN_TITLE_CHARS} 字):给整件事起个短名。");
            }
            Some(t.to_string())
        }
    };
    let Some(raw) = args.get("items").and_then(|v| v.as_array()) else {
        anyhow::bail!(
            "缺少 items 数组:每项 {{text, done}}(或直接一句话字符串),每次发完整清单全量替换;清空计划传空数组。"
        );
    };
    if raw.len() > PLAN_MAX_ITEMS {
        anyhow::bail!(
            "计划最多 {PLAN_MAX_ITEMS} 条(收到 {} 条):把细碎步骤合并成更大的批次,重新发一遍。",
            raw.len()
        );
    }
    let mut items = Vec::with_capacity(raw.len());
    for (i, it) in raw.iter().enumerate() {
        let n = i + 1;
        let (text, done) = match it {
            serde_json::Value::String(s) => (s.trim().to_string(), false),
            other => (
                other.get("text").and_then(|v| v.as_str()).unwrap_or("").trim().to_string(),
                arg_bool(other, "done", false),
            ),
        };
        if text.is_empty() {
            anyhow::bail!("第 {n} 条的 text 是空的:每条写一句要干的事。");
        }
        let chars = text.chars().count();
        if chars > PLAN_ITEM_CHARS {
            anyhow::bail!("第 {n} 条太长({chars} 字,上限 {PLAN_ITEM_CHARS} 字):精简成一句话,细节不用写进计划。");
        }
        items.push(PlanItem { text, done });
    }
    Ok(Plan { title, items })
}

/// plan_set 的两种形(2026-09-17):`items` = 全量替换(老形);只给 `done` = **增量标完成** ——
/// 把已有清单里第 N 项(1 起)或原文对得上的项标 done,不用重发整张清单。缘由 = 用户实锤
/// 「创建了基本不咋更新」:进度每回合都在〔此刻〕里给模型了,它不动手的原因之一是全量重发
/// 整张清单对它是负担;把更新降到一句话。两者都给按 items 全量算。
#[derive(Debug, Clone, PartialEq)]
pub enum PlanOp {
    Replace(Plan),
    MarkDone(Vec<DoneSel>),
}

/// `done` 里的一个选择器:序号(1 起)或那一项的原文(大小写不敏感、子串双向对得上就算)。
#[derive(Debug, Clone, PartialEq)]
pub enum DoneSel {
    Index(usize),
    Text(String),
}

/// 解析一次调用(单源:工具 run 与回合循环嗅探共用)。`items` 在 → Replace;否则 `done` 在 → MarkDone;
/// 都没有 / done 空 / 序号不是正整数 → bail,话术引导两种形。
pub fn parse_op(args: &serde_json::Value) -> anyhow::Result<PlanOp> {
    if args.get("items").is_some() {
        return parse_args(args).map(PlanOp::Replace);
    }
    let Some(done) = args.get("done").and_then(|v| v.as_array()) else {
        anyhow::bail!(
            "缺少 items 或 done:改清单发完整 items(全量替换,清空传空数组);\
             只标完成传 done=[序号(1 起)或那一项的原文]。"
        );
    };
    anyhow::ensure!(!done.is_empty(), "done 是空的:填要标完成的序号(1 起)或那一项的原文。");
    let mut sels = Vec::with_capacity(done.len());
    for d in done {
        match d {
            serde_json::Value::Number(n) => {
                let i = n
                    .as_u64()
                    .filter(|i| *i >= 1)
                    .ok_or_else(|| anyhow::anyhow!("done 里的序号要是 1 起的正整数,收到 {n}"))?;
                sels.push(DoneSel::Index(i as usize));
            }
            serde_json::Value::String(s) => {
                let s = s.trim();
                anyhow::ensure!(!s.is_empty(), "done 里有空项:填序号或那一项的原文。");
                match s.parse::<usize>() {
                    Ok(i) if i >= 1 => sels.push(DoneSel::Index(i)),
                    Ok(_) => anyhow::bail!("done 里的序号要是 1 起的正整数,收到 {s}"),
                    Err(_) => sels.push(DoneSel::Text(s.to_string())),
                }
            }
            other => anyhow::bail!("done 每项要是序号或原文,收到 {other}"),
        }
    }
    Ok(PlanOp::MarkDone(sels))
}

/// 把一步操作落到当前计划上(纯函数;回合循环嗅探时调)。MarkDone 对不上号的选择器忽略 ——
/// 工具本体无状态、校验不了序号越界,下一条〔此刻〕会把真实清单摆出来。
pub fn apply(current: &Plan, op: PlanOp) -> Plan {
    match op {
        PlanOp::Replace(p) => p,
        PlanOp::MarkDone(sels) => {
            let mut next = current.clone();
            for sel in sels {
                match sel {
                    DoneSel::Index(i) => {
                        if let Some(it) = next.items.get_mut(i - 1) {
                            it.done = true;
                        }
                    }
                    DoneSel::Text(t) => {
                        let tl = t.to_lowercase();
                        if let Some(it) = next.items.iter_mut().find(|it| {
                            let x = it.text.to_lowercase();
                            x == tl || x.contains(&tl) || tl.contains(&x)
                        }) {
                            it.done = true;
                        }
                    }
                }
            }
            next
        }
    }
}

/// 工具结果回显(喂回模型的观察):清单全貌 + 下一步引导;清空态说清已清空。
fn render_echo(plan: &Plan) -> String {
    if plan.is_empty() {
        return "计划已清空。".into();
    }
    let (done, total) = (plan.done_count(), plan.items.len());
    let name = plan.title.as_deref().map(|t| format!("「{t}」")).unwrap_or_default();
    let mut out = format!("计划{name}已记下(完成 {done}/{total}):\n");
    for it in &plan.items {
        out.push_str(if it.done { "✓ " } else { "○ " });
        out.push_str(&it.text);
        out.push('\n');
    }
    if done == total {
        out.push_str("全部完成:向用户收尾汇报;这份计划可用空 items 清空。");
    } else {
        out.push_str("接着干下一项;每做完一批就再调 plan_set:只标完成传 done=[序号],改清单才发完整 items。");
    }
    out
}

pub(super) struct PlanSet {
    spec: ToolSpec,
}

impl PlanSet {
    pub(super) fn new() -> PlanSet {
        PlanSet {
            spec: ToolSpec {
                name: "plan_set",
                description: "给正在干的多步骤长活记一份工作计划(只在当前会话内有效,不是给用户的提醒)。\
                              一件事要分好几步、好几批才能干完(批量处理一堆文件、多步网页操作、下载完接着加工),\
                              先把步骤列出来再动手;之后**每做完一批就再调一次:只标完成传 done=[序号]**\
                              (1 起,也可以传那一项的原文),不用重发清单;要改清单才发完整 items(全量替换,\
                              不是追加)。全部干完或者用户不要了,items 传空数组清空。三两步能办完的小事别用。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "title": {
                            "type": "string",
                            "description": "整件事的短名(可选,不超过 30 字)"
                        },
                        "items": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "text": { "type": "string", "description": "一句话一步,不超过 120 字" },
                                    "done": { "type": "boolean", "description": "这一步是否已完成(缺省未完成)" }
                                },
                                "required": ["text"]
                            },
                            "description": "完整步骤清单(全量替换,最多 30 条);空数组 = 清空计划。只标完成不用传它,传 done 就行"
                        },
                        "done": {
                            "type": "array",
                            "items": { "type": ["integer", "string"] },
                            "description": "把已有清单里这些项标为完成:序号(1 起)或那一项的原文;不传 items 时用它,清单其余不动"
                        }
                    }
                }),
                timeout: std::time::Duration::from_secs(5),
                ui_key: "tool.plan_set",
            },
        }
    }
}

#[async_trait]
impl Tool for PlanSet {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: serde_json::Value, _ctx: &ToolCtx) -> anyhow::Result<String> {
        // 纯校验 + 回显:写槽/发事件由回合循环按同一 parse_op 嗅探完成(status=ok 才写)。
        match parse_op(&args)? {
            PlanOp::Replace(plan) => Ok(render_echo(&plan)),
            // 工具本体无状态,看不到当前清单:只确认收到,真实进度下一条〔此刻〕会带
            PlanOp::MarkDone(sels) => Ok(format!(
                "已把 {} 项标为完成(以当前清单为准,〔此刻〕会带最新进度)。接着干下一项;\
                 全部干完向用户收尾,再传空 items 清空。",
                sels.len()
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 增量标完成:序号 / 原文都认,其余项与标题不动;越界忽略;items 在就是全量替换;
    /// 都没给 / done 空 / 序号 0 报错;〔此刻〕行带更新与清空提示。
    #[test]
    fn parse_op_marks_done_by_index_or_text_and_apply_keeps_rest() {
        let cur = parse_args(&json!({ "title": "补词", "items": ["扫文件夹", "补第一批", "发手机"] })).unwrap();
        let next = apply(&cur, parse_op(&json!({ "done": [1, "第一批"] })).unwrap());
        assert_eq!(next.done_count(), 2);
        assert!(next.items[0].done && next.items[1].done && !next.items[2].done);
        assert_eq!(next.title.as_deref(), Some("补词"), "标完成不动标题");
        let next2 = apply(&cur, parse_op(&json!({ "done": ["3", 9] })).unwrap());
        assert!(next2.items[2].done && !next2.items[0].done, "字符串序号也认,越界忽略");
        assert!(matches!(parse_op(&json!({ "items": [] })).unwrap(), PlanOp::Replace(p) if p.is_empty()));
        assert!(parse_op(&json!({})).is_err());
        assert!(parse_op(&json!({ "done": [] })).is_err());
        assert!(parse_op(&json!({ "done": [0] })).is_err());
        let line = cur.ambient_line().unwrap();
        assert!(line.contains("done=[序号]") && line.contains("清空"), "{line}");
    }

    #[test]
    fn parse_accepts_objects_bare_strings_and_stringified_done() {
        let plan = parse_args(&json!({
            "title": "  给曲库补词  ",
            "items": [
                { "text": "扫一遍文件夹", "done": "true" },
                { "text": "补第一批歌词", "done": false },
                "把结果发手机"
            ]
        }))
        .unwrap();
        assert_eq!(plan.title.as_deref(), Some("给曲库补词"), "title 去空白");
        assert_eq!(plan.items.len(), 3);
        assert!(plan.items[0].done, "字符串 \"true\" 该认(arg_bool 同款宽容)");
        assert!(!plan.items[1].done);
        assert_eq!(plan.items[2], PlanItem { text: "把结果发手机".into(), done: false }, "裸字符串当未完成项");
        assert_eq!(plan.done_count(), 1);
    }

    #[test]
    fn parse_empty_items_is_clear_and_missing_items_bails() {
        let plan = parse_args(&json!({ "items": [] })).unwrap();
        assert!(plan.is_empty(), "空数组 = 清空计划,合法");
        let err = parse_args(&json!({ "title": "x" })).unwrap_err().to_string();
        assert!(err.contains("items"), "缺 items 要点名:{err}");
    }

    #[test]
    fn parse_caps_bail_with_guidance() {
        let many: Vec<_> = (0..31).map(|i| json!({ "text": format!("步骤{i}") })).collect();
        let err = parse_args(&json!({ "items": many })).unwrap_err().to_string();
        assert!(err.contains("30"), "超条数要报上限:{err}");

        let long = "长".repeat(121);
        let err = parse_args(&json!({ "items": [{ "text": long }] })).unwrap_err().to_string();
        assert!(err.contains("120") && err.contains("第 1 条"), "超长要点名第几条与上限:{err}");

        let err = parse_args(&json!({ "items": [{ "text": "  " }] })).unwrap_err().to_string();
        assert!(err.contains("第 1 条"), "空文本要点名:{err}");

        let err = parse_args(&json!({ "title": "名".repeat(31), "items": ["a"] }))
            .unwrap_err()
            .to_string();
        assert!(err.contains("30"), "title 超长要报上限:{err}");
    }

    #[test]
    fn ambient_line_lists_undone_and_counts_done() {
        assert_eq!(Plan::default().ambient_line(), None, "无计划不占背景");

        let plan = parse_args(&json!({
            "title": "整理曲库",
            "items": [
                { "text": "扫文件夹", "done": true },
                { "text": "补歌词" },
                { "text": "发手机" }
            ]
        }))
        .unwrap();
        let line = plan.ambient_line().unwrap();
        assert!(line.contains("整理曲库") && line.contains("1/3"), "带 title 与进度:{line}");
        assert!(line.contains("补歌词") && line.contains("发手机"), "未完项全列:{line}");
        assert!(!line.contains("扫文件夹"), "已完成项不列只计数:{line}");
        assert!(!line.contains(';') && !line.contains(';'), "行内别用分号(ambient 多源拿分号拼):{line}");

        let untitled = parse_args(&json!({ "items": ["a"] })).unwrap();
        let line = untitled.ambient_line().unwrap();
        assert!(line.contains("0/1") && line.contains('a'), "没 title 也成行:{line}");

        let done = parse_args(&json!({ "items": [{ "text": "a", "done": true }] })).unwrap();
        let line = done.ambient_line().unwrap();
        assert!(line.contains("全部完成"), "全完成提示收尾/清空:{line}");
    }

    #[test]
    fn remaining_brief_only_lists_undone() {
        let plan = parse_args(&json!({
            "items": [{ "text": "a", "done": true }, { "text": "b" }, { "text": "c" }]
        }))
        .unwrap();
        assert_eq!(plan.remaining_brief().as_deref(), Some("b、c"));
        let done = parse_args(&json!({ "items": [{ "text": "a", "done": true }] })).unwrap();
        assert_eq!(done.remaining_brief(), None, "没未完项 = None");
    }

    #[test]
    fn echo_shows_list_and_clear_state() {
        let plan = parse_args(&json!({
            "title": "整理曲库",
            "items": [{ "text": "扫文件夹", "done": true }, { "text": "补歌词" }]
        }))
        .unwrap();
        let echo = render_echo(&plan);
        assert!(echo.contains("整理曲库") && echo.contains("1/2"), "{echo}");
        assert!(echo.contains("✓ 扫文件夹") && echo.contains("○ 补歌词"), "{echo}");

        let cleared = render_echo(&Plan::default());
        assert!(cleared.contains("清空"), "{cleared}");
    }

    #[tokio::test]
    async fn tool_run_roundtrip() {
        let dir = std::env::temp_dir().join(format!("lw-plantool-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _ = std::fs::remove_file(dir.join("t.db"));
        let store = crate::store::Store::open(&dir.join("t.db")).unwrap();
        let ctx = ToolCtx {
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
        };
        let tool = PlanSet::new();
        let out = tool.run(json!({ "items": ["第一步", "第二步"] }), &ctx).await.unwrap();
        assert!(out.contains("第一步") && out.contains("0/2"), "{out}");
        assert!(tool.run(json!({}), &ctx).await.is_err(), "缺 items 该退回");
    }
}
