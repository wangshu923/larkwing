//! Anthropic 兼容 provider(Messages API 方言)。与 openai_compat 同构:
//! 出向 to_wire / 入向 parse_event 止步本文件;官方 Anthropic = 一组配置,
//! 各家 Anthropic 兼容端点同样只换配置(差异走 Quirks)。

use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use super::sse::LineBuffer;
use super::{AuthStyle, ChatEvent, ChatRequest, LlmConfig, LlmError, LlmProvider, Thinking, Usage};

const IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Messages API 必填 max_tokens;未指定时的产品默认。
const DEFAULT_MAX_TOKENS: u32 = 8192;

/// data:image/png;base64,XXXX → Anthropic base64 image block;非 data URL → url source。
fn anthropic_image_block(url: &str) -> Value {
    if let Some(rest) = url.strip_prefix("data:") {
        if let Some((meta, data)) = rest.split_once(',') {
            let media_type = meta.split(';').next().unwrap_or("image/jpeg");
            return json!({
                "type": "image",
                "source": { "type": "base64", "media_type": media_type, "data": data }
            });
        }
    }
    json!({ "type": "image", "source": { "type": "url", "url": url } })
}

pub struct AnthropicCompatProvider {
    cfg: LlmConfig,
    net: crate::net::Client,
}

impl AnthropicCompatProvider {
    pub fn new(cfg: LlmConfig) -> Self {
        let net = crate::net::Client::new(|b| b.connect_timeout(std::time::Duration::from_secs(10)));
        Self { cfg, net }
    }

