use anyhow::Result;
use serde::Serialize;

use super::db::{m, now_ms, Db, Migration};

pub const MIGRATIONS: &[Migration] = &[m(
    "0004_memory_init",
    "CREATE TABLE memories (
        id         INTEGER PRIMARY KEY,
        user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
        kind       TEXT NOT NULL,
        content    TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    );",
)];

/// 记忆归人(宪法 §6),跨场景共享。kind 先宽松:profile/fact/summary;
/// "具体记什么/怎么提炼"是 TBD,这张表只是留口子。
#[derive(Debug, Clone, Serialize)]
pub struct Memory {
    pub id: i64,
    pub user_id: i64,
    pub kind: String,
    pub content: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone)]
pub struct MemoryRepo {
    db: Db,
}

impl MemoryRepo {
    pub(super) fn new(db: Db) -> Self {
        Self { db }
    }

    pub fn add(&self, user: i64, kind: &str, content: &str) -> Result<Memory> {
        self.db.with(|c| {
            let now = now_ms();
            c.execute(
                "INSERT INTO memories (user_id, kind, content, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?4)",
                rusqlite::params![user, kind, content, now],
            )?;
            Ok(Memory {
                id: c.last_insert_rowid(),
                user_id: user,
                kind: kind.into(),
                content: content.into(),
                created_at: now,
                updated_at: now,
            })
        })
    }

    /// 删除一条(回忆页的"记错了点掉"):按 user 限定,防串号删别人的。
    /// 返回是否真删到了(false = 不存在/不是这个人的)。
    pub fn delete(&self, user: i64, id: i64) -> Result<bool> {
        self.db.with(|c| {
            let n = c.execute(
                "DELETE FROM memories WHERE id = ?1 AND user_id = ?2",
                rusqlite::params![id, user],
            )?;
            Ok(n > 0)
        })
    }

    /// 删除某用户的全部记忆(删家人时清理,PLAN §11 D;隐私 = 人走记忆走)。
    pub fn delete_for_user(&self, user: i64) -> Result<()> {
        self.db.with(|c| {
            c.execute("DELETE FROM memories WHERE user_id = ?1", [user])?;
            Ok(())
        })
    }

    pub fn list(&self, user: i64) -> Result<Vec<Memory>> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT id, user_id, kind, content, created_at, updated_at
                 FROM memories WHERE user_id = ?1 ORDER BY id ASC",
            )?;
            let list = stmt
                .query_map([user], |r| {
                    Ok(Memory {
                        id: r.get(0)?,
                        user_id: r.get(1)?,
                        kind: r.get(2)?,
                        content: r.get(3)?,
                        created_at: r.get(4)?,
                        updated_at: r.get(5)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(list)
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::store::Store;

    #[test]
    fn delete_is_scoped_to_user() {
        let dir = std::env::temp_dir().join(format!("lw-mem-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::open(&dir.join("t.db")).unwrap();
        let me = store.users.ensure_default_user().unwrap();
        let m = store.memory.add(me.id, "fact", "对花生过敏").unwrap();

        assert!(!store.memory.delete(me.id + 99, m.id).unwrap(), "别人删不动我的记忆");
        assert_eq!(store.memory.list(me.id).unwrap().len(), 1);

        assert!(store.memory.delete(me.id, m.id).unwrap());
        assert!(store.memory.list(me.id).unwrap().is_empty());
        assert!(!store.memory.delete(me.id, m.id).unwrap(), "重复删 = false,不报错");
        std::fs::remove_dir_all(&dir).ok();
    }
}
