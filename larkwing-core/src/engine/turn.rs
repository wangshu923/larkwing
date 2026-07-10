//! 通用回合循环(engine 私有):全系统唯一一份,不认识任何具体任务(宪法 §5)。
//! 形状:开流 → 攒文本(照旧流 UI)/攒 tool_calls → 无调用则收尾 → 否则并发执行
//! (每工具超时 + 取消级联)→ 落库 → 回填再开流;空转 / 到顶 tool_choice=none 强制收尾,周期自检让模型自决。
//! turn 级状态 = 本文件里的局部变量,session 级在 SessionSlot,app 级在 Engine 字段(PLAN §4)。

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::llm::{ChatEvent, ChatMessage, ChatRequest, LlmProvider, ToolCall, ToolChoice, Usage};
use crate::bus::{AppEvent, Mood};
use crate::store::Store;
use crate::tools::{Tool, ToolCtx};

use super::{usage, AppError, AssistantPayload, ErrorKind, ToolRowPayload, ToolUseState, TurnEvent};

// 工具轮控制(PLAN §8):不是单个魔法数 —— 深度是任务属性,固定数调大放走失控、调小卡死深任务。
// 拆成三层:模型自检(智能判官)+ 空转兜底网(自检失灵时)+ 硬上限(纯 backstop)。
// 三个阈值眼下是常数;真做复杂桌面任务时按计划挪进 Scene.options 数据位、按场景调。
/// 硬上限:纯失控 backstop,正常永远碰不到;到顶强制用嘴收尾。
const MAX_TOOL_ROUNDS: usize = 200;
/// 每隔几轮给模型插一句中立自检,让它自己评要不要继续 / 收尾(智能判官)。
const SELF_CHECK_EVERY: usize = 10;
/// 连续多少轮"全重复调用 / 全报错"(无新进展)即判空转、强制收尾;
/// 接住"模型自欺、自检答继续"的洞 —— 真在干活每轮有新结果则清零,碰不到。
const MAX_STALL_ROUNDS: usize = 5;

/// 回合任一出口都把 mood 收回 Idle(悬浮窗 mood 灯熄):Drop 兜底覆盖所有 return 分支。
struct MoodGuard(crate::bus::Bus);
impl Drop for MoodGuard {
    fn drop(&mut self) {
        self.0.publish(AppEvent::Mood(Mood::Idle));
    }
}

/// 旁听临时回合(唤醒确认层「呼名+续句」仲裁)的**悬置用户行**:先不落库,模型开口 /
/// 调真工具那一刻才随首次持久化一起写入(= 转正);只回 __IGNORE__ / 空 = 整轮蒸发——
/// 不进 UI、不进历史、不进记忆,只留 tracing 与 usage 流水(观测归观测,§6.4)。
pub(super) struct PendingUser {
    pub content: String,
    pub payload: Option<String>,
}

