//! 供应商 = 数据。接入新供应商是加一条 ProviderSpec,不是写代码;
//! 真不兼容的厂商才下楼写专属 LlmProvider 实现(trait 逃生口)。
//!
//! 路由立场(宪法 §4):钥匙是用户的,怎么用脑子是产品的。
//! 用户可见的只有策略三档;档位排序/故障切换是引擎内政,可随版本重调而数据不变。

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::catalog::{self, Tier};
use super::{LlmConfig, LlmProvider, Quirks, Thinking};

/// 协议方言。serde 透传未知值会失败 —— 刻意的:配置里写错协议名应当立刻被发现。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    OpenaiCompat,
    AnthropicCompat,
}

impl Protocol {
    pub fn as_str(self) -> &'static str {
        match self {
            Protocol::OpenaiCompat => "openai_compat",
            Protocol::AnthropicCompat => "anthropic_compat",
        }
    }

    pub fn parse(s: &str) -> Option<Protocol> {
        match s {
            "openai_compat" => Some(Protocol::OpenaiCompat),
            "anthropic_compat" => Some(Protocol::AnthropicCompat),
            _ => None,
        }
    }
}

/// 一个供应商条目。字段全部带默认 → 老 JSON / 手写残缺 JSON 反序列化天然兼容。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderSpec {
    pub id: String,
    /// 用户可见名(设置页卡片标题)。
    pub name: String,
    pub protocol: Protocol,
    pub base_url: String,
    pub api_key: String,
    /// 默认模型;选型时以它的目录档位代表本供应商。
    pub model: String,
    /// "深思"档模型;None = 同 model(靠 thinking 开关加深)。
    pub thinking_model: Option<String>,
    pub enabled: bool,
    pub quirks: Quirks,
}

impl Default for ProviderSpec {
    fn default() -> Self {
        ProviderSpec {
            id: String::new(),
            name: String::new(),
            protocol: Protocol::OpenaiCompat,
            base_url: String::new(),
            api_key: String::new(),
            model: String::new(),
            thinking_model: None,
            enabled: true,
            quirks: Quirks::default(),
        }
    }
}

impl ProviderSpec {
    /// DeepSeek 官方预设(默认供应商,宪法 §4)。
    pub fn deepseek(api_key: String) -> Self {
        let cfg = LlmConfig::deepseek(api_key);
        ProviderSpec {
            id: "deepseek".into(),
            name: "DeepSeek".into(),
            protocol: Protocol::OpenaiCompat,
            base_url: cfg.base_url,
            api_key: cfg.api_key,
            model: cfg.model,
            quirks: cfg.quirks,
            ..Default::default()
        }
    }

    /// Anthropic 官方预设。
    pub fn anthropic(api_key: String) -> Self {
        let cfg = LlmConfig::anthropic(api_key);
        ProviderSpec {
            id: "anthropic".into(),
            name: "Anthropic".into(),
            protocol: Protocol::AnthropicCompat,
            base_url: cfg.base_url,
            api_key: cfg.api_key,
            model: cfg.model,
            quirks: cfg.quirks,
            ..Default::default()
        }
    }

    /// 有钥匙且启用才参与选型。`${VAR}` 引用按解析后的值判断:
    /// 变量没设 = 没钥匙,该供应商安静退出候选,不报错(容错铁律)。
    pub fn usable(&self) -> bool {
        self.enabled && !resolve_env(&self.api_key).trim().is_empty()
    }

    /// 解析发生在这里(重建候选时),存储里永远保留 `${VAR}` 原文。
    fn to_config(&self) -> LlmConfig {
        LlmConfig {
            base_url: resolve_env(&self.base_url),
            api_key: resolve_env(&self.api_key),
            model: self.model.clone(),
            temperature: None,
            thinking: Thinking::Off,
            quirks: Quirks {
                extra_headers: self
                    .quirks
                    .extra_headers
                    .iter()
                    .map(|(k, v)| (k.clone(), resolve_env(v)))
                    .collect(),
                ..self.quirks.clone()
            },
        }
    }

