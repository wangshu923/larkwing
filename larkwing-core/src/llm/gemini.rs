//! Google Gemini 原生 provider(generateContent REST 方言,非 OpenAI 兼容)。
//!
//! 为什么下楼写原生(宪法 §4 reasoning 保真铁律):Gemini 的 `thoughtSignature` 是
//! **不透明、必须逐字往返**的状态,OpenAI 兼容层无处安放 —— 走兼容端点开思考会**静默降质**
//! (G2.5)或 400(G3)。本实现把签名经中立 `reasoning_state` 载体原样存取,保真不丢。
//!
//! 方言要点(考古 robot `gemini.py` + 官方文档):
//! - finishReason 永远 STOP,工具意图靠"有没有 functionCall"判(本实现据此置 stop_reason);
//! - JSON Schema 砍半:functionDeclarations 只认一小撮关键字,其余必剥否则 400(`clean_schema`);
//! - tool 结果要函数**名字**不只 id → 走 tool_id→name 前向映射;Gemini 不返回 call id → 自造;
//! - thoughtSignature 贴在**具体 functionCall part**,按 call id 存进 reasoning_state、回放原位;
//! - 系统提示走 systemInstruction 独立字段;REST 字段 camelCase。

use futures_util::StreamExt;
use serde_json::{json, Map, Value};
use tokio::sync::mpsc;

use super::sse::LineBuffer;
use super::{ChatEvent, ChatRequest, LlmConfig, LlmError, LlmProvider, Thinking, ToolCall, Usage};

const IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// reasoning_state 里 Gemini 签名的存放键:`{ "sig": { "<call_id>": "<b64>" } }`。
const SIG_KEY: &str = "sig";

pub struct GeminiProvider {
    cfg: LlmConfig,
    net: crate::net::Client,
}

impl GeminiProvider {
    pub fn new(cfg: LlmConfig) -> Self {
        let net = crate::net::Client::new(|b| b.connect_timeout(std::time::Duration::from_secs(10)));
        Self { cfg, net }
    }

    /// 出向方言翻译:中立 ChatRequest → generateContent 请求体。
    fn to_wire(&self, req: &ChatRequest) -> Value {
        let mut body = Map::new();

        if !req.system.is_empty() {
            body.insert(
                "systemInstruction".into(),
                json!({ "parts": [{ "text": req.system }] }),
            );
        }

        body.insert("contents".into(), Value::Array(self.contents(&req.messages)));

        if !req.tools.is_empty() {
            let decls: Vec<Value> = req
                .tools
                .iter()
                .map(|t| {
                    json!({
                        "name": t.name,
                        "description": t.description,
                        "parameters": clean_schema(&t.parameters),
                    })
                })
                .collect();
            body.insert("tools".into(), json!([{ "functionDeclarations": decls }]));
            // ToolChoice::None = 强制收尾:禁用函数调用(Gemini 的 tool_choice 对应物)
            if req.tool_choice == super::ToolChoice::None {
                body.insert(
                    "toolConfig".into(),
                    json!({ "functionCallingConfig": { "mode": "NONE" } }),
                );
            }
        }

        let mut gen = Map::new();
        if let Some(t) = req.options.temperature.or(self.cfg.temperature) {
            gen.insert("temperature".into(), json!(t));
        }
        if let Some(m) = req.options.max_tokens {
            gen.insert("maxOutputTokens".into(), json!(m));
        }
        // 思考档:Off → 关死(thinkingBudget=0);开 → 要回思考摘要(includeThoughts)。
        let thinking = req.options.thinking.unwrap_or(self.cfg.thinking);
        match thinking {
            Thinking::Off => {
                gen.insert("thinkingConfig".into(), json!({ "thinkingBudget": 0 }));
            }
            _ => {
                gen.insert("thinkingConfig".into(), json!({ "includeThoughts": true }));
            }
        }
        if !gen.is_empty() {
            body.insert("generationConfig".into(), Value::Object(gen));
        }

        Value::Object(body)
    }

