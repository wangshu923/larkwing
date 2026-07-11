//! 分离 job 域(PLAN §8 预记录约束 #3 的兑现):提醒/定时类任务持久化,防重启丢失。
//! robot cron 的消费级重生:cron 表达式 → repeat 枚举(模型翻译自然语言,用户永远
//! 不见 cron);到点以"一条消息"进入原会话,触发 engine 自启回合(真相在库、回合无状态)。

use anyhow::Result;
use serde::Serialize;

use super::db::{m, now_ms, Db, Migration};

pub const MIGRATIONS: &[Migration] = &[
    m(
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
    ),
    // 条件提醒(PLAN 天气块):kind=time(到点,现有)| cond(满足条件才触发,due_at 复用成
    // 「下次检查时刻」);condition = JSON 天气谓词(see scheduler::watch)。现有提醒走默认 time。
    m(
        "0010_jobs_condition",
        "ALTER TABLE jobs ADD COLUMN kind TEXT NOT NULL DEFAULT 'time';
         ALTER TABLE jobs ADD COLUMN condition TEXT;",
    ),
    // 跨人提醒/捎话(人际路由):created_by = 发起人(NULL = 自己设的,老数据零变化)。
    // 给家人设的提醒 user_id = 收件的家人(到点归 TA、TA 也看得见撤得掉),发起人凭本列
    // 在 reminder_list / cancel 里同样看得见、撤得掉(「我给爸爸设的,我要能反悔」)。
    m("0022_jobs_created_by", "ALTER TABLE jobs ADD COLUMN created_by INTEGER;"),
];

