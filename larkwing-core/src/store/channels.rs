//! 远程渠道会话映射(PLAN 远程渠道):平台 chat ↔ Larkwing 会话的持久绑定。
//! 一个 (channel, ext_id) → 一个 conv_id;回访同一 chat 续接同一会话(复用 send_message 的历史回放)。
//! 与「渠道 = 数据」一致:channel 是开放 TEXT,加渠道不改本域。
//! 渠道归人(多用户第一步):每个 chat 可指认给一位家人(`user_id`,NULL = 会话归属者),
//! 入站回合据此带 `speaker_user`(记忆/提醒归 TA);`label` 存平台昵称,只为家人页认得出这是谁的对话。

use anyhow::Result;
use rusqlite::OptionalExtension;
use serde::Serialize;

use super::db::{m, now_ms, Db, Migration};

pub const MIGRATIONS: &[Migration] = &[
    m(
        "0013_channels_init",
        "CREATE TABLE channel_threads (
        id         INTEGER PRIMARY KEY,
        channel    TEXT NOT NULL,
        ext_id     TEXT NOT NULL,
        conv_id    INTEGER NOT NULL,
        created_at INTEGER NOT NULL,
        UNIQUE (channel, ext_id)
    );",
    ),
    m(
        "0017_channel_threads_user",
        "ALTER TABLE channel_threads ADD COLUMN user_id INTEGER;
         ALTER TABLE channel_threads ADD COLUMN label TEXT;",
    ),
    // 提醒推回手机(A1):平台侧主动推送要的收件地址。TG 用 ext_id(= chat_id)就够,
    // 钉钉 sessionWebhook 有时效 → 单聊存 senderStaffId 走主动推送 API;群聊不存(不推)。
    m(
        "0018_channel_threads_push_id",
        "ALTER TABLE channel_threads ADD COLUMN push_id TEXT;",
    ),
];

/// 一条渠道会话映射(家人页「谁的对话」列表行;serde 直过 IPC)。
#[derive(Debug, Clone, Serialize)]
pub struct ChannelThread {
    pub id: i64,
    pub channel: String,
    pub ext_id: String,
    pub conv_id: i64,
    /// 指认给的家人(NULL = 未指认 → 按会话归属者,零行为变化)。
    pub user_id: Option<i64>,
    /// 平台昵称(TG first_name / 钉钉 senderNick),入站时顺手记下,给绑定 UI 认脸。
    pub label: Option<String>,
    /// 平台侧主动推送的收件地址(提醒推回手机):钉钉单聊 = senderStaffId;
    /// TG 不需要(ext_id 即 chat_id);NULL = 该对话推不了(钉钉群聊等)。
    pub push_id: Option<String>,
}

#[derive(Clone)]
pub struct ChannelRepo {
    db: Db,
}

impl ChannelRepo {
    pub(super) fn new(db: Db) -> Self {
        Self { db }
    }

    /// 该平台 chat 已绑定的会话 id(无则 None → 调用方建会话再 bind)。
    pub fn conv_for(&self, channel: &str, ext_id: &str) -> Result<Option<i64>> {
        Ok(self.thread_for(channel, ext_id)?.map(|t| t.conv_id))
    }

    /// 整行映射(渠道入站用:conv_id + 指认的家人一次拿到)。
    pub fn thread_for(&self, channel: &str, ext_id: &str) -> Result<Option<ChannelThread>> {
        self.db.with(|c| {
            let t = c
                .query_row(
                    "SELECT id, channel, ext_id, conv_id, user_id, label, push_id
                     FROM channel_threads WHERE channel = ?1 AND ext_id = ?2",
                    rusqlite::params![channel, ext_id],
                    row_to_thread,
                )
                .optional()?;
            Ok(t)
        })
    }

    /// 反查:这个会话是不是渠道映射会话(提醒推回手机用;桌面会话 → None)。
    /// 同 conv 理论上只一条映射(bind 是 chat→conv 覆盖式);取最新建的那条兜底。
    pub fn thread_by_conv(&self, conv_id: i64) -> Result<Option<ChannelThread>> {
        self.db.with(|c| {
            let t = c
                .query_row(
                    "SELECT id, channel, ext_id, conv_id, user_id, label, push_id
                     FROM channel_threads WHERE conv_id = ?1
                     ORDER BY created_at DESC LIMIT 1",
                    [conv_id],
                    row_to_thread,
                )
                .optional()?;
            Ok(t)
        })
    }

