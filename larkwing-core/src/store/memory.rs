use anyhow::Result;
use serde::Serialize;

use super::db::{m, now_ms, Db, Migration};

pub const MIGRATIONS: &[Migration] = &[
    m(
        "0004_memory_init",
        "CREATE TABLE memories (
            id         INTEGER PRIMARY KEY,
            user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            kind       TEXT NOT NULL,
            content    TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL
        );",
    ),
    // 记忆智能 Phase 1(PLAN §13):分层 + salience + 出处 + 上次取用时间。
    // resident = 进不进前缀(画像·常驻 vs 情节·经验·按需);salience = 强化分(用到 +1,
    // 未来衰减/升降层用,§13.3 ④);source = 出处(explicit/correction/distilled,§13.3 ⑥);
    // last_used_at = 上次取用/确认。旧行默认常驻、salience=1 → 向前兼容(行为不变)。
    m(
        "0014_memory_layering",
        "ALTER TABLE memories ADD COLUMN resident     INTEGER NOT NULL DEFAULT 1;
         ALTER TABLE memories ADD COLUMN salience     REAL    NOT NULL DEFAULT 1.0;
         ALTER TABLE memories ADD COLUMN source       TEXT    NOT NULL DEFAULT 'explicit';
         ALTER TABLE memories ADD COLUMN last_used_at INTEGER;",
    ),
];

/// 常驻·画像区预算(字符数;§13 公理 2「注得窄」)。超额的新条目自动降为按需层,
/// 镜像 briefing 的写时执法 —— 装配时无条件全装常驻层 → 前缀字节稳定。
const RESIDENT_BUDGET_CHARS: usize = 1000;

// 记忆种类(§13.4「种类 × 遗忘非对称」的 taxonomy;旧 `kind` 的认真版)。
/// 声明性事实(喜好、习惯陈述)。
pub const KIND_FACT: &str = "fact";
/// 程序性/经验(「这个家怎么做事」)。Phase 2 主用。
pub const KIND_EXPERIENCE: &str = "experience";
/// 情节(短命,提炼原料)→ 默认按需层,不进前缀。
pub const KIND_EPISODIC: &str = "episodic";
/// 身份/情感/安全(名字、家人、过敏、纪念)→ 受保护,绝不静默衰减(§13.4)。
pub const KIND_IDENTITY: &str = "identity";

/// 受保护种类:身份/情感/安全。Phase 2 的 salience 衰减/下沉**绝不**碰它们
/// ——「忘了孩子过敏花生」对一个家是背叛不是优化(§13.4)。Phase 1 先立判定。
pub fn is_protected(kind: &str) -> bool {
    matches!(kind, KIND_IDENTITY)
}

/// 该种类默认是否进常驻前缀:画像类(fact/identity/experience)默认常驻,
/// 情节类(episodic)默认按需。最终是否常驻还要过写时预算(见 `add`)。
fn default_resident(kind: &str) -> bool {
    !matches!(kind, KIND_EPISODIC)
}

/// 记忆归人(§4.7),跨场景共享。
#[derive(Debug, Clone, Serialize)]
pub struct Memory {
    pub id: i64,
    pub user_id: i64,
    pub kind: String,
    pub content: String,
    /// 是否进稳定前缀(画像·常驻层);false = 沉在按需层,靠 `recall` 取。
    pub resident: bool,
    /// 强化分:取用 +1(§13.3 ④);未来衰减/升降层依据。
    pub salience: f64,
    /// 出处:explicit(用户说记)/ correction(被纠正)/ distilled(提炼)。
    pub source: String,
    /// 上次取用/确认(unix ms);None = 自建后未再取用。
    pub last_used_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone)]
pub struct MemoryRepo {
    db: Db,
}

impl MemoryRepo {
    pub(super) fn new(db: Db) -> Self {
        Self { db }
    }

