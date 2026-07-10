//! 记忆提炼 / 反思(PLAN §13.3 ⑤ / §13.6 Phase 3):后台把最近一段对话蒸馏成耐久记忆 ——
//! 把用户透露过、但模型当时没显式 `remember` 的事实/习惯沉淀下来,让经验**自动累积**。
//!
//! **提炼仍保守**:新提炼的条目落 `source='distilled'` 且 `resident=0`(进按需层不污染前缀,
//! 靠 `recall` 复用挣 salience 才升层)+ 去重守门(`has_similar`)。蒸馏**质量**只能真模型 + 真对话验。
//!
//! **但 Phase 3 已转「全量激进」维护(2026-06-23 用户拍板,推翻原『只增不删』)**:`run` 末尾跑
//! `MemoryRepo::maintain`(确定性:衰减→下沉→升层→合并近重复→硬清过期,**会删除非 identity 记忆**),
//! 且 LLM 可发 `replaces` 指令走 `supersede` **纠错替换**旧记忆(连用户显式记的 explicit 也可被替换 / 清,
//! 用户拍板接受误伤风险)。**唯一铁守 = 身份/安全类(`KIND_IDENTITY`)对一切删改全程豁免(§13.4)**:
//! 不衰减、不下沉、不合并、不纠错替换、不硬清 —— 那类只能用户亲口改。阈值见 `store/memory.rs` 顶部单源。
//!
//! 形状是纯函数(`build_request` / `parse`,可单测)+ 一个编排(`run`,FakeLlm 可端到端测)。
//! **自动触发已接(2026-06-18)**:`Engine::send_message` 每 `CONSOLIDATE_EVERY_TURNS` 个用户回合
//! 后台 spawn 一次(`Engine::spawn_consolidate`:尽力件、防并发、不阻塞回合、写到说话人)。
//! cheap-model 路由已接(2026-06-24,§13.6 变体 A):调用方(engine `background_provider`)挑**最便宜档**
//! provider 传进来,与聊天主选解耦、复用 tier 目录不新增模型名;`run` 本身对此无感(只认传入的 provider)。
//! 提炼 / 维护**质量**只能真模型 + 真对话验,Mac 验不了。

use std::sync::Arc;

use anyhow::Result;

use crate::llm::{ChatMessage, ChatOptions, ChatRequest, LlmProvider, ToolChoice};
use crate::store::memory::{KIND_EXPERIENCE, KIND_FACT, KIND_IDENTITY};
use crate::store::{Memory, Message, Store};

/// 整理器法条(人格中立、不与人对话,§5):立规则,不教学。
/// **铁律(2026-06-18 用户准则):宁缺毋滥、绝不强行提炼。默认输出 `[]`;只有对话里真出现了
/// 「值得长期记住、且现在还没记下」的硬事实/习惯才记 —— 没有就空,这是常态、不是失职。**
const SYSTEM: &str = "你是后台记忆整理器,不与任何人对话,只输出 JSON。\
  你的默认答案是空数组 []。只有当「最近对话」里**确实**出现了值得长期记住、且「已记得的事」里还没有的\
  硬信息时,才把它记下来。**没有就输出 []——这在绝大多数情况下就是正确答案,不是失职,绝不要为了凑数硬找。**\
  【只在同时满足以下全部时才记一条】\
  a) 是关于用户本人或这个家的**稳定事实或长期习惯**(身份/家人/忌口过敏/长期偏好/做事方式);\
  b) 对话里**明确说过或清楚显露**,不是你的推测、引申或脑补;\
  c) **以后大概率还用得上**,不是只对此刻有意义;\
  d) 「已记得的事」里没有等价表述。\
  【命中任一即丢,绝不记】一次性的闲聊/情绪/天气/当下安排;泛泛而谈、无具体可复用内容;\
  靠猜的/不确定的;能从常识推出的;客套话;对助手的临时指令。**只要拿不准,就不记。**\
  最多 5 条,第三人称简短陈述。输出一个 JSON 数组,每项形如 \
  {\"content\":\"...\",\"kind\":\"identity|experience|fact\",\"replaces\":\"可选\"}:\
  身份/安全/家人(名字、过敏、忌口)→ identity;做事习惯/偏好 → experience;其它长期事实 → fact。\
  【纠错替换】若「最近对话」里用户**明确纠正**了「已记得的事」里某条(『不是…是…』『改成…』『其实…』『现在…了』),\
  就在该项加 \"replaces\":\"<旧记忆里能唯一定位它的一小段原文>\",content 写纠正后的新说法。\
  **身份/安全/家人类(过敏、名字、忌口)绝不这样替换 —— 那种要本人亲口改。** 没有明确纠正就别加 replaces。\
  再说一遍:没有真正值得记的,就只输出 []。";

/// 一次最多提炼几条(与法条 ④ 一致,代码再兜一道)。
const MAX_ITEMS: usize = 5;
/// 单条内容上限(同 remember 的防撑爆)。
const CONTENT_MAX_CHARS: usize = 200;

