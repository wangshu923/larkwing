//! 回合请求装配 + 建连:把原料拼成 `ChatRequest`,再开一条流。
//!
//! `assemble_request` 是 send_message / send_overheard / 子回合**共用**的原料清单 ——
//! 只此一处,两个入口的稳定前缀因此字节级相同、共享 provider 前缀缓存(§4.8)。

use super::*;

/// 本回合的上下文尾部字数预算:按**主候选**(首位 = 主选,99% 会服务它)的上下文窗口算
/// (model-aware:大窗口装文档、小窗口防溢出)。窗口未知 → 回落默认。极少数主选挂、回落到
/// 更小模型且 request 偏大时,开流 400 走既有 Failed 兜底(不静默,§3.5),不为罕见复合失败牺牲常路。
/// 记忆归人(§6):声纹/渠道识别出的说话人确属真实用户 → 本回合记忆/工具归 TA;
/// 否则会话归属者(访客/电视声识别不出 → fallback,绝不误记到家人名下)。
pub(super) fn resolve_mem_user(store: &Store, conv_user: i64, speaker_user: Option<i64>) -> anyhow::Result<i64> {
    Ok(match speaker_user {
        Some(sid) if sid != conv_user && store.users.get(sid)?.is_some() => sid,
        _ => conv_user,
    })
}

/// 上下文原料装配(send_message / send_overheard 共用;阻塞,调用方包 spawn_blocking):
/// 常驻记忆 / 需知 / 人设 / 名字 / 说话人表 / 关怀 → build_context → thinking 档。
/// **纯读不写**(用户行落库 / touch / 定题种子归 send_message 自己)—— 原料清单只此一处,
/// 两个入口的稳定前缀因此字节级相同、共享 provider 前缀缓存(§4.8)。
/// history 由调用方备好:send_message = 落完用户行的整页;overheard = 整页 + 一条不落库的虚拟 user 行。
#[allow(clippy::too_many_arguments)]
pub(super) fn assemble_request(
    store: &Store,
    scene: &crate::scenes::Scene,
    conv_id: i64,
    conv_user: i64,
    mem_user: i64,
    history: &[crate::store::Message],
    page_base: usize,
    budget: usize,
    tool_defs: &[ToolDef],
) -> anyhow::Result<crate::llm::ChatRequest> {
    // 记忆只取常驻·画像层进前缀(§13.3 ②;按需层靠 recall 工具取),写时已执法预算
    // → 前缀有界、字节稳定,记得再多也不胀前缀(修掉「全量进前缀」雷,§13.1)
    let memories = store.memory.list_resident(mem_user)?;
    // 观测:这回合带进前缀的常驻记忆(测「用到了记忆吗」—— recall 不一定触发,
    // 大多数记忆是从这里被动进上下文的;§4.4 进库前的轻量日志版)
    tracing::info!(
        target: "larkwing::memory",
        conv = conv_id, resident = memories.len(),
        "turn ctx → {}",
        memories.iter().map(|m| m.content.as_str()).collect::<Vec<_>>().join(" | ")
    );
    // 技能索引(L1):agent 的、恒全局;启用条目恒常驻无折叠,正文归 skill_lookup 按需取
    let skills = store.skills.list_enabled_index()?;
    // 任务需知:只有常驻条目进前缀(预算在写入时执法,这里无条件全装);
    // 非常驻的归 briefing_lookup 工具按需取
    let briefings: Vec<crate::store::Briefing> =
        store.briefings.list_for(mem_user)?.into_iter().filter(|b| b.resident).collect();
    // 性格设定用**会话归属者**(家给 7274 的人设,跟说话人无关 → 前缀字节稳定,
    // 一家人轮流说话不会让缓存失效);没设过=出厂默认句,空串=纯出厂人设
    let style = store
        .settings
        .get(Some(conv_user), "persona.style")?
        .unwrap_or_else(|| context::DEFAULT_PERSONA_STYLE.into());
    // 用户给助手起的名字(ui.pet_name):与性格设定同走会话归属者 → 前缀字节稳定;
    // 没设过/空 = 出厂名,build_context 不注入
    let pet_name = store.settings.get(Some(conv_user), "ui.pet_name")?;
    // 说话人标记原料(渠道归人/声纹,§6 记忆归人的可见面):历史里出现过 speaker
    // 才查家人表(id→名,排除会话归属者:主人自己不标);常态会话 = 空 map 零查询。
    let speakers: std::collections::HashMap<i64, String> = if history
        .iter()
        .any(|m| m.payload.as_deref().is_some_and(|p| p.contains("speaker_user")))
    {
        store.users.list()?.into_iter().filter(|u| u.id != conv_user).map(|u| (u.id, u.name)).collect()
    } else {
        Default::default()
    };
    let care_enabled = store.settings.get(None, "care.enabled")?.as_deref() != Some("0");
    // 未了的事(★主动关怀 切片2·B):开着关怀才取(归说话人 mem_user;限量进前缀)
    let care_todos = if care_enabled {
        store.todos.list_open(mem_user, context::TODO_PREFIX_LIMIT)?
    } else {
        Vec::new()
    };
    let mut request = context::build_context(
        scene,
        pet_name.as_deref(),
        Some(&style),
        care_enabled,
        &skills,
        &memories,
        &briefings,
        &care_todos,
        history,
        page_base,
        budget,
        tool_defs,
        &speakers,
    );
    // 反应模式(最快/轻度/中度/重度):每回合取值,改完下一句话就生效,无需重建 provider
    let thinking = match store.settings.get(None, "llm.thinking")?.as_deref() {
        Some("off") => crate::llm::Thinking::Off,
        Some("light") => crate::llm::Thinking::Light,
        Some("heavy") => crate::llm::Thinking::Heavy,
        // 缺省/"medium"/旧值"on"/未知 → 中度(默认反应模式)
        _ => crate::llm::Thinking::Medium,
    };
    if thinking != crate::llm::Thinking::Off {
        request.options.thinking = Some(thinking);
    }
    Ok(request)
}

