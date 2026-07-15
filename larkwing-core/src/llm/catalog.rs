//! 模型目录 = 数据(与"皮肤=数据、场景=数据"同一哲学)。
//!
//! 回答两个路由必答题:谁更聪明(tier,产品观点、粗分三档几乎不会标错)、
//! 谁更便宜(牌价,公开事实的快照)。原则:
//! - 模糊匹配:中转站常给模型名加前缀(`anthropic/claude-…`),按家族子串认;
//! - 未知模型 → 均衡档,路由永不因目录缺项罢工(容错铁律);
//! - 价格存疑就不装懂:没把握的条目价格留 None,记账层只报 token 不报钱。
//! 价格为编写时快照(USD / 百万 token),发版前人工校对;将来要保鲜再加远程目录刷新。

use std::sync::RwLock;

use serde::{Deserialize, Serialize};

use super::Usage;

/// 能力档位:粗分三档是刻意的 —— 档位背后的映射可随版本重调而 UI/数据不变。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    /// 麻利:轻量快速,闲聊够用
    Light,
    /// 均衡:日常默认
    Balanced,
    /// 聪明:旗舰/深思
    Smart,
}

/// 计价方式 —— **影响压缩**(该不该为省钱少留上下文),不改记账数字(记账仍按 token 估、见
/// est_cost_usd)。当前所有列出的 provider 都有前缀缓存 → 默认 `Cached`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BillingMode {
    /// 按量 + 有前缀缓存(重发上下文便宜)→ 多留(默认,DeepSeek/Anthropic/Gemini… 都是)。
    #[default]
    Cached,
    /// 按量、无缓存(每轮全价重发上下文)→ 少留、勤压。
    Uncached,
    /// 按调用次数(token 不计较)→ 多留。
    PerCall,
}

/// 用户对某个具体模型的覆盖(「高级」里改的)。键 = 用户在「模型」框填的那个 id,精确匹配
/// (不做模糊 —— 用户填什么覆盖什么)。各字段 None = 该项用目录猜测(纠错语义,非配置 §3)。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelOverride {
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<Tier>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_usd_per_m: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub out_usd_per_m: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ctx_window_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub billing: Option<BillingMode>,
    /// 能不能看图(None = 用目录猜测)。目录不认识的自架视觉模型(llava / qwen-vl 自部署)
    /// 靠它标上;目录说错了也靠它纠(2026-07-15,用户要求 robot supported_media 的对应物)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vision: Option<bool>,
}

impl ModelOverride {
    /// 没有任何覆盖字段 = 空壳(删除该条的判据,让模型回落纯目录)。
    pub fn is_empty(&self) -> bool {
        self.tier.is_none()
            && self.in_usd_per_m.is_none()
            && self.out_usd_per_m.is_none()
            && self.ctx_window_tokens.is_none()
            && self.billing.is_none()
            && self.vision.is_none()
    }
}

/// 用户覆盖层(进程级,读多写极少):engine 在 boot + 用户改后 `set_overrides` 推入,查找时先看它。
/// catalog 本身**不依赖 store**(数据由 engine 喂进来,守 §6.1);overlay 只是「目录之上的纠错层」。
static OVERRIDES: RwLock<Vec<ModelOverride>> = RwLock::new(Vec::new());

/// engine 调用:用最新覆盖表整体替换 overlay(boot / 用户保存后)。
pub fn set_overrides(overrides: Vec<ModelOverride>) {
    *OVERRIDES.write().expect("catalog overrides lock poisoned") = overrides;
}

/// 当前覆盖表快照(给设置页回读 / 测试)。
pub fn overrides() -> Vec<ModelOverride> {
    OVERRIDES.read().expect("catalog overrides lock poisoned").clone()
}

/// 按 model id 精确(大小写不敏感)取覆盖。用户填什么键什么 → 不模糊匹配。
fn override_for(model_id: &str) -> Option<ModelOverride> {
    let id = model_id.to_ascii_lowercase();
    OVERRIDES
        .read()
        .expect("catalog overrides lock poisoned")
        .iter()
        .find(|o| o.model.to_ascii_lowercase() == id)
        .cloned()
}

