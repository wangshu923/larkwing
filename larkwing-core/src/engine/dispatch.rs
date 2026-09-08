//! 回合入口:发消息 / 插队 / 取消 / 旁听仲裁 / 自启回合,以及入站附件的处理。
//!
//! 内循环本体在 `turn.rs`;这里只管「谁来开一个回合、开之前先做什么」。
//! 取消 = 协作式 token(不 abort,硬杀会跳过 partial 落库);同会话新 send 自动
//! 取消旧回合并 await 收尾。

use super::request::{assemble_request, open_stream, resolve_mem_user, tail_budget};
use super::*;

/// 图片按 mime 定落盘扩展名(仅为文件名好看 / relay 猜 content-type;识别失败给 bin)。
// image_ext / save_image_blob 已上移 crate::files 单源(2026-08-13 show_image 批:
// 工具侧亮图与这里的用户发图共用同一个内容寻址仓,tools 不反依赖 engine → 家安在 files)。
use crate::files::{image_ext, save_image_blob};

/// 入站附件 → (图 image_url parts, 文档抽出的文字 + 落盘路径行, 落库小票)。send_message 与插队共用。
/// `atts_dir` = 图片缩略图落盘目录(`<media>/attachments`);图 bytes 写盘、小票记相对名,
/// 供重开会话回看(不进 DB、不喂 LLM)。图另落一份收件区原名件(见分支内注释)。
fn process_attachments(
    attachments: &[InAttachment],
    atts_dir: &std::path::Path,
    inbox_dir: &std::path::Path,
) -> (Vec<crate::llm::ContentPart>, String, Vec<AttachmentRef>) {
    use base64::Engine as _;
    let mut image_parts = Vec::new();
    let mut doc_text = String::new();
    let mut refs = Vec::new();
    for a in attachments {
        if crate::attach::is_image(&a.mime) {
            image_parts.push(crate::llm::ContentPart::ImageUrl {
                url: format!("data:{};base64,{}", a.mime, a.data),
            });
            // 解一次 base64,双落盘共用(失败只是缺那份落盘,回合照走):
            //   attachments/ = hash 名缩略图源,UI 回看用(§1 用户反馈:图转一圈只剩名字太糙);
            //   inbox/ = 原名件 + 路径行进上下文(2026-07-13 与文档收件区对称)——「把刚发的图
            //   存到桌面/发给某某」模型才有路径可操作;路径行随内容落库,后续回合(图 bytes
            //   不回放)也还找得到这张图。
            let bytes =
                base64::engine::general_purpose::STANDARD.decode(a.data.as_bytes()).ok();
            let file = bytes.as_ref().and_then(|b| save_image_blob(atts_dir, b, &a.mime));
            // 没扩展名的图(渠道来的名字可能光秃)按 mime 补,落到收件区好认好开。
            let clean = crate::files::sanitize_filename(&a.name);
            let named = if clean.contains('.') {
                clean
            } else {
                format!("{clean}.{}", image_ext(&a.mime))
            };
            let saved = bytes.as_ref().and_then(|b| save_inbox_blob(inbox_dir, &named, b));
            if let Some(p) = &saved {
                let shown = p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                doc_text.push_str(&format!("\n\n〔图片:{shown} 已存到本地:{}(可移动、整理、发送)〕", p.display()));
            }
            refs.push(AttachmentRef {
                kind: "image".into(),
                name: a.name.clone(),
                mime: a.mime.clone(),
                file,
            });
            continue;
        }
        // 文档/文件:bytes 先落「收件区」(B,2026-07-11)——给模型一个**可 fs 操作的本地
        // 绝对路径**,「把发来的文件存到电脑 / 整理」才成立;扫描件抽不出文字也照落(存文件
        // 不需读内容)。再尝试抽文字(能抽的多轮追问还在,§9)。
        // ⚠️ doc_text 带的是**运行时**绝对路径,会随内容进 history 落库 —— 数据根搬家后老对话
        // 里的路径会失效(只影响回看老对话,不影响当下操作),换取模型直接拿到路径能操作。
        let bytes =
            base64::engine::general_purpose::STANDARD.decode(a.data.as_bytes()).ok();
        let saved = bytes.as_ref().and_then(|b| save_inbox_blob(inbox_dir, &a.name, b));
        let extracted =
            bytes.as_ref().and_then(|b| crate::attach::extract_doc_text(&a.name, &a.mime, b));
        let abs = saved.as_ref().map(|p| p.display().to_string());
        match (&abs, &extracted) {
            (Some(p), Some(t)) => {
                doc_text.push_str(&format!("\n\n〔附件:{} 已存到本地:{p}〕\n{t}", a.name))
            }
            (Some(p), None) => doc_text.push_str(&format!(
                "\n\n〔附件:{} 已存到本地:{p}(读不出文字,可能是扫描件;文件已在本地,可移动、整理、发送)〕",
                a.name
            )),
            (None, Some(t)) => doc_text.push_str(&format!("\n\n〔附件:{}〕\n{t}", a.name)),
            (None, None) => doc_text.push_str(&format!("\n\n〔附件:{}(暂时读不出内容)〕", a.name)),
        }
        refs.push(AttachmentRef {
            kind: "doc".into(),
            name: a.name.clone(),
            mime: a.mime.clone(),
            // 相对名(收件区内);随数据根搬家不断链(§6.2 DB 不存绝对路径)
            file: saved.as_ref().and_then(|p| p.file_name()).map(|s| s.to_string_lossy().into_owned()),
        });
    }
    (image_parts, doc_text, refs)
}

