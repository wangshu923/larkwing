use anyhow::Result;
use rusqlite::OptionalExtension;
use serde::Serialize;

use super::db::{m, now_ms, Db, Migration};

pub const MIGRATIONS: &[Migration] = &[
    m(
        "0003_chat_init",
        "CREATE TABLE conversations (
        id         INTEGER PRIMARY KEY,
        user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
        scene_id   TEXT NOT NULL DEFAULT 'companion',
        title      TEXT NOT NULL DEFAULT '',
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    );
    CREATE TABLE messages (
        id              INTEGER PRIMARY KEY,
        conversation_id INTEGER NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
        role            TEXT NOT NULL,
        content         TEXT NOT NULL,
        created_at      INTEGER NOT NULL
    );
    CREATE INDEX idx_messages_conv ON messages (conversation_id, id);",
    ),
    // 工具运行时(PLAN §8):assistant 行存 tool_calls+reasoning,'tool' 行存 call_id/name/status。
    // JSON 形状是 engine 的私有词汇,store 只存 TEXT。
    m("0005_messages_payload", "ALTER TABLE messages ADD COLUMN payload TEXT;"),
    // 会话渠道(渠道=数据,宪法 §5/§9):语音/界面/系统/未来远程渠道,会话列表按此渲染小图标。
    m(
        "0006_conversations_channel",
        "ALTER TABLE conversations ADD COLUMN channel TEXT NOT NULL DEFAULT 'ui';",
    ),
    // 会话钉住(用户右键「钉住」):没聊完的会话排到最前 + 列表挂 📌。0/1 布尔,列表排序 pinned DESC 在前。
    m(
        "0007_conversations_pinned",
        "ALTER TABLE conversations ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0;",
    ),
];

/// 会话渠道(渠道=数据):列保持开放 TEXT 让未来渠道当数据加,已知值给常量防拼错。
/// `ui` = 默认/列表不渲染标记的基线;非界面渠道才标。与消息级 `via`(念不念)正交。
pub const CHANNEL_UI: &str = "ui";
pub const CHANNEL_VOICE: &str = "voice";
pub const CHANNEL_SYSTEM: &str = "system";

#[derive(Debug, Clone, Serialize)]
pub struct Conversation {
    pub id: i64,
    pub user_id: i64,
    pub scene_id: String,
    pub title: String,
    /// 渠道(会话级,持久):ui/voice/system/…。列表按此渲染小图标。见 CHANNEL_* 常量。
    pub channel: String,
    /// 钉住:用户右键标记的「没聊完」会话,列表排最前 + 挂 📌。
    pub pinned: bool,
    pub created_at: i64,
    pub updated_at: i64,
    /// 发起人显示名(engine 富化,非 DB 列):渠道指认的家人 / 非主人发起者;
    /// 主人自己的会话 = None(是「你」,不标)。IPC 增量字段。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_name: Option<String>,
}

/// 持久形态(≠ llm::ChatMessage 调用形态)。role 开放为 TEXT:user/assistant/tool。
#[derive(Debug, Clone, Serialize)]
pub struct Message {
    pub id: i64,
    pub conversation_id: i64,
    pub role: String,
    pub content: String,
    pub created_at: i64,
    /// 工具轮附加数据(JSON,engine 私有词汇);普通消息为 None。IPC 上是增量字段。
    pub payload: Option<String>,
    /// 说话人显示名(engine 富化,非 DB 列):user 行的说话人若非会话归属者(家人插话 /
    /// 声纹 / 渠道归人)则填其名;归属者自己说的 = None(是「我」,不标)。IPC 增量字段。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker_name: Option<String>,
    /// 触发来源(engine 富化,非 DB 列):assistant 行若由定时任务 / 提醒自动触发则为
    /// Some("reminder"),普通对话回复 = None。前端据此标「⏰ 提醒」。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<String>,
}

