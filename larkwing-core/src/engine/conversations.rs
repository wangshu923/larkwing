//! 会话:增删改查 / 回溯与分叉 / 统计与「想了想」轨迹。
//!
//! 会话生命周期是 core 一等公民、永不委托插件层(§6.4);回合管线对「会话从哪来」
//! 零感知,只要 conv_id + ChatRepo 契约。副作用不回滚 —— 世界已经发生。

use super::*;

/// 「想了想」轨迹的一步(PLAN §9 思考漏出·展开层):一次工具调用的技术细节。
/// ui_key 给折叠摘要兜底;name/args/result/status 是展开后给好奇/专业用户看的真东西。
#[derive(Debug, Clone, Serialize)]
pub struct TraceStep {
    pub name: String,
    pub ui_key: String,
    pub args: String,
    pub result: String,
    pub status: String,
}

/// 「想了想」里的一条。**严格按时间顺序**排成一条队列(思考→工具→工具→思考→…),
/// 不是「工具一堆 + CoT 一坨」两个桶 —— 桶会把真实次序抹掉(2026-08-15 用户:
/// 「思考块在最下面、上面全是工具,很奇怪」)。一条 assistant 行 = 一轮,轮内次序恒为
/// 「先想(reasoning)→ 再调工具(tool_calls)」,照这个次序进队列即得真实时间线。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TraceItem {
    /// 说这段话之前的一段思考(CoT 原文)。
    Thinking { text: String },
    /// 一次工具调用(结果由随后的 tool 行按 call_id 回填)。
    Tool(TraceStep),
}

impl TraceItem {
    /// 是工具步骤就借出来(折叠层「N 步」只数工具;回执小票、建议气泡也只看工具)。
    pub fn as_tool(&self) -> Option<&TraceStep> {
        match self {
            TraceItem::Tool(s) => Some(s),
            TraceItem::Thinking { .. } => None,
        }
    }
}

/// 一回合的「想了想」轨迹:贴在该回合代表气泡上。折叠药丸只露「想了想 · N 步」(§3 干净默认);
/// 展开 = 时间序条目列表(每条再点开才露工具名/入参/结果、或 CoT 原文 —— 用户拍板:
/// 展开是给好奇/专业用户的技术披露,非专业用户不必点开;§3 铁律2 在折叠层守住)。
#[derive(Debug, Clone, Serialize)]
pub struct TurnTrace {
    pub message_id: i64,
    pub items: Vec<TraceItem>,
}

impl Engine {
    pub fn new_conversation(&self, channel: &str) -> Result<Conversation, AppError> {
        let user = self.store.users.ensure_default_user()?;
        Ok(self.store.chat.create_conversation_full(user.id, DEFAULT_SCENE_ID, channel)?)
    }

    pub fn list_conversations(&self) -> Result<Vec<Conversation>, AppError> {
        let me = self.store.users.ensure_default_user()?;
        let mut list = self.store.chat.list_conversations(me.id)?;
        // 发起人显名(说话人显性化 §):渠道指认的家人 / 非主人发起者才标;主人自己的会话不标
        // (是「你」)。系统会话(channel=system)靠 channel 字段前端显「系统」,不占 owner_name。
        //
        // 归人映射一次取全(原先每条会话一次 `conversation_owner` = thread_by_conv + get_conversation,
        // 再可能一次 users.get,侧栏每刷一次就 2–3×N 条查询;`Db::with` 是单连接互斥锁,等于跟
        // 同时刻的回合落库抢锁。§ 效率审计 2026-09-08)。口径不变:指认过的算指认的家人,
        // 否则回落会话 `user_id` —— 而这批会话本就是按 `me.id` 查出来的,回落值恒等于 me、不标名,
        // 所以只有「指认给了别人」才需要名字,连 users 表都不用碰。
        let assigned = self.store.channels.assigned_users_by_conv()?;
        let names: HashMap<i64, String> = if assigned.values().any(|uid| *uid != me.id) {
            self.store.users.list()?.into_iter().map(|u| (u.id, u.name)).collect()
        } else {
            HashMap::new()
        };
        for c in list.iter_mut() {
            let owner = assigned.get(&c.id).copied().unwrap_or(c.user_id);
            if owner != me.id {
                c.owner_name = names.get(&owner).cloned();
            }
        }
        Ok(list)
    }

