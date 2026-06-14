use anyhow::Result;
use rusqlite::OptionalExtension;

use super::db::{m, Db, Migration};

pub const MIGRATIONS: &[Migration] = &[m(
    "0002_settings_init",
    "CREATE TABLE settings (
        scope TEXT NOT NULL,
        key   TEXT NOT NULL,
        value TEXT NOT NULL,
        PRIMARY KEY (scope, key)
    );",
)];

/// scope = None 为 app 级;Some(user_id) 为用户级。
/// 小状态/开关的兜底位(key 带前缀自治,如 `tool.reminder.*`),不为它们开新域。
#[derive(Clone)]
pub struct SettingsRepo {
    db: Db,
}

fn scope_str(scope: Option<i64>) -> String {
    match scope {
        None => "app".into(),
        Some(id) => format!("user:{id}"),
    }
}

impl SettingsRepo {
    pub(super) fn new(db: Db) -> Self {
        Self { db }
    }

    pub fn get(&self, scope: Option<i64>, key: &str) -> Result<Option<String>> {
        self.db.with(|c| {
            let v = c
                .query_row(
                    "SELECT value FROM settings WHERE scope = ?1 AND key = ?2",
                    rusqlite::params![scope_str(scope), key],
                    |r| r.get(0),
                )
                .optional()?;
            Ok(v)
        })
    }

    pub fn list(&self, scope: Option<i64>) -> Result<Vec<(String, String)>> {
        self.db.with(|c| {
            let mut stmt =
                c.prepare("SELECT key, value FROM settings WHERE scope = ?1 ORDER BY key")?;
            let rows = stmt
                .query_map([scope_str(scope)], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    pub fn set(&self, scope: Option<i64>, key: &str, value: &str) -> Result<()> {
        self.db.with(|c| {
            c.execute(
                "INSERT INTO settings (scope, key, value) VALUES (?1, ?2, ?3)
                 ON CONFLICT(scope, key) DO UPDATE SET value = excluded.value",
                rusqlite::params![scope_str(scope), key, value],
            )?;
            Ok(())
        })
    }
}
