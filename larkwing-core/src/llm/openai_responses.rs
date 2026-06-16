//! OpenAI Responses API 原生 provider(非 Chat Completions)。
//!
//! 为什么下楼写原生(宪法 §4 reasoning 保真铁律):OpenAI 推理模型的 reasoning 状态在
//! **Chat Completions 里根本没有槽位**——跨工具轮无法回传 → 静默降质。Responses API 才有
//! reasoning item(stateless 下带 `encrypted_content`);本实现 `store=false` +
//! `include:["reasoning.encrypted_content"]`,把 reasoning item 经中立 reasoning_state 逐字往返。
//!
//! 事件型 SSE:每帧 `data: {"type": "...", ...}`。只认文档化核心事件,其余忽略。
//! 真钥匙验收:事件名/item 形状以官方为准,无 key 验不了流式细节(见 PLAN §3 真机验收单)。

use futures_util::StreamExt;
use serde_json::{json, Map, Value};
use tokio::sync::mpsc;

use super::sse::LineBuffer;
use super::{ChatEvent, ChatRequest, LlmConfig, LlmError, LlmProvider, ToolCall, Usage};

const IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// reasoning_state 里 OpenAI reasoning item 的存放键:`{ "items": [ <原始 reasoning item> ] }`。
const ITEMS_KEY: &str = "items";

pub struct OpenAiResponsesProvider {
    cfg: LlmConfig,
    net: crate::net::Client,
}

impl OpenAiResponsesProvider {
    pub fn new(cfg: LlmConfig) -> Self {
        let net = crate::net::Client::new(|b| b.connect_timeout(std::time::Duration::from_secs(10)));
        Self { cfg, net }
    }

    /// 出向方言翻译:中立 ChatRequest → Responses API 请求体。
    fn to_wire(&self, req: &ChatRequest) -> Value {
        let mut body = Map::new();
        body.insert(
            "model".into(),
            json!(req.options.model.as_deref().unwrap_or(&self.cfg.model)),
        );
        if !req.system.is_empty() {
            body.insert("instructions".into(), json!(req.system));
        }
        body.insert("input".into(), Value::Array(self.input_items(&req.messages)));
        body.insert("stream".into(), json!(true));
        // stateless 回放:不让服务端存,改由我们逐字回传 reasoning item 的 encrypted_content
        body.insert("store".into(), json!(false));
        body.insert("include".into(), json!(["reasoning.encrypted_content"]));

        if !req.tools.is_empty() {
            // Responses 工具形状:type/name/description/parameters 平铺(非 Chat 的 function 嵌套)
            let tools: Vec<Value> = req
                .tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    })
                })
                .collect();
            body.insert("tools".into(), Value::Array(tools));
            if req.tool_choice == super::ToolChoice::None {
                body.insert("tool_choice".into(), json!("none"));
            }
        }
        if let Some(t) = req.options.temperature.or(self.cfg.temperature) {
            body.insert("temperature".into(), json!(t));
        }
        if let Some(m) = req.options.max_tokens {
            body.insert("max_output_tokens".into(), json!(m));
        }
        // 思考档 → reasoning.effort(推理模型才生效;非推理模型忽略,无害)
        let thinking = req.options.thinking.unwrap_or(self.cfg.thinking);
        if let Some(effort) = thinking.effort_str() {
            body.insert("reasoning".into(), json!({ "effort": effort }));
        }

        Value::Object(body)
    }

    /// messages → Responses input items[]。关键:Assistant 轮的 reasoning item(逐字)排在它的
    /// function_call 之前回放,否则推理链断裂(reasoning 保真铁律)。
    fn input_items(&self, messages: &[super::ChatMessage]) -> Vec<Value> {
        use super::ChatMessage;
        let mut items: Vec<Value> = Vec::new();
        for msg in messages {
            match msg {
                ChatMessage::User { content, .. } => {
                    items.push(json!({ "role": "user", "content": content }));
                }
                ChatMessage::Assistant { content, tool_calls, reasoning_state, .. } => {
                    // 1) reasoning item 先回放(逐字保真:encrypted_content 不能改、不能丢)
                    if let Some(arr) =
                        reasoning_state.as_ref().and_then(|v| v.get(ITEMS_KEY)).and_then(Value::as_array)
                    {
                        items.extend(arr.iter().cloned());
                    }
                    // 2) function_call item(s)
                    for call in tool_calls {
                        items.push(json!({
                            "type": "function_call",
                            "call_id": call.id,
                            "name": call.name,
                            "arguments": call.args.to_string(),
                        }));
                    }
                    // 3) 有正文才补一条 assistant message
                    if !content.is_empty() {
                        items.push(json!({ "role": "assistant", "content": content }));
                    }
                }
                ChatMessage::ToolResult { call_id, content } => {
                    items.push(json!({
                        "type": "function_call_output",
                        "call_id": call_id,
                        "output": content,
                    }));
                }
            }
        }
        items
    }
}