#[derive(Debug, Clone)]
pub struct ModelInfo {
    /// 家族子串(小写),按"特异在前、宽泛在后"排序参与匹配。
    pub family: &'static str,
    pub tier: Tier,
    /// 能不能看图(vision):false 的模型收到 image_url 会直接 400(DeepSeek 真机实锤
    /// `unknown variant image_url`)→ 兼容层把图降级成占位文本,回合不炸、模型如实说看不了。
    /// 未知模型按 false(安全:宁可少看图,不可打挂回合);真有本地视觉模型(llava 类)再登记。
    pub vision: bool,
    /// USD / 百万 token;None = 价格未知,记账只报 token。
    pub in_usd_per_m: Option<f64>,
    pub out_usd_per_m: Option<f64>,
    /// 上下文窗口(token);None = 未知(本地/未登记)→ 上下文预算回落默认值。
    /// 喂给 `engine::context::tail_budget_chars` 算尾部字数预算(大窗口装文档、小窗口防溢出)。
    pub ctx_window_tokens: Option<u32>,
}

const fn m(
    family: &'static str,
    tier: Tier,
    vision: bool,
    in_usd_per_m: Option<f64>,
    out_usd_per_m: Option<f64>,
    ctx_window_tokens: Option<u32>,
) -> ModelInfo {
    ModelInfo { family, tier, vision, in_usd_per_m, out_usd_per_m, ctx_window_tokens }
}

/// 顺序即匹配优先级:特异条目(-flash/-mini)必须排在其宽泛家族(deepseek-v4)之前。
/// 第 5 列 = 上下文窗口(token,2026-06 采集快照,全部 200K–1M;发版前可校)。
const CATALOG: &[ModelInfo] = &[
    // DeepSeek(默认供应商;V4 全系 1M 窗口。flash = 廉价档,v4 = Pro 牌价)
    m("deepseek-v4-flash", Tier::Light, false, Some(0.14), Some(0.28), Some(1_000_000)),
    m("deepseek-v4", Tier::Balanced, false, Some(1.74), Some(3.48), Some(1_000_000)), // V4-Pro 实价
    m("deepseek-chat", Tier::Balanced, false, Some(0.28), Some(0.42), Some(1_000_000)), // 旧名,2026-07-24 弃用前仍可能遇到
    m("deepseek-reasoner", Tier::Smart, false, Some(0.28), Some(0.42), Some(1_000_000)),
    // Anthropic(Opus/Sonnet 1M,Haiku 200K)
    m("claude-opus", Tier::Smart, true, Some(5.0), Some(25.0), Some(1_000_000)),
    m("claude-sonnet", Tier::Smart, true, Some(3.0), Some(15.0), Some(1_000_000)),
    m("claude-haiku", Tier::Light, true, Some(1.0), Some(5.0), Some(200_000)),
    // 其他常见(经 OpenAI 兼容端点/中转可达)
    m("gpt-5-mini", Tier::Light, true, Some(0.25), Some(2.0), Some(400_000)),
    m("gpt-5", Tier::Smart, true, Some(0.625), Some(5.0), Some(400_000)),
    // Kimi:K2.5/K2.6 起原生多模态(MoonViT,图/视频输入;2026-07 官方 API 核实),
    // 老 K2 是纯文本 —— 特异在前,免得 k2.5/k2.6 被 kimi-k2 行错杀成不看图。
    m("kimi-k2.6", Tier::Smart, true, None, None, Some(256_000)),
    m("kimi-k2.5", Tier::Smart, true, None, None, Some(256_000)),
    m("kimi-k2", Tier::Balanced, false, None, None, Some(256_000)),
    // Qwen:qwen-max 的 API 仍是纯文本;视觉/全模态是独立的 VL / Omni 系(dashscope,
    // 2026-07 核实)。窗口/价随版本浮动不装懂,留 None。
    m("qwen3-vl", Tier::Balanced, true, None, None, None),
    m("qwen-vl", Tier::Balanced, true, None, None, None),
    m("qwen3-omni", Tier::Balanced, true, None, None, None),
    m("qwen-omni", Tier::Balanced, true, None, None, None),
    m("qwen-max", Tier::Balanced, false, None, None, Some(256_000)), // 3.6-Max 256K(3.7-Max 已 1M,保守取低)
    // Google Gemini(经官方 OpenAI 兼容端点;牌价随版本浮动、存疑就不装懂 → 留 None 只报 token)。
    // 特异在前、通配在后(子串匹配按顺序)。窗口全系 1M。
    m("gemini-2.5-flash", Tier::Light, true, None, None, Some(1_000_000)),
    m("gemini-2.0-flash", Tier::Light, true, None, None, Some(1_000_000)),
    m("gemini-flash", Tier::Light, true, None, None, Some(1_000_000)), // gemini-flash-latest 等
    m("gemini-2.5-pro", Tier::Smart, true, None, None, Some(1_000_000)),
    m("gemini-3", Tier::Smart, true, None, None, Some(1_000_000)), // gemini-3-pro 等
    m("gemini", Tier::Balanced, true, None, None, Some(1_000_000)), // 通配兜底:未列出的 gemini 版本
    // Ollama 本地模型(llama/qwen/mistral/…):名字千变万化且本地零计费,故意不列 ——
    // 命中目录兜底规则(未知 → 均衡档 + 不报钱 + 窗口 None → 预算回落默认),正合"本地、免费"的语义。
];

