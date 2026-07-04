//! 会话 LLM 命名(方案 A,2026-07-02):新会话首条用户消息落库时 store 侧先给**截断占位**
//! (`store::chat::derive_title`,即时、零成本),这里后台用**最便宜档** provider(与记忆提炼
//! 同一条 cheap-model 路由,§13.6)非流式起一个短标题,`set_title_if` CAS 替换占位 ——
//! 用户已重命名就绝不覆盖。尽力件:失败/超时/没 provider = 保留占位,只记日志,绝不打扰主对话。
//!
//! 形状仿 consolidate:纯函数(`build_request`/`clean`,可单测)+ 一个编排(`run`,FakeLlm 可端到端测)。
//! 只喂首条用户消息的前 `INPUT_MAX_CHARS` 字(信息量最大;文档附件会把消息撑到几万字,别全喂)。
//! 不开思考:标题是浅任务,快 + 省(对比:提炼开思考是 eval 实证需要判断力,这里不需要)。

use std::sync::Arc;

use anyhow::Result;

use crate::llm::{ChatMessage, ChatOptions, ChatRequest, LlmProvider, ToolChoice};
use crate::store::chat::{derive_title, TITLE_MAX_CHARS};
use crate::store::Store;

/// 标题生成器法条(人格中立、不与人对话,§5):只立输出契约。
const SYSTEM: &str = "你是后台会话标题生成器,不与任何人对话。根据用户在一段新对话里的第一句话,\
  给这段对话起一个简短、具体的标题:不超过 12 个字(拉丁语系语言不超过 5 个词),用这句话本身的语言;\
  概括用户想做的事,不要照抄整句。只输出标题文字本身 —— 不要引号或书名号,不要句末标点,\
  不要解释,不要「标题:」之类前缀。";

/// 喂给模型的输入上限(字符):文档文字并进落库内容后首条消息可能几万字(§9),
/// 用户真正的诉求在开头,截前 400 字足够定题、不烧钱。send_message 取种子时引用同一常量(单源)。
pub(crate) const INPUT_MAX_CHARS: usize = 400;

/// 构造定题请求(纯函数):不带工具、不开思考,一条 user 消息进、一行标题出。
pub(crate) fn build_request(first_msg: &str) -> ChatRequest {
    let clipped: String = first_msg.chars().take(INPUT_MAX_CHARS).collect();
    ChatRequest {
        system: SYSTEM.into(),
        messages: vec![ChatMessage::User { content: format!("【用户开场】\n{clipped}"), parts: vec![] }],
        options: ChatOptions::default(),
        tools: vec![],
        tool_choice: ToolChoice::default(),
    }
}

/// 清洗模型输出成可用标题:取首个非空行,剥引号/围栏/「标题:」前缀,去尾部句读,
/// 按 `TITLE_MAX_CHARS` 封顶(与占位同一上限当护栏)。清不出东西 = None(保留占位)。
pub(crate) fn clean(raw: &str) -> Option<String> {
    let mut line = raw.lines().map(str::trim).find(|l| !l.is_empty())?;
    // 迭代剥到不动点:引号可能包在句读外(「…」。)也可能包在里面("…".),一遍剥不净
    loop {
        let stripped = line
            .trim_start_matches("标题:")
            .trim_start_matches("标题:")
            .trim_matches(|c: char| {
                matches!(c, '"' | '\'' | '“' | '”' | '‘' | '’' | '「' | '」' | '『' | '』' | '《' | '》' | '`')
            })
            .trim_end_matches(|c: char| {
                matches!(c, '。' | '.' | '!' | '！' | '?' | '？' | '…' | ',' | '，' | ';' | '；')
            })
            .trim();
        if stripped == line {
            break;
        }
        line = stripped;
    }
    let title: String = line.chars().take(TITLE_MAX_CHARS).collect();
    (!title.is_empty()).then_some(title)
}

