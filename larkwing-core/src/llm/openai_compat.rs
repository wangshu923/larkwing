//! OpenAI 兼容 provider。DeepSeek = 一组配置(base_url + model),不是专属实现;
//! 以后 Kimi/Qwen/Ollama 同样只换配置。方言知识(出向 to_wire / 入向 parse_chunk)止步本文件。

use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use super::sse::LineBuffer;
use super::{AuthStyle, ChatEvent, ChatRequest, LlmConfig, LlmError, LlmProvider, Thinking, Usage};

/// 整流不设总时长上限;60s 无增量判网络失败。
const IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

pub struct OpenAiCompatProvider {
    cfg: LlmConfig,
    http: reqwest::Client,
}

impl OpenAiCompatProvider {
    pub fn new(cfg: LlmConfig) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("构建 HTTP client 失败");
        Self { cfg, http }
    }

    /// 出向方言翻译:中立 ChatRequest → OpenAI 系请求体。
    fn to_wire(&self, req: &ChatRequest) -> Value {
        let mut messages = Vec::with_capacity(req.messages.len() + 1);
        // 我们的 system 是独立字段;OpenAI 方言翻成首条 system 消息
        messages.push(json!({ "role": "system", "content": req.system }));
        for msg in &req.messages {
            messages.push(match msg {
                super::ChatMessage::User { content } => {
                    json!({ "role": "user", "content": content })
                }
                super::ChatMessage::Assistant { content, reasoning, tool_calls } => {
                    let mut m = json!({ "role": "assistant", "content": content });
                    if !tool_calls.is_empty() {
                        // OpenAI 方言:arguments 是 JSON 编码后的字符串,不是对象
                        m["tool_calls"] = Value::Array(
                            tool_calls
                                .iter()
                                .map(|c| {
                                    json!({
                                        "id": c.id,
                                        "type": "function",
                                        "function": { "name": c.name, "arguments": c.args.to_string() }
                                    })
                                })
                                .collect(),
                        );
                        // 坑 #4:带 tool_calls 的轮次回传必须附 reasoning(DeepSeek 缺它 400);
                        // 借 thinking_field quirk 当 DeepSeek 方言标,严格网关不发未知字段
                        if self.cfg.quirks.thinking_field {
                            m["reasoning_content"] = json!(reasoning.clone().unwrap_or_default());
                        }
                    }
                    m
                }
                super::ChatMessage::ToolResult { call_id, content } => {
                    json!({ "role": "tool", "tool_call_id": call_id, "content": content })
                }
            });
        }
        let mut body = json!({
            "model": req.options.model.as_deref().unwrap_or(&self.cfg.model),
            "messages": messages,
            "stream": true,
        });
        // 坑 #8:不显式要,多数 OpenAI 兼容网关流式不给 usage;严格端点不认此字段则按 quirk 省掉
        if !self.cfg.quirks.no_stream_options {
            body["stream_options"] = json!({ "include_usage": true });
        }
        // 思考档位的两种 OpenAI 系方言:
        // 坑 #2:DeepSeek 的 thinking 开关永远显式带,不赌默认值(只有开关,非 Off 都算开);
        // reasoning_effort 端点(gpt-5 系)翻成 low/medium/high,Off 不发字段;
        // 都没声明的端点什么都不带 —— 未知字段可能被严格网关 400。
        let lvl = req.options.thinking.unwrap_or(self.cfg.thinking);
        if self.cfg.quirks.thinking_field {
            body["thinking"] =
                json!({ "type": if lvl != Thinking::Off { "enabled" } else { "disabled" } });
        }
        if self.cfg.quirks.effort_field {
            if let Some(e) = lvl.effort_str() {
                body["reasoning_effort"] = json!(e);
            }
        }
        if let Some(t) = req.options.temperature.or(self.cfg.temperature) {
            body["temperature"] = json!(t);
        }
        if let Some(m) = req.options.max_tokens {
            body["max_tokens"] = json!(m);
        }
        if !req.tools.is_empty() {
            body["tools"] = Value::Array(
                req.tools
                    .iter()
                    .map(|t| {
                        json!({
                            "type": "function",
                            "function": {
                                "name": t.name,
                                "description": t.description,
                                "parameters": t.parameters,
                            }
                        })
                    })
                    .collect(),
            );
            // Auto 是各家默认,不发字段(严格网关少见少错);None = 强制收尾
            if req.tool_choice == super::ToolChoice::None {
                body["tool_choice"] = json!("none");
            }
        }
        body
    }

    /// 认证 + 附加头(野生端点分歧点,见 Quirks):chat 与余额查询共用。
    fn with_auth(&self, mut builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        builder = match self.cfg.quirks.auth.unwrap_or(AuthStyle::Bearer) {
            AuthStyle::Bearer => builder.bearer_auth(&self.cfg.api_key),
            AuthStyle::XApiKey => builder.header("x-api-key", &self.cfg.api_key),
            AuthStyle::ApiKeyHeader => builder.header("api-key", &self.cfg.api_key),
        };
        for (k, v) in &self.cfg.quirks.extra_headers {
            builder = builder.header(k, v);
        }
        builder
    }

    /// 独立成函数供测试检视。
    fn request_builder(&self, url: &str) -> reqwest::RequestBuilder {
        self.with_auth(self.http.post(url))
    }
}

