//! 「未了的事」小账(★主动关怀里程碑 切片2·B):记住用户提过、还没了结的打算(想做/想买/想去),
//! 让旺财跨会话顺口关心进展。
//!
//! **为什么独立成域、不进记忆系统**(同 `media_progress` §7.1 的判断,用户 2026-07-05 拍板):
//! 待办是**有生命周期**的东西(open → done → 过期自清),不是"关于人的稳定事实"。塞进 §13 记忆会
//! ① 松动「宁缺毋滥」记事准则(用户明确要保留它)② 记忆没有"办完"概念 → 办完还反复问。
//! 故另开一张小账,自带 open/done + 过期。归人(per-user,§4.7);内容超长在**工具层**退回(§3.5),
//! 这里只管存取。**绝不进前缀预算失控**:进前缀由 `list_open(limit)` 限量(调用方定)。

use anyhow::Result;
use rusqlite::OptionalExtension;
use serde::Serialize;

use super::db::{m, now_ms, Db, Migration};
use super::like_escape;

pub const MIGRATIONS: &[Migration] = &[
    m(
        "0020_todos_init",
        "CREATE TABLE todos (
        id          INTEGER PRIMARY KEY,
        user_id     INTEGER NOT NULL,
        content     TEXT    NOT NULL,
        done        INTEGER NOT NULL DEFAULT 0,
        created_at  INTEGER NOT NULL,
        updated_at  INTEGER NOT NULL
    );
    CREATE INDEX idx_todos_open ON todos(user_id, done, created_at);",
    ),
    // `expired`:这条待办是**过期自清**(1)还是**真办完/了结**(0)—— 两者原先都写 done=1,
    // 家庭日记无从区分,把 30 天过期的待办写成「办完了」= 日记里出现从未发生的完成事件
    // (2026-08-22 审计)。存量已 done 的行按「真办完」(0)处理:分不清的老数据宁可当办完,
    // 也别把用户真做过的事诬成过期(前者只是日记里多一条,后者是抹掉真实功劳)。
    m("0031_todos_expired", "ALTER TABLE todos ADD COLUMN expired INTEGER NOT NULL DEFAULT 0;"),
];

/// 家庭日记取料的一条待办动静:(内容, 是否 done, 是否过期自清, 创建时刻, 更新时刻)。
/// expired=true 是「30 天没动被系统了结」,不是用户真办完 —— 日记据此不写「办完了」。
pub type TodoChange = (String, bool, bool, i64, i64);

/// 一条未了的事(只在 open 态进前缀;done 后不再露面)。
#[derive(Debug, Clone, Serialize)]
pub struct Todo {
    pub id: i64,
    pub content: String,
    pub created_at: i64,
}

#[derive(Clone)]
pub struct TodoRepo {
    db: Db,
}

impl TodoRepo {
    /// 删某人的全部待办(删家人时连带清 —— users.id 会复用,不清则新家人继承旧待办)。
    pub fn delete_for_user(&self, user_id: i64) -> Result<usize> {
        self.db.with(|c| Ok(c.execute("DELETE FROM todos WHERE user_id = ?1", [user_id])?))
    }

    pub(super) fn new(db: Db) -> Self {
        Self { db }
    }

    /// 记一件未了的事;已有**完全相同**内容的 open 条目则不重复记(返回既有 id)。
    pub fn add(&self, user_id: i64, content: &str) -> Result<i64> {
        self.db.with(|c| {
            if let Some(id) = c
                .query_row(
                    "SELECT id FROM todos WHERE user_id=?1 AND done=0 AND content=?2",
                    rusqlite::params![user_id, content],
                    |r| r.get::<_, i64>(0),
                )
                .optional()?
            {
                return Ok(id); // 已经惦记着了,别记重
            }
            let now = now_ms();
            c.execute(
                "INSERT INTO todos (user_id, content, done, created_at, updated_at)
                 VALUES (?1, ?2, 0, ?3, ?3)",
                rusqlite::params![user_id, content, now],
            )?;
            Ok(c.last_insert_rowid())
        })
    }