    /// 出向方言翻译:中立 ChatRequest → Messages API 请求体。
    /// system 翻成顶层参数,且打 cache_control 标(§7:稳定大前缀吃显式缓存;
    /// 不支持的兼容端点会忽略,无害)。空 system 整个省掉 —— 空文本块会被严格端点 400。
    fn to_wire(&self, req: &ChatRequest) -> Value {
        // 工具结果携图只对视觉模型有意义(catalog::vision;Claude 全系为真)——非视觉降级为纯文本。
        let vision = super::catalog::supports_vision(&self.cfg.model);
        let messages: Vec<Value> = req
            .messages
            .iter()
            .map(|m| match m {
                super::ChatMessage::User { content, parts } => {
                    if parts.is_empty() {
                        json!({ "role": "user", "content": content })
                    } else {
                        let mut blocks = Vec::with_capacity(parts.len() + 1);
                        if !content.is_empty() {
                            blocks.push(json!({ "type": "text", "text": content }));
                        }
                        for p in parts {
                            blocks.push(match p {
                                super::ContentPart::Text { text } => {
                                    json!({ "type": "text", "text": text })
                                }
                                super::ContentPart::ImageUrl { url } => anthropic_image_block(url),
                            });
                        }
                        json!({ "role": "user", "content": blocks })
                    }
                }
                // reasoning 不回传:Anthropic 的 thinking block 要求原始签名,无法合成;
                // 思考轮回传策略是 B 期联调课题(PLAN §8 watch-item)
                super::ChatMessage::Assistant { content, tool_calls, .. } => {
                    if tool_calls.is_empty() {
                        json!({ "role": "assistant", "content": content })
                    } else {
                        let mut blocks = Vec::with_capacity(tool_calls.len() + 1);
                        if !content.is_empty() {
                            blocks.push(json!({ "type": "text", "text": content }));
                        }
                        for c in tool_calls {
                            blocks.push(json!({
                                "type": "tool_use", "id": c.id, "name": c.name, "input": c.args
                            }));
                        }
                        json!({ "role": "assistant", "content": blocks })
                    }
                }
                // Anthropic 方言:工具结果是 user 消息里的 tool_result block。带图时 tool_result
                // 的 content 由字符串升成数组(text block + image block),Anthropic 原生支持
                // (computer use 同款);无图 / 非视觉则保持字符串(出向字节不变,吃前缀缓存;
                // 非视觉丢图由 tool_result_text 如实留话)。
                super::ChatMessage::ToolResult { call_id, content, parts } => {
                    let inner = if parts.is_empty() || !vision {
                        json!(super::tool_result_text(content, parts, vision))
                    } else {
                        let mut blocks = Vec::with_capacity(parts.len() + 1);
                        blocks.push(json!({ "type": "text", "text": content }));
                        for p in parts {
                            if let super::ContentPart::ImageUrl { url } = p {
                                blocks.push(anthropic_image_block(url));
                            }
                        }
                        json!(blocks)
                    };
                    json!({
                        "role": "user",
                        "content": [{ "type": "tool_result", "tool_use_id": call_id, "content": inner }]
                    })
                }
            })
            .collect();
        // 思考档位 → budget(API 硬约束:budget ≥ 1024 且 < max_tokens,开思考时抬高 max_tokens 容下它)
        let budget: u32 = match req.options.thinking.unwrap_or(self.cfg.thinking) {
            Thinking::Off => 0,
            Thinking::Light => 1024,
            Thinking::Medium => 4096,
            Thinking::Heavy => 16384, // 各家 high/max 档的对应物
        };
        let max_tokens = req.options.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);
        let max_tokens = if budget > 0 { max_tokens.max(budget * 2) } else { max_tokens };
        let mut body = json!({
            "model": req.options.model.as_deref().unwrap_or(&self.cfg.model),
            "max_tokens": max_tokens,
            "messages": messages,
            "stream": true,
        });
        if !req.system.is_empty() {
            body["system"] = json!([{
                "type": "text",
                "text": req.system,
                "cache_control": { "type": "ephemeral" }
            }]);
        }
        // 与 OpenAI 方言相反:thinking 只在开启时出现,关闭 = 不带字段。
        // 开思考时不带 temperature —— API 拒绝二者同时出现。
        if budget > 0 {
            body["thinking"] = json!({ "type": "enabled", "budget_tokens": budget });
        } else if let Some(t) = req.options.temperature.or(self.cfg.temperature) {
            body["temperature"] = json!(t);
        }
        if !req.tools.is_empty() {
            body["tools"] = Value::Array(
                req.tools
                    .iter()
                    .map(|t| {
                        json!({
                            "name": t.name,
                            "description": t.description,
                            "input_schema": t.parameters,
                        })
                    })
                    .collect(),
            );
            if req.tool_choice == super::ToolChoice::None {
                body["tool_choice"] = json!({ "type": "none" });
            }
        }
        body
    }

    fn request_builder(&self, client: &reqwest::Client, url: &str) -> reqwest::RequestBuilder {
        let mut builder = client.post(url).header("anthropic-version", "2023-06-01");
        builder = match self.cfg.quirks.auth.unwrap_or(AuthStyle::XApiKey) {
            AuthStyle::Bearer => builder.bearer_auth(&self.cfg.api_key),
            AuthStyle::XApiKey => builder.header("x-api-key", &self.cfg.api_key),
            AuthStyle::ApiKeyHeader => builder.header("api-key", &self.cfg.api_key),
        };
        for (k, v) in &self.cfg.quirks.extra_headers {
            builder = builder.header(k, v);
        }
        builder
    }
}

/// stop_reason 归一:Anthropic 词表大半已是中立词;stop_sequence 视作正常收尾,其余透传。
fn normalize_stop_reason(raw: &str) -> String {
    match raw {
        "stop_sequence" => "end_turn".into(),
        other => other.into(), // end_turn / max_tokens / tool_use 本就是中立词
    }
}

/// tool_use block 攒桶:Anthropic 流里 id/name 随 content_block_start 完整到达,
/// 只有 input 走 input_json_delta 碎片 —— 比 OpenAI 系斯文,但同样按 index 开桶。
#[derive(Default)]
struct ToolUseAcc {
    id: String,
    name: String,
    json: String,
}

fn finalize_tool_calls(
    calls: std::collections::BTreeMap<u64, ToolUseAcc>,
    stop_reason: Option<&str>,
) -> Vec<super::ToolCall> {
    calls
        .into_iter()
        .map(|(idx, acc)| {
            let id = if acc.id.is_empty() { format!("call_{idx}") } else { acc.id };
            let (args, is_incomplete) = super::parse_tool_args(&acc.json, stop_reason, &acc.name);
            super::ToolCall { id, name: acc.name, args, is_incomplete }
        })
        .collect()
}

