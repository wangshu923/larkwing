//! 记忆提炼 / 反思(PLAN §13.3 ⑤ / §13.6 Phase 3):后台把最近一段对话蒸馏成耐久记忆 ——
//! 把用户透露过、但模型当时没显式 `remember` 的事实/习惯沉淀下来,让经验**自动累积**。
//!
//! **保守落地(本期硬纪律,§13.4 污染控制)**:
//! - **只增不删**:绝不删除/改写用户原记忆(蒸馏是猜测,不是权威)。
//! - **永远进按需层**:提炼条目落 `source='distilled'` 且 `resident=0` —— 不自动污染前缀,
//!   要靠 `recall` 复用(salience 强化)或用户确认才可能升层(升层本身后置)。
//! - **去重守门**:与已有记忆近重复的直接跳过(`has_similar`)。
//! - 蒸馏**质量**取决于真模型 + 真对话,Mac 上只能验「管线」(canned 输出→落库正确),验不了「提炼得好不好」。
//!
//! 形状是纯函数(`build_request` / `parse`,可单测)+ 一个编排(`run`,FakeLlm 可端到端测)。
//! 触发(每 N 轮 / 会话收尾)+ cheap-model 路由本期**未接**:自动跑一个未经真模型验证的 LLM 环
//! 去改用户记忆,风险/成本都该先验;`run` 现作为可调用入口(engine 方法 / 命令 / 收尾后续接)。

use std::sync::Arc;

use anyhow::Result;

use crate::llm::{ChatMessage, ChatOptions, ChatRequest, LlmProvider, ToolChoice};
use crate::store::memory::{KIND_EXPERIENCE, KIND_FACT, KIND_IDENTITY};
use crate::store::{Memory, Message, Store};

/// 整理器法条(人格中立、不与人对话,§5):立规则,不教学。
const SYSTEM: &str = "你是后台记忆整理器,不与任何人对话,只输出 JSON。\
  读下面「已记得的事」和「最近对话」,提炼出**值得长期记住、但还没记下**的、关于用户或这个家的事实或习惯。\
  规则:① 已记得的别重复;② 一次性的闲聊/情绪/天气/当下安排不要记;③ 拿不准就别记(宁缺毋滥);\
  ④ 最多 5 条;⑤ 第三人称简短陈述。\
  输出一个 JSON 数组,每项形如 {\"content\":\"...\",\"kind\":\"identity|experience|fact\"}:\
  身份/安全/家人(名字、过敏、忌口)→ identity;做事习惯/偏好 → experience;其它长期事实 → fact。\
  没有值得记的就输出 []。";

/// 一次最多提炼几条(与法条 ④ 一致,代码再兜一道)。
const MAX_ITEMS: usize = 5;
/// 单条内容上限(同 remember 的防撑爆)。
const CONTENT_MAX_CHARS: usize = 200;

/// 蒸馏出的一条候选(种类已归一到合法常量)。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DistilledItem {
    pub content: String,
    pub kind: &'static str,
}

/// 构造蒸馏请求(纯函数,可 golden):system=整理器法条,body=已记忆清单 + 最近对话转录。
pub(crate) fn build_request(recent: &[Message], existing: &[Memory]) -> ChatRequest {
    let mut body = String::from("【已记得的事(别重复)】\n");
    if existing.is_empty() {
        body.push_str("(暂无)\n");
    } else {
        for m in existing {
            body.push_str("- ");
            body.push_str(&m.content);
            body.push('\n');
        }
    }
    body.push_str("\n【最近对话】\n");
    for m in recent {
        let who = match m.role.as_str() {
            "user" => "用户",
            "assistant" => "7274",
            _ => continue, // tool / event 行不进转录
        };
        body.push_str(who);
        body.push_str(": ");
        body.push_str(&m.content);
        body.push('\n');
    }
    ChatRequest {
        system: SYSTEM.into(),
        messages: vec![ChatMessage::User { content: body, parts: vec![] }],
        options: ChatOptions::default(),
        tools: vec![],
        tool_choice: ToolChoice::default(),
    }
}

/// 解析蒸馏输出:容忍 ```json 围栏/前后废话(抠第一个 `[` 到最后一个 `]`);非法/缺 kind 落 fact;
/// 空 content 丢弃;裁长度;封顶条数。解析失败 = 空(后台尽力件,绝不抛给主对话)。
pub(crate) fn parse(text: &str) -> Vec<DistilledItem> {
    let slice = match (text.find('['), text.rfind(']')) {
        (Some(a), Some(b)) if b > a => &text[a..=b],
        _ => return Vec::new(),
    };
    let raw: Vec<serde_json::Value> = serde_json::from_str(slice).unwrap_or_default();
    raw.into_iter()
        .filter_map(|v| {
            let content: String = v
                .get("content")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())?
                .chars()
                .take(CONTENT_MAX_CHARS)
                .collect();
            let kind = match v.get("kind").and_then(serde_json::Value::as_str) {
                Some("identity") => KIND_IDENTITY,
                Some("experience") => KIND_EXPERIENCE,
                _ => KIND_FACT,
            };
            Some(DistilledItem { content, kind })
        })
        .take(MAX_ITEMS)
        .collect()
}