    /// 写入(归人,§4.7)。常驻与否**在写入时定**:画像类且常驻区预算未超 → 常驻,
    /// 否则降按需(镜像 briefing 写时执法 → 装配无条件全装、前缀字节稳定,§13.3 ②)。
    /// 返回 `(记忆, 是否进了常驻区)`,调用方据此如实告知用户。
    pub fn add(&self, user: i64, kind: &str, content: &str, source: &str) -> Result<(Memory, bool)> {
        self.db.with(|c| {
            let now = now_ms();
            let incoming = content.chars().count();
            // 常驻与否 + 遗忘(§13.3 ② / ④):
            // - 受保护(身份/安全)→ 永远常驻;为腾预算尽量赶走非保护常驻(赶不动也照进,保护优先于预算)
            // - 画像 / 经验 → 预算内进;超了就把「最不该留的」非保护常驻(salience 最低、其次最旧
            //   = 没被取用的)下沉腾位,腾不出 → 这条降按需
            // - 情节 → 按需
            // eviction = 降为按需(resident=0),**不是删除** —— 绝不静默蒸发(§13.4),回忆页 / recall 仍在。
            let resident = if is_protected(kind) {
                evict_non_protected_until_fits(c, user, incoming)?;
                true
            } else if default_resident(kind) {
                evict_non_protected_until_fits(c, user, incoming)?
            } else {
                false
            };
            c.execute(
                "INSERT INTO memories
                   (user_id, kind, content, resident, salience, source, last_used_at, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, 1.0, ?5, ?6, ?6, ?6)",
                rusqlite::params![user, kind, content, resident as i64, source, now],
            )?;
            let mem = Memory {
                id: c.last_insert_rowid(),
                user_id: user,
                kind: kind.into(),
                content: content.into(),
                resident,
                salience: 1.0,
                source: source.into(),
                last_used_at: Some(now),
                created_at: now,
                updated_at: now,
            };
            Ok((mem, resident))
        })
    }

    /// 删除一条(回忆页「记错了点掉」):按 user 限定,防串号删别人的。
    pub fn delete(&self, user: i64, id: i64) -> Result<bool> {
        self.db.with(|c| {
            let n = c.execute(
                "DELETE FROM memories WHERE id = ?1 AND user_id = ?2",
                rusqlite::params![id, user],
            )?;
            Ok(n > 0)
        })
    }

    /// 删除某用户的全部记忆(删家人时清理;隐私 = 人走记忆走)。
    pub fn delete_for_user(&self, user: i64) -> Result<()> {
        self.db.with(|c| {
            c.execute("DELETE FROM memories WHERE user_id = ?1", [user])?;
            Ok(())
        })
    }

