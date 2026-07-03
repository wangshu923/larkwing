//! engine 集成测试:FakeLlm 驱动整条回合管线(不碰网络)。

use std::path::PathBuf;
use std::sync::Arc;

use larkwing_core::engine::{Engine, TurnEvent};
use larkwing_core::llm::fake::{FakeLlm, FakeTurn};
use larkwing_core::llm::ToolCall;
use larkwing_core::scenes::Scenes;
use larkwing_core::store::Store;

fn temp_db(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "larkwing_engine_test_{}_{}.db",
        std::process::id(),
        name
    ));
    let _ = std::fs::remove_file(&p);
    p
}

fn setup(name: &str, delay_ms: u64) -> (Store, Arc<Engine>, i64) {
    let store = Store::open(&temp_db(name)).unwrap();
    let engine = Engine::new(store.clone(), Scenes::builtin());
    engine.set_provider(Some(Arc::new(FakeLlm::with_delay(delay_ms))));
    let user = store.users.ensure_default_user().unwrap();
    let conv = store.chat.create_conversation(user.id, "companion").unwrap();
    (store, engine, conv.id)
}

#[tokio::test(flavor = "multi_thread")]
async fn turn_streams_and_persists_assistant_reply() {
    let (store, engine, conv_id) = setup("roundtrip", 1);

    let mut rx = engine.send_message(conv_id, "你好呀".into(), None, vec![]).await.unwrap();
    let mut streamed = String::new();
    let mut done_id = None;
    while let Some(ev) = rx.recv().await {
        match ev {
            TurnEvent::Delta(t) => streamed.push_str(&t),
            TurnEvent::Done { message_id } => done_id = Some(message_id),
            other => panic!("意外事件: {other:?}"),
        }
    }

    let done_id = done_id.expect("必须收到 Done");
    let msgs = store.chat.recent_messages(conv_id, 10).unwrap();
    assert_eq!(msgs.len(), 2, "user + assistant");
    assert_eq!(msgs[0].role, "user");
    assert_eq!(msgs[1].role, "assistant");
    assert_eq!(msgs[1].id, done_id, "Done 带的 id 与落库一致");
    assert_eq!(msgs[1].content, streamed, "落库内容 = 流式攒出的内容");
    assert!(streamed.contains("你好呀"), "FakeLlm 会回声用户输入");
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_mid_stream_persists_partial_and_emits_cancelled() {
    let (store, engine, conv_id) = setup("cancel", 25);

    let mut rx = engine.send_message(conv_id, "讲个长故事".into(), None, vec![]).await.unwrap();
    let mut cancelled = false;
    let mut deltas = 0;
    while let Some(ev) = rx.recv().await {
        match ev {
            TurnEvent::Delta(_) => {
                deltas += 1;
                if deltas == 3 {
                    engine.cancel(conv_id).await; // 显式取消(停止按钮路径)
                }
            }
            TurnEvent::Cancelled => cancelled = true,
            TurnEvent::Done { .. } => panic!("取消后不该收到 Done"),
            _ => {}
        }
    }
    assert!(cancelled, "必须收到 Cancelled 收尾事件");

    // partial 落库为普通 assistant 消息(半句话,诚实记录)
    let msgs = store.chat.recent_messages(conv_id, 10).unwrap();
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[1].role, "assistant");
    assert!(!msgs[1].content.is_empty(), "partial 非空才落库");
}

