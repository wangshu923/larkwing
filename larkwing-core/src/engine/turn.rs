//! 通用回合循环(engine 私有):全系统唯一一份,不认识任何具体任务(宪法 §5)。
//! 形状:开流 → 攒文本(照旧流 UI)/攒 tool_calls → 无调用则收尾 → 否则并发执行
//! (每工具超时 + 取消级联)→ 落库 → 回填再开流;轮数到顶 tool_choice=none 强制收尾。
//! turn 级状态 = 本文件里的局部变量,session 级在 SessionSlot,app 级在 Engine 字段(PLAN §4)。

use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::llm::{ChatEvent, ChatMessage, ChatRequest, LlmProvider, ToolCall, ToolChoice, Usage};
use crate::bus::{AppEvent, Mood};
use crate::store::Store;
use crate::tools::{Tool, ToolCtx};

use super::{usage, AppError, AssistantPayload, ErrorKind, ToolRowPayload, ToolUseState, TurnEvent};

/// 工具轮上限(PLAN §8):防失控;到顶后强制用嘴收尾。
const MAX_TOOL_ROUNDS: usize = 4;

/// 回合任一出口都把 mood 收回 Idle(悬浮窗 mood 灯熄):Drop 兜底覆盖所有 return 分支。
struct MoodGuard(crate::bus::Bus);
impl Drop for MoodGuard {
    fn drop(&mut self) {
        self.0.publish(AppEvent::Mood(Mood::Idle));
    }
}

pub(super) struct Turn {
    pub store: Store,
    pub conv_id: i64,
    pub user_id: i64,
    pub token: CancellationToken,
    pub tx: mpsc::Sender<TurnEvent>,
    /// 第 1 轮建连成功的那家;2+ 轮粘住它(半截话不换人 + 保前缀缓存)。
    pub provider: Arc<dyn LlmProvider>,
    /// 流水归属:选中的供应商 id + 实际模型(engine 选定时配齐),记账/分析按它。
    pub provider_id: String,
    /// 本回合实际用的模型 id,记账按它查目录牌价。
    pub model: String,
    /// 本回合应答的用户消息 id(流水的回合锚点)。
    pub user_msg_id: i64,
    /// 第 1 轮开流(建连)的时刻:engine 调 chat_stream 前掐表,计时含建连与 TTFB。
    pub first_round_start: std::time::Instant,
    /// 工作副本:工具轮逐轮回填。将来场景自决(enter_mode)的"重建请求"接口就在这。
    pub request: ChatRequest,
    /// 场景白名单子集(执行用;给模型看的定义已在 request.tools)。
    pub tools: Vec<Arc<dyn Tool>>,
    /// 影音运行时(进 ToolCtx)。
    pub media: crate::media::MediaRuntime,
    /// 第 1 轮已开的流(建连失败切换发生在 engine;Turn 内不再切换)。
    pub rx: mpsc::Receiver<ChatEvent>,
}

/// 一轮流式消费的结局。
enum RoundEnd {
    Finished {
        text: String,
        reasoning: Option<String>,
        tool_calls: Vec<ToolCall>,
        usage: Usage,
        timing: usage::RoundTiming,
    },
    Cancelled { partial: String },
    Failed { partial: String, kind: ErrorKind, message: String },
}