    /// 回忆页用:**全部**记忆(两层都要,给用户看 / 改 / 删)。
    pub fn list(&self, user: i64) -> Result<Vec<Memory>> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT id, user_id, kind, content, resident, salience, source, last_used_at, created_at, updated_at
                 FROM memories WHERE user_id = ?1 ORDER BY id ASC",
            )?;
            let list = stmt
                .query_map([user], map_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(list)
        })
    }

    /// 进前缀用:只取**常驻·画像层**,id 升序 → 字节稳定(§13.3 ② / §4.8)。
    /// 写时已执法预算,这里无条件全取(同 briefing 立场)。
    pub fn list_resident(&self, user: i64) -> Result<Vec<Memory>> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT id, user_id, kind, content, resident, salience, source, last_used_at, created_at, updated_at
                 FROM memories WHERE user_id = ?1 AND resident = 1 ORDER BY id ASC",
            )?;
            let list = stmt
                .query_map([user], map_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(list)
        })
    }

    /// 按需检索(`recall` 工具,§13.3 ③):匹配内容 / 种类;**命中即强化**
    /// (salience +1、刷新 last_used,§13.3 ④「用到的留在高层」)。返回命中(强化前的快照)。
    pub fn recall(&self, user: i64, query: &str) -> Result<Vec<Memory>> {
        self.db.with(|c| {
            let now = now_ms();
            let pat = format!("%{query}%");
            let hits: Vec<Memory> = {
                let mut stmt = c.prepare(
                    "SELECT id, user_id, kind, content, resident, salience, source, last_used_at, created_at, updated_at
                     FROM memories
                     WHERE user_id = ?1 AND (content LIKE ?2 OR kind LIKE ?2)
                     ORDER BY id ASC",
                )?;
                let rows = stmt
                    .query_map(rusqlite::params![user, pat], map_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                rows
            };
            for hit in &hits {
                c.execute(
                    "UPDATE memories SET salience = salience + 1.0, last_used_at = ?2, updated_at = ?2
                     WHERE id = ?1",
                    rusqlite::params![hit.id, now],
                )?;
            }
            Ok(hits)
        })
    }

    /// 提炼写入(§13.6 Phase 3,**保守**):source='distilled' 且**永远进按需层**(resident=0)
    /// —— 模型的提炼是猜测,不让它自动污染前缀,得靠 recall 复用/用户确认才算数;不触发驱逐
    /// (本就非常驻)。绝不替代/删除用户原记忆 —— 只增不删(§13.4)。
    pub fn add_distilled(&self, user: i64, kind: &str, content: &str) -> Result<Memory> {
        self.db.with(|c| {
            let now = now_ms();
            c.execute(
                "INSERT INTO memories
                   (user_id, kind, content, resident, salience, source, last_used_at, created_at, updated_at)
                 VALUES (?1, ?2, ?3, 0, 1.0, 'distilled', ?4, ?4, ?4)",
                rusqlite::params![user, kind, content, now],
            )?;
            Ok(Memory {
                id: c.last_insert_rowid(),
                user_id: user,
                kind: kind.into(),
                content: content.into(),
                resident: false,
                salience: 1.0,
                source: "distilled".into(),
                last_used_at: Some(now),
                created_at: now,
                updated_at: now,
            })
        })
    }

    /// 近重复判定(提炼去重 §13.3 ⑤ 的保守版):内容相等或互相包含即视为「已有」,跳过不再落。
    /// 宁可漏记(下次再提炼),不重复污染。
    pub fn has_similar(&self, user: i64, content: &str) -> Result<bool> {
        let needle = content.trim();
        if needle.is_empty() {
            return Ok(true); // 空内容当「已有」直接跳过
        }
        Ok(self.list(user)?.iter().any(|m| {
            let e = m.content.trim();
            !e.is_empty() && (e == needle || e.contains(needle) || needle.contains(e))
        }))
    }

    /// 常驻区现有字数(写时预算执法用)。
    pub fn resident_chars(&self, user: i64) -> Result<usize> {
        self.db.with(|c| resident_chars_conn(c, user))
    }
}

/// 常驻区字符数(SQLite `LENGTH` 对 TEXT 返回字符数,非字节)。
fn resident_chars_conn(c: &rusqlite::Connection, user: i64) -> Result<usize> {
    let total: i64 = c.query_row(
        "SELECT COALESCE(SUM(LENGTH(content)), 0) FROM memories WHERE user_id = ?1 AND resident = 1",
        [user],
        |r| r.get(0),
    )?;
    Ok(total.max(0) as usize)
}

/// 腾常驻预算(§13.3 ④「下沉」):把「最不该留的」非保护常驻 —— salience 最低、并列时最旧
/// (= 用到的留下、没被取用的下沉)—— 降为按需,直到新内容(`incoming` 字符)能装下。
/// 返回是否装下了(false = 没有可降的非保护常驻、仍超额)。
/// **受保护(身份/安全)绝不被赶(§13.4);降级 = resident=0,不是删除(绝不静默蒸发)。**
/// 注:`kind != KIND_IDENTITY` 即「非保护」(当前仅身份类受保护,与 `is_protected` 同步)。
fn evict_non_protected_until_fits(c: &rusqlite::Connection, user: i64, incoming: usize) -> Result<bool> {
    use rusqlite::OptionalExtension;
    loop {
        if resident_chars_conn(c, user)? + incoming <= RESIDENT_BUDGET_CHARS {
            return Ok(true);
        }
        let victim: Option<i64> = c
            .query_row(
                "SELECT id FROM memories
                 WHERE user_id = ?1 AND resident = 1 AND kind != ?2
                 ORDER BY salience ASC, id ASC LIMIT 1",
                rusqlite::params![user, KIND_IDENTITY],
                |r| r.get(0),
            )
            .optional()?;
        match victim {
            Some(id) => {
                c.execute("UPDATE memories SET resident = 0 WHERE id = ?1", [id])?;
            }
            None => return Ok(false),
        }
    }
}