#[tokio::test(flavor = "multi_thread")]
async fn new_send_cancels_inflight_and_history_stays_complete() {
    let (store, engine, conv_id) = setup("replace", 25);

    // 第一条还在飞
    let mut rx1 = engine.send_message(conv_id, "第一条".into(), None, vec![]).await.unwrap();
    // 等到它至少吐了一个字,确保 partial 非空
    loop {
        match rx1.recv().await {
            Some(TurnEvent::Delta(_)) => break,
            Some(_) => continue,
            None => panic!("流意外结束"),
        }
    }

    // 立刻发第二条:隐式取消旧回合,且 await 其收尾(partial 先落库)
    let mut rx2 = engine.send_message(conv_id, "第二条".into(), None, vec![]).await.unwrap();

    // 旧流收到 Cancelled
    let mut old_cancelled = false;
    while let Some(ev) = rx1.recv().await {
        if matches!(ev, TurnEvent::Cancelled) {
            old_cancelled = true;
        }
    }
    assert!(old_cancelled, "旧回合必须被取消");

    // 新回合正常完成
    let mut done = false;
    while let Some(ev) = rx2.recv().await {
        if matches!(ev, TurnEvent::Done { .. }) {
            done = true;
        }
    }
    assert!(done);

    // 历史完形:user1, partial-assistant, user2, assistant
    let msgs = store.chat.recent_messages(conv_id, 10).unwrap();
    let roles: Vec<&str> = msgs.iter().map(|m| m.role.as_str()).collect();
    assert_eq!(roles, vec!["user", "assistant", "user", "assistant"]);
    assert_eq!(msgs[0].content, "第一条");
    assert_eq!(msgs[2].content, "第二条");
}

fn call(id: &str, name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall { id: id.into(), name: name.into(), args, is_incomplete: false }
}

// 通用循环全链路:模型要工具 → 并发执行(remember 真写记忆/now 真读钟)→ 回填再调
// → 终回流式收尾。行序、payload、事件、记忆全部对账。
#[tokio::test(flavor = "multi_thread")]
async fn tool_round_executes_persists_and_finishes() {
    let (store, engine, conv_id) = setup("tools", 1);
    engine.set_provider(Some(Arc::new(FakeLlm::scripted(vec![
        FakeTurn {
            text: String::new(),
            tool_calls: vec![
                call("call_a", "remember", serde_json::json!({ "fact": "用户对花生过敏" })),
                call("call_b", "now", serde_json::json!({})),
            ],
            ..Default::default()
        },
        FakeTurn { text: "记下啦!".into(), ..Default::default() },
    ]))));

    let mut rx = engine.send_message(conv_id, "我对花生过敏,现在几点?".into(), None, vec![]).await.unwrap();
    let mut streamed = String::new();
    let mut tool_started = 0;
    let mut tool_finished = 0;
    let mut done_id = None;
    while let Some(ev) = rx.recv().await {
        match ev {
            TurnEvent::Delta(t) => streamed.push_str(&t),
            TurnEvent::ToolUse { label, state } => {
                assert!(label.starts_with("tool."), "label 是 i18n 键,不露工具概念");
                match state {
                    larkwing_core::engine::ToolUseState::Started => tool_started += 1,
                    larkwing_core::engine::ToolUseState::Finished => tool_finished += 1,
                }
            }
            TurnEvent::Done { message_id } => done_id = Some(message_id),
            TurnEvent::Thinking(_) => {}
            other => panic!("意外事件: {other:?}"),
        }
    }
    assert_eq!((tool_started, tool_finished), (2, 2));
    assert_eq!(streamed, "记下啦!");

    // 行序与 payload:user / assistant(带 tool_calls)/ tool×2 / assistant 终回
    let msgs = store.chat.recent_messages(conv_id, 10).unwrap();
    let roles: Vec<&str> = msgs.iter().map(|m| m.role.as_str()).collect();
    assert_eq!(roles, ["user", "assistant", "tool", "tool", "assistant"]);
    assert!(msgs[1].payload.as_deref().unwrap_or("").contains("call_a"));
    assert!(msgs[2].payload.as_deref().unwrap_or("").contains("\"status\":\"ok\""));
    assert_eq!(msgs[4].id, done_id.expect("必须收到 Done"), "Done 带的 id = 终回落库 id");

    // 工具真干活了:remember 写进了记忆域,now 返回了合法 JSON
    let user = store.users.ensure_default_user().unwrap();
    let mems = store.memory.list(user.id).unwrap();
    assert!(mems.iter().any(|m| m.content.contains("花生")), "remember 必须落进 store.memory");
    let now_row = msgs.iter().find(|m| {
        m.role == "tool" && m.payload.as_deref().unwrap_or("").contains("\"name\":\"now\"")
    });
    let parsed: serde_json::Value = serde_json::from_str(&now_row.unwrap().content).unwrap();
    assert!(parsed.get("now").is_some());
}

