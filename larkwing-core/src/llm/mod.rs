//! LLM 接缝:中立类型 + trait。厂商方言永不出各 provider 文件。
//!
//! 多供应商三级结构(宪法 §4):
//! 1. 协议实现打底:openai_compat / anthropic_compat 两种"方言"覆盖绝大多数端点;
//! 2. 厂商差异 = Quirks 数据修正(认证头风格、字段缺失、严格网关),不另写代码;
//! 3. 真不兼容的厂商单独实现 LlmProvider —— trait 本来就是逃生口。
//! 供应商本身 = 数据(registry::ProviderSpec),模型档位/价格 = 数据(catalog)。

pub mod anthropic_compat;
pub mod catalog;
pub mod fake;
pub mod openai_compat;
pub mod registry;
mod sse;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

/// 工具定义(中立形态),进 ChatRequest.tools;各方言自行翻译
/// (OpenAI 系包 function 壳,Anthropic 系字段叫 input_schema)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    /// JSON Schema(object 形)。
    pub parameters: serde_json::Value,
}

/// 工具选择策略。None = 本次调用禁用工具 —— 轮数到顶时 engine 强制用嘴收尾。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoice {
    #[default]
    Auto,
    None,
}

/// 工具调用(中立形态)。OpenAI 系的流式参数碎片由各 provider 攒完整后才出现在这里
/// (重组规范见 PLAN §3 约束 #6);id 是并发对账与 call/result 配对的主键。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// 已解析的 JSON 参数(provider 负责把字符串碎片拼成合法 JSON)。
    #[serde(default)]
    pub args: serde_json::Value,
    /// 截断检测(规范 #6):参数攒完不是合法 JSON(典型 = stop_reason: max_tokens 拦腰截断)。
    /// 消费方必须拒绝执行,把"参数不完整"作为错误结果喂回模型。
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_incomplete: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// 碎片攒完 → 解析参数。截断检测必须发生在 stop_reason 归一之后(robot 实战次级坑:
/// 顺序错了检测永不命中);空串兜 "{}"。两方言共用,故放中立层。
pub(crate) fn parse_tool_args(raw: &str, stop_reason: Option<&str>, tool: &str) -> (serde_json::Value, bool) {
    let raw = if raw.trim().is_empty() { "{}" } else { raw };
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(v) => (v, false),
        Err(_) => {
            if stop_reason == Some("max_tokens") {
                tracing::warn!(tool, "tool_call 参数被 max_tokens 截断,标记 is_incomplete");
            } else {
                tracing::warn!(tool, "tool_call 参数不是合法 JSON,标记 is_incomplete");
            }
            (serde_json::Value::String(raw.to_string()), true)
        }
    }
}

/// 调用形态(≠ store::Message 持久形态)。PLAN §3 终态的工具期形状:
/// content 暂为 String(Text-only 退化形,多媒体期换 Vec<ContentPart>)。
/// system 独立成 ChatRequest 字段(Anthropic 兼容)。
/// serde 形 = `{"role":"user","content":"…"}`——场景 few-shot 直接按此形手写(PLAN §8)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum ChatMessage {
    User {
        content: String,
    },
    Assistant {
        #[serde(default)]
        content: String,
        /// 坑 #4:带 tool_calls 的轮次回传时 DeepSeek 要求附带 reasoning,缺则 400。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCall>,
    },
    /// 工具结果。JSON tag 用 "tool",与 store 的 role 词表同字。
    #[serde(rename = "tool")]
    ToolResult {
        call_id: String,
        content: String,
    },
}

impl ChatMessage {
    pub fn user(content: impl Into<String>) -> Self {
        ChatMessage::User { content: content.into() }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        ChatMessage::Assistant {
            content: content.into(),
            reasoning: None,
            tool_calls: Vec::new(),
        }
    }
}

/// 思考档位(中立词表,用户侧叫"反应模式":最快/轻度/中度/重度)。
/// 各方言自行翻译:DeepSeek 只有开关(非 Off 都算开);Anthropic 按档位配 budget;
/// 支持 reasoning_effort 的 OpenAI 系端点翻成 low/medium/high(见 Quirks::effort_field)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Thinking {
    #[default]
    Off,
    Light,
    Medium,
    /// 复杂场景:对应各家 high / max 级别
    Heavy,
}

