//! 回合记账:轮级 Usage → 流水落库(store::usage,一轮一行,分析的原料)→ 今日聚合点灯。
//! 钱按目录牌价估算(catalog);估不出价落 NULL 并在聚合里如实标 unpriced —— 不装懂(宪法 §4)。
//! 账本是观测,不是业务:任何失败只记日志,绝不打断聊天。

use chrono::Timelike;
use serde::Serialize;

use crate::llm::{catalog, Usage};
use crate::store::{Store, UsageRound, UsageTotals};

/// 一轮 LLM 调用的消耗摘要(TurnEvent::Usage 的 round 部分)。
#[derive(Debug, Clone, Default, Serialize)]
pub struct UsageDigest {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_hit_tokens: i64,
    /// 目录牌价估算(USD);None = 模型/价格未知,只报 token。
    pub cost_usd: Option<f64>,
    /// 本轮耗时:开流(建连)到收尾事件。
    pub elapsed_ms: i64,
    /// 首字延迟(体感秒回 §7 的关键指标);整轮没吐字 = None。
    pub ttft_ms: Option<i64>,
}

/// 历史/提醒气泡的 hover 读数(PLAN §11 D):一条 assistant 气泡 + 它那回合的累计用量。
/// 字段与前端 TurnStats 对齐(ms = 该回合 LLM 轮耗时之和,历史无端到端真值,够看)。
#[derive(Debug, Clone, Serialize)]
pub struct MsgStats {
    pub message_id: i64,
    pub ms: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_hit_tokens: i64,
    pub cost_usd: Option<f64>,
}

/// 今日累计(自然日,本地时区):usage_rounds 流水的窗口聚合。
#[derive(Debug, Clone, Default, Serialize)]
pub struct DayUsage {
    pub date: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_hit_tokens: i64,
    /// 可估价部分的累计(USD)。
    pub cost_usd: f64,
    /// 今日出现过估不出价的轮次 → cost_usd 不是全貌,UI 该把口气放软。
    pub unpriced: bool,
}

/// 一轮流水的归属(Turn 持有,engine 选定供应商时配齐)。
#[derive(Debug, Clone)]
pub struct RoundMeta {
    pub user_id: i64,
    pub conv_id: i64,
    /// 本回合应答的用户消息 id:分析按它聚轮成回合、JOIN 回提问原文。
    pub user_msg_id: i64,
    pub provider_id: String,
    pub model: String,
}

/// 一轮的计时(Turn 在流式消费时测得)。
#[derive(Debug, Clone, Copy, Default)]
pub struct RoundTiming {
    pub elapsed_ms: i64,
    pub ttft_ms: Option<i64>,
}

/// 本地自然日零点(unix 毫秒)。按"当前本地时刻 - 当日已过时长"算,不碰时区库的深水区。
fn day_start_ms() -> i64 {
    let now = chrono::Local::now();
    let tod_ms = i64::from(now.time().num_seconds_from_midnight()) * 1000
        + i64::from(now.time().nanosecond() / 1_000_000);
    now.timestamp_millis() - tod_ms
}

fn today_str() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// 读今日聚合(灯带初值/快照)。查询失败按空账返回 —— 灯带宁可暗着,不挡开机。
pub fn usage_today(store: &Store) -> DayUsage {
    let totals = store.usage.totals_since(day_start_ms()).unwrap_or_else(|e| {
        tracing::warn!("今日用量聚合失败: {e:#}");
        Default::default()
    });
    DayUsage {
        date: today_str(),
        input_tokens: totals.input_tokens,
        output_tokens: totals.output_tokens,
        cache_hit_tokens: totals.cache_hit_tokens,
        cost_usd: totals.cost_usd,
        unpriced: totals.unpriced_rounds > 0,
    }
}

/// 读会话累计(灯带"话题"段初值;之后的快照随 TurnEvent::Usage 走)。失败按空账,不挡链路。
pub fn usage_conversation(store: &Store, conv_id: i64) -> UsageTotals {
    store.usage.totals_for_conversation(conv_id).unwrap_or_else(|e| {
        tracing::warn!(conv = conv_id, "会话用量聚合失败: {e:#}");
        Default::default()
    })
}

