//! 用量域:LLM 轮级流水 + 余额快照。分析的原料 —— 一轮一行只进不改,余额变了才记。
//! 刻意不挂外键:会话可以删,账不能跟着消失(钱已经花了,账本要如实)。

use anyhow::Result;
use rusqlite::OptionalExtension;
use serde::Serialize;

use super::db::{m, now_ms, Db, Migration};

pub const MIGRATIONS: &[Migration] = &[
    m(
        "0006_usage_init",
        "CREATE TABLE usage_rounds (
            id               INTEGER PRIMARY KEY AUTOINCREMENT,
            user_id          INTEGER NOT NULL,
            conversation_id  INTEGER NOT NULL,
            provider_id      TEXT NOT NULL,
            model            TEXT NOT NULL,
            input_tokens     INTEGER NOT NULL,
            output_tokens    INTEGER NOT NULL,
            cache_hit_tokens INTEGER NOT NULL,
            cost_usd         REAL,
            created_at       INTEGER NOT NULL
        );
        CREATE INDEX idx_usage_rounds_time ON usage_rounds(created_at);
        CREATE INDEX idx_usage_rounds_conv ON usage_rounds(conversation_id);
        CREATE TABLE balance_snapshots (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            provider_id TEXT NOT NULL,
            currency    TEXT NOT NULL,
            amount      TEXT NOT NULL,
            created_at  INTEGER NOT NULL
        );
        CREATE INDEX idx_balance_snapshots_time ON balance_snapshots(created_at);",
    ),
    // 时间维度(体感秒回 §7 的观测原料)+ 回合锚点:
    // elapsed_ms = 开流到收尾;ttft_ms = 首字延迟(没吐过字 = NULL);
    // user_msg_id = 本回合应答的用户消息 id,分析按它聚轮成回合、JOIN 回提问原文。
    m(
        "0007_usage_timing",
        "ALTER TABLE usage_rounds ADD COLUMN elapsed_ms INTEGER NOT NULL DEFAULT 0;
        ALTER TABLE usage_rounds ADD COLUMN ttft_ms INTEGER;
        ALTER TABLE usage_rounds ADD COLUMN user_msg_id INTEGER;",
    ),
];

/// 一轮 LLM 调用的流水(插入入参;created_at 由库统一盖章)。
/// cost_usd None = 牌价估不出 —— 落 NULL,聚合时如实标 unpriced。
#[derive(Debug, Clone)]
pub struct UsageRound {
    pub user_id: i64,
    pub conversation_id: i64,
    /// 本回合应答的用户消息 id(回合锚点)。
    pub user_msg_id: i64,
    pub provider_id: String,
    pub model: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_hit_tokens: i64,
    pub cost_usd: Option<f64>,
    /// 本轮耗时:开流(建连)到收尾事件。
    pub elapsed_ms: i64,
    /// 首字延迟(第一个增量事件);整轮没吐字 = None。
    pub ttft_ms: Option<i64>,
}

/// 一个聚合窗口(时间窗/会话)的合计。直接过桥给前端,字段即 wire 形。
#[derive(Debug, Clone, Default, Serialize)]
pub struct UsageTotals {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_hit_tokens: i64,
    /// 可估价轮次的合计(USD);unpriced_rounds > 0 时它不是全貌。
    pub cost_usd: f64,
    pub unpriced_rounds: i64,
}

/// 一个回合(按 user_msg_id 聚合)的读数:历史/提醒气泡 hover 读数的数据源(PLAN §11 D)。
/// 工具回合多轮 → tokens/耗时累加;cost 该回合有不可估价轮则整笔不报(与气泡 stats 同口径)。
#[derive(Debug, Clone)]
pub struct TurnRollup {
    pub user_msg_id: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_hit_tokens: i64,
    pub cost_usd: Option<f64>,
    pub elapsed_ms: i64,
}

#[derive(Clone)]
pub struct UsageRepo {
    db: Db,
}

impl UsageRepo {
    pub(super) fn new(db: Db) -> Self {
        Self { db }
    }

    pub fn add_round(&self, row: &UsageRound) -> Result<()> {
        self.insert_round_at(row, now_ms())
    }

    /// created_at 单列出来:窗口聚合的测试要能造"昨天的账"。
    fn insert_round_at(&self, row: &UsageRound, created_at: i64) -> Result<()> {
        self.db.with(|c| {
            c.execute(
                "INSERT INTO usage_rounds (user_id, conversation_id, user_msg_id, provider_id,
                    model, input_tokens, output_tokens, cache_hit_tokens, cost_usd,
                    elapsed_ms, ttft_ms, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                rusqlite::params![
                    row.user_id,
                    row.conversation_id,
                    row.user_msg_id,
                    row.provider_id,
                    row.model,
                    row.input_tokens,
                    row.output_tokens,
                    row.cache_hit_tokens,
                    row.cost_usd,
                    row.elapsed_ms,
                    row.ttft_ms,
                    created_at,
                ],
            )?;
            Ok(())
        })
    }