    /// messages → Gemini contents[]。维护 tool_id→name 前向映射(functionResponse 必填 name);
    /// 连续 ToolResult 合并进同一条 user content;Assistant 的签名按 call id 回贴到 functionCall part。
    fn contents(&self, messages: &[super::ChatMessage]) -> Vec<Value> {
        use super::ChatMessage;
        let mut out: Vec<Value> = Vec::new();
        let mut name_of: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let mut pending_tool: Vec<Value> = Vec::new(); // 攒连续 functionResponse

        let flush = |pending: &mut Vec<Value>, out: &mut Vec<Value>| {
            if !pending.is_empty() {
                out.push(json!({ "role": "user", "parts": std::mem::take(pending) }));
            }
        };

        for msg in messages {
            match msg {
                ChatMessage::User { content, parts } => {
                    flush(&mut pending_tool, &mut out);
                    let mut p: Vec<Value> = Vec::new();
                    if !content.is_empty() {
                        p.push(json!({ "text": content }));
                    }
                    for part in parts {
                        if let super::ContentPart::Text { text } = part {
                            p.push(json!({ "text": text }));
                        } else if let super::ContentPart::ImageUrl { url } = part {
                            // data: URL → inlineData;否则按 fileData uri(best-effort)
                            if let Some(inline) = data_url_to_inline(url) {
                                p.push(inline);
                            } else {
                                p.push(json!({ "fileData": { "fileUri": url } }));
                            }
                        }
                    }
                    if !p.is_empty() {
                        out.push(json!({ "role": "user", "parts": p }));
                    }
                }
                ChatMessage::Assistant { content, tool_calls, reasoning_state, .. } => {
                    flush(&mut pending_tool, &mut out);
                    let sigs = reasoning_state
                        .as_ref()
                        .and_then(|v| v.get(SIG_KEY))
                        .and_then(Value::as_object);
                    let mut p: Vec<Value> = Vec::new();
                    if !content.is_empty() {
                        p.push(json!({ "text": content }));
                    }
                    for call in tool_calls {
                        name_of.insert(call.id.clone(), call.name.clone());
                        let mut part = json!({
                            "functionCall": { "name": call.name, "args": call.args }
                        });
                        // 签名原位回贴(逐字保真,reasoning 保真铁律)
                        if let Some(sig) = sigs.and_then(|m| m.get(&call.id)).and_then(Value::as_str) {
                            part["thoughtSignature"] = json!(sig);
                        }
                        p.push(part);
                    }
                    if !p.is_empty() {
                        out.push(json!({ "role": "model", "parts": p }));
                    }
                }
                ChatMessage::ToolResult { call_id, content } => {
                    let name = name_of.get(call_id).cloned().unwrap_or_default();
                    pending_tool.push(json!({
                        "functionResponse": { "name": name, "response": { "result": content } }
                    }));
                }
            }
        }
        flush(&mut pending_tool, &mut out);
        out
    }

    fn url(&self, model: &str) -> String {
        format!(
            "{}/models/{}:streamGenerateContent?alt=sse",
            self.cfg.base_url.trim_end_matches('/'),
            model
        )
    }
}