/// 跨会话搜索命中:带会话标题 / 渠道供列表展示;`snippet` 是截断的展示标签(非数据,§6.5)。
/// 字段保持 snake_case(与 Message/Conversation 一致,前端接口同形)。
#[derive(Debug, Clone, Serialize)]
pub struct SearchHit {
    pub conversation_id: i64,
    pub conversation_title: String,
    pub channel: String,
    pub role: String,
    pub snippet: String,
    pub created_at: i64,
}

/// 标题默认 = 首条用户消息截断(字符数,不花 LLM)。
/// LLM 命名(engine/title.rs)以此为占位:后台生成好再 `set_title_if` 静默替换。
pub(crate) const TITLE_MAX_CHARS: usize = 24;

/// 从消息内容派生占位标题:取首行、截到第一个句末标点(别切半句),再按字符数封顶。
/// 纯派生展示标签(§6.2 豁免),不是数据;engine 的 CAS 替换靠它可重算出同一占位。
pub(crate) fn derive_title(content: &str) -> String {
    let first_line = content.trim().lines().next().unwrap_or("").trim();
    let cut = first_line
        .char_indices()
        .find(|(_, c)| matches!(c, '。' | '！' | '？' | '!' | '?'))
        .map(|(i, _)| first_line[..i].trim_end())
        .unwrap_or(first_line);
    // 首字符就是标点(cut 为空)→ 退回整行,别产出空标题
    let base = if cut.is_empty() { first_line } else { cut };
    base.chars().take(TITLE_MAX_CHARS).collect()
}

#[derive(Clone)]
pub struct ChatRepo {
    db: Db,
}

impl ChatRepo {
    pub(super) fn new(db: Db) -> Self {
        Self { db }
    }

    /// 默认渠道(界面)的便捷壳;非界面渠道走 create_conversation_full(仿 append_message/_full)。
    pub fn create_conversation(&self, user: i64, scene_id: &str) -> Result<Conversation> {
        self.create_conversation_full(user, scene_id, CHANNEL_UI)
    }

    pub fn create_conversation_full(
        &self,
        user: i64,
        scene_id: &str,
        channel: &str,
    ) -> Result<Conversation> {
        self.db.with(|c| {
            let now = now_ms();
            c.execute(
                "INSERT INTO conversations (user_id, scene_id, channel, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?4)",
                rusqlite::params![user, scene_id, channel, now],
            )?;
            Ok(Conversation {
                id: c.last_insert_rowid(),
                user_id: user,
                scene_id: scene_id.into(),
                title: String::new(),
                channel: channel.into(),
                pinned: false,
                created_at: now,
                updated_at: now,
                owner_name: None,
            })
        })
    }

    pub fn get_conversation(&self, id: i64) -> Result<Option<Conversation>> {
        self.db.with(|c| {
            let conv = c
                .query_row(
                    "SELECT id, user_id, scene_id, title, channel, pinned, created_at, updated_at
                     FROM conversations WHERE id = ?1",
                    [id],
                    row_to_conversation,
                )
                .optional()?;
            Ok(conv)
        })
    }

    pub fn latest_conversation(&self, user: i64) -> Result<Option<Conversation>> {
        self.db.with(|c| {
            let conv = c
                .query_row(
                    "SELECT id, user_id, scene_id, title, channel, pinned, created_at, updated_at
                     FROM conversations WHERE user_id = ?1
                     ORDER BY updated_at DESC LIMIT 1",
                    [user],
                    row_to_conversation,
                )
                .optional()?;
            Ok(conv)
        })
    }

    /// 最近一句"说出口"的 assistant 文本(悬浮窗待机轮播):跳过纯工具轮空串与 __IGNORE__。
    pub fn latest_assistant_line(&self, conv: i64) -> Result<Option<String>> {
        self.db.with(|c| {
            let line: Option<String> = c
                .query_row(
                    "SELECT content FROM messages
                     WHERE conversation_id = ?1 AND role = 'assistant'
                       AND content != '' AND content != '__IGNORE__'
                     ORDER BY id DESC LIMIT 1",
                    [conv],
                    |r| r.get(0),
                )
                .optional()?;
            Ok(line)
        })
    }