#[async_trait::async_trait]
impl LlmProvider for OpenAiResponsesProvider {
    fn model_id(&self) -> &str {
        &self.cfg.model
    }

    async fn chat_stream(&self, req: ChatRequest) -> Result<mpsc::Receiver<ChatEvent>, LlmError> {
        if self.cfg.api_key.trim().is_empty() {
            return Err(LlmError::NoApiKey);
        }
        let url = format!("{}/responses", self.cfg.base_url.trim_end_matches('/'));
        let body = self.to_wire(&req);
        let key = self.cfg.api_key.clone();
        let resp = self
            .net
            .send(&url, |c| c.post(&url).bearer_auth(&key).json(&body))
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
            let mut calls: Vec<ToolCall> = Vec::new();
            let mut reasoning_items: Vec<Value> = Vec::new(); // 逐字攒,回放原样
            loop {
                let bytes = match tokio::time::timeout(IDLE_TIMEOUT, stream.next()).await {
                    Err(_) => {
                        let _ = tx
                            .send(ChatEvent::Failed(LlmError::Network("流空闲超时".into())))
                            .await;
                        return;
                    }
                    Ok(None) => {
                        finalize(&tx, usage, calls, reasoning_items).await;
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
                    if data.is_empty() || data == "[DONE]" {
                        continue;
                    }
                    let Ok(value) = serde_json::from_str::<Value>(data) else { continue };
                    match parse_event(&value, &tx, &mut usage, &mut calls, &mut reasoning_items).await
                    {
                        Flow::Continue => {}
                        Flow::Stop => return, // 接收端 drop / 完成 / 失败
                    }
                }
            }
        });
        Ok(rx)
    }
}

enum Flow {
    Continue,
    Stop,
}