    /// 载一页消息(默认最新一页)。`cursor` 见 `store::chat::MsgCursor`:向上翻页给 `before`、
    /// 搜索命中定位给 `around`。**三个读命令共用同一游标**,所以同一页的读数与轨迹也对得上。
    ///
    /// ⚠️ 分页的固有边界:`mark_triggered` 与轨迹的 tool_call↔result 配对都靠「整段消息序列」
    /// 推,页首若把 event→assistant 或 call→result 劈开,那一处标记 / 配对会缺(只影响显示,
    /// 不影响数据)。首屏那页永远完整,只有翻上去的页边界可能撞到。
    pub fn load_conversation(
        &self,
        conv_id: i64,
        cursor: crate::store::chat::MsgCursor,
    ) -> Result<Vec<Message>, AppError> {
        // 从消息 payload(UserMeta JSON)解出声纹 / 渠道归人写入的说话人 id(共用同一字段)。
        fn parse_speaker_user(payload: &str) -> Option<i64> {
            serde_json::from_str::<UserMeta>(payload).ok().and_then(|m| m.speaker_user)
        }
        // 标「自动触发」:event 行(自启回合的任务语境)之后、下一条 user 行之前的 assistant
        // 回复 = 提醒/定时任务到点触发,标 trigger 让前端显「⏰ 提醒」。
        fn mark_triggered(msgs: &mut [Message]) {
            let mut triggered = false;
            for m in msgs.iter_mut() {
                match m.role.as_str() {
                    "event" => triggered = true,
                    "user" => triggered = false,
                    "assistant" if triggered => m.trigger = Some("reminder".into()),
                    _ => {}
                }
            }
        }
        let mut msgs = self.store.chat.page(conv_id, cursor)?;
        // 「谁说的」显名:user 行说话人若非会话归属者(家人插话 / 声纹 / 渠道归人)→ 填名字;
        // 归属者自己说的不标(是「我」)。声纹与渠道共用 payload.speaker_user,一套覆盖两者。
        //
        // 名字一次取全家:原先逐行 `users.get(sid)`,一个渠道会话最多 200 次查询(§ 效率审计
        // 2026-09-08)。payload 仍只解析一遍(先收要显名的行,再决定要不要碰 users 表);
        // 家人被删 → 查不到名字 = None,与原先 `users.get` 返回 None 同义。
        let owner = self.conversation_owner(conv_id)?;
        let mut pending: Vec<(usize, i64)> = Vec::new();
        for (i, m) in msgs.iter().enumerate() {
            if m.role != "user" {
                continue;
            }
            if let Some(sid) = m.payload.as_deref().and_then(parse_speaker_user) {
                if Some(sid) != owner {
                    pending.push((i, sid));
                }
            }
        }
        if !pending.is_empty() {
            let names: HashMap<i64, String> =
                self.store.users.list()?.into_iter().map(|u| (u.id, u.name)).collect();
            for (i, sid) in pending {
                msgs[i].speaker_name = names.get(&sid).cloned();
            }
        }
        mark_triggered(&mut msgs);
        Ok(msgs)
    }

    /// 会话的「归属者」= 这个会话里的「我」:渠道会话 = 指认的家人(若指认),否则会话 user_id
    /// (桌面主人)。用于说话人显性化:归属者说的不标名、非归属者才标。
    fn conversation_owner(&self, conv_id: i64) -> Result<Option<i64>, AppError> {
        if let Some(t) = self.store.channels.thread_by_conv(conv_id)? {
            if t.user_id.is_some() {
                return Ok(t.user_id);
            }
        }
        Ok(self.store.chat.get_conversation(conv_id)?.map(|c| c.user_id))
    }

    /// 跨会话搜索当前用户的聊天记录(排除工具 / 系统事件内部行)。最近命中在前。
    pub fn search_messages(&self, query: &str, limit: i64) -> Result<Vec<SearchHit>, AppError> {
        let user = self.store.users.ensure_default_user()?;
        Ok(self.store.chat.search_messages(user.id, query, limit)?)
    }