/// 模糊匹配:归一小写后,目录家族子串出现在模型 id 里即命中(吃掉中转前缀/版本后缀)。
pub fn lookup(model_id: &str) -> Option<&'static ModelInfo> {
    let id = model_id.to_ascii_lowercase();
    CATALOG.iter().find(|info| id.contains(info.family))
}

/// 能不能看图:先看用户覆盖(「高级」里标的,自架 llava / qwen-vl 类靠它),再查目录;
/// 未知模型按 **false**(宁可少看图、不可让 image_url 打挂回合——DeepSeek 真机 400
/// 实锤 2026-07-03;且同一家不同端点行为不一,不可赌服务商自己占位)。
pub fn supports_vision(model_id: &str) -> bool {
    if let Some(v) = override_for(model_id).and_then(|o| o.vision) {
        return v;
    }
    lookup(model_id).map(|i| i.vision).unwrap_or(false)
}

/// 兜底规则:未知模型按均衡档对待。用户覆盖优先(「高级」里改过的档位)。
pub fn tier_of(model_id: &str) -> Tier {
    if let Some(t) = override_for(model_id).and_then(|o| o.tier) {
        return t;
    }
    lookup(model_id).map(|i| i.tier).unwrap_or(Tier::Balanced)
}

/// 上下文窗口(token)。用户覆盖优先;否则查目录;None = 未知(本地模型)→ 调用方(engine 上下文
/// 预算)回落保守默认。绑机制不绑模型名(§4.4):新模型未列也不罢工,只是回落默认预算。
pub fn ctx_window_of(model_id: &str) -> Option<u32> {
    if let Some(o) = override_for(model_id) {
        if o.ctx_window_tokens.is_some() {
            return o.ctx_window_tokens;
        }
    }
    lookup(model_id).and_then(|i| i.ctx_window_tokens)
}

/// 计价方式:用户覆盖优先,否则默认「按量 + 缓存」(目录无此列 —— 当前 provider 都有前缀缓存)。
/// 喂给 `engine::context::tail_budget_chars` 决定上下文留多少(无缓存 → 少留勤压)。
pub fn billing_of(model_id: &str) -> BillingMode {
    override_for(model_id).and_then(|o| o.billing).unwrap_or_default()
}