impl Turn {
    pub async fn run(self) {
        let Turn {
            store,
            conv_id,
            user_id,
            token,
            tx,
            provider,
            provider_id,
            model,
            user_msg_id,
            first_round_start,
            mut request,
            tools,
            media,
            mut rx,
        } = self;
        // mood 上总线(PLAN §12 修订):悬浮窗据此显「正在想/正在说」;
        // Guard 在任一出口(done/failed/cancelled/落库失败/建连失败)收回 Idle。
        let bus = media.bus().clone();
        bus.publish(AppEvent::Mood(Mood::Thinking));
        let _mood = MoodGuard(bus.clone());
        let meta = usage::RoundMeta { user_id, conv_id, user_msg_id, provider_id, model };
        let mut round_start = first_round_start;
        let ctx = ToolCtx { user_id, conv_id, store: store.clone(), media };
        let label_of = |name: &str| -> String {
            tools
                .iter()
                .find(|t| t.spec().name == name)
                .map(|t| t.spec().ui_key.to_string())
                .unwrap_or_else(|| "tool.unknown".into())
        };

        for round in 0..=MAX_TOOL_ROUNDS {
            let (text, reasoning, tool_calls) =
                match drain(&token, &tx, &bus, rx, conv_id, round_start).await {
                    RoundEnd::Cancelled { partial } => {
                        persist_partial(&store, conv_id, &partial).await;
                        let _ = tx.send(TurnEvent::Cancelled).await;
                        return;
                    }
                    RoundEnd::Failed { partial, kind, message } => {
                        persist_partial(&store, conv_id, &partial).await;
                        let _ = tx.send(TurnEvent::Failed { kind, message }).await;
                        return;
                    }
                    RoundEnd::Finished { text, reasoning, tool_calls, usage, timing } => {
                        // 记账 + 点灯:有 token 才记(严格端点/假流回 0,不点没数据的灯);
                        // 账本绝不打断回合 —— 记账线程挂了只丢这一笔
                        if usage.input_tokens + usage.output_tokens > 0 {
                            let (s, m) = (store.clone(), meta.clone());
                            let recorded = tokio::task::spawn_blocking(move || {
                                usage::record_round(&s, &m, &usage, timing)
                            })
                            .await;
                            if let Ok((round_usage, today, conv)) = recorded {
                                let _ = tx
                                    .send(TurnEvent::Usage { round: round_usage, today, conv })
                                    .await;
                            }
                        }
                        (text, reasoning, tool_calls)
                    }
                };

            // 纯文本收尾 —— 或者端点无视 tool_choice=none 仍要调工具(防御):
            // 都按终回处理,后者不落 tool_calls,绝不留孤儿配对。
            if tool_calls.is_empty() || round == MAX_TOOL_ROUNDS {
                if !tool_calls.is_empty() {
                    tracing::warn!(conv = conv_id, "工具轮超限仍想调用,丢弃调用强制收尾");
                }
                match persist_row(&store, conv_id, "assistant", &text, None).await {
                    Some(message_id) => {
                        let _ = tx.send(TurnEvent::Done { message_id }).await;
                    }
                    None => {
                        let _ = tx
                            .send(TurnEvent::Failed {
                                kind: ErrorKind::Internal,
                                message: "回复落库失败".into(),
                            })
                            .await;
                    }
                }
                return;
            }

            // ---- 工具轮 ----
            bus.publish(AppEvent::Mood(Mood::Thinking)); // 工具执行中 = 思考态(本轮若已 Speaking 过则切回)
            let payload = serde_json::to_string(&AssistantPayload {
                tool_calls: tool_calls.clone(),
                reasoning: reasoning.clone(),
            })
            .ok();
            if persist_row(&store, conv_id, "assistant", &text, payload.as_deref()).await.is_none()
            {
                let _ = tx
                    .send(TurnEvent::Failed {
                        kind: ErrorKind::Internal,
                        message: "工具轮落库失败".into(),
                    })
                    .await;
                return;
            }
            for call in &tool_calls {
                let _ = tx
                    .send(TurnEvent::ToolUse {
                        label: label_of(&call.name),
                        state: ToolUseState::Started,
                    })
                    .await;
            }

            // 并发执行(join_all + 每工具超时);取消级联:取消瞬间为每个 call 合成
            // "已取消"结果落库,保历史完形(约束 #5,OpenAI 系孤儿 tool_call 会 400)
            let results = tokio::select! {
                _ = token.cancelled() => None,
                r = run_tools(&tools, &tool_calls, &ctx) => Some(r),
            };
            let results = match results {
                Some(r) => r,
                None => {
                    for call in &tool_calls {
                        let p = tool_payload(call, "cancelled");
                        persist_row(&store, conv_id, "tool", "已取消", p.as_deref()).await;
                    }
                    let _ = tx.send(TurnEvent::Cancelled).await;
                    return;
                }
            };

            // 落 tool 行 + 回填工作副本
            request.messages.push(ChatMessage::Assistant {
                content: text,
                reasoning,
                tool_calls: tool_calls.clone(),
            });
            for (call, status, content) in &results {
                let p = tool_payload(call, status);
                persist_row(&store, conv_id, "tool", content, p.as_deref()).await;
                let _ = tx
                    .send(TurnEvent::ToolUse {
                        label: label_of(&call.name),
                        state: ToolUseState::Finished,
                    })
                    .await;
                request.messages.push(ChatMessage::ToolResult {
                    call_id: call.id.clone(),
                    content: content.clone(),
                });
            }

            // 末轮强制收尾:下一次开流禁用工具,模型只能用嘴说
            if round + 1 == MAX_TOOL_ROUNDS {
                request.tool_choice = ToolChoice::None;
            }

            // 再开流,粘住同一 provider。此处建连失败不切换:半截对话已经发生,
            // 换人重说会精神分裂 —— 走 Failed 友好兜底(铁律 §3.5)。
            round_start = std::time::Instant::now(); // 2+ 轮计时:从本轮建连起
            match provider.chat_stream(request.clone()).await {
                Ok(next) => rx = next,
                Err(e) => {
                    let app = AppError::from(e);
                    let _ = tx.send(TurnEvent::Failed { kind: app.kind, message: app.message }).await;
                    return;
                }
            }
        }
    }
}