    /// 最近一条 event 行原文(自启回合落的任务语境)。提醒推回手机的 Failed 保底:
    /// 回合没跑成也把提醒本体送到人手上(§3.5 不静默)。
    pub fn latest_event_line(&self, conv: i64) -> Result<Option<String>> {
        self.db.with(|c| {
            let line: Option<String> = c
                .query_row(
                    "SELECT content FROM messages
                     WHERE conversation_id = ?1 AND role = 'event' AND content != ''
                     ORDER BY id DESC LIMIT 1",
                    [conv],
                    |r| r.get(0),
                )
                .optional()?;
            Ok(line)
        })
    }

    pub fn list_conversations(&self, user: i64) -> Result<Vec<Conversation>> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT id, user_id, scene_id, title, channel, pinned, created_at, updated_at
                 FROM conversations WHERE user_id = ?1
                 ORDER BY pinned DESC, updated_at DESC",
            )?;
            let list = stmt
                .query_map([user], row_to_conversation)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(list)
        })
    }

    /// 级联删消息(外键 ON DELETE CASCADE)。调用方(engine)负责先取消在飞回合。
    pub fn delete_conversation(&self, id: i64) -> Result<()> {
        self.db.with(|c| {
            c.execute("DELETE FROM conversations WHERE id = ?1", [id])?;
            Ok(())
        })
    }

    /// 事务:插消息 + 推会话 updated_at + 首条用户消息兼职标题。
    pub fn append_message(&self, conv: i64, role: &str, content: &str) -> Result<Message> {
        self.append_message_full(conv, role, content, None)
    }

    /// 带 payload 的完整形(工具轮用);append_message 是 None 的便捷壳。
    pub fn append_message_full(
        &self,
        conv: i64,
        role: &str,
        content: &str,
        payload: Option<&str>,
    ) -> Result<Message> {
        self.db.tx(|tx| {
            let now = now_ms();
            tx.execute(
                "INSERT INTO messages (conversation_id, role, content, created_at, payload)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![conv, role, content, now, payload],
            )?;
            let id = tx.last_insert_rowid();
            tx.execute(
                "UPDATE conversations SET updated_at = ?2 WHERE id = ?1",
                rusqlite::params![conv, now],
            )?;
            if role == "user" {
                let title: String =
                    tx.query_row("SELECT title FROM conversations WHERE id = ?1", [conv], |r| {
                        r.get(0)
                    })?;
                if title.is_empty() {
                    tx.execute(
                        "UPDATE conversations SET title = ?2 WHERE id = ?1",
                        rusqlite::params![conv, derive_title(content)],
                    )?;
                }
            }
            Ok(Message {
                id,
                conversation_id: conv,
                role: role.into(),
                content: content.into(),
                created_at: now,
                payload: payload.map(Into::into),
                speaker_name: None,
                trigger: None,
            })
        })
    }

    /// 显式定题。任务专属会话用:它只有 event/assistant 行,标题不会自动生成。
    /// 用户右键「重命名」也走这条(无条件覆盖)。
    pub fn set_title(&self, conv: i64, title: &str) -> Result<()> {
        self.db.with(|c| {
            c.execute(
                "UPDATE conversations SET title = ?2 WHERE id = ?1",
                rusqlite::params![conv, title],
            )?;
            Ok(())
        })
    }

    /// 条件定题(CAS):仅当现值仍是 `expected` 才写(后台 LLM 命名用 —— 用户已重命名就绝不覆盖)。
    /// 返回是否真写了。不动 updated_at(改标题不算"有新活动",列表不重排)。
    pub fn set_title_if(&self, conv: i64, expected: &str, title: &str) -> Result<bool> {
        self.db.with(|c| {
            let n = c.execute(
                "UPDATE conversations SET title = ?2 WHERE id = ?1 AND title = ?3",
                rusqlite::params![conv, title, expected],
            )?;
            Ok(n > 0)
        })
    }

    /// 钉住 / 取消钉住(用户右键)。只改标记,不动 updated_at(钉住不算"有新活动")。
    pub fn set_pinned(&self, conv: i64, pinned: bool) -> Result<()> {
        self.db.with(|c| {
            c.execute(
                "UPDATE conversations SET pinned = ?2 WHERE id = ?1",
                rusqlite::params![conv, pinned],
            )?;
            Ok(())
        })
    }

    pub fn count_messages(&self, conv: i64) -> Result<i64> {
        self.db.with(|c| {
            let n = c.query_row(
                "SELECT COUNT(*) FROM messages WHERE conversation_id = ?1",
                [conv],
                |r| r.get(0),
            )?;
            Ok(n)
        })
    }

    /// 按 id 升序取一页(供 ContextBuilder 的锚定窗口)。
    pub fn messages_page(&self, conv: i64, offset: i64, limit: i64) -> Result<Vec<Message>> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT id, conversation_id, role, content, created_at, payload
                 FROM messages WHERE conversation_id = ?1
                 ORDER BY id ASC LIMIT ?2 OFFSET ?3",
            )?;
            let list = stmt
                .query_map(rusqlite::params![conv, limit, offset], row_to_message)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(list)
        })
    }

    /// 最后 limit 条,升序返回(供 UI 首屏)。
    pub fn recent_messages(&self, conv: i64, limit: i64) -> Result<Vec<Message>> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT id, conversation_id, role, content, created_at, payload FROM (
                    SELECT id, conversation_id, role, content, created_at, payload
                    FROM messages WHERE conversation_id = ?1
                    ORDER BY id DESC LIMIT ?2
                 ) ORDER BY id ASC",
            )?;
            let list = stmt
                .query_map(rusqlite::params![conv, limit], row_to_message)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(list)
        })
    }

    /// 跨会话按时间区间取用户可见对话(`[from_ms, to_ms)`,升序,封顶 limit):家庭日记蒸馏取料用。
    /// 只收 user/assistant 且内容非空(tool/event 内部行、静默回合不进日记)。
    pub fn messages_between(&self, from_ms: i64, to_ms: i64, limit: i64) -> Result<Vec<Message>> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT id, conversation_id, role, content, created_at, payload
                 FROM messages
                 WHERE created_at >= ?1 AND created_at < ?2
                   AND role IN ('user','assistant') AND content <> ''
                 ORDER BY created_at ASC, id ASC LIMIT ?3",
            )?;
            let list = stmt
                .query_map(rusqlite::params![from_ms, to_ms, limit], row_to_message)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(list)
        })
    }

    /// 跨会话搜索(当前用户):`messages.content` 子串匹配,排除 tool/event 内部行
    /// (`role IN (user, assistant)`)。substring `LIKE` 即可(同 recall 立场,历史量小够用;
    /// 真要语义查找走 §13.9 检索核心)。最近命中在前。
    pub fn search_messages(&self, user: i64, query: &str, limit: i64) -> Result<Vec<SearchHit>> {
        let q = query.trim();
        if q.is_empty() {
            return Ok(vec![]);
        }
        // 转义 LIKE 通配符,让用户查的 % _ \ 当字面量(配 SQL 里 ESCAPE '\')。
        let pattern = format!("%{}%", super::like_escape(q));
        let ql = q.to_lowercase();
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT m.conversation_id, c.title, c.channel, m.role, m.content, m.created_at
                 FROM messages m JOIN conversations c ON c.id = m.conversation_id
                 WHERE c.user_id = ?1
                   AND m.role IN ('user', 'assistant')
                   AND m.content LIKE ?2 ESCAPE '\\'
                 ORDER BY m.id DESC LIMIT ?3",
            )?;
            let list = stmt
                .query_map(rusqlite::params![user, pattern, limit], |r| {
                    let content: String = r.get(4)?;
                    Ok(SearchHit {
                        conversation_id: r.get(0)?,
                        conversation_title: r.get(1)?,
                        channel: r.get(2)?,
                        role: r.get(3)?,
                        snippet: snippet_around(&content, &ql, 36),
                        created_at: r.get(5)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(list)
        })
    }
}