pub(super) fn tail_budget(candidates: &[(String, Arc<dyn LlmProvider>)]) -> usize {
    match candidates.first() {
        Some((_, p)) => {
            let m = p.model_id();
            context::tail_budget_chars(
                crate::llm::catalog::ctx_window_of(m),
                crate::llm::catalog::billing_of(m),
            )
        }
        None => context::tail_budget_chars(None, crate::llm::catalog::BillingMode::default()),
    }
}

/// 候选逐个建连,首个成功者胜出(主选优先):launch 与 delegate 子回合共用,别再复制。
/// 返回 (事件流, provider_id, provider, 赢家建连起点 —— 计时从"这一家"起,切换浪费不算在赢家头上)。
pub(super) async fn open_stream(
    candidates: &[(String, Arc<dyn LlmProvider>)],
    request: &crate::llm::ChatRequest,
) -> Result<
    (mpsc::Receiver<crate::llm::ChatEvent>, String, Arc<dyn LlmProvider>, std::time::Instant),
    LlmError,
> {
    let mut first_err: Option<LlmError> = None;
    for (id, provider) in candidates {
        let started = std::time::Instant::now();
        match provider.chat_stream(request.clone()).await {
            Ok(rx) => {
                if first_err.is_some() {
                    tracing::warn!(provider = %id, "主选供应商建连失败,已切换备用");
                }
                return Ok((rx, id.clone(), provider.clone(), started));
            }
            Err(e) => {
                tracing::warn!(provider = %id, err = %e, "建连失败,尝试下一个候选");
                first_err.get_or_insert(e);
            }
        }
    }
    Err(first_err.expect("candidates 非空,必有错误"))
}

impl Engine {
    /// 回合开始时把「此刻」背景状态追加给模型(通用缝):各源各贡献一行,拼成一条不落库的
    /// 〔此刻 · …〕注记挂到末条 user 消息(持久前缀字节不动 → 前缀缓存不破)。目前只有 media
    /// 播放态(修「歌放完了模型却以为还在播」);以后的进行中任务 / 待触发提醒等在此各 push 一条
    /// 即可,缝不再动。在 build_context **之后**调,因装配闭包在线程池里拿不到 &self。
    pub(super) fn inject_ambient(&self, conv_id: i64, request: &mut crate::llm::ChatRequest) {
        let mut lines: Vec<String> = Vec::new();
        if let Some(s) = self.media.playback_summary() {
            lines.push(s);
        }
        // 运行中的后台差事(批量下载/配词):模型零工具调用即知进度,可直接答「到哪了」;
        // 带编号 → 「停下」可直奔 task_cancel。没有在跑 = 不占背景。
        if let Some(s) = self.media.bg().ambient_line() {
            lines.push(s);
        }
        // 「计划」(§6.5 会话内工作备忘):有未清空的计划就带一行 —— 跨回合断链
        // (批量 job 收尾唤回合「不知道还剩哪些」)正是靠这行接住。
        let plan_line = self
            .sessions
            .lk()
            .get(&conv_id)
            .and_then(|s| s.plan.lk().ambient_line());
        if let Some(s) = plan_line {
            lines.push(s);
        }
        if !lines.is_empty() {
            context::attach_ambient(request, &lines.join(";"));
        }
    }
}