/// 消费一轮流:文本/思考边攒边转发,Done 收口。取消 = drop rx,provider 任务随之中止。
/// started = 本轮开流(建连)时刻:TTFT 锁存在第一个增量事件,elapsed 盖章在收尾。
async fn drain(
    token: &CancellationToken,
    tx: &mpsc::Sender<TurnEvent>,
    bus: &crate::bus::Bus,
    mut rx: mpsc::Receiver<ChatEvent>,
    conv_id: i64,
    started: std::time::Instant,
) -> RoundEnd {
    let mut buffer = String::new(); // turn 级状态:攒文本,流完一次落库(不逐 token 写)
    let mut reasoning = String::new(); // 坑 #4:工具轮的 reasoning 要回传,顺手攒下
    let mut ttft_ms: Option<i64> = None; // 首个增量事件锁存(思考也算"开口"——用户看得见动静)
    let mut spoke = false; // 本轮首个 Delta → 广播 Speaking(只发一次,不每字刷总线)
    loop {
        tokio::select! {
            // 协作式取消(不能硬 abort:会跳过 partial 落库和收尾事件)
            _ = token.cancelled() => {
                drop(rx); // provider 任务 send 失败即中止,HTTP 断开
                return RoundEnd::Cancelled { partial: buffer };
            }
            ev = rx.recv() => match ev {
                Some(ChatEvent::Delta(t)) => {
                    if !spoke {
                        spoke = true;
                        bus.publish(AppEvent::Mood(Mood::Speaking)); // 首字出 = 说话态(悬浮窗)
                    }
                    ttft_ms.get_or_insert_with(|| started.elapsed().as_millis() as i64);
                    buffer.push_str(&t);
                    // UI 不听了也继续攒:落库不依赖前端在场
                    let _ = tx.send(TurnEvent::Delta(t)).await;
                }
                Some(ChatEvent::Thinking(t)) => {
                    ttft_ms.get_or_insert_with(|| started.elapsed().as_millis() as i64);
                    reasoning.push_str(&t);
                    let _ = tx.send(TurnEvent::Thinking(t)).await;
                }
                Some(ChatEvent::Done { usage, stop_reason, tool_calls }) => {
                    if stop_reason.as_deref() == Some("max_tokens") {
                        // 截断不许装正常(robot 实战教训:静默截断 = 半截话当完整话)
                        tracing::warn!(conv = conv_id, "回复因 max_tokens 被截断");
                    }
                    let timing = usage::RoundTiming {
                        elapsed_ms: started.elapsed().as_millis() as i64,
                        ttft_ms,
                    };
                    tracing::info!(
                        input = usage.input_tokens,
                        output = usage.output_tokens,
                        cache_hit = usage.cache_hit_tokens,
                        elapsed_ms = timing.elapsed_ms,
                        ttft_ms = timing.ttft_ms,
                        stop = stop_reason.as_deref().unwrap_or("unknown"),
                        calls = tool_calls.len(),
                        conv = conv_id,
                        "回合轮完成"
                    );
                    return RoundEnd::Finished {
                        text: buffer,
                        reasoning: (!reasoning.is_empty()).then_some(reasoning),
                        tool_calls,
                        usage,
                        timing,
                    };
                }
                Some(ChatEvent::Failed(e)) => {
                    let app = AppError::from(e);
                    return RoundEnd::Failed { partial: buffer, kind: app.kind, message: app.message };
                }
                // 流断在 Done 之前:按网络失败处理,保住已有内容
                None => {
                    return RoundEnd::Failed {
                        partial: buffer,
                        kind: ErrorKind::Network,
                        message: "回复流提前结束".into(),
                    };
                }
            }
        }
    }
}

