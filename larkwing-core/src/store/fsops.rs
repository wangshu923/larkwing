//! 文件操作日志(PLAN §9 文件能力):操作记录页 + 撤销/重做的数据源。
//! 一次工具调用 = 一批 = 一行;`ops` 列 = 该批每条操作的结构化记录(JSON,
//! 见 `crate::files::FsOpItem`),足够反向(撤销)与重放(重做)。
//! **功能性,非安全承诺**(用户准则)—— 同普通文件管理器的操作历史。

use anyhow::Result;
use serde::Serialize;

use super::db::{m, now_ms, Db, Migration};

pub const MIGRATIONS: &[Migration] = &[m(
    "0012_fsops_init",
    "CREATE TABLE fsops (
        id         INTEGER PRIMARY KEY,
        user_id    INTEGER NOT NULL,
        kind       TEXT NOT NULL,
        ops        TEXT NOT NULL,
        n          INTEGER NOT NULL,
        state      TEXT NOT NULL DEFAULT 'applied',
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    );
    CREATE INDEX idx_fsops_user ON fsops (user_id, id DESC);",
)];

/// 一批文件操作的记账行。`ops` 是 `Vec<crate::files::FsOpItem>` 的 JSON;
/// `state`: "applied"(已生效)| "undone"(已撤销,可重做)。
#[derive(Debug, Clone, Serialize)]
pub struct FsOpRow {
    pub id: i64,
    pub user_id: i64,
    pub kind: String,
    pub ops: String,
    pub n: i64,
    pub state: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone)]
pub struct FsOpRepo {
    db: Db,
}

impl FsOpRepo {
    pub(super) fn new(db: Db) -> Self {
        Self { db }
    }

    /// 记一批(工具执行后调用);返回带 id 的行。
    pub fn record(&self, user_id: i64, kind: &str, ops_json: &str, n: i64) -> Result<FsOpRow> {
        self.db.with(|c| {
            let now = now_ms();
            c.execute(
                "INSERT INTO fsops (user_id, kind, ops, n, state, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, 'applied', ?5, ?5)",
                rusqlite::params![user_id, kind, ops_json, n, now],
            )?;
            let id = c.last_insert_rowid();
            c.query_row(SELECT_ONE, [id], map_row).map_err(Into::into)
        })
    }

    pub fn get(&self, id: i64) -> Result<Option<FsOpRow>> {
        self.db.with(|c| match c.query_row(SELECT_ONE, [id], map_row) {
            Ok(r) => Ok(Some(r)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        })
    }

    /// 操作记录页:某用户最近 `limit` 批,最近在前。
    pub fn list_for(&self, user_id: i64, limit: i64) -> Result<Vec<FsOpRow>> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT id, user_id, kind, ops, n, state, created_at, updated_at
                 FROM fsops WHERE user_id = ?1 ORDER BY id DESC LIMIT ?2",
            )?;
            let rows = stmt
                .query_map(rusqlite::params![user_id, limit], map_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// 撤销(state="applied")/ 重做(state="undone")的就近入口:该用户最近一条该状态批次。
    pub fn latest(&self, user_id: i64, state: &str) -> Result<Option<FsOpRow>> {
        self.db.with(|c| {
            match c.query_row(
                "SELECT id, user_id, kind, ops, n, state, created_at, updated_at
                 FROM fsops WHERE user_id = ?1 AND state = ?2 ORDER BY id DESC LIMIT 1",
                rusqlite::params![user_id, state],
                map_row,
            ) {
                Ok(r) => Ok(Some(r)),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(e.into()),
            }
        })
    }

    /// 撤销/重做后更新状态(+ 时间戳)。
    pub fn set_state(&self, id: i64, state: &str) -> Result<()> {
        self.db.with(|c| {
            c.execute(
                "UPDATE fsops SET state = ?2, updated_at = ?3 WHERE id = ?1",
                rusqlite::params![id, state, now_ms()],
            )?;
            Ok(())
        })
    }
}

const SELECT_ONE: &str =
    "SELECT id, user_id, kind, ops, n, state, created_at, updated_at FROM fsops WHERE id = ?1";

fn map_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<FsOpRow> {
    Ok(FsOpRow {
        id: r.get(0)?,
        user_id: r.get(1)?,
        kind: r.get(2)?,
        ops: r.get(3)?,
        n: r.get(4)?,
        state: r.get(5)?,
        created_at: r.get(6)?,
        updated_at: r.get(7)?,
    })
}

#[cfg(test)]
mod tests {
    use crate::store::Store;

    fn store(tag: &str) -> Store {
        let p = std::env::temp_dir().join(format!("lw-fsops-test-{}-{tag}.db", std::process::id()));
        let _ = std::fs::remove_file(&p);
        Store::open(&p).unwrap()
    }

    #[test]
    fn record_list_latest_and_state_roundtrip() {
        let s = store("rt");
        let a = s.fsops.record(1, "move", r#"[{"op":"move","src":"/a","dst":"/b"}]"#, 1).unwrap();
        let b = s.fsops.record(1, "trash", r#"[{"op":"trash","path":"/c"}]"#, 1).unwrap();
        s.fsops.record(2, "move", "[]", 0).unwrap(); // 别人的,不串

        // 列表:最近在前,只见自己的
        let mine = s.fsops.list_for(1, 50).unwrap();
        assert_eq!(mine.len(), 2);
        assert_eq!(mine[0].id, b.id, "最近在前");

        // 撤销入口 = 最近一条 applied
        let latest = s.fsops.latest(1, "applied").unwrap().unwrap();
        assert_eq!(latest.id, b.id);

        // 撤销 b → 重做入口变 b,撤销入口回退到 a
        s.fsops.set_state(b.id, "undone").unwrap();
        assert_eq!(s.fsops.latest(1, "applied").unwrap().unwrap().id, a.id);
        assert_eq!(s.fsops.latest(1, "undone").unwrap().unwrap().id, b.id);
        assert_eq!(s.fsops.get(b.id).unwrap().unwrap().state, "undone");
    }
}
