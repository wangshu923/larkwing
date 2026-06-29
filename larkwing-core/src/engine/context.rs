//! ContextBuilder:全系统唯一知道"prompt 长什么样"的地方(单一装配权)。
//! 纯函数、无 IO;原料(persona/记忆/历史/工具定义)由回合管线从 store 取来递入。
//! 核心不变量 = 前缀稳定:稳定层在前(persona + 画像记忆 + few-shot),易变尾在后
//! (锚定窗口内的最近消息)。

use std::collections::HashSet;

use crate::llm::{ChatMessage, ChatRequest, ToolDef};
use crate::scenes::Scene;
use crate::store::{Briefing, Memory, Message};

use super::{AssistantPayload, ToolRowPayload, UserMeta};

/// persona.style 的出厂默认 = **空**:默认保持中性,不预设任何性格倾向(不萌也不酷,
/// 连"不卖萌"这类否定式规定也不写——那本身就是一种倾向),让默认适配最多用户。
/// 想要某种性格的用户自己在设置「我的性格」里一句话写(输入框占位符已给示例)。
/// 空 = 不注入"性格设定"层,人设就是 companion.json 那段纯功能性中性底座。
/// 与前端 useSettings DEFAULTS['persona.style'] 手工同步(两边各一行,改要一起改)。
pub(super) const DEFAULT_PERSONA_STYLE: &str = "";

/// 出厂默认助手名:填进 persona 的 `{name}` 占位(用户没在「叫我什么」改过时)。
/// 与前端 pet.name(locales)、宪法 §4.1 默认名手工同步 —— 改默认名 = 改这三处。
pub(super) const DEFAULT_NAME: &str = "7274";

/// 运行时法条(PLAN §9 提示词蓝图):**人格中立底座**的一部分 —— 通用行为纪律,
/// 与性格无关,住这里不住场景数据;第二个场景出现时自动继承。
/// 点名的工具全部是常驻基础工具(tools::BASE_TOOLS),每场景必在。静态 → 字节稳定吃缓存。
pub(super) const LAWS: &str = "\
## 怎么记事(两本账)
用户告诉你的事,分三种处理:
- 关于「人」的长期事实(名字、家人、喜好、忌口、纪念日)→ 用 remember 记小本本。
- 关于「这个家的环境」(资源放在哪、目录路径、设备、家里的惯例)→ 用 briefing_write 记家庭备忘;同一主题(domain)整存整取、再写会整体覆盖。所以不管是更正(换了/搬了/不对)还是补充新信息,都要把该主题已有的内容连同新内容一起写全,绝不只写新增那点、把旧的覆盖没了;只有旧信息确实过期作废,才用新内容整段替掉。拿不准旧内容是什么,先 briefing_lookup 查一遍再写。
- 情绪、闲聊、一次性安排 → 不记。拿不准就先不记:记错了用户能删,乱记很烦人。

## 用你知道的,别反问
下方「你记得关于用户的这些事」和「任务需知」里写着的,就是你已经知道的,直接用,别去反问你手上已经有的信息。像是以前聊到过、但「你记得的事」里没写的(某个习惯、说过的偏好、家人或宠物),先用 recall 查一遍;像是家里登记过、但「任务需知」里没写的(资源/目录/设备),先用 briefing_lookup 查一遍;都查不到再说不知道。

## 说人话
工具安静地用,结果自然织进话里,不念原始数据。记完用一句话确认你理解的内容,让用户有机会当场纠正。永远不向用户提「工具」「调用」「数据库」「需知」这些词;「小本本」可以说——那是用户看得见的本子。

## 说话守则
用户消息开头带〔语音〕= 这一轮你是在跟人**说话**,不是写字:短句口语,先说结论,一句话说得清就别铺垫;不用表格、列表、代码块、链接、emoji 和任何 Markdown 记号;少用没信息量的语气词,也别自己脑补内容简介(剧情、背景这些没人问就别加);数字和时间说人话(「三点一刻」而不是「15:15」);内容多就挑两三个要点说,再问要不要继续。没有〔语音〕= 正常排版,该用列表用列表,该带点 emoji 也行。〔语音〕是系统加的输入形态标记,不是用户打的字,绝不复述它。

## 不是每句话都冲你来
〔语音〕回合里,如果听到的内容明显不是对你说的——电视的声音、家里人互相聊天、没头没尾的环境碎片——就只回 __IGNORE__ 这一个词,什么都不解释。拿不准的就当是对你说的,正常回应。