// 空转兜底网:模型连环调"同名同参"的工具(全无新进展)→ 连续 MAX_STALL_ROUNDS 轮后
// tool_choice=none 强制收尾(FakeLlm 尊重它),绝不无限循环。首轮算进展(头回拿到结果),
// 其后 5 次重复触发空转 → 恰好 1 + MAX_STALL_ROUNDS = 6 条 tool 行。
// (硬上限 200 是纯失控 backstop:要 200+ 轮各不相同的成功调用才碰得到,不在单测里铺;
//  自检软提示的"功效"也不在此测 —— FakeLlm 不读消息内容,要真模型才验得出,留给真机。)
#[tokio::test(flavor = "multi_thread")]
async fn repeated_calls_trip_stall_net_and_force_finish() {
    let (store, engine, conv_id) = setup("stallnet", 1);
    let turns: Vec<FakeTurn> = (0..7)
        .map(|i| FakeTurn {
            text: format!("查{i}"),
            tool_calls: vec![call(&format!("call_{i}"), "now", serde_json::json!({}))],
            ..Default::default()
        })
        .collect();
    engine.set_provider(Some(Arc::new(FakeLlm::scripted(turns))));

    let mut rx = engine.send_message(conv_id, "一直查时间".into(), None, vec![]).await.unwrap();
    let mut done = false;
    while let Some(ev) = rx.recv().await {
        if matches!(ev, TurnEvent::Done { .. }) {
            done = true;
        }
    }
    assert!(done, "空转到顶必须正常收尾,不许挂死");

    let msgs = store.chat.recent_messages(conv_id, 30).unwrap();
    let tool_rows = msgs.iter().filter(|m| m.role == "tool").count();
    assert_eq!(tool_rows, 6, "1 轮首次进展 + MAX_STALL_ROUNDS(5) 轮重复空转 = 6 条 tool 行");
    assert_eq!(msgs.last().unwrap().role, "assistant");
    assert_eq!(msgs.last().unwrap().content, "查6", "末轮被 tool_choice=none 禁了工具,只剩嘴");
}

// 进展守卫不误杀:每轮都有"新且成功"的调用(真在干活)→ stall 始终清零,
// 跑足 7 轮(> MAX_STALL_ROUNDS)也不被兜底网切断,直到模型自己收尾。
// 这是"深任务不被卡死"的另一半保证 —— 调大上限之所以安全,正因为有它。
#[tokio::test(flavor = "multi_thread")]
async fn fresh_progress_keeps_stall_at_zero_so_deep_work_runs() {
    let (store, engine, conv_id) = setup("deepwork", 1);
    let mut turns: Vec<FakeTurn> = (0..7)
        .map(|i| FakeTurn {
            text: String::new(),
            tool_calls: vec![call(
                &format!("call_{i}"),
                "remember",
                serde_json::json!({ "fact": format!("第{i}条事实") }),
            )],
            ..Default::default()
        })
        .collect();
    turns.push(FakeTurn { text: "都记下啦!".into(), ..Default::default() }); // 模型自然收尾
    engine.set_provider(Some(Arc::new(FakeLlm::scripted(turns))));

    let mut rx = engine.send_message(conv_id, "记一连串事实".into(), None, vec![]).await.unwrap();
    let mut done = false;
    while let Some(ev) = rx.recv().await {
        if matches!(ev, TurnEvent::Done { .. }) {
            done = true;
        }
    }
    assert!(done);

    let msgs = store.chat.recent_messages(conv_id, 40).unwrap();
    let tool_rows = msgs.iter().filter(|m| m.role == "tool").count();
    assert_eq!(tool_rows, 7, "7 轮都有新进展 → 超过 MAX_STALL_ROUNDS 也不被切断");
    assert_eq!(msgs.last().unwrap().content, "都记下啦!", "由模型自己收尾,不是被强制");
}

