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

pub const MIGRATIONS: &[Migration] = &[m(
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
)];

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

    /// 过期自清:open 且创建至今超过 `max_age_ms` 的,静默了结,免无限累积。返回清掉条数。
    /// 搭后台维护轮跑(注入 `now` 可单测)。
    pub fn expire_stale(&self, user_id: i64, now: i64, max_age_ms: i64) -> Result<usize> {
        self.db.with(|c| {
            let n = c.execute(
                "UPDATE todos SET done=1, updated_at=?2
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
}