    /// since_ms(含)以来的聚合;"今日"= 调用方传本地零点。
    pub fn totals_since(&self, since_ms: i64) -> Result<UsageTotals> {
        self.totals_where("created_at >= ?1", since_ms)
    }

    /// 单个会话的累计(灯带"话题"段;重启不丢,切话题跟着切)。
    pub fn totals_for_conversation(&self, conv_id: i64) -> Result<UsageTotals> {
        self.totals_where("conversation_id = ?1", conv_id)
    }

    /// 该会话每个回合的聚合读数(按 user_msg_id 分组);engine 再映射到 assistant 气泡。
    pub fn rounds_by_turn(&self, conv_id: i64) -> Result<Vec<TurnRollup>> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT user_msg_id, COALESCE(SUM(input_tokens),0), COALESCE(SUM(output_tokens),0),
                        COALESCE(SUM(cache_hit_tokens),0), SUM(cost_usd),
                        COALESCE(SUM(cost_usd IS NULL),0), COALESCE(SUM(elapsed_ms),0)
                 FROM usage_rounds WHERE conversation_id = ?1 GROUP BY user_msg_id",
            )?;
            let rows = stmt
                .query_map([conv_id], |r| {
                    let unpriced: i64 = r.get(5)?;
                    Ok(TurnRollup {
                        user_msg_id: r.get(0)?,
                        input_tokens: r.get(1)?,
                        output_tokens: r.get(2)?,
                        cache_hit_tokens: r.get(3)?,
                        // 有不可估价轮 → 整回合不报价(null);否则取 SUM
                        cost_usd: if unpriced > 0 { None } else { r.get(4)? },
                        elapsed_ms: r.get(6)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// cond 只来自上面两个固定常量,无注入面。
    fn totals_where(&self, cond: &str, param: i64) -> Result<UsageTotals> {
        self.db.with(|c| {
            let sql = format!(
                "SELECT COALESCE(SUM(input_tokens), 0), COALESCE(SUM(output_tokens), 0),
                        COALESCE(SUM(cache_hit_tokens), 0), COALESCE(SUM(cost_usd), 0.0),
                        COALESCE(SUM(cost_usd IS NULL), 0)
                 FROM usage_rounds WHERE {cond}"
            );
            let t = c.query_row(&sql, [param], |r| {
                Ok(UsageTotals {
                    input_tokens: r.get(0)?,
                    output_tokens: r.get(1)?,
                    cache_hit_tokens: r.get(2)?,
                    cost_usd: r.get(3)?,
                    unpriced_rounds: r.get(4)?,
                })
            })?;
            Ok(t)
        })
    }

    /// 余额快照:与最近一条同供应商的值相同就不记(只留变化点,差值即真实花费)。
    /// 返回是否真的落了一行。
    pub fn add_balance_snapshot(
        &self,
        provider_id: &str,
        currency: &str,
        amount: &str,
    ) -> Result<bool> {
        self.db.with(|c| {
            let last: Option<(String, String)> = c
                .query_row(
                    "SELECT currency, amount FROM balance_snapshots
                     WHERE provider_id = ?1 ORDER BY id DESC LIMIT 1",
                    [provider_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            if last.as_ref().is_some_and(|(c0, a0)| c0 == currency && a0 == amount) {
                return Ok(false);
            }
            c.execute(
                "INSERT INTO balance_snapshots (provider_id, currency, amount, created_at)
                 VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![provider_id, currency, amount, now_ms()],
            )?;
            Ok(true)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(name: &str) -> UsageRepo {
        let p = std::env::temp_dir().join(format!(
            "larkwing_usage_repo_test_{}_{name}.db",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&p);
        let db = Db::open(&p).unwrap();
        db.migrate(MIGRATIONS).unwrap();
        UsageRepo::new(db)
    }

    fn round(input: i64, output: i64, cost: Option<f64>) -> UsageRound {
        UsageRound {
            user_id: 1,
            conversation_id: 1,
            user_msg_id: 42,
            provider_id: "deepseek".into(),
            model: "deepseek-v4-pro".into(),
            input_tokens: input,
            output_tokens: output,
            cache_hit_tokens: input / 2,
            cost_usd: cost,
            elapsed_ms: 1234,
            ttft_ms: Some(380),
        }
    }

    #[test]
    fn totals_window_excludes_older_rows() {
        let r = repo("window");
        r.insert_round_at(&round(1000, 100, Some(0.001)), 1_000).unwrap(); // "昨天"的账
        r.add_round(&round(200, 20, Some(0.0002))).unwrap();

        let all = r.totals_since(0).unwrap();
        assert_eq!(all.input_tokens, 1200);
        let today = r.totals_since(2_000).unwrap(); // 窗口卡在老账之后
        assert_eq!(today.input_tokens, 200);
        assert_eq!(today.output_tokens, 20);
        assert!(today.cost_usd > 0.0);
        assert_eq!(today.unpriced_rounds, 0);
    }

    #[test]
    fn conversation_totals_are_isolated_per_conversation() {
        let r = repo("conv");
        r.add_round(&round(100, 10, Some(0.1))).unwrap(); // conversation_id = 1
        r.add_round(&UsageRound { conversation_id: 2, ..round(900, 90, None) }).unwrap();
        let c1 = r.totals_for_conversation(1).unwrap();
        assert_eq!(c1.input_tokens, 100);
        assert_eq!(c1.unpriced_rounds, 0);
        let c2 = r.totals_for_conversation(2).unwrap();
        assert_eq!(c2.input_tokens, 900);
        assert_eq!(c2.unpriced_rounds, 1);
        assert_eq!(r.totals_for_conversation(99).unwrap().input_tokens, 0, "没账的会话 = 空");
    }

    // 时间维度与回合锚点真的落了列(分析就靠它们 GROUP BY / JOIN)
    #[test]
    fn timing_and_turn_anchor_are_persisted() {
        let r = repo("timing");
        r.add_round(&round(100, 10, None)).unwrap();
        let (elapsed, ttft, anchor): (i64, Option<i64>, i64) = r
            .db
            .with(|c| {
                Ok(c.query_row(
                    "SELECT elapsed_ms, ttft_ms, user_msg_id FROM usage_rounds LIMIT 1",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )?)
            })
            .unwrap();
        assert_eq!(elapsed, 1234);
        assert_eq!(ttft, Some(380));
        assert_eq!(anchor, 42);
    }

    #[test]
    fn rounds_by_turn_aggregates_per_anchor_and_null_costs() {
        let r = repo("by_turn");
        // 回合 A(user_msg_id=42):两轮(工具回合),一轮有价一轮无价 → cost 整笔不报
        r.add_round(&round(100, 10, Some(0.001))).unwrap();
        r.add_round(&round(50, 5, None)).unwrap();
        // 回合 B(user_msg_id=7):一轮有价
        r.add_round(&UsageRound { user_msg_id: 7, ..round(200, 20, Some(0.002)) }).unwrap();

        let mut rows = r.rounds_by_turn(1).unwrap();
        rows.sort_by_key(|t| t.user_msg_id);
        assert_eq!(rows.len(), 2, "两个回合各一行");

        let b = &rows[0]; // user_msg_id = 7
        assert_eq!(b.user_msg_id, 7);
        assert_eq!(b.input_tokens, 200);
        assert_eq!(b.cost_usd, Some(0.002));

        let a = &rows[1]; // user_msg_id = 42,两轮累加
        assert_eq!(a.input_tokens, 150);
        assert_eq!(a.output_tokens, 15);
        assert_eq!(a.elapsed_ms, 2468, "两轮耗时累加");
        assert_eq!(a.cost_usd, None, "有不可估价轮 → 整回合不报价(与气泡同口径)");

        assert!(r.rounds_by_turn(99).unwrap().is_empty(), "没账的会话 = 空");
    }

    #[test]
    fn unpriced_rounds_are_counted_not_guessed() {
        let r = repo("unpriced");
        r.add_round(&round(100, 10, None)).unwrap();
        r.add_round(&round(100, 10, Some(0.5))).unwrap();
        let t = r.totals_since(0).unwrap();
        assert_eq!(t.unpriced_rounds, 1);
        assert!((t.cost_usd - 0.5).abs() < 1e-9, "NULL 不计钱也不猜钱");
    }

    #[test]
    fn balance_snapshots_record_changes_only() {
        let r = repo("balance");
        assert!(r.add_balance_snapshot("deepseek", "CNY", "10.00").unwrap());
        assert!(!r.add_balance_snapshot("deepseek", "CNY", "10.00").unwrap(), "没变不记");
        assert!(r.add_balance_snapshot("deepseek", "CNY", "9.84").unwrap(), "变了才记");
        // 不同供应商互不干扰
        assert!(r.add_balance_snapshot("anthropic", "USD", "10.00").unwrap());
    }
}
