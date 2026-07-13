//! 远程渠道会话映射(PLAN 远程渠道):平台 chat ↔ Larkwing 会话的持久绑定。
//! **映射是历史行(append-only,2026-07-13 会话轮换起)**:一个 (channel, ext_id) 名下可有
//! 多行,**最新一行 = 现行会话**(`thread_for`);老行留档,`thread_by_conv` 反查(提醒推回
//! 手机、会话列表发起人标签)对轮换走的老会话照样命中。改绑 = 插新行(继承指认/昵称/推送地址)。
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
    // 会话轮换(单聊 12h 闲置开新会话,§7.7 2026-07-13):映射从「一 chat 一行覆盖式」改
    // 「历史行」——去掉 UNIQUE(channel, ext_id),同 chat 每次改绑插新行、老行留档,轮换走的
    // 老会话仍能按 conv_id 反查到(提醒推送/发起人标签不断链)。SQLite 表级 UNIQUE 只能重建表。
    m(
        "0023_channel_threads_history",
        "CREATE TABLE channel_threads_v2 (
        id         INTEGER PRIMARY KEY,
        channel    TEXT NOT NULL,
        ext_id     TEXT NOT NULL,
        conv_id    INTEGER NOT NULL,
        created_at INTEGER NOT NULL,
        user_id    INTEGER,
        label      TEXT,
        push_id    TEXT
    );
    INSERT INTO channel_threads_v2 (id, channel, ext_id, conv_id, created_at, user_id, label, push_id)
        SELECT id, channel, ext_id, conv_id, created_at, user_id, label, push_id FROM channel_threads;
    DROP TABLE channel_threads;
    ALTER TABLE channel_threads_v2 RENAME TO channel_threads;
    CREATE INDEX idx_channel_threads_chat ON channel_threads (channel, ext_id);
    CREATE INDEX idx_channel_threads_conv ON channel_threads (conv_id);",
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

    /// 该平台 chat 现行会话 id(最新一行;无则 None → 调用方建会话再 bind)。
    pub fn conv_for(&self, channel: &str, ext_id: &str) -> Result<Option<i64>> {
        Ok(self.thread_for(channel, ext_id)?.map(|t| t.conv_id))
    }

    /// 现行映射(渠道入站用:conv_id + 指认的家人一次拿到)= 该 chat 最新一行;老行是历史。
    pub fn thread_for(&self, channel: &str, ext_id: &str) -> Result<Option<ChannelThread>> {
        self.db.with(|c| {
            let t = c
                .query_row(
                    "SELECT id, channel, ext_id, conv_id, user_id, label, push_id
                     FROM channel_threads WHERE channel = ?1 AND ext_id = ?2
                     ORDER BY id DESC LIMIT 1",
                    rusqlite::params![channel, ext_id],
                    row_to_thread,
                )
                .optional()?;
            Ok(t)
        })
    }

    /// 反查:这个会话是不是渠道映射会话(提醒推回手机、发起人标签用;桌面会话 → None)。
    /// 历史行让轮换走的老会话也命中(到点回合落在老会话里,推送链不断)。
    pub fn thread_by_conv(&self, conv_id: i64) -> Result<Option<ChannelThread>> {
        self.db.with(|c| {
            let t = c
                .query_row(
                    "SELECT id, channel, ext_id, conv_id, user_id, label, push_id
                     FROM channel_threads WHERE conv_id = ?1
                     ORDER BY id DESC LIMIT 1",
                    [conv_id],
                    row_to_thread,
                )
                .optional()?;
            Ok(t)
        })
    }

    /// 该 chat 名下「最近有动静」的会话:(conv_id, updated_at),已删除的不算。
    /// 会话轮换的判据源——现行会话通常就是最新的;提醒刚在轮换走的老会话到点时,
    /// 老会话反而最新(用户回「收到」该接回那里,不落进没头没尾的新会话)。
    /// 跨域读 conversations 属 §6.2「schema 不隔离」正常用法。
    pub fn latest_active_conv(&self, channel: &str, ext_id: &str) -> Result<Option<(i64, i64)>> {
        self.db.with(|c| {
            let r = c
                .query_row(
                    "SELECT t.conv_id, c.updated_at
                     FROM channel_threads t JOIN conversations c ON c.id = t.conv_id
                     WHERE t.channel = ?1 AND t.ext_id = ?2
                     ORDER BY c.updated_at DESC, t.id DESC LIMIT 1",
                    rusqlite::params![channel, ext_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            Ok(r)
        })
    }

    /// 把 chat 的现行会话指到 conv_id:插一条新历史行,**继承**上一行的指认/昵称/推送地址
    /// (轮换开新会话不丢归人);已是现行则幂等 no-op。老行留档给 `thread_by_conv` 反查。
    /// 读-插两步不在一个事务:并发重复只会多一条同值历史行,最新行仍对,可容忍。
    pub fn bind(&self, channel: &str, ext_id: &str, conv_id: i64) -> Result<()> {
        let prev = self.thread_for(channel, ext_id)?;
        if prev.as_ref().is_some_and(|t| t.conv_id == conv_id) {
            return Ok(());
        }
        let (user_id, label, push_id) =
            prev.map(|t| (t.user_id, t.label, t.push_id)).unwrap_or_default();
        self.db.with(|c| {
            c.execute(
                "INSERT INTO channel_threads
                     (channel, ext_id, conv_id, created_at, user_id, label, push_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![channel, ext_id, conv_id, now_ms(), user_id, label, push_id],
            )?;
            Ok(())
        })
    }

    /// 全部映射(家人页「远程对话」区;每个 chat 只出最新一行,最近建的在前)。
    pub fn list(&self) -> Result<Vec<ChannelThread>> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT id, channel, ext_id, conv_id, user_id, label, push_id
                 FROM channel_threads
                 WHERE id IN (SELECT MAX(id) FROM channel_threads GROUP BY channel, ext_id)
                 ORDER BY created_at DESC",
            )?;
            let rows =
                stmt.query_map([], row_to_thread)?.collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// 指认这条对话是谁在用(None = 取消指认,回到会话归属者)。
    /// 指认是「这个 chat 是谁的」→ 落到该 chat 的**全部**历史行(轮换走的老会话
    /// 发起人标签跟着对;指认错了重指也追溯改正)。
    pub fn bind_user(&self, thread_id: i64, user_id: Option<i64>) -> Result<()> {
        self.db.with(|c| {
            c.execute(
                "UPDATE channel_threads SET user_id = ?2
                 WHERE (channel, ext_id) =
                       (SELECT channel, ext_id FROM channel_threads WHERE id = ?1)",
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
    fn rebind_appends_history_and_channels_are_isolated() {
        let s = store("rebind");
        s.channels.bind("telegram", "100", 1).unwrap();
        s.channels.bind("telegram", "100", 1).unwrap(); // 已是现行 → 幂等,不多插行
        s.channels.bind("telegram", "100", 2).unwrap(); // 改绑(轮换/会话被删后重建)= 插新行
        assert_eq!(s.channels.conv_for("telegram", "100").unwrap(), Some(2), "最新行 = 现行");
        // 老行留档:轮换走的老会话反查照样命中(提醒推送/发起人标签靠它)
        let old = s.channels.thread_by_conv(1).unwrap().unwrap();
        assert_eq!((old.channel.as_str(), old.ext_id.as_str()), ("telegram", "100"));
        // 家人页列表每 chat 只出最新一行
        assert_eq!(s.channels.list().unwrap().len(), 1, "历史行不进列表");
        // 同 ext_id 不同渠道互不串
        s.channels.bind("dingtalk", "100", 9).unwrap();
        assert_eq!(s.channels.conv_for("telegram", "100").unwrap(), Some(2));
        assert_eq!(s.channels.conv_for("dingtalk", "100").unwrap(), Some(9));
        assert_eq!(s.channels.list().unwrap().len(), 2);
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

        // 改绑(轮换)= 新行继承指认与昵称
        s.channels.bind_user(t.id, Some(42)).unwrap();
        s.channels.bind("telegram", "555", 8).unwrap();
        let t = s.channels.thread_for("telegram", "555").unwrap().unwrap();
        assert_eq!((t.conv_id, t.user_id, t.label.as_deref()), (8, Some(42), Some("豆豆")));

        // 指认落全部历史行(老会话的发起人标签跟着对);昵称更新同样全行生效
        let old = s.channels.thread_by_conv(3).unwrap().unwrap();
        assert_eq!(old.user_id, Some(42), "历史行同步指认");
        s.channels.bind_user(t.id, Some(7)).unwrap();
        let old = s.channels.thread_by_conv(3).unwrap().unwrap();
        assert_eq!(old.user_id, Some(7), "重新指认追溯改正历史行");
        s.channels.set_label("telegram", "555", "蛋蛋").unwrap();
        let old = s.channels.thread_by_conv(3).unwrap().unwrap();
        assert_eq!(old.label.as_deref(), Some("蛋蛋"), "昵称按 (channel, ext_id) 全行更新");
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

        // 推送地址:去空白落库、空白不覆盖;改绑(轮换)新行继承
        s.channels.set_push_id("dingtalk", "cidAAA", " staff9 ").unwrap();
        s.channels.set_push_id("dingtalk", "cidAAA", "   ").unwrap();
        s.channels.bind("dingtalk", "cidAAA", 9).unwrap();
        let t = s.channels.thread_by_conv(9).unwrap().unwrap();
        assert_eq!(t.push_id.as_deref(), Some("staff9"));
        // 更新推送地址按 (channel, ext_id) 全行生效:老会话到点推送用的也是新地址
        // (微信 context_token 每条消息都在换,提醒落在轮换走的老会话时必须拿最新的)
        s.channels.set_push_id("dingtalk", "cidAAA", "staff10").unwrap();
        let old = s.channels.thread_by_conv(7).unwrap().unwrap();
        assert_eq!(old.push_id.as_deref(), Some("staff10"), "历史行推送地址保鲜");
    }

    #[test]
    fn latest_active_conv_tracks_freshest_and_skips_deleted() {
        let s = store("active");
        let user = s.users.ensure_default_user().unwrap();
        let mk = || s.chat.create_conversation_full(user.id, "companion", "telegram").unwrap();

        assert!(s.channels.latest_active_conv("telegram", "x").unwrap().is_none(), "没绑过");

        // 绑到不存在的会话(桌面删了)→ JOIN 落空 = 没有活会话
        s.channels.bind("telegram", "x", 424242).unwrap();
        assert!(s.channels.latest_active_conv("telegram", "x").unwrap().is_none(), "悬空不算");

        // 轮换出两代会话:名下最近动静 = updated_at 最大的那个,不一定是现行行
        let a = mk();
        s.channels.bind("telegram", "x", a.id).unwrap();
        let b = mk();
        s.channels.bind("telegram", "x", b.id).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2)); // updated_at 是毫秒,拉开平局
        s.chat.append_message(a.id, "event", "提醒:该吃药了").unwrap(); // 老会话被提醒叫醒
        let (cid, _) = s.channels.latest_active_conv("telegram", "x").unwrap().unwrap();
        assert_eq!(cid, a.id, "动静在老会话(提醒到点)→ 该接回它");
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
