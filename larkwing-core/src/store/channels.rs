//! 远程渠道会话映射(PLAN 远程渠道):平台 chat ↔ Larkwing 会话的持久绑定。
//! 一个 (channel, ext_id) → 一个 conv_id;回访同一 chat 续接同一会话(复用 send_message 的历史回放)。
//! 与「渠道 = 数据」一致:channel 是开放 TEXT,加渠道不改本域。

use anyhow::Result;
use rusqlite::OptionalExtension;

use super::db::{m, now_ms, Db, Migration};

pub const MIGRATIONS: &[Migration] = &[m(
    "0013_channels_init",
    "CREATE TABLE channel_threads (
        id         INTEGER PRIMARY KEY,
        channel    TEXT NOT NULL,
        ext_id     TEXT NOT NULL,
        conv_id    INTEGER NOT NULL,
        created_at INTEGER NOT NULL,
        UNIQUE (channel, ext_id)
    );",
)];

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
        self.db.with(|c| {
            let id = c
                .query_row(
                    "SELECT conv_id FROM channel_threads WHERE channel = ?1 AND ext_id = ?2",
                    rusqlite::params![channel, ext_id],
                    |r| r.get::<_, i64>(0),
                )
                .optional()?;
            Ok(id)
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
}