/// 一轮收尾的记账:估价 → 流水落库 → 聚合。返回 (本轮摘要, 今日累计, 会话累计) 供 TurnEvent 点灯。
pub fn record_round(
    store: &Store,
    meta: &RoundMeta,
    usage: &Usage,
    timing: RoundTiming,
) -> (UsageDigest, DayUsage, UsageTotals) {
    let cost = catalog::est_cost_usd(&meta.model, usage);
    let digest = UsageDigest {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_hit_tokens: usage.cache_hit_tokens,
        cost_usd: cost,
        elapsed_ms: timing.elapsed_ms,
        ttft_ms: timing.ttft_ms,
    };
    let row = UsageRound {
        user_id: meta.user_id,
        conversation_id: meta.conv_id,
        user_msg_id: meta.user_msg_id,
        provider_id: meta.provider_id.clone(),
        model: meta.model.clone(),
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_hit_tokens: usage.cache_hit_tokens,
        cost_usd: cost,
        elapsed_ms: timing.elapsed_ms,
        ttft_ms: timing.ttft_ms,
    };
    if let Err(e) = store.usage.add_round(&row) {
        tracing::warn!("用量流水落库失败: {e:#}");
    }
    (digest, usage_today(store), usage_conversation(store, meta.conv_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store(name: &str) -> Store {
        let p = std::env::temp_dir().join(format!(
            "larkwing_usage_test_{}_{name}.db",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&p);
        Store::open(&p).unwrap()
    }

    fn meta(model: &str) -> RoundMeta {
        RoundMeta {
            user_id: 1,
            conv_id: 1,
            user_msg_id: 7,
            provider_id: "deepseek".into(),
            model: model.into(),
        }
    }

    fn usage(input: i64, output: i64, cache: i64) -> Usage {
        Usage { input_tokens: input, output_tokens: output, cache_hit_tokens: cache }
    }

    fn timing() -> RoundTiming {
        RoundTiming { elapsed_ms: 1500, ttft_ms: Some(400) }
    }

    #[test]
    fn rounds_persist_and_accumulate_into_today_and_conversation() {
        let store = temp_store("accumulate");
        let (round, day, _) =
            record_round(&store, &meta("deepseek-v4-pro"), &usage(1000, 500, 800), timing());
        assert_eq!(round.input_tokens, 1000);
        assert!(round.cost_usd.is_some(), "目录有价的模型必须给估价");
        assert_eq!(round.elapsed_ms, 1500);
        assert_eq!(round.ttft_ms, Some(400));
        let (_, day2, conv) =
            record_round(&store, &meta("deepseek-v4-pro"), &usage(100, 50, 0), timing());
        assert_eq!(day2.input_tokens, 1100);
        assert_eq!(day2.output_tokens, 550);
        assert_eq!(day2.cache_hit_tokens, 800);
        assert!(day2.cost_usd > day.cost_usd);
        assert!(!day2.unpriced);
        // 会话累计随轮快照(meta 的 conv_id = 1)
        assert_eq!(conv.input_tokens, 1100);
        assert_eq!(usage_conversation(&store, 1).input_tokens, 1100);
        assert_eq!(usage_conversation(&store, 2).input_tokens, 0, "别的会话不沾账");
        // 流水真在库里:全窗聚合 = 两行之和(分析的原料)
        let all = store.usage.totals_since(0).unwrap();
        assert_eq!(all.input_tokens, 1100);
        // 重启后的灯带初值与流水一致
        assert_eq!(usage_today(&store).input_tokens, 1100);
    }

    #[test]
    fn unknown_model_reports_tokens_only_and_flags_unpriced() {
        let store = temp_store("unpriced");
        let (round, day, _) =
            record_round(&store, &meta("totally-unknown-llm"), &usage(10, 5, 0), timing());
        assert!(round.cost_usd.is_none(), "估不出价不装懂");
        assert!(day.unpriced);
        assert_eq!(day.cost_usd, 0.0);
        // 后续有价轮:钱照累,unpriced 保持(今日账本已非全貌)
        let (_, day2, _) =
            record_round(&store, &meta("deepseek-v4-pro"), &usage(1000, 1000, 0), timing());
        assert!(day2.unpriced);
        assert!(day2.cost_usd > 0.0);
    }

    #[test]
    fn today_window_starts_at_local_midnight() {
        let start = day_start_ms();
        let now = chrono::Local::now().timestamp_millis();
        assert!(start <= now, "零点不在未来");
        assert!(now - start < 86_400_000, "零点距现在不足一天");
        // 空库 = 空账,日期是今天
        let store = temp_store("fresh");
        let day = usage_today(&store);
        assert_eq!(day.input_tokens, 0);
        assert_eq!(day.date, today_str());
    }
}