/// 并发执行一轮 tool_calls。错误也是观察:超时/报错/未知名都变成喂回模型的结果文本,
/// 模型自行换路(兜底逻辑通用化,不打断回合)。返回 (call, status, content)。
async fn run_tools(
    tools: &[Arc<dyn Tool>],
    calls: &[ToolCall],
    ctx: &ToolCtx,
) -> Vec<(ToolCall, String, String)> {
    let futs = calls.iter().map(|call| async move {
        if call.is_incomplete {
            // 规范 #6:截断的参数拒绝执行(robot 实战伤痕:半截文件编辑参数差点拿去干活)
            return (
                call.clone(),
                "error".to_string(),
                "参数不完整(可能被长度上限截断),没有执行。请换种方式或缩短参数重试。".to_string(),
            );
        }
        let Some(tool) = tools.iter().find(|t| t.spec().name == call.name) else {
            return (call.clone(), "error".to_string(), format!("没有叫 {} 的工具", call.name));
        };
        match tokio::time::timeout(tool.spec().timeout, tool.run(call.args.clone(), ctx)).await {
            Err(_) => {
                tracing::warn!(tool = %call.name, "工具执行超时,没有拿到结果");
                (call.clone(), "timeout".to_string(), "工具执行超时,没有拿到结果".to_string())
            }
            Ok(Err(e)) => {
                // 观测:工具失败原因进日志(之前只回喂模型,控制台看不见 → 用户"看不出问题")。
                // 错误仍当观察喂回模型(截断 500),但全量进日志便于排障。
                let full = format!("{e:#}");
                tracing::warn!(tool = %call.name, "工具执行出错: {full}");
                let msg: String = full.chars().take(500).collect();
                (call.clone(), "error".to_string(), msg)
            }
            Ok(Ok(out)) => (call.clone(), "ok".to_string(), out),
        }
    });
    futures_util::future::join_all(futs).await
}

fn tool_payload(call: &ToolCall, status: &str) -> Option<String> {
    serde_json::to_string(&ToolRowPayload {
        call_id: call.id.clone(),
        name: call.name.clone(),
        status: status.into(),
    })
    .ok()
}

async fn persist_row(
    store: &Store,
    conv_id: i64,
    role: &'static str,
    content: &str,
    payload: Option<&str>,
) -> Option<i64> {
    let store = store.clone();
    let content = content.to_string();
    let payload = payload.map(str::to_string);
    let result = tokio::task::spawn_blocking(move || {
        store.chat.append_message_full(conv_id, role, &content, payload.as_deref()).map(|m| m.id)
    })
    .await;
    match result {
        Ok(Ok(id)) => Some(id),
        Ok(Err(e)) => {
            tracing::error!(conv = conv_id, role, "消息落库失败: {e:#}");
            None
        }
        Err(e) => {
            tracing::error!(conv = conv_id, role, "落库任务挂了: {e}");
            None
        }
    }
}

/// partial 落库为普通消息:7274 确实说了半句,像人被打断 —— 对会话最诚实的记录。
async fn persist_partial(store: &Store, conv_id: i64, buffer: &str) {
    if buffer.trim().is_empty() {
        return;
    }
    persist_row(store, conv_id, "assistant", buffer, None).await;
}