    /// 先取消在飞 → 级联删消息 → 清会话槽。
    pub async fn delete_conversation(&self, conv_id: i64) -> Result<(), AppError> {
        self.cancel(conv_id).await;
        self.store.chat.delete_conversation(conv_id)?;
        let removed = self.sessions.lk().remove(&conv_id);
        // 会话没了,HUD/悬浮窗上的计划卡跟着收(空快照 = 收卡信号,§6.5)
        if let Some(slot) = removed {
            if !slot.plan.lk().is_empty() {
                self.bus.publish(crate::bus::AppEvent::Plan(crate::bus::PlanCard {
                    conv_id,
                    title: None,
                    items: vec![],
                }));
            }
        }
        Ok(())
    }

    /// 会话回溯「从这里重新说」:先取消在飞(await 收尾,partial 落库后一并截掉)→
    /// 删掉该**用户消息**(含)之后的所有行。副作用不回滚——那几轮设的提醒/记的记忆/
    /// 动过的文件留着,各自页面可管:聊天流可以改写,世界已经发生(与「partial 落普通
    /// 消息(像人被打断)」同一哲学)。会话槽不清(会话还活着,瞬态丢了也只是重算)。
    pub async fn rollback_conversation(&self, conv_id: i64, msg_id: i64) -> Result<(), AppError> {
        self.cancel(conv_id).await;
        self.store.chat.truncate_from(conv_id, msg_id)?;
        Ok(())
    }

    /// 会话分叉「从这里另起新会话」:切点之前的历史复制进新会话(ui 渠道、空标题),
    /// 原会话一字不动——回溯的无损姊妹。不取消在飞:读的是切点之前的稳定前缀,
    /// 原会话的回合继续跑它的。
    pub fn fork_conversation(&self, conv_id: i64, msg_id: i64) -> Result<Conversation, AppError> {
        Ok(self.store.chat.fork_before(conv_id, msg_id)?)
    }

    /// 用户右键重命名会话(无条件覆盖标题;空串交给前端拦,这里只落库)。
    pub fn rename_conversation(&self, conv_id: i64, title: &str) -> Result<(), AppError> {
        Ok(self.store.chat.set_title(conv_id, title)?)
    }

    /// 钉住 / 取消钉住会话(列表排最前 + 📌)。
    pub fn set_conversation_pinned(&self, conv_id: i64, pinned: bool) -> Result<(), AppError> {
        Ok(self.store.chat.set_pinned(conv_id, pinned)?)
    }

    /// 今日用量快照(灯带初值;之后的增量由 TurnEvent::Usage 推送)。
    pub fn usage_today(&self) -> DayUsage {
        usage::usage_today(&self.store)
    }

    /// 会话累计快照(灯带"话题"段初值:开机/切话题时取;之后随 TurnEvent::Usage 推送)。
    pub fn usage_conversation(&self, conv_id: i64) -> crate::store::UsageTotals {
        usage::usage_conversation(&self.store, conv_id)
    }

    /// 历史/提醒气泡的 hover 读数(PLAN §11 D):把库里每回合的用量映射到对应的
    /// assistant 气泡 id —— 前端 load 会话后回填,让自启回合/历史消息也能 hover 看读数
    /// (在飞回合仍由 TurnEvent::Usage 实时常显,不走这条)。
    pub fn conversation_stats(
        &self,
        conv_id: i64,
        cursor: crate::store::chat::MsgCursor,
    ) -> Result<Vec<MsgStats>, AppError> {
        let rollups = self.store.usage.rounds_by_turn(conv_id)?;
        if rollups.is_empty() {
            return Ok(vec![]);
        }
        // 回合锚点(user/event 行 id)→ 该回合"代表气泡"= 其后最后一条有内容的 assistant
        // (跨过中途的纯 tool_call 空 assistant 行;event 行是自启回合锚点,与 round.user_msg_id 对齐)
        let msgs = self.store.chat.page(conv_id, cursor)?;
        let mut key_to_assistant: HashMap<i64, i64> = HashMap::new();
        let mut cur: Option<i64> = None;
        for m in &msgs {
            match m.role.as_str() {
                "user" | "event" => cur = Some(m.id),
                "assistant" if !m.content.trim().is_empty() => {
                    if let Some(k) = cur {
                        key_to_assistant.insert(k, m.id);
                    }
                }
                _ => {}
            }
        }
        Ok(rollups
            .into_iter()
            .filter_map(|r| {
                key_to_assistant.get(&r.user_msg_id).map(|&aid| MsgStats {
                    message_id: aid,
                    ms: r.elapsed_ms,
                    input_tokens: r.input_tokens,
                    output_tokens: r.output_tokens,
                    cache_hit_tokens: r.cache_hit_tokens,
                    cost_usd: r.cost_usd,
                })
            })
            .collect())
    }