/// stop_reason 归一到中立词表(robot 同款):未知值原样透传不丢失,None 不进表。
fn normalize_finish_reason(raw: &str) -> String {
    match raw {
        "stop" => "end_turn".into(),
        "tool_calls" => "tool_use".into(),
        "length" => "max_tokens".into(),
        other => other.into(),
    }
}

/// 流式 tool_call 碎片的攒桶(规范 #6):按 index 开桶,id/name 只认首片真值,
/// arguments 字符串逐片拼接。BTreeMap 迭代天然按 index 排序 = 与模型声明顺序一致。
#[derive(Default)]
struct ToolCallAcc {
    id: String,
    name: String,
    args: String,
}

/// 攒完的碎片 → 中立 ToolCall:没 id 合成 call_{idx}(否则结果回灌时配对链断),
/// 空参兜 "{}",截断检测在 stop_reason 归一之后(parse_tool_args)。
fn finalize_tool_calls(
    calls: std::collections::BTreeMap<u64, ToolCallAcc>,
    stop_reason: Option<&str>,
) -> Vec<super::ToolCall> {
    calls
        .into_iter()
        .map(|(idx, acc)| {
            let id = if acc.id.is_empty() { format!("call_{idx}") } else { acc.id };
            let (args, is_incomplete) = super::parse_tool_args(&acc.args, stop_reason, &acc.name);
            super::ToolCall { id, name: acc.name, args, is_incomplete }
        })
        .collect()
}