/// 入向方言翻译:一个 SSE data 块 → 0..n 个事件。防御原则与 openai_compat 一致:
/// - usage 分两头到(message_start 带 input/cache,message_delta 带 output),已有值不被砸回 0;
/// - 未知事件类型一律忽略(ping / 未来新事件);
/// - error 事件立刻 Failed(坑 #7 的 Anthropic 形态);
/// - message_stop 产出 Done(携带攒完的 tool_calls)—— 终结事件由调用方识别后收摊。
fn parse_event(
    value: &Value,
    usage: &mut Usage,
    finish: &mut Option<String>,
    calls: &mut std::collections::BTreeMap<u64, ToolUseAcc>,
) -> Vec<ChatEvent> {
    let mut events = Vec::new();
    match value.get("type").and_then(Value::as_str) {
        Some("error") => {
            let message = value
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("provider 在流中返回错误")
                .to_string();
            events.push(ChatEvent::Failed(LlmError::Api { status: 0, message }));
        }
        Some("message_start") => {
            if let Some(u) = value.pointer("/message/usage") {
                usage.input_tokens =
                    u.get("input_tokens").and_then(Value::as_i64).unwrap_or(usage.input_tokens);
                usage.cache_hit_tokens = u
                    .get("cache_read_input_tokens")
                    .and_then(Value::as_i64)
                    .unwrap_or(usage.cache_hit_tokens);
            }
        }
        Some("content_block_start") => {
            // tool_use block 开桶:id/name 完整到达,input 后续走 input_json_delta
            if value.pointer("/content_block/type").and_then(Value::as_str) == Some("tool_use") {
                let idx = value.get("index").and_then(Value::as_u64).unwrap_or(0);
                let acc = calls.entry(idx).or_default();
                if let Some(id) = value.pointer("/content_block/id").and_then(Value::as_str) {
                    acc.id = id.into();
                }
                if let Some(n) = value.pointer("/content_block/name").and_then(Value::as_str) {
                    acc.name = n.into();
                }
            }
        }
        Some("content_block_delta") => match value.pointer("/delta/type").and_then(Value::as_str) {
            Some("text_delta") => {
                if let Some(t) = value.pointer("/delta/text").and_then(Value::as_str) {
                    if !t.is_empty() {
                        events.push(ChatEvent::Delta(t.to_string()));
                    }
                }
            }
            Some("thinking_delta") => {
                if let Some(t) = value.pointer("/delta/thinking").and_then(Value::as_str) {
                    if !t.is_empty() {
                        events.push(ChatEvent::Thinking(t.to_string()));
                    }
                }
            }
            Some("input_json_delta") => {
                if let Some(j) = value.pointer("/delta/partial_json").and_then(Value::as_str) {
                    let idx = value.get("index").and_then(Value::as_u64).unwrap_or(0);
                    calls.entry(idx).or_default().json.push_str(j);
                }
            }
            _ => {} // signature_delta 等:忽略
        },
        Some("message_delta") => {
            if let Some(reason) = value.pointer("/delta/stop_reason").and_then(Value::as_str) {
                *finish = Some(normalize_stop_reason(reason));
            }
            if let Some(n) = value.pointer("/usage/output_tokens").and_then(Value::as_i64) {
                usage.output_tokens = n;
            }
        }
        Some("message_stop") => {
            let stop_reason = finish.take();
            let tool_calls =
                finalize_tool_calls(std::mem::take(calls), stop_reason.as_deref());
            events.push(ChatEvent::Done { usage: usage.clone(), stop_reason, tool_calls });
        }
        _ => {} // ping / content_block_stop / 未知事件:忽略
    }
    events
}

fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

#[async_trait::async_trait]
impl LlmProvider for AnthropicCompatProvider {
    fn model_id(&self) -> &str {
        &self.cfg.model
    }