impl Thinking {
    /// reasoning_effort 词表(OpenAI 系)。Off 不发字段,返回 None。
    pub fn effort_str(self) -> Option<&'static str> {
        match self {
            Thinking::Off => None,
            Thinking::Light => Some("low"),
            Thinking::Medium => Some("medium"),
            Thinking::Heavy => Some("high"),
        }
    }
}

/// 单轮覆盖;None = 用 LlmConfig 默认。serde 直通 IPC。
/// 管道合法用户 = 场景预设 / "高级"设置 / 调试面板,普通聊天 UI 不长旋钮。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ChatOptions {
    pub model: Option<String>,
    pub temperature: Option<f32>,
    pub thinking: Option<Thinking>,
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Clone, Default)]
pub struct ChatRequest {
    /// 语义独立:OpenAI 系翻成首条 system 消息,Anthropic 翻成顶层参数。
    pub system: String,
    pub messages: Vec<ChatMessage>,
    pub options: ChatOptions,
    /// 白名单工具定义(场景声明顺序,会话内稳定 → 前缀不抖,吃缓存)。空 = 无工具。
    pub tools: Vec<ToolDef>,
    /// None = 禁用工具(engine 轮数到顶强制收尾用);无工具时各方言不发该字段。
    pub tool_choice: ToolChoice,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Usage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    /// DeepSeek 自动前缀缓存的命中量,进日志观测省钱效果。
    pub cache_hit_tokens: i64,
}

/// 账户余额快照(支持查询的供应商才有,如 DeepSeek /user/balance)。
/// amount 保留供应商原文字符串:只展示,不做算术 —— 不替别人家的账面装懂。
#[derive(Debug, Clone, Serialize)]
pub struct AccountBalance {
    /// "CNY" / "USD" 等,UI 自行翻译成货币符号。
    pub currency: String,
    pub amount: String,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum LlmError {
    #[error("还没有配置 API key")]
    NoApiKey,
    #[error("API key 无效")]
    BadApiKey,
    #[error("网络问题: {0}")]
    Network(String),
    #[error("API 错误 {status}: {message}")]
    Api { status: u16, message: String },
}

/// provider ↔ engine 的词汇表(engine ↔ UI 用 TurnEvent,各边界各自的词汇)。
#[derive(Debug)]
pub enum ChatEvent {
    Delta(String),
    /// reasoning_content 增量。MVP 关思考不会出现,但解析层必须认识它(坑 #3)。
    Thinking(String),
    /// stop_reason 已归一到中立词表:end_turn / tool_use / max_tokens,未知值透传(坑 #9)。
    /// max_tokens = 回复被长度上限拦腰截断,消费方必须可见,不许装正常。
    /// tool_calls = provider 按规范 #6 攒完整的调用(流中碎片不外漏);空 = 纯文本回合。
    Done {
        usage: Usage,
        stop_reason: Option<String>,
        tool_calls: Vec<ToolCall>,
    },
    Failed(LlmError),
}

/// 认证头风格:野生兼容端点的第一大分歧点。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthStyle {
    /// `Authorization: Bearer <key>`(OpenAI 系默认)
    Bearer,
    /// `x-api-key: <key>`(Anthropic 系默认)
    XApiKey,
    /// `api-key: <key>`(Azure 系)
    ApiKeyHeader,
}

/// 厂商差异修正层:兼容但不完全兼容的端点,用数据修正而不是新实现。
/// 全部字段有默认值 → 老配置 JSON 反序列化天然兼容。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Quirks {
    /// None = 按协议默认(openai_compat→Bearer,anthropic_compat→XApiKey)。
    pub auth: Option<AuthStyle>,
    /// DeepSeek 方言:请求体显式带 thinking 字段(坑 #2)。
    /// 默认 false —— 严格 OpenAI 兼容网关遇到未知字段可能 400。
    pub thinking_field: bool,
    /// 端点支持 reasoning_effort(gpt-5 系):思考档位翻成 low/medium/high 字段。
    pub effort_field: bool,
    /// 严格端点不认 stream_options.include_usage,置 true 省掉(代价:可能拿不到 usage,记账降级)。
    pub no_stream_options: bool,
    /// 中转站常要求的固定附加请求头。
    pub extra_headers: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