/// status 词表:pending(待触发)/ done(一次性完成)/ cancelled / missed(错过太久不补发)。
/// repeat 词表:once / daily / weekdays / weekly(每周同星期,锚定 due_at 的星期)。
#[derive(Debug, Clone, Serialize)]
pub struct Job {
    pub id: i64,
    pub user_id: i64,
    pub conv_id: i64,
    pub content: String,
    /// unix 毫秒;time 型 = 触发时刻,cond 型 = 下次检查时刻。
    pub due_at: i64,
    pub repeat: String,
    pub status: String,
    /// time(到点触发)| cond(满足 condition 才触发)。
    pub kind: String,
    /// cond 型的谓词 JSON(scheduler::watch 解析);time 型为 None。
    pub condition: Option<String>,
    /// 发起人(跨人提醒:给家人设的,user_id = 收件家人、这里记谁设的;None = 自己设的)。
    pub created_by: Option<i64>,
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
        self.add_for(user_id, None, conv_id, content, due_at, repeat)
    }

    /// 带发起人的写入(跨人提醒:user_id = 收件家人,created_by = 谁设的;自己设的走 `add`)。
    pub fn add_for(
        &self,
        user_id: i64,
        created_by: Option<i64>,
        conv_id: i64,
        content: &str,
        due_at: i64,
        repeat: &str,
    ) -> Result<Job> {
        self.db.with(|c| {
            let now = now_ms();
            c.execute(
                "INSERT INTO jobs (user_id, conv_id, content, due_at, repeat, status, created_by, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6, ?7, ?7)",
                rusqlite::params![user_id, conv_id, content, due_at, repeat, created_by, now],
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
                kind: "time".into(),
                condition: None,
                created_by,
                created_at: now,
                updated_at: now,
            })
        })
    }

    /// 条件提醒(kind=cond):due_at = 首次检查时刻,condition = 谓词 JSON;repeat 固定 once
    /// (满足即收尾)。检查节奏由 scheduler 推进 due_at 控制,不走 repeat。
    pub fn add_watch(
        &self,
        user_id: i64,
        conv_id: i64,
        content: &str,
        condition: &str,
        first_check_at: i64,
    ) -> Result<Job> {
        self.db.with(|c| {
            let now = now_ms();
            c.execute(
                "INSERT INTO jobs (user_id, conv_id, content, due_at, repeat, status, kind, condition, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, 'once', 'pending', 'cond', ?5, ?6, ?6)",
                rusqlite::params![user_id, conv_id, content, first_check_at, condition, now],
            )?;
            let id = c.last_insert_rowid();
            Ok(Job {
                id,
                user_id,
                conv_id,
                content: content.into(),
                due_at: first_check_at,
                repeat: "once".into(),
                status: "pending".into(),
                kind: "cond".into(),
                condition: Some(condition.into()),
                created_by: None,
                created_at: now,
                updated_at: now,
            })
        })
    }

    /// 该用户的待触发清单(悬浮窗「下个提醒」等自己视角的用途),按 due_at 升序。
    pub fn list_pending(&self, user_id: i64) -> Result<Vec<Job>> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT id, user_id, conv_id, content, due_at, repeat, status, created_at, updated_at, kind, condition, created_by
                 FROM jobs WHERE user_id = ?1 AND status = 'pending' ORDER BY due_at",
            )?;
            let rows = stmt
                .query_map([user_id], map_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// 该用户**看得见**的待触发清单(reminder_list 用):自己的 + 自己给家人设的
    /// (created_by = TA;跨人提醒发起人得看得见自己设了什么、才反悔得了)。
    pub fn list_visible(&self, user_id: i64) -> Result<Vec<Job>> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT id, user_id, conv_id, content, due_at, repeat, status, created_at, updated_at, kind, condition, created_by
                 FROM jobs WHERE (user_id = ?1 OR created_by = ?1) AND status = 'pending' ORDER BY due_at",
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
                "SELECT id, user_id, conv_id, content, due_at, repeat, status, created_at, updated_at, kind, condition, created_by
                 FROM jobs WHERE status = 'pending' AND due_at <= ?1 ORDER BY due_at",
            )?;
            let rows = stmt
                .query_map([now], map_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// 取消(按 user 限定,防串号);收件人与发起人都可撤(跨人提醒:给爸爸设的,
    /// 设的人和爸爸自己都能取消)。返回是否真取消了。
    pub fn cancel(&self, user_id: i64, id: i64) -> Result<bool> {
        self.set_status_scoped(user_id, id, "cancelled")
    }

    /// 全家待触发提醒(桌面提醒页 = 主人的管理面:家人经渠道/归人设的也看得见、管得着;
    /// 工具侧 reminder_list/cancel 仍按说话人限定)。
    pub fn list_pending_all(&self) -> Result<Vec<Job>> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT id, user_id, conv_id, content, due_at, repeat, status, created_at, updated_at, kind, condition, created_by
                 FROM jobs WHERE status = 'pending' ORDER BY due_at",
            )?;
            let jobs = stmt.query_map([], map_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(jobs)
        })
    }

    /// 不限定用户的取消(桌面提醒页用,主人可撤全家的;pending 才可撤)。
    pub fn cancel_any(&self, id: i64) -> Result<bool> {
        self.db.with(|c| {
            let n = c.execute(
                "UPDATE jobs SET status = 'cancelled', updated_at = ?2
                 WHERE id = ?1 AND status = 'pending'",
                rusqlite::params![id, now_ms()],
            )?;
            Ok(n > 0)
        })
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
                 WHERE id = ?1 AND (user_id = ?2 OR created_by = ?2) AND status = 'pending'",
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
        kind: r.get(9)?,
        condition: r.get(10)?,
        created_by: r.get(11)?,
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
    fn cross_person_visibility_and_cancel() {
        let s = store("cross");
        let owner = s.users.ensure_default_user().unwrap();
        let dad = s.users.create("爸爸").unwrap();
        // 主人给爸爸设的:收件人 = 爸爸,发起人 = 主人
        let j = s.jobs.add_for(dad.id, Some(owner.id), 7, "吃降压药", 9000, "once").unwrap();
        assert_eq!(j.created_by, Some(owner.id));

        // 爸爸自己视角(list_pending)与双方可见视角(list_visible)都看得到
        assert_eq!(s.jobs.list_pending(dad.id).unwrap().len(), 1);
        assert!(s.jobs.list_pending(owner.id).unwrap().is_empty(), "悬浮窗视角只看自己收件的");
        assert_eq!(s.jobs.list_visible(owner.id).unwrap().len(), 1, "发起人看得见自己设的");
        assert_eq!(s.jobs.list_visible(dad.id).unwrap().len(), 1);

        // 无关第三人撤不了;发起人能撤(反悔);收件人也能撤(另设一条验)
        let kid = s.users.create("小朋友").unwrap();
        assert!(!s.jobs.cancel(kid.id, j.id).unwrap(), "无关的人撤不了");
        assert!(s.jobs.cancel(owner.id, j.id).unwrap(), "发起人可撤");
        let j2 = s.jobs.add_for(dad.id, Some(owner.id), 7, "复查", 9000, "once").unwrap();
        assert!(s.jobs.cancel(dad.id, j2.id).unwrap(), "收件人可撤");
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