// 白名单外/幻觉工具名:错误也是观察,变成结果喂回模型,回合不崩
#[tokio::test(flavor = "multi_thread")]
async fn unknown_tool_becomes_error_observation() {
    let (store, engine, conv_id) = setup("ghosttool", 1);
    engine.set_provider(Some(Arc::new(FakeLlm::scripted(vec![
        FakeTurn {
            text: String::new(),
            tool_calls: vec![call("call_g", "ghost", serde_json::json!({}))],
            ..Default::default()
        },
        FakeTurn { text: "啊我没这个本事,换个方式~".into(), ..Default::default() },
    ]))));

    let mut rx = engine.send_message(conv_id, "干点怪事".into(), None, vec![]).await.unwrap();
    let mut done = false;
    while let Some(ev) = rx.recv().await {
        if matches!(ev, TurnEvent::Done { .. }) {
            done = true;
        }
    }
    assert!(done);
    let msgs = store.chat.recent_messages(conv_id, 10).unwrap();
    let tool_row = msgs.iter().find(|m| m.role == "tool").expect("错误也要落 tool 行");
    assert!(tool_row.payload.as_deref().unwrap_or("").contains("\"status\":\"error\""));
    assert!(tool_row.content.contains("ghost"));
}

// 记账灯带:provider 回了 usage 的轮才点灯;工具回合每轮各发一次,今日账本跨轮累加;
// FakeLlm 模型不在目录 → 不报钱只报 token,today.unpriced 如实标记(不装懂)。
// 同时验证流水真落了 usage_rounds(一轮一行,分析的原料)。
#[tokio::test(flavor = "multi_thread")]
async fn usage_events_fire_per_round_and_accumulate_today() {
    use larkwing_core::llm::Usage;
    let (store, engine, conv_id) = setup("usage", 1);
    engine.set_provider(Some(Arc::new(FakeLlm::scripted(vec![
        FakeTurn {
            text: String::new(),
            tool_calls: vec![call("call_t", "now", serde_json::json!({}))],
            usage: Usage { input_tokens: 100, output_tokens: 10, cache_hit_tokens: 64 },
        },
        FakeTurn {
            text: "现在三点啦".into(),
            usage: Usage { input_tokens: 150, output_tokens: 20, cache_hit_tokens: 128 },
            ..Default::default()
        },
    ]))));

    let mut rx = engine.send_message(conv_id, "几点了?".into(), None, vec![]).await.unwrap();
    let mut rounds = Vec::new();
    let mut last_today = None;
    let mut last_conv = None;
    while let Some(ev) = rx.recv().await {
        if let TurnEvent::Usage { round, today, conv } = ev {
            rounds.push(round);
            last_today = Some(today);
            last_conv = Some(conv);
        }
    }
    assert_eq!(rounds.len(), 2, "工具回合两轮 LLM 调用,各点一次灯");
    assert_eq!(rounds[0].input_tokens, 100);
    assert_eq!(rounds[1].output_tokens, 20);
    assert!(rounds[0].cost_usd.is_none(), "FakeLlm 模型不在目录:只报 token 不报钱");
    // 计时:第 1 轮纯 tool_call 没吐字 → TTFT 如实为 None;第 2 轮流了字 → 有 TTFT 且 ≤ 总耗时
    assert!(rounds[0].ttft_ms.is_none(), "没吐过字的轮不该有首字延迟");
    let ttft = rounds[1].ttft_ms.expect("流了字的轮必须有首字延迟");
    assert!(rounds[1].elapsed_ms >= ttft, "总耗时不能短于首字延迟");
    let today = last_today.expect("必须带今日累计快照");
    assert_eq!(today.input_tokens, 250);
    assert_eq!(today.output_tokens, 30);
    assert_eq!(today.cache_hit_tokens, 192);
    assert!(today.unpriced);
    // 会话累计快照:事件携带 = 重新查询 = 250(灯带"话题"段;重启/切话题的初值同源)
    assert_eq!(last_conv.expect("必须带会话累计").input_tokens, 250);
    assert_eq!(engine.usage_conversation(conv_id).input_tokens, 250);
    // 账本落了库:重新查与事件一致(重启后灯带初值从这来)
    assert_eq!(engine.usage_today().input_tokens, 250);
    // 流水持久化:两轮 = 两行,窗口聚合与事件对账(分析就吃这张表)
    let totals = store.usage.totals_since(0).unwrap();
    assert_eq!(totals.input_tokens, 250);
    assert_eq!(totals.unpriced_rounds, 2, "FakeLlm 两轮都估不出价,NULL 如实落库");
}