    /// 协议 → 实现。新协议在这里加一臂;专属厂商实现也从这里接入。
    pub fn build(&self) -> Arc<dyn LlmProvider> {
        match self.protocol {
            Protocol::OpenaiCompat => {
                Arc::new(super::openai_compat::OpenAiCompatProvider::new(self.to_config()))
            }
            Protocol::AnthropicCompat => {
                Arc::new(super::anthropic_compat::AnthropicCompatProvider::new(self.to_config()))
            }
        }
    }
}

/// 用脑策略:用户可见的唯一路由旋钮(设置页三档,绝不露路由表)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Strategy {
    /// 省着用:低档优先
    Thrifty,
    /// 均衡:尊重用户排的列表顺序
    #[default]
    Balanced,
    /// 聪明优先:高档优先
    SmartFirst,
}

impl Strategy {
    /// 设置 KV 里的字符串形态;未知值回落均衡(容错铁律)。
    pub fn parse(s: &str) -> Strategy {
        match s {
            "thrifty" => Strategy::Thrifty,
            "smart_first" => Strategy::SmartFirst,
            _ => Strategy::Balanced,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ProviderRegistry {
    specs: Vec<ProviderSpec>,
}

impl ProviderRegistry {
    pub fn new(specs: Vec<ProviderSpec>) -> Self {
        Self { specs }
    }

    /// 兼容现状的最小注册表:单 DeepSeek。
    pub fn deepseek_only(api_key: String) -> Self {
        Self::new(vec![ProviderSpec::deepseek(api_key)])
    }

    pub fn from_json(json: &str) -> anyhow::Result<Self> {
        Ok(Self::new(serde_json::from_str(json)?))
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(&self.specs).expect("ProviderSpec 序列化不该失败")
    }

    pub fn specs(&self) -> &[ProviderSpec] {
        &self.specs
    }

    pub fn is_empty_usable(&self) -> bool {
        !self.specs.iter().any(ProviderSpec::usable)
    }

    /// 选型 = 排序后的候选列表:首位是主选,其余是建连失败时的故障切换顺序。
    /// 规则(引擎内政,UI 永不暴露):
    /// - Thrifty 低档优先 / SmartFirst 高档优先 / Balanced 保持列表序(用户排的顺序即偏好);
    /// - 同档之间稳定排序,列表序即并列裁决,行为可预期;
    /// - 档位来自 catalog::tier_of(spec.model),未知模型按均衡档(目录兜底规则)。
    pub fn candidates(&self, strategy: Strategy) -> Vec<&ProviderSpec> {
        let mut out: Vec<&ProviderSpec> = self.specs.iter().filter(|s| s.usable()).collect();
        match strategy {
            Strategy::Balanced => {}
            Strategy::Thrifty => out.sort_by_key(|s| tier_rank(s)),
            Strategy::SmartFirst => out.sort_by_key(|s| std::cmp::Reverse(tier_rank(s))),
        }
        out
    }
}

/// 配置值里的 `${VAR}` 环境变量插值:明文也好、引用也好,随用户(api_key /
/// base_url / 附加头的值均适用)。未设置的变量替换为空串 —— 对钥匙而言即"没钥匙"。
/// 没有转义语法:配置值里不存在字面 `${` 的真实场景,不为它发明规则。
pub fn resolve_env(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        match rest[start + 2..].find('}') {
            Some(end) => {
                let name = &rest[start + 2..start + 2 + end];
                out.push_str(&std::env::var(name).unwrap_or_default());
                rest = &rest[start + 2 + end + 1..];
            }
            None => {
                // 没闭合:按字面输出,不吞内容
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

fn tier_rank(spec: &ProviderSpec) -> u8 {
    match catalog::tier_of(&spec.model) {
        Tier::Light => 0,
        Tier::Balanced => 1,
        Tier::Smart => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(id: &str, model: &str) -> ProviderSpec {
        ProviderSpec {
            id: id.into(),
            name: id.into(),
            api_key: "sk-test".into(),
            model: model.into(),
            ..Default::default()
        }
    }

    #[test]
    fn json_roundtrip_tolerates_missing_fields() {
        // 手写残缺 JSON:缺 quirks/enabled/thinking_model 等,全部走默认
        let json = r#"[{
            "id": "relay", "name": "某中转", "protocol": "openai_compat",
            "base_url": "https://relay.example.com/v1", "api_key": "sk-x", "model": "gpt-5"
        }]"#;
        let reg = ProviderRegistry::from_json(json).unwrap();
        let s = &reg.specs()[0];
        assert!(s.enabled);
        assert_eq!(s.quirks, Quirks::default());
        // 回写再读不丢
        let reg2 = ProviderRegistry::from_json(&reg.to_json()).unwrap();
        assert_eq!(reg2.specs()[0].id, "relay");
    }

    #[test]
    fn unknown_protocol_fails_loudly() {
        let json = r#"[{ "id": "x", "protocol": "grpc_compat" }]"#;
        assert!(ProviderRegistry::from_json(json).is_err(), "协议写错必须立刻报错,不许静默吞");
    }

    #[test]
    fn unusable_specs_are_filtered() {
        let mut disabled = spec("a", "deepseek-v4-pro");
        disabled.enabled = false;
        let mut keyless = spec("b", "deepseek-v4-pro");
        keyless.api_key = "  ".into();
        let reg = ProviderRegistry::new(vec![disabled, keyless, spec("c", "deepseek-v4-pro")]);
        let c = reg.candidates(Strategy::Balanced);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].id, "c");
    }

    #[test]
    fn strategy_orders_candidates_by_tier() {
        let reg = ProviderRegistry::new(vec![
            spec("smart", "claude-opus-4-8"),
            spec("light", "deepseek-v4-flash"),
            spec("mid", "deepseek-v4-pro"),
        ]);
        let ids = |st: Strategy| -> Vec<String> {
            reg.candidates(st).iter().map(|s| s.id.clone()).collect()
        };
        assert_eq!(ids(Strategy::Balanced), ["smart", "light", "mid"], "均衡 = 用户列表序");
        assert_eq!(ids(Strategy::Thrifty), ["light", "mid", "smart"]);
        assert_eq!(ids(Strategy::SmartFirst), ["smart", "mid", "light"]);
    }

    #[test]
    fn strategy_parse_falls_back_to_balanced() {
        assert_eq!(Strategy::parse("thrifty"), Strategy::Thrifty);
        assert_eq!(Strategy::parse("smart_first"), Strategy::SmartFirst);
        assert_eq!(Strategy::parse("whatever"), Strategy::Balanced);
    }

    // ${VAR} 引用:明文/引用随用户;解析在取值时,存储保留原文
    #[test]
    fn env_refs_resolve_at_use_time() {
        std::env::set_var("LW_TEST_KEY_A", "sk-from-env");
        assert_eq!(resolve_env("${LW_TEST_KEY_A}"), "sk-from-env");
        assert_eq!(resolve_env("Bearer ${LW_TEST_KEY_A}!"), "Bearer sk-from-env!");
        assert_eq!(resolve_env("plain-sk-123"), "plain-sk-123", "明文原样直通");
        assert_eq!(resolve_env("${LW_TEST_UNSET_VAR_XYZ}"), "", "未设置变量 → 空串");
        assert_eq!(resolve_env("${no_close"), "${no_close", "没闭合按字面输出");

        let mut s = spec("env", "deepseek-v4-pro");
        s.api_key = "${LW_TEST_KEY_A}".into();
        assert!(s.usable(), "引用解析出钥匙 → 可用");
        s.api_key = "${LW_TEST_UNSET_VAR_XYZ}".into();
        assert!(!s.usable(), "引用解析为空 → 按没钥匙处理,安静退出候选");
        // 序列化保留引用原文,不落明文
        let json = serde_json::to_string(&ProviderSpec {
            api_key: "${LW_TEST_KEY_A}".into(),
            ..spec("env", "m")
        })
        .unwrap();
        assert!(json.contains("${LW_TEST_KEY_A}"));
        assert!(!json.contains("sk-from-env"));
    }

    #[test]
    fn presets_carry_dialect_quirks() {
        let ds = ProviderSpec::deepseek("k".into());
        assert!(ds.quirks.thinking_field, "DeepSeek 方言必须显式带 thinking 字段(坑 #2)");
        let an = ProviderSpec::anthropic("k".into());
        assert_eq!(an.protocol, Protocol::AnthropicCompat);
        assert!(!an.quirks.thinking_field);
    }
}
