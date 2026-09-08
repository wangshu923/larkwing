//! 分头办事(delegate):把一件自包含的活派给**新鲜上下文的子回合**独立跑。
//!
//! 子回合复用同一份回合循环(`assemble_request` 同源),只是 ephemeral:会话行一概
//! 不落库,汇报 = 最后一段收尾文本。常量单源在 `tools/delegate.rs`。

use super::request::{assemble_request, open_stream, tail_budget};
use super::*;

/// delegate 的 engine 侧适配器(tools::delegate::SubAgent 的实现;接缝另一半,
/// webrender 的镜像方向 —— 那个是壳实现 core 消费,这个是 engine 实现 tools 消费)。
struct EngineSubAgent {
    engine: std::sync::Weak<Engine>,
    /// 父回合的流水锚点:子回合 usage_rounds 记到父回合头上(气泡读数如实含子回合成本 §6.4)。
    user_msg_id: i64,
}

#[async_trait::async_trait]
impl crate::tools::delegate::SubAgent for EngineSubAgent {
    async fn run(&self, ctx: &crate::tools::ToolCtx, task: &str) -> anyhow::Result<String> {
        let Some(engine) = self.engine.upgrade() else {
            anyhow::bail!("引擎正在关闭,这路活没派出去");
        };
        engine.run_sub_turn(ctx, task, self.user_msg_id).await
    }
}

/// 按字符截断(子回合汇报进 report job 用;别按字节切,多字节边界会 panic —— §7.8 ①的教训)。
/// 算法在 `crate::text::clip` 单源,这里只定本站点的尾缀话术。
fn clip_chars(s: &str, max: usize) -> String {
    crate::text::clip(s, max, "…(汇报太长截了尾巴;长产出本该落成文件、汇报里给路径)")
}

impl Engine {
    /// delegate 适配器工厂(每回合现造,烤进父回合的流水锚点 user_msg_id):
    /// weak 还没回填(理论上只在构造半途)→ None,ToolCtx.agent = None → 工具如实退回。
    pub(super) fn sub_agent(&self, user_msg_id: i64) -> Option<Arc<dyn crate::tools::delegate::SubAgent>> {
        let weak = self.weak_self.get()?.clone();
        Some(Arc::new(EngineSubAgent { engine: weak, user_msg_id }))
    }

    /// HUD「停止」钮直连(按钮不绕 LLM,§7.1 嘴控/重试同哲学):拨 bgtasks 协作旗标,
    /// 与 task_cancel 工具同一个;返回任务名,None = 没在跑(已收尾/查无此号)。
    pub fn bg_cancel(&self, id: u64) -> Option<String> {
        self.media.bg().cancel(id)
    }