/// 入向方言翻译:一个 SSE data 块 → 0..n 个事件。防御清单(robot 实战考古):
/// - choices 可能为空(usage-only 尾帧)/ delta 可能缺失 —— pointer 链天然安全;
/// - usage 可能部分缺字段:已拿到的值作 default,不许被残缺块砸回 0(坑 #8);
/// - cached tokens 两套字段:DeepSeek 平铺 prompt_cache_hit_tokens,OpenAI 系藏在
///   prompt_tokens_details.cached_tokens 两层嵌套里;
/// - 思考字段名不统一:reasoning_content(DeepSeek)/ reasoning(OpenRouter 系网关);
/// - finish_reason 只随一帧出现、其余帧为 None —— 调用方负责锁存非空值(坑 #9);
/// - 流中 error 帧 data: {"error":...} —— 必须立刻 Failed,否则空等到超时(坑 #7);
/// - tool_calls 碎片(规范 #6):index 可能缺失兜 0,id/name 后续片为 null 不许覆盖真值。
fn parse_chunk(
    value: &Value,
    usage: &mut Usage,
    finish: &mut Option<String>,
    calls: &mut std::collections::BTreeMap<u64, ToolCallAcc>,
) -> Vec<ChatEvent> {
    let mut events = Vec::new();

    // 坑 #7:流中 error 帧
    if let Some(err) = value.get("error").filter(|e| !e.is_null()) {
        let message = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("provider 在流中返回错误")
            .to_string();
        events.push(ChatEvent::Failed(LlmError::Api { status: 0, message }));
        return events;
    }

    if let Some(u) = value.get("usage").filter(|u| !u.is_null()) {
        usage.input_tokens = u
            .get("prompt_tokens")
            .and_then(Value::as_i64)
            .unwrap_or(usage.input_tokens);
        usage.output_tokens = u
            .get("completion_tokens")
            .and_then(Value::as_i64)
            .unwrap_or(usage.output_tokens);
        usage.cache_hit_tokens = u
            .get("prompt_cache_hit_tokens")
            .or_else(|| u.pointer("/prompt_tokens_details/cached_tokens"))
            .and_then(Value::as_i64)
            .unwrap_or(usage.cache_hit_tokens);
    }

    // 坑 #9:finish_reason 锁存(只在某一帧非空,之后的 usage 帧又变 null)
    if let Some(reason) = value.pointer("/choices/0/finish_reason").and_then(Value::as_str) {
        *finish = Some(normalize_finish_reason(reason));
    }

    if let Some(delta) = value.pointer("/choices/0/delta") {
        // 坑 #3:reasoning 与 content 是并行字段,分开认识;字段名两路探测
        if let Some(t) = delta
            .get("reasoning_content")
            .or_else(|| delta.get("reasoning"))
            .and_then(Value::as_str)
        {
            if !t.is_empty() {
                events.push(ChatEvent::Thinking(t.to_string()));
            }
        }
        if let Some(t) = delta.get("content").and_then(Value::as_str) {
            if !t.is_empty() {
                events.push(ChatEvent::Delta(t.to_string()));
            }
        }
        if let Some(frags) = delta.get("tool_calls").and_then(Value::as_array) {
            for frag in frags {
                let idx = frag.get("index").and_then(Value::as_u64).unwrap_or(0); // index 缺失兜 0
                let acc = calls.entry(idx).or_default();
                // 首片真值才写入:后续片 id/name 为 null,不许把真值覆盖回空(规范 #6)
                if let Some(id) = frag.get("id").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                    acc.id = id.into();
                }
                if let Some(n) = frag
                    .pointer("/function/name")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    acc.name = n.into();
                }
                if let Some(a) = frag.pointer("/function/arguments").and_then(Value::as_str) {
                    acc.args.push_str(a); // 被任意切碎的 JSON 字符串,逐片拼
                }
            }
        }
    }
    events
}

fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

#[async_trait::async_trait]
impl LlmProvider for OpenAiCompatProvider {
    fn model_id(&self) -> &str {
        &self.cfg.model
    }

    /// DeepSeek 形状的余额查询(GET /user/balance,认证同 chat)。
    /// 其他 OpenAI 兼容端点多半 404 → None,无害;锦上添花链路,一切失败静默。
    async fn balance(&self) -> Option<super::AccountBalance> {
        if self.cfg.api_key.trim().is_empty() {
            return None;
        }
        let url = format!("{}/user/balance", self.cfg.base_url.trim_end_matches('/'));
        let resp = self
            .with_auth(self.http.get(&url))
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let v: Value = resp.json().await.ok()?;
        // {"balance_infos":[{"currency":"CNY","total_balance":"110.00",…}]}
        let info = v.get("balance_infos")?.as_array()?.first()?;
        Some(super::AccountBalance {
            currency: info.get("currency")?.as_str()?.to_string(),
            amount: info.get("total_balance")?.as_str()?.to_string(),
        })
    }

