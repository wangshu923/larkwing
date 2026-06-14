//! 模型目录 = 数据(与"皮肤=数据、场景=数据"同一哲学)。
//!
//! 回答两个路由必答题:谁更聪明(tier,产品观点、粗分三档几乎不会标错)、
//! 谁更便宜(牌价,公开事实的快照)。原则:
//! - 模糊匹配:中转站常给模型名加前缀(`anthropic/claude-…`),按家族子串认;
//! - 未知模型 → 均衡档,路由永不因目录缺项罢工(容错铁律);
//! - 价格存疑就不装懂:没把握的条目价格留 None,记账层只报 token 不报钱。
//! 价格为编写时快照(USD / 百万 token),发版前人工校对;将来要保鲜再加远程目录刷新。

use super::Usage;

/// 能力档位:粗分三档是刻意的 —— 档位背后的映射可随版本重调而 UI/数据不变。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    /// 麻利:轻量快速,闲聊够用
    Light,
    /// 均衡:日常默认
    Balanced,
    /// 聪明:旗舰/深思
    Smart,
}

#[derive(Debug, Clone)]
pub struct ModelInfo {
    /// 家族子串(小写),按"特异在前、宽泛在后"排序参与匹配。
    pub family: &'static str,
    pub tier: Tier,
    /// USD / 百万 token;None = 价格未知,记账只报 token。
    pub in_usd_per_m: Option<f64>,
    pub out_usd_per_m: Option<f64>,
}

const fn m(
    family: &'static str,
    tier: Tier,
    in_usd_per_m: Option<f64>,
    out_usd_per_m: Option<f64>,
) -> ModelInfo {
    ModelInfo { family, tier, in_usd_per_m, out_usd_per_m }
}

/// 顺序即匹配优先级:特异条目(-flash/-mini)必须排在其宽泛家族(deepseek-v4)之前。
const CATALOG: &[ModelInfo] = &[
    // DeepSeek(默认供应商;v4 价沿 v3.2 官价代位,发版前校对)
    m("deepseek-v4-flash", Tier::Light, None, None),
    m("deepseek-v4", Tier::Balanced, Some(0.28), Some(0.42)),
    m("deepseek-chat", Tier::Balanced, Some(0.28), Some(0.42)), // 旧名,2026-07 弃用前仍可能遇到
    m("deepseek-reasoner", Tier::Smart, Some(0.28), Some(0.42)),
    // Anthropic
    m("claude-opus", Tier::Smart, Some(5.0), Some(25.0)),
    m("claude-sonnet", Tier::Smart, Some(3.0), Some(15.0)),
    m("claude-haiku", Tier::Light, Some(1.0), Some(5.0)),
    // 其他常见(经 OpenAI 兼容端点/中转可达)
    m("gpt-5-mini", Tier::Light, Some(0.25), Some(2.0)),
    m("gpt-5", Tier::Smart, Some(1.25), Some(10.0)),
    m("kimi-k2", Tier::Balanced, None, None),
    m("qwen-max", Tier::Balanced, None, None),
];

/// 模糊匹配:归一小写后,目录家族子串出现在模型 id 里即命中(吃掉中转前缀/版本后缀)。
pub fn lookup(model_id: &str) -> Option<&'static ModelInfo> {
    let id = model_id.to_ascii_lowercase();
    CATALOG.iter().find(|info| id.contains(info.family))
}

/// 兜底规则:未知模型按均衡档对待。
pub fn tier_of(model_id: &str) -> Tier {
    lookup(model_id).map(|i| i.tier).unwrap_or(Tier::Balanced)
}

/// 按目录牌价估算一轮成本(USD)。None = 模型未知或价格未知 —— 调用方只报 token,不报钱。
/// 缓存命中部分不另算折扣价(各家折扣率不一),按全价估,宁可高估不低估。
pub fn est_cost_usd(model_id: &str, usage: &Usage) -> Option<f64> {
    let info = lookup(model_id)?;
    let (input, output) = (info.in_usd_per_m?, info.out_usd_per_m?);
    Some((usage.input_tokens as f64 * input + usage.output_tokens as f64 * output) / 1_000_000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_match_eats_relay_prefixes_and_version_suffixes() {
        assert_eq!(tier_of("anthropic/claude-sonnet-4-6"), Tier::Smart);
        assert_eq!(tier_of("Claude-Haiku-4-5-20251001"), Tier::Light);
        assert_eq!(tier_of("openrouter/deepseek/deepseek-v4-pro"), Tier::Balanced);
    }

    #[test]
    fn specific_entries_win_over_family_entries() {
        // -flash 必须先于 deepseek-v4 命中
        assert_eq!(tier_of("deepseek-v4-flash"), Tier::Light);
        assert_eq!(tier_of("gpt-5-mini-2026-01"), Tier::Light);
        assert_eq!(tier_of("gpt-5"), Tier::Smart);
    }

    #[test]
    fn unknown_model_defaults_to_balanced_and_no_cost() {
        assert_eq!(tier_of("totally-unknown-llm"), Tier::Balanced);
        assert!(est_cost_usd("totally-unknown-llm", &Usage::default()).is_none());
        // 目录里有但价格未知:同样不报钱
        let usage = Usage { input_tokens: 1000, output_tokens: 1000, cache_hit_tokens: 0 };
        assert!(est_cost_usd("kimi-k2", &usage).is_none());
    }

    #[test]
    fn cost_estimation_uses_listed_prices() {
        let usage =
            Usage { input_tokens: 1_000_000, output_tokens: 1_000_000, cache_hit_tokens: 0 };
        let cost = est_cost_usd("deepseek-v4-pro", &usage).unwrap();
        assert!((cost - 0.70).abs() < 1e-9, "0.28 + 0.42 = 0.70,实际 {cost}");
    }

    #[test]
    fn tier_ordering_supports_meets_need_comparison() {
        assert!(Tier::Smart > Tier::Balanced);
        assert!(Tier::Balanced > Tier::Light);
    }
}