    /// 绑定平台 chat → 会话(幂等:同 (channel, ext_id) 覆盖到最新 conv_id)。
    pub fn bind(&self, channel: &str, ext_id: &str, conv_id: i64) -> Result<()> {
        self.db.with(|c| {
            c.execute(
                "INSERT INTO channel_threads (channel, ext_id, conv_id, created_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(channel, ext_id) DO UPDATE SET conv_id = excluded.conv_id",
                rusqlite::params![channel, ext_id, conv_id, now_ms()],
            )?;
            Ok(())
        })
    }

    /// 全部映射(家人页「远程对话」区;最近建的在前)。
    pub fn list(&self) -> Result<Vec<ChannelThread>> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT id, channel, ext_id, conv_id, user_id, label, push_id
                 FROM channel_threads ORDER BY created_at DESC",
            )?;
            let rows =
                stmt.query_map([], row_to_thread)?.collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// 指认这条对话是谁在用(None = 取消指认,回到会话归属者)。
    pub fn bind_user(&self, thread_id: i64, user_id: Option<i64>) -> Result<()> {
        self.db.with(|c| {
            c.execute(
                "UPDATE channel_threads SET user_id = ?2 WHERE id = ?1",
                rusqlite::params![thread_id, user_id],
            )?;
            Ok(())
        })
    }

    /// 记下主动推送收件地址(钉钉单聊 senderStaffId;入站顺手写,空白不写)。
    pub fn set_push_id(&self, channel: &str, ext_id: &str, push_id: &str) -> Result<()> {
        let push_id = push_id.trim();
        if push_id.is_empty() {
            return Ok(());
        }
        self.db.with(|c| {
            c.execute(
                "UPDATE channel_threads SET push_id = ?3
                 WHERE channel = ?1 AND ext_id = ?2 AND (push_id IS NULL OR push_id <> ?3)",
                rusqlite::params![channel, ext_id, push_id],
            )?;
            Ok(())
        })
    }

    /// 记下平台昵称(入站顺手写,空白不写;给家人页认脸,不参与任何逻辑)。
    pub fn set_label(&self, channel: &str, ext_id: &str, label: &str) -> Result<()> {
        let label = label.trim();
        if label.is_empty() {
            return Ok(());
        }
        self.db.with(|c| {
            c.execute(
                "UPDATE channel_threads SET label = ?3
                 WHERE channel = ?1 AND ext_id = ?2 AND (label IS NULL OR label <> ?3)",
                rusqlite::params![channel, ext_id, label],
            )?;
            Ok(())
        })
    }

    /// 某家人被删:清掉所有指认(回落会话归属者,不删映射本身)。
    pub fn unbind_user(&self, user_id: i64) -> Result<()> {
        self.db.with(|c| {
            c.execute(
                "UPDATE channel_threads SET user_id = NULL WHERE user_id = ?1",
                rusqlite::params![user_id],
            )?;
            Ok(())
        })
    }
}

fn row_to_thread(r: &rusqlite::Row<'_>) -> rusqlite::Result<ChannelThread> {
    Ok(ChannelThread {
        id: r.get(0)?,
        channel: r.get(1)?,
        ext_id: r.get(2)?,
        conv_id: r.get(3)?,
        user_id: r.get(4)?,
        label: r.get(5)?,
        push_id: r.get(6)?,
    })
}

#[cfg(test)]
mod tests {
    use crate::store::Store;

    fn store(tag: &str) -> Store {
        let p = std::env::temp_dir().join(format!("lw-chan-test-{}-{tag}.db", std::process::id()));
        let _ = std::fs::remove_file(&p);
        Store::open(&p).unwrap()
    }

    #[test]
    fn bind_then_lookup_roundtrip() {
        let s = store("roundtrip");
        assert_eq!(s.channels.conv_for("telegram", "12345").unwrap(), None);
        s.channels.bind("telegram", "12345", 7).unwrap();
        assert_eq!(s.channels.conv_for("telegram", "12345").unwrap(), Some(7));
    }