/// 命中片段:短则原样;长则取命中词附近一窗(字符级,避免切碎多字节 UTF-8)。展示标签,非数据。
fn snippet_around(content: &str, query_lower: &str, radius: usize) -> String {
    let chars: Vec<char> = content.chars().collect();
    let qn = query_lower.chars().count();
    if chars.len() <= radius * 2 + qn {
        return content.to_string();
    }
    let lower = content.to_lowercase();
    let hit_char =
        lower.find(query_lower).map(|bp| lower[..bp].chars().count()).unwrap_or(0);
    let start = hit_char.saturating_sub(radius);
    let end = (hit_char + qn + radius).min(chars.len());
    let mut s = String::new();
    if start > 0 {
        s.push('…');
    }
    s.extend(&chars[start..end]);
    if end < chars.len() {
        s.push('…');
    }
    s
}

fn row_to_conversation(r: &rusqlite::Row<'_>) -> rusqlite::Result<Conversation> {
    Ok(Conversation {
        id: r.get(0)?,
        user_id: r.get(1)?,
        scene_id: r.get(2)?,
        title: r.get(3)?,
        channel: r.get(4)?,
        pinned: r.get(5)?,
        created_at: r.get(6)?,
        updated_at: r.get(7)?,
        owner_name: None,
    })
}