    async fn chat_stream(&self, req: ChatRequest) -> Result<mpsc::Receiver<ChatEvent>, LlmError> {
        if self.cfg.api_key.trim().is_empty() {
            return Err(LlmError::NoApiKey);
        }
        let url = format!("{}/v1/messages", self.cfg.base_url.trim_end_matches('/'));
        let body = self.to_wire(&req);
        let resp = self
            .net
            .send(&url, |c| self.request_builder(c, &url).json(&body))
            .await
            .map_err(|e| LlmError::Network(e.to_string()))?;

        let status = resp.status();
        if !status.is_success() {
            let message = resp.text().await.unwrap_or_default();
            return Err(match status.as_u16() {
                401 | 403 => LlmError::BadApiKey,
                s => LlmError::Api {
                    status: s,
                    message: truncate_chars(&message, 500),
                },
            });
        }

        let (tx, rx) = mpsc::channel::<ChatEvent>(64);
        let mut stream = resp.bytes_stream();
        tokio::spawn(async move {
            let mut lines = LineBuffer::default();
            let mut usage = Usage::default();
            let mut finish: Option<String> = None;
            let mut calls = std::collections::BTreeMap::new(); // tool_use block 攒桶
            loop {
                let bytes = match tokio::time::timeout(IDLE_TIMEOUT, stream.next()).await {
                    Err(_) => {
                        let _ = tx
                            .send(ChatEvent::Failed(LlmError::Network("流空闲超时".into())))
                            .await;
                        return;
                    }
                    // 流自然结束但没收到 message_stop:容忍,按完成处理
                    Ok(None) => {
                        let stop_reason = finish.take();
                        let tool_calls = finalize_tool_calls(calls, stop_reason.as_deref());
                        let _ = tx.send(ChatEvent::Done { usage, stop_reason, tool_calls }).await;
                        return;
                    }
                    Ok(Some(Err(e))) => {
                        let _ = tx.send(ChatEvent::Failed(LlmError::Network(e.to_string()))).await;
                        return;
                    }
                    Ok(Some(Ok(bytes))) => bytes,
                };
                for line in lines.push(&bytes) {
                    // 只认 data: 行;event: 行冗余(data 里有 type 字段),注释/keep-alive 忽略
                    let Some(data) = line.strip_prefix("data:") else {
                        continue;
                    };
                    let Ok(value) = serde_json::from_str::<Value>(data.trim()) else {
                        continue;
                    };
                    for ev in parse_event(&value, &mut usage, &mut finish, &mut calls) {
                        // Done / Failed 都是终结事件:发完即收摊,连接随 stream drop 断开
                        let terminal = matches!(ev, ChatEvent::Done { .. } | ChatEvent::Failed(_));
                        if tx.send(ev).await.is_err() || terminal {
                            return;
                        }
                    }
                }
            }
        });
        Ok(rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{ChatMessage, ChatOptions, ToolCall};

    fn provider() -> AnthropicCompatProvider {
        AnthropicCompatProvider::new(LlmConfig::anthropic("sk-ant-test".into()))
    }

    // 媒体输入:带图 user 出向成 Anthropic content blocks(text + base64 image)
    #[test]
    fn to_wire_user_with_image_becomes_blocks() {
        use crate::llm::ContentPart;
        let p = provider();
        let wire = p.to_wire(&ChatRequest {
            messages: vec![ChatMessage::user_with_parts(
                "这是什么菜",
                vec![ContentPart::ImageUrl { url: "data:image/jpeg;base64,AAAA".into() }],
            )],
            ..Default::default()
        });
        let content = &wire["messages"][0]["content"];
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "image");
        assert_eq!(content[1]["source"]["type"], "base64");
        assert_eq!(content[1]["source"]["media_type"], "image/jpeg");
        assert_eq!(content[1]["source"]["data"], "AAAA");
    }

    // 工具结果多媒体:带图 tool_result 的 content 由字符串升成数组(text + image block)。
    #[test]
    fn to_wire_tool_result_with_image_becomes_blocks() {
        use crate::llm::ContentPart;
        let p = provider(); // Claude = 视觉
        let wire = p.to_wire(&ChatRequest {
            messages: vec![ChatMessage::ToolResult {
                call_id: "c1".into(),
                content: "这是页面截图".into(),
                parts: vec![ContentPart::ImageUrl { url: "data:image/png;base64,BBBB".into() }],
            }],
            ..Default::default()
        });
        let tr = &wire["messages"][0]["content"][0];
        assert_eq!(tr["type"], "tool_result");
        assert_eq!(tr["tool_use_id"], "c1");
        assert_eq!(tr["content"][0]["type"], "text");
        assert_eq!(tr["content"][0]["text"], "这是页面截图");
        assert_eq!(tr["content"][1]["type"], "image");
        assert_eq!(tr["content"][1]["source"]["media_type"], "image/png");
        assert_eq!(tr["content"][1]["source"]["data"], "BBBB");
        // 无图 tool_result:content 仍是字符串(出向字节不变,零缓存回归)
        let plain = p.to_wire(&ChatRequest {
            messages: vec![ChatMessage::tool_result("c1", "ok")],
            ..Default::default()
        });
        assert_eq!(plain["messages"][0]["content"][0]["content"], "ok");
        // 非视觉模型(目录未知按 false):图不嵌、content 回字符串,但丢图必留话
        let mut cfg = LlmConfig::anthropic("sk-ant-test".into());
        cfg.model = "unknown-text-model".into();
        let nv = AnthropicCompatProvider::new(cfg);
        let wire = nv.to_wire(&ChatRequest {
            messages: vec![ChatMessage::ToolResult {
                call_id: "c1".into(),
                content: "这是页面截图".into(),
                parts: vec![ContentPart::ImageUrl { url: "data:image/png;base64,BBBB".into() }],
            }],
            ..Default::default()
        });
        let inner = wire["messages"][0]["content"][0]["content"].as_str().unwrap();
        assert!(inner.starts_with("这是页面截图"), "{inner}");
        assert!(inner.contains("没能传给当前模型"), "{inner}");
    }

    fn chunk(json: &str) -> Value {
        serde_json::from_str(json).unwrap()
    }

    /// 旧式三参便捷壳:多数测试不关心 tool_use 攒桶。
    fn parse(v: &Value, usage: &mut Usage, finish: &mut Option<String>) -> Vec<ChatEvent> {
        let mut calls = std::collections::BTreeMap::new();
        parse_event(v, usage, finish, &mut calls)
    }

    #[test]
    fn to_wire_puts_system_toplevel_with_cache_control() {
        let p = provider();
        let req = ChatRequest {
            system: "你是 7274".into(),
            messages: vec![ChatMessage::user("你好")],
            options: ChatOptions::default(),
            ..Default::default()
        };
        let wire = p.to_wire(&req);
        assert_eq!(wire["system"][0]["text"], "你是 7274");
        assert_eq!(wire["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(wire["messages"][0]["role"], "user");
        assert_eq!(wire["max_tokens"], DEFAULT_MAX_TOKENS, "Messages API 必填 max_tokens");
        // 与 OpenAI 方言相反:关思考 = 不带字段
        assert!(wire.get("thinking").is_none());
    }

    #[test]
    fn to_wire_thinking_tiers_map_to_budgets_and_drop_temperature() {
        let p = provider();
        let wire_for = |lvl: Thinking, max_tokens: Option<u32>| {
            p.to_wire(&ChatRequest {
                options: ChatOptions {
                    thinking: Some(lvl),
                    max_tokens,
                    temperature: Some(0.7), // 开思考时 API 拒绝 temperature:必须省掉
                    ..Default::default()
                },
                ..Default::default()
            })
        };
        // 三档 budget:轻 1024 / 中 4096 / 重 16384(各家 high/max 的对应物)
        for (lvl, want) in
            [(Thinking::Light, 1024), (Thinking::Medium, 4096), (Thinking::Heavy, 16384)]
        {
            // max_tokens 给小值:必须被抬到 budget*2,不许发非法 budget
            let wire = wire_for(lvl, Some(1024));
            assert_eq!(wire["thinking"]["budget_tokens"], want);
            let max_tokens = wire["max_tokens"].as_u64().unwrap();
            assert!(max_tokens >= (want as u64) * 2, "max_tokens {max_tokens} 必须容下 budget {want}");
            assert!(wire.get("temperature").is_none(), "thinking 与 temperature 互斥");
        }
        // 空 system:整个字段省掉,不发空文本块
        assert!(wire_for(Thinking::Light, None).get("system").is_none());
        // Off:不带 thinking 字段,temperature 回归
        let wire = wire_for(Thinking::Off, None);
        assert!(wire.get("thinking").is_none());
        assert_eq!(wire["max_tokens"], DEFAULT_MAX_TOKENS);
        assert!((wire["temperature"].as_f64().unwrap() - 0.7).abs() < 1e-6);
    }

    // 工具轮回传的出向形状(B 期接入,翻译先钉死):tool_use 在 assistant blocks,
    // tool_result 在 user 消息的 blocks 里(Anthropic 方言),input 是 JSON 对象非字符串
    #[test]
    fn to_wire_translates_tool_round_messages() {
        let p = provider();
        let req = ChatRequest {
            system: String::new(),
            messages: vec![
                ChatMessage::user("我对花生过敏"),
                ChatMessage::Assistant {
                    content: "让我记一下".into(),
                    reasoning: None,
                    tool_calls: vec![ToolCall {
                        id: "call_1".into(),
                        name: "remember".into(),
                        args: serde_json::json!({ "fact": "对花生过敏" }),
                        is_incomplete: false,
                    }],
                    reasoning_state: None,
                },
                ChatMessage::tool_result("call_1", "ok"),
            ],
            options: ChatOptions::default(),
            ..Default::default()
        };
        let wire = p.to_wire(&req);
        let blocks = &wire["messages"][1]["content"];
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], "让我记一下");
        assert_eq!(blocks[1]["type"], "tool_use");
        assert_eq!(blocks[1]["id"], "call_1");
        assert_eq!(blocks[1]["name"], "remember");
        assert_eq!(blocks[1]["input"]["fact"], "对花生过敏");
        assert_eq!(wire["messages"][2]["role"], "user");
        assert_eq!(wire["messages"][2]["content"][0]["type"], "tool_result");
        assert_eq!(wire["messages"][2]["content"][0]["tool_use_id"], "call_1");

        // 无 tool_calls 的 assistant:仍是纯字符串 content,不升格 blocks
        let plain = p.to_wire(&ChatRequest {
            system: String::new(),
            messages: vec![ChatMessage::assistant("哔!")],
            options: ChatOptions::default(),
            ..Default::default()
        });
        assert_eq!(plain["messages"][0]["content"], "哔!");
    }

    #[test]
    fn request_builder_defaults_to_x_api_key_with_version_header() {
        let p = provider();
        let req = p
            .request_builder(p.net.direct(), "https://api.anthropic.com/v1/messages")
            .build()
            .unwrap();
        assert_eq!(req.headers()["x-api-key"], "sk-ant-test");
        assert_eq!(req.headers()["anthropic-version"], "2023-06-01");
        assert!(req.headers().get("authorization").is_none());
    }

    // 事件全流程:message_start 的 input/cache → 文本与思考增量 → message_delta 锁存
    // stop_reason 与 output → message_stop 产出携带完整 usage 的 Done
    #[test]
    fn event_sequence_accumulates_usage_and_finishes() {
        let mut usage = Usage::default();
        let mut finish = None;
        assert!(parse(
            &chunk(r#"{"type":"message_start","message":{"usage":{"input_tokens":100,"cache_read_input_tokens":64}}}"#),
            &mut usage, &mut finish,
        ).is_empty());
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.cache_hit_tokens, 64);

        let evs = parse(
            &chunk(r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"想想"}}"#),
            &mut usage, &mut finish,
        );
        assert!(matches!(&evs[0], ChatEvent::Thinking(t) if t == "想想"));

        let evs = parse(
            &chunk(r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"哔!"}}"#),
            &mut usage, &mut finish,
        );
        assert!(matches!(&evs[0], ChatEvent::Delta(t) if t == "哔!"));

