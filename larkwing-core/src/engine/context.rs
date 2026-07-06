//! ContextBuilder:全系统唯一知道"prompt 长什么样"的地方(单一装配权)。
//! 纯函数、无 IO;原料(persona/记忆/历史/工具定义)由回合管线从 store 取来递入。
//! 核心不变量 = 前缀稳定:稳定层在前(persona + 画像记忆 + few-shot),易变尾在后
//! (锚定窗口内的最近消息)。

use std::collections::{HashMap, HashSet};

use crate::llm::{ChatMessage, ChatRequest, ToolDef};
use crate::scenes::Scene;
use crate::store::{Briefing, Memory, Message, Todo};

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

## 一家人
用户消息开头带〔某某说〕= 这句话是家里的「某某」说的(从手机渠道或语音过来);没有这个标记 = 平时这位用户本人。谁在说话,「我」就指谁:TA 说「提醒我」「我喜欢…」,要记的、要办的都是 TA 自己的事,别记到别人头上。回应时自然称呼说话的人就好;这个标记是系统加的身份注记,不是用户打的字,绝不复述它。

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

/// 历史装载的 I/O 上界(条):caller 一次最多从库取这么多最近消息,真正的窗口由下面的
/// **字数预算**在其中裁定。远大于任何字数预算能留的条数 → 只防"超长会话每轮拉上万行",
/// 不构成语义窗口(原 `WINDOW_MAX=48` 的条数窗口已废,改由字数预算 + model-aware 决定)。
pub(super) const HISTORY_PAGE_MAX: usize = 800;
/// 窗口起点的整块推进粒度(条)。起点只按 `WINDOW_CHUNK` 的**绝对**倍数跳 —— 一个 chunk 内
/// 字节稳定吃 DeepSeek 自动前缀缓存(原 anchored_start 的稳定性机制,现由字数预算驱动)。
pub(super) const WINDOW_CHUNK: usize = 16;

/// 尾部字数预算:**未知窗口**(本地/未登记模型)的回落值。= 历史值 → 对这些模型零行为变化。
pub(super) const DEFAULT_TAIL_BUDGET_CHARS: usize = 48_000;
/// 大窗口模型的字数预算上限(≈30 万字):装得下文档 + 多轮,又不至于每轮重建过大。
pub(super) const MAX_TAIL_BUDGET_CHARS: usize = 300_000;
/// 预算 = 上下文窗口的 1/N(其余 1−1/N 留给稳定前缀 + 输出 + thinking)。
pub(super) const TAIL_RESERVE_DEN: u32 = 2;

/// 由模型上下文窗口(token)+ 计价方式算尾部字数预算(§0.2.0 安全阀 model-aware + 计价感知)。
/// - 窗口:`None`(本地/未登记)→ 保守 `DEFAULT`;`Some(w)` → `min(MAX, w / TAIL_RESERVE_DEN)`
///   (CJK 最坏 ~1 token/字 → token 数当字数上界 = 安全。小窗口缩防溢出;大窗口在 [默认, MAX] 间装文档)。
/// - 计价方式:**无缓存**(每轮全价重发尾巴)→ 封到 `DEFAULT`、少留勤压;**有缓存 / 按次**
///   (重用便宜 / token 不计较)→ 按窗口放大(常态)。
/// 只缩或在 [默认, MAX] 间放大,绝不无界增长。起步值,真用可调(§13.7「只能真用才能调」)。
pub(super) fn tail_budget_chars(
    window_tokens: Option<u32>,
    billing: crate::llm::catalog::BillingMode,
) -> usize {
    let base = match window_tokens {
        None => DEFAULT_TAIL_BUDGET_CHARS,
        Some(w) => ((w / TAIL_RESERVE_DEN) as usize).min(MAX_TAIL_BUDGET_CHARS),
    };
    match billing {
        // 无缓存:重发整条尾巴每轮全价 → 别留超过默认(小窗口已被 base 缩得更小)。
        crate::llm::catalog::BillingMode::Uncached => base.min(DEFAULT_TAIL_BUDGET_CHARS),
        // 有缓存 / 按次:多留(装文档、保连续性),代价走缓存 / 与 token 无关。
        crate::llm::catalog::BillingMode::Cached | crate::llm::catalog::BillingMode::PerCall => base,
    }
}