    /// 历史回放的「想了想」轨迹:把每回合中途的工具调用(名/入参/结果/状态)+ CoT 原文,
    /// 归到该回合的代表回复气泡(有可见文字的 assistant 行)。全从落库 payload 重建。
    /// live 回合落库后由前端补拉这条(不在 TurnEvent 里塞 args/result,免破流式词汇)。
    ///
    /// 锚定与 live 对齐(turn.rs 的 Segment 事件在 ToolUse 之前发):一条**有可见文字**的
    /// assistant 行封口当前段,收下「上一锚以来声明的工具 + 本轮在封口前流出的 CoT」;**本行
    /// 自己声明的工具归下一段**(live 里 ToolUse 在封口之后才到、落到新气泡)。一回合的尾部
    /// 工具(最后一条可见回复之后才声明、整轮再无可见回复)折进该回合最后一个可见气泡。
    /// **整轮零可见回复**(模型调了工具却一句没说)整段不产出 —— 没有气泡可挂(前端 `visible`
    /// 本就滤掉空 assistant 行),用户拍板静默回合不补独立药丸(§3 干净默认)。
    ///
    /// 三处比旧实现修对:① 结算落在**回合边界 / 循环末尾**,尾部工具不再随下一条 user 行清空被
    /// 丢(「调了工具但收尾无可见文字」整段丢轨迹的根因);② `idx_by_call` 整段不清 →
    /// tool 结果行(排在声明它的 assistant 行之后)一定回填得上(旧实现遇「同轮先说话再调工具」
    /// 当场结算复位、结果丢失);③ 同轮文字 + 工具时工具归下一气泡,与 live 封口顺序一致。
    pub fn conversation_trace(
        &self,
        conv_id: i64,
        cursor: crate::store::chat::MsgCursor,
    ) -> Result<Vec<TurnTrace>, AppError> {
        // 收一回合:尾部未封口的条目折进最后一个可见气泡(有锚才折,无锚 = 静默回合,丢弃),
        // 再把各段非空者落成 TurnTrace。idx 一并清,跨回合不串。
        fn flush_turn(
            out: &mut Vec<TurnTrace>,
            segments: &mut Vec<(i64, Vec<TraceItem>)>,
            buf: &mut Vec<TraceItem>,
            idx: &mut HashMap<String, usize>,
        ) {
            if !buf.is_empty() {
                if let Some(last) = segments.last_mut() {
                    last.1.append(buf);
                }
            }
            for (anchor, items) in segments.drain(..) {
                if !items.is_empty() {
                    out.push(TurnTrace { message_id: anchor, items });
                }
            }
            buf.clear();
            idx.clear();
        }

        let msgs = self.store.chat.page(conv_id, cursor)?;
        let mut out = Vec::new();
        // 当前(未封口)段:buf 整段不清,tool 结果行排在声明它的 assistant 行之后才到 —— 提前清
        // 就回填不上。封口才把 buf 转入 segments。**一条队列按到达顺序装**,思考与工具天然交错。
        let mut buf: Vec<TraceItem> = Vec::new();
        let mut idx_by_call: HashMap<String, usize> = HashMap::new();
        // 本回合已封口的段:(锚气泡 id, 该段条目)。回合边界一次性落成 TurnTrace。
        let mut segments: Vec<(i64, Vec<TraceItem>)> = Vec::new();

        for m in &msgs {
            match m.role.as_str() {
                "user" | "event" => {
                    flush_turn(&mut out, &mut segments, &mut buf, &mut idx_by_call)
                }
                "assistant" => {
                    let payload = m
                        .payload
                        .as_deref()
                        .and_then(|p| serde_json::from_str::<AssistantPayload>(p).ok());
                    // 本轮 CoT 先入队(对齐 live:思考在说话/调工具之前流出)。
                    if let Some(r) = payload
                        .as_ref()
                        .and_then(|p| p.reasoning.as_deref())
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                    {
                        buf.push(TraceItem::Thinking { text: r.to_string() });
                    }
                    // 有可见文字 = 封口当前段到这条回复气泡(本轮工具尚未入 buf → 归下一段)。
                    if !m.content.trim().is_empty() {
                        segments.push((m.id, std::mem::take(&mut buf)));
                        idx_by_call.clear();
                    }
                    // 本轮声明的工具入(封口后可能已是新的)当前段。
                    if let Some(p) = &payload {
                        for c in &p.tool_calls {
                            let ui_key = self
                                .tools
                                .get(&c.name)
                                .map(|t| t.spec().ui_key.to_string())
                                .unwrap_or_else(|| "tool.unknown".into());
                            idx_by_call.insert(c.id.clone(), buf.len());
                            buf.push(TraceItem::Tool(TraceStep {
                                name: c.name.clone(),
                                ui_key,
                                args: c.args.to_string(),
                                result: String::new(),
                                status: String::new(),
                            }));
                        }
                    }
                }
                // tool 行:按 call_id 回填结果/状态到当前段对应步骤。
                "tool" => {
                    if let Some(tp) = m
                        .payload
                        .as_deref()
                        .and_then(|p| serde_json::from_str::<ToolRowPayload>(p).ok())
                    {
                        if let Some(TraceItem::Tool(step)) =
                            idx_by_call.get(&tp.call_id).and_then(|&i| buf.get_mut(i))
                        {
                            step.result = m.content.clone();
                            step.status = tp.status;
                        }
                    }
                }
                _ => {}
            }
        }
        // 循环末尾再结算一回合(最后一回合没有后续 user 行触发边界)。
        flush_turn(&mut out, &mut segments, &mut buf, &mut idx_by_call);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::testkit::engine;

    /// 说话人显性化(§):load_conversation 富化 —— 非归属者说话人标名(声纹 / 渠道共用 speaker_user),
    /// 归属者(主人)自己说的不标;event 行之后的 assistant 标 reminder,普通对话回复不标。
    #[test]
    fn load_conversation_enriches_speaker_and_trigger() {
        let eng = engine("load-enrich");
        let me = eng.store.users.ensure_default_user().unwrap();
        let fam = eng.store.users.create("小明").unwrap();
        let conv = eng.store.chat.create_conversation(me.id, DEFAULT_SCENE_ID).unwrap();
        // 主人自己打字(无 payload)→ 不标名(是「我」)
        eng.store.chat.append_message(conv.id, "user", "主人说的").unwrap();
        // 家人插话(payload.speaker_user = 家人)→ 标家人名
        let meta = format!(r#"{{"speaker_user":{}}}"#, fam.id);
        eng.store.chat.append_message_full(conv.id, "user", "家人说的", Some(&meta)).unwrap();
        // 提醒到点:event 行 + assistant 转述 → assistant 标 reminder
        eng.store.chat.append_message(conv.id, "event", "【定时任务到点】喝水").unwrap();
        eng.store.chat.append_message(conv.id, "assistant", "该喝水啦").unwrap();
        // 普通对话:user 清触发标记 → 后续 assistant 不标
        eng.store.chat.append_message(conv.id, "user", "在吗").unwrap();
        eng.store.chat.append_message(conv.id, "assistant", "在的").unwrap();

        let msgs = eng.load_conversation(conv.id, Default::default()).unwrap();
        let find = |c: &str| msgs.iter().find(|m| m.content == c).unwrap().clone();
        assert_eq!(find("主人说的").speaker_name, None, "归属者(主人)自己说的不标名");
        assert_eq!(find("家人说的").speaker_name.as_deref(), Some("小明"), "家人插话标名");
        assert_eq!(find("该喝水啦").trigger.as_deref(), Some("reminder"), "event 后 assistant 标自动触发");
        assert_eq!(find("在的").trigger, None, "普通对话回复不标触发");
    }

    /// 会话列表的发起人显名(说话人显性化 §7.7):渠道指认给**别人**的会话才标名字;
    /// 主人自己的(桌面 / 指认给主人 / 未指认)一律不标;家人被删 → 标不出名字就 None。
    /// (2026-09-08 效率审计把逐会话 `conversation_owner` + `users.get` 换成两条批量查询,
    /// 这条测试钉住换法前后口径一致。)
    #[test]
    fn list_conversations_labels_only_other_peoples_conversations() {
        let eng = engine("list-owner");
        let me = eng.store.users.ensure_default_user().unwrap();
        let fam = eng.store.users.create("小明").unwrap();
        let gone = eng.store.users.create("走了的").unwrap();

        let desktop = eng.store.chat.create_conversation(me.id, DEFAULT_SCENE_ID).unwrap();
        // 渠道会话三种指认:家人 / 主人自己 / 未指认
        let famc = eng.store.chat.create_conversation(me.id, DEFAULT_SCENE_ID).unwrap();
        eng.store.channels.bind("telegram", "fam", famc.id).unwrap();
        let t = eng.store.channels.thread_for("telegram", "fam").unwrap().unwrap();
        eng.store.channels.bind_user(t.id, Some(fam.id)).unwrap();

        let minec = eng.store.chat.create_conversation(me.id, DEFAULT_SCENE_ID).unwrap();
        eng.store.channels.bind("telegram", "mine", minec.id).unwrap();
        let t = eng.store.channels.thread_for("telegram", "mine").unwrap().unwrap();
        eng.store.channels.bind_user(t.id, Some(me.id)).unwrap();

        let anon = eng.store.chat.create_conversation(me.id, DEFAULT_SCENE_ID).unwrap();
        eng.store.channels.bind("telegram", "anon", anon.id).unwrap();

        // 指认给已删除的家人:名字查不到 = None(不该 panic、也不该借别人的名)
        let ghostc = eng.store.chat.create_conversation(me.id, DEFAULT_SCENE_ID).unwrap();
        eng.store.channels.bind("telegram", "ghost", ghostc.id).unwrap();
        let t = eng.store.channels.thread_for("telegram", "ghost").unwrap().unwrap();
        eng.store.channels.bind_user(t.id, Some(gone.id)).unwrap();
        eng.store.users.delete(gone.id).unwrap();

        let list = eng.list_conversations().unwrap();
        let name_of = |id: i64| {
            list.iter().find(|c| c.id == id).expect("会话应在列表里").owner_name.clone()
        };
        assert_eq!(name_of(famc.id).as_deref(), Some("小明"), "指认给家人 → 标家人名");
        assert_eq!(name_of(desktop.id), None, "桌面会话不标");
        assert_eq!(name_of(minec.id), None, "指认给主人自己不标(是「你」)");
        assert_eq!(name_of(anon.id), None, "未指认 → 回落会话归属者 = 主人,不标");
        assert_eq!(name_of(ghostc.id), None, "指认的家人已删 → 名字取不到 = None");
    }

    fn conv_with(eng: &Engine) -> i64 {
        let user = eng.store.users.ensure_default_user().unwrap();
        eng.store.chat.create_conversation(user.id, "companion").unwrap().id
    }

    /// 造一条工具轮 assistant 行 payload：calls = (call_id, 工具名)。
    fn asst(calls: &[(&str, &str)], reasoning: Option<&str>) -> String {
        let tool_calls = calls
            .iter()
            .map(|(id, name)| crate::llm::ToolCall {
                id: (*id).into(),
                name: (*name).into(),
                args: serde_json::json!({}),
                is_incomplete: false,
            })
            .collect();
        serde_json::to_string(&AssistantPayload {
            tool_calls,
            reasoning: reasoning.map(Into::into),
            ..Default::default()
        })
        .unwrap()
    }

    /// 药丸条目 → 可读速写:工具 = 工具名,思考 = `想:原文`。断言时间顺序用。
    fn sketch(tr: &TurnTrace) -> Vec<String> {
        tr.items
            .iter()
            .map(|it| match it {
                TraceItem::Tool(s) => s.name.clone(),
                TraceItem::Thinking { text } => format!("想:{text}"),
            })
            .collect()
    }

    /// 只取工具步骤(折叠层「N 步」/回执小票的口径)。
    fn tools(tr: &TurnTrace) -> Vec<&TraceStep> {
        tr.items.iter().filter_map(TraceItem::as_tool).collect()
    }

    /// 造一条 tool 结果行 payload。
    fn toolp(call_id: &str, name: &str, status: &str) -> String {
        serde_json::to_string(&ToolRowPayload {
            call_id: call_id.into(),
            name: name.into(),
            status: status.into(),
            attachments: Vec::new(),
        })
        .unwrap()
    }

    /// 常见形：先静默调几轮工具、最后才说话 → 整回合一个完整药丸，挂在可见回复上，
    /// 工具结果都回填到位（旧实现「同轮先说话」会丢结果，这里顺带验结果回填）。
    #[test]
    fn trace_silent_tool_rounds_then_final_reply_is_one_complete_pill() {
        let eng = engine("trace-common");
        let c = conv_with(&eng);
        let ch = &eng.store.chat;
        ch.append_message(c, "user", "放首歌").unwrap();
        ch.append_message_full(c, "assistant", "", Some(&asst(&[("c1", "media_search")], Some("想想放什么")))).unwrap();
        ch.append_message_full(c, "tool", "找到了《星海漫游》", Some(&toolp("c1", "media_search", "ok"))).unwrap();
        ch.append_message_full(c, "assistant", "", Some(&asst(&[("c2", "media_play")], None))).unwrap();
        ch.append_message_full(c, "tool", "开始播放", Some(&toolp("c2", "media_play", "ok"))).unwrap();
        let final_id = ch.append_message(c, "assistant", "正在为你播放").unwrap().id;

        let out = eng.conversation_trace(c, Default::default()).unwrap();
        assert_eq!(out.len(), 1, "整回合只一个药丸;得到 {out:?}");
        let tr = &out[0];
        assert_eq!(tr.message_id, final_id, "锚在最后那条可见回复");
        // 时间顺序:想 → 搜 → 放(CoT 不再被堆到最后)
        assert_eq!(sketch(tr), ["想:想想放什么", "media_search", "media_play"], "严格按时间排");
        let ts = tools(tr);
        assert_eq!(ts[0].result, "找到了《星海漫游》", "结果回填得上(旧实现这里会空)");
        assert_eq!(ts[0].status, "ok");
        assert_eq!(ts[1].result, "开始播放");
    }

    /// 本改动的核心:多轮「想→做→想→做」必须原样交错,不能被归成两堆(2026-08-15)。
    #[test]
    fn trace_items_keep_thinking_and_tools_interleaved_in_time_order() {
        let eng = engine("trace-order");
        let c = conv_with(&eng);
        let ch = &eng.store.chat;
        ch.append_message(c, "user", "把这批歌配上歌词").unwrap();
        // 轮1:想 → 两个工具
        ch.append_message_full(c, "assistant", "", Some(&asst(&[("c1", "fs_list"), ("c2", "fs_find")], Some("先看看有哪些")))).unwrap();
        ch.append_message_full(c, "tool", "12 个文件", Some(&toolp("c1", "fs_list", "ok"))).unwrap();
        ch.append_message_full(c, "tool", "找到 3 个", Some(&toolp("c2", "fs_find", "ok"))).unwrap();
        // 轮2:再想 → 再一个工具
        ch.append_message_full(c, "assistant", "", Some(&asst(&[("c3", "lyrics_fetch")], Some("这批可以直接配")))).unwrap();
        ch.append_message_full(c, "tool", "配好 3 个", Some(&toolp("c3", "lyrics_fetch", "ok"))).unwrap();
        // 收尾轮:说话之前还想了一下
        let fin = ch.append_message_full(c, "assistant", "配好啦", Some(&asst(&[], Some("汇报一下")))).unwrap().id;

        let out = eng.conversation_trace(c, Default::default()).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].message_id, fin);
        assert_eq!(
            sketch(&out[0]),
            [
                "想:先看看有哪些",
                "fs_list",
                "fs_find",
                "想:这批可以直接配",
                "lyrics_fetch",
                "想:汇报一下",
            ],
            "想/做严格交错,与真实时间线一致"
        );
        assert_eq!(tools(&out[0]).len(), 3, "折叠层「N 步」只数工具");
    }

