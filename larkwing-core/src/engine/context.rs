//! ContextBuilder:全系统唯一知道"prompt 长什么样"的地方(单一装配权)。
//! 纯函数、无 IO;原料(persona/记忆/历史/工具定义)由回合管线从 store 取来递入。
//! 核心不变量 = 前缀稳定:稳定层在前(persona + 画像记忆 + few-shot),易变尾在后
//! (锚定窗口内的最近消息)。

use std::collections::HashSet;

use crate::llm::{ChatMessage, ChatRequest, ToolDef};
use crate::scenes::Scene;
use crate::store::{Briefing, Memory, Message};

use super::{AssistantPayload, ToolRowPayload, UserMeta};

/// persona.style 的出厂默认句(用户没动过时生效;清空 = 纯出厂人设,不注入)。
/// 它是出厂人设的一句话摘要 —— 让用户看见"可以这样改",兼作前缀里的人设锚点。
/// 与前端 useSettings DEFAULTS['persona.style'] 手工同步(两边各一行,改要一起改)。
pub(super) const DEFAULT_PERSONA_STYLE: &str =
    "暖心又好奇的小机灵,偶尔「滴——」一声卖萌,永远向着这个家";

/// 运行时法条(PLAN §9 提示词蓝图):**人格中立底座**的一部分 —— 通用行为纪律,
/// 与性格无关,住这里不住场景数据;第二个场景出现时自动继承。
/// 点名的工具全部是常驻基础工具(tools::BASE_TOOLS),每场景必在。静态 → 字节稳定吃缓存。
pub(super) const LAWS: &str = "\
## 怎么记事(两本账)
用户告诉你的事,分三种处理:
- 关于「人」的长期事实(名字、家人、喜好、忌口、纪念日)→ 用 remember 记小本本。
- 关于「这个家的环境」(资源放在哪、目录路径、设备、家里的惯例)→ 用 briefing_write 记家庭备忘;同一主题重写会整体覆盖,用户说「换了/搬了/不对」就把该主题完整的新状态重写一遍,不要叠新条目。
- 情绪、闲聊、一次性安排 → 不记。拿不准就先不记:记错了用户能删,乱记很烦人。

## 用你知道的,别反问
下方「你记得关于用户的这些事」和「任务需知」里写着的,就是你已经知道的,直接用——比如任务需知里有电影目录,用户说「放个电影」就直接去目录里找,绝不反问「电影放在哪」。像是家里登记过、但下方没写的,先用 briefing_lookup 查一遍,查不到再说不知道。

## 说人话
工具安静地用,结果自然织进话里,不念原始数据。记完用一句话确认你理解的内容,让用户有机会当场纠正。永远不向用户提「工具」「调用」「数据库」「需知」这些词;「小本本」可以说——那是用户看得见的本子。

## 说话守则
用户消息开头带〔语音〕= 这一轮你是在跟人**说话**,不是写字:短句口语,先说结论;不用表格、列表、代码块、链接和任何 Markdown 记号;数字和时间说人话(「三点一刻」而不是「15:15」);内容多就挑两三个要点说,再问要不要继续。没有〔语音〕= 正常排版,该用列表用列表。〔语音〕是系统加的输入形态标记,不是用户打的字,绝不复述它。

## 不是每句话都冲你来
〔语音〕回合里,如果听到的内容明显不是对你说的——电视的声音、家里人互相聊天、没头没尾的环境碎片——就只回 __IGNORE__ 这一个词,什么都不解释。拿不准的就当是对你说的,正常回应。

## 出厂示范
对话最开头以【示范】开头的几段是出厂教学样例:人物、称呼、事实全部是虚构的,与眼前这位用户无关,绝不当作用户信息来引用或称呼;真实对话从示范之后开始。";

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

