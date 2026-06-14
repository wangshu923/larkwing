//! 任务需知(PLAN §9):跟着**任务/能力域**走的环境知识(资源在哪、目录、家里的惯例)。
//! 与记忆**机制同构、数据分账**:小本本归人(宪法 §6),需知归 域+scope(家|个人)。
//! 一个 (domain, scope) 一行,upsert = 整体覆盖 —— 模型更新时重写该主题完整状态,
//! 最稳的更新原语(无 id 记账、无合并逻辑)。常驻与否是数据属性,预算在**写入时**执法
//! (装配无条件全装 → 前缀字节稳定)。

use anyhow::Result;
use serde::Serialize;

use super::db::{m, now_ms, Db, Migration};

pub const MIGRATIONS: &[Migration] = &[m(
    "0008_briefings_init",
    "CREATE TABLE briefings (
        id         INTEGER PRIMARY KEY,
        domain     TEXT NOT NULL,
        content    TEXT NOT NULL,
        scope      TEXT NOT NULL,
        resident   INTEGER NOT NULL DEFAULT 1,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL,
        UNIQUE (domain, scope)
    );",
)];

/// scope 词表:"home"(这个家,默认)或 "user:<id>"(个人,如爸爸的代码仓库)。
/// 多用户来了这就是现成边界。
pub fn scope_home() -> String {
    "home".into()
}

pub fn scope_user(user_id: i64) -> String {
    format!("user:{user_id}")
}

#[derive(Debug, Clone, Serialize)]
pub struct Briefing {
    pub id: i64,
    pub domain: String,
    pub content: String,
    pub scope: String,
    pub resident: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone)]
pub struct BriefingRepo {
    db: Db,
}

impl BriefingRepo {
    pub(super) fn new(db: Db) -> Self {
        Self { db }
    }

    /// upsert:同 (domain, scope) 整体覆盖(created_at 保留首次时间)。
    pub fn upsert(&self, scope: &str, domain: &str, content: &str, resident: bool) -> Result<Briefing> {
        self.db.with(|c| {
            let now = now_ms();
            c.execute(
                "INSERT INTO briefings (domain, content, scope, resident, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5)
                 ON CONFLICT(domain, scope) DO UPDATE SET
                   content = excluded.content,
                   resident = excluded.resident,
                   updated_at = excluded.updated_at",
                rusqlite::params![domain, content, scope, resident, now],
            )?;
            let row = c.query_row(
                "SELECT id, domain, content, scope, resident, created_at, updated_at
                 FROM briefings WHERE domain = ?1 AND scope = ?2",
                rusqlite::params![domain, scope],
                map_row,
            )?;
            Ok(row)
        })
    }

    pub fn remove(&self, scope: &str, domain: &str) -> Result<bool> {
        self.db.with(|c| {
            let n = c.execute(
                "DELETE FROM briefings WHERE domain = ?1 AND scope = ?2",
                rusqlite::params![domain, scope],
            )?;
            Ok(n > 0)
        })
    }

    /// 回忆页用:按 id 删(行与 (domain,scope) 一一对应)。
    pub fn remove_by_id(&self, id: i64) -> Result<bool> {
        self.db.with(|c| {
            let n = c.execute("DELETE FROM briefings WHERE id = ?1", [id])?;
            Ok(n > 0)
        })
    }

    /// 某用户视角的全部需知 = home + 个人 scope;(scope, domain) 稳定序 —— 装配进
    /// 前缀的顺序由此固定,前缀字节稳定。
    pub fn list_for(&self, user_id: i64) -> Result<Vec<Briefing>> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT id, domain, content, scope, resident, created_at, updated_at
                 FROM briefings WHERE scope IN (?1, ?2) ORDER BY scope, domain",
            )?;
            let rows = stmt
                .query_map(rusqlite::params![scope_home(), scope_user(user_id)], map_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// 常驻区现有字数(写入时预算执法用)。
    pub fn resident_chars(&self, user_id: i64) -> Result<usize> {
        Ok(self
            .list_for(user_id)?
            .iter()
            .filter(|b| b.resident)
            .map(|b| b.content.chars().count() + b.domain.chars().count())
            .sum())
    }
}

fn map_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Briefing> {
    Ok(Briefing {
        id: r.get(0)?,
        domain: r.get(1)?,
        content: r.get(2)?,
        scope: r.get(3)?,
        resident: r.get::<_, i64>(4)? != 0,
        created_at: r.get(5)?,
        updated_at: r.get(6)?,
    })
}

#[cfg(test)]
mod tests {
    use crate::store::Store;

    fn store(tag: &str) -> Store {
        let p = std::env::temp_dir()
            .join(format!("lw-brief-test-{}-{tag}.db", std::process::id()));
        let _ = std::fs::remove_file(&p);
        Store::open(&p).unwrap()
    }

    #[test]
    fn upsert_overwrites_same_domain_scope() {
        let s = store("upsert");
        let a = s.briefings.upsert("home", "media", "电影在 D:\\Movies", true).unwrap();
        let b = s.briefings.upsert("home", "media", "电影搬到 E:\\Film 了", true).unwrap();
        assert_eq!(a.id, b.id, "同主题同 scope = 同一行,整体覆盖");
        assert_eq!(a.created_at, b.created_at, "首次时间保留");
        let all = s.briefings.list_for(1).unwrap();
        assert_eq!(all.len(), 1);
        assert!(all[0].content.contains("E:"));
    }

    #[test]
    fn scopes_are_isolated_and_listed_stably() {
        let s = store("scope");
        s.briefings.upsert("user:1", "coding", "仓库在 ~/code", true).unwrap();
        s.briefings.upsert("home", "media", "电影在 NAS", true).unwrap();
        s.briefings.upsert("home", "appliance", "路由器在客厅电视柜", false).unwrap();
        s.briefings.upsert("user:2", "coding", "别人的", true).unwrap();

        let mine = s.briefings.list_for(1).unwrap();
        let keys: Vec<(String, String)> =
            mine.iter().map(|b| (b.scope.clone(), b.domain.clone())).collect();
        assert_eq!(
            keys,
            [
                ("home".to_string(), "appliance".to_string()),
                ("home".to_string(), "media".to_string()),
                ("user:1".to_string(), "coding".to_string()),
            ],
            "home 在前、域内字典序 —— 装配顺序由此恒定"
        );
        assert!(!mine.iter().any(|b| b.content == "别人的"), "user:2 不可见");
    }

    #[test]
    fn remove_and_budget_helper() {
        let s = store("rm");
        s.briefings.upsert("home", "media", "电影在NAS", true).unwrap(); // 6 + domain 5 字
        let b = s.briefings.upsert("home", "tips", "门禁密码贴冰箱", false).unwrap();
        assert_eq!(s.briefings.resident_chars(1).unwrap(), 11, "非常驻不计入预算");
        assert!(s.briefings.remove("home", "media").unwrap());
        assert!(!s.briefings.remove("home", "media").unwrap());
        assert!(s.briefings.remove_by_id(b.id).unwrap());
        assert!(s.briefings.list_for(1).unwrap().is_empty());
    }
}