    async fn chat_stream(&self, req: ChatRequest) -> Result<mpsc::Receiver<ChatEvent>, LlmError> {
        if self.cfg.api_key.trim().is_empty() {
            return Err(LlmError::NoApiKey);
        }
        let url = format!("{}/chat/completions", self.cfg.base_url.trim_end_matches('/'));
        let resp = self
            .request_builder(&url)
            .json(&self.to_wire(&req))
            .send()
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
            let mut finish: Option<String> = None; // finish_reason 锁存(坑 #9)
            let mut calls = std::collections::BTreeMap::new(); // tool_call 碎片攒桶(规范 #6)
            loop {
                let bytes = match tokio::time::timeout(IDLE_TIMEOUT, stream.next()).await {
                    Err(_) => {
                        let _ = tx
                            .send(ChatEvent::Failed(LlmError::Network("流空闲超时".into())))
                            .await;
                        return;
                    }
                    // 流自然结束但没收到 [DONE]:容忍,按完成处理
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
                    let Some(data) = line.strip_prefix("data:") else {
                        continue; // 注释行 / keep-alive,忽略
                    };
                    let data = data.trim();
                    if data == "[DONE]" {
                        let stop_reason = finish.take();
                        let tool_calls =
                            finalize_tool_calls(std::mem::take(&mut calls), stop_reason.as_deref());
                        let _ = tx.send(ChatEvent::Done { usage, stop_reason, tool_calls }).await;
                        return;
                    }
                    let Ok(value) = serde_json::from_str::<Value>(data) else {
                        continue;
                    };
                    for ev in parse_chunk(&value, &mut usage, &mut finish, &mut calls) {
                        // 流中 error 帧:发完 Failed 立即收摊,不再空等(坑 #7)
                        let failed = matches!(ev, ChatEvent::Failed(_));
                        // 取消 = 接收端 drop:send 失败即中止,HTTP 连接随 stream 一起断开
                        if tx.send(ev).await.is_err() || failed {
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
    use crate::llm::{ChatMessage, ToolCall};

    fn provider() -> OpenAiCompatProvider {
        OpenAiCompatProvider::new(LlmConfig::deepseek("sk-test".into()))
    }

    /// 旧式三参便捷壳:多数测试不关心 tool_call 碎片桶。
    fn parse(v: &Value, usage: &mut Usage, finish: &mut Option<String>) -> Vec<ChatEvent> {
        let mut calls = std::collections::BTreeMap::new();
        parse_chunk(v, usage, finish, &mut calls)
    }

    #[test]
    fn to_wire_translates_system_thinking_and_overrides() {
        let p = provider();
        let req = ChatRequest {
            system: "你是旺财".into(),
            messages: vec![ChatMessage::user("你好")],
            options: crate::llm::ChatOptions {
                temperature: Some(1.1),
                ..Default::default()
            },
            ..Default::default()
        };
        let wire = p.to_wire(&req);
        assert_eq!(wire["model"], "deepseek-v4-pro");
        assert_eq!(wire["messages"][0]["role"], "system");
        assert_eq!(wire["messages"][0]["content"], "你是旺财");
        assert_eq!(wire["messages"][1]["role"], "user");
        // 坑 #2:thinking 显式关闭;坑 #8:显式要 usage 尾帧
        assert_eq!(wire["thinking"]["type"], "disabled");
        assert_eq!(wire["stream_options"]["include_usage"], true);
        let temp = wire["temperature"].as_f64().unwrap();
        assert!((temp - 1.1).abs() < 1e-6, "temperature 应约等于 1.1,实际 {temp}");
    }

    // 工具轮回传的出向形状(B 期接入,翻译先钉死):arguments 必须是 JSON 字符串;
    // 坑 #4:DeepSeek 方言(thinking_field quirk)带 tool_calls 的轮次必须附 reasoning_content
    #[test]
    fn to_wire_translates_tool_round_messages() {
        let p = provider();
        let req = ChatRequest {
            system: "你是 7274".into(),
            messages: vec![
                ChatMessage::user("我对花生过敏"),
                ChatMessage::Assistant {
                    content: String::new(),
                    reasoning: Some("该记下来".into()),
                    tool_calls: vec![ToolCall {
                        id: "call_1".into(),
                        name: "remember".into(),
                        args: serde_json::json!({ "fact": "对花生过敏" }),
                        is_incomplete: false,
                    }],
                },
                ChatMessage::ToolResult { call_id: "call_1".into(), content: "ok".into() },
            ],
            ..Default::default()
        };
        let wire = p.to_wire(&req);
        let call = &wire["messages"][2]["tool_calls"][0];
        assert_eq!(call["id"], "call_1");
        assert_eq!(call["type"], "function");
        assert_eq!(call["function"]["name"], "remember");
        let args: Value =
            serde_json::from_str(call["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args["fact"], "对花生过敏");
        assert_eq!(wire["messages"][2]["reasoning_content"], "该记下来");
        assert_eq!(wire["messages"][3]["role"], "tool");
        assert_eq!(wire["messages"][3]["tool_call_id"], "call_1");

        // 普通 assistant 消息:不带 tool_calls / reasoning_content 字段
        let plain = p.to_wire(&ChatRequest {
            system: String::new(),
            messages: vec![ChatMessage::assistant("汪!")],
            ..Default::default()
        });
        assert!(plain["messages"][1].get("tool_calls").is_none());
        assert!(plain["messages"][1].get("reasoning_content").is_none());
    }

    fn chunk(json: &str) -> Value {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn parse_chunk_separates_content_reasoning_and_usage() {
        let mut usage = Usage::default();
        let mut finish = None;
        let v = chunk(
            r#"{"choices":[{"delta":{"reasoning_content":"想想","content":"汪!"}}],
                "usage":{"prompt_tokens":10,"completion_tokens":2,"prompt_cache_hit_tokens":8}}"#,
        );
        let events = parse(&v, &mut usage, &mut finish);
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], ChatEvent::Thinking(t) if t == "想想"));
        assert!(matches!(&events[1], ChatEvent::Delta(t) if t == "汪!"));
        assert_eq!(usage.cache_hit_tokens, 8);
    }

    #[test]
    fn parse_chunk_tolerates_empty_choices_and_missing_usage() {
        let mut usage = Usage::default();
        let mut finish = None;
        assert!(parse(&chunk(r#"{"choices":[]}"#), &mut usage, &mut finish).is_empty());
    }

    // 规范 #6 全套:并行调用按 index 开桶、id/name 只认首片、args 跨片拼接、
    // 输出按 index 排序、没 id 合成 call_{idx}、空参兜 {}
    #[test]
    fn tool_call_fragments_reassemble_across_chunks() {
        let mut usage = Usage::default();
        let mut finish = None;
        let mut calls = std::collections::BTreeMap::new();
        // 片 1:两个调用的首片(id/name 真值);call 1 故意没有 id(测合成)
        parse_chunk(
            &chunk(
                r#"{"choices":[{"delta":{"tool_calls":[
                    {"index":0,"id":"call_a","function":{"name":"remember","arguments":"{\"fa"}},
                    {"index":1,"function":{"name":"now","arguments":""}}]}}]}"#,
            ),
            &mut usage, &mut finish, &mut calls,
        );
        // 片 2:后续片 id/name 为 null,arguments 续拼(JSON 在引号中间被切开)
        parse_chunk(
            &chunk(
                r#"{"choices":[{"delta":{"tool_calls":[
                    {"index":0,"id":null,"function":{"name":null,"arguments":"ct\":\"对花生过敏\"}"}}]}}]}"#,
            ),
            &mut usage, &mut finish, &mut calls,
        );
        parse_chunk(
            &chunk(r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#),
            &mut usage, &mut finish, &mut calls,
        );
        assert_eq!(finish.as_deref(), Some("tool_use"));

        let out = finalize_tool_calls(calls, finish.as_deref());
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].id, "call_a");
        assert_eq!(out[0].name, "remember");
        assert_eq!(out[0].args["fact"], "对花生过敏");
        assert!(!out[0].is_incomplete);
        assert_eq!(out[1].id, "call_1", "全程没 id → 合成 call_{{idx}}");
        assert_eq!(out[1].args, serde_json::json!({}), "空参数兜 {{}}");
    }

    // 截断检测(规范 #6 + 坑 #9):stop=length 已归一为 max_tokens,半截 JSON 标 is_incomplete
    #[test]
    fn truncated_tool_call_args_are_flagged_incomplete() {
        let mut usage = Usage::default();
        let mut finish = None;
        let mut calls = std::collections::BTreeMap::new();
        parse_chunk(
            &chunk(
                r#"{"choices":[{"delta":{"tool_calls":[
                    {"index":0,"id":"call_x","function":{"name":"remember","arguments":"{\"fact\":\"被截"}}]}}]}"#,
            ),
            &mut usage, &mut finish, &mut calls,
        );
        parse_chunk(
            &chunk(r#"{"choices":[{"delta":{},"finish_reason":"length"}]}"#),
            &mut usage, &mut finish, &mut calls,
        );
        let out = finalize_tool_calls(calls, finish.as_deref());
        assert!(out[0].is_incomplete, "半截参数必须拒绝执行");
    }

    // tools / tool_choice 出向:Auto 不发字段,None 发 "none"
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
        assert_eq!(wire["tools"][0]["type"], "function");
        assert_eq!(wire["tools"][0]["function"]["name"], "now");
        assert!(wire.get("tool_choice").is_none(), "Auto 不发字段");

        req.tool_choice = crate::llm::ToolChoice::None;
        assert_eq!(p.to_wire(&req)["tool_choice"], "none");

        // 无工具时连 tools 都不发
        let bare = p.to_wire(&ChatRequest::default());
        assert!(bare.get("tools").is_none());
    }

    // 坑 #7:流中 error 帧必须立刻 Failed,不许静默吞掉空等超时
    #[test]
    fn parse_chunk_turns_midstream_error_frame_into_failed() {
        let mut usage = Usage::default();
        let mut finish = None;
        let v = chunk(r#"{"error":{"message":"Rate limit reached","code":"429"}}"#);
        let events = parse(&v, &mut usage, &mut finish);
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0],
            ChatEvent::Failed(LlmError::Api { message, .. }) if message.contains("Rate limit")));
    }

    // 坑 #8:残缺 usage 块不许把已拿到的值砸回 0;OpenAI 系嵌套 cached_tokens 也要认
    #[test]
    fn parse_chunk_preserves_usage_across_partial_frames() {
        let mut usage = Usage::default();
        let mut finish = None;
        parse(
            &chunk(r#"{"usage":{"prompt_tokens":100,"completion_tokens":5,"prompt_cache_hit_tokens":64}}"#),
            &mut usage, &mut finish,
        );
        // 残缺块只带 completion_tokens:其余字段保留旧值
        parse(&chunk(r#"{"usage":{"completion_tokens":9}}"#), &mut usage, &mut finish);
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.output_tokens, 9);
        assert_eq!(usage.cache_hit_tokens, 64);

        // OpenAI 系两层嵌套形态
        let mut usage2 = Usage::default();
        parse(
            &chunk(r#"{"usage":{"prompt_tokens":50,"prompt_tokens_details":{"cached_tokens":32}}}"#),
            &mut usage2, &mut finish,
        );
        assert_eq!(usage2.cache_hit_tokens, 32);
    }

    // 坑 #9:finish_reason 只在一帧出现且要归一;后续 null 不许冲掉锁存值
    #[test]
    fn finish_reason_is_latched_and_normalized() {
        let mut usage = Usage::default();
        let mut finish = None;
        parse(
            &chunk(r#"{"choices":[{"delta":{"content":"半"},"finish_reason":null}]}"#),
            &mut usage, &mut finish,
        );
        assert_eq!(finish, None);
        parse(
            &chunk(r#"{"choices":[{"delta":{},"finish_reason":"length"}]}"#),
            &mut usage, &mut finish,
        );
        assert_eq!(finish.as_deref(), Some("max_tokens"), "length 归一为 max_tokens");
        // usage 尾帧 finish_reason 为 null / choices 为空:不冲掉锁存值
        parse(&chunk(r#"{"choices":[],"usage":{"prompt_tokens":1}}"#), &mut usage, &mut finish);
        assert_eq!(finish.as_deref(), Some("max_tokens"));
        // 未知取值透传
        assert_eq!(normalize_finish_reason("content_filter"), "content_filter");
        assert_eq!(normalize_finish_reason("stop"), "end_turn");
        assert_eq!(normalize_finish_reason("tool_calls"), "tool_use");
    }

    // 非 DeepSeek 方言:thinking 字段默认不带(严格网关未知字段 400);
    // no_stream_options quirk 把 stream_options 也省掉
    #[test]
    fn quirks_control_dialect_specific_fields() {
        let mut cfg = LlmConfig::deepseek("sk-test".into());
        cfg.quirks = crate::llm::Quirks { no_stream_options: true, ..Default::default() };
        let p = OpenAiCompatProvider::new(cfg);
        let wire = p.to_wire(&ChatRequest::default());
        assert!(wire.get("thinking").is_none(), "通用方言不带 thinking 字段");
        assert!(wire.get("stream_options").is_none(), "no_stream_options 应省掉 stream_options");
    }

    // 思考档位的两种 OpenAI 系翻译:DeepSeek 开关(非 Off 即开)/ reasoning_effort 三级
    #[test]
    fn thinking_tiers_translate_per_quirk() {
        let req_with = |lvl: Thinking| ChatRequest {
            options: crate::llm::ChatOptions { thinking: Some(lvl), ..Default::default() },
            ..Default::default()
        };
        // DeepSeek 方言:Light/Heavy 都翻成 enabled
        let ds = provider();
        assert_eq!(ds.to_wire(&req_with(Thinking::Light))["thinking"]["type"], "enabled");
        assert_eq!(ds.to_wire(&req_with(Thinking::Heavy))["thinking"]["type"], "enabled");
        assert_eq!(ds.to_wire(&req_with(Thinking::Off))["thinking"]["type"], "disabled");
        assert!(ds.to_wire(&req_with(Thinking::Heavy)).get("reasoning_effort").is_none());

        // effort 端点:low/medium/high,Off 不发字段
        let mut cfg = LlmConfig::deepseek("sk-test".into());
        cfg.quirks = crate::llm::Quirks { effort_field: true, ..Default::default() };
        let p = OpenAiCompatProvider::new(cfg);
        assert_eq!(p.to_wire(&req_with(Thinking::Light))["reasoning_effort"], "low");
        assert_eq!(p.to_wire(&req_with(Thinking::Medium))["reasoning_effort"], "medium");
        assert_eq!(p.to_wire(&req_with(Thinking::Heavy))["reasoning_effort"], "high");
        assert!(p.to_wire(&req_with(Thinking::Off)).get("reasoning_effort").is_none());
    }

    // 认证头三风格:Bearer(默认)/ x-api-key / api-key + 附加头
    #[test]
    fn request_builder_honors_auth_style_and_extra_headers() {
        let build = |quirks: crate::llm::Quirks| {
            let mut cfg = LlmConfig::deepseek("sk-test".into());
            cfg.quirks = quirks;
            OpenAiCompatProvider::new(cfg)
                .request_builder("https://example.com/v1/chat/completions")
                .build()
                .unwrap()
        };
        let req = build(Default::default());
        assert_eq!(req.headers()["authorization"], "Bearer sk-test");

        let req = build(crate::llm::Quirks {
            auth: Some(AuthStyle::ApiKeyHeader),
            extra_headers: vec![("x-relay-id".into(), "larkwing".into())],
            ..Default::default()
        });
        assert!(req.headers().get("authorization").is_none());
        assert_eq!(req.headers()["api-key"], "sk-test");
        assert_eq!(req.headers()["x-relay-id"], "larkwing");

        let req = build(crate::llm::Quirks { auth: Some(AuthStyle::XApiKey), ..Default::default() });
        assert_eq!(req.headers()["x-api-key"], "sk-test");
    }

    // 思考字段名不统一:reasoning(OpenRouter 系)也要认
    #[test]
    fn reasoning_field_name_fallback() {
        let mut usage = Usage::default();
        let mut finish = None;
        let events = parse(
            &chunk(r#"{"choices":[{"delta":{"reasoning":"琢磨一下"}}]}"#),
            &mut usage, &mut finish,
        );
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], ChatEvent::Thinking(t) if t == "琢磨一下"));
    }
}