#[tokio::test(flavor = "multi_thread")]
async fn send_without_provider_fails_preflight_with_no_api_key() {
    let store = Store::open(&temp_db("nokey")).unwrap();
    let engine = Engine::new(store.clone(), Scenes::builtin());
    let user = store.users.ensure_default_user().unwrap();
    let conv = store.chat.create_conversation(user.id, "companion").unwrap();

    let err = engine.send_message(conv.id, "在吗".into(), None, vec![]).await.err().unwrap();
    assert_eq!(err.kind, larkwing_core::engine::ErrorKind::NoApiKey);
}

#[tokio::test(flavor = "multi_thread")]
async fn boot_creates_conversation_and_offers_opening_line() {
    let store = Store::open(&temp_db("boot")).unwrap();
    let engine = Engine::new(store, Scenes::builtin());
    let snap = engine.boot().unwrap();
    assert!(!snap.has_api_key);
    assert!(snap.messages.is_empty());
    assert!(snap.opening_line.is_some(), "空会话要给开场白");
    assert_eq!(snap.locale, "zh-CN", "未设置时 locale 默认 zh-CN");

    // 再 boot 回到同一会话,不重复建
    let snap2 = engine.boot().unwrap();
    assert_eq!(snap.conversation.id, snap2.conversation.id);
}

// 供应商卡片管理:预设漏出且预填、钥匙掩码不泄明文、upsert 合并、内置不可删
#[tokio::test(flavor = "multi_thread")]
async fn provider_cards_prefill_mask_and_upsert() {
    use larkwing_core::engine::ProviderPatch;
    let (store, engine, _) = setup("providers", 1);
    store.settings.set(None, "llm.api_key", "sk-secret-a3f9").unwrap();

    // 没配置过 llm.providers:两张内置卡都漏出来,DeepSeek 带兜底钥匙、Anthropic 是空模板
    let views = engine.list_providers().unwrap();
    let ids: Vec<&str> = views.iter().map(|v| v.id.as_str()).collect();
    assert_eq!(ids, ["deepseek", "anthropic"]);
    let ds = &views[0];
    assert!(ds.builtin && ds.key_set);
    assert_eq!(ds.key_masked, "····a3f9", "明文钥匙只回尾4位掩码");
    assert!(!ds.base_url.is_empty() && !ds.model.is_empty(), "预设全部预填");
    let an = &views[1];
    assert!(an.builtin && !an.key_set && an.key_masked.is_empty());

    // 给 Anthropic 贴钥匙(掩码回显的空串/None 不应动钥匙)
    let views = engine
        .save_provider(ProviderPatch {
            id: "anthropic".into(),
            name: None,
            protocol: None,
            base_url: None,
            model: None,
            enabled: None,
            api_key: Some("sk-ant-xyz9".into()),
        })
        .unwrap();
    let an = views.iter().find(|v| v.id == "anthropic").unwrap();
    assert!(an.key_set);
    assert_eq!(an.key_masked, "····xyz9");

    // 再存一次不带钥匙:钥匙保持
    let views = engine
        .save_provider(ProviderPatch {
            id: "anthropic".into(),
            name: None,
            protocol: None,
            base_url: None,
            model: Some("claude-haiku-4-5".into()),
            enabled: Some(false),
            api_key: Some("".into()),
        })
        .unwrap();
    let an = views.iter().find(|v| v.id == "anthropic").unwrap();
    assert!(an.key_set, "空钥匙入参不许冲掉已存钥匙");
    assert!(!an.enabled);
    assert_eq!(an.model, "claude-haiku-4-5");

    // 落盘的 JSON 里钥匙是真值(本机数据库),视图层才掩码
    let json = store.settings.get(None, "llm.providers").unwrap().unwrap();
    assert!(json.contains("sk-ant-xyz9"));

    // 自定义卡:补齐必填才能存;内置不可删,自定义可删
    let err = engine
        .save_provider(ProviderPatch {
            id: "relay".into(),
            name: Some("某中转".into()),
            protocol: Some("openai_compat".into()),
            base_url: None,
            model: None,
            enabled: None,
            api_key: Some("sk-r".into()),
        })
        .unwrap_err();
    assert!(err.message.contains("接入点"), "新卡缺 base_url/model 必须报错");
    engine
        .save_provider(ProviderPatch {
            id: "relay".into(),
            name: Some("某中转".into()),
            protocol: Some("openai_compat".into()),
            base_url: Some("https://relay.example.com/v1/".into()),
            model: Some("gpt-5-mini".into()),
            enabled: None,
            api_key: Some("sk-r".into()),
        })
        .unwrap();
    assert!(engine.remove_provider("deepseek").is_err(), "内置只可禁用不可删");
    let views = engine.remove_provider("relay").unwrap();
    assert!(!views.iter().any(|v| v.id == "relay"));
}