## 此刻状态(背景信息)
用户消息末尾有时带一行〔此刻 · …〕,那是系统给你的当下真实状态。当背景看:**只在用户这句话确实跟它有关时才参考**,就按它给的当下情况来、别凭印象或旧消息瞎猜,也别为此反问;无关时别主动提起,任何时候都绝不复述这行标记本身。

## 出厂示范
对话最开头以【示范】开头的几段是出厂教学样例,**只用来学怎么用工具、怎么说话**。里面的人物、称呼、地名、文件、片名/歌名,以及工具查到的结果(本地有没有某文件、搜到什么、想起什么)**全部是编的**,与眼前这位用户无关:既不能当作用户的真实信息去引用或称呼,也绝不能当成你自己已经知道的事实——该查证的仍要现查现问,不拿示范里的结论当答案。真实对话从示范之后开始。";

/// 把「此刻状态」背景行追加到最后一条 user 消息上(回合级、**不落库** → 持久前缀字节不动、
/// 前缀缓存不破;与附件当轮注入同款手法)。summary 由各 ambient 源给出(目前只有 media 播放态,
/// 这条通用缝以后可叠加进行中任务 / 待触发提醒等)。中性机制语言,模型据此知道当下真相
/// (修「歌放完了却以为还在播」)。理论上必有 user 消息;万一没有则静默跳过。
pub(super) fn attach_ambient(request: &mut ChatRequest, summary: &str) {
    if let Some(ChatMessage::User { content, .. }) =
        request.messages.iter_mut().rev().find(|m| matches!(m, ChatMessage::User { .. }))
    {
        content.push_str("\n\n〔此刻 · ");
        content.push_str(summary);
        content.push('〕');
    }
}

/// 定时任务到点的注入形:wake_turn 现场构造一条;历史回放里的 event 行走同一翻译
/// (两处字节一致)。中性机制语言(人格中立),对外话术由模型按当前人格组织。
pub(super) fn event_injection(content: &str) -> String {
    format!(
        "【定时任务到点】{content}\n(这是之前定好的安排自动触发,不是用户此刻说的话。\
         该捎话就用你的口吻说出来;该干活就直接动手,完成后简短汇报。)"
    )
}

/// 窗口内最多消息数。
pub(super) const WINDOW_MAX: usize = 48;
/// 锚点一次推进的整块大小。
pub(super) const WINDOW_CHUNK: usize = 16;

/// 窗口锚定:返回历史的起始下标。
/// 不每轮滑一条 —— 锚点只按整块推进,保证前缀字节级稳定,吃 DeepSeek 自动前缀缓存。
/// 不变量:start 是 CHUNK 的倍数,且 total - start ∈ (WINDOW_MAX - WINDOW_CHUNK, WINDOW_MAX]。
/// 注意:整块起点可能落在工具轮中间 —— build_context 内再向后吸附到 user 边界,
/// 防 tool_call/result 配对被拆散(OpenAI 系孤儿 tool 消息会 400)。
pub(super) fn anchored_start(total: usize) -> usize {
    if total <= WINDOW_MAX {
        return 0;
    }
    (total - WINDOW_MAX).div_ceil(WINDOW_CHUNK) * WINDOW_CHUNK
}

/// 尾部字数预算上限(字符,§0.2.0 上下文安全阀)。DeepSeek 级上下文 ~64K token;稳定前缀
/// (persona + 法条 + 记忆≤`RESIDENT_BUDGET_CHARS` + 需知 + few-shot≤800tok)已写时执法有界,
/// 故把**易变尾**也压在此预算内,总量就稳在窗口下。中文最坏 ~1 token/字时 4.8 万字 ≈ 3.2 万
/// token,加前缀仍宽裕;英文更省。起步值,真用可调(§13.7 同款「只能真用才能调」)。
pub(super) const TAIL_BUDGET_CHARS: usize = 48_000;