/// 历史窗口:返回 `history` 中保留窗口的起始下标(已吸附到 user/event 回合边界)。
/// 统一了原 `anchored_start`(整块锚定保缓存)+ `tail_budget_start`(字数封顶防溢出)+ 边界吸附:
/// - `base` = `history[0]` 在整段会话里的**绝对**下标(给整块锚定一个稳定参照,缓存才稳)。
/// - 常态(总字数 ≤ `budget`)→ 返回 0(留全部 page),**前缀缓存零损伤**(绝大多数回合走这)。
/// - 超预算 → 起点前进到第一个「`WINDOW_CHUNK` 的绝对倍数 且 其后字数 ≤ budget」处;total 在一个
///   chunk 内增长时该绝对起点不变 → 字节稳定(原 anchored 的稳定性,现以字数为触发)。
/// - 不变量:永不裁掉最后一轮(末个 user/event 起点之后整段恒保留,哪怕它自己就超预算——
///   没法再省,交 provider / per-tool 上限兜)。
pub(super) fn windowed_start(history: &[Message], base: usize, budget: usize) -> usize {
    if history.is_empty() {
        return 0;
    }
    let chars = |m: &Message| {
        m.content.chars().count() + m.payload.as_deref().map_or(0, |p| p.chars().count())
    };
    let total: usize = history.iter().map(&chars).sum();
    if total <= budget {
        return 0; // 常态:留全部 → 前缀缓存稳定
    }
    // 末轮起点(末个 user/event 边界):窗口起点绝不越过它(不变量)。
    let last_round =
        history.iter().rposition(|m| m.role == "user" || m.role == "event").unwrap_or(0);
    // 从前往后找第一个「绝对下标是 WINDOW_CHUNK 倍数 且 其后字数 ≤ budget」的起点。
    // suffix = chars(history[i..]) 单调递减;首个满足者即「装得下的最大窗口」的对齐起点。
    let mut suffix = total;
    let mut chosen = last_round; // 兜底:至少保末轮
    let mut i = 0usize;
    while i <= last_round {
        if (base + i) % WINDOW_CHUNK == 0 && suffix <= budget {
            chosen = i;
            break;
        }
        suffix -= chars(&history[i]);
        i += 1;
    }
    // 吸附到 user/event 边界(对齐点可能落在工具轮中间 → 向后推到回合起点行),不越过末轮。
    let snap = history[chosen..=last_round]
        .iter()
        .position(|m| m.role == "user" || m.role == "event")
        .unwrap_or(0);
    (chosen + snap).min(last_round)
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
pub(super) fn cap_messages_tail(messages: &mut Vec<ChatMessage>, budget: usize) {
    let total: usize = messages.iter().map(message_chars).sum();
    if total <= budget {
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
        if acc > budget {
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

/// 主动关怀·对话跟进(★主动关怀里程碑 切片2):受 `care.enabled` 收口的一段**克制倾向** ——
/// 开着才进前缀(关掉整段不进,前缀随之变;设置很少动,可接受)。人格中立(§5):是"偶尔顺口
/// 关心一句"的行为纪律、不是性格。分寸靠强约束 + eval 反例守(闲聊别硬塞)。**跨会话由头依赖记忆
/// 是否记下**(§13;松动"宁缺毋滥"要另议)——本段先吃"已记住的事 + 当前会话里没了结的话头"。
/// §6.6:不硬编助手名(全用第二人称"你")。
const CARE_FOLLOWUP: &str = "\n\n## 主动关心(自然、克制)\n\
你记得用户提过、还没了结的事 —— 想做 / 想买 / 想去的打算,或之前聊到、牵挂的近况。\
话题自然又相关时,你可以顺口轻轻关心一句进展;但这是锦上添花、不是任务:\
绝不每轮都提、绝不追着问、绝不硬把话题拐过去,用户没接住就自然放下、别重复。\
没有真正相关又值得一提的,就正常聊、什么都不提。";

/// 进前缀的"未了的事"条数上限(★主动关怀 切片2·B):有限量、不让待办把前缀撑大(§4.8)。
/// 起步值,真用可调(§13.7)。
pub(super) const TODO_PREFIX_LIMIT: usize = 5;

pub(super) fn build_context(
    scene: &Scene,
    user_name: Option<&str>,
    user_style: Option<&str>,
    care_enabled: bool,
    memories: &[Memory],
    briefings: &[Briefing],
    todos: &[Todo],
    history: &[Message],
    history_base: usize,
    budget: usize,
    tool_defs: &[ToolDef],
    speakers: &HashMap<i64, String>,
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
    // 主动关怀·对话跟进(切片2):受 care.enabled 收口的克制倾向;关掉即整段不进前缀(前缀随设置变)。
    if care_enabled {
        system.push_str(CARE_FOLLOWUP);
    }
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
    // 未了的事(★主动关怀 切片2·B):开着关怀 + 有开着的待办才进(list_open 已限量);办完让模型用
    // finish_todo 了结(§3.5 不静默,了结后不再露面)。跟进的**分寸**在 CARE_FOLLOWUP,这里只摆数据。
    if care_enabled && !todos.is_empty() {
        system.push_str(
            "\n\n## 你还惦记着帮 TA 留意的事(合适时顺口关心进展;TA 说做完 / 不做了就用 finish_todo 了结)\n",
        );
        for t in todos {
            system.push_str("- ");
            system.push_str(&t.content);
            system.push('\n');
        }
    }

    // 历史窗口:字数预算(model-aware,防溢出 + 装文档)+ 整块锚定(保前缀缓存)+ 边界吸附
    // (起点落在工具轮中间则向后推到回合起点,保 tool_call/result 成对,防 OpenAI 系 400)。
    // 常态字数 ≤ 预算 → start=0 → 与无窗口等价、缓存零损伤。统一了原 anchored_start + 安全阀。
    let start = windowed_start(history, history_base, budget);
    let history = &history[start..];

    // few-shot 在真实历史之前:按场景静态 → 字节级稳定 → 吃前缀缓存(PLAN §8)
    let mut messages: Vec<ChatMessage> = scene.few_shots.clone();

    // 易变尾:锚定窗口内的最近消息(store::Message → llm::ChatMessage 的映射只在这)。
    // 工具轮回放:assistant 行的 payload 带 tool_calls + reasoning(坑 #4:DeepSeek
    // 要求工具轮回传 reasoning),'tool' 行配对回放;孤儿 tool 行跳过(防 400)。
    let mut open_calls: HashSet<String> = HashSet::new();
    for msg in history {
        match msg.role.as_str() {
            // 语音会话模式(PLAN §11)+ 渠道归人:payload 里的形态/说话人翻成确定性标记——
            // 同一消息每轮翻译结果一致 → 历史区字节稳定,前缀缓存零损伤;
            // 「说话守则」「一家人」法条按标记生效。speaker 名字查 speakers(id→名,单源 users 表,
            // caller 已排除会话归属者:主人自己不标);查不到(家人已删)= 不标,消息照常。
            "user" => {
                let meta = msg
                    .payload
                    .as_deref()
                    .and_then(|p| serde_json::from_str::<UserMeta>(p).ok())
                    .unwrap_or_default();
                let mut prefix = String::new();
                if meta.speak {
                    prefix.push_str("〔语音〕");
                }
                if let Some(name) = meta.speaker_user.and_then(|id| speakers.get(&id)) {
                    prefix.push_str(&format!("〔{name}说〕"));
                }
                let content = if prefix.is_empty() {
                    msg.content.clone()
                } else {
                    format!("{prefix}{}", msg.content)
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
            speaker_name: None,
            trigger: None,
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

    /// 测试便捷包装:history_base=0 + 宽预算(不触发裁剪)→ 专注前缀/渲染逻辑;
    /// 窗口裁剪的逻辑由 `windowed_start`/`tail_budget_chars` 的专项测试覆盖。
    fn bc(
        scene: &Scene,
        name: Option<&str>,
        style: Option<&str>,
        mems: &[Memory],
        briefs: &[Briefing],
        history: &[Message],
        tools: &[ToolDef],
    ) -> ChatRequest {
        build_context(
            scene,
            name,
            style,
            false, // care 跟进倾向:结构测试默认不带(on/off 由 care_followup_gated_by_flag 专项覆盖)
            mems,
            briefs,
            &[], // todos:结构测试默认不带(open_todos 段由专项测试覆盖)
            history,
            0,
            MAX_TAIL_BUDGET_CHARS,
            tools,
            &HashMap::new(),
        )
    }

    #[test]
    fn laws_and_briefings_join_system_in_stable_order() {
        let scenes = Scenes::builtin();
        let scene = scenes.default_scene();
        let briefs =
            vec![brief(1, "appliance", "路由器在电视柜"), brief(2, "media", "电影在 NAS")];
        let req = bc(scene, None, None, &[], &briefs, &[], &[]);

        // 法条紧跟 persona(底座纪律,人格中立)
        let laws_at = req.system.find("## 怎么记事").expect("法条必须进 system");
        assert!(laws_at > scene.persona.len() - 1, "法条在 persona 之后");
        assert!(req.system.contains("briefing_lookup"), "法条点名常驻基础工具");

        // 需知节:固定标题 + repo 给的稳定序原样保持
        let a = req.system.find("[appliance]").unwrap();
        let m = req.system.find("[media]").unwrap();
        assert!(req.system.find("## 任务需知").unwrap() < a && a < m);

        // 无需知 = 无该节,且前缀与再次构造字节级一致(golden)
        let none = bc(scene, None, None, &[], &[], &[], &[]);
        assert!(!none.system.contains("## 任务需知"));
        assert_eq!(none.system, bc(scene, None, None, &[], &[], &[], &[]).system);
    }

    #[test]
    fn care_followup_gated_by_flag() {
        let scenes = Scenes::builtin();
        let scene = scenes.default_scene();
        // care 开 → 克制跟进倾向进前缀
        let on = build_context(
            scene, None, None, true, &[], &[], &[], &[], 0, MAX_TAIL_BUDGET_CHARS, &[],
            &HashMap::new(),
        );
        assert!(on.system.contains("## 主动关心"), "care 开 → 跟进倾向进前缀");
        // care 关 → 整段不进(bc 默认 false)
        let off = bc(scene, None, None, &[], &[], &[], &[]);
        assert!(!off.system.contains("## 主动关心"), "care 关 → 不进前缀");
    }

    #[test]
    fn open_todos_shown_only_when_care_on() {
        let scenes = Scenes::builtin();
        let scene = scenes.default_scene();
        let todos = vec![Todo { id: 1, content: "给妈妈买生日礼物".into(), created_at: 0 }];
        // care 开 + 有待办 → 段进前缀、内容在
        let on = build_context(
            scene, None, None, true, &[], &[], &todos, &[], 0, MAX_TAIL_BUDGET_CHARS, &[],
            &HashMap::new(),
        );
        assert!(on.system.contains("还惦记着") && on.system.contains("给妈妈买生日礼物"));
        // care 关 → 不进(哪怕有待办)
        let off = build_context(
            scene, None, None, false, &[], &[], &todos, &[], 0, MAX_TAIL_BUDGET_CHARS, &[],
            &HashMap::new(),
        );
        assert!(!off.system.contains("还惦记着"));
    }

    #[test]
    fn tail_budget_scales_by_window() {
        use crate::llm::catalog::BillingMode::{Cached, PerCall, Uncached};
        // 未知窗口(本地/未登记)→ 回落默认(零行为变化)
        assert_eq!(tail_budget_chars(None, Cached), DEFAULT_TAIL_BUDGET_CHARS);
        // 大窗口 → 在 [默认, 上限] 间放大(装文档);1M → 取 MAX 封顶
        assert_eq!(tail_budget_chars(Some(1_000_000), Cached), MAX_TAIL_BUDGET_CHARS);
        assert_eq!(tail_budget_chars(Some(200_000), Cached), 100_000); // 200K/2,未触顶
        // 小窗口 → 缩到默认以下(防溢出);8K 本地模型 → 4K
        assert_eq!(tail_budget_chars(Some(8_000), Cached), 4_000);
        assert!(tail_budget_chars(Some(64_000), Cached) < DEFAULT_TAIL_BUDGET_CHARS, "64K 窗口预算应缩到默认以下");
        // 计价感知:无缓存 → 大窗口也封到默认(少留勤压);按次 = 同有缓存(多留)
        assert_eq!(tail_budget_chars(Some(1_000_000), Uncached), DEFAULT_TAIL_BUDGET_CHARS, "无缓存封顶默认");
        assert_eq!(tail_budget_chars(Some(1_000_000), PerCall), MAX_TAIL_BUDGET_CHARS, "按次 = 多留");
        // 小窗口下无缓存不会反而放大:仍取较小者
        assert_eq!(tail_budget_chars(Some(8_000), Uncached), 4_000, "小窗口无缓存仍按窗口缩");
    }

    #[test]
    fn windowed_start_noop_under_budget() {
        // 常态:短消息远在预算内 → 起点 0 → 与无窗口等价(前缀缓存不破)。
        let h = vec![
            msg(1, "user", "你好", None),
            msg(2, "assistant", "嗨", None),
            msg(3, "user", "今天天气?", None),
        ];
        assert_eq!(windowed_start(&h, 0, DEFAULT_TAIL_BUDGET_CHARS), 0);
    }

    #[test]
    fn windowed_start_trims_over_budget_to_chunk_boundary() {
        // 40 条(20 轮)小消息,总量约 2× 预算 → 必须裁到预算内。
        let budget = 4_000usize;
        let per = "字".repeat(budget / 10); // 每条约 0.1 预算 → 20 轮约 2× 预算
        let mut h = Vec::new();
        for i in 0..20 {
            h.push(msg(2 * i + 1, "user", &per, None));
            h.push(msg(2 * i + 2, "assistant", &per, None));
        }
        let start = windowed_start(&h, 0, budget);
        assert!(start > 0, "超预算必须裁");
        // 起点对齐 WINDOW_CHUNK 的绝对倍数(base=0)或吸附后落在末轮(边界兜底)
        let last_round = h.iter().rposition(|m| m.role == "user").unwrap();
        assert!(start % WINDOW_CHUNK == 0 || start == last_round, "起点应整块对齐或为末轮");
        // 起点落在 user 边界(不劈开工具配对/回合)
        assert_eq!(h[start].role, "user", "窗口起点须为回合起点(user)");
        // 保留段在预算内
        let kept: usize = h[start..].iter().map(|m| m.content.chars().count()).sum();
        assert!(kept <= budget, "保留段应压在预算内: {kept}");
    }

    #[test]
    fn windowed_start_keeps_last_round_even_if_oversized() {
        // 不变量:最后一轮自己就超预算也不裁 → 起点 = 末轮起点,不越过它。
        let huge = "字".repeat(10_000);
        let h = vec![
            msg(1, "user", "旧", None),
            msg(2, "assistant", "旧答", None),
            msg(3, "user", &huge, None), // 末轮 user 单条就远超预算
            msg(4, "assistant", "答", None),
        ];
        assert_eq!(windowed_start(&h, 0, 4_000), 2, "起点=末轮起点(下标2),保末轮 user 头");
    }

    #[test]
    fn windowed_start_advances_in_chunks_not_every_turn() {
        // 前缀缓存稳定的本体:模拟会话逐轮增长,起点应**按整块跳、不每轮 creep**。
        // 预算留 >> WINDOW_CHUNK 轮(真实预算的常态),起点才落在整块对齐点、有迟滞。
        let per = 10usize; // 每条 10 字 → 每轮 20 字
        let budget = 30 * per * 2; // 约留 30 轮(> WINDOW_CHUNK=16)
        let mut h = Vec::new();
        for i in 0..60 {
            // 先铺 60 轮(已超预算)
            h.push(msg(2 * i + 1, "user", &"字".repeat(per), None));
            h.push(msg(2 * i + 2, "assistant", &"字".repeat(per), None));
        }
        let mut starts = Vec::new();
        for i in 60..100 {
            // 再逐轮追加 40 轮,每轮记一次窗口起点(base=0:总数 < 页上界,与生产一致)
            h.push(msg(2 * i + 1, "user", &"字".repeat(per), None));
            h.push(msg(2 * i + 2, "assistant", &"字".repeat(per), None));
            starts.push(windowed_start(&h, 0, budget));
        }
        // 单调不减(锚点只前进)
        assert!(starts.windows(2).all(|w| w[0] <= w[1]), "锚点只能前进: {starts:?}");
        // 大多数相邻轮起点**相同**(整块迟滞 → 一个 chunk 内不动 → 缓存稳定;creep 的话几乎轮轮都变)
        let stable = starts.windows(2).filter(|w| w[0] == w[1]).count();
        assert!(stable > starts.len() / 2, "起点应按整块跳、不每轮 creep:仅 {stable}/{} 轮持平", starts.len() - 1);
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
        cap_messages_tail(&mut m, DEFAULT_TAIL_BUDGET_CHARS);
        assert_eq!(m.len(), before.len());
    }

    #[test]
    fn cap_messages_drops_old_rounds_and_snaps_to_user() {
        // 工具循环累积:老 user + 一坨超大 ToolResult,再来新 user → 超预算丢老的、首条保 User。
        let big = "果".repeat(DEFAULT_TAIL_BUDGET_CHARS * 8 / 10);
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
        cap_messages_tail(&mut m, DEFAULT_TAIL_BUDGET_CHARS);
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
        let a = bc(scene, None, None, &memories, &[], &history, &[]);
        let b = bc(scene, None, None, &memories, &[], &history, &[]);
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
        let req = bc(scene, None, Some(" 贫嘴但靠谱,偶尔冒东北话 "), &[mem], &[], &[], &[]);
        let style_at = req.system.find("贫嘴但靠谱").expect("性格设定必须进 system");
        let mem_at = req.system.find("花生").expect("记忆必须进 system");
        assert!(style_at > scene.persona.len(), "性格设定在出厂人设之后(叠加在中性底座上)");
        assert!(style_at < mem_at, "性格设定在记忆之前(按稳定度排序保前缀)");
        // 空白设定 = 不注入,前缀与无设定时字节级一致(用户清空 = 纯出厂人设)
        let none = bc(scene, None, None, &[], &[], &[], &[]);
        let blank = bc(scene, None, Some("   "), &[], &[], &[], &[]);
        assert_eq!(none.system, blank.system);

        // 出厂默认 = 空:默认保持中性,不注入"性格设定"层 → 与无设定字节级一致
        // (想要性格的用户自己在设置里写;留空是产品决策,见 DEFAULT_PERSONA_STYLE 注释)
        assert!(DEFAULT_PERSONA_STYLE.trim().is_empty(), "默认人设保持中性:默认句留空,不预设性格倾向");
        let dflt = bc(scene, None, Some(DEFAULT_PERSONA_STYLE), &[], &[], &[], &[]);
        assert_eq!(dflt.system, none.system, "默认句为空 = 纯出厂人设(不注入性格层)");
    }

    #[test]
    fn user_name_fills_persona_placeholder() {
        let scenes = Scenes::builtin();
        let scene = scenes.default_scene();
        // 设了名字 = 直接填进 persona 的「你是 {name}」身份句,逐字出现、无残留占位符
        let named = bc(scene, Some(" 小布 "), None, &[], &[], &[], &[]);
        assert!(named.system.contains("你是 小布"), "名字直接进 persona 身份句");
        assert!(!named.system.contains("{name}"), "占位符必须被替换干净");
        // 没设/空白 = 回落出厂默认名,与显式传默认名字节级一致(前缀稳定、出厂 persona 原样)
        let none = bc(scene, None, None, &[], &[], &[], &[]);
        let blank = bc(scene, Some("   "), None, &[], &[], &[], &[]);
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
        let req = bc(scene, None, Some(DEFAULT_PERSONA_STYLE), &memories, &briefs, &history, &defs);
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
        let req = bc(scene, None, None, &[], &[], &history, &[]);
        let tail = &req.messages[scene.few_shots.len()..];
        assert_eq!(tail[0], ChatMessage::user("〔语音〕现在几点"), "speak 行加标记");
        assert_eq!(tail[2], ChatMessage::user("谢啦"), "无标记照旧,历史零膨胀");
        // 说话守则住底座 LAWS(静态):模式怎么翻转都不碰前缀
        assert!(req.system.contains("## 说话守则"));
        let again = bc(scene, None, None, &[], &[], &history, &[]);
        assert_eq!(req.messages, again.messages, "同输入同翻译(前缀稳定 golden)");
    }

    #[test]
    fn speaker_marked_user_rows_get_name_prefix() {
        let scenes = Scenes::builtin();
        let scene = scenes.default_scene();
        let history = vec![
            // 渠道归人:指认过的家人(id=5)说的
            msg(1, "user", "提醒我明天买菜", Some(r#"{"speaker_user":5}"#)),
            msg(2, "assistant", "好嘞", None),
            // 语音 + 声纹同时命中:两个标记按序叠加
            msg(3, "user", "放首歌", Some(r#"{"input":"wake","speak":true,"speaker_user":5}"#)),
            // 家人已删(speakers 里没有 9):不标,消息照常
            msg(4, "user", "在吗", Some(r#"{"speaker_user":9}"#)),
            msg(5, "user", "我自己说的", None),
        ];
        let speakers = HashMap::from([(5i64, "妈妈".to_string())]);
        let req = build_context(
            scene,
            None,
            None,
            false,
            &[],
            &[],
            &[],
            &history,
            0,
            MAX_TAIL_BUDGET_CHARS,
            &[],
            &speakers,
        );
        let tail = &req.messages[scene.few_shots.len()..];
        assert_eq!(tail[0], ChatMessage::user("〔妈妈说〕提醒我明天买菜"));
        assert_eq!(tail[2], ChatMessage::user("〔语音〕〔妈妈说〕放首歌"), "语音在前、说话人在后");
        assert_eq!(tail[3], ChatMessage::user("在吗"), "查无此人(已删)不标");
        assert_eq!(tail[4], ChatMessage::user("我自己说的"), "无 speaker 照旧");
        // 「一家人」法条住底座 LAWS(静态,字节稳定)
        assert!(req.system.contains("## 一家人"));
    }

    #[test]
    fn window_snaps_to_user_boundary_and_replays_tool_rounds() {
        let scenes = Scenes::builtin();
        let scene = scenes.default_scene();
        // 历史以孤儿 tool 行开头(行 1):渲染时按 open_calls 配对跳过(防 400)。
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
        let req = bc(scene, None, None, &[], &[], &history, &[]);
        let tail = &req.messages[scene.few_shots.len()..];
        assert_eq!(tail.len(), 4, "孤儿 tool 行被跳过");
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
        let mut req = bc(scene, None, None, &[], &[], &history, &[]);
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