/// 按目录牌价估算一轮成本(USD)。None = 模型未知或价格未知 —— 调用方只报 token,不报钱。
/// 缓存命中部分不另算折扣价(各家折扣率不一),按全价估,宁可高估不低估。
pub fn est_cost_usd(model_id: &str, usage: &Usage) -> Option<f64> {
    // 覆盖价仅当进/出**都**给了才用(避免半价混算);否则整体回落目录。
    let (input, output) = match override_for(model_id).and_then(|o| o.in_usd_per_m.zip(o.out_usd_per_m))
    {
        Some(pair) => pair,
        None => {
            let info = lookup(model_id)?;
            (info.in_usd_per_m?, info.out_usd_per_m?)
        }
    };
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
        assert!((cost - 5.22).abs() < 1e-9, "1.74 + 3.48 = 5.22,实际 {cost}");
    }

    #[test]
    fn ctx_window_by_family_and_unknown_is_none() {
        // 模糊匹配照样吃前缀/后缀;全部 200K–1M
        assert_eq!(ctx_window_of("anthropic/claude-opus-4-8"), Some(1_000_000));
        assert_eq!(ctx_window_of("claude-haiku-4-5"), Some(200_000));
        assert_eq!(ctx_window_of("deepseek-v4-pro"), Some(1_000_000));
        assert_eq!(ctx_window_of("gpt-5-mini-2026-01"), Some(400_000));
        // 本地/未登记 → None(预算回落默认)
        assert_eq!(ctx_window_of("llama3.2"), None);
        assert_eq!(ctx_window_of("totally-unknown-llm"), None);
    }

    #[test]
    fn tier_ordering_supports_meets_need_comparison() {
        assert!(Tier::Smart > Tier::Balanced);
        assert!(Tier::Balanced > Tier::Light);
    }

    #[test]
    fn gemini_tiers_by_variant_and_prices_stay_none() {
        assert_eq!(tier_of("gemini-2.5-flash"), Tier::Light);
        assert_eq!(tier_of("gemini-2.0-flash-001"), Tier::Light);
        assert_eq!(tier_of("gemini-flash-latest"), Tier::Light);
        assert_eq!(tier_of("gemini-2.5-pro"), Tier::Smart);
        assert_eq!(tier_of("models/gemini-3-pro-preview"), Tier::Smart); // 带前缀
        assert_eq!(tier_of("gemini-exp-1206"), Tier::Balanced); // 通配兜底
        // Gemini 牌价存疑:只报 token 不报钱
        let usage = Usage { input_tokens: 1_000_000, output_tokens: 1_000_000, cache_hit_tokens: 0 };
        assert!(est_cost_usd("gemini-2.5-flash", &usage).is_none());
    }

    #[test]
    fn user_override_wins_over_catalog_then_clears() {
        // 用唯一 model id,避免与并行测试争用进程级 overlay(精确匹配 → 其他 id 不受影响)。
        let id = "ov-test-only-model";
        // 未设覆盖:未知模型 → 均衡档 + 无价 + 无窗口(目录兜底)
        assert_eq!(tier_of(id), Tier::Balanced);
        assert_eq!(ctx_window_of(id), None);
        assert_eq!(billing_of(id), BillingMode::Cached, "默认计价 = 按量+缓存");
        set_overrides(vec![ModelOverride {
            model: id.into(),
            tier: Some(Tier::Smart),
            in_usd_per_m: Some(2.0),
            out_usd_per_m: Some(8.0),
            ctx_window_tokens: Some(32_000),
            billing: Some(BillingMode::Uncached),
            vision: Some(true),
        }]);
        assert_eq!(tier_of(id), Tier::Smart, "覆盖档位生效");
        assert_eq!(ctx_window_of(id), Some(32_000), "覆盖窗口生效");
        assert_eq!(billing_of(id), BillingMode::Uncached, "覆盖计价方式生效");
        assert!(supports_vision(id), "覆盖 vision 生效(自架视觉模型标上)");
        let usage = Usage { input_tokens: 1_000_000, output_tokens: 1_000_000, cache_hit_tokens: 0 };
        assert!((est_cost_usd(id, &usage).unwrap() - 10.0).abs() < 1e-9, "覆盖价 2+8=10");
        // 半价覆盖(只给进价)→ 整体回落目录;未知模型目录无价 → None
        set_overrides(vec![ModelOverride {
            model: id.into(),
            tier: None,
            in_usd_per_m: Some(2.0),
            out_usd_per_m: None,
            ctx_window_tokens: None,
            billing: None,
            vision: None,
        }]);
        assert!(est_cost_usd(id, &usage).is_none(), "半价覆盖不生效,回落目录(无价)");
        assert_eq!(tier_of(id), Tier::Balanced, "tier 未覆盖 → 回落");
        assert!(!supports_vision(id), "vision 未覆盖 → 回落目录(未知 = false)");
        set_overrides(vec![]); // 收尾清空,不泄漏给其他测试
        assert_eq!(tier_of(id), Tier::Balanced);
    }

    // 2026-07 校订的视觉家族行:特异在前的排序是承重的(k2.5/k2.6 不被 kimi-k2 错杀)
    #[test]
    fn vision_catalog_families_2026_07() {
        assert!(supports_vision("kimi-k2.6-preview"), "K2.6 原生多模态");
        assert!(supports_vision("kimi-k2.5"), "K2.5 原生多模态");
        assert!(!supports_vision("kimi-k2-0711-preview"), "老 K2 纯文本");
        assert!(supports_vision("qwen-vl-max"));
        assert!(supports_vision("qwen3-vl-plus"));
        assert!(supports_vision("qwen3-omni-flash"));
        assert!(!supports_vision("qwen-max"), "qwen-max 的 API 仍纯文本");
        assert!(!supports_vision("llava:13b"), "目录不认识 → false,靠覆盖标");
        assert!(supports_vision("gemini-2.5-flash") && supports_vision("claude-sonnet-5"));
        assert!(!supports_vision("deepseek-v4"), "DeepSeek API 无图片输入(网页端灰度不算)");
    }

    #[test]
    fn ollama_local_models_fall_to_balanced_and_free() {
        // 本地模型名千变万化 → 命中兜底:均衡档 + 不报钱(本地免费,None 正确)
        assert_eq!(tier_of("llama3.2"), Tier::Balanced);
        assert_eq!(tier_of("qwen2.5-coder:7b"), Tier::Balanced);
        let usage = Usage { input_tokens: 1_000_000, output_tokens: 1_000_000, cache_hit_tokens: 0 };
        assert!(est_cost_usd("llama3.2", &usage).is_none());
    }
}