    /// 报告的 bug：调了工具但整轮一句话没说（DeepSeek 真会这样）。Option A：无可见气泡可挂，
    /// 不补独立药丸 → 产出为空。关键是不再「连前面的也一起丢」/不 panic。
    #[test]
    fn trace_fully_silent_turn_yields_no_pill() {
        let eng = engine("trace-silent");
        let c = conv_with(&eng);
        let ch = &eng.store.chat;
        ch.append_message(c, "user", "记一下我对花生过敏").unwrap();
        ch.append_message_full(c, "assistant", "", Some(&asst(&[("c1", "remember")], None))).unwrap();
        ch.append_message_full(c, "tool", "已记住", Some(&toolp("c1", "remember", "ok"))).unwrap();
        ch.append_message(c, "assistant", "").unwrap(); // 收尾轮:空文字、无工具

        let out = eng.conversation_trace(c, Default::default()).unwrap();
        assert!(out.is_empty(), "全静默回合不产药丸(Option A);得到 {out:?}");
    }

    /// 回合边界复位 + 纯文字回合不产药丸（否则会盖掉前端在飞攒的收尾轮 CoT，见 useChat 注释）。
    #[test]
    fn trace_resets_between_turns_and_pure_text_turn_has_no_pill() {
        let eng = engine("trace-reset");
        let c = conv_with(&eng);
        let ch = &eng.store.chat;
        // 回合 1:工具 + 可见收尾
        ch.append_message(c, "user", "放歌").unwrap();
        ch.append_message_full(c, "assistant", "", Some(&asst(&[("c1", "media_play")], None))).unwrap();
        ch.append_message_full(c, "tool", "在放了", Some(&toolp("c1", "media_play", "ok"))).unwrap();
        let t1 = ch.append_message(c, "assistant", "在放了").unwrap().id;
        // 回合 2:纯文字(无工具)
        ch.append_message(c, "user", "你好").unwrap();
        ch.append_message(c, "assistant", "你好呀").unwrap();

        let out = eng.conversation_trace(c, Default::default()).unwrap();
        assert_eq!(out.len(), 1, "只回合1有药丸,纯文字回合不产;得到 {out:?}");
        assert_eq!(out[0].message_id, t1);
        assert_eq!(sketch(&out[0]), ["media_play"], "回合2没串进回合1");
    }