        assert!(parse(
            &chunk(r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":9}}"#),
            &mut usage, &mut finish,
        ).is_empty());

        let evs = parse(&chunk(r#"{"type":"message_stop"}"#), &mut usage, &mut finish);
        match &evs[0] {
            ChatEvent::Done { usage, stop_reason, .. } => {
                assert_eq!(usage.input_tokens, 100);
                assert_eq!(usage.output_tokens, 9);
                assert_eq!(usage.cache_hit_tokens, 64);
                assert_eq!(stop_reason.as_deref(), Some("end_turn"));
            }
            other => panic!("应是 Done,实际 {other:?}"),
        }
    }

    // 工具流全程:content_block_start 开桶(id/name 完整)→ input_json_delta 碎片拼接
    // → message_delta 锁存 tool_use → message_stop 产出携带 tool_calls 的 Done
    #[test]
    fn tool_use_blocks_reassemble_into_done() {
        let mut usage = Usage::default();
        let mut finish = None;
        let mut calls = std::collections::BTreeMap::new();
        parse_event(
            &chunk(
                r#"{"type":"content_block_start","index":1,
                    "content_block":{"type":"tool_use","id":"toolu_a","name":"remember","input":{}}}"#,
            ),
            &mut usage, &mut finish, &mut calls,
        );
        for frag in [r#"{"fact":"#, r#""对花生过敏"}"#] {
            parse_event(
                &chunk(&format!(
                    r#"{{"type":"content_block_delta","index":1,
                        "delta":{{"type":"input_json_delta","partial_json":{}}}}}"#,
                    serde_json::to_string(frag).unwrap()
                )),
                &mut usage, &mut finish, &mut calls,
            );
        }
        parse_event(
            &chunk(r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":7}}"#),
            &mut usage, &mut finish, &mut calls,
        );
        let evs = parse_event(&chunk(r#"{"type":"message_stop"}"#), &mut usage, &mut finish, &mut calls);
        match &evs[0] {
            ChatEvent::Done { stop_reason, tool_calls, .. } => {
                assert_eq!(stop_reason.as_deref(), Some("tool_use"));
                assert_eq!(tool_calls.len(), 1);
                assert_eq!(tool_calls[0].id, "toolu_a");
                assert_eq!(tool_calls[0].name, "remember");
                assert_eq!(tool_calls[0].args["fact"], "对花生过敏");
                assert!(!tool_calls[0].is_incomplete);
            }
            other => panic!("应是 Done,实际 {other:?}"),
        }
    }

    // tools / tool_choice 出向:input_schema 字段名;None 发 {"type":"none"}
    #[test]
    fn to_wire_translates_tools_and_tool_choice() {
        let p = provider();
        let mut req = ChatRequest {
            tools: vec![crate::llm::ToolDef {
                name: "now".into(),
                description: "查看当前时间".into(),
                parameters: serde_json::json!({ "type": "object", "properties": {} }),
            }],
            ..Default::default()
        };
        let wire = p.to_wire(&req);
        assert_eq!(wire["tools"][0]["name"], "now");
        assert!(wire["tools"][0].get("input_schema").is_some());
        assert!(wire.get("tool_choice").is_none(), "Auto 不发字段");
        req.tool_choice = crate::llm::ToolChoice::None;
        assert_eq!(p.to_wire(&req)["tool_choice"]["type"], "none");
    }

    // 坑 #7 的 Anthropic 形态:error 事件立刻 Failed
    #[test]
    fn error_event_turns_into_failed() {
        let mut usage = Usage::default();
        let mut finish = None;
        let evs = parse(
            &chunk(r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#),
            &mut usage, &mut finish,
        );
        assert!(matches!(&evs[0],
            ChatEvent::Failed(LlmError::Api { message, .. }) if message.contains("Overloaded")));
    }

    // ping / content_block_start / 未知事件:忽略不出事
    #[test]
    fn unknown_and_housekeeping_events_are_ignored() {
        let mut usage = Usage::default();
        let mut finish = None;
        for raw in [
            r#"{"type":"ping"}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"some_future_event","payload":{}}"#,
        ] {
            assert!(parse(&chunk(raw), &mut usage, &mut finish).is_empty(), "{raw} 应被忽略");
        }
    }

    #[test]
    fn stop_reason_normalization() {
        assert_eq!(normalize_stop_reason("stop_sequence"), "end_turn");
        assert_eq!(normalize_stop_reason("end_turn"), "end_turn");
        assert_eq!(normalize_stop_reason("max_tokens"), "max_tokens");
        assert_eq!(normalize_stop_reason("refusal"), "refusal", "未知值透传");
    }
}
