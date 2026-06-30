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
    // 维护可观测化(§13.7 调阈值的前提):每轮激进维护(衰减/下沉/升层/合并/硬清)做了多少事落一行
    // (流水只进不改,§6.4 观测进库),供真实使用反推「淡出/合并是否过激」。只在 touched() 时写、省空行。
    m(
        "0016_memory_maintenance",
        "CREATE TABLE memory_maintenance (
            id         INTEGER PRIMARY KEY,
            user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            decayed    INTEGER NOT NULL,
            demoted    INTEGER NOT NULL,
            promoted   INTEGER NOT NULL,
            merged     INTEGER NOT NULL,
            expired    INTEGER NOT NULL,
            created_at INTEGER NOT NULL
        );",
    ),
];

/// 常驻·画像区预算(字符数;§13 公理 2「注得窄」)。超额的新条目自动降为按需层,
/// 镜像 briefing 的写时执法 —— 装配时无条件全装常驻层 → 前缀字节稳定。
const RESIDENT_BUDGET_CHARS: usize = 1000;

// === Phase 3 激进维护参数(§13.6 ②③;用户拍板「全量激进」2026-06-23)===
// ⚠️ 这些是 §13.7 标注的「只能真用才能调」watch-item —— 给可辩护的**起步值**,后续真实使用反推再调。
// 全部集中在此处单源(§4.8 / §4.11):改阈值只动这里。身份/安全类(identity)对下面一切**全程豁免**(§13.4)。
/// 衰减闲置宽限(毫秒):操作类距上次取用超过它,维护轮才开始扣 salience。
const DECAY_IDLE_GRACE_MS: i64 = 7 * 24 * 3600 * 1000; // 7 天
/// 每个维护轮的衰减步长(salience 扣减;recall +1 抵消 → 常被用的稳住、没人用的往下掉)。
const DECAY_STEP: f64 = 1.0;
/// 下沉 / 过期阈值:salience ≤ 它即「凉」。下沉(常驻→按需)与硬清都以它为界。
const COLD_SALIENCE: f64 = 0.0;
/// 升层阈值:按需记忆 salience ≥ 它(靠反复 recall 挣到)→ 进前缀(预算内、只挤更弱的)。
const PROMOTE_SALIENCE: f64 = 3.0;
/// 硬清过期闲置(毫秒):已下沉 + 凉 + 闲置超过它 + 非 identity → 删(必先经下沉,「先降级后删」§13.4)。
const EXPIRE_IDLE_MS: i64 = 90 * 24 * 3600 * 1000; // 90 天

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

/// 一轮激进维护做了多少事(§13.6 ②③);进日志、给 eval 断言,不面向用户。
#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct MaintenanceReport {
    /// 衰减(扣了 salience)的条数。
    pub decayed: usize,
    /// 下沉(常驻→按需)的条数。
    pub demoted: usize,
    /// 升层(按需→常驻)的条数。
    pub promoted: usize,
    /// 合并近重复删掉的条数。
    pub merged: usize,
    /// 硬清过期删掉的条数。
    pub expired: usize,
}

impl MaintenanceReport {
    /// 这轮是否动过记忆(用于日志降噪)。
    pub fn touched(&self) -> bool {
        self.decayed + self.demoted + self.promoted + self.merged + self.expired > 0
    }
}