    /// 还开着的事(新→旧,限量);进前缀给旺财顺口关心用。
    pub fn list_open(&self, user_id: i64, limit: usize) -> Result<Vec<Todo>> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT id, content, created_at FROM todos
                 WHERE user_id=?1 AND done=0 ORDER BY created_at DESC LIMIT ?2",
            )?;
            let rows = stmt.query_map(rusqlite::params![user_id, limit as i64], |r| {
                Ok(Todo { id: r.get(0)?, content: r.get(1)?, created_at: r.get(2)? })
            })?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
    }

    /// 拖最久的一件 open(悬浮窗关怀候选用):最近记的用户自己还记得,老的才是「它帮你记着」。
    pub fn oldest_open(&self, user_id: i64) -> Result<Option<Todo>> {
        self.db.with(|c| {
            Ok(c.query_row(
                "SELECT id, content, created_at FROM todos
                 WHERE user_id=?1 AND done=0 ORDER BY created_at ASC, id ASC LIMIT 1",
                rusqlite::params![user_id],
                |r| Ok(Todo { id: r.get(0)?, content: r.get(1)?, created_at: r.get(2)? }),
            )
            .optional()?)
        })
    }

    /// 了结:用户说做完 / 不做了。先试完全相同,再退子串包含,命中最近一条 open → done。
    /// 返回是否命中(没命中 → 工具层如实告知,别静默 §3.5)。
    pub fn mark_done(&self, user_id: i64, needle: &str) -> Result<bool> {
        self.db.with(|c| {
            let now = now_ms();
            // ① 完全相同优先(模型多半照抄前缀里看到的原文)
            let exact = c.execute(
                "UPDATE todos SET done=1, updated_at=?3
                 WHERE id = (SELECT id FROM todos
                             WHERE user_id=?1 AND done=0 AND content=?2
                             ORDER BY created_at DESC LIMIT 1)",
                rusqlite::params![user_id, needle, now],
            )?;
            if exact > 0 {
                return Ok(true);
            }
            // ② 退子串包含(转义 LIKE 元字符,§6.3 复用 like_escape)
            let like = format!("%{}%", like_escape(needle));
            let n = c.execute(
                "UPDATE todos SET done=1, updated_at=?3
                 WHERE id = (SELECT id FROM todos
                             WHERE user_id=?1 AND done=0 AND content LIKE ?2 ESCAPE '\\'
                             ORDER BY created_at DESC LIMIT 1)",
                rusqlite::params![user_id, like, now],
            )?;
            Ok(n > 0)
        })
    }

    /// 回忆页勾掉一件事(办完 / 不用了):按 (user,id) 限定了结,勾不到别人的。
    /// 返回是否命中(false = 不存在或已了结)。与 `mark_done` 同语义,只是 UI 按 id 直达。
    pub fn close(&self, user_id: i64, id: i64) -> Result<bool> {
        self.db.with(|c| {
            let n = c.execute(
                "UPDATE todos SET done=1, updated_at=?3 WHERE user_id=?1 AND id=?2 AND done=0",
                rusqlite::params![user_id, id, now_ms()],
            )?;
            Ok(n > 0)
        })
    }

    /// 区间内有动静的待办(创建或了结落在 `[from_ms, to_ms)`,全家不分人):家庭日记取料用。
    /// 返回 (content, done, expired, created_at, updated_at)。`expired`=过期自清(≠真办完),
    /// 日记据此不把过期写成「办完了」。
    pub fn changed_between(&self, from_ms: i64, to_ms: i64) -> Result<Vec<TodoChange>> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT content, done, expired, created_at, updated_at FROM todos
                 WHERE (created_at >= ?1 AND created_at < ?2)
                    OR (updated_at >= ?1 AND updated_at < ?2)
                 ORDER BY updated_at ASC",
            )?;
            let rows = stmt.query_map(rusqlite::params![from_ms, to_ms], |r| {
                Ok((
                    r.get(0)?,
                    r.get::<_, i64>(1)? != 0,
                    r.get::<_, i64>(2)? != 0,
                    r.get(3)?,
                    r.get(4)?,
                ))
            })?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
    }

    /// 过期自清:open 且创建至今超过 `max_age_ms` 的,静默了结,免无限累积。返回清掉条数。
    /// 搭后台维护轮跑(注入 `now` 可单测)。**标 `expired=1`**(区别于真办完的 done):
    /// 家庭日记据此不把过期误写成「办完了」(2026-08-22 审计)。
    pub fn expire_stale(&self, user_id: i64, now: i64, max_age_ms: i64) -> Result<usize> {
        self.db.with(|c| {
            let n = c.execute(
                "UPDATE todos SET done=1, expired=1, updated_at=?2
                 WHERE user_id=?1 AND done=0 AND ?2 - created_at > ?3",
                rusqlite::params![user_id, now, max_age_ms],
            )?;
            Ok(n)
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::store::Store;

    fn store(tag: &str) -> Store {
        let p = std::env::temp_dir().join(format!("lw-todos-test-{}-{tag}.db", std::process::id()));
        let _ = std::fs::remove_file(&p);
        Store::open(&p).unwrap()
    }

    #[test]
    fn add_dedups_open_and_lists_newest_first() {
        let s = store("add");
        let a = s.todos.add(1, "给妈妈买生日礼物").unwrap();
        let b = s.todos.add(1, "把书房收拾了").unwrap();
        // 完全相同的 open 不重复记 → 返回既有 id
        assert_eq!(s.todos.add(1, "给妈妈买生日礼物").unwrap(), a);
        let open = s.todos.list_open(1, 10).unwrap();
        assert_eq!(open.len(), 2, "两件事,去重没多");
        assert_eq!(open[0].id, b, "新→旧");
        // 归人隔离
        assert!(s.todos.list_open(2, 10).unwrap().is_empty());
    }

    #[test]
    fn oldest_open_picks_first_recorded_and_skips_done() {
        let s = store("oldest");
        let a = s.todos.add(1, "给车做年检").unwrap();
        s.todos.add(1, "回小王电话").unwrap();
        assert_eq!(s.todos.oldest_open(1).unwrap().unwrap().id, a, "拖最久的在前");
        // 最老那件了结后,轮到下一件;全了结 → None;归人隔离
        assert!(s.todos.close(1, a).unwrap());
        assert_eq!(s.todos.oldest_open(1).unwrap().unwrap().content, "回小王电话");
        assert!(s.todos.oldest_open(2).unwrap().is_none());
    }

    #[test]
    fn mark_done_exact_then_substring_then_miss() {
        let s = store("done");
        s.todos.add(1, "给妈妈买生日礼物").unwrap();
        // 子串命中("买生日礼物" ⊂ 原文)
        assert!(s.todos.mark_done(1, "买生日礼物").unwrap());
        assert!(s.todos.list_open(1, 10).unwrap().is_empty(), "了结后不再 open");
        // 再了结 = 没有可结的
        assert!(!s.todos.mark_done(1, "买生日礼物").unwrap());
        // 完全不沾边 = miss
        s.todos.add(1, "练字").unwrap();
        assert!(!s.todos.mark_done(1, "报税").unwrap());
    }

    #[test]
    fn close_by_id_scoped_to_user() {
        let s = store("close");
        let id = s.todos.add(1, "给自行车打气").unwrap();
        // 别人勾不到我的
        assert!(!s.todos.close(2, id).unwrap());
        assert_eq!(s.todos.list_open(1, 10).unwrap().len(), 1);
        // 本人勾掉 → 不再 open;重复勾 = 没命中
        assert!(s.todos.close(1, id).unwrap());
        assert!(s.todos.list_open(1, 10).unwrap().is_empty());
        assert!(!s.todos.close(1, id).unwrap());
    }

    #[test]
    fn expire_stale_closes_by_age() {
        let s = store("expire");
        s.todos.add(1, "一件很久没动的事").unwrap();
        // 用暴露的 created_at 注入 now,不碰私有 db;阈值 10s
        let t = s.todos.list_open(1, 10).unwrap()[0].created_at;
        // 还没到期(now-created < 阈值)→ 不清
        assert_eq!(s.todos.expire_stale(1, t + 1_000, 10_000).unwrap(), 0);
        assert_eq!(s.todos.list_open(1, 10).unwrap().len(), 1);
        // 到期 → 静默了结
        assert_eq!(s.todos.expire_stale(1, t + 20_000, 10_000).unwrap(), 1);
        assert!(s.todos.list_open(1, 10).unwrap().is_empty());
    }

    /// **过期自清 ≠ 办完(2026-08-22 审计)**:两者原先都写 done=1,家庭日记把 30 天过期的
    /// 待办写成「办完了」= 从未发生的完成事件。`changed_between` 的 expired 位要区分开。
    #[test]
    fn expire_is_distinguishable_from_real_done() {
        let s = store("expire-vs-done");
        s.todos.add(1, "真办完的事").unwrap();
        s.todos.add(1, "过期的事").unwrap();
        assert!(s.todos.mark_done(1, "真办完的事").unwrap()); // done=1, expired=0
        // 让「过期的事」到期:用它的 created_at 往后推足够久当 now(不碰私有 now_ms)
        let base = s.todos.list_open(1, 10).unwrap()[0].created_at;
        let now = base + 40 * 24 * 3600 * 1000;
        s.todos.expire_stale(1, now, 30 * 24 * 3600 * 1000).unwrap(); // done=1, expired=1

        let rows = s.todos.changed_between(0, now + 1000).unwrap();
        let real_done: Vec<&str> = rows
            .iter()
            .filter(|(_, done, expired, ..)| *done && !*expired)
            .map(|(c, ..)| c.as_str())
            .collect();
        assert_eq!(real_done, vec!["真办完的事"], "只有真办完的才算完成,过期的不算");
        // 过期那条确实 done 了(不再 open),只是标了 expired
        assert!(rows.iter().any(|(c, done, expired, ..)| c == "过期的事" && *done && *expired));
    }
}