/// 蒸馏出的一条候选(种类已归一到合法常量)。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DistilledItem {
    pub content: String,
    pub kind: &'static str,
    /// 纠错替换(§13.6 Phase 3 激进):非空 = 用 `content` 覆盖「旧记忆里含这片段的那条」(仅明确纠正时)。
    pub replaces: Option<String>,
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
            // 中立角色标签(§5 底座不嵌具名;提炼模型只需分清谁在说,不需要知道名字,
            // 也免得跟用户改名后的真名打架——顺手修掉原硬编的旧默认名「7274」)
            "assistant" => "助手",
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
            let replaces = v
                .get("replaces")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            Some(DistilledItem { content, kind, replaces })
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
    let mut req = build_request(&recent, &existing);
    // 提炼认全局反应模式 `llm.thinking`,**解析与回合循环完全一致**(engine/mod.rs:1461):
    // **缺省 → Medium**(2026-06-19 起后台提炼默认开思考)。依据 = eval A/B 实测:关思考时提炼
    // 几乎一律 `[]`、开思考 consolidate-learns 0/5→5/5 且 restraint 仍 5/5(双向判对)。后台任务
    // 不卡延迟、成本每 N 回合一次推理可忽略。用户显式设 `off` 仍可关。
    let thinking = match store.settings.get(None, "llm.thinking")?.as_deref() {
        Some("off") => crate::llm::Thinking::Off,
        Some("light") => crate::llm::Thinking::Light,
        Some("heavy") => crate::llm::Thinking::Heavy,
        _ => crate::llm::Thinking::Medium,
    };
    if thinking != crate::llm::Thinking::Off {
        req.options.thinking = Some(thinking);
    }
    let text = provider
        .chat(req)
        .await
        .map_err(|e| anyhow::anyhow!("记忆提炼 LLM 调用失败: {e:?}"))?;
    let mut added = 0usize;
    for item in parse(&text) {
        // 纠错替换(§13.6 Phase 3 激进,用户拍板「本期开 LLM 纠错」2026-06-23):明确纠正 → 覆盖旧记忆。
        // identity 不被替换(supersede 内置滤掉);没匹配到旧记忆则退化成普通新增(走下面去重)。
        if let Some(old) = item.replaces.as_deref() {
            if store.memory.supersede(user_id, old, item.kind, &item.content)? {
                added += 1;
                continue;
            }
        }
        if store.memory.has_similar(user_id, &item.content)? {
            continue; // 去重守门
        }
        store.memory.add_distilled(user_id, item.kind, &item.content)?;
        added += 1;
    }
    // 激进维护(§13.6 ②③):衰减 / 下沉 / 升层 / 合并近重复 / 硬清(确定性,身份类全程豁免 §13.4)。
    // 搭车提炼的后台节奏一并跑;尽力件,失败不影响已落库的提炼。
    if let Ok(rep) = store.memory.maintain(user_id, crate::store::now_ms()) {
        if rep.touched() {
            tracing::debug!(target: "larkwing::memory", user = user_id, ?rep, "记忆维护轮");
        }
    }
    // 未了的事·过期自清(★主动关怀 切片2·B):搭同一后台维护轮;open 且太久没动的静默了结,免无限
    // 累积(进前缀本就 list_open 限量,这里管 DB 长期清爽)。起步 30 天,真用可调(§13.7);尽力件。
    const TODO_STALE_MS: i64 = 30 * 86_400_000;
    let _ = store.todos.expire_stale(user_id, crate::store::now_ms(), TODO_STALE_MS);
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

    #[test]
    fn parse_reads_optional_replaces() {
        let items = parse(
            "[{\"content\":\"新事实\",\"kind\":\"fact\",\"replaces\":\"旧片段\"},\
             {\"content\":\"无纠错\",\"kind\":\"fact\"},{\"content\":\"空串\",\"kind\":\"fact\",\"replaces\":\"  \"}]",
        );
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].replaces.as_deref(), Some("旧片段"));
        assert_eq!(items[1].replaces, None, "缺字段 = None");
        assert_eq!(items[2].replaces, None, "空白 replaces 当没填");
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

    #[tokio::test]
    async fn run_applies_correction_supersede() {
        let store = store("supersede");
        let user = store.users.ensure_default_user().unwrap();
        let conv = store.chat.create_conversation(user.id, "companion").unwrap();
        store.chat.append_message(conv.id, "user", "我不喝美式了,改喝拿铁").unwrap();
        store.chat.append_message(conv.id, "assistant", "好的~").unwrap();
        let (old, _) = store.memory.add(user.id, KIND_FACT, "用户喜欢喝美式", "explicit").unwrap();

        // 提炼器发 replaces → 走纠错替换(覆盖旧记忆,不是新增)
        let json = "[{\"content\":\"用户改喝拿铁了\",\"kind\":\"fact\",\"replaces\":\"美式\"}]";
        let provider: Arc<dyn LlmProvider> =
            Arc::new(FakeLlm::scripted(vec![FakeTurn { text: json.into(), ..Default::default() }]));
        let added = run(&provider, &store, user.id, conv.id, 50).await.unwrap();
        assert_eq!(added, 1);
        let all = store.memory.list(user.id).unwrap();
        assert_eq!(all.len(), 1, "旧的被替换、不是新增");
        assert_eq!(all[0].content, "用户改喝拿铁了");
        assert_eq!(all[0].source, "correction");
        // 注:SQLite 删唯一行后插入会复用 rowid → 不能用 id 判旧条已删,判内容才可靠。
        assert!(all.iter().all(|m| m.content != old.content), "旧内容已被替换掉");
    }
}