/// 旁听转正:把悬置的 user 行落库(此后与普通回合无异)。幂等(take 后为 None);
/// 返回 false = user 行落库失败,调用方按落库失败收尾。
async fn commit_overheard(
    store: &Store,
    conv_id: i64,
    pending: &mut Option<PendingUser>,
    tx: &mpsc::Sender<TurnEvent>,
) -> bool {
    let Some(p) = pending.take() else { return true };
    match persist_row(store, conv_id, "user", &p.content, p.payload.as_deref()).await {
        Some(id) => {
            let _ = tx.send(TurnEvent::Committed { message_id: id }).await;
            true
        }
        None => false,
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
    /// 壳层网页渲染器(进 ToolCtx;None = 壳层没注入,web_render 工具如实退回)。
    pub web: Option<Arc<dyn crate::webrender::WebRenderer>>,
    /// 第 1 轮已开的流(建连失败切换发生在 engine;Turn 内不再切换)。
    pub rx: mpsc::Receiver<ChatEvent>,
    /// 插队队列(PLAN §9 B):与 engine.inject 命令共用;回合在轮间/收尾前排空它。
    pub inject: Arc<Mutex<super::InjectState>>,
    /// 旁听临时回合的悬置用户行(send_overheard 传入):Some = 本回合是旁听仲裁——
    /// 转正前 Delta/Thinking/mood 全静音(可能整轮蒸发,不给用户看半截);None = 普通回合。
    pub overheard: Option<PendingUser>,
}

/// 一轮流式消费的结局。
enum RoundEnd {
    Finished {
        text: String,
        reasoning: Option<String>,
        tool_calls: Vec<ToolCall>,
        /// 不透明 reasoning 状态(原生方言经 ChatEvent::ReasoningState 传出;兼容方言 None)。
        reasoning_state: Option<serde_json::Value>,
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
            web,
            mut rx,
            inject,
            mut overheard,
        } = self;
        // 本回合防溢出预算:按**实际服务的** provider 的窗口算(可能是 fallback,故比建连前的主候选更准)。
        let budget = super::context::tail_budget_chars(
            crate::llm::catalog::ctx_window_of(provider.model_id()),
            crate::llm::catalog::billing_of(provider.model_id()),
        );
        // mood 上总线(PLAN §12 修订):悬浮窗据此显「正在想/正在说」;
        // Guard 在任一出口(done/failed/cancelled/落库失败/建连失败)收回 Idle。
        let bus = media.bus().clone();
        // 旁听未转正期间不点 mood 灯(可能整轮蒸发,悬浮窗别"正在想"两秒又灭 —— 零痕迹)
        if overheard.is_none() {
            bus.publish(AppEvent::Mood(Mood::Thinking));
        }
        let _mood = MoodGuard(bus.clone());
        // 回合任一出口闸上注入队列(Failed/Cancelled 退出后别再往死队列里塞)
        let _inject_guard = InjectGuard(inject.clone());
        let meta = usage::RoundMeta { user_id, conv_id, user_msg_id, provider_id, model };
        let mut round_start = first_round_start;
        let ctx = ToolCtx { user_id, conv_id, store: store.clone(), media, web };
        let label_of = |name: &str| -> String {
            tools
                .iter()
                .find(|t| t.spec().name == name)
                .map(|t| t.spec().ui_key.to_string())
                .unwrap_or_else(|| "tool.unknown".into())
        };

        let mut round: usize = 0;
        let mut stall: usize = 0; // 连续无进展(全重复 / 全失败)轮数
        let mut seen_calls: HashSet<String> = HashSet::new(); // 本回合已发过的工具调用指纹
        loop {
            // 旁听未转正 = 静音消费(Delta/Thinking/mood 不外发;蒸发时用户从头到尾无感)
            let muted = overheard.is_some();
            let (text, reasoning, tool_calls, reasoning_state) =
                match drain(&token, &tx, &bus, rx, conv_id, round_start, muted).await {
                    RoundEnd::Cancelled { partial } => {
                        // 旁听未转正被取消(真输入把仲裁挤掉了):什么都没发生过,不落 partial
                        if overheard.is_none() {
                            persist_partial(&store, conv_id, &partial).await;
                        }
                        let _ = tx.send(TurnEvent::Cancelled).await;
                        return;
                    }
                    RoundEnd::Failed { partial, kind, message } => {
                        // 旁听未转正就失败(仲裁自身出错):没有可见变化,只留日志
                        if overheard.is_none() {
                            persist_partial(&store, conv_id, &partial).await;
                        } else {
                            tracing::warn!(conv = conv_id, %message, "旁听仲裁失败(未转正,无可见变化)");
                        }
                        let _ = tx.send(TurnEvent::Failed { kind, message }).await;
                        return;
                    }
                    RoundEnd::Finished { text, reasoning, tool_calls, reasoning_state, usage, timing } => {
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
                        (text, reasoning, tool_calls, reasoning_state)
                    }
                };

            // 纯文本收尾 —— 或端点无视 tool_choice=none 仍要调工具(防御):都按终回处理。
            // 收尾前先看插队(PLAN §9 B):有就先落这段回复、把注入接上、重开流继续(不打断进度)。
            if tool_calls.is_empty() || round >= MAX_TOOL_ROUNDS || stall >= MAX_STALL_ROUNDS {
                if !tool_calls.is_empty() {
                    tracing::warn!(conv = conv_id, "工具轮到顶 / 空转仍想调用,丢弃调用强制收尾");
                }
                // 旁听仲裁判决点:模型只回 __IGNORE__ / 空 = 「不是叫我」→ 整轮蒸发
                // (user 行还悬着没落,直接 return = 不进 UI/历史/记忆;只留这行日志和 usage 流水)
                if overheard.is_some() {
                    let verdict = text.trim();
                    if verdict.is_empty() || verdict == "__IGNORE__" {
                        tracing::info!(conv = conv_id, "旁听仲裁:不是叫我 → 整轮蒸发");
                        let _ = tx.send(TurnEvent::Dismissed).await;
                        return;
                    }
                    // 开口了 = 转正:先落悬置的 user 行,再走正常收尾
                    if !commit_overheard(&store, conv_id, &mut overheard, &tx).await {
                        let _ = tx
                            .send(TurnEvent::Failed {
                                kind: ErrorKind::Internal,
                                message: "旁听转正落库失败".into(),
                            })
                            .await;
                        return;
                    }
                }
                // 收尾前看插队(原子):空 → 置 finishing 收尾;非空 → 取出注入(先落本段回复再 apply)
                let pending = take_or_finish(&inject);
                if pending.is_empty() {
                    // 真收尾(take_or_finish 内已原子置 finishing,此后 inject 拒绝)
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
                // 用户在收尾前插了话:先把这段回复落库(它答上一段输入),保证历史顺序
                // assistant(本段回复) 在 user(注入) 之前;再 apply 注入、重开流继续答。
                if persist_row(&store, conv_id, "assistant", &text, None).await.is_none() {
                    let _ = tx
                        .send(TurnEvent::Failed {
                            kind: ErrorKind::Internal,
                            message: "回复落库失败".into(),
                        })
                        .await;
                    return;
                }
                for it in pending {
                    apply_injection(&store, conv_id, &tx, &mut request, it).await;
                }
                round = 0; // 新输入 → 新一轮工具预算
                stall = 0;
                seen_calls.clear();
                request.tool_choice = ToolChoice::Auto;
                round_start = std::time::Instant::now();
                super::context::cap_messages_tail(&mut request.messages, budget); // 防溢出安全阀(注入后)
                match provider.chat_stream(request.clone()).await {
                    Ok(next) => rx = next,
                    Err(e) => {
                        let app = AppError::from(e);
                        let _ = tx.send(TurnEvent::Failed { kind: app.kind, message: app.message }).await;
                        return;
                    }
                }
                continue;
            }

            // ---- 工具轮 ----
            // 旁听 + 要调真工具 = 干活即转正(工具有副作用,必须先把悬置 user 行落了,
            // 历史完形:user → assistant(tool_calls) → tool 顺序不乱)
            if overheard.is_some()
                && !commit_overheard(&store, conv_id, &mut overheard, &tx).await
            {
                let _ = tx
                    .send(TurnEvent::Failed {
                        kind: ErrorKind::Internal,
                        message: "旁听转正落库失败".into(),
                    })
                    .await;
                return;
            }
            bus.publish(AppEvent::Mood(Mood::Thinking)); // 工具执行中 = 思考态(本轮若已 Speaking 过则切回;旁听转正后首次点灯)
            let payload = serde_json::to_string(&AssistantPayload {
                tool_calls: tool_calls.clone(),
                reasoning: reasoning.clone(),
                reasoning_state: reasoning_state.clone(),
            })
            .ok();
            match persist_row(&store, conv_id, "assistant", &text, payload.as_deref()).await {
                // 这一轮带了可见文字 + 还要继续调工具:它在落库里是独立 assistant 内容行,通知前端
                // 封口当前气泡(钉 mid 供「想了想」回挂)、另起新泡接后续文字 —— 在飞结构对齐落库。
                Some(mid) => {
                    if !text.trim().is_empty() {
                        let _ = tx.send(TurnEvent::Segment { message_id: mid }).await;
                    }
                }
                None => {
                    let _ = tx
                        .send(TurnEvent::Failed {
                            kind: ErrorKind::Internal,
                            message: "工具轮落库失败".into(),
                        })
                        .await;
                    return;
                }
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
                reasoning_state,
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

            // 进展守卫:本轮是否有"新调用且成功"的结果。全重复 / 全失败 = 这轮没推进,计一次空转;
            // 真在干活(每轮有新成功结果)则清零,深任务因此永远碰不到兜底网。先登记全部指纹再综合判。
            let mut made_progress = false;
            for (call, status, _) in &results {
                let fresh = seen_calls.insert(call_fingerprint(call));
                if fresh && status == "ok" {
                    made_progress = true;
                }
            }
            stall = if made_progress { 0 } else { stall + 1 };

            // 插队(PLAN §9 B):轮间排空注入队列,append 进 request,下一轮 LLM 就带上
            drain_injections(&store, conv_id, &tx, &inject, &mut request).await;
            round += 1;
            // 收尾闸:① 硬上限到顶(纯失控 backstop)② 连续空转到顶(自检失灵的兜底网)。
            // 命中就禁用工具,下一次开流模型只能用嘴收尾;否则按周期插一句自检让模型自决。
            if round >= MAX_TOOL_ROUNDS || stall >= MAX_STALL_ROUNDS {
                request.tool_choice = ToolChoice::None;
                if stall >= MAX_STALL_ROUNDS {
                    tracing::warn!(conv = conv_id, round, stall, "连续多轮无新进展,判定空转,强制收尾");
                }
            } else if round % SELF_CHECK_EVERY == 0 {
                // 软提示自检(PLAN §8):中立一句进 request 尾部 —— 不落库(不进历史 / 不重放)、
                // 处于已变动的工具结果尾后(不破前缀缓存)。让当前模型自己评要不要继续:
                // 智能判官在前,机械空转网只兜它"自欺答继续"的洞。
                request.messages.push(ChatMessage::user(format!(
                    "[system] You have made {round} consecutive rounds of tool calls. \
                     Reassess now: if the task is complete, or cannot make progress right now, \
                     stop calling tools and reply to the user directly in natural language. \
                     Continue only if you are genuinely making progress."
                )));
            }

            // 再开流,粘住同一 provider。此处建连失败不切换:半截对话已经发生,
            // 换人重说会精神分裂 —— 走 Failed 友好兜底(铁律 §3.5)。
            round_start = std::time::Instant::now(); // 2+ 轮计时:从本轮建连起
            // 防溢出安全阀:工具轮累积 ToolResult(单条可达 4 万字)会撑大 request → 开流前封顶。
            super::context::cap_messages_tail(&mut request.messages, budget);
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

/// 回合任一出口闸上注入队列:此后 inject 命令一律拒绝(防往已死回合的队列里塞而丢消息)。
struct InjectGuard(Arc<Mutex<super::InjectState>>);
impl Drop for InjectGuard {
    fn drop(&mut self) {
        if let Ok(mut st) = self.0.lock() {
            st.finishing = true;
        }
    }
}

/// 轮间排空注入队列:把排队插进来的 user 消息落库 + 发 Injected + append 进 request(下一轮带上)。
async fn drain_injections(
    store: &Store,
    conv_id: i64,
    tx: &mpsc::Sender<TurnEvent>,
    inject: &Arc<Mutex<super::InjectState>>,
    request: &mut ChatRequest,
) {
    let items = {
        let mut st = inject.lock().expect("inject lock poisoned");
        std::mem::take(&mut st.buffer)
    };
    for it in items {
        apply_injection(store, conv_id, tx, request, it).await;
    }
}

/// 收尾前原子检查:队列空 → 置 finishing 收尾(返回空);非空 → 取出注入交调用方
/// (调用方落完本段回复后再 apply,保证 assistant(回复) 在 user(注入) 之前,历史顺序正确)。
fn take_or_finish(inject: &Arc<Mutex<super::InjectState>>) -> Vec<super::InjectReady> {
    let mut st = inject.lock().expect("inject lock poisoned");
    if st.buffer.is_empty() {
        st.finishing = true; // 原子闸上:此后 inject 命令一律拒绝(防丢)
        Vec::new()
    } else {
        std::mem::take(&mut st.buffer)
    }
}

/// 一条注入:落 user 行 → 发 Injected(带 id + 原话 + 小票)→ append 进 request(下一轮带上)。
async fn apply_injection(
    store: &Store,
    conv_id: i64,
    tx: &mpsc::Sender<TurnEvent>,
    request: &mut ChatRequest,
    it: super::InjectReady,
) {
    // 落库用 llm_content(含文档文字)→ 注入的文档也进 history、多轮还在(与主发送路径一致,§9);
    // UI 事件仍用 display(用户原文,不灌文档正文)。无文档时 llm_content == display,行为不变。
    if let Some(id) =
        persist_row(store, conv_id, "user", &it.llm_content, it.payload.as_deref()).await
    {
        let _ = tx
            .send(TurnEvent::Injected { message_id: id, text: it.display, attachments: it.refs })
            .await;
    }
    request.messages.push(ChatMessage::user_with_parts(it.llm_content, it.parts));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{InjectReady, InjectState};

    fn temp_store(name: &str) -> Store {
        let p = std::env::temp_dir().join(format!("lw_turn_{}_{}.db", std::process::id(), name));
        let _ = std::fs::remove_file(&p);
        Store::open(&p).unwrap()
    }
    fn ready(text: &str) -> InjectReady {
        InjectReady {
            display: text.into(),
            llm_content: text.into(),
            parts: Vec::new(),
            refs: Vec::new(),
            payload: None,
        }
    }

    // 插队应用:落 user 行 + 发 Injected + append 进 request(下一轮 LLM 就带上)
    #[tokio::test]
    async fn apply_injection_persists_emits_and_appends() {
        let store = temp_store("apply");
        let uid = store.users.ensure_default_user().unwrap().id;
        let conv = store.chat.create_conversation(uid, "companion").unwrap();
        let (tx, mut rx) = mpsc::channel::<TurnEvent>(8);
        let mut request = ChatRequest::default();

        apply_injection(&store, conv.id, &tx, &mut request, ready("插一句:主角叫小七")).await;

        assert!(
            matches!(request.messages.last(), Some(ChatMessage::User { content, .. }) if content == "插一句:主角叫小七"),
            "注入的 user 消息要 append 进 request",
        );
        match rx.try_recv() {
            Ok(TurnEvent::Injected { message_id, text, .. }) => {
                assert!(message_id > 0);
                assert_eq!(text, "插一句:主角叫小七");
            }
            other => panic!("应发 Injected,实际 {other:?}"),
        }
        let msgs = store.chat.recent_messages(conv.id, 10).unwrap();
        assert!(msgs.iter().any(|m| m.role == "user" && m.content == "插一句:主角叫小七"), "落库一条 user 行");
    }

    // 收尾闸:空队列 → 置 finishing 收尾(原子防丢:此后 inject 命令一律拒绝)
    #[test]
    fn take_or_finish_sets_finishing_when_empty() {
        let inject = Arc::new(Mutex::new(InjectState::default()));
        let pending = take_or_finish(&inject);
        assert!(pending.is_empty(), "空队列返回空");
        assert!(inject.lock().unwrap().finishing, "收尾后闸应置位");
    }

    // 收尾前有插队 → 取出注入、不置 finishing、队列清空(交调用方落完本段回复后再 apply)
    #[test]
    fn take_or_finish_takes_items_without_finishing() {
        let inject = Arc::new(Mutex::new(InjectState::default()));
        inject.lock().unwrap().buffer.push(ready("等下,改成科幻风"));
        let pending = take_or_finish(&inject);
        assert_eq!(pending.len(), 1, "非空时取出注入");
        assert_eq!(pending[0].display, "等下,改成科幻风");
        assert!(!inject.lock().unwrap().finishing, "有插队时不置 finishing");
        assert!(inject.lock().unwrap().buffer.is_empty(), "取出后队列清空");
    }
}

/// 消费一轮流:文本/思考边攒边转发,Done 收口。取消 = drop rx,provider 任务随之中止。
/// started = 本轮开流(建连)时刻:TTFT 锁存在第一个增量事件,elapsed 盖章在收尾。
/// muted = 旁听未转正:增量与 mood 都不外发(只攒),整轮可能蒸发 —— 不给用户看半截。
async fn drain(
    token: &CancellationToken,
    tx: &mpsc::Sender<TurnEvent>,
    bus: &crate::bus::Bus,
    mut rx: mpsc::Receiver<ChatEvent>,
    conv_id: i64,
    started: std::time::Instant,
    muted: bool,
) -> RoundEnd {
    let mut buffer = String::new(); // turn 级状态:攒文本,流完一次落库(不逐 token 写)
    let mut reasoning = String::new(); // 坑 #4:工具轮的 reasoning 要回传,顺手攒下
    let mut reasoning_state: Option<serde_json::Value> = None; // 不透明 reasoning 状态(原生方言发)
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
                        if !muted {
                            bus.publish(AppEvent::Mood(Mood::Speaking)); // 首字出 = 说话态(悬浮窗)
                        }
                    }
                    ttft_ms.get_or_insert_with(|| started.elapsed().as_millis() as i64);
                    buffer.push_str(&t);
                    // UI 不听了也继续攒:落库不依赖前端在场
                    if !muted {
                        let _ = tx.send(TurnEvent::Delta(t)).await;
                    }
                }
                Some(ChatEvent::Thinking(t)) => {
                    ttft_ms.get_or_insert_with(|| started.elapsed().as_millis() as i64);
                    reasoning.push_str(&t);
                    if !muted {
                        let _ = tx.send(TurnEvent::Thinking(t)).await;
                    }
                }
                Some(ChatEvent::ReasoningState(v)) => {
                    // 不透明 reasoning 状态:攒下,Done 时随 Finished 带出(逐字保真,不解析)
                    reasoning_state = Some(v);
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
                        reasoning_state,
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

/// 工具调用指纹(进展守卫用):同名 + 同参 = 同一调用。args 的 Display 即紧凑 JSON,
/// 模型重复调同一个工具会发出相同串 → 命中"重复",据此判空转。
fn call_fingerprint(call: &ToolCall) -> String {
    format!("{}:{}", call.name, call.args)
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