    /// 跑一路子回合(delegate 的 engine 半边,§6.5「分头办事」):新鲜上下文(wake_turn
    /// 同款原料、历史为空、简报物化成唯一 user 消息)+ 工具面 = 场景白名单 − 排除表 +
    /// ephemeral Turn(不落会话行/不点灯;usage 流水锚父回合)。`IN_TURN_WAIT`(30s 单源)
    /// 内跑完当场回汇报;没跑完**不作废** → 转 bgtasks 后台接着跑(〔此刻〕/进度/取消/
    /// 看门狗/收尾 report job 唤回合全白拿),当场返回「已转后台(编号 N)」。
    async fn run_sub_turn(
        &self,
        ctx: &crate::tools::ToolCtx,
        task: &str,
        user_msg_id: i64,
    ) -> anyhow::Result<String> {
        use crate::tools::delegate::{SUB_MAX_ROUNDS, SUB_PARALLEL_MAX, SUB_REPORT_MAX_CHARS};

        // 并发闸:守卫式计数(panic/取消不漏减),盖全生命周期(同步段 + 转后台段);
        // 满了如实退回不排队(bgtasks 口径;无嵌套 → 无死锁,但排队会拖住父回合,不值)。
        struct Active(Arc<std::sync::atomic::AtomicUsize>);
        impl Drop for Active {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::Relaxed);
            }
        }
        let before = self.sub_active.fetch_add(1, Ordering::Relaxed);
        let active = Active(self.sub_active.clone());
        if before >= SUB_PARALLEL_MAX {
            anyhow::bail!(
                "已有 {before} 路活在分头跑(上限 {SUB_PARALLEL_MAX}),等几路跑完再派;\
                 急的话这一步自己直接调工具干。"
            );
        }

        let candidates = self.llm.rd().clone();
        anyhow::ensure!(!candidates.is_empty(), "还没有配置 API key,派不了活");
        let budget = tail_budget(&candidates);

        // 场景/归属沿父回合会话(会话正被删的极端情形 → 默认场景,不挡活)
        let (conv_id, mem_user) = (ctx.conv_id, ctx.user_id);
        let store = self.store.clone();
        let conv = tokio::task::spawn_blocking(move || store.chat.get_conversation(conv_id))
            .await
            .map_err(anyhow::Error::from)??;
        let (scene_id, conv_user) =
            conv.map(|c| (c.scene_id, c.user_id)).unwrap_or_else(|| (DEFAULT_SCENE_ID.into(), mem_user));
        let scene =
            self.scenes.get(&scene_id).unwrap_or_else(|| self.scenes.default_scene()).clone();

        // 工具面 = 场景白名单 − 排除表(单源 delegate::allowed_in_sub;排除表含 delegate
        // 自身 = 深度 1 的白名单半边,ToolCtx.agent=None 是另一半)
        let mut sub_tools = self.tools.subset(&scene.tools);
        sub_tools.retain(|t| crate::tools::delegate::allowed_in_sub(t.spec().name));
        let tool_defs: Vec<ToolDef> = sub_tools.iter().map(|t| t.spec().def()).collect();

        // 新鲜请求:与聊天回合同一套原料装配(assemble_request 单源),历史为空;
        // 稳定前缀模板与聊天回合一致,工具面不同 → 子回合有自己的一条前缀缓存线(层内稳定)。
        let store = self.store.clone();
        let scene_owned = scene.clone();
        let task_owned = task.to_string();
        let mut request =
            tokio::task::spawn_blocking(move || -> anyhow::Result<crate::llm::ChatRequest> {
                let mut request = assemble_request(
                    &store,
                    &scene_owned,
                    conv_id,
                    conv_user,
                    mem_user,
                    &[],
                    0,
                    budget,
                    &tool_defs,
                )?;
                // thinking 档 assemble_request 尾部已设过(评审抓的重复读),这里不再重复
                request
                    .messages
                    .push(crate::llm::ChatMessage::user(context::delegate_injection(&task_owned)));
                Ok(request)
            })
            .await
            .map_err(anyhow::Error::from)??;

        context::cap_messages_tail(&mut request.messages, budget); // 简报有 backstop,这里纯防御
        let (rx_llm, provider_id, provider, first_round_start) =
            match open_stream(&candidates, &request).await {
                Ok(quad) => quad,
                Err(e) => {
                    let app = AppError::from(e);
                    anyhow::bail!("这路活没派出去(连不上大脑):{}", app.message);
                }
            };

        let model =
            request.options.model.clone().unwrap_or_else(|| provider.model_id().to_string());
        let (tx, mut rx) = mpsc::channel::<TurnEvent>(64);
        let sub_token = CancellationToken::new();
        // 父回合取消 / 工具超时 → 本 future 被 drop → guard 顺手掐灭子回合(转后台时解除武装)
        let cancel_guard = sub_token.clone().drop_guard();

        tokio::spawn(
            turn::Turn {
                store: self.store.clone(),
                conv_id,
                user_id: mem_user,
                token: sub_token.clone(),
                tx,
                provider,
                provider_id,
                model,
                user_msg_id,
                first_round_start,
                request,
                tools: sub_tools,
                media: self.media.clone(),
                web: self.web_renderer(),
                voice: self.voice(),
                confirm: Some(self.confirmer.clone()),
                rx: rx_llm,
                inject: Arc::new(Mutex::new(InjectState::default())),
                plan: Arc::new(Mutex::new(Default::default())),
                overheard: None,
                ephemeral: true,
                max_rounds: SUB_MAX_ROUNDS,
                grants: ctx.grants.clone(), // 共享父回合授权缓存(「仅这次」含派生的子回合)
                agent: None,                // 深度 1 双锁的另一半
            }
            .run(),
        );

        // HUD 任务卡(kind=delegate):步进 = 子回合正在调的工具动词(ui_key 直接进前端字典)
        let brief: String = task.chars().take(40).collect();
        let handle = self.media.tasks().start(
            "delegate",
            crate::bus::Text::with("task.delegate", serde_json::json!({ "t": brief })),
        );

        // —— 同步段:IN_TURN_WAIT 内跑完当场回汇报(读类结果要回到推理手里才接得上) ——
        // 汇报 = 最后一段收尾文本:每见 ToolUse 即清缓冲(之前攒的是过程叙述,不是汇报)。
        let mut digest = String::new();
        let mut units = 0usize;
        let deadline = tokio::time::Instant::now() + crate::media::IN_TURN_WAIT;
        loop {
            let ev = match tokio::time::timeout_at(deadline, rx.recv()).await {
                Err(_) => break, // 等不完 → 转后台接着跑(不作废)
                Ok(ev) => ev,
            };
            match ev {
                Some(TurnEvent::Delta(t)) => digest.push_str(&t),
                Some(TurnEvent::ToolUse { state: ToolUseState::Started, label, .. }) => {
                    digest.clear();
                    units += 1;
                    handle.step(&label, serde_json::Value::Null);
                }
                Some(TurnEvent::Done { .. }) => {
                    let report = digest.trim().to_string();
                    // 空汇报按失败收卡(评审:原先先 done 再 bail = HUD 绿卡 + 工具却报错,
                    // 与转后台段同情形的 fail 口径不一致)
                    if report.is_empty() {
                        handle.fail("task.err.delegate", serde_json::Value::Null);
                        anyhow::bail!(
                            "这路活跑完了但没带回汇报;把「要回报什么」写清楚再派一次,或自己直接查。"
                        );
                    }
                    handle.done();
                    return Ok(report);
                }
                Some(TurnEvent::Failed { message, .. }) => {
                    handle.fail("task.err.delegate", serde_json::Value::Null);
                    anyhow::bail!("这路活没办成:{message}");
                }
                Some(TurnEvent::Cancelled) => {
                    handle.fail("task.err.cancelled", serde_json::Value::Null);
                    anyhow::bail!("这路活被取消了");
                }
                Some(_) => {} // Thinking / Usage 等,与汇报无关
                None => {
                    handle.fail("task.err.delegate", serde_json::Value::Null);
                    anyhow::bail!("子回合的事件流断了(内部错误)");
                }
            }
        }

        // —— 转后台:注册 bgtasks(〔此刻〕/task_status/取消/看门狗/收尾唤回合全白拿) ——
        let ticket =
            match self.media.bg().submit(format!("分头办事:{brief}"), (mem_user, conv_id), 0) {
                Ok(t) => t,
                Err(e) => {
                    // 后台满:掐掉这路(guard drop 即取消子回合),如实退回
                    handle.fail("task.err.delegate", serde_json::Value::Null);
                    anyhow::bail!(
                        "{e:#};这路活半分钟没跑完、想转后台接着跑但后台满了,这次先停了,\
                         等几件后台事跑完再派。"
                    );
                }
            };
        let bg_id = ticket.id();
        handle.bind_bg(bg_id);
        ticket.beat(units, "接着干");
        let _ = cancel_guard.disarm(); // 父回合收尾/取消不再级联;停这路 = task_cancel / HUD 停止钮
        let token_bg = sub_token.clone();
        let brief_bg = brief.clone();
        let join = tokio::spawn(async move {
            let _active = active; // 并发计数盖到后台段结束
            // 本任务被看门狗 abort / panic → 子回合跟着停(BgTicket 的 Drop 汇报「半路断」)
            let _kill = token_bg.clone().drop_guard();
            let mut digest = digest;
            let mut units = units;
            loop {
                tokio::select! {
                    ev = rx.recv() => match ev {
                        Some(TurnEvent::Delta(t)) => digest.push_str(&t),
                        Some(TurnEvent::ToolUse { state: ToolUseState::Started, label, name, .. }) => {
                            digest.clear();
                            units += 1;
                            ticket.beat(units, format!("调用 {name}"));
                            handle.step(&label, serde_json::Value::Null);
                        }
                        Some(TurnEvent::Done { .. }) => {
                            let report = digest.trim();
                            if report.is_empty() {
                                handle.fail("task.err.delegate", serde_json::Value::Null);
                                ticket.finish(false, format!(
                                    "分头办的活「{brief_bg}」跑完了但没带回汇报;要不要换个问法重派,听用户的。"
                                ));
                            } else {
                                handle.done();
                                ticket.finish(true, format!(
                                    "分头办的活「{brief_bg}」办完了,汇报如下(向用户转述要点;该接着干的接着干):\n{}",
                                    clip_chars(report, SUB_REPORT_MAX_CHARS)
                                ));
                            }
                            break;
                        }
                        Some(TurnEvent::Failed { message, .. }) => {
                            handle.fail("task.err.delegate", serde_json::Value::Null);
                            ticket.finish(false, format!(
                                "分头办的活「{brief_bg}」没办成:{message}。如实告诉用户。"
                            ));
                            break;
                        }
                        Some(TurnEvent::Cancelled) => {
                            handle.fail("task.err.cancelled", serde_json::Value::Null);
                            ticket.finish(false, format!("分头办的活「{brief_bg}」按要求停下了。"));
                            break;
                        }
                        Some(_) => {}
                        None => {
                            handle.fail("task.err.delegate", serde_json::Value::Null);
                            ticket.finish(false, format!(
                                "分头办的活「{brief_bg}」半路断了(事件流没了)。如实告诉用户。"
                            ));
                            break;
                        }
                    },
                    _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {
                        if ticket.is_cancelled() {
                            token_bg.cancel(); // 协作式:子回合走 Cancelled 收尾,上面接住
                        }
                    }
                }
            }
        });
        self.media.bg().attach_abort(bg_id, join.abort_handle());
        Ok(format!(
            "这路活半分钟内没跑完,已转后台接着跑(编号 {bg_id}),跑完会自动回来汇报 —— \
             先接着干别的;要中途停下用 task_cancel。"
        ))
    }
}