/// 跑一次提炼:加载最近对话 + 现有记忆 → 调 LLM(非流式)→ 解析 → **保守落库**
/// (近重复跳过、其余 `add_distilled` 进按需层)。返回新增条数。
pub(crate) async fn run(
    provider: &Arc<dyn LlmProvider>,
    store: &Store,
    user_id: i64,
    conv_id: i64,
    lookback: i64,
) -> Result<usize> {
    let recent = store.chat.recent_messages(conv_id, lookback)?;
    // 没有用户发言 = 没有可提炼的材料(任务专属会话只有 event 行时跳过)
    if !recent.iter().any(|m| m.role == "user") {
        return Ok(0);
    }
    let existing = store.memory.list(user_id)?;
    let req = build_request(&recent, &existing);
    let text = provider
        .chat(req)
        .await
        .map_err(|e| anyhow::anyhow!("记忆提炼 LLM 调用失败: {e:?}"))?;
    let mut added = 0usize;
    for item in parse(&text) {
        if store.memory.has_similar(user_id, &item.content)? {
            continue; // 去重守门
        }
        store.memory.add_distilled(user_id, item.kind, &item.content)?;
        added += 1;
    }
    Ok(added)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::fake::{FakeLlm, FakeTurn};

    fn store(tag: &str) -> Store {
        let dir = std::env::temp_dir().join(format!("lw-consol-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _ = std::fs::remove_file(dir.join("t.db"));
        Store::open(&dir.join("t.db")).unwrap()
    }

    #[test]
    fn parse_tolerates_fences_and_clamps_kind() {
        let txt = "好的:```json\n[{\"content\":\"A\",\"kind\":\"identity\"},\
                   {\"content\":\"B\",\"kind\":\"weird\"},{\"content\":\"  \",\"kind\":\"fact\"}]\n```";
        let items = parse(txt);
        assert_eq!(items.len(), 2, "空 content 被丢");
        assert_eq!(items[0].kind, KIND_IDENTITY);
        assert_eq!(items[1].kind, KIND_FACT, "未知 kind 落 fact");
        assert!(parse("我觉得没啥好记的").is_empty(), "非 JSON → 空(不抛错)");
        assert!(parse("[]").is_empty());
    }

    #[tokio::test]
    async fn run_distills_into_on_demand_and_dedups() {
        let store = store("run");
        let user = store.users.ensure_default_user().unwrap();
        let conv = store.chat.create_conversation(user.id, "companion").unwrap();
        store.chat.append_message(conv.id, "user", "我家猫叫咪咪;对了我们整理音乐都按歌手分").unwrap();
        store.chat.append_message(conv.id, "assistant", "记住啦~").unwrap();
        // 已有记忆:提炼里若返回「猫」相关会被去重
        store.memory.add(user.id, KIND_FACT, "用户养了只猫叫咪咪", "explicit").unwrap();

        // FakeLlm 脚本:一条新 experience + 一条与已有重复(应被去重)
        let json = "提炼结果:[{\"content\":\"整理音乐按歌手分\",\"kind\":\"experience\"},\
                    {\"content\":\"用户养了只猫叫咪咪\",\"kind\":\"fact\"}]";
        let provider: Arc<dyn LlmProvider> =
            Arc::new(FakeLlm::scripted(vec![FakeTurn { text: json.into(), ..Default::default() }]));

        let added = run(&provider, &store, user.id, conv.id, 50).await.unwrap();
        assert_eq!(added, 1, "新 experience 落地、重复的猫被去重跳过");

        let distilled: Vec<Memory> =
            store.memory.list(user.id).unwrap().into_iter().filter(|m| m.source == "distilled").collect();
        assert_eq!(distilled.len(), 1);
        assert_eq!(distilled[0].content, "整理音乐按歌手分");
        assert_eq!(distilled[0].kind, "experience");
        assert!(!distilled[0].resident, "提炼条目进按需层、不污染前缀");
        // 进按需层 → 不在 list_resident,但 recall 取得到
        assert!(!store.memory.list_resident(user.id).unwrap().iter().any(|m| m.id == distilled[0].id));
        assert!(store.memory.recall(user.id, "歌手").unwrap().iter().any(|m| m.content.contains("歌手分")));
    }

    #[tokio::test]
    async fn run_noops_on_empty_distillation() {
        let store = store("empty");
        let user = store.users.ensure_default_user().unwrap();
        let conv = store.chat.create_conversation(user.id, "companion").unwrap();
        store.chat.append_message(conv.id, "user", "今天好累啊").unwrap();
        let provider: Arc<dyn LlmProvider> =
            Arc::new(FakeLlm::scripted(vec![FakeTurn { text: "[]".into(), ..Default::default() }]));
        assert_eq!(run(&provider, &store, user.id, conv.id, 50).await.unwrap(), 0);
        assert!(store.memory.list(user.id).unwrap().is_empty(), "没东西可提炼 = 不落库");
    }
}
