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
    /// Google Gemini 原生(generateContent):为 thought_signature 逐字保真而下楼(reasoning 保真铁律)。
    Gemini,
    /// OpenAI Responses API 原生:为 reasoning encrypted_content 逐字保真而下楼(同上铁律)。
    OpenaiResponses,
}

impl Protocol {
    pub fn as_str(self) -> &'static str {
        match self {
            Protocol::OpenaiCompat => "openai_compat",
            Protocol::AnthropicCompat => "anthropic_compat",
            Protocol::Gemini => "gemini",
            Protocol::OpenaiResponses => "openai_responses",
        }
    }

    pub fn parse(s: &str) -> Option<Protocol> {
        match s {
            "openai_compat" => Some(Protocol::OpenaiCompat),
            "anthropic_compat" => Some(Protocol::AnthropicCompat),
            "gemini" => Some(Protocol::Gemini),
            "openai_responses" => Some(Protocol::OpenaiResponses),
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

    /// Google Gemini 官方预设(**原生** generateContent:为 thought_signature 保真,宪法 §4 铁律)。
    pub fn gemini(api_key: String) -> Self {
        let cfg = LlmConfig::gemini(api_key);
        ProviderSpec {
            id: "gemini".into(),
            name: "Google Gemini".into(),
            protocol: Protocol::Gemini,
            base_url: cfg.base_url,
            api_key: cfg.api_key,
            model: cfg.model,
            quirks: cfg.quirks,
            ..Default::default()
        }
    }

    /// Ollama 本地预设(经 OpenAI 兼容端点 /v1)。base_url 空 → 默认 localhost。
    pub fn ollama(base_url: String) -> Self {
        let cfg = LlmConfig::ollama(base_url);
        ProviderSpec {
            id: "ollama".into(),
            name: "Ollama".into(),
            protocol: Protocol::OpenaiCompat,
            base_url: cfg.base_url,
            api_key: cfg.api_key,
            model: cfg.model,
            quirks: cfg.quirks,
            ..Default::default()
        }
    }

    /// OpenAI 官方预设(**原生** Responses API:为 reasoning encrypted_content 保真,宪法 §4 铁律)。
    pub fn openai(api_key: String) -> Self {
        let cfg = LlmConfig::openai(api_key);
        ProviderSpec {
            id: "openai".into(),
            name: "OpenAI".into(),
            protocol: Protocol::OpenaiResponses,
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
        let base_url = resolve_env(&self.base_url);
        // 方言修正:用户没显式设过(仍是默认值)→ 按接入点主机从预设表派生(千问 enable_thinking / Kimi·混元
        // 开关 + 回传 / 智谱·豆包 开关);显式设过的(哪怕只改了一位)原样尊重、不覆盖;中转站等认不出的
        // 主机 → 维持默认。解析完 `${ENV}` 再派生,引用形接入点也认得。已接入的卡与「自己接一个大脑」草稿
        // (engine `list_models_for`)都经 `build()` 走这里 —— 唯一判定处。
        let mut quirks = if self.quirks == Quirks::default() {
            quirks_for_base_url(&base_url).unwrap_or_default()
        } else {
            self.quirks.clone()
        };
        quirks.extra_headers =
            quirks.extra_headers.iter().map(|(k, v)| (k.clone(), resolve_env(v))).collect();
        LlmConfig {
            base_url,
            api_key: resolve_env(&self.api_key),
            model: self.model.clone(),
            temperature: None,
            thinking: Thinking::Off,
            quirks,
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
            Protocol::Gemini => Arc::new(super::gemini::GeminiProvider::new(self.to_config())),
            Protocol::OpenaiResponses => {
                Arc::new(super::openai_responses::OpenAiResponsesProvider::new(self.to_config()))
            }
        }
    }
}

/// 厂商预设 = 数据(§4.4「供应商 = 数据」)。给「自己接一个大脑」卡的「从预设开始」下拉:选一家 →
/// 名字 / 协议 / 接入点自动填,用户只贴钥匙。**刻意不带默认模型**(2026-09-04 用户拍板「不写死」:
/// 厂商换代频繁、写死必陈;模型靠设置页 ▾ 向接入点现查,选好再接入)。
/// 名字 = 品牌专有名词、中英同形,不进 i18n(同 DeepSeek / Anthropic 预设名)。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderPreset {
    /// 稳定 id(新建卡的 id 前缀;与模板卡 deepseek / anthropic 不撞)。
    pub id: String,
    pub name: String,
    pub protocol: Protocol,
    pub base_url: String,
    /// 钥匙占位值:None = 必须用户贴钥匙;Some = 本地服务不验钥匙,用占位让 `usable()` 放行(Ollama)。
    pub key_placeholder: Option<String>,
    /// 该家接入点的方言修正(思考开关形 / reasoning 回传 / 千问 enable_thinking)。**不过桥**(前端只填名字 /
    /// 协议 / 接入点;接入时 `ProviderSpec::to_config` 按主机从本表派生,见 `quirks_for_base_url`)。
    #[serde(skip)]
    pub quirks: Quirks,
}

/// 预设表。名单 = 国内主流五家(千问 / 豆包 / Kimi / 智谱 / 混元,用户拍板「主流就行」)+ 已有但此前
/// UI 够不着的 OpenAI / Gemini / Ollama(它们走原生协议 / 本地端点,自定义卡原先只给两种兼容协议选);
/// 后三家的接入点取自 `LlmConfig` 预设构造器(单源,不复制字面量)。国内五家的接入点 = 各家官方文档的
/// 公开地址(协议事实,非 §4.11 产品默认);千问用百炼旧域名(官方仍可用,新域名带用户 WorkspaceId
/// 没法预填);豆包 model 填 Model ID(需控制台「开通」)或 `ep-…` 接入点 ID 都行。
/// 每家的**方言修正(quirks)也在这一张表**(2026-09 各家官方文档核),`quirks_for_base_url` 按接入点
/// 主机派生,别处不再复制这份判定。
/// DeepSeek / Anthropic 两张模板卡另走 `engine::effective_specs`,不在此表(免同一家两个入口)。
pub fn presets() -> Vec<ProviderPreset> {
    let lit = |id: &str, name: &str, protocol: Protocol, base_url: &str, quirks: Quirks| ProviderPreset {
        id: id.into(),
        name: name.into(),
        protocol,
        base_url: base_url.into(),
        key_placeholder: None,
        quirks,
    };
    // 开关形 thinking:{type: enabled|disabled};Kimi / 混元官方还要求工具循环里把 assistant 的
    // reasoning_content 原样回传(缺了要么 400 要么推理断链)。
    let toggle = Quirks { thinking_toggle: true, ..Quirks::default() };
    let toggle_roundtrip =
        Quirks { thinking_toggle: true, reasoning_roundtrip: true, ..Quirks::default() };
    let openai = LlmConfig::openai(String::new());
    let gemini = LlmConfig::gemini(String::new());
    let ollama = LlmConfig::ollama(String::new());
    vec![
        // 千问:enable_thinking: true|false(+thinking_budget);Off **不带字段**
        // (help.aliyun.com/zh/model-studio/deep-thinking)
        lit(
            "qwen",
            "千问 Qwen",
            Protocol::OpenaiCompat,
            "https://dashscope.aliyuncs.com/compatible-mode/v1",
            Quirks { enable_thinking_bool: true, ..Quirks::default() },
        ),
        // 豆包:thinking:{type: enabled|disabled|auto}(Seed 1.6 / 2.x 同形;auto 不用,非 Off 即 enabled;
        // 2026-09 经 BytePlus 同族文档 + 镜像文档核,火山官方页 SPA 抓不到正文)
        lit(
            "doubao",
            "豆包 Doubao",
            Protocol::OpenaiCompat,
            "https://ark.cn-beijing.volces.com/api/v3",
            toggle.clone(),
        ),
        // Kimi:K2.6 thinking:{type} 默认 enabled;K3 / K2.7-code 恒思考**没有** thinking 参数(K3 走
        // reasoning_effort low/high/max,没 medium → 不开 effort_field;它们收到 thinking:{type} 会不会 400 =
        // 真机 watch,绑机制不绑模型名 §6.3);工具循环**必须**回传 reasoning_content
        // (platform.kimi.com/docs/guide/use-thinking-models)
        lit("kimi", "Kimi", Protocol::OpenaiCompat, "https://api.moonshot.cn/v1", toggle_roundtrip.clone()),
        // 智谱:thinking:{type:"enabled"} 开思考(docs.bigmodel.cn glm-5-turbo 页);回传 reasoning_content
        // 是否必需未核 → 不回传
        lit("zhipu", "智谱 GLM", Protocol::OpenaiCompat, "https://open.bigmodel.cn/api/paas/v4", toggle),
        // 混元:接入点 = TokenHub(官方文档现只写它,hy3 / hy4-preview 新模型只在这;老域名
        // api.hunyuan.cloud.tencent.com 2026-09 仍活着但只有 hunyuan-* 老模型,方言经 HOST_ALIASES 同映射);
        // thinking:{type: enabled|disabled}(无 enable_thinking)+ 工具调用要把 assistant 消息含
        // reasoning_content 整体回写(cloud.tencent.com/document/product/1823/132252)
        lit(
            "hunyuan",
            "混元 Hunyuan",
            Protocol::OpenaiCompat,
            "https://tokenhub.tencentmaas.com/v1",
            toggle_roundtrip,
        ),
        lit("openai", "OpenAI", Protocol::OpenaiResponses, &openai.base_url, Quirks::default()),
        lit("gemini", "Gemini", Protocol::Gemini, &gemini.base_url, Quirks::default()),
        ProviderPreset {
            key_placeholder: Some(ollama.api_key.clone()),
            ..lit("ollama", "Ollama", Protocol::OpenaiCompat, &ollama.base_url, Quirks::default())
        },
    ]
}

/// 老域名 / 别名 → 预设 id:接入点换了域名但方言不变的家。
const HOST_ALIASES: &[(&str, &str)] = &[
    // 混元老接入点(2026-09 仍活着,但 hy3 / hy4-preview 只在 TokenHub);方言同一套
    ("api.hunyuan.cloud.tencent.com", "hunyuan"),
];

/// URL → 小写主机名(去 scheme / 用户信息 / 端口 / 路径);解析不出 → 空串。
fn host_of(url: &str) -> String {
    let rest = url.trim();
    let rest = rest.split_once("://").map(|(_, r)| r).unwrap_or(rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    authority.split(':').next().unwrap_or("").to_ascii_lowercase()
}

/// 按接入点主机派生该家的方言修正:命中预设表(含 HOST_ALIASES 老域名)→ Some(该家 quirks);中转站 /
/// 自架 / 认不出的主机 → None(维持现状:用默认或用户显式设的)。**唯一判定处**:`ProviderSpec::to_config`
/// 在 spec.quirks 仍是默认值时用它 —— 已接入的卡与「自己接一个大脑」草稿(engine `list_models_for`)都经
/// `build()` → `to_config()`,不长第二处判定。
pub fn quirks_for_base_url(base_url: &str) -> Option<Quirks> {
    let host = host_of(base_url);
    if host.is_empty() {
        return None;
    }
    let all = presets();
    if let Some((_, id)) = HOST_ALIASES.iter().find(|(alias, _)| *alias == host) {
        return all.iter().find(|p| p.id == *id).map(|p| p.quirks.clone());
    }
    all.iter().find(|p| host_of(&p.base_url) == host).map(|p| p.quirks.clone())
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

    // 预设表:id 唯一、接入点规整(末尾不带 /,provider 自己拼路径)、协议名能过桥往返、
    // 模板卡不混进来;本地服务给占位钥匙放行,云厂商必须贴钥匙。
    #[test]
    fn presets_are_well_formed_and_distinct() {
        let ps = presets();
        let mut ids: Vec<&str> = ps.iter().map(|p| p.id.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), ps.len(), "预设 id 必须唯一");
        for p in &ps {
            assert!(!p.name.is_empty() && !p.base_url.is_empty(), "{}", p.id);
            assert!(!p.base_url.ends_with('/'), "{}: 接入点末尾别带 /", p.id);
            assert!(p.base_url.starts_with("http"), "{}: {}", p.id, p.base_url);
            assert_eq!(Protocol::parse(p.protocol.as_str()), Some(p.protocol), "{}", p.id);
            assert!(!["deepseek", "anthropic"].contains(&p.id.as_str()), "模板卡不进预设表");
        }
        let by = |id: &str| ps.iter().find(|p| p.id == id).expect(id);
        assert_eq!(by("ollama").key_placeholder.as_deref(), Some("ollama"));
        assert!(by("qwen").key_placeholder.is_none());
        assert_eq!(by("gemini").protocol, Protocol::Gemini, "Gemini 走原生方言(保真铁律)");
        assert_eq!(by("openai").protocol, Protocol::OpenaiResponses);
        // 后三家与 LlmConfig 预设同源:构造器改了接入点,这里自动跟着变
        assert_eq!(by("ollama").base_url, LlmConfig::ollama(String::new()).base_url);
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

    #[test]
    fn gemini_native_and_ollama_compat_presets() {
        // Gemini = 原生方言(为 thought_signature 保真,reasoning 保真铁律)
        let g = ProviderSpec::gemini("k".into());
        assert_eq!(g.protocol, Protocol::Gemini, "Gemini 走原生,不走兼容端点");
        assert!(g.base_url.ends_with("/v1beta"), "base_url 末尾不带模型路径,由 provider 拼");
        assert!(!g.base_url.contains("openai"), "原生路径不含 /openai");
        assert!(g.usable(), "有钥匙即可用");

        // Ollama = 兼容端点(纯文本 reasoning 无损,/v1 绕开原生坑)
        let o = ProviderSpec::ollama(String::new());
        assert_eq!(o.protocol, Protocol::OpenaiCompat);
        assert_eq!(o.protocol, Protocol::OpenaiCompat);
        assert_eq!(o.base_url, "http://localhost:11434/v1", "空 base_url → 默认 localhost");
        assert!(o.usable(), "Ollama 占位钥匙让它进候选(本地不验钥匙,但 usable() 要非空)");
        // 自定义远程 Ollama 实例
        let o2 = ProviderSpec::ollama("http://192.168.1.9:11434/v1".into());
        assert_eq!(o2.base_url, "http://192.168.1.9:11434/v1");
    }

    // 2026-09 国内五家的方言修正随预设表走:Kimi / 混元 = 开关 + 回传,智谱 / 豆包 = 只开关,千问 = enable_thinking;
    // 混元接入点换 TokenHub;quirks 不过桥(序列化里没有这个字段,前端形状不变)。
    #[test]
    fn presets_carry_domestic_quirks_and_hunyuan_tokenhub() {
        let ps = presets();
        let by = |id: &str| ps.iter().find(|p| p.id == id).expect(id);
        let toggle = Quirks { thinking_toggle: true, ..Quirks::default() };
        let both = Quirks { thinking_toggle: true, reasoning_roundtrip: true, ..Quirks::default() };
        assert_eq!(by("kimi").quirks, both);
        assert_eq!(by("hunyuan").quirks, both);
        assert_eq!(by("zhipu").quirks, toggle);
        assert_eq!(by("doubao").quirks, toggle);
        assert_eq!(by("qwen").quirks, Quirks { enable_thinking_bool: true, ..Quirks::default() });
        for id in ["openai", "gemini", "ollama"] {
            assert_eq!(by(id).quirks, Quirks::default(), "{id}: 无特殊方言");
        }
        assert_eq!(by("hunyuan").base_url, "https://tokenhub.tencentmaas.com/v1");
        let json = serde_json::to_value(by("kimi")).unwrap();
        assert!(json.get("quirks").is_none(), "quirks 不过桥,预设 JSON 形状不变");
        assert_eq!(json["baseUrl"], "https://api.moonshot.cn/v1");
    }

    // 方言修正按接入点主机派生(单源 = 预设表 + 老域名别名):五家各自的位、老域名同映射、大小写 / 端口 /
    // 无 scheme 都认;中转站 / 自架 / 空串 → None;DeepSeek 不在预设表(quirks 由构造器给)。
    #[test]
    fn quirks_derive_from_base_url_host() {
        let q = |u: &str| quirks_for_base_url(u).expect(u);
        let kimi = q("https://api.moonshot.cn/v1");
        assert!(kimi.thinking_toggle && kimi.reasoning_roundtrip && !kimi.enable_thinking_bool);
        let hy = q("https://tokenhub.tencentmaas.com/v1");
        assert!(hy.thinking_toggle && hy.reasoning_roundtrip);
        assert_eq!(q("https://api.hunyuan.cloud.tencent.com/v1"), hy, "混元老域名同一套方言");
        let zhipu = q("https://open.bigmodel.cn/api/paas/v4/");
        assert!(zhipu.thinking_toggle && !zhipu.reasoning_roundtrip, "智谱回传未核 → 只开关");
        let doubao = q("https://ark.cn-beijing.volces.com/api/v3");
        assert!(doubao.thinking_toggle && !doubao.reasoning_roundtrip);
        let qwen = q("https://dashscope.aliyuncs.com/compatible-mode/v1");
        assert!(qwen.enable_thinking_bool && !qwen.thinking_toggle && !qwen.thinking_field);
        assert_eq!(q("HTTPS://API.MOONSHOT.CN:443/v1"), kimi, "大小写 / 端口不影响");
        assert_eq!(q("api.moonshot.cn/v1"), kimi, "无 scheme 也认");
        assert!(quirks_for_base_url("https://relay.example.com/v1").is_none(), "中转站 → None");
        assert!(quirks_for_base_url("https://moonshot.cn.evil.example/v1").is_none(), "只认整主机名");
        assert!(quirks_for_base_url("").is_none());
        assert_eq!(quirks_for_base_url("https://api.openai.com/v1"), Some(Quirks::default()), "无特殊方言的家 = 默认");
        assert!(quirks_for_base_url("https://api.deepseek.com").is_none());
        assert_eq!(host_of("https://user:pw@example.com:8443/x?y#z"), "example.com");
    }

    // to_config 只在 quirks 仍是默认值时派生;显式设过的(哪怕只一位)原样尊重;${ENV} 接入点先解析再派生;
    // 认不出的主机维持默认;DeepSeek 预设的 thinking_field 不受影响。
    #[test]
    fn to_config_derives_quirks_only_when_unset() {
        let mut s = spec("kimi", "kimi-k2.6");
        s.base_url = "https://api.moonshot.cn/v1".into();
        let cfg = s.to_config();
        assert!(cfg.quirks.thinking_toggle && cfg.quirks.reasoning_roundtrip, "默认 quirks → 按主机派生");
        s.quirks = Quirks { no_stream_options: true, ..Quirks::default() };
        let cfg = s.to_config();
        assert!(cfg.quirks.no_stream_options && !cfg.quirks.thinking_toggle, "显式设过 → 不派生不覆盖");

        std::env::set_var("LW_TEST_QWEN_URL", "https://dashscope.aliyuncs.com/compatible-mode/v1");
        let mut e = spec("qwen", "qwen3.8-max");
        e.base_url = "${LW_TEST_QWEN_URL}".into();
        let cfg = e.to_config();
        assert!(cfg.quirks.enable_thinking_bool, "引用形接入点解析后照样派生");
        assert_eq!(cfg.base_url, "https://dashscope.aliyuncs.com/compatible-mode/v1");

        let mut r = spec("relay", "gpt-5");
        r.base_url = "https://relay.example.com/v1".into();
        assert_eq!(r.to_config().quirks, Quirks::default(), "中转站维持默认");
        assert!(ProviderSpec::deepseek("k".into()).to_config().quirks.thinking_field);
        // 显式 quirks 里的附加头照旧解析 ${ENV}
        std::env::set_var("LW_TEST_HDR", "v1");
        let mut h = spec("relay2", "gpt-5");
        h.quirks = Quirks {
            extra_headers: vec![("x-h".into(), "${LW_TEST_HDR}".into())],
            ..Quirks::default()
        };
        assert_eq!(h.to_config().quirks.extra_headers, vec![("x-h".to_string(), "v1".to_string())]);
    }
}