/// 收到的文档/文件 bytes 落收件区,返回落定的绝对路径(给模型 fs 操作)。文件名清洗 +
/// 永不覆盖(同名 ` (N)`,§7.2 复用);写失败返回 None(回合照走,只是模型拿不到路径)。
fn save_inbox_blob(inbox_dir: &std::path::Path, name: &str, bytes: &[u8]) -> Option<std::path::PathBuf> {
    let clean = crate::files::sanitize_filename(name);
    let clean = if clean.is_empty() { "file".to_string() } else { clean };
    std::fs::create_dir_all(inbox_dir).ok()?;
    let path = crate::files::dedupe_path(&inbox_dir.join(clean));
    std::fs::write(&path, bytes).ok().map(|_| path)
}

/// 命令侧构造一条注入的就绪形(处理附件 + 物化 meta + 拼 LLM 文本)。
fn build_inject_ready(
    text: String,
    meta: Option<UserMeta>,
    attachments: Vec<InAttachment>,
    atts_dir: &std::path::Path,
    inbox_dir: &std::path::Path,
) -> InjectReady {
    let (parts, doc_text, refs) = process_attachments(&attachments, atts_dir, inbox_dir);
    let mut eff_meta = meta.unwrap_or_default();
    eff_meta.attachments = refs.clone();
    let payload = (!eff_meta.is_default()).then(|| serde_json::to_string(&eff_meta).ok()).flatten();
    let llm_content = format!("{text}{doc_text}");
    InjectReady { display: text, llm_content, parts, refs, payload }
}

// ---------- Engine ----------

impl Engine {
    /// 幂等取消:没在飞 = no-op。await 旧回合收尾(partial 落库完成)后才返回。
    pub async fn cancel(&self, conv_id: i64) {
        let handle = {
            let mut sessions = self.sessions.lk();
            sessions.get_mut(&conv_id).and_then(|slot| slot.inflight.take())
        };
        if let Some(h) = handle {
            h.token.cancel();
            let _ = h.join.await;
        }
    }