/// 尾部字数安全阀:从最新往回累加 content+payload 字数,返回「装配尾部应从哪条起」的下标。
/// 历史窗口已按条数锚定(caller `anchored_start`),但**单条消息大小不设限**——`fs_read_text`
/// 单次可达 4 万字、web 抓取 6 千字,几坨大结果攒在窗口里就能把上下文撑爆(provider 400
/// `context_length_exceeded`,且大消息卡在窗口内每轮重建都超大 → 会话卡死)。这里按字数封顶。
/// 不变量:① 预算够(总字数 ≤ 预算)→ 返回 0 → 与无安全阀等价 → **前缀缓存零损伤**(常态走这);
/// ② 永不裁掉最后一轮(末个 user/event 起点之后整段恒保留,哪怕它自己就超预算——没法再省,
/// 交 provider / per-tool 上限兜)。snap 边界由 build_context 在此之后做。
fn tail_budget_start(history: &[Message]) -> usize {
    let msg_chars = |m: &Message| {
        m.content.chars().count() + m.payload.as_deref().map_or(0, |p| p.chars().count())
    };
    let total: usize = history.iter().map(&msg_chars).sum();
    if total <= TAIL_BUDGET_CHARS {
        return 0; // 常态:不触发 → 起点 0 → 缓存稳定
    }
    // 最后一轮的起点(末个 user/event 边界):预算线绝不越过它(不变量 ②)。
    let last_round = history
        .iter()
        .rposition(|m| m.role == "user" || m.role == "event")
        .unwrap_or(0);
    // 从末尾往回累加,找到仍在预算内的最早下标。
    let mut acc = 0usize;
    let mut keep = history.len();
    for (i, m) in history.iter().enumerate().rev() {
        acc += msg_chars(m);
        if acc > TAIL_BUDGET_CHARS {
            break;
        }
        keep = i;
    }
    keep.min(last_round)
}

/// 单条 ChatMessage 的字数成本(估 token 用):content + reasoning + tool_calls(name+args)。
/// parts(图)不计——视觉块按 provider 自己的图 token 算,不在文本预算里。
fn message_chars(m: &ChatMessage) -> usize {
    match m {
        ChatMessage::User { content, .. } => content.chars().count(),
        ChatMessage::Assistant { content, reasoning, tool_calls, .. } => {
            content.chars().count()
                + reasoning.as_deref().map_or(0, |r| r.chars().count())
                + tool_calls
                    .iter()
                    .map(|c| c.name.chars().count() + c.args.to_string().chars().count())
                    .sum::<usize>()
        }
        ChatMessage::ToolResult { content, .. } => content.chars().count(),
    }
}

/// 就地封顶(§0.2.0 安全阀的活动期一半):`build_context` 的尾部安全阀只管**初始装配**,但
/// ① 工具循环里 `request.messages` 每轮累积 ToolResult(单条 `fs_read_text` 可达 4 万字)、
/// ② 文档附件在 build_context **之后**才注入到末条 user(绕过初始安全阀)——两条都会让 messages
/// 持续变大撑爆上下文。故在**每次开流前**再兜一道:就地把过老消息从前面整轮丢到字数预算内。
/// 不变量:cut 落在 User 边界(首条保持 User → 不留孤儿 ToolResult / 悬空 tool_calls 轮);绝不丢
/// 最后一条 User(当前任务);system 前缀不在 messages 里、永不动;常态不超预算 → no-op → 缓存不破。
pub(super) fn cap_messages_tail(messages: &mut Vec<ChatMessage>) {
    let total: usize = messages.iter().map(message_chars).sum();
    if total <= TAIL_BUDGET_CHARS {
        return; // 常态:不触发 → 不动 messages → 前缀缓存零损伤
    }
    let is_user = |m: &ChatMessage| matches!(m, ChatMessage::User { .. });
    // 最后一条 User(当前任务):cut 绝不越过它(不变量 ②)。
    let last_user = messages.iter().rposition(is_user).unwrap_or(0);
    // 从末尾往回累加,找仍在预算内的最早下标。
    let mut acc = 0usize;
    let mut keep = messages.len();
    for (i, m) in messages.iter().enumerate().rev() {
        acc += message_chars(m);
        if acc > TAIL_BUDGET_CHARS {
            break;
        }
        keep = i;
    }
    let keep = keep.min(last_user);
    // 向后吸附到 User 边界:首条保持 User,不留孤儿 ToolResult / 悬空 tool_calls 轮。
    let snap = messages[keep..].iter().position(is_user).unwrap_or(0);
    let cut = keep + snap;
    if cut > 0 {
        messages.drain(0..cut);
        tracing::warn!(dropped = cut, "上下文逼近上限,丢弃最老的整轮消息(安全阀)");
    }
}