// 渠道归人 / 声纹(多用户第一步)的端到端链:speaker_user 物化进 payload 落库 →
// 装配时加〔名字说〕确定性标记(FakeLlm 回声最后一条 user 的 content,标记从外部可见)→
// touch 只动会话归属者 —— 家人在手机渠道说话不改「最近活跃」,否则主人重启会被切成 TA 的视角。
#[tokio::test(flavor = "multi_thread")]
async fn speaker_user_marks_context_and_keeps_boot_owner() {
    let (store, engine, conv_id) = setup("speaker", 1);
    let owner = store.users.ensure_default_user().unwrap();
    let kid = store.users.create("豆豆").unwrap();
    // 让 send 里的 touch 时间戳严格晚于 kid 的创建时间(同毫秒会让"最近活跃"排序不定)
    tokio::time::sleep(std::time::Duration::from_millis(3)).await;

    let meta = larkwing_core::engine::UserMeta {
        speaker_user: Some(kid.id),
        ..Default::default()
    };
    let mut rx = engine
        .send_message(conv_id, "提醒我明天带作业".into(), Some(meta), vec![])
        .await
        .unwrap();
    let mut streamed = String::new();
    while let Some(ev) = rx.recv().await {
        if let TurnEvent::Delta(t) = ev {
            streamed.push_str(&t)
        }
    }
    // ① 装配标记端到端可见:回声内容 = 带说话人标记的形态
    assert!(
        streamed.contains("〔豆豆说〕提醒我明天带作业"),
        "回声应带说话人标记: {streamed}"
    );
    // ② payload 物化(真相在库,历史回放同一字节形)
    let msgs = store.chat.recent_messages(conv_id, 10).unwrap();
    assert!(
        msgs[0]
            .payload
            .as_deref()
            .unwrap_or("")
            .contains(&format!("\"speaker_user\":{}", kid.id)),
        "user 行 payload 应物化 speaker_user: {:?}",
        msgs[0].payload
    );
    // ③ touch 会话归属者而非说话人:豆豆说完话,「最近活跃」(boot 恢复依据)仍是主人
    let kid_after = store.users.get(kid.id).unwrap().unwrap();
    assert_eq!(kid_after.last_active_at, kid.last_active_at, "说话人不该被 touch");
    let current = store.users.ensure_default_user().unwrap();
    assert_eq!(current.id, owner.id, "重启后当前用户仍是主人");
}