fn map_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Memory> {
    Ok(Memory {
        id: r.get(0)?,
        user_id: r.get(1)?,
        kind: r.get(2)?,
        content: r.get(3)?,
        resident: r.get::<_, i64>(4)? != 0,
        salience: r.get(5)?,
        source: r.get(6)?,
        last_used_at: r.get(7)?,
        created_at: r.get(8)?,
        updated_at: r.get(9)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    fn store(tag: &str) -> (Store, i64) {
        let dir = std::env::temp_dir().join(format!("lw-mem-test-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _ = std::fs::remove_file(dir.join("t.db"));
        let store = Store::open(&dir.join("t.db")).unwrap();
        let me = store.users.ensure_default_user().unwrap();
        (store, me.id)
    }

    #[test]
    fn delete_is_scoped_to_user() {
        let (store, me) = store("delete");
        let (m, _) = store.memory.add(me, KIND_FACT, "对花生过敏", "explicit").unwrap();
        assert!(!store.memory.delete(me + 99, m.id).unwrap(), "别人删不动我的记忆");
        assert_eq!(store.memory.list(me).unwrap().len(), 1);
        assert!(store.memory.delete(me, m.id).unwrap());
        assert!(store.memory.list(me).unwrap().is_empty());
        assert!(!store.memory.delete(me, m.id).unwrap(), "重复删 = false,不报错");
    }

    #[test]
    fn episodic_goes_on_demand_facts_go_resident() {
        let (store, me) = store("layer");
        let (_, r1) = store.memory.add(me, KIND_FACT, "用户不吃香菜", "explicit").unwrap();
        let (_, r2) = store.memory.add(me, KIND_EPISODIC, "今天聊到周末想出去玩", "explicit").unwrap();
        assert!(r1, "画像类(fact)默认进常驻");
        assert!(!r2, "情节类(episodic)默认沉按需层");
        // 进前缀的只有常驻层
        let resident = store.memory.list_resident(me).unwrap();
        assert_eq!(resident.len(), 1);
        assert_eq!(resident[0].content, "用户不吃香菜");
        // 回忆页看到全部
        assert_eq!(store.memory.list(me).unwrap().len(), 2);
    }

    #[test]
    fn over_budget_evicts_least_used_not_the_newcomer() {
        let (store, me) = store("budget");
        let pad = "九".repeat(295);
        let mk = |n: i64| format!("第{n}条{pad}"); // 每条 ~298 字,预算 1000 → 容 3 条
        let mut ids = vec![];
        for n in 0..3 {
            let (m, r) = store.memory.add(me, KIND_FACT, &mk(n), "explicit").unwrap();
            assert!(r, "预算内 → 常驻");
            ids.push(m.id);
        }
        // 第 4 条超预算:不是简单拒绝新人,而是赶走「最不该留的」非保护常驻
        //(salience 相等 → 最旧 = 第0条),新来的留下(§13.3 ④ 用到的留、没用的下沉)
        let (m4, r4) = store.memory.add(me, KIND_FACT, &mk(3), "explicit").unwrap();
        assert!(r4, "腾位后新条目进常驻");
        let resident: Vec<i64> =
            store.memory.list_resident(me).unwrap().iter().map(|m| m.id).collect();
        assert_eq!(resident.len(), 3, "常驻区仍被预算封在 3 条(前缀有界)");
        assert!(!resident.contains(&ids[0]), "最旧、没被取用的那条被下沉");
        assert!(resident.contains(&m4.id), "新条目在常驻区");
        assert_eq!(store.memory.list(me).unwrap().len(), 4, "下沉 = 降级不是删除");
    }

    #[test]
    fn reinforced_resident_survives_eviction() {
        let (store, me) = store("evict-sal");
        let pad = "九".repeat(295);
        let mk = |n: i64| format!("第{n}条{pad}");
        let (m0, _) = store.memory.add(me, KIND_FACT, &mk(0), "explicit").unwrap();
        let (m1, _) = store.memory.add(me, KIND_FACT, &mk(1), "explicit").unwrap();
        store.memory.add(me, KIND_FACT, &mk(2), "explicit").unwrap(); // 3 条满
        // 取用「第0条」→ salience 升,成了最该留的;第1条最旧且 salience 最低
        store.memory.recall(me, "第0条").unwrap();
        let (m3, _) = store.memory.add(me, KIND_FACT, &mk(3), "explicit").unwrap();
        let resident: Vec<i64> =
            store.memory.list_resident(me).unwrap().iter().map(|m| m.id).collect();
        assert!(resident.contains(&m0.id), "被取用过的(高 salience)留住");
        assert!(!resident.contains(&m1.id), "最旧且没被取用的(salience 最低)下沉");
        assert!(resident.contains(&m3.id), "新来的在常驻");
    }

    #[test]
    fn distilled_lands_on_demand_and_dedup_guards() {
        let (store, me) = store("distill");
        store.memory.add(me, KIND_FACT, "用户养了只猫叫咪咪", "explicit").unwrap();
        // 提炼条目 → 按需层(不进前缀)、source=distilled、不删原记忆
        let d = store.memory.add_distilled(me, KIND_EXPERIENCE, "周末喜欢爬山").unwrap();
        assert!(!d.resident, "提炼条目永远进按需层,不污染前缀");
        assert_eq!(d.source, "distilled");
        assert!(!store.memory.list_resident(me).unwrap().iter().any(|m| m.id == d.id));
        assert_eq!(store.memory.list(me).unwrap().len(), 2, "只增不删");
        // 去重守门:与已有近重复的不再落
        assert!(store.memory.has_similar(me, "猫").unwrap(), "「猫」被已有「养了只猫叫咪咪」包含");
        assert!(store.memory.has_similar(me, "用户养了只猫叫咪咪").unwrap(), "完全相同 = 重复");
        assert!(!store.memory.has_similar(me, "喜欢喝美式").unwrap(), "不相关 = 不重复");
    }

    #[test]
    fn protected_identity_never_evicted() {
        let (store, me) = store("protect");
        let pad = "九".repeat(295);
        let (allergy, ra) =
            store.memory.add(me, KIND_IDENTITY, &format!("对花生过敏{pad}"), "explicit").unwrap();
        assert!(ra, "身份/安全类常驻");
        // 灌一堆非保护事实反复触发 eviction —— 受保护那条绝不被赶(§13.4)
        for n in 0..6 {
            store.memory.add(me, KIND_FACT, &format!("事实{n}{pad}"), "explicit").unwrap();
        }
        let resident: Vec<i64> =
            store.memory.list_resident(me).unwrap().iter().map(|m| m.id).collect();
        assert!(resident.contains(&allergy.id), "受保护的身份/安全记忆绝不被下沉(§13.4)");
    }

    #[test]
    fn recall_finds_on_demand_and_reinforces() {
        let (store, me) = store("recall");
        store.memory.add(me, KIND_EPISODIC, "上次说放轻松的指纯音乐歌单", "correction").unwrap();
        store.memory.add(me, KIND_FACT, "用户养了只猫叫咪咪", "explicit").unwrap();
        // 按需取:关键词命中按需层那条
        let hits = store.memory.recall(me, "轻松").unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].content.contains("纯音乐"));
        assert_eq!(hits[0].salience, 1.0, "返回的是强化前快照");
        // 命中即强化:salience +1、last_used 刷新(§13.3 ④)
        let again = store.memory.recall(me, "轻松").unwrap();
        assert_eq!(again[0].salience, 2.0, "再次取用 salience 累加");
        assert!(again[0].last_used_at.is_some());
        // 查不到 → 空
        assert!(store.memory.recall(me, "不存在的东西").unwrap().is_empty());
    }

    #[test]
    fn protected_kind_is_identity_only() {
        assert!(is_protected(KIND_IDENTITY), "身份/安全类受保护");
        for k in [KIND_FACT, KIND_EXPERIENCE, KIND_EPISODIC, "其他"] {
            assert!(!is_protected(k), "{k} 不受保护(可衰减)");
        }
    }
}