/// 一行维护观测(回看用;§6.4 流水只进不改)。给前端/调试看「淡出·合并是否过激」(§13.7)。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MaintenanceLog {
    pub decayed: i64,
    pub demoted: i64,
    pub promoted: i64,
    pub merged: i64,
    pub expired: i64,
    pub created_at: i64,
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

    /// 后台激进维护(§13.6 ②③,用户拍板「全量激进」2026-06-23)。**确定性、用注入 `now`**(unix ms)
    /// → Mac 可单测。一轮顺序:① 衰减 → ② 下沉 → ③ 升层 → ④ 合并近重复 → ⑤ 硬清过期。
    /// **铁守(§13.4)**:身份/安全类(`KIND_IDENTITY`)对全部五步**豁免** —— 不衰减、不下沉、不合并、不删。
    /// 删除(④⑤)只发生在这里、只作用于非 identity;下沉/升层可逆;在一个事务里跑(中途崩则整轮回滚)。
    pub fn maintain(&self, user: i64, now: i64) -> Result<MaintenanceReport> {
        self.db.with(|c| {
            let tx = c.unchecked_transaction()?;
            let mut rep = MaintenanceReport::default();
            // ① 衰减:操作类(非 identity)闲置超宽限、salience>0 → 扣一步(下限 0)。
            //    闲置 = now - max(last_used_at, created_at)(老行 last_used_at 可能为空 → 退回建档时间)。
            rep.decayed = tx.execute(
                "UPDATE memories
                   SET salience = MAX(0, salience - ?3), updated_at = ?4
                 WHERE user_id = ?1 AND kind != ?2 AND salience > 0
                   AND (?4 - COALESCE(last_used_at, created_at)) > ?5",
                rusqlite::params![user, KIND_IDENTITY, DECAY_STEP, now, DECAY_IDLE_GRACE_MS],
            )?;
            // ② 下沉:常驻 + 非 identity + 凉(salience ≤ 界)→ 转按需(可逆,recall / 回忆页仍在)。
            rep.demoted = tx.execute(
                "UPDATE memories SET resident = 0, updated_at = ?3
                 WHERE user_id = ?1 AND kind != ?2 AND resident = 1 AND salience <= ?4",
                rusqlite::params![user, KIND_IDENTITY, now, COLD_SALIENCE],
            )?;
            // ③ 升层:按需 + salience 够高 → 进前缀;预算内直接进,挤也只挤「比它更弱的」常驻(§4.8 有界)。
            rep.promoted = promote_high_salience(&tx, user, now)?;
            // ④ 合并近重复:同 kind、内容互相包含 → 留更好的(explicit>distilled / 高 salience / 更完整),删另一条。
            rep.merged = merge_duplicates(&tx, user, now)?;
            // ⑤ 硬清过期:已下沉 + 凉 + 闲置超久 + 非 identity → 删(必先经下沉,「先降级后删」§13.4)。
            rep.expired = tx.execute(
                "DELETE FROM memories
                 WHERE user_id = ?1 AND kind != ?2 AND resident = 0 AND salience <= ?3
                   AND (?4 - COALESCE(last_used_at, created_at)) > ?5",
                rusqlite::params![user, KIND_IDENTITY, COLD_SALIENCE, now, EXPIRE_IDLE_MS],
            )?;
            // 落观测行(§6.4 只进不改):仅当这轮真动过记忆才记,省空行;与维护同事务(中途崩则一并回滚)。
            if rep.touched() {
                tx.execute(
                    "INSERT INTO memory_maintenance
                       (user_id, decayed, demoted, promoted, merged, expired, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    rusqlite::params![
                        user,
                        rep.decayed as i64,
                        rep.demoted as i64,
                        rep.promoted as i64,
                        rep.merged as i64,
                        rep.expired as i64,
                        now
                    ],
                )?;
            }
            tx.commit()?;
            Ok(rep)
        })
    }

    /// 最近 N 条维护观测(新→旧),供调阈值时回看「淡出/合并是否过激」(§13.7;只读流水)。
    pub fn recent_maintenance(&self, user: i64, limit: i64) -> Result<Vec<MaintenanceLog>> {
        self.db.with(|c| {
            let mut st = c.prepare(
                "SELECT decayed, demoted, promoted, merged, expired, created_at
                   FROM memory_maintenance WHERE user_id = ?1
                  ORDER BY id DESC LIMIT ?2",
            )?;
            let rows = st
                .query_map(rusqlite::params![user, limit], |r| {
                    Ok(MaintenanceLog {
                        decayed: r.get(0)?,
                        demoted: r.get(1)?,
                        promoted: r.get(2)?,
                        merged: r.get(3)?,
                        expired: r.get(4)?,
                        created_at: r.get(5)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// 纠错替换(§13.6 Phase 3 激进,用户拍板「本期开 LLM 纠错」+「explicit 也可替换」2026-06-23):
    /// 用新事实覆盖矛盾的旧记忆。找**第一条非 identity**、内容含 `replaces` 片段的记忆 → 删掉,
    /// 落新内容(`source='correction'`,继承旧条常驻位、超预算则降按需)。返回是否真替换了一条。
    /// **identity/安全类绝不被替换(§13.4)**:既不匹配旧 identity(查询滤掉),纠错产物也不冒充 identity
    /// (kind=identity 落回 fact)—— 那类必须用户亲口改。`replaces` 空 = 不做。
    pub fn supersede(&self, user: i64, replaces: &str, kind: &str, content: &str) -> Result<bool> {
        let needle = replaces.trim();
        if needle.is_empty() || content.trim().is_empty() {
            return Ok(false);
        }
        self.db.with(|c| {
            use rusqlite::OptionalExtension;
            let pat = format!("%{}%", crate::store::like_escape(needle));
            let old: Option<(i64, i64)> = c
                .query_row(
                    "SELECT id, resident FROM memories
                     WHERE user_id = ?1 AND kind != ?2 AND content LIKE ?3 ESCAPE '\\'
                     ORDER BY id ASC LIMIT 1",
                    rusqlite::params![user, KIND_IDENTITY, pat],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let Some((old_id, was_resident)) = old else {
                return Ok(false); // 没找到能定位的旧记忆 → 不替换(调用方退化成普通新增)
            };
            let now = now_ms();
            c.execute("DELETE FROM memories WHERE id = ?1", [old_id])?;
            // 纠错产物不冒充受保护类(identity 需用户亲口立);继承旧条常驻位,超预算则降按需。
            let kind = if is_protected(kind) { KIND_FACT } else { kind };
            let resident = if was_resident != 0 {
                evict_non_protected_until_fits(c, user, content.chars().count())?
            } else {
                false
            };
            c.execute(
                "INSERT INTO memories
                   (user_id, kind, content, resident, salience, source, last_used_at, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, 1.0, 'correction', ?5, ?5, ?5)",
                rusqlite::params![user, kind, content, resident as i64, now],
            )?;
            Ok(true)
        })
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

/// 腾常驻预算到能装下 `incoming` 字符的薄封装(写入 / 纠错用):任何非保护常驻都可下沉。
/// 见 `evict_to_fit` —— **先核算装得下才动手**。受保护(身份/安全)绝不被赶(§13.4)。
fn evict_non_protected_until_fits(c: &rusqlite::Connection, user: i64, incoming: usize) -> Result<bool> {
    evict_to_fit(c, user, incoming, None)
}

/// 腾常驻预算到能装下 `incoming` 字符:**先核算可腾空间、确定装得下才下沉**(否则一条都不动)。
/// 杜绝「为一个永远装不下的候选白白下沉一批常驻」的副作用(2026-06-23 reviewer 抓到:identity
/// 把预算挤满时,后续非 identity 写入 / 升层会反复下沉腾不出位的弱常驻 → 无谓的前缀流失)。
/// `victim_max_sal=Some(s)`:只下沉 salience 严格 < s 的(升层用,绝不挤掉同等 / 更强);`None`:任何非保护常驻。
/// 排序「最该走的先走」= salience 升、并列最旧。受保护(identity)绝不被赶(§13.4);降级=resident0、非删除。
/// 返回是否装下(false ⇒ 腾不出,且**未做任何下沉**)。
fn evict_to_fit(
    c: &rusqlite::Connection,
    user: i64,
    incoming: usize,
    victim_max_sal: Option<f64>,
) -> Result<bool> {
    let used = resident_chars_conn(c, user)?;
    if used + incoming <= RESIDENT_BUDGET_CHARS {
        return Ok(true); // 已够位,无需下沉
    }
    let victims: Vec<(i64, usize)> = {
        let mut stmt = c.prepare(
            "SELECT id, LENGTH(content) FROM memories
             WHERE user_id = ?1 AND resident = 1 AND kind != ?2
               AND (?3 IS NULL OR salience < ?3)
             ORDER BY salience ASC, id ASC",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![user, KIND_IDENTITY, victim_max_sal], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?.max(0) as usize))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    // 先核算:把所有可腾的都腾了还装不下 ⇒ 一条都不动、直接放弃(关键修复:无副作用地失败)。
    let freeable: usize = victims.iter().map(|(_, len)| *len).sum();
    if used.saturating_sub(freeable) + incoming > RESIDENT_BUDGET_CHARS {
        return Ok(false);
    }
    // 装得下:从最该走的开始下沉,够位即停。
    let mut used = used;
    for (id, len) in victims {
        if used + incoming <= RESIDENT_BUDGET_CHARS {
            break;
        }
        c.execute("UPDATE memories SET resident = 0 WHERE id = ?1", [id])?;
        used = used.saturating_sub(len);
    }
    Ok(true)
}

/// 升层(§13.6 ③):按需记忆 salience 够高(靠反复 recall 挣到)→ 进前缀。强者优先;预算内直接进,
/// 挤位**只挤比它弱的**非保护常驻(`Some(sal)` 限定;绝不为低分挤掉高分 → 前缀仍 §4.8 有界),
/// 挤不出就留按需(recall 仍可取,且不白白下沉别人)。distilled / 纠错产物同样靠这条凭实绩升层。
fn promote_high_salience(c: &rusqlite::Connection, user: i64, now: i64) -> Result<usize> {
    let cands: Vec<(i64, usize, f64)> = {
        let mut stmt = c.prepare(
            "SELECT id, LENGTH(content), salience FROM memories
             WHERE user_id = ?1 AND resident = 0 AND salience >= ?2
             ORDER BY salience DESC, id ASC",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![user, PROMOTE_SALIENCE], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?.max(0) as usize, r.get::<_, f64>(2)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    let mut promoted = 0;
    for (id, len, sal) in cands {
        if evict_to_fit(c, user, len, Some(sal))? {
            c.execute(
                "UPDATE memories SET resident = 1, updated_at = ?2 WHERE id = ?1",
                rusqlite::params![id, now],
            )?;
            promoted += 1;
        }
    }
    Ok(promoted)
}

/// 合并近重复(§13.6 ④):同 kind、内容互相包含(含相等)视为一条;留更好的、删另一条。
/// 幸存者吸收两条:salience 取大、常驻位取或(任一常驻则留前缀,合并不该把信息挤出前缀)。
/// **identity 绝不参与合并(§13.4)** —— 身份/安全宁可冗余也不自动删。删除只此一处与硬清(⑤)。
fn merge_duplicates(c: &rusqlite::Connection, user: i64, now: i64) -> Result<usize> {
    let rows: Vec<MergeRow> = {
        let mut stmt = c.prepare(
            "SELECT id, kind, content, resident, salience, source, last_used_at FROM memories
             WHERE user_id = ?1 AND kind != ?2 ORDER BY id ASC",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![user, KIND_IDENTITY], |r| {
                Ok(MergeRow {
                    id: r.get(0)?,
                    kind: r.get(1)?,
                    content: r.get(2)?,
                    resident: r.get::<_, i64>(3)? != 0,
                    salience: r.get(4)?,
                    source: r.get(5)?,
                    last_used_at: r.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    let mut alive: Vec<MergeRow> = Vec::with_capacity(rows.len());
    let mut removed = 0usize;
    for incoming in rows {
        let it = incoming.content.trim();
        if it.is_empty() {
            alive.push(incoming);
            continue;
        }
        let dup = alive.iter().position(|k| {
            k.kind == incoming.kind && {
                let kt = k.content.trim();
                !kt.is_empty() && (kt.contains(it) || it.contains(kt))
            }
        });
        match dup {
            Some(pos) => {
                let salience = alive[pos].salience.max(incoming.salience);
                let resident = alive[pos].resident || incoming.resident;
                // 幸存者继承两条里更新的取用时间,否则合并可能让 idle 时钟变陈、被提早衰减/硬清(reviewer)。
                let last_used = alive[pos].last_used_at.max(incoming.last_used_at);
                let keep_existing = alive[pos].better_than(&incoming);
                let (loser_id, mut survivor) =
                    if keep_existing { (incoming.id, alive[pos].clone()) } else { (alive[pos].id, incoming) };
                survivor.salience = salience;
                survivor.resident = resident;
                survivor.last_used_at = last_used;
                c.execute("DELETE FROM memories WHERE id = ?1", [loser_id])?;
                c.execute(
                    "UPDATE memories SET salience = ?2, resident = ?3, last_used_at = ?4, updated_at = ?5 WHERE id = ?1",
                    rusqlite::params![survivor.id, salience, resident as i64, last_used, now],
                )?;
                alive[pos] = survivor;
                removed += 1;
            }
            None => alive.push(incoming),
        }
    }
    Ok(removed)
}

/// 合并时的轻量行视图 + 「谁更该留」判据。
#[derive(Clone)]
struct MergeRow {
    id: i64,
    kind: String,
    content: String,
    resident: bool,
    salience: f64,
    source: String,
    last_used_at: Option<i64>,
}

impl MergeRow {
    /// 自己是否比 `other` 更值得留(依次):salience 高 > explicit/correction 胜 distilled >
    /// 内容更完整(更长)> 更早建档(id 小)。f64 不可 Ord,故逐级显式比较。
    fn better_than(&self, other: &MergeRow) -> bool {
        if self.salience != other.salience {
            return self.salience > other.salience;
        }
        let trusted = |m: &MergeRow| m.source != "distilled"; // 用户说的 / 纠错过的胜模型猜的
        if trusted(self) != trusted(other) {
            return trusted(self);
        }
        let (sl, ol) = (self.content.chars().count(), other.content.chars().count());
        if sl != ol {
            return sl > ol;
        }
        self.id <= other.id
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

    // ===== Phase 3 激进维护(§13.6 ②③;注入 now 做确定性时间)=====
    const DAY_MS: i64 = 86_400_000;

    #[test]
    fn idle_operational_decays_then_demotes_same_pass() {
        let (store, me) = store("decay");
        let (m, r) = store.memory.add(me, KIND_FACT, "偶尔提一句的事", "explicit").unwrap();
        assert!(r, "操作类默认常驻");
        // 闲置超 7 天宽限 → 一轮里 衰减(1→0)+ 下沉(salience≤0)
        let rep = store.memory.maintain(me, m.created_at + 8 * DAY_MS).unwrap();
        assert_eq!(rep.decayed, 1);
        assert_eq!(rep.demoted, 1, "掉到 0 → 下沉(同一轮 decay 在 demote 前)");
        assert!(store.memory.list_resident(me).unwrap().is_empty(), "下沉后不进前缀");
        assert_eq!(store.memory.list(me).unwrap().len(), 1, "下沉=降级不删");
    }

    #[test]
    fn fresh_operational_does_not_decay() {
        let (store, me) = store("no-decay");
        let (m, _) = store.memory.add(me, KIND_FACT, "刚记的事", "explicit").unwrap();
        let rep = store.memory.maintain(me, m.created_at + 2 * DAY_MS).unwrap();
        assert_eq!(rep.decayed, 0, "没过宽限不衰减");
        assert_eq!(rep.demoted, 0);
    }

    #[test]
    fn maintenance_logged_only_when_touched() {
        let (store, me) = store("maint-log");
        let (m, _) = store.memory.add(me, KIND_FACT, "偶尔提一句的事", "explicit").unwrap();
        // no-op 轮(没过宽限)→ 不落观测行
        store.memory.maintain(me, m.created_at + 2 * DAY_MS).unwrap();
        assert!(store.memory.recent_maintenance(me, 50).unwrap().is_empty(), "no-op 不落行");
        // touched 轮(衰减+下沉)→ 落一行,计数与 report 对得上
        let rep = store.memory.maintain(me, m.created_at + 8 * DAY_MS).unwrap();
        assert!(rep.touched());
        let logs = store.memory.recent_maintenance(me, 50).unwrap();
        assert_eq!(logs.len(), 1, "touched 落且只落一行");
        assert_eq!(logs[0].decayed, rep.decayed as i64);
        assert_eq!(logs[0].demoted, rep.demoted as i64);
    }

    #[test]
    fn identity_never_decays_demotes_or_expires() {
        let (store, me) = store("id-immune");
        let (m, _) = store.memory.add(me, KIND_IDENTITY, "女儿对花生过敏", "explicit").unwrap();
        // 200 天没碰过:操作类早被衰减下沉甚至硬清了,身份类必须纹丝不动(§13.4)
        let rep = store.memory.maintain(me, m.created_at + 200 * DAY_MS).unwrap();
        assert_eq!((rep.decayed, rep.demoted, rep.expired), (0, 0, 0));
        let resident = store.memory.list_resident(me).unwrap();
        assert!(resident.iter().any(|x| x.id == m.id), "身份类永不下沉");
        assert_eq!(resident[0].salience, 1.0, "salience 没动");
    }

    #[test]
    fn high_salience_on_demand_promotes_into_prefix() {
        let (store, me) = store("promote");
        let (m, r) = store.memory.add(me, KIND_EPISODIC, "用户最近老问减脂餐", "explicit").unwrap();
        assert!(!r, "情节类默认按需");
        store.memory.recall(me, "减脂").unwrap(); // 1→2
        store.memory.recall(me, "减脂").unwrap(); // 2→3 ≥ 升层阈值
        // now≈建档时间 → 不触发衰减;只看升层
        let rep = store.memory.maintain(me, m.created_at).unwrap();
        assert_eq!(rep.promoted, 1, "salience≥3 的按需记忆升进前缀(靠 recall 挣到)");
        assert!(store.memory.list_resident(me).unwrap().iter().any(|x| x.id == m.id));
    }

    #[test]
    fn promotion_only_evicts_weaker_residents() {
        let (store, me) = store("promote-budget");
        let pad = "九".repeat(495);
        // 两条强常驻填满预算(各 ~498 字,预算 1000 容 2 条);salience 抬高
        let (strong0, _) = store.memory.add(me, KIND_FACT, &format!("强0{pad}"), "explicit").unwrap();
        let (strong1, _) = store.memory.add(me, KIND_FACT, &format!("强1{pad}"), "explicit").unwrap();
        for _ in 0..5 {
            store.memory.recall(me, "强0").unwrap();
            store.memory.recall(me, "强1").unwrap();
        }
        // 一条按需、salience 只到 3:挤不动 salience=6 的强常驻 → 留按需
        let (weak, _) = store.memory.add(me, KIND_EPISODIC, &format!("弱{pad}"), "explicit").unwrap();
        store.memory.recall(me, "弱").unwrap();
        store.memory.recall(me, "弱").unwrap(); // 1→3
        let rep = store.memory.maintain(me, weak.created_at).unwrap();
        assert_eq!(rep.promoted, 0, "不为低分挤掉更强的常驻(前缀有界且不抖)");
        let resident: Vec<i64> = store.memory.list_resident(me).unwrap().iter().map(|m| m.id).collect();
        assert!(resident.contains(&strong0.id) && resident.contains(&strong1.id));
        assert!(!resident.contains(&weak.id));
    }

    #[test]
    fn eviction_does_not_demote_when_candidate_cannot_fit() {
        // 回归(2026-06-23 reviewer):identity 把预算挤满后,再写一条「即便挤走所有非保护常驻也装不下」
        // 的非保护记忆 —— 不得为它白白下沉既有的弱常驻(修复前的 evict 会一条条沉了再放弃)。
        let (store, me) = store("evict-nofit");
        store.memory.add(me, KIND_IDENTITY, &"九".repeat(900), "explicit").unwrap(); // 身份类强制常驻、占满
        let (small, rs) =
            store.memory.add(me, KIND_FACT, &format!("小{}", "九".repeat(48)), "explicit").unwrap(); // 49 字,900+49≤1000
        assert!(rs, "小条还塞得下 → 常驻");
        // 200 字新条:identity 900 + 200 > 1000,挤走 small(49)也不够 → 该自己降按需、且不动 small
        let (newer, rn) = store.memory.add(me, KIND_FACT, &format!("大{}", "九".repeat(199)), "explicit").unwrap();
        assert!(!rn, "装不下 → 自己降按需");
        let resident: Vec<i64> = store.memory.list_resident(me).unwrap().iter().map(|m| m.id).collect();
        assert!(resident.contains(&small.id), "装不下时不得白白下沉既有常驻(无副作用地失败)");
        assert!(!resident.contains(&newer.id));
    }

    #[test]
    fn merge_near_duplicates_keeps_trusted_and_absorbs() {
        let (store, me) = store("merge");
        let (short, _) = store.memory.add(me, KIND_FACT, "喜欢喝美式", "explicit").unwrap();
        let _long = store.memory.add_distilled(me, KIND_FACT, "喜欢喝美式咖啡,尤其早上").unwrap();
        let rep = store.memory.maintain(me, short.created_at).unwrap();
        assert_eq!(rep.merged, 1, "互相包含 → 合并成一条");
        let all = store.memory.list(me).unwrap();
        assert_eq!(all.len(), 1, "删掉一条(非 identity)");
        assert_eq!(all[0].id, short.id, "salience 同 → explicit 胜 distilled");
    }

    #[test]
    fn merge_skips_identity() {
        let (store, me) = store("merge-id");
        let (a, _) = store.memory.add(me, KIND_IDENTITY, "对花生过敏", "explicit").unwrap();
        let (b, _) = store.memory.add(me, KIND_IDENTITY, "对花生过敏,严重", "explicit").unwrap();
        let rep = store.memory.maintain(me, a.created_at).unwrap();
        assert_eq!(rep.merged, 0, "身份类宁可冗余也不自动合并(§13.4)");
        let ids: Vec<i64> = store.memory.list(me).unwrap().iter().map(|m| m.id).collect();
        assert!(ids.contains(&a.id) && ids.contains(&b.id));
    }

    #[test]
    fn cold_idle_operational_decays_demotes_then_expires_in_one_pass() {
        let (store, me) = store("expire");
        let (_m, _) = store.memory.add(me, KIND_FACT, "一条会过期的事", "explicit").unwrap();
        let created = store.memory.list(me).unwrap()[0].created_at;
        // 91 天没碰:衰减→下沉→(已下沉+凉+闲置超90天)硬清,一轮走完
        let rep = store.memory.maintain(me, created + 91 * DAY_MS).unwrap();
        assert_eq!((rep.decayed, rep.demoted, rep.expired), (1, 1, 1));
        assert!(store.memory.list(me).unwrap().is_empty(), "真删除(非 identity,先降级后删)");
    }

    #[test]
    fn supersede_replaces_matching_nonidentity() {
        let (store, me) = store("supersede");
        let (old, _) = store.memory.add(me, KIND_FACT, "用户喜欢喝美式", "explicit").unwrap();
        assert!(store.memory.supersede(me, "美式", KIND_FACT, "用户其实喜欢喝拿铁").unwrap());
        let all = store.memory.list(me).unwrap();
        assert_eq!(all.len(), 1, "删旧 + 加新 = 净 1 条");
        assert_eq!(all[0].content, "用户其实喜欢喝拿铁");
        assert_eq!(all[0].source, "correction");
        // 注:不能用 id 判旧条已删 —— SQLite 删唯一行后插入会复用 rowid;判内容才可靠。
        assert!(all.iter().all(|m| m.content != old.content), "旧内容已被替换掉");
    }

    #[test]
    fn supersede_downgrades_identity_kind_to_fact() {
        let (store, me) = store("supersede-downgrade");
        store.memory.add(me, KIND_FACT, "用户住在浦东", "explicit").unwrap();
        // LLM 想用 identity 类纠错 → 落回 fact(纠错不冒充受保护类)
        assert!(store.memory.supersede(me, "浦东", KIND_IDENTITY, "用户搬到了徐汇").unwrap());
        let all = store.memory.list(me).unwrap();
        assert_eq!(all[0].kind, KIND_FACT, "纠错产物不冒充 identity");
    }

    #[test]
    fn supersede_never_touches_identity_memory() {
        let (store, me) = store("supersede-protect");
        let (id_mem, _) = store.memory.add(me, KIND_IDENTITY, "女儿对花生过敏", "explicit").unwrap();
        assert!(!store.memory.supersede(me, "花生", KIND_FACT, "女儿不过敏了").unwrap(), "身份类不被替换");
        let all = store.memory.list(me).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, id_mem.id, "身份记忆原封不动");
        assert_eq!(all[0].content, "女儿对花生过敏");
    }

    #[test]
    fn supersede_no_match_returns_false() {
        let (store, me) = store("supersede-miss");
        store.memory.add(me, KIND_FACT, "用户养猫", "explicit").unwrap();
        assert!(!store.memory.supersede(me, "不存在的片段", KIND_FACT, "新事实").unwrap());
        assert_eq!(store.memory.list(me).unwrap().len(), 1, "没匹配 → 原样不动");
    }
}