    /// 同轮先说话再调工具:与 live 一致(Segment 封口在 ToolUse 之前)——该轮工具落到下一个气泡;
    /// 封口前的本轮 CoT 留在本气泡。回放须对齐,否则「重启后药丸换了个气泡」。
    #[test]
    fn trace_same_round_text_and_tools_anchors_tools_to_next_bubble() {
        let eng = engine("trace-interleaved");
        let c = conv_with(&eng);
        let ch = &eng.store.chat;
        ch.append_message(c, "user", "找电影并播放").unwrap();
        let a1 = ch.append_message_full(c, "assistant", "让我找找", Some(&asst(&[("c1", "media_search")], Some("先搜一下")))).unwrap().id;
        ch.append_message_full(c, "tool", "找到了", Some(&toolp("c1", "media_search", "ok"))).unwrap();
        let a2 = ch.append_message_full(c, "assistant", "开始播放", Some(&asst(&[("c2", "media_play")], None))).unwrap().id;
        ch.append_message_full(c, "tool", "播放中", Some(&toolp("c2", "media_play", "ok"))).unwrap();
        let a3 = ch.append_message(c, "assistant", "放好了").unwrap().id;

        let out = eng.conversation_trace(c, Default::default()).unwrap();
        assert_eq!(out.len(), 3, "三个可见气泡各成一段;得到 {out:?}");
        // a1「让我找找」:封口前只有本轮 CoT,本轮声明的 search 归下一气泡
        assert_eq!(out[0].message_id, a1);
        assert_eq!(sketch(&out[0]), ["想:先搜一下"], "本轮 search 归下一气泡,不留在 a1");
        // a2「开始播放」:收下 a1 轮声明的 search(结果回填得上)
        assert_eq!(out[1].message_id, a2);
        assert_eq!(sketch(&out[1]), ["media_search"]);
        assert_eq!(tools(&out[1])[0].result, "找到了");
        // a3「放好了」:收下 a2 轮声明的 play
        assert_eq!(out[2].message_id, a3);
        assert_eq!(sketch(&out[2]), ["media_play"]);
        assert_eq!(tools(&out[2])[0].result, "播放中");
    }
}