    /// 旁听仲裁回合(唤醒确认层「呼名+续句」三段式,§8.2 精度方向):名字被喊到但不一定
    /// 是叫它(「天天,暂停」vs「看天天向上」),整句交模型判。**临时回合**:用户行悬置不落库
    /// —— 模型只回 __IGNORE__ = 整轮蒸发(零 UI/历史/记忆痕迹,只留 tracing + usage 流水);
    /// 开口/调工具 = 连用户行一起转正落库。无前端 Channel(wake_turn 同款引擎内消费),
    /// 终态经全局车道:kind="overheard"(转正 → 前端刷新+念)/ "overheard_dismissed"
    /// (蒸发 → 前端只恢复 duck)。与在飞回合冲突 → 直接放弃(真输入优先,旁听是机会性的)。
    pub async fn send_overheard(
        &self,
        conv_id: i64,
        text: String,
        speaker: Option<i64>,
    ) -> Result<(), AppError> {
        // 任何早退都发 dismissed 信号:前端 duck 等着它恢复(§3.5 不静默吊着)
        let dismissed = |bus: &crate::bus::Bus| {
            bus.publish(crate::bus::AppEvent::Conversation(crate::bus::ConversationActivity {
                conv_id,
                kind: "overheard_dismissed".into(),
                outcome: crate::bus::TurnOutcome::Done,
            }));
        };
        let candidates = self.llm.rd().clone();
        if candidates.is_empty() {
            tracing::info!(conv = conv_id, "旁听仲裁:没有可用大脑,放弃");
            dismissed(&self.bus);
            return Ok(());
        }
        // 忙检(wake_turn 同款 is_finished):在飞就放弃 —— 别为仲裁打断真对话
        {
            let sessions = self.sessions.lk();
            let busy = sessions
                .get(&conv_id)
                .and_then(|s| s.inflight.as_ref())
                .is_some_and(|h| !h.join.is_finished());
            if busy {
                tracing::info!(conv = conv_id, "旁听仲裁:会话有在飞回合,放弃");
                dismissed(&self.bus);
                return Ok(());
            }
        }
        let Some(conversation) = self.store.chat.get_conversation(conv_id)? else {
            dismissed(&self.bus);
            return Ok(());
        };
        let scene = self
            .scenes
            .get(&conversation.scene_id)
            .unwrap_or_else(|| self.scenes.default_scene())
            .clone();
        let tool_subset = self.tools.subset(&scene.tools);
        let tool_defs: Vec<ToolDef> = tool_subset.iter().map(|t| t.spec().def()).collect();
        let budget = tail_budget(&candidates);

        let store = self.store.clone();
        let conv_user = conversation.user_id;
        let text_for_ctx = text.clone();
        let (mut request, mem_user, payload) = tokio::task::spawn_blocking(
            move || -> anyhow::Result<(crate::llm::ChatRequest, i64, Option<String>)> {
                let mem_user = resolve_mem_user(&store, conv_user, speaker)?;
                // 旁听形态物化成 payload(转正落库用同一份;〔旁听〕〔语音〕标记由它驱动)。
                // speak=true:屋里有人开口 = 语音交互,回应要念(§3.4)。
                let meta = UserMeta {
                    input: "overheard".into(),
                    speak: true,
                    speaker_user: speaker,
                    ..Default::default()
                };
                let payload = serde_json::to_string(&meta).ok();
                let total = store.chat.count_messages(conv_id)? as usize;
                let page_base = total.saturating_sub(context::HISTORY_PAGE_MAX);
                let mut history = store.chat.messages_page(
                    conv_id,
                    page_base as i64,
                    (total - page_base) as i64,
                )?;
                // 虚拟 user 行(**不落库**):与真实行走同一条装配路 → 标记/说话人渲染字节一致;
                // id=0 只当窗口锚点用。转正时 turn.rs 才把同内容+同 payload 真正写库。
                history.push(crate::store::Message {
                    id: 0,
                    conversation_id: conv_id,
                    role: "user".into(),
                    content: text_for_ctx,
                    created_at: crate::store::now_ms(),
                    payload: payload.clone(),
                    speaker_name: None,
                    trigger: None,
                });
                let request = assemble_request(
                    &store, &scene, conv_id, conv_user, mem_user, &history, page_base, budget,
                    &tool_defs,
                )?;
                Ok((request, mem_user, payload))
            },
        )
        .await
        .map_err(AppError::internal)??;

        // 「此刻」背景照带:仲裁「天天暂停」正需要知道在播什么(不落库、不破缓存)
        self.inject_ambient(conv_id, &mut request);

        let pending = turn::PendingUser { content: text, payload };
        let mut rx = match self
            .launch(conv_id, mem_user, candidates, request, tool_subset, 0, Some(pending))
            .await
        {
            Ok(rx) => rx,
            Err(e) => {
                tracing::info!(conv = conv_id, err = %e.message, "旁听仲裁建连失败,放弃");
                dismissed(&self.bus);
                return Ok(());
            }
        };
        // 引擎内消费(wake_turn 同款):转正才喊「会话有动静」;蒸发只发 duck 恢复信号
        let bus = self.bus.clone();
        tokio::spawn(async move {
            let mut committed = false;
            let mut outcome = crate::bus::TurnOutcome::Done;
            while let Some(ev) = rx.recv().await {
                match ev {
                    TurnEvent::Committed { .. } => committed = true,
                    TurnEvent::Failed { .. } => outcome = crate::bus::TurnOutcome::Failed,
                    _ => {}
                }
            }
            let (kind, outcome) = if committed {
                ("overheard", outcome)
            } else {
                // 未转正的失败/取消/蒸发对用户全无可见变化 → 统一走 dismissed 信号
                ("overheard_dismissed", crate::bus::TurnOutcome::Done)
            };
            bus.publish(crate::bus::AppEvent::Conversation(crate::bus::ConversationActivity {
                conv_id,
                kind: kind.into(),
                outcome,
            }));
        });
        Ok(())
    }