    #[test]
    fn rebind_overwrites_and_channels_are_isolated() {
        let s = store("rebind");
        s.channels.bind("telegram", "100", 1).unwrap();
        s.channels.bind("telegram", "100", 2).unwrap(); // 同 chat 改绑(会话被删后重建)
        assert_eq!(s.channels.conv_for("telegram", "100").unwrap(), Some(2));
        // 同 ext_id 不同渠道互不串
        s.channels.bind("dingtalk", "100", 9).unwrap();
        assert_eq!(s.channels.conv_for("telegram", "100").unwrap(), Some(2));
        assert_eq!(s.channels.conv_for("dingtalk", "100").unwrap(), Some(9));
    }

    #[test]
    fn bind_user_and_label_roundtrip() {
        let s = store("binduser");
        s.channels.bind("telegram", "555", 3).unwrap();
        let t = s.channels.thread_for("telegram", "555").unwrap().unwrap();
        assert_eq!((t.user_id, t.label), (None, None), "新映射无指认无昵称");

        // 指认给家人 + 记昵称
        s.channels.bind_user(t.id, Some(42)).unwrap();
        s.channels.set_label("telegram", "555", " 豆豆 ").unwrap();
        let t = s.channels.thread_for("telegram", "555").unwrap().unwrap();
        assert_eq!(t.user_id, Some(42));
        assert_eq!(t.label.as_deref(), Some("豆豆"), "昵称去空白落库");

        // 空白昵称不覆盖;取消指认回 NULL
        s.channels.set_label("telegram", "555", "   ").unwrap();
        s.channels.bind_user(t.id, None).unwrap();
        let t = s.channels.thread_for("telegram", "555").unwrap().unwrap();
        assert_eq!(t.label.as_deref(), Some("豆豆"));
        assert_eq!(t.user_id, None);

        // rebind(会话重建)保留指认与昵称(UPDATE 不动新列)
        s.channels.bind_user(t.id, Some(42)).unwrap();
        s.channels.bind("telegram", "555", 8).unwrap();
        let t = s.channels.thread_for("telegram", "555").unwrap().unwrap();
        assert_eq!((t.conv_id, t.user_id), (8, Some(42)));
    }

    #[test]
    fn push_id_and_conv_reverse_lookup() {
        let s = store("pushid");
        assert!(s.channels.thread_by_conv(7).unwrap().is_none(), "桌面会话无映射");
        s.channels.bind("dingtalk", "cidAAA", 7).unwrap();

        // 反查:conv → 映射行
        let t = s.channels.thread_by_conv(7).unwrap().unwrap();
        assert_eq!((t.channel.as_str(), t.ext_id.as_str()), ("dingtalk", "cidAAA"));
        assert_eq!(t.push_id, None, "新映射无推送地址");

        // 推送地址:去空白落库、空白不覆盖;rebind 保留(UPDATE 不动该列)
        s.channels.set_push_id("dingtalk", "cidAAA", " staff9 ").unwrap();
        s.channels.set_push_id("dingtalk", "cidAAA", "   ").unwrap();
        s.channels.bind("dingtalk", "cidAAA", 9).unwrap();
        let t = s.channels.thread_by_conv(9).unwrap().unwrap();
        assert_eq!(t.push_id.as_deref(), Some("staff9"));
    }

    #[test]
    fn unbind_user_clears_all_assignments() {
        let s = store("unbind");
        s.channels.bind("telegram", "a", 1).unwrap();
        s.channels.bind("dingtalk", "b", 2).unwrap();
        let ta = s.channels.thread_for("telegram", "a").unwrap().unwrap();
        let tb = s.channels.thread_for("dingtalk", "b").unwrap().unwrap();
        s.channels.bind_user(ta.id, Some(5)).unwrap();
        s.channels.bind_user(tb.id, Some(5)).unwrap();
        s.channels.unbind_user(5).unwrap();
        let list = s.channels.list().unwrap();
        assert_eq!(list.len(), 2);
        assert!(list.iter().all(|t| t.user_id.is_none()), "该家人的指认全清");
    }
}