/// Gemini Schema 只认一小撮关键字,其余必剥否则 400(考古 robot `_clean_schema`)。
/// 剥完再修 required:去掉已不在 properties 里的项,空则删 required。
fn clean_schema(schema: &Value) -> Value {
    const STRIP: &[&str] = &[
        "$schema", "$id", "$ref", "$comment", "$defs", "definitions",
        "exclusiveMinimum", "exclusiveMaximum", "multipleOf",
        "minLength", "maxLength", "pattern",
        "additionalProperties", "patternProperties",
        "oneOf", "allOf", "not", "if", "then", "else",
        "const", "examples", "readOnly", "writeOnly", "deprecated",
        "contentMediaType", "contentEncoding",
        "uniqueItems", "minProperties", "maxProperties",
        "prefixItems", "unevaluatedItems", "unevaluatedProperties",
    ];
    match schema {
        Value::Object(m) => {
            let mut out = Map::new();
            for (k, v) in m {
                if STRIP.contains(&k.as_str()) {
                    continue;
                }
                out.insert(k.clone(), clean_schema(v));
            }
            // required 与 properties 对齐:留下的 required 必须在 properties 里有定义
            if let (Some(Value::Array(req)), Some(Value::Object(props))) =
                (out.get("required").cloned(), out.get("properties"))
            {
                let kept: Vec<Value> = req
                    .into_iter()
                    .filter(|r| r.as_str().map(|s| props.contains_key(s)).unwrap_or(false))
                    .collect();
                if kept.is_empty() {
                    out.remove("required");
                } else {
                    out.insert("required".into(), Value::Array(kept));
                }
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(clean_schema).collect()),
        other => other.clone(),
    }
}

/// data: URL → Gemini inlineData part;非 data URL 返回 None。
fn data_url_to_inline(url: &str) -> Option<Value> {
    let rest = url.strip_prefix("data:")?;
    let (meta, b64) = rest.split_once(",")?;
    let mime = meta.split(';').next().unwrap_or("image/png");
    Some(json!({ "inlineData": { "mimeType": mime, "data": b64 } }))
}

/// finishReason 归一(robot 同款):STOP→end_turn / MAX_TOKENS→max_tokens / 其余透传小写。
/// 注意:有 functionCall 时上层覆盖为 tool_use(Gemini 自身不给工具 stop 值)。
fn normalize_finish(raw: &str) -> String {
    match raw {
        "STOP" => "end_turn".into(),
        "MAX_TOKENS" => "max_tokens".into(),
        other => other.to_ascii_lowercase(),
    }
}

#[async_trait::async_trait]
impl LlmProvider for GeminiProvider {
    fn model_id(&self) -> &str {
        &self.cfg.model
    }

    async fn chat_stream(&self, req: ChatRequest) -> Result<mpsc::Receiver<ChatEvent>, LlmError> {
        if self.cfg.api_key.trim().is_empty() {
            return Err(LlmError::NoApiKey);
        }
        let model = req.options.model.clone().unwrap_or_else(|| self.cfg.model.clone());
        let url = self.url(&model);
        let body = self.to_wire(&req);
        let key = self.cfg.api_key.clone();
        let resp = self
            .net
            .send(&url, |c| c.post(&url).header("x-goog-api-key", &key).json(&body))
            .await
            .map_err(|e| LlmError::Network(e.to_string()))?;

        let status = resp.status();
        if !status.is_success() {
            let message = resp.text().await.unwrap_or_default();
            return Err(match status.as_u16() {
                401 | 403 => LlmError::BadApiKey,
                s => LlmError::Api { status: s, message: message.chars().take(500).collect() },
            });
        }

        let (tx, rx) = mpsc::channel::<ChatEvent>(64);
        let mut stream = resp.bytes_stream();
        tokio::spawn(async move {
            let mut lines = LineBuffer::default();
            let mut usage = Usage::default();
            let mut finish: Option<String> = None;
            let mut calls: Vec<ToolCall> = Vec::new();
            let mut sigs: Map<String, Value> = Map::new(); // call_id → thoughtSignature(逐字)
            loop {
                let bytes = match tokio::time::timeout(IDLE_TIMEOUT, stream.next()).await {
                    Err(_) => {
                        let _ = tx
                            .send(ChatEvent::Failed(LlmError::Network("流空闲超时".into())))
                            .await;
                        return;
                    }
                    Ok(None) => {
                        finalize(&tx, usage, finish, calls, sigs).await;
                        return;
                    }
                    Ok(Some(Err(e))) => {
                        let _ = tx.send(ChatEvent::Failed(LlmError::Network(e.to_string()))).await;
                        return;
                    }
                    Ok(Some(Ok(b))) => b,
                };
                for line in lines.push(&bytes) {
                    let Some(data) = line.strip_prefix("data:") else { continue };
                    let data = data.trim();
                    if data.is_empty() {
                        continue;
                    }
                    let Ok(value) = serde_json::from_str::<Value>(data) else { continue };
                    if parse_chunk(&value, &tx, &mut usage, &mut finish, &mut calls, &mut sigs)
                        .await
                        .is_err()
                    {
                        return; // 接收端 drop:中止
                    }
                }
            }
        });
        Ok(rx)
    }
}

/// 解析一帧 generateContent SSE:吐 Delta/Thinking,攒 functionCall + 签名 + usage + finishReason。
/// Err = 下游 send 失败(取消),调用方应中止。
async fn parse_chunk(
    value: &Value,
    tx: &mpsc::Sender<ChatEvent>,
    usage: &mut Usage,
    finish: &mut Option<String>,
    calls: &mut Vec<ToolCall>,
    sigs: &mut Map<String, Value>,
) -> Result<(), ()> {
    if let Some(cand) = value.pointer("/candidates/0") {
        if let Some(parts) = cand.pointer("/content/parts").and_then(Value::as_array) {
            for part in parts {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    let is_thought = part.get("thought").and_then(Value::as_bool).unwrap_or(false);
                    let ev = if is_thought {
                        ChatEvent::Thinking(text.to_string())
                    } else {
                        ChatEvent::Delta(text.to_string())
                    };
                    tx.send(ev).await.map_err(|_| ())?;
                }
                if let Some(fc) = part.get("functionCall") {
                    let name = fc.get("name").and_then(Value::as_str).unwrap_or_default();
                    let args = fc.get("args").cloned().unwrap_or_else(|| json!({}));
                    let id = format!("gm_{}", calls.len());
                    // 签名贴在本 part:逐字存进 sigs,按 call id 键(回放原位)
                    if let Some(sig) = part.get("thoughtSignature").and_then(Value::as_str) {
                        sigs.insert(id.clone(), json!(sig));
                    }
                    calls.push(ToolCall {
                        id,
                        name: name.to_string(),
                        args,
                        is_incomplete: false,
                    });
                }
            }
        }
        if let Some(fr) = cand.get("finishReason").and_then(Value::as_str) {
            *finish = Some(normalize_finish(fr));
        }
    }
    if let Some(um) = value.get("usageMetadata") {
        let g = |k: &str| um.get(k).and_then(Value::as_u64).unwrap_or(0) as i64;
        usage.input_tokens = g("promptTokenCount");
        usage.output_tokens = g("candidatesTokenCount");
        usage.cache_hit_tokens = g("cachedContentTokenCount");
    }
    Ok(())
}