    /// 回合入口。同会话已有在飞 → 自动取消旧的再开新的(会话管控)。
    /// 前置错误走 Err(镜像 llm 两阶段);开流后走 TurnEvent。
    pub async fn send_message(
        &self,
        conv_id: i64,
        text: String,
        meta: Option<UserMeta>,
        attachments: Vec<InAttachment>,
    ) -> Result<mpsc::Receiver<TurnEvent>, AppError> {
        // 1. 会话管控:必须 await 旧回合收尾,partial 先落库,新回合拼历史才完整
        self.cancel(conv_id).await;

        // 2. 前置检查:候选快照(失序读快照,reload 不阻塞在飞回合)
        let candidates = self.llm.rd().clone();
        if candidates.is_empty() {
            return Err(AppError { kind: ErrorKind::NoApiKey, message: "还没有配置 API key".into() });
        }
        let conversation = self.store.chat.get_conversation(conv_id)?.ok_or(AppError {
            kind: ErrorKind::NotFound,
            message: format!("会话 {conv_id} 不存在"),
        })?;
        let scene = self
            .scenes
            .get(&conversation.scene_id)
            .unwrap_or_else(|| self.scenes.default_scene())
            .clone();

        // 3. 白名单工具子集(场景声明顺序,会话内稳定 → 前缀不抖)
        let tool_subset = self.tools.subset(&scene.tools);
        let tool_defs: Vec<ToolDef> = tool_subset.iter().map(|t| t.spec().def()).collect();

        // 本回合上下文尾部字数预算(按主候选窗口算,model-aware)
        let budget = tail_budget(&candidates);

        // 落用户消息 + 取上下文原料 + 单一装配权出 ChatRequest(阻塞 IO 下沉线程池)
        let store = self.store.clone();
        let conv_user = conversation.user_id;
        let atts_dir = self.media.attachments_dir(); // 图缩略图落盘目录(闭包内写图)
        let inbox_dir = self.media.inbox_dir(); // 收件区(闭包内落文档/图片原名件,给模型可操作路径)
        // 标题还空 = 本次 append 会写下截断占位 → 回头后台给它起个正经名(engine/title.rs)
        let needs_title = conversation.title.is_empty();
        let (mut request, user_msg_id, mem_user, title_seed) = tokio::task::spawn_blocking(
            move || -> anyhow::Result<(crate::llm::ChatRequest, i64, i64, Option<String>)> {
            // 入站附件(媒体输入 PLAN §9):图 bytes → image_url 当轮注入(不落库,vision 重复计费);
            // 文档**文字**并进落库的 user 消息内容 → 进 history,多轮追问还在、且落可缓存前缀
            // (§9 收窄:仅图 bytes 当轮,文档持久化)。图 bytes 另落文件、小票记相对名(重开会话
            // 回看图);图的收件区**路径行**随 doc_text 进落库内容(2026-07-13「存图到电脑」);
            // 只把小票(AttachmentRef)落 payload。与插队共用
            let (image_parts, doc_text, att_refs) =
                process_attachments(&attachments, &atts_dir, &inbox_dir);

            // 语音会话模式(PLAN §11)+ 附件小票:非默认形态物化进 payload(真相在库)
            let mut eff_meta = meta.unwrap_or_default();
            eff_meta.attachments = att_refs;
            let payload = (!eff_meta.is_default())
                .then(|| serde_json::to_string(&eff_meta))
                .transpose()?;
            // 文档文字 + 图片路径行并进落库内容(图 bytes 不并:仍当轮)。doc_text 自带「〔附件/图片:…〕」分隔。
            let stored_content =
                if doc_text.is_empty() { text } else { format!("{text}{doc_text}") };
            let user_msg = store.chat.append_message_full(
                conv_id,
                "user",
                &stored_content,
                payload.as_deref(),
            )?;
            // 定题种子 = 落库内容前缀(文档附件会把消息撑到几万字,title.rs 只吃开头;
            // 占位可由它重算 —— 占位只由首行前 24 字决定,这个截断必然覆盖)
            let title_seed = needs_title
                .then(|| stored_content.chars().take(title::INPUT_MAX_CHARS).collect::<String>());
            // 记忆归人(§6):声纹识别出且确属真实用户 → 本回合用 TA;否则会话归属者
            // (访客/电视声识别不出 → fallback,绝不误记到家人名下,robot 同款立场)
            let mem_user = resolve_mem_user(&store, conv_user, eff_meta.speaker_user)?;
            // touch **会话归属者**而非说话人:last_active_at 是「这台电脑前用 app 的人」的信号,
            // boot 的 ensure_default_user 按它恢复当前用户 —— 若 touch 说话人,家人在手机渠道
            // 说一句就会「最近活跃」,主人重启 app 竟被切成 TA 的视角(渠道归人启用后才暴露)。
            store.users.touch(conv_user)?;
            let total = store.chat.count_messages(conv_id)? as usize;
            // I/O 上界分页:最多取 HISTORY_PAGE_MAX 条,真窗口由 build_context 内字数预算裁;
            // page_base = 该页首条的绝对下标,喂 windowed_start 做整块锚定(缓存稳定)。
            let page_base = total.saturating_sub(context::HISTORY_PAGE_MAX);
            let history = store.chat.messages_page(
                conv_id,
                page_base as i64,
                (total - page_base) as i64,
            )?;
            // 原料读取 + 装配收口 assemble_request(与 send_overheard 共用 → 两个入口的
            // 稳定前缀字节级相同,共享 provider 前缀缓存 §4.8)
            let mut request = assemble_request(
                &store, &scene, conv_id, conv_user, mem_user, &history, page_base, budget,
                &tool_defs,
            )?;
            // 当轮注入图片 parts:挂到最后一条 user 消息上,持久前缀(few-shot/历史)字节不动 →
            // 缓存不破,也不为历史里的旧图反复付 vision 费(图当轮;文档文字已并进落库内容)。
            if !image_parts.is_empty() {
                if let Some(crate::llm::ChatMessage::User { parts, .. }) = request
                    .messages
                    .iter_mut()
                    .rev()
                    .find(|m| matches!(m, crate::llm::ChatMessage::User { .. }))
                {
                    parts.extend(image_parts);
                }
            }
            Ok((request, user_msg.id, mem_user, title_seed))
        })
        .await
        .map_err(AppError::internal)??;

        // 记忆自动提炼(PLAN §13 Phase 3):每 N 个用户回合后台蒸馏一次(尽力件,不阻塞本回合)。
        // 写到说话人(mem_user,记忆归人 §6);用户消息已落库,蒸馏读得到这段历史。
        if self.bump_consolidate_due(conv_id) {
            self.spawn_consolidate(conv_id, mem_user);
        }

        // 会话 LLM 命名(engine/title.rs,2026-07-02):新会话首条消息 → 后台起正经标题替换截断占位。
        // 与回合并行、尽力件,不阻塞首回合一个字。
        if let Some(seed) = title_seed {
            self.spawn_title(conv_id, seed);
        }

        // 「此刻」背景状态(播放器在不在放…)挂到末条 user,喂模型当下真相(不落库、不破缓存)
        self.inject_ambient(conv_id, &mut request);

        // 4+5. 开流 + spawn 回合(与 wake_turn 共用尾段)。ToolCtx.user_id = mem_user:
        // remember 写到说话人(记忆归人);会话归属仍是 conv_user。
        self.launch(conv_id, mem_user, candidates, request, tool_subset, user_msg_id, None).await
    }

