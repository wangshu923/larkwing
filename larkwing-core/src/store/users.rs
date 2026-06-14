use anyhow::Result;
use rusqlite::OptionalExtension;
use serde::Serialize;

use super::db::{m, now_ms, Db, Migration};

pub const MIGRATIONS: &[Migration] = &[m(
    "0001_users_init",
    "CREATE TABLE users (
        id             INTEGER PRIMARY KEY,
        name           TEXT NOT NULL,
        skin_id        TEXT NOT NULL DEFAULT 'warm',
        created_at     INTEGER NOT NULL,
        last_active_at INTEGER NOT NULL
    );",
)];

#[derive(Debug, Clone, Serialize)]
pub struct User {
    pub id: i64,
    pub name: String,
    pub skin_id: String,
    pub created_at: i64,
    pub last_active_at: i64,
}

#[derive(Clone)]
pub struct UserRepo {
    db: Db,
}

impl UserRepo {
    pub(super) fn new(db: Db) -> Self {
        Self { db }
    }

    /// 首启零配置:一个用户都没有时自动建默认用户;否则返回最近活跃的那个。
    pub fn ensure_default_user(&self) -> Result<User> {
        self.db.with(|c| {
            let existing = c
                .query_row(
                    "SELECT id, name, skin_id, created_at, last_active_at
                     FROM users ORDER BY last_active_at DESC LIMIT 1",
                    [],
                    row_to_user,
                )
                .optional()?;
            if let Some(u) = existing {
                return Ok(u);
            }
            let now = now_ms();
            c.execute(
                "INSERT INTO users (name, skin_id, created_at, last_active_at)
                 VALUES ('我', 'warm', ?1, ?1)",
                [now],
            )?;
            Ok(User {
                id: c.last_insert_rowid(),
                name: "我".into(),
                skin_id: "warm".into(),
                created_at: now,
                last_active_at: now,
            })
        })
    }

    /// 添加家人(PLAN §11 D 多用户落地):新建一个用户,记忆/声纹各自独立。
    pub fn create(&self, name: &str) -> Result<User> {
        self.db.with(|c| {
            let now = now_ms();
            c.execute(
                "INSERT INTO users (name, skin_id, created_at, last_active_at)
                 VALUES (?1, 'warm', ?2, ?2)",
                rusqlite::params![name, now],
            )?;
            Ok(User {
                id: c.last_insert_rowid(),
                name: name.into(),
                skin_id: "warm".into(),
                created_at: now,
                last_active_at: now,
            })
        })
    }

    /// 删除家人(只删 users 行;关联的记忆/声纹由 engine.delete_user 编排清理)。
    pub fn delete(&self, id: i64) -> Result<()> {
        self.db.with(|c| {
            c.execute("DELETE FROM users WHERE id = ?1", [id])?;
            Ok(())
        })
    }

    pub fn count(&self) -> Result<i64> {
        self.db.with(|c| Ok(c.query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))?))
    }

    /// 校验 user 是否真实存在(声纹识别结果回灌前的防注入)。
    pub fn get(&self, id: i64) -> Result<Option<User>> {
        self.db.with(|c| {
            Ok(c.query_row(
                "SELECT id, name, skin_id, created_at, last_active_at FROM users WHERE id = ?1",
                [id],
                row_to_user,
            )
            .optional()?)
        })
    }

    pub fn list(&self) -> Result<Vec<User>> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT id, name, skin_id, created_at, last_active_at
                 FROM users ORDER BY last_active_at DESC",
            )?;
            let users = stmt
                .query_map([], row_to_user)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(users)
        })
    }

    pub fn set_skin(&self, user: i64, skin_id: &str) -> Result<()> {
        self.db.with(|c| {
            c.execute(
                "UPDATE users SET skin_id = ?2 WHERE id = ?1",
                rusqlite::params![user, skin_id],
            )?;
            Ok(())
        })
    }

    pub fn rename(&self, user: i64, name: &str) -> Result<()> {
        self.db.with(|c| {
            c.execute(
                "UPDATE users SET name = ?2 WHERE id = ?1",
                rusqlite::params![user, name],
            )?;
            Ok(())
        })
    }

    pub fn touch(&self, user: i64) -> Result<()> {
        self.db.with(|c| {
            c.execute(
                "UPDATE users SET last_active_at = ?2 WHERE id = ?1",
                rusqlite::params![user, now_ms()],
            )?;
            Ok(())
        })
    }
}

fn row_to_user(r: &rusqlite::Row<'_>) -> rusqlite::Result<User> {
    Ok(User {
        id: r.get(0)?,
        name: r.get(1)?,
        skin_id: r.get(2)?,
        created_at: r.get(3)?,
        last_active_at: r.get(4)?,
    })
}
