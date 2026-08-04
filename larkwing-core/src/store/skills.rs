//! 技能(工作手册):**agent 的**、恒全局 —— 指导完成某类事的做法/准则,与用户无关
//! (记忆是与用户的「约定」归人,技能是 agent 自己的手册,两者分账)。
//! 三层渐进披露全在库里:L1 索引(name + when_to_use,恒常驻前缀、无折叠)→
//! L2 正文(skill_lookup 按需取)→ L3 附录节(skill_sections,带节名再取一层)。
//! 触发的唯一定义 = skill_lookup 命中一次(skill_hits 流水,统计三数字由它现算)。
//! 内置技能(source=builtin)boot 时按 slug 刷出厂内容、enabled 保留用户状态;可关不可删。

use anyhow::Result;
use serde::Serialize;

use super::db::{m, now_ms, Db, Migration};

pub const MIGRATIONS: &[Migration] = &[m(
    "0025_skills_init",
    "CREATE TABLE skills (
        id          INTEGER PRIMARY KEY,
        slug        TEXT UNIQUE,
        name        TEXT NOT NULL,
        when_to_use TEXT NOT NULL,
        content     TEXT NOT NULL,
        enabled     INTEGER NOT NULL DEFAULT 1,
        source      TEXT NOT NULL,
        created_at  INTEGER NOT NULL,
        updated_at  INTEGER NOT NULL
    );
    CREATE TABLE skill_sections (
        id       INTEGER PRIMARY KEY,
        skill_id INTEGER NOT NULL,
        name     TEXT NOT NULL,
        content  TEXT NOT NULL,
        ord      INTEGER NOT NULL,
        UNIQUE (skill_id, name)
    );
    CREATE TABLE skill_hits (
        id       INTEGER PRIMARY KEY,
        skill_id INTEGER NOT NULL,
        conv_id  INTEGER NOT NULL,
        at       INTEGER NOT NULL
    );
    CREATE INDEX idx_skill_hits ON skill_hits(skill_id, at);",
)];

pub const SOURCE_BUILTIN: &str = "builtin";
pub const SOURCE_USER: &str = "user";

/// 触发统计的「近 N 天」窗口(UI 三数字之二)。
const RECENT_WINDOW_MS: i64 = 7 * 24 * 3600 * 1000;

#[derive(Debug, Clone, Serialize)]
pub struct Skill {
    pub id: i64,
    /// 内置稳定键(出厂数据的身份,跨版本刷新用);用户教的 = None。
    pub slug: Option<String>,
    pub name: String,
    /// 何时用(L1 索引层,随 name 恒常驻前缀)。
    pub when_to_use: String,
    /// 怎么做(L2 正文,skill_lookup 按需取)。
    pub content: String,
    pub enabled: bool,
    pub source: String,
    pub created_at: i64,
    pub updated_at: i64,
}

/// L1 索引行:装配进前缀的最小形(名称 + 何时用)。
#[derive(Debug, Clone)]
pub struct SkillIndex {
    pub name: String,
    pub when_to_use: String,
}

/// 附录节(L3):skill_lookup 带节名再取一层。
#[derive(Debug, Clone, Serialize)]
pub struct SkillSection {
    pub name: String,
    pub content: String,
}

/// UI 列表行:技能 + 触发统计三数字(总次数 / 近 7 天 / 最近触发)。
#[derive(Debug, Clone, Serialize)]
pub struct SkillWithStats {
    #[serde(flatten)]
    pub skill: Skill,
    pub total_hits: i64,
    pub recent_hits: i64,
    pub last_hit_at: Option<i64>,
    /// 附录节名(详情页折叠展示;多数技能为空)。
    pub sections: Vec<String>,
}

/// 出厂技能(assets 数据的解析形,`crate::skills_builtin` 提供)。
pub struct BuiltinSkill {
    pub slug: String,
    pub name: String,
    pub when_to_use: String,
    pub content: String,
    pub sections: Vec<(String, String)>,
}

#[derive(Clone)]
pub struct SkillRepo {
    db: Db,
}

impl SkillRepo {
    pub(super) fn new(db: Db) -> Self {
        Self { db }
    }