    /// 插队(PLAN §9 B):把一条消息塞进**正在跑的回合**,它在下一次 LLM 调用就带上(不打断)。
    /// 返回 false = 没有在飞回合 / 回合正收尾 —— 调用方(前端)改用普通发送起新回合。
    pub async fn inject(
        &self,
        conv_id: i64,
        text: String,
        meta: Option<UserMeta>,
        attachments: Vec<InAttachment>,
    ) -> bool {
        // 取在飞回合的注入句柄(锁内只 clone Arc)
        let inject = {
            let sessions = self.sessions.lk();
            sessions.get(&conv_id).and_then(|slot| slot.inject.clone())
        };
        let Some(inject) = inject else { return false };
        // 提前拒:已在收尾就别处理了
        if inject.lk().finishing {
            return false;
        }
        // 处理附件(阻塞下沉线程池)→ 就绪形。附件目录先取(cheap PathBuf),move 进闭包写图。
        let atts_dir = self.media.attachments_dir();
        let inbox_dir = self.media.inbox_dir();
        let ready = match tokio::task::spawn_blocking(move || {
            build_inject_ready(text, meta, attachments, &atts_dir, &inbox_dir)
        })
        .await
        {
            Ok(r) => r,
            Err(_) => return false,
        };
        // 入队(再查一次 finishing:处理期间回合可能已收尾,原子防丢)
        let mut st = inject.lk();
        if st.finishing {
            return false;
        }
        st.buffer.push(ready);
        true
    }

