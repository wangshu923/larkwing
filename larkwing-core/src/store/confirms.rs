//! 确认流水(§7.8 动作确认闸的审计半边):一次确认一行,只进不改(usage_rounds 同款
//! 流水纪律)。回答的是事后那句「上周它到底点没点?谁点的头?」——入口在操作记录页
//! 加一个分组(不造新页、不叫「审批」,§3 用户面零新概念)。

use anyhow::Result;
use serde::Serialize;

use super::db::{m, now_ms, Db, Migration};

pub const MIGRATIONS: &[Migration] = &[m(
    "0024_confirms_init",
    "CREATE TABLE confirms (
        id         INTEGER PRIMARY KEY,
        user_id    INTEGER NOT NULL,
        conv_id    INTEGER NOT NULL,
        origin     TEXT NOT NULL,
        host       TEXT NOT NULL,
        action     TEXT NOT NULL,
        kind       TEXT NOT NULL,
        decision   TEXT NOT NULL,
        via        TEXT NOT NULL,
        created_at INTEGER NOT NULL
    );
    CREATE INDEX idx_confirms_recent ON confirms (id DESC);",
)];

/// 一次确认的记账行。decision: allowed | denied;via: desktop | float | voice | channel
/// | timeout | no_ui(超时/无通道按 denied 记,via 说明为什么)。
#[derive(Debug, Clone, Serialize)]
pub struct ConfirmRow {
    pub id: i64,
    pub user_id: i64,
    pub conv_id: i64,
    pub origin: String,
    pub host: String,
    pub action: String,
    pub kind: String,
    pub decision: String,
    pub via: String,
    pub created_at: i64,
}

#[derive(Clone)]
pub struct ConfirmRepo {
    db: Db,
}

impl ConfirmRepo {
    pub(super) fn new(db: Db) -> Self {
        Self { db }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record(
        &self,
        user_id: i64,
        conv_id: i64,
        origin: &str,
        host: &str,
        action: &str,
        kind: &str,
        decision: &str,
        via: &str,
    ) -> Result<()> {
        self.db.with(|c| {
            c.execute(
                "INSERT INTO confirms (user_id, conv_id, origin, host, action, kind, decision, via, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                rusqlite::params![user_id, conv_id, origin, host, action, kind, decision, via, now_ms()],
            )?;
            Ok(())
        })
    }

    /// 记录页:最近 `limit` 条,最近在前。主人管理面(提醒页同款):全家的都列,
    /// 行里带 user_id 前端 resolve 家人名。
    pub fn list_recent(&self, limit: i64) -> Result<Vec<ConfirmRow>> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT id, user_id, conv_id, origin, host, action, kind, decision, via, created_at
                 FROM confirms ORDER BY id DESC LIMIT ?1",
            )?;
            let rows = stmt
                .query_map([limit], |r| {
                    Ok(ConfirmRow {
                        id: r.get(0)?,
                        user_id: r.get(1)?,
                        conv_id: r.get(2)?,
                        origin: r.get(3)?,
                        host: r.get(4)?,
                        action: r.get(5)?,
                        kind: r.get(6)?,
                        decision: r.get(7)?,
                        via: r.get(8)?,
                        created_at: r.get(9)?,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::Store;

    fn temp_store(name: &str) -> Store {
        let p = std::env::temp_dir().join(format!(
            "larkwing_test_confirms_{}_{}.db",
            std::process::id(),
            name
        ));
        let _ = std::fs::remove_file(&p);
        Store::open(&p).unwrap()
    }

    #[test]
    fn record_and_list_recent_first() {
        let store = temp_store("roundtrip");
        store
            .confirms
            .record(1, 5, "ui", "x.example.com", "确认支付 ¥128", "click", "allowed", "desktop")
            .unwrap();
        store
            .confirms
            .record(2, 6, "weixin", "y.example.com", "Delete", "submit", "denied", "channel")
            .unwrap();
        let rows = store.confirms.list_recent(10).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].host, "y.example.com", "最近在前");
        assert_eq!(rows[1].decision, "allowed");
        assert_eq!(rows[0].user_id, 2, "带归属,前端 resolve 家人名");
    }
}