pub struct LlmConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub temperature: Option<f32>,
    /// 默认思考档位;DeepSeek 方言下无论开关都显式带 thinking 字段(坑 #2,见 Quirks)。
    pub thinking: Thinking,
    pub quirks: Quirks,
}

impl LlmConfig {
    /// DeepSeek = 一组配置,不是专属实现。
    /// 注意:旧模型名 deepseek-chat / deepseek-reasoner 于 2026-07-24 弃用(坑 #1)。
    pub fn deepseek(api_key: String) -> Self {
        LlmConfig {
            base_url: "https://api.deepseek.com".into(),
            api_key,
            model: "deepseek-v4-pro".into(),
            temperature: None,
            thinking: Thinking::Off,
            quirks: Quirks { thinking_field: true, ..Quirks::default() },
        }
    }

    /// Anthropic 官方 = anthropic_compat 协议的一组配置。
    pub fn anthropic(api_key: String) -> Self {
        LlmConfig {
            base_url: "https://api.anthropic.com".into(),
            api_key,
            model: "claude-sonnet-4-6".into(),
            temperature: None,
            thinking: Thinking::Off,
            quirks: Quirks::default(),
        }
    }
}

#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// 两阶段错误:建连前的问题(没 key、401、连不上)走 Err;开流后的问题走 Failed 事件。
    /// 取消 = drop Receiver:provider 内部任务 send 失败即中止并断开连接。
    async fn chat_stream(&self, req: ChatRequest) -> Result<mpsc::Receiver<ChatEvent>, LlmError>;

    /// 本 provider 的默认模型 id(记账按它查目录牌价;options.model 覆盖时以覆盖为准)。
    /// 空串 = 不知道 → 目录查不到 → 记账只报 token,链路天然降级。
    fn model_id(&self) -> &str {
        ""
    }

    /// 账户余额(支持的供应商才覆写,如 DeepSeek)。None = 不支持/查不到。
    /// 锦上添花链路:任何失败都静默成 None,绝不打扰主对话。
    async fn balance(&self) -> Option<AccountBalance> {
        None
    }

    /// 非流式便捷口(记忆提炼/摘要等后台用途):drain 流拼完整文本,忽略工具调用。
    async fn chat(&self, req: ChatRequest) -> Result<String, LlmError> {
        let mut rx = self.chat_stream(req).await?;
        let mut out = String::new();
        while let Some(ev) = rx.recv().await {
            match ev {
                ChatEvent::Delta(t) => out.push_str(&t),
                ChatEvent::Thinking(_) => {}
                ChatEvent::Done { .. } => return Ok(out),
                ChatEvent::Failed(e) => return Err(e),
            }
        }
        // 流提前断开:把已有内容当结果返回,调用方自行判断够不够用
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // few-shot 手写形状的 golden:场景 JSON 里就按这个形写(PLAN §8),改坏即炸
    #[test]
    fn chat_message_serde_shape_for_few_shots() {
        let user: ChatMessage =
            serde_json::from_str(r#"{"role":"user","content":"我对花生过敏"}"#).unwrap();
        assert_eq!(user, ChatMessage::user("我对花生过敏"));

        let call: ChatMessage = serde_json::from_str(
            r#"{"role":"assistant","content":"",
                "tool_calls":[{"id":"fs_1","name":"remember","args":{"fact":"对花生过敏"}}]}"#,
        )
        .unwrap();
        match &call {
            ChatMessage::Assistant { tool_calls, reasoning, .. } => {
                assert_eq!(tool_calls[0].name, "remember");
                assert_eq!(tool_calls[0].args["fact"], "对花生过敏");
                assert!(reasoning.is_none());
            }
            other => panic!("应是 Assistant,实际 {other:?}"),
        }

        let result: ChatMessage =
            serde_json::from_str(r#"{"role":"tool","call_id":"fs_1","content":"ok"}"#).unwrap();
        assert_eq!(
            result,
            ChatMessage::ToolResult { call_id: "fs_1".into(), content: "ok".into() }
        );

        // 普通 assistant 出向不带空 tool_calls/reasoning 字段:序列化干净、不挤前缀
        let plain = serde_json::to_value(ChatMessage::assistant("汪!")).unwrap();
        assert_eq!(plain, serde_json::json!({ "role": "assistant", "content": "汪!" }));
    }
}