/// 跑一次定题:调 LLM → 清洗 → CAS 替换占位(占位由 `seed` 重算,与 store 侧同一函数 ——
/// `seed` 是消息前缀截断,而占位只由首行前 `TITLE_MAX_CHARS` 字决定,故两边必然一致)。
/// 返回 Some(新标题) = 真写了;None = 模型没给出可用标题 / 用户已改名(不覆盖)。
pub(crate) async fn run(
    provider: &Arc<dyn LlmProvider>,
    store: &Store,
    conv_id: i64,
    seed: &str,
) -> Result<Option<String>> {
    let text = provider
        .chat(build_request(seed))
        .await
        .map_err(|e| anyhow::anyhow!("会话定题 LLM 调用失败: {e:?}"))?;
    let Some(title) = clean(&text) else { return Ok(None) };
    let expected = derive_title(seed);
    Ok(store.chat.set_title_if(conv_id, &expected, &title)?.then_some(title))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::fake::{FakeLlm, FakeTurn};

    fn store(tag: &str) -> (Store, i64) {
        let dir = std::env::temp_dir().join(format!("lw-title-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _ = std::fs::remove_file(dir.join("t.db"));
        let store = Store::open(&dir.join("t.db")).unwrap();
        let me = store.users.ensure_default_user().unwrap();
        (store, me.id)
    }

    fn scripted(text: &str) -> Arc<dyn LlmProvider> {
        Arc::new(FakeLlm::scripted(vec![FakeTurn { text: text.into(), ..Default::default() }]))
    }

    #[test]
    fn clean_strips_wrapping_and_clamps() {
        assert_eq!(clean("「整理下载文件夹」").as_deref(), Some("整理下载文件夹"));
        assert_eq!(clean("标题:看电影安排。").as_deref(), Some("看电影安排"));
        assert_eq!(clean("\n\n  周末出游计划!  \n第二行").as_deref(), Some("周末出游计划"));
        assert_eq!(clean("\"Fix the build\".").as_deref(), Some("Fix the build"));
        let long = "长".repeat(40);
        assert_eq!(clean(&long).unwrap().chars().count(), TITLE_MAX_CHARS);
        assert_eq!(clean("   \n  "), None, "全空白 = 清不出标题");
        assert_eq!(clean("「。」"), None, "剥完只剩标点 = None");
    }

    #[test]
    fn build_request_clips_doc_heavy_input() {
        let msg = format!("帮我总结这份文档{}", "字".repeat(1000));
        let req = build_request(&msg);
        let ChatMessage::User { content, .. } = &req.messages[0] else { panic!("应是 user 消息") };
        assert!(content.chars().count() < INPUT_MAX_CHARS + 20, "附件长文不全喂");
        assert!(req.tools.is_empty() && req.options.thinking.is_none(), "不带工具、不开思考");
    }

    #[tokio::test]
    async fn run_replaces_placeholder_via_cas() {
        let (store, user) = store("cas-hit");
        let conv = store.chat.create_conversation(user, "companion").unwrap();
        store.chat.append_message(conv.id, "user", "帮我把下载文件夹里的照片按月份整理好").unwrap();
        let seed = "帮我把下载文件夹里的照片按月份整理好";
        let got = run(&scripted("「整理照片」"), &store, conv.id, seed).await.unwrap();
        assert_eq!(got.as_deref(), Some("整理照片"));
        assert_eq!(store.chat.get_conversation(conv.id).unwrap().unwrap().title, "整理照片");
    }

    #[tokio::test]
    async fn run_never_clobbers_user_rename() {
        let (store, user) = store("cas-miss");
        let conv = store.chat.create_conversation(user, "companion").unwrap();
        store.chat.append_message(conv.id, "user", "帮我把下载文件夹里的照片按月份整理好").unwrap();
        store.chat.set_title(conv.id, "我自己起的名").unwrap(); // 用户抢先重命名
        let got =
            run(&scripted("整理照片"), &store, conv.id, "帮我把下载文件夹里的照片按月份整理好").await.unwrap();
        assert_eq!(got, None);
        assert_eq!(store.chat.get_conversation(conv.id).unwrap().unwrap().title, "我自己起的名");
    }

    #[tokio::test]
    async fn run_keeps_placeholder_on_unusable_output() {
        let (store, user) = store("garbage");
        let conv = store.chat.create_conversation(user, "companion").unwrap();
        store.chat.append_message(conv.id, "user", "在吗?想问个事").unwrap();
        let got = run(&scripted("  \n "), &store, conv.id, "在吗?想问个事").await.unwrap();
        assert_eq!(got, None);
        assert_eq!(store.chat.get_conversation(conv.id).unwrap().unwrap().title, "在吗");
    }
}
