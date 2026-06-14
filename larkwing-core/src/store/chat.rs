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
];

#[derive(Debug, Clone, Serialize)]
pub struct Conversation {
    pub id: i64,
    pub user_id: i64,
    pub scene_id: String,
    pub title: String,
    pub created_at: i64,
    pub updated_at: i64,
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
}

/// 标题默认 = 首条用户消息截断(字符数,不花 LLM)。
const TITLE_MAX_CHARS: usize = 24;

#[derive(Clone)]
pub struct ChatRepo {
    db: Db,
}

impl ChatRepo {
    pub(super) fn new(db: Db) -> Self {
        Self { db }
    }

    pub fn create_conversation(&self, user: i64, scene_id: &str) -> Result<Conversation> {
        self.db.with(|c| {
            let now = now_ms();
            c.execute(
                "INSERT INTO conversations (user_id, scene_id, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?3)",
                rusqlite::params![user, scene_id, now],
            )?;
            Ok(Conversation {
                id: c.last_insert_rowid(),
                user_id: user,
                scene_id: scene_id.into(),
                title: String::new(),
                created_at: now,
                updated_at: now,
            })
        })
    }

    pub fn get_conversation(&self, id: i64) -> Result<Option<Conversation>> {
        self.db.with(|c| {
            let conv = c
                .query_row(
                    "SELECT id, user_id, scene_id, title, created_at, updated_at
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
                    "SELECT id, user_id, scene_id, title, created_at, updated_at
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

    pub fn list_conversations(&self, user: i64) -> Result<Vec<Conversation>> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT id, user_id, scene_id, title, created_at, updated_at
                 FROM conversations WHERE user_id = ?1 ORDER BY updated_at DESC",
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
                    let t: String = content.chars().take(TITLE_MAX_CHARS).collect();
                    tx.execute(
                        "UPDATE conversations SET title = ?2 WHERE id = ?1",
                        rusqlite::params![conv, t],
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
            })
        })
    }

    /// 显式定题。任务专属会话用:它只有 event/assistant 行,标题不会自动生成。
    pub fn set_title(&self, conv: i64, title: &str) -> Result<()> {
        self.db.with(|c| {
            c.execute(
                "UPDATE conversations SET title = ?2 WHERE id = ?1",
                rusqlite::params![conv, title],
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
}

fn row_to_conversation(r: &rusqlite::Row<'_>) -> rusqlite::Result<Conversation> {
    Ok(Conversation {
        id: r.get(0)?,
        user_id: r.get(1)?,
        scene_id: r.get(2)?,
        title: r.get(3)?,
        created_at: r.get(4)?,
        updated_at: r.get(5)?,
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
    })
}
