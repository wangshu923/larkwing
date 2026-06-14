//! 执行层:连接 + 锁 + 事务 + 迁移机。不认识任何业务表。

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use rusqlite::{Connection, Transaction};

/// 一条迁移:id 全局唯一、带序号前缀(如 `0003_chat_init`),按 id 排序执行。
#[derive(Clone, Copy)]
pub struct Migration {
    pub id: &'static str,
    pub sql: &'static str,
}

pub const fn m(id: &'static str, sql: &'static str) -> Migration {
    Migration { id, sql }
}

#[derive(Clone)]
pub struct Db(Arc<Mutex<Connection>>);

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        let conn = Connection::open(path)
            .with_context(|| format!("打开数据库失败: {}", path.display()))?;
        // WAL 对内存库不生效,失败可忽略;其余 PRAGMA 必须成功
        conn.pragma_update(None, "journal_mode", "WAL").ok();
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(Db(Arc::new(Mutex::new(conn))))
    }

    /// 拿锁执行。域方法的唯一入口。
    pub fn with<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let conn = self.0.lock().expect("db mutex poisoned");
        f(&conn)
    }

    /// 跨域事务。
    pub fn tx<T>(&self, f: impl FnOnce(&Transaction) -> Result<T>) -> Result<T> {
        let mut conn = self.0.lock().expect("db mutex poisoned");
        let tx = conn.transaction()?;
        let out = f(&tx)?;
        tx.commit()?;
        Ok(out)
    }

    /// 迁移机:`schema_migrations` 按名记账;收集 → 排序 → 重号即报错 → 补跑未记账的。
    pub fn migrate(&self, all: &[Migration]) -> Result<()> {
        let mut sorted: Vec<&Migration> = all.iter().collect();
        sorted.sort_by_key(|mig| mig.id);
        for pair in sorted.windows(2) {
            if pair[0].id == pair[1].id {
                bail!("迁移 id 重复: {}", pair[0].id);
            }
        }
        self.tx(|tx| {
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS schema_migrations (
                    id         TEXT PRIMARY KEY,
                    applied_at INTEGER NOT NULL
                );",
            )?;
            for mig in &sorted {
                let applied: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM schema_migrations WHERE id = ?1)",
                    [mig.id],
                    |r| r.get(0),
                )?;
                if applied {
                    continue;
                }
                tx.execute_batch(mig.sql)
                    .with_context(|| format!("迁移 {} 执行失败", mig.id))?;
                tx.execute(
                    "INSERT INTO schema_migrations (id, applied_at) VALUES (?1, ?2)",
                    rusqlite::params![mig.id, now_ms()],
                )?;
            }
            Ok(())
        })
    }
}

/// 全库统一时间戳:unix 毫秒。
pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