fn row_to_message(r: &rusqlite::Row<'_>) -> rusqlite::Result<Message> {
    Ok(Message {
        id: r.get(0)?,
        conversation_id: r.get(1)?,
        role: r.get(2)?,
        content: r.get(3)?,
        created_at: r.get(4)?,
        payload: r.get(5)?,
        speaker_name: None,
        trigger: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    fn store(tag: &str) -> (Store, i64) {
        let dir = std::env::temp_dir().join(format!("lw-chat-test-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _ = std::fs::remove_file(dir.join("t.db"));
        let store = Store::open(&dir.join("t.db")).unwrap();
        let me = store.users.ensure_default_user().unwrap();
        (store, me.id)
    }

    #[test]
    fn derive_title_cuts_at_sentence_end_not_midway() {
        assert_eq!(derive_title("帮我放个电影。最好是科幻的"), "帮我放个电影");
        assert_eq!(derive_title("明天早上八点,提醒我交水电费"), "明天早上八点,提醒我交水电费"); // 逗号不切
        assert_eq!(derive_title("在吗?想问个事"), "在吗");
        assert_eq!(derive_title("  第一行\n第二行"), "第一行");
    }

    #[test]
    fn derive_title_clamps_and_survives_edge_cases() {
        let long: String = "长".repeat(40);
        assert_eq!(derive_title(&long).chars().count(), TITLE_MAX_CHARS);
        assert_eq!(derive_title("。。。"), "。。。"); // 首字符即标点 → 退回整行,不出空标题
        assert_eq!(derive_title(""), "");
    }

    #[test]
    fn first_user_message_sets_smart_placeholder() {
        let (store, user) = store("placeholder");
        let conv = store.chat.create_conversation(user, "companion").unwrap();
        store.chat.append_message(conv.id, "user", "放首儿歌吧!孩子想听").unwrap();
        let got = store.chat.get_conversation(conv.id).unwrap().unwrap();
        assert_eq!(got.title, "放首儿歌吧");
        // 第二条不再改题
        store.chat.append_message(conv.id, "user", "换一首。").unwrap();
        assert_eq!(store.chat.get_conversation(conv.id).unwrap().unwrap().title, "放首儿歌吧");
    }

    #[test]
    fn set_title_if_is_compare_and_swap() {
        let (store, user) = store("cas");
        let conv = store.chat.create_conversation(user, "companion").unwrap();
        store.chat.append_message(conv.id, "user", "帮我整理下载文件夹").unwrap();
        let placeholder = derive_title("帮我整理下载文件夹");
        // 占位仍在 → 替换成功
        assert!(store.chat.set_title_if(conv.id, &placeholder, "整理下载文件夹").unwrap());
        // 占位已不在(等价于用户已重命名)→ 绝不覆盖
        assert!(!store.chat.set_title_if(conv.id, &placeholder, "别的").unwrap());
        assert_eq!(store.chat.get_conversation(conv.id).unwrap().unwrap().title, "整理下载文件夹");
    }
}