    /// 共用尾段:开流(建连失败按候选顺序切换)→ spawn 回合 → 登记在飞。
    /// 全军覆没报主选的错误(最有代表性)。开流之后的失败不切换 —— 半截话已经
    /// 流向用户,静默换供应商重说会精神分裂,走既有 Failed 友好兜底。
    /// 工具轮的 2+ 次开流粘住本次选中的 provider(Turn 持有它)。
    #[allow(clippy::too_many_arguments)] // 私有装配尾段:每个参数都有单独语义,收拢成 struct 反而绕
    async fn launch(
        &self,
        conv_id: i64,
        user_id: i64,
        candidates: Vec<(String, Arc<dyn LlmProvider>)>,
        mut request: crate::llm::ChatRequest,
        tool_subset: Vec<Arc<dyn crate::tools::Tool>>,
        user_msg_id: i64,
        overheard: Option<turn::PendingUser>,
    ) -> Result<mpsc::Receiver<TurnEvent>, AppError> {
        // 防溢出安全阀(model-aware):工具循环累积的 ToolResult / 背景状态在 build_context 之后
        // 才注入(绕过初始窗口),单条可达数万字 → 开流前对累积的 messages 再按预算封顶一道(§0.2.0)。
        context::cap_messages_tail(&mut request.messages, tail_budget(&candidates));
        let (rx_llm, provider_id, provider, first_round_start) =
            open_stream(&candidates, &request).await?;

        let (tx, rx) = mpsc::channel::<TurnEvent>(64);
        let token = CancellationToken::new();
        // 记账用的模型 id:单轮覆盖优先,否则取选中 provider 的默认模型
        let model =
            request.options.model.clone().unwrap_or_else(|| provider.model_id().to_string());
        let inject = Arc::new(Mutex::new(InjectState::default())); // 插队队列:Turn 与 inject 命令共用
        // 计划槽(§6.5):Turn 嗅探 plan_set 后写它;先于 spawn 取,与收尾块用同一个 slot
        let plan = {
            let mut sessions = self.sessions.lk();
            sessions.entry(conv_id).or_default().plan.clone()
        };
        let is_overheard = overheard.is_some();
        let join = tokio::spawn(
            turn::Turn {
                store: self.store.clone(),
                conv_id,
                user_id,
                token: token.clone(),
                tx,
                provider,
                provider_id,
                model,
                user_msg_id,
                first_round_start,
                request,
                tools: tool_subset,
                media: self.media.clone(),
                web: self.web_renderer(),
                voice: self.voice(),
                confirm: Some(self.confirmer.clone()),
                rx: rx_llm,
                inject: inject.clone(),
                plan,
                overheard,
                ephemeral: false,
                max_rounds: turn::MAX_TOOL_ROUNDS,
                grants: Default::default(),
                agent: self.sub_agent(user_msg_id),
            }
            .run(),
        );
        // 登记在飞句柄。**绝不能直接覆盖**(2026-08-22 审计):TurnHandle drop 既不 abort join
        // 也不 cancel token,被挤掉的回合会继续在同一会话里跑、而**再没有人持有它的 token**——
        // 停止按钮(cancel_generation)、delete/rollback 的 `cancel().await` 全都够不到它,
        // 它还会往已被截断的会话里接着插行。忙检(is_finished)与真正登记之间隔着 open_stream
        // 整次 HTTP 建连,这个窗口里谁都看不见对方(渠道同 chat 连发两条最容易撞)。
        //
        // 谁让路就取消谁,任何一支都不许无主地跑下去:
        //   · 普通回合撞上在飞 → 新的赢(与 send_message 先 cancel 的语义一致),挤掉旧的;
        //   · **旁听仲裁撞上在飞 → 旁听让路**(它是机会性的,真输入优先 §8.2)——取消刚起的自己,
        //     绝不能反过来把用户真回合杀掉。
        let loser = {
            let mut sessions = self.sessions.lk();
            let slot = sessions.entry(conv_id).or_default();
            register_inflight(slot, TurnHandle { token, join }, is_overheard, inject)
        };
        if let Some(h) = loser {
            h.token.cancel();
            // 不在这里 await 收尾:launch 的调用方正等 rx(await 会把新回合的起播卡在旧回合上)。
            // 协作式取消 + partial 落库由被取消方自己走完,这里只保证它**有人取消过**。
            tokio::spawn(async move {
                let _ = h.join.await;
            });
        }
        Ok(rx)
    }