pub(super) fn build_context(
    scene: &Scene,
    user_style: Option<&str>,
    memories: &[Memory],
    briefings: &[Briefing],
    history: &[Message],
    tool_defs: &[ToolDef],
) -> ChatRequest {
    // 稳定层:persona(性格)→ 法条(底座纪律)→ 性格设定 → 记忆 → 任务需知
    // (按稳定度降序排,全部进缓存前缀)
    let mut system = String::with_capacity(scene.persona.len() + LAWS.len() + 256);
    system.push_str(&scene.persona);
    system.push_str("\n\n");
    system.push_str(LAWS);
    // 用户的一句话性格设定:放在出厂人设之后 = 冲突时用户说了算(它是这家人的 7274)
    if let Some(style) = user_style.map(str::trim).filter(|s| !s.is_empty()) {
        system.push_str("\n\n## 这个家给你的性格设定(与上面冲突时,以这里为准)\n");
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

    // 锚点吸附 user/event 边界:窗口起点若落在工具轮中间,向后推进到第一条回合起点行,
    // 保证窗口内 tool_call/result 永远成对(吸附量 ≤ 一个工具轮,有界)。
    // event 也是合法回合起点(任务专属会话里没有 user 行)。
    let first_user = history
        .iter()
        .position(|m| m.role == "user" || m.role == "event")
        .unwrap_or(0);
    let history = &history[first_user..];

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
        let req = build_context(scene, None, &[], &briefs, &[], &[]);

        // 法条紧跟 persona(底座纪律,人格中立)
        let laws_at = req.system.find("## 怎么记事").expect("法条必须进 system");
        assert!(laws_at > scene.persona.len() - 1, "法条在 persona 之后");
        assert!(req.system.contains("briefing_lookup"), "法条点名常驻基础工具");

        // 需知节:固定标题 + repo 给的稳定序原样保持
        let a = req.system.find("[appliance]").unwrap();
        let m = req.system.find("[media]").unwrap();
        assert!(req.system.find("## 任务需知").unwrap() < a && a < m);

        // 无需知 = 无该节,且前缀与再次构造字节级一致(golden)
        let none = build_context(scene, None, &[], &[], &[], &[]);
        assert!(!none.system.contains("## 任务需知"));
        assert_eq!(none.system, build_context(scene, None, &[], &[], &[], &[]).system);
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
    fn build_context_is_deterministic_and_filters_unknown_roles() {
        let scenes = Scenes::builtin();
        let scene = scenes.default_scene();
        let memories = vec![];
        let history = vec![
            msg(1, "user", "你好", None),
            msg(2, "alien", "{}", None),
            msg(3, "assistant", "汪!", None),
        ];
        let a = build_context(scene, None, &memories, &[], &history, &[]);
        let b = build_context(scene, None, &memories, &[], &history, &[]);
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
            created_at: 0, updated_at: 0,
        };
        let req = build_context(scene, Some(" 贫嘴但靠谱,偶尔冒东北话 "), &[mem], &[], &[], &[]);
        let style_at = req.system.find("贫嘴但靠谱").expect("性格设定必须进 system");
        let mem_at = req.system.find("花生").expect("记忆必须进 system");
        assert!(style_at > scene.persona.len(), "性格设定在出厂人设之后(冲突时用户优先)");
        assert!(style_at < mem_at, "性格设定在记忆之前(按稳定度排序保前缀)");
        // 空白设定 = 不注入,前缀与无设定时字节级一致(用户清空 = 纯出厂人设)
        let none = build_context(scene, None, &[], &[], &[], &[]);
        let blank = build_context(scene, Some("   "), &[], &[], &[], &[]);
        assert_eq!(none.system, blank.system);

        // 出厂默认句:得在后端长度上限内,且注入后逐字出现
        assert!(DEFAULT_PERSONA_STYLE.chars().count() <= 100, "默认句超 persona.style 上限");
        let dflt = build_context(scene, Some(DEFAULT_PERSONA_STYLE), &[], &[], &[], &[]);
        assert!(dflt.system.contains(DEFAULT_PERSONA_STYLE));
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
            created_at: 0, updated_at: 0,
        };
        let memories = vec![mem(1, "用户不吃香菜"), mem(2, "用户对花生过敏")];
        let briefs = vec![brief(1, "media", "电影在 \\\\nas\\film;动画片在 \\\\nas\\kids")];
        let history = vec![
            msg(1, "user", "今晚吃什么好", None),
            msg(2, "assistant", "番茄锅怎么样?不放香菜,蘸料也帮你避开花生~", None),
        ];
        let req = build_context(scene, Some(DEFAULT_PERSONA_STYLE), &memories, &briefs, &history, &defs);
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
        let req = build_context(scene, None, &[], &[], &history, &[]);
        let tail = &req.messages[scene.few_shots.len()..];
        assert_eq!(tail[0], ChatMessage::user("〔语音〕现在几点"), "speak 行加标记");
        assert_eq!(tail[2], ChatMessage::user("谢啦"), "无标记照旧,历史零膨胀");
        // 说话守则住底座 LAWS(静态):模式怎么翻转都不碰前缀
        assert!(req.system.contains("## 说话守则"));
        let again = build_context(scene, None, &[], &[], &history, &[]);
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
        let req = build_context(scene, None, &[], &[], &history, &[]);
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
}