/// 流终:有 functionCall → stop_reason=tool_use;先发 ReasoningState(签名)再发 Done。
async fn finalize(
    tx: &mpsc::Sender<ChatEvent>,
    usage: Usage,
    finish: Option<String>,
    calls: Vec<ToolCall>,
    sigs: Map<String, Value>,
) {
    let stop_reason = if calls.is_empty() {
        finish.or_else(|| Some("end_turn".into()))
    } else {
        Some("tool_use".into())
    };
    if !sigs.is_empty() {
        let _ = tx.send(ChatEvent::ReasoningState(json!({ SIG_KEY: sigs }))).await;
    }
    let _ = tx.send(ChatEvent::Done { usage, stop_reason, tool_calls: calls }).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{ChatMessage, ChatOptions, ToolDef};

    fn cfg() -> LlmConfig {
        LlmConfig::gemini("k".into())
    }

    fn req(messages: Vec<ChatMessage>, tools: Vec<ToolDef>) -> ChatRequest {
        ChatRequest {
            system: "你是 7274".into(),
            messages,
            options: ChatOptions::default(),
            tools,
            tool_choice: super::super::ToolChoice::Auto,
        }
    }

    #[test]
    fn to_wire_system_contents_and_thinking_off() {
        let p = GeminiProvider::new(cfg());
        let wire = p.to_wire(&req(vec![ChatMessage::user("几点了")], vec![]));
        assert_eq!(wire["systemInstruction"]["parts"][0]["text"], "你是 7274");
        assert_eq!(wire["contents"][0]["role"], "user");
        assert_eq!(wire["contents"][0]["parts"][0]["text"], "几点了");
        // thinking=Off → thinkingBudget:0(关死,省钱)
        assert_eq!(wire["generationConfig"]["thinkingConfig"]["thinkingBudget"], 0);
    }

    #[test]
    fn clean_schema_strips_unsupported_keys_and_fixes_required() {
        let raw = json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "fact": { "type": "string", "minLength": 1, "pattern": "^.+$" }
            },
            "required": ["fact", "ghost"]
        });
        let cleaned = clean_schema(&raw);
        assert!(cleaned.get("additionalProperties").is_none(), "additionalProperties 必剥");
        assert!(cleaned["properties"]["fact"].get("minLength").is_none(), "minLength 必剥");
        assert!(cleaned["properties"]["fact"].get("pattern").is_none(), "pattern 必剥");
        // required 去掉 properties 里没有的 ghost,只留 fact
        assert_eq!(cleaned["required"], json!(["fact"]));
    }

    #[test]
    fn tool_choice_none_disables_function_calling() {
        let p = GeminiProvider::new(cfg());
        let tool = ToolDef {
            name: "now".into(),
            description: "时间".into(),
            parameters: json!({ "type": "object", "properties": {} }),
        };
        let mut r = req(vec![ChatMessage::user("hi")], vec![tool]);
        r.tool_choice = super::super::ToolChoice::None;
        let wire = p.to_wire(&r);
        assert_eq!(wire["toolConfig"]["functionCallingConfig"]["mode"], "NONE");
    }

    // 核心保真:历史 Assistant 的 thoughtSignature 经 reasoning_state 原位回贴到 functionCall part
    #[test]
    fn thought_signature_round_trips_onto_function_call_part() {
        let p = GeminiProvider::new(cfg());
        let assistant = ChatMessage::Assistant {
            content: String::new(),
            reasoning: None,
            tool_calls: vec![ToolCall {
                id: "gm_0".into(),
                name: "now".into(),
                args: json!({}),
                is_incomplete: false,
            }],
            reasoning_state: Some(json!({ "sig": { "gm_0": "OPAQUE_SIG_B64" } })),
        };
        let wire = p.to_wire(&req(
            vec![
                ChatMessage::user("几点"),
                assistant,
                ChatMessage::ToolResult { call_id: "gm_0".into(), content: "12:00".into() },
            ],
            vec![],
        ));
        let model_turn = &wire["contents"][1];
        assert_eq!(model_turn["role"], "model");
        let fc_part = &model_turn["parts"][0];
        assert_eq!(fc_part["functionCall"]["name"], "now");
        assert_eq!(fc_part["thoughtSignature"], "OPAQUE_SIG_B64", "签名必须逐字回贴原位");
        // tool 结果合进 user content,functionResponse 带回函数名(前向映射)
        let tool_turn = &wire["contents"][2];
        assert_eq!(tool_turn["role"], "user");
        assert_eq!(tool_turn["parts"][0]["functionResponse"]["name"], "now");
        assert_eq!(tool_turn["parts"][0]["functionResponse"]["response"]["result"], "12:00");
    }

    #[tokio::test]
    async fn parse_chunk_emits_text_and_collects_function_call_with_signature() {
        let (tx, mut rx) = mpsc::channel::<ChatEvent>(16);
        let mut usage = Usage::default();
        let mut finish = None;
        let mut calls = Vec::new();
        let mut sigs = Map::new();
        // 一帧:文本 + 一个带签名的 functionCall + usage
        let chunk = json!({
            "candidates": [{
                "content": { "parts": [
                    { "text": "好的" },
                    { "functionCall": { "name": "now", "args": {} }, "thoughtSignature": "SIG123" }
                ]},
                "finishReason": "STOP"
            }],
            "usageMetadata": { "promptTokenCount": 10, "candidatesTokenCount": 3 }
        });
        parse_chunk(&chunk, &tx, &mut usage, &mut finish, &mut calls, &mut sigs).await.unwrap();
        drop(tx);
        // 文本 → Delta
        match rx.recv().await.unwrap() {
            ChatEvent::Delta(t) => assert_eq!(t, "好的"),
            other => panic!("期望 Delta,得到 {other:?}"),
        }
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "now");
        assert_eq!(sigs["gm_0"], "SIG123", "签名按 call id 攒下");
        assert_eq!(usage.input_tokens, 10);
    }
}