    /// 自启回合(调度器到点调用;PLAN §8「分离 job 型」的兑现):
    /// 执行一律**新鲜上下文** —— 稳定前缀与聊天回合字节级相同(共享缓存),不回放历史;
    /// 任务语境靠创建时物化进 content。无前端 Channel,engine 自己消费事件流,
    /// 完成后经全局事件车道喊"会话有动静"。
    /// 返回 false = 目标会话正有在飞回合,本次不打扰(调度器下个 tick 重试)。
    pub async fn wake_turn(&self, job: &crate::store::Job) -> Result<bool, AppError> {
        let candidates = self.llm.rd().clone();
        if candidates.is_empty() {
            return Err(AppError { kind: ErrorKind::NoApiKey, message: "还没有配置 API key".into() });
        }
        let budget = tail_budget(&candidates);

        // 会话兜底:原会话被删 → 该用户最新会话 → 新建(boot 同款链)
        let store = self.store.clone();
        let (job_conv, job_user) = (job.conv_id, job.user_id);
        let conversation = tokio::task::spawn_blocking(
            move || -> anyhow::Result<crate::store::Conversation> {
                if let Some(c) = store.chat.get_conversation(job_conv)? {
                    return Ok(c);
                }
                if let Some(c) = store.chat.latest_conversation(job_user)? {
                    return Ok(c);
                }
                // 自启回合兜底新建 = 系统渠道(原会话被删、用户也无任何会话时才走到)
                store.chat.create_conversation_full(
                    job_user,
                    DEFAULT_SCENE_ID,
                    crate::store::chat::CHANNEL_SYSTEM,
                )
            },
        )
        .await
        .map_err(AppError::internal)??;
        let conv_id = conversation.id;

        // 忙检:绝不打断用户正在进行的对话;调度器会重试。
        // ⚠️ 必须看 `join.is_finished()`,不能只看 `inflight.is_some()` —— 正常收尾的回合
        // **不清 inflight**(只有 cancel/新 send 才 take),句柄常驻 Some;只查 is_some 的话,
        // 会话只要聊过一次,之后的提醒就被「会话忙」永远无声跳过(2026-07-04 真机实锤:
        // 提醒到点零动静零日志;重启曾侥幸——sessions 是内存态,重启即清)。
        {
            let sessions = self.sessions.lk();
            let busy = sessions
                .get(&conv_id)
                .and_then(|s| s.inflight.as_ref())
                .is_some_and(|h| !h.join.is_finished());
            if busy {
                return Ok(false);
            }
        }

        let scene = self
            .scenes
            .get(&conversation.scene_id)
            .unwrap_or_else(|| self.scenes.default_scene())
            .clone();
        let tool_subset = self.tools.subset(&scene.tools);
        let tool_defs: Vec<ToolDef> = tool_subset.iter().map(|t| t.spec().def()).collect();

        // 落 event 行(UI 渲染成系统线)+ 拼新鲜请求。汇报类(后台差事忙完)在 payload 里
        // 记一笔 kind=report:前端据此换标签、且不自动念(到点提醒才必须出声)。
        let store = self.store.clone();
        let user_id = job.user_id;
        let content = job.content.clone();
        let is_report = job.kind == "report";
        let (mut request, event_msg_id) = tokio::task::spawn_blocking(
            move || -> anyhow::Result<(crate::llm::ChatRequest, i64)> {
                let event_payload = is_report.then_some(r#"{"kind":"report"}"#);
                let event_msg =
                    store.chat.append_message_full(conv_id, "event", &content, event_payload)?;
                // 只取常驻·画像层(§13.3 ②);任务回合与聊天回合共用同款前缀
                let memories = store.memory.list_resident(user_id)?;
                let skills = store.skills.list_enabled_index()?;
                let briefings: Vec<crate::store::Briefing> = store
                    .briefings
                    .list_for(user_id)?
                    .into_iter()
                    .filter(|b| b.resident)
                    .collect();
                let style = store
                    .settings
                    .get(Some(user_id), "persona.style")?
                    .unwrap_or_else(|| context::DEFAULT_PERSONA_STYLE.into());
                let pet_name = store.settings.get(Some(user_id), "ui.pet_name")?;
                let care_enabled =
                    store.settings.get(None, "care.enabled")?.as_deref() != Some("0");
                // 未了的事(★主动关怀 切片2·B):开着关怀才取(归 user_id;限量进前缀)
                let care_todos = if care_enabled {
                    store.todos.list_open(user_id, context::TODO_PREFIX_LIMIT)?
                } else {
                    Vec::new()
                };
                // 历史 = 空(新鲜上下文);注入消息与回放翻译同一字节形
                let mut request = context::build_context(
                    &scene,
                    pet_name.as_deref(),
                    Some(&style),
                    care_enabled,
                    &skills,
                    &memories,
                    &briefings,
                    &care_todos,
                    &[],
                    0,
                    budget,
                    &tool_defs,
                    &std::collections::HashMap::new(), // 历史为空 → 无说话人标记
                );
                request
                    .messages
                    .push(crate::llm::ChatMessage::user(context::event_injection(&content)));
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
                Ok((request, event_msg.id))
            },
        )
        .await
        .map_err(AppError::internal)??;

        // 自启回合也带「此刻」背景(任务到点时音乐可能正放着);不落库、不破缓存
        self.inject_ambient(conv_id, &mut request);
        let mut rx = match self
            .launch(conv_id, user_id, candidates, request, tool_subset, event_msg_id, None)
            .await
        {
            Ok(rx) => rx,
            // 建连失败(没 key / 401 / 连不上):event 行已经落了,但回合根本没起来。调度器的 Err
            // 分支不推进 due_at(留着重试),所以下个 tick 会再进来一次、再落一行 —— 撤掉这行,
            // 否则「⏰ 到点了」每 30s 堆一条、还全进后续回合上下文(2026-08-22 审计)。
            // 回合真起来后的失败(Failed 事件)不走这里,那种「到点必有动静」的系统线要留(§3.5)。
            Err(e) => {
                let store = self.store.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    store.chat.delete_message(conv_id, event_msg_id)
                })
                .await;
                return Err(e);
            }
        };