pub(super) fn build_context(
    scene: &Scene,
    user_name: Option<&str>,
    user_style: Option<&str>,
    memories: &[Memory],
    briefings: &[Briefing],
    history: &[Message],
    tool_defs: &[ToolDef],
) -> ChatRequest {
    // 稳定层:persona(性格)→ 法条(底座纪律)→ 性格设定 → 记忆 → 任务需知
    // (按稳定度降序排,全部进缓存前缀)
    let mut system = String::with_capacity(scene.persona.len() + LAWS.len() + 256);
    // 名字直接填进 persona 的「你是 {name}」占位(ui.pet_name;没改过 = 出厂默认名):
    // 比"先写死代号再追加一句覆盖"直白 —— 模型只看到一个名字、不打架。
    // 名字每用户稳定(与会话归属者绑定)→ 替换结果字节稳定吃前缀缓存;默认名 = 出厂 persona 原样。
    let name = user_name.map(str::trim).filter(|s| !s.is_empty()).unwrap_or(DEFAULT_NAME);
    system.push_str(&scene.persona.replace("{name}", name));
    system.push_str("\n\n");
    system.push_str(LAWS);
    // 用户的一句话性格设定:叠加在中性底座之上的性格/口吻层(底座不预设性格 → 加性、非覆盖;
    // 它是这家人给 7274 挑的性格)。仍放在出厂人设之后:稳定层排序 + 与底座format(简短等)相左时按它来。
    if let Some(style) = user_style.map(str::trim).filter(|s| !s.is_empty()) {
        system.push_str("\n\n## 性格设定(就按这个性格和口吻说话)\n");
        system.push_str(style);
    }
    if !memories.is_empty() {
        system.push_str("\n\n## 你记得关于用户的这些事\n");
        for mem in memories {
            system.push_str("- ");
            system.push_str(&mem.content);
            system.push('\n');
        }
    }
    // 任务需知(PLAN §9):常驻条目,(scope, domain) 稳定序由 repo 保证 → 字节稳定。
    // 预算在写入时执法,这里无条件全装。
    if !briefings.is_empty() {
        system.push_str("\n\n## 任务需知(环境与资源)\n");
        for b in briefings {
            system.push_str(&format!("- [{}] {}\n", b.domain, b.content));
        }
    }

    // 尾部字数安全阀(§0.2.0 防溢出):先按字数把过老的整轮压掉(常态不触发 → 返回 0)。
    let budget_start = tail_budget_start(history);
    // 锚点吸附 user/event 边界:起点若落在工具轮中间,向后推进到第一条回合起点行,
    // 保证窗口内 tool_call/result 永远成对(吸附量 ≤ 一个工具轮,有界)。
    // event 也是合法回合起点(任务专属会话里没有 user 行)。budget_start=0 时 = 原 first_user 吸附。
    let snap = history[budget_start..]
        .iter()
        .position(|m| m.role == "user" || m.role == "event")
        .unwrap_or(0);
    let history = &history[budget_start + snap..];

    // few-shot 在真实历史之前:按场景静态 → 字节级稳定 → 吃前缀缓存(PLAN §8)
    let mut messages: Vec<ChatMessage> = scene.few_shots.clone();

    // 易变尾:锚定窗口内的最近消息(store::Message → llm::ChatMessage 的映射只在这)。
    // 工具轮回放:assistant 行的 payload 带 tool_calls + reasoning(坑 #4:DeepSeek
    // 要求工具轮回传 reasoning),'tool' 行配对回放;孤儿 tool 行跳过(防 400)。
    let mut open_calls: HashSet<String> = HashSet::new();
    for msg in history {
        match msg.role.as_str() {
            // 语音会话模式(PLAN §11):speak=true 的 user 行加确定性标记——同一消息
            // 每轮翻译结果一致 → 历史区字节稳定,前缀缓存零损伤;说话守则按标记生效
            "user" => {
                let speak = msg
                    .payload
                    .as_deref()
                    .and_then(|p| serde_json::from_str::<UserMeta>(p).ok())
                    .map(|m| m.speak)
                    .unwrap_or(false);
                let content = if speak {
                    format!("〔语音〕{}", msg.content)
                } else {
                    msg.content.clone()
                };
                messages.push(ChatMessage::user(content));
            }
            // 定时任务的触发行:回放成 user 形 + 机制标记(与 wake 注入同一翻译)
            "event" => messages.push(ChatMessage::user(event_injection(&msg.content))),
            "assistant" => {
                let payload: AssistantPayload = msg
                    .payload
                    .as_deref()
                    .and_then(|p| serde_json::from_str(p).ok())
                    .unwrap_or_default();
                open_calls.extend(payload.tool_calls.iter().map(|c| c.id.clone()));
                messages.push(ChatMessage::Assistant {
                    content: msg.content.clone(),
                    reasoning: payload.reasoning,
                    tool_calls: payload.tool_calls,
                    reasoning_state: payload.reasoning_state,
                });
            }
            "tool" => {
                let Some(payload) = msg
                    .payload
                    .as_deref()
                    .and_then(|p| serde_json::from_str::<ToolRowPayload>(p).ok())
                else {
                    tracing::warn!(msg = msg.id, "tool 行缺 payload,跳过");
                    continue;
                };
                if open_calls.remove(&payload.call_id) {
                    messages.push(ChatMessage::ToolResult {
                        call_id: payload.call_id,
                        content: msg.content.clone(),
                    });
                } else {
                    tracing::warn!(msg = msg.id, call = %payload.call_id, "孤儿 tool 行,跳过(防 400)");
                }
            }
            // 未知角色直接过滤(演化预留)
            _ => {}
        }
    }

    ChatRequest {
        system,
        messages,
        options: scene.options.clone(),
        tools: tool_defs.to_vec(),
        tool_choice: crate::llm::ToolChoice::Auto,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scenes::Scenes;

    fn msg(id: i64, role: &str, content: &str, payload: Option<&str>) -> Message {
        Message {
            id,
            conversation_id: 1,
            role: role.into(),
            content: content.into(),
            created_at: 0,
            payload: payload.map(Into::into),
        }
    }

    fn brief(id: i64, domain: &str, content: &str) -> Briefing {
        Briefing {
            id,
            domain: domain.into(),
            content: content.into(),
            scope: "home".into(),
            resident: true,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn laws_and_briefings_join_system_in_stable_order() {
        let scenes = Scenes::builtin();
        let scene = scenes.default_scene();
        let briefs =
            vec![brief(1, "appliance", "路由器在电视柜"), brief(2, "media", "电影在 NAS")];
        let req = build_context(scene, None,None, &[], &briefs, &[], &[]);

        // 法条紧跟 persona(底座纪律,人格中立)
        let laws_at = req.system.find("## 怎么记事").expect("法条必须进 system");
        assert!(laws_at > scene.persona.len() - 1, "法条在 persona 之后");
        assert!(req.system.contains("briefing_lookup"), "法条点名常驻基础工具");

        // 需知节:固定标题 + repo 给的稳定序原样保持
        let a = req.system.find("[appliance]").unwrap();
        let m = req.system.find("[media]").unwrap();
        assert!(req.system.find("## 任务需知").unwrap() < a && a < m);

        // 无需知 = 无该节,且前缀与再次构造字节级一致(golden)
        let none = build_context(scene, None,None, &[], &[], &[], &[]);
        assert!(!none.system.contains("## 任务需知"));
        assert_eq!(none.system, build_context(scene, None,None, &[], &[], &[], &[]).system);
    }

    #[test]
    fn anchor_is_zero_until_window_full() {
        for total in 0..=WINDOW_MAX {
            assert_eq!(anchored_start(total), 0);
        }
    }

    #[test]
    fn anchor_moves_in_whole_chunks_and_keeps_window_bounded() {
        let mut prev = 0;
        for total in (WINDOW_MAX + 1)..400 {
            let start = anchored_start(total);
            assert_eq!(start % WINDOW_CHUNK, 0, "锚点必须是整块倍数");
            let window = total - start;
            assert!(window <= WINDOW_MAX, "窗口超上限: total={total}");
            assert!(window > WINDOW_MAX - WINDOW_CHUNK, "窗口缩得过小: total={total}");
            assert!(start >= prev, "锚点只能前进");
            prev = start;
        }
    }

    #[test]
    fn anchor_stable_within_a_chunk_of_turns() {
        // 前缀稳定的本体:同一锚段内连续多轮,start 不变
        let base = anchored_start(WINDOW_MAX + 1);
        for total in (WINDOW_MAX + 1)..(WINDOW_MAX + WINDOW_CHUNK) {
            assert_eq!(anchored_start(total), base);
        }
    }

    #[test]
    fn tail_budget_noop_under_budget() {
        // 常态:普通短消息远在预算内 → 起点 0 → 与无安全阀等价(前缀缓存不破)。
        let h = vec![
            msg(1, "user", "你好", None),
            msg(2, "assistant", "嗨", None),
            msg(3, "user", "今天天气?", None),
        ];
        assert_eq!(tail_budget_start(&h), 0);
    }

    #[test]
    fn tail_budget_trims_old_rounds_when_over() {
        // 三轮,每轮一个超大 user(各 ~预算的 0.7)→ 总量超预算 → 丢最老、保最新。
        let big = "图".repeat(TAIL_BUDGET_CHARS * 7 / 10);
        let h = vec![
            msg(1, "user", &big, None), // 最老,应被丢
            msg(2, "assistant", "ok1", None),
            msg(3, "user", &big, None),
            msg(4, "assistant", "ok2", None),
            msg(5, "user", &big, None), // 最新一轮,必保
            msg(6, "assistant", "ok3", None),
        ];
        let start = tail_budget_start(&h);
        assert!(start > 0, "超预算必须裁");
        // 起点落在某条 user/event 边界上(或其前),保最后一轮完整
        assert!(start <= 4, "最后一轮(下标4起)必须整段保留");
        // 被保留段字数在预算内(末轮单独超标的情形除外,这里每轮 0.7×预算两轮就超)
        let kept: usize = h[start..]
            .iter()
            .map(|m| m.content.chars().count())
            .sum();
        assert!(kept <= TAIL_BUDGET_CHARS, "保留段应压在预算内: {kept}");
    }

    #[test]
    fn tail_budget_keeps_last_round_even_if_oversized() {
        // 不变量 ②:最后一轮自己就超预算也不裁(没法再省)→ 起点 = 末轮起点,不越过它。
        let huge = "字".repeat(TAIL_BUDGET_CHARS * 2);
        let h = vec![
            msg(1, "user", "旧", None),
            msg(2, "assistant", "旧答", None),
            msg(3, "user", &huge, None), // 末轮 user 单条就 2× 预算
            msg(4, "assistant", "答", None),
        ];
        // 末个 user/event = 下标 2(huge user) → 起点 = 2,不越过它(保末轮 user 头)
        assert_eq!(tail_budget_start(&h), 2);
    }

    #[test]
    fn cap_messages_noop_under_budget() {
        // 常态:短消息 → 不动 messages(前缀缓存不破)。
        let mut m = vec![
            ChatMessage::user("你好"),
            ChatMessage::assistant("嗨"),
            ChatMessage::user("放首歌"),
        ];
        let before = m.clone();
        cap_messages_tail(&mut m);
        assert_eq!(m.len(), before.len());
    }

    #[test]
    fn cap_messages_drops_old_rounds_and_snaps_to_user() {
        // 工具循环累积:老 user + 一坨超大 ToolResult,再来新 user → 超预算丢老的、首条保 User。
        let big = "果".repeat(TAIL_BUDGET_CHARS * 8 / 10);
        let mut m = vec![
            ChatMessage::user("旧任务"),
            ChatMessage::Assistant {
                content: String::new(),
                reasoning: None,
                tool_calls: vec![],
                reasoning_state: None,
            },
            ChatMessage::ToolResult { call_id: "c1".into(), content: big.clone() },
            ChatMessage::user("新任务"),
            ChatMessage::ToolResult { call_id: "c2".into(), content: big },
        ];
        cap_messages_tail(&mut m);
        // 首条必须是 User(无孤儿 ToolResult);且最后一条 User「新任务」保留。
        assert!(matches!(m.first(), Some(ChatMessage::User { .. })), "首条须为 User");
        assert!(
            m.iter().any(|x| matches!(x, ChatMessage::User { content, .. } if content == "新任务")),
            "当前任务 user 必保"
        );
        assert!(
            !m.iter().any(|x| matches!(x, ChatMessage::User { content, .. } if content == "旧任务")),
            "超预算的老轮应被丢"
        );
    }

    #[test]
    fn build_context_is_deterministic_and_filters_unknown_roles() {
        let scenes = Scenes::builtin();
        let scene = scenes.default_scene();
        let memories = vec![];
        let history = vec![
            msg(1, "user", "你好", None),
            msg(2, "alien", "{}", None),
            msg(3, "assistant", "汪!", None),
        ];
        let a = build_context(scene, None,None, &memories, &[], &history, &[]);
        let b = build_context(scene, None,None, &memories, &[], &history, &[]);
        assert_eq!(a.system, b.system, "同输入必须同前缀(golden)");
        let n_few = scene.few_shots.len();
        assert_eq!(a.messages.len(), n_few + 2, "未知角色被过滤,few-shot 打头");
        assert_eq!(a.messages[..n_few], scene.few_shots[..], "few-shot 原样在前");
        assert_eq!(a.messages[n_few], ChatMessage::user("你好"));
        assert_eq!(b.messages.len(), a.messages.len());
    }

    #[test]
    fn user_style_joins_prefix_between_persona_and_memories() {
        let scenes = Scenes::builtin();
        let scene = scenes.default_scene();
        let mem = crate::store::Memory {
            id: 1, user_id: 1, kind: "fact".into(), content: "对花生过敏".into(),
            resident: true, salience: 1.0, source: "explicit".into(), last_used_at: None,
            created_at: 0, updated_at: 0,
        };
        let req = build_context(scene, None,Some(" 贫嘴但靠谱,偶尔冒东北话 "), &[mem], &[], &[], &[]);
        let style_at = req.system.find("贫嘴但靠谱").expect("性格设定必须进 system");
        let mem_at = req.system.find("花生").expect("记忆必须进 system");
        assert!(style_at > scene.persona.len(), "性格设定在出厂人设之后(叠加在中性底座上)");
        assert!(style_at < mem_at, "性格设定在记忆之前(按稳定度排序保前缀)");
        // 空白设定 = 不注入,前缀与无设定时字节级一致(用户清空 = 纯出厂人设)
        let none = build_context(scene, None,None, &[], &[], &[], &[]);
        let blank = build_context(scene, None,Some("   "), &[], &[], &[], &[]);
        assert_eq!(none.system, blank.system);

        // 出厂默认 = 空:默认保持中性,不注入"性格设定"层 → 与无设定字节级一致
        // (想要性格的用户自己在设置里写;留空是产品决策,见 DEFAULT_PERSONA_STYLE 注释)
        assert!(DEFAULT_PERSONA_STYLE.trim().is_empty(), "默认人设保持中性:默认句留空,不预设性格倾向");
        let dflt = build_context(scene, None,Some(DEFAULT_PERSONA_STYLE), &[], &[], &[], &[]);
        assert_eq!(dflt.system, none.system, "默认句为空 = 纯出厂人设(不注入性格层)");
    }

    #[test]
    fn user_name_fills_persona_placeholder() {
        let scenes = Scenes::builtin();
        let scene = scenes.default_scene();
        // 设了名字 = 直接填进 persona 的「你是 {name}」身份句,逐字出现、无残留占位符
        let named = build_context(scene, Some(" 小布 "), None, &[], &[], &[], &[]);
        assert!(named.system.contains("你是 小布"), "名字直接进 persona 身份句");
        assert!(!named.system.contains("{name}"), "占位符必须被替换干净");
        // 没设/空白 = 回落出厂默认名,与显式传默认名字节级一致(前缀稳定、出厂 persona 原样)
        let none = build_context(scene, None, None, &[], &[], &[], &[]);
        let blank = build_context(scene, Some("   "), None, &[], &[], &[], &[]);
        assert_eq!(none.system, blank.system, "空名回落默认,前缀字节稳定");
        assert!(none.system.contains(&format!("你是 {DEFAULT_NAME}")), "默认名填入身份句");
        assert!(!none.system.contains("{name}"), "默认也不留占位符");
    }

    /// 肉眼检查口:LLM 实际看到的全貌(system / few-shot+历史 / tools)。
    /// 跑法:cargo test -p larkwing-core dump_prompt -- --ignored --nocapture
    #[test]
    #[ignore = "调试用,手动跑"]
    fn dump_prompt_for_eyeballing() {
        let scenes = Scenes::builtin();
        let scene = scenes.default_scene();
        let tools = crate::tools::Tools::builtin();
        let defs: Vec<_> = tools.subset(&scene.tools).iter().map(|t| t.spec().def()).collect();
        let mem = |id: i64, content: &str| crate::store::Memory {
            id, user_id: 1, kind: "fact".into(), content: content.into(),
            resident: true, salience: 1.0, source: "explicit".into(), last_used_at: None,
            created_at: 0, updated_at: 0,
        };
        let memories = vec![mem(1, "用户不吃香菜"), mem(2, "用户对花生过敏")];
        let briefs = vec![brief(1, "media", "电影在 \\\\nas\\film;动画片在 \\\\nas\\kids")];
        let history = vec![
            msg(1, "user", "今晚吃什么好", None),
            msg(2, "assistant", "番茄锅怎么样?不放香菜,蘸料也帮你避开花生~", None),
        ];
        let req = build_context(scene, None,Some(DEFAULT_PERSONA_STYLE), &memories, &briefs, &history, &defs);
        println!("\n========== system(OpenAI 系翻成首条 system 消息) ==========\n{}", req.system);
        println!("\n========== messages = few-shot(稳定前缀) + 锚定窗口历史 ==========");
        for m in &req.messages {
            println!("{}", serde_json::to_string(m).unwrap());
        }
        println!("\n========== tools(白名单定义) ==========\n{}",
            serde_json::to_string_pretty(&req.tools).unwrap());
    }

    #[test]
    fn speak_marked_user_rows_get_voice_prefix_deterministically() {
        let scenes = Scenes::builtin();
        let scene = scenes.default_scene();
        let history = vec![
            msg(1, "user", "现在几点", Some(r#"{"input":"mic","speak":true}"#)),
            msg(2, "assistant", "三点一刻啦", None),
            msg(3, "user", "谢啦", None),
        ];
        let req = build_context(scene, None,None, &[], &[], &history, &[]);
        let tail = &req.messages[scene.few_shots.len()..];
        assert_eq!(tail[0], ChatMessage::user("〔语音〕现在几点"), "speak 行加标记");
        assert_eq!(tail[2], ChatMessage::user("谢啦"), "无标记照旧,历史零膨胀");
        // 说话守则住底座 LAWS(静态):模式怎么翻转都不碰前缀
        assert!(req.system.contains("## 说话守则"));
        let again = build_context(scene, None,None, &[], &[], &history, &[]);
        assert_eq!(req.messages, again.messages, "同输入同翻译(前缀稳定 golden)");
    }

    #[test]
    fn window_snaps_to_user_boundary_and_replays_tool_rounds() {
        let scenes = Scenes::builtin();
        let scene = scenes.default_scene();
        // 窗口起点落在工具轮中间:行 1(孤儿 tool)必须被吸附/跳过
        let history = vec![
            msg(1, "tool", "ok", Some(r#"{"call_id":"call_z","name":"now","status":"ok"}"#)),
            msg(2, "user", "今天几号", None),
            msg(
                3,
                "assistant",
                "",
                Some(r#"{"tool_calls":[{"id":"call_a","name":"now","args":{}}],"reasoning":"查一下"}"#),
            ),
            msg(4, "tool", r#"{"now":"2026-06-12 21:00"}"#, Some(r#"{"call_id":"call_a","name":"now","status":"ok"}"#)),
            msg(5, "assistant", "今天 6 月 12 号~", None),
        ];
        let req = build_context(scene, None,None, &[], &[], &history, &[]);
        let tail = &req.messages[scene.few_shots.len()..];
        assert_eq!(tail.len(), 4, "孤儿 tool 行被吸附掉");
        assert_eq!(tail[0], ChatMessage::user("今天几号"));
        match &tail[1] {
            ChatMessage::Assistant { tool_calls, reasoning, .. } => {
                assert_eq!(tool_calls[0].id, "call_a");
                assert_eq!(reasoning.as_deref(), Some("查一下"), "坑 #4:工具轮 reasoning 回放");
            }
            other => panic!("应是带 tool_calls 的 Assistant,实际 {other:?}"),
        }
        assert!(matches!(&tail[2], ChatMessage::ToolResult { call_id, .. } if call_id == "call_a"));
    }

    #[test]
    fn attach_ambient_appends_to_last_user_and_spares_prefix() {
        let scenes = Scenes::builtin();
        let scene = scenes.default_scene();
        let history = vec![msg(1, "user", "放点啥", None)];
        let mut req = build_context(scene, None, None, &[], &[], &history, &[]);
        let sys_before = req.system.clone();
        let n_before = req.messages.len();
        attach_ambient(&mut req, "播放器现在空闲,没有在播放任何内容");
        // 稳定前缀(system)绝不被碰 → 前缀缓存不破;也不新增消息(挂到末条 user 上)
        assert_eq!(req.system, sys_before, "ambient 注入绝不动 system 前缀");
        assert_eq!(req.messages.len(), n_before, "ambient 不新增消息,只追加到末条 user");
        match req.messages.last() {
            Some(ChatMessage::User { content, .. }) => {
                assert!(content.starts_with("放点啥"), "用户原文在前");
                assert!(content.contains("〔此刻 · 播放器现在空闲"), "末尾挂上此刻状态标记");
            }
            other => panic!("末条应是 user,实际 {other:?}"),
        }
    }
}
