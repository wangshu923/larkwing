//! 分离 job 域(PLAN §8 预记录约束 #3 的兑现):提醒/定时类任务持久化,防重启丢失。
//! robot cron 的消费级重生:cron 表达式 → repeat 枚举(模型翻译自然语言,用户永远
//! 不见 cron);到点以"一条消息"进入原会话,触发 engine 自启回合(真相在库、回合无状态)。

use anyhow::Result;
use serde::Serialize;

use super::db::{m, now_ms, Db, Migration};

pub const MIGRATIONS: &[Migration] = &[m(
    "0009_jobs_init",
    "CREATE TABLE jobs (
        id           INTEGER PRIMARY KEY,
        user_id      INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
        conv_id      INTEGER NOT NULL,
        content      TEXT NOT NULL,
        due_at       INTEGER NOT NULL,
        repeat       TEXT NOT NULL DEFAULT 'once',
        status       TEXT NOT NULL DEFAULT 'pending',
        created_at   INTEGER NOT NULL,
        updated_at   INTEGER NOT NULL
    );
    CREATE INDEX idx_jobs_due ON jobs (status, due_at);",
)];

/// status 词表:pending(待触发)/ done(一次性完成)/ cancelled / missed(错过太久不补发)。
/// repeat 词表:once / daily / weekdays / weekly(每周同星期,锚定 due_at 的星期)。
#[derive(Debug, Clone, Serialize)]
pub struct Job {
    pub id: i64,
    pub user_id: i64,
    pub conv_id: i64,
    pub content: String,
    /// unix 毫秒(本地时区换算后的绝对时刻)。
    pub due_at: i64,
    pub repeat: String,
    pub status: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone)]
pub struct JobRepo {
    db: Db,
}

impl JobRepo {
    pub(super) fn new(db: Db) -> Self {
        Self { db }
    }

    pub fn add(
        &self,
        user_id: i64,
        conv_id: i64,
        content: &str,
        due_at: i64,
        repeat: &str,
    ) -> Result<Job> {
        self.db.with(|c| {
            let now = now_ms();
            c.execute(
                "INSERT INTO jobs (user_id, conv_id, content, due_at, repeat, status, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6, ?6)",
                rusqlite::params![user_id, conv_id, content, due_at, repeat, now],
            )?;
            let id = c.last_insert_rowid();
            Ok(Job {
                id,
                user_id,
                conv_id,
                content: content.into(),
                due_at,
                repeat: repeat.into(),
                status: "pending".into(),
                created_at: now,
                updated_at: now,
            })
        })
    }

    /// 该用户的待触发清单(reminder_list / 将来 UI 用),按 due_at 升序。
    pub fn list_pending(&self, user_id: i64) -> Result<Vec<Job>> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT id, user_id, conv_id, content, due_at, repeat, status, created_at, updated_at
                 FROM jobs WHERE user_id = ?1 AND status = 'pending' ORDER BY due_at",
            )?;
            let rows = stmt
                .query_map([user_id], map_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// 到点的任务(调度器轮询用):全用户、status=pending、due_at <= now。
    pub fn due(&self, now: i64) -> Result<Vec<Job>> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT id, user_id, conv_id, content, due_at, repeat, status, created_at, updated_at
                 FROM jobs WHERE status = 'pending' AND due_at <= ?1 ORDER BY due_at",
            )?;
            let rows = stmt
                .query_map([now], map_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// 取消(按 user 限定,防串号);返回是否真取消了。
    pub fn cancel(&self, user_id: i64, id: i64) -> Result<bool> {
        self.set_status_scoped(user_id, id, "cancelled")
    }

    /// 触发后的状态推进:一次性 → done/missed;重复 → 推进 due_at 留在 pending。
    pub fn finish(&self, id: i64, status: &str) -> Result<()> {
        self.db.with(|c| {
            c.execute(
                "UPDATE jobs SET status = ?2, updated_at = ?3 WHERE id = ?1",
                rusqlite::params![id, status, now_ms()],
            )?;
            Ok(())
        })
    }

    pub fn advance(&self, id: i64, next_due: i64) -> Result<()> {
        self.db.with(|c| {
            c.execute(
                "UPDATE jobs SET due_at = ?2, updated_at = ?3 WHERE id = ?1",
                rusqlite::params![id, next_due, now_ms()],
            )?;
            Ok(())
        })
    }

    fn set_status_scoped(&self, user_id: i64, id: i64, status: &str) -> Result<bool> {
        self.db.with(|c| {
            let n = c.execute(
                "UPDATE jobs SET status = ?3, updated_at = ?4
                 WHERE id = ?1 AND user_id = ?2 AND status = 'pending'",
                rusqlite::params![id, user_id, status, now_ms()],
            )?;
            Ok(n > 0)
        })
    }
}

fn map_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Job> {
    Ok(Job {
        id: r.get(0)?,
        user_id: r.get(1)?,
        conv_id: r.get(2)?,
        content: r.get(3)?,
        due_at: r.get(4)?,
        repeat: r.get(5)?,
        status: r.get(6)?,
        created_at: r.get(7)?,
        updated_at: r.get(8)?,
    })
}

#[cfg(test)]
mod tests {
    use crate::store::Store;

    fn store(tag: &str) -> Store {
        let p =
            std::env::temp_dir().join(format!("lw-jobs-test-{}-{tag}.db", std::process::id()));
        let _ = std::fs::remove_file(&p);
        Store::open(&p).unwrap()
    }

    #[test]
    fn add_due_cancel_flow() {
        let s = store("flow");
        let u = s.users.ensure_default_user().unwrap();
        let a = s.jobs.add(u.id, 1, "吃药", 1000, "once").unwrap();
        let b = s.jobs.add(u.id, 1, "交作业", 99_999, "weekly").unwrap();

        let due = s.jobs.due(2000).unwrap();
        assert_eq!(due.len(), 1, "只有到点的");
        assert_eq!(due[0].id, a.id);

        assert!(!s.jobs.cancel(u.id + 9, b.id).unwrap(), "别人取消不了我的提醒");
        assert!(s.jobs.cancel(u.id, b.id).unwrap());
        assert!(!s.jobs.cancel(u.id, b.id).unwrap(), "重复取消 = false");
        assert_eq!(s.jobs.list_pending(u.id).unwrap().len(), 1);
    }

    #[test]
    fn finish_and_advance() {
        let s = store("adv");
        let u = s.users.ensure_default_user().unwrap();
        let once = s.jobs.add(u.id, 1, "关火", 1000, "once").unwrap();
        let daily = s.jobs.add(u.id, 1, "吃药", 1000, "daily").unwrap();

        s.jobs.finish(once.id, "done").unwrap();
        s.jobs.advance(daily.id, 87_400_000).unwrap();

        assert!(s.jobs.due(2000).unwrap().is_empty(), "done 的不再到点,daily 推进了");
        let pending = s.jobs.list_pending(u.id).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].due_at, 87_400_000);
    }
}