        // 无人挂流:自己消费到收尾,记下终态,然后经全局事件车道喊一声
        // (UI 据此刷新列表;用户不在该会话时按 outcome 在列表项打标)
        let bus = self.bus.clone();
        tokio::spawn(async move {
            let mut outcome = crate::bus::TurnOutcome::Done;
            while let Some(ev) = rx.recv().await {
                if matches!(ev, TurnEvent::Failed { .. }) {
                    outcome = crate::bus::TurnOutcome::Failed;
                }
            }
            bus.publish(crate::bus::AppEvent::Conversation(crate::bus::ConversationActivity {
                conv_id,
                // 汇报 ≠ 提醒:桌面据此决定念不念(渠道那边两者都照推,见 channels::outbound_loop)
                kind: if is_report { "report".into() } else { "reminder".into() },
                outcome,
            }));
        });
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 「存图到电脑」(2026-07-13,与文档收件区对称):图除当轮 image_url 注入外,原名件落
    /// 收件区、「已存到本地」路径行进 doc_text(随内容落库);UI 缩略图 hash 小票不变;
    /// 同名 ` (N)` 永不覆盖;渠道来的光秃名按 mime 补扩展。
    #[test]
    fn process_attachments_gives_images_a_local_inbox_path() {
        use base64::Engine as _;
        let base = std::env::temp_dir().join(format!("lw-img-inbox-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let atts = base.join("atts");
        let inbox = base.join("inbox");
        let b64 = base64::engine::general_purpose::STANDARD.encode(b"fake-image-bytes");
        let att = |name: &str, mime: &str| InAttachment {
            name: name.into(),
            mime: mime.into(),
            data: b64.clone(),
        };

        let (parts, doc_text, refs) =
            process_attachments(&[att("全家福.png", "image/png")], &atts, &inbox);
        assert_eq!(parts.len(), 1, "当轮视觉注入不变");
        assert!(inbox.join("全家福.png").is_file(), "原名件落收件区");
        assert!(
            doc_text.contains("已存到本地") && doc_text.contains("全家福.png"),
            "路径行进上下文: {doc_text}"
        );
        assert_eq!(refs[0].kind, "image");
        assert!(
            refs[0].file.as_deref().unwrap_or_default().ends_with(".png"),
            "UI 缩略图小票仍是 attachments 的 hash 名"
        );

        // 同名第二张:永不覆盖(§7.2 规①),路径行报实际落定名
        let (_, doc2, _) = process_attachments(&[att("全家福.png", "image/png")], &atts, &inbox);
        assert!(inbox.join("全家福 (2).png").is_file(), "同名去重不覆盖");
        assert!(doc2.contains("全家福 (2).png"), "路径行报去重后的名字: {doc2}");

        // 渠道来的光秃名(无扩展):按 mime 补,好认好开
        let (_, doc3, _) = process_attachments(&[att("photo", "image/jpeg")], &atts, &inbox);
        assert!(inbox.join("photo.jpg").is_file(), "无扩展名按 mime 补");
        assert!(doc3.contains("photo.jpg"), "{doc3}");
    }
}
