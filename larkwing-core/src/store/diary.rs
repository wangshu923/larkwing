//! 家庭日记「这些日子」(★主动关怀姊妹篇,2026-07-09 用户拍板):后台把「上次写到这次之间」
//! 这个家发生的事蒸馏成按日一两句的日记,回忆页可看可删 —— 陪伴的依恋感来自共同经历的可回顾性。
//!
//! **为什么独立成域、不进记忆系统**(同 todos / media_progress 的判断):日记是**按日历日组织的
//! 情感记录**,不是「关于人的稳定事实」(不进前缀、不参与召回、不喂回模型);它是给人看的,
//! 不是给模型用的。home 一天至多一条(全家一本,不 per-user)。
//!
//! **水位线不在这里**:写到哪天由 engine/diary.rs 经 settings `diary.covered_until` 管
//! (小状态不开新域 §6.2);本域只管存取。蒸馏触发/区间语义见 engine 侧。

use anyhow::Result;
use serde::Serialize;

use super::db::{m, now_ms, Db, Migration};

pub const MIGRATIONS: &[Migration] = &[m(
    "0021_diary_init",
    "CREATE TABLE diary (
        id          INTEGER PRIMARY KEY,
        date        TEXT    NOT NULL UNIQUE,
        content     TEXT    NOT NULL,
        created_at  INTEGER NOT NULL
    );",
)];

/// 一天的日记(date = 本地日 'YYYY-MM-DD';home 共有,无归人维度)。
#[derive(Debug, Clone, Serialize)]
pub struct DiaryEntry {
    pub id: i64,
    pub date: String,
    pub content: String,
    pub created_at: i64,
}

#[derive(Clone)]
pub struct DiaryRepo {
    db: Db,
}

impl DiaryRepo {
    pub(super) fn new(db: Db) -> Self {
        Self { db }
    }

    /// 记某天的日记;该天已有则不覆盖(蒸馏正常沿水位线只前进、不会重写;用户删过的
    /// 日子区间也不会回去,IGNORE 是安全默认)。返回是否真插入。
    pub fn upsert(&self, date: &str, content: &str) -> Result<bool> {
        self.db.with(|c| {
            let n = c.execute(
                "INSERT OR IGNORE INTO diary (date, content, created_at) VALUES (?1, ?2, ?3)",
                rusqlite::params![date, content, now_ms()],
            )?;
            Ok(n > 0)
        })
    }

    /// 日记流(日期新→旧,限量),回忆页「这些日子」用。
    pub fn list(&self, limit: usize) -> Result<Vec<DiaryEntry>> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT id, date, content, created_at FROM diary
                 ORDER BY date DESC LIMIT ?1",
            )?;
            let rows = stmt.query_map(rusqlite::params![limit as i64], |r| {
                Ok(DiaryEntry {
                    id: r.get(0)?,
                    date: r.get(1)?,
                    content: r.get(2)?,
                    created_at: r.get(3)?,
                })
            })?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
    }

    /// 删掉一天(回忆页右键;用户不想留的日子)。返回是否命中。
    pub fn delete(&self, id: i64) -> Result<bool> {
        self.db.with(|c| {
            let n = c.execute("DELETE FROM diary WHERE id=?1", rusqlite::params![id])?;
            Ok(n > 0)
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::store::Store;

    fn store(tag: &str) -> Store {
        let p = std::env::temp_dir().join(format!("lw-diary-test-{}-{tag}.db", std::process::id()));
        let _ = std::fs::remove_file(&p);
        Store::open(&p).unwrap()
    }

    #[test]
    fn upsert_once_per_day_and_lists_newest_first() {
        let s = store("upsert");
        assert!(s.diary.upsert("2026-07-08", "第一次一起看了《汪汪队》。").unwrap());
        assert!(s.diary.upsert("2026-07-09", "把车年检的事记下了。").unwrap());
        // 同一天不覆盖(IGNORE 安全默认)
        assert!(!s.diary.upsert("2026-07-08", "另一段").unwrap());
        let all = s.diary.list(10).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].date, "2026-07-09", "日期新→旧");
        assert!(all[1].content.contains("汪汪队"), "已有的那天没被覆盖");
    }

    #[test]
    fn delete_removes_a_day() {
        let s = store("delete");
        s.diary.upsert("2026-07-08", "x").unwrap();
        let id = s.diary.list(10).unwrap()[0].id;
        assert!(s.diary.delete(id).unwrap());
        assert!(!s.diary.delete(id).unwrap(), "重复删 = 没命中");
        assert!(s.diary.list(10).unwrap().is_empty());
    }
}