    /// boot 同步出厂技能:按 slug upsert(name/when_to_use/content/附录刷成出厂值,
    /// **enabled 保留用户状态**、created_at 保留首次);出厂清单里已移除的 slug 连带清掉
    /// (sections/hits 一起)。用户教的(slug IS NULL)不受影响。
    pub fn sync_builtins(&self, builtins: &[BuiltinSkill]) -> Result<()> {
        self.db.with(|c| {
            let now = now_ms();
            let tx = c.unchecked_transaction()?;
            for b in builtins {
                tx.execute(
                    "INSERT INTO skills (slug, name, when_to_use, content, enabled, source, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, 1, ?5, ?6, ?6)
                     ON CONFLICT(slug) DO UPDATE SET
                       name = excluded.name,
                       when_to_use = excluded.when_to_use,
                       content = excluded.content,
                       updated_at = excluded.updated_at",
                    rusqlite::params![b.slug, b.name, b.when_to_use, b.content, SOURCE_BUILTIN, now],
                )?;
                let id: i64 = tx.query_row(
                    "SELECT id FROM skills WHERE slug = ?1",
                    [b.slug.as_str()],
                    |r| r.get(0),
                )?;
                // 附录节全量重建(出厂数据为准,无用户可变状态)
                tx.execute("DELETE FROM skill_sections WHERE skill_id = ?1", [id])?;
                for (ord, (name, content)) in b.sections.iter().enumerate() {
                    tx.execute(
                        "INSERT INTO skill_sections (skill_id, name, content, ord) VALUES (?1, ?2, ?3, ?4)",
                        rusqlite::params![id, name, content, ord as i64],
                    )?;
                }
            }
            // 孤儿内置(本版出厂清单里没有的 slug)清掉
            let keep: Vec<&str> = builtins.iter().map(|b| b.slug.as_str()).collect();
            let mut stmt = tx.prepare(
                "SELECT id, slug FROM skills WHERE source = ?1 AND slug IS NOT NULL",
            )?;
            let existing: Vec<(i64, String)> = stmt
                .query_map([SOURCE_BUILTIN], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(stmt);
            for (id, slug) in existing {
                if !keep.contains(&slug.as_str()) {
                    tx.execute("DELETE FROM skill_sections WHERE skill_id = ?1", [id])?;
                    tx.execute("DELETE FROM skill_hits WHERE skill_id = ?1", [id])?;
                    tx.execute("DELETE FROM skills WHERE id = ?1", [id])?;
                }
            }
            tx.commit()?;
            Ok(())
        })
    }

    /// L1 装配:启用技能的索引行,id 稳定序(内置先插在前)→ 前缀字节稳定。
    pub fn list_enabled_index(&self) -> Result<Vec<SkillIndex>> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT name, when_to_use FROM skills WHERE enabled = 1 ORDER BY id",
            )?;
            let rows = stmt
                .query_map([], |r| {
                    Ok(SkillIndex { name: r.get(0)?, when_to_use: r.get(1)? })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// UI 列表:全部技能 + 统计三数字 + 附录节名。
    pub fn list_with_stats(&self) -> Result<Vec<SkillWithStats>> {
        self.db.with(|c| {
            let since = now_ms() - RECENT_WINDOW_MS;
            let mut stmt = c.prepare(
                "SELECT s.id, s.slug, s.name, s.when_to_use, s.content, s.enabled, s.source,
                        s.created_at, s.updated_at,
                        COUNT(h.id),
                        COALESCE(SUM(CASE WHEN h.at >= ?1 THEN 1 ELSE 0 END), 0),
                        MAX(h.at)
                 FROM skills s LEFT JOIN skill_hits h ON h.skill_id = s.id
                 GROUP BY s.id ORDER BY s.id",
            )?;
            let mut rows = stmt
                .query_map([since], |r| {
                    Ok(SkillWithStats {
                        skill: map_skill(r)?,
                        total_hits: r.get(9)?,
                        recent_hits: r.get(10)?,
                        last_hit_at: r.get(11)?,
                        sections: Vec::new(),
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut sec = c.prepare(
                "SELECT name FROM skill_sections WHERE skill_id = ?1 ORDER BY ord",
            )?;
            for row in &mut rows {
                row.sections = sec
                    .query_map([row.skill.id], |r| r.get(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
            }
            Ok(rows)
        })
    }

    /// lookup 定位:精确名匹配优先,其次名/时机包含查询词(大小写不敏感)。
    /// 返回技能 + 附录节名清单(导航行由工具层生成)。
    pub fn find(&self, query: &str) -> Result<Option<(Skill, Vec<String>)>> {
        self.db.with(|c| {
            let q = query.trim();
            let skill = c
                .query_row(
                    "SELECT id, slug, name, when_to_use, content, enabled, source, created_at, updated_at
                     FROM skills WHERE enabled = 1 AND name = ?1 ORDER BY id LIMIT 1",
                    [q],
                    map_skill,
                )
                .rusqlite_optional()?;
            let skill = match skill {
                Some(s) => Some(s),
                None => c
                    .query_row(
                        "SELECT id, slug, name, when_to_use, content, enabled, source, created_at, updated_at
                         FROM skills
                         WHERE enabled = 1 AND (name LIKE ?1 ESCAPE '\\' OR when_to_use LIKE ?1 ESCAPE '\\')
                         ORDER BY id LIMIT 1",
                        [format!("%{}%", super::like_escape(q))],
                        map_skill,
                    )
                    .rusqlite_optional()?,
            };
            let Some(skill) = skill else { return Ok(None) };
            let mut stmt =
                c.prepare("SELECT name FROM skill_sections WHERE skill_id = ?1 ORDER BY ord")?;
            let sections = stmt
                .query_map([skill.id], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(Some((skill, sections)))
        })
    }

    /// L3:取一个附录节的内容。
    pub fn get_section(&self, skill_id: i64, name: &str) -> Result<Option<SkillSection>> {
        self.db.with(|c| {
            c.query_row(
                "SELECT name, content FROM skill_sections WHERE skill_id = ?1 AND name = ?2",
                rusqlite::params![skill_id, name],
                |r| Ok(SkillSection { name: r.get(0)?, content: r.get(1)? }),
            )
            .rusqlite_optional()
        })
    }

    /// 用户教的技能写入:同名 user 条整体覆盖(返回旧内容供回显,§3.5 不静默吃数据);
    /// 同名撞内置 → Err(工具层话术引导换名或关内置)。
    pub fn upsert_user(&self, name: &str, when_to_use: &str, content: &str) -> Result<Option<String>> {
        self.db.with(|c| {
            let now = now_ms();
            let existing: Option<(i64, String, String)> = c
                .query_row(
                    "SELECT id, source, content FROM skills WHERE name = ?1 ORDER BY id LIMIT 1",
                    [name],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .rusqlite_optional()?;
            match existing {
                Some((_, source, _)) if source == SOURCE_BUILTIN => {
                    anyhow::bail!("已有同名的内置技能「{name}」,换个名字,或先在技能页把内置那条关掉。")
                }
                Some((id, _, old)) => {
                    c.execute(
                        "UPDATE skills SET when_to_use = ?2, content = ?3, updated_at = ?4 WHERE id = ?1",
                        rusqlite::params![id, when_to_use, content, now],
                    )?;
                    Ok(Some(old))
                }
                None => {
                    c.execute(
                        "INSERT INTO skills (slug, name, when_to_use, content, enabled, source, created_at, updated_at)
                         VALUES (NULL, ?1, ?2, ?3, 1, ?4, ?5, ?5)",
                        rusqlite::params![name, when_to_use, content, SOURCE_USER, now],
                    )?;
                    Ok(None)
                }
            }
        })
    }

    /// 删用户教的技能(连附录/流水)。返回:Ok(true)=删了;Ok(false)=查无;
    /// Err=同名是内置(删不掉,只能关)。
    pub fn remove_user(&self, name: &str) -> Result<bool> {
        self.db.with(|c| {
            let existing: Option<(i64, String)> = c
                .query_row(
                    "SELECT id, source FROM skills WHERE name = ?1 ORDER BY id LIMIT 1",
                    [name],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .rusqlite_optional()?;
            match existing {
                None => Ok(false),
                Some((_, source)) if source == SOURCE_BUILTIN => {
                    anyhow::bail!("「{name}」是内置技能,删不掉;不想用可以在技能页把它关掉。")
                }
                Some((id, _)) => {
                    delete_skill_rows(c, id)?;
                    Ok(true)
                }
            }
        })
    }

    /// UI 删除(按 id;内置拒)。
    pub fn delete_by_id(&self, id: i64) -> Result<bool> {
        self.db.with(|c| {
            let source: Option<String> = c
                .query_row("SELECT source FROM skills WHERE id = ?1", [id], |r| r.get(0))
                .rusqlite_optional()?;
            match source.as_deref() {
                None => Ok(false),
                Some(SOURCE_BUILTIN) => anyhow::bail!("内置技能不能删除,只能停用"),
                Some(_) => {
                    delete_skill_rows(c, id)?;
                    Ok(true)
                }
            }
        })
    }

    pub fn set_enabled(&self, id: i64, enabled: bool) -> Result<bool> {
        self.db.with(|c| {
            let n = c.execute(
                "UPDATE skills SET enabled = ?2, updated_at = ?3 WHERE id = ?1",
                rusqlite::params![id, enabled, now_ms()],
            )?;
            Ok(n > 0)
        })
    }

    /// 触发流水:skill_lookup 命中即记一条(触发的唯一定义)。
    pub fn record_hit(&self, skill_id: i64, conv_id: i64) -> Result<()> {
        self.db.with(|c| {
            c.execute(
                "INSERT INTO skill_hits (skill_id, conv_id, at) VALUES (?1, ?2, ?3)",
                rusqlite::params![skill_id, conv_id, now_ms()],
            )?;
            Ok(())
        })
    }

    /// 总条数(写入 backstop 用)。
    pub fn count(&self) -> Result<i64> {
        self.db
            .with(|c| c.query_row("SELECT COUNT(*) FROM skills", [], |r| r.get(0)).map_err(Into::into))
    }
}

fn delete_skill_rows(c: &rusqlite::Connection, id: i64) -> rusqlite::Result<()> {
    c.execute("DELETE FROM skill_sections WHERE skill_id = ?1", [id])?;
    c.execute("DELETE FROM skill_hits WHERE skill_id = ?1", [id])?;
    c.execute("DELETE FROM skills WHERE id = ?1", [id])?;
    Ok(())
}

fn map_skill(r: &rusqlite::Row<'_>) -> rusqlite::Result<Skill> {
    Ok(Skill {
        id: r.get(0)?,
        slug: r.get(1)?,
        name: r.get(2)?,
        when_to_use: r.get(3)?,
        content: r.get(4)?,
        enabled: r.get::<_, i64>(5)? != 0,
        source: r.get(6)?,
        created_at: r.get(7)?,
        updated_at: r.get(8)?,
    })
}

/// rusqlite `query_row` 的 Option 化(QueryReturnedNoRows → None)。
trait RusqliteOptional<T> {
    fn rusqlite_optional(self) -> Result<Option<T>>;
}

impl<T> RusqliteOptional<T> for rusqlite::Result<T> {
    fn rusqlite_optional(self) -> Result<Option<T>> {
        match self {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    fn store(tag: &str) -> Store {
        let p = std::env::temp_dir().join(format!("lw-skills-test-{}-{tag}.db", std::process::id()));
        let _ = std::fs::remove_file(&p);
        Store::open(&p).unwrap()
    }

    fn builtin(slug: &str, name: &str, content: &str) -> BuiltinSkill {
        BuiltinSkill {
            slug: slug.into(),
            name: name.into(),
            when_to_use: "测试用".into(),
            content: content.into(),
            sections: Vec::new(),
        }
    }

    #[test]
    fn sync_builtins_refreshes_content_keeps_enabled_and_prunes_orphans() {
        let s = store("sync");
        s.skills.sync_builtins(&[builtin("a", "技能甲", "v1"), builtin("b", "技能乙", "v1")]).unwrap();
        let all = s.skills.list_with_stats().unwrap();
        assert_eq!(all.len(), 2);
        let a_id = all[0].skill.id;

        // 用户关掉 a → 升级(内容刷 v2、b 移除、新增 c)→ a 内容更新但 enabled 保留,b 清掉
        s.skills.set_enabled(a_id, false).unwrap();
        s.skills.record_hit(a_id, 1).unwrap();
        s.skills
            .sync_builtins(&[builtin("a", "技能甲", "v2"), builtin("c", "技能丙", "v1")])
            .unwrap();
        let all = s.skills.list_with_stats().unwrap();
        let names: Vec<&str> = all.iter().map(|r| r.skill.name.as_str()).collect();
        assert_eq!(names, ["技能甲", "技能丙"], "b 作为孤儿被清,c 新增");
        assert_eq!(all[0].skill.content, "v2", "内容刷成出厂新值");
        assert!(!all[0].skill.enabled, "enabled 保留用户状态");
        assert_eq!(all[0].skill.id, a_id, "同 slug 同行,id 稳定");
        assert_eq!(all[0].total_hits, 1, "触发流水保留");
    }

    #[test]
    fn enabled_index_is_stable_and_excludes_disabled() {
        let s = store("index");
        s.skills.sync_builtins(&[builtin("a", "甲", "..."), builtin("b", "乙", "...")]).unwrap();
        s.skills.upsert_user("丙", "用户教的", "步骤").unwrap();
        let idx = s.skills.list_enabled_index().unwrap();
        let names: Vec<&str> = idx.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["甲", "乙", "丙"], "id 稳定序:内置在前、用户教的在后");

        let id = s.skills.list_with_stats().unwrap()[1].skill.id;
        s.skills.set_enabled(id, false).unwrap();
        let idx = s.skills.list_enabled_index().unwrap();
        assert_eq!(idx.len(), 2, "停用的不进索引");
        assert!(!idx.iter().any(|i| i.name == "乙"));
    }

    #[test]
    fn find_exact_then_fuzzy_and_disabled_hidden() {
        let s = store("find");
        s.skills.upsert_user("放歌放视频", "点播影音时", "先本地后网络").unwrap();
        // 精确名
        let (hit, _) = s.skills.find("放歌放视频").unwrap().unwrap();
        assert_eq!(hit.content, "先本地后网络");
        // 模糊:名/时机包含
        let (hit, _) = s.skills.find("放歌").unwrap().unwrap();
        assert_eq!(hit.name, "放歌放视频");
        let (hit, _) = s.skills.find("点播").unwrap().unwrap();
        assert_eq!(hit.name, "放歌放视频");
        // 查无
        assert!(s.skills.find("修水管").unwrap().is_none());
        // 停用即隐身
        let id = s.skills.list_with_stats().unwrap()[0].skill.id;
        s.skills.set_enabled(id, false).unwrap();
        assert!(s.skills.find("放歌放视频").unwrap().is_none());
    }

    #[test]
    fn user_upsert_overwrites_echoes_old_and_rejects_builtin_name() {
        let s = store("upsert");
        s.skills.sync_builtins(&[builtin("a", "放歌放视频", "出厂做法")]).unwrap();
        // 撞内置名 → 拒
        assert!(s.skills.upsert_user("放歌放视频", "x", "y").is_err());
        // 用户条:首写 None,覆盖回显旧内容
        assert!(s.skills.upsert_user("洗照片", "整理照片时", "v1").unwrap().is_none());
        let old = s.skills.upsert_user("洗照片", "整理照片时", "v2").unwrap();
        assert_eq!(old.as_deref(), Some("v1"));
    }

    #[test]
    fn remove_user_and_delete_by_id_guard_builtins() {
        let s = store("rm");
        s.skills.sync_builtins(&[builtin("a", "内置活", "...")]).unwrap();
        s.skills.upsert_user("自学的", "某时", "某法").unwrap();
        // 对话删:内置拒、用户条删净、查无 false
        assert!(s.skills.remove_user("内置活").is_err());
        assert!(s.skills.remove_user("自学的").unwrap());
        assert!(!s.skills.remove_user("自学的").unwrap());
        // UI 按 id 删:内置拒
        let bid = s.skills.list_with_stats().unwrap()[0].skill.id;
        assert!(s.skills.delete_by_id(bid).is_err());
    }

    #[test]
    fn hits_stats_and_sections_roundtrip() {
        let s = store("hits");
        let mut b = builtin("a", "网页办事", "总纲");
        b.sections = vec![("登录墙".into(), "交给用户操作".into())];
        s.skills.sync_builtins(&[b]).unwrap();
        let id = s.skills.list_with_stats().unwrap()[0].skill.id;

        s.skills.record_hit(id, 42).unwrap();
        s.skills.record_hit(id, 42).unwrap();
        let row = &s.skills.list_with_stats().unwrap()[0];
        assert_eq!(row.total_hits, 2);
        assert_eq!(row.recent_hits, 2, "刚记的都在 7 天窗口内");
        assert!(row.last_hit_at.is_some());
        assert_eq!(row.sections, ["登录墙"]);

        // L3 节
        let sec = s.skills.get_section(id, "登录墙").unwrap().unwrap();
        assert_eq!(sec.content, "交给用户操作");
        assert!(s.skills.get_section(id, "没有的节").unwrap().is_none());
        // find 带回节名
        let (_, sections) = s.skills.find("网页办事").unwrap().unwrap();
        assert_eq!(sections, ["登录墙"]);
    }
}