/// 解析一帧 Responses 事件。按 `type` 分发;吐 Delta/Thinking,攒 function_call / reasoning item / usage。
/// response.completed → finalize 并停;send 失败(取消)→ 停。
async fn parse_event(
    value: &Value,
    tx: &mpsc::Sender<ChatEvent>,
    usage: &mut Usage,
    calls: &mut Vec<ToolCall>,
    reasoning_items: &mut Vec<Value>,
) -> Flow {
    let ty = value.get("type").and_then(Value::as_str).unwrap_or("");
    match ty {
        // 正文增量
        "response.output_text.delta" => {
            if let Some(d) = value.get("delta").and_then(Value::as_str) {
                if tx.send(ChatEvent::Delta(d.to_string())).await.is_err() {
                    return Flow::Stop;
                }
            }
        }
        // 思考摘要增量(原始 CoT 不外露,只有摘要)
        "response.reasoning_summary_text.delta" => {
            if let Some(d) = value.get("delta").and_then(Value::as_str) {
                if tx.send(ChatEvent::Thinking(d.to_string())).await.is_err() {
                    return Flow::Stop;
                }
            }
        }
        // 完整 item 落定:function_call 收调用,reasoning 逐字收下(含 encrypted_content)
        "response.output_item.done" => {
            if let Some(item) = value.get("item") {
                collect_item(item, calls, reasoning_items);
            }
        }
        // 收尾:usage 在此,output 数组兜底再扫一遍(防只在此出现的 item)
        "response.completed" => {
            if let Some(resp) = value.get("response") {
                if let Some(u) = resp.get("usage") {
                    let g = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0) as i64;
                    usage.input_tokens = g("input_tokens");
                    usage.output_tokens = g("output_tokens");
                    // 缓存命中藏在 input_tokens_details.cached_tokens
                    usage.cache_hit_tokens = u
                        .pointer("/input_tokens_details/cached_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0) as i64;
                }
            }
            finalize(tx, std::mem::take(usage), std::mem::take(calls), std::mem::take(reasoning_items))
                .await;
            return Flow::Stop;
        }
        "response.failed" | "error" => {
            let msg = value
                .pointer("/response/error/message")
                .or_else(|| value.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("Responses 流报错");
            let _ = tx.send(ChatEvent::Failed(LlmError::Api { status: 0, message: msg.into() })).await;
            return Flow::Stop;
        }
        _ => {} // 其余事件(.added/.created/.in_progress 等)忽略
    }
    Flow::Continue
}

/// 从一个 output item 收集:function_call → ToolCall;reasoning → 逐字存(回放用)。
fn collect_item(item: &Value, calls: &mut Vec<ToolCall>, reasoning_items: &mut Vec<Value>) {
    match item.get("type").and_then(Value::as_str) {
        Some("function_call") => {
            let raw_args = item.get("arguments").and_then(Value::as_str).unwrap_or("{}");
            let args: Value = serde_json::from_str(raw_args).unwrap_or_else(|_| json!({}));
            let incomplete = serde_json::from_str::<Value>(raw_args).is_err();
            calls.push(ToolCall {
                // Responses 给 call_id;缺则用 item id 兜底
                id: item
                    .get("call_id")
                    .or_else(|| item.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                name: item.get("name").and_then(Value::as_str).unwrap_or_default().to_string(),
                args,
                is_incomplete: incomplete,
            });
        }
        // reasoning item 逐字收下(含 encrypted_content),回放原样塞回 input —— 不解析、不裁剪
        Some("reasoning") => reasoning_items.push(item.clone()),
        _ => {}
    }
}

/// 流终:有 function_call → tool_use,否则 end_turn;先发 ReasoningState(items)再发 Done。
async fn finalize(
    tx: &mpsc::Sender<ChatEvent>,
    usage: Usage,
    calls: Vec<ToolCall>,
    reasoning_items: Vec<Value>,
) {
    let stop_reason =
        Some(if calls.is_empty() { "end_turn".into() } else { "tool_use".to_string() });
    if !reasoning_items.is_empty() {
        let _ = tx
            .send(ChatEvent::ReasoningState(json!({ ITEMS_KEY: reasoning_items })))
            .await;
    }
    let _ = tx.send(ChatEvent::Done { usage, stop_reason, tool_calls: calls }).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{ChatMessage, ChatOptions, ToolDef};

    fn cfg() -> LlmConfig {
        LlmConfig::openai(String::from("sk-x"))
    }

    fn req(messages: Vec<ChatMessage>) -> ChatRequest {
        ChatRequest {
            system: "你是 7274".into(),
            messages,
            options: ChatOptions::default(),
            tools: vec![ToolDef {
                name: "now".into(),
                description: "时间".into(),
                parameters: json!({ "type": "object", "properties": {} }),
            }],
            tool_choice: super::super::ToolChoice::Auto,
        }
    }

    #[test]
    fn to_wire_stateless_with_encrypted_reasoning_and_flat_tools() {
        let p = OpenAiResponsesProvider::new(cfg());
        let wire = p.to_wire(&req(vec![ChatMessage::user("几点")]));
        assert_eq!(wire["instructions"], "你是 7274");
        assert_eq!(wire["store"], false, "stateless,自带 reasoning 回放");
        assert_eq!(wire["include"][0], "reasoning.encrypted_content");
        // 工具平铺(name 在顶层,非 Chat 的 function 嵌套)
        assert_eq!(wire["tools"][0]["type"], "function");
        assert_eq!(wire["tools"][0]["name"], "now");
        assert_eq!(wire["input"][0]["role"], "user");
        assert_eq!(wire["input"][0]["content"], "几点");
    }

    // 核心保真:历史 Assistant 的 reasoning item 逐字回放,且排在它的 function_call 之前
    #[test]
    fn reasoning_items_round_trip_before_function_call() {
        let p = OpenAiResponsesProvider::new(cfg());
        let reasoning_item = json!({
            "type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "ENC_BLOB"
        });
        let assistant = ChatMessage::Assistant {
            content: String::new(),
            reasoning: None,
            tool_calls: vec![ToolCall {
                id: "fc_1".into(),
                name: "now".into(),
                args: json!({}),
                is_incomplete: false,
            }],
            reasoning_state: Some(json!({ "items": [reasoning_item.clone()] })),
        };
        let wire = p.to_wire(&req(vec![
            ChatMessage::user("几点"),
            assistant,
            ChatMessage::ToolResult { call_id: "fc_1".into(), content: "12:00".into() },
        ]));
        let input = wire["input"].as_array().unwrap();
        // user, reasoning(逐字), function_call, function_call_output
        assert_eq!(input[1], reasoning_item, "reasoning item 必须逐字回放,不改一字");
        assert_eq!(input[2]["type"], "function_call");
        assert_eq!(input[2]["call_id"], "fc_1");
        assert!(
            input.iter().position(|i| i["type"] == "reasoning").unwrap()
                < input.iter().position(|i| i["type"] == "function_call").unwrap(),
            "reasoning 必须排在 function_call 之前"
        );
        assert_eq!(input[3]["type"], "function_call_output");
        assert_eq!(input[3]["output"], "12:00");
    }

    #[tokio::test]
    async fn parse_events_collect_call_and_reasoning_then_finalize() {
        let (tx, mut rx) = mpsc::channel::<ChatEvent>(16);
        let mut usage = Usage::default();
        let mut calls = Vec::new();
        let mut items = Vec::new();

        // 文本增量
        let d = json!({ "type": "response.output_text.delta", "delta": "好" });
        assert!(matches!(
            parse_event(&d, &tx, &mut usage, &mut calls, &mut items).await,
            Flow::Continue
        ));
        // reasoning item 落定(逐字收)
        let r = json!({ "type": "response.output_item.done",
            "item": { "type": "reasoning", "id": "rs_1", "encrypted_content": "ENC" } });
        parse_event(&r, &tx, &mut usage, &mut calls, &mut items).await;
        // function_call item 落定
        let f = json!({ "type": "response.output_item.done",
            "item": { "type": "function_call", "call_id": "fc_1", "name": "now", "arguments": "{}" } });
        parse_event(&f, &tx, &mut usage, &mut calls, &mut items).await;
        // 收尾
        let c = json!({ "type": "response.completed",
            "response": { "usage": { "input_tokens": 12, "output_tokens": 4 } } });
        assert!(matches!(
            parse_event(&c, &tx, &mut usage, &mut calls, &mut items).await,
            Flow::Stop
        ));
        drop(tx);

        let mut got = Vec::new();
        while let Some(ev) = rx.recv().await {
            got.push(ev);
        }
        // Delta("好"), ReasoningState({items:[..]}), Done{tool_use}
        assert!(matches!(&got[0], ChatEvent::Delta(t) if t == "好"));
        match &got[1] {
            ChatEvent::ReasoningState(v) => {
                assert_eq!(v["items"][0]["encrypted_content"], "ENC", "encrypted_content 逐字保真");
            }
            other => panic!("期望 ReasoningState,得到 {other:?}"),
        }
        match &got[2] {
            ChatEvent::Done { stop_reason, tool_calls, usage } => {
                assert_eq!(stop_reason.as_deref(), Some("tool_use"));
                assert_eq!(tool_calls[0].name, "now");
                assert_eq!(usage.input_tokens, 12);
            }
            other => panic!("期望 Done,得到 {other:?}"),
        }
    }
}
