//! 场景/人格 = 数据,不是代码插件(宪法 §5)。场景为内部偏置预设,用户不可见(铁律 §3.2):
//! 人格 + 开场白 + 工具白名单 + few-shot 示范对话。
//! MVP `include_str!` 编进二进制;以后要热加载再挪资源目录。

use std::collections::{HashMap, HashSet};

use anyhow::{bail, ensure, Result};
use serde::Deserialize;

use crate::llm::{ChatMessage, ChatOptions};
use crate::tools::Tools;

#[derive(Debug, Clone, Deserialize)]
pub struct Scene {
    pub id: String,
    pub name: String,
    /// 人格提示,ContextBuilder 拼进稳定前缀。
    pub persona: String,
    /// 开场白:会话还没消息时 UI 显示(引导式上手)。
    pub opening_line: String,
    /// 开场白的多语覆盖(locale → 文案);缺省回落 `opening_line`。开场白先于用户开口、
    /// 无对话语言可跟随,故按 ui.locale 出对应语言——这是 locale 唯一触达人格数据之处,
    /// 而非分叉整份人格(宪法 §5/§6:场景=数据,人格语言中立;对话语言由模型跟随用户)。
    #[serde(default)]
    pub openings: HashMap<String, String>,
    /// 场景级参数覆盖 —— ChatOptions 管道的合法用户之一。
    #[serde(default)]
    pub options: ChatOptions,
    /// 工具白名单(声明顺序即进 prompt 顺序,会话内稳定 → 前缀不抖)。
    #[serde(default)]
    pub tools: Vec<String>,
    /// few-shot 示范对话(PLAN §8):中立消息形状,拼装在 system 之后、真实历史之前。
    /// 作用 = 把不同供应商/模型引导到同一条工具使用路径;含反例(不该调工具的对话)。
    #[serde(default)]
    pub few_shots: Vec<ChatMessage>,
    /// 唤醒流程的短句话术(PLAN §11 C):**人格数据**(宪法 §5 人格中立底座——
    /// 应答词随人格走,换场景换话术);core 预合成成音频直出,不在底座写死任何一句。
    #[serde(default)]
    pub voice: SceneVoice,
}

/// 唤醒应答 / 没听清追问 / 告退语(两段式有声兜底,绝不静默失败)。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SceneVoice {
    #[serde(default)]
    pub wake_acks: Vec<String>,
    #[serde(default)]
    pub retry: Vec<String>,
    #[serde(default)]
    pub farewell: Vec<String>,
}

impl Scene {
    /// 按 ui.locale 取开场白;无对应语言则回落基础 `opening_line`(它就是默认语言版本)。
    pub fn opening_for(&self, locale: &str) -> String {
        self.openings
            .get(locale)
            .cloned()
            .unwrap_or_else(|| self.opening_line.clone())
    }

    /// few-shot 的体量:(段数, 字符数)。一段 = 一条 user 消息起头的一组;字符数 = 各消息正文 +
    /// 工具调用的名字与参数 + 工具结果(按字符数,不是字节 / token —— 只求一把不随序列化细节漂的尺子)。
    pub fn few_shot_size(&self) -> (usize, usize) {
        let mut segments = 0;
        let mut chars = 0;
        for msg in &self.few_shots {
            match msg {
                ChatMessage::User { content, .. } => {
                    segments += 1;
                    chars += content.chars().count();
                }
                ChatMessage::Assistant { content, tool_calls, .. } => {
                    chars += content.chars().count();
                    for c in tool_calls {
                        chars += c.name.chars().count() + c.args.to_string().chars().count();
                    }
                }
                ChatMessage::ToolResult { content, .. } => chars += content.chars().count(),
            }
        }
        (segments, chars)
    }

    /// 加载时校验(PLAN §8):白名单引用必须已注册;few-shot 引用工具 ⊆ 基础工具∪白名单、
    /// call/result 配对完整且有序(孤儿 tool_call 会被严格端点 400)、示例 id 用 fs_ 前缀、
    /// 体量不超 `FEWSHOT_MAX_*`。
    pub fn validate(&self, registry: &Tools) -> Result<()> {
        for name in &self.tools {
            ensure!(registry.get(name).is_some(), "场景 {} 白名单引用未注册工具 {name}", self.id);
        }
        let (segments, chars) = self.few_shot_size();
        ensure!(
            segments <= FEWSHOT_MAX_SEGMENTS && chars <= FEWSHOT_MAX_CHARS,
            "场景 {} 的 few-shot 超出体量闸:{segments} 段 / {chars} 字符(上限 {FEWSHOT_MAX_SEGMENTS} 段 / \
             {FEWSHOT_MAX_CHARS} 字符)。示范对话进每一个回合的稳定前缀 —— 要加新的,先替换掉一段旧的",
            self.id
        );
        let allowed = |name: &str| {
            crate::tools::BASE_TOOLS.contains(&name) || self.tools.iter().any(|t| t == name)
        };
        let mut open: HashSet<&str> = HashSet::new();
        for (i, msg) in self.few_shots.iter().enumerate() {
            match msg {
                ChatMessage::User { .. } => {}
                ChatMessage::Assistant { tool_calls, .. } => {
                    for c in tool_calls {
                        ensure!(
                            allowed(&c.name),
                            "场景 {} few-shot[{i}] 调了基础工具与白名单之外的工具 {}",
                            self.id,
                            c.name
                        );
                        ensure!(
                            c.id.starts_with("fs_"),
                            "场景 {} few-shot[{i}] 示例 id {} 必须用 fs_ 前缀(与真实 id 隔开)",
                            self.id,
                            c.id
                        );
                        ensure!(
                            open.insert(c.id.as_str()),
                            "场景 {} few-shot[{i}] 示例 id {} 重复",
                            self.id,
                            c.id
                        );
                    }
                }
                ChatMessage::ToolResult { call_id, .. } => {
                    ensure!(
                        open.remove(call_id.as_str()),
                        "场景 {} few-shot[{i}] 的结果 {call_id} 没有对应的在前 tool_call",
                        self.id
                    );
                }
            }
        }
        if let Some(orphan) = open.iter().next() {
            bail!("场景 {} few-shot 有 tool_call {orphan} 缺配对结果(严格端点会 400)", self.id);
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct Scenes {
    by_id: HashMap<String, Scene>,
}

pub const DEFAULT_SCENE_ID: &str = "companion";

/// few-shot 的体量闸(2026-11-12 用户拍板:规则认现状,加守卫防止继续涨)。出厂场景现在是
/// 15 段、2175 字符(`Scene::few_shot_size` 的口径);这两个数 = 现状加一成左右的余量。示范对话进**每一个**回合的稳定前缀,多一段就是
/// 每回合多付一段 —— 要加新示范,先想想能不能替换掉一段旧的;真要抬这两个数,回来问(§4.11)。
pub const FEWSHOT_MAX_SEGMENTS: usize = 16;
pub const FEWSHOT_MAX_CHARS: usize = 2400;


impl Scenes {
    pub fn builtin() -> Scenes {
        let mut by_id = HashMap::new();
        let companion: Scene = serde_json::from_str(include_str!("../assets/scenes/companion.json"))
            .expect("内置场景 companion.json 不合法");
        by_id.insert(companion.id.clone(), companion);
        Scenes { by_id }
    }

    pub fn get(&self, id: &str) -> Option<&Scene> {
        self.by_id.get(id)
    }

    pub fn default_scene(&self) -> &Scene {
        self.by_id
            .get(DEFAULT_SCENE_ID)
            .expect("companion 场景必须存在")
    }

    pub fn all(&self) -> impl Iterator<Item = &Scene> {
        self.by_id.values()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::ToolCall;

    #[test]
    fn builtin_companion_scene_parses_and_validates() {
        let scenes = Scenes::builtin();
        let s = scenes.default_scene();
        assert_eq!(s.id, "companion");
        assert!(s.persona.contains("{name}"), "persona 用占位符留名,由 build_context 填入");
        assert!(!s.opening_line.is_empty());
        // remember/briefing 三件套是常驻基础工具,白名单只声明场景特有的
        assert_eq!(
            s.tools,
            ["now", "weather", "media_search", "media_play", "media_control", "media_download", "lyrics_fetch", "torrent_download", "ffmpeg_run", "task_status", "task_cancel", "open", "system_volume", "power",
             "system_status", "startup_list", "startup_toggle",
             "fs_list", "fs_find",
             "fs_read_text", "fs_stat", "fs_move", "fs_copy", "fs_mkdir", "fs_trash", "fs_write_text",
             "fs_append", "fs_edit", "fs_undo", "fs_unzip", "fs_zip", "fs_usage",
             "reminder_set", "reminder_list", "reminder_cancel", "watch_set", "web_search", "web_fetch",
             "web_download", "web_render", "qr_decode", "qr_encode", "pdf_to_png", "print_file", "read_image", "read_audio",
             "speak_to_file", "show_image", "send_file", "send_text", "note_todo", "finish_todo"]
        );
        assert!(!s.few_shots.is_empty(), "companion 必须带 few-shot 示范");
        // 体量闸:现状必须在闸内,且闸没有被悄悄放宽到形同虚设(余量不超过两成)
        let (segments, chars) = s.few_shot_size();
        assert!(segments <= FEWSHOT_MAX_SEGMENTS && chars <= FEWSHOT_MAX_CHARS, "{segments} 段 / {chars} 字符");
        assert!(
            FEWSHOT_MAX_SEGMENTS <= segments + 3 && FEWSHOT_MAX_CHARS <= chars + chars / 5,
            "few-shot 的闸比现状宽太多了({segments} 段 / {chars} 字符):删了示范就把闸一起收紧"
        );
        // 反例纪律:至少一段"不调工具直接聊"的示范
        assert!(
            s.few_shots.iter().any(|m| matches!(
                m,
                ChatMessage::Assistant { tool_calls, content, .. } if tool_calls.is_empty() && !content.is_empty()
            )),
            "few-shot 必须含反例"
        );
        s.validate(&Tools::builtin()).expect("内置场景必须过校验");
    }

    /// few-shot 超出体量闸 → 校验不过(段数、字符数各验一次)。
    #[test]
    fn validate_rejects_few_shots_over_budget() {
        let registry = Tools::builtin();
        let mut s = Scenes::builtin().default_scene().clone();
        let say = |t: &str| ChatMessage::Assistant {
            content: t.into(),
            reasoning: None,
            tool_calls: vec![],
            reasoning_state: None,
        };
        s.few_shots = (0..=FEWSHOT_MAX_SEGMENTS)
            .flat_map(|_| [ChatMessage::user("在吗"), say("在")])
            .collect();
        let err = s.validate(&registry).unwrap_err().to_string();
        assert!(err.contains("体量闸"), "{err}");

        s.few_shots = vec![ChatMessage::user("讲个长的"), say(&"长".repeat(FEWSHOT_MAX_CHARS))];
        assert!(s.validate(&registry).unwrap_err().to_string().contains("体量闸"));

        s.few_shots = vec![ChatMessage::user("在吗"), say("在")];
        s.validate(&registry).expect("闸内的照常通过");
    }

    #[test]
    fn validate_rejects_orphan_calls_and_foreign_tools() {
        let scenes = Scenes::builtin();
        let mut s = scenes.default_scene().clone();
        let registry = Tools::builtin();

        // 孤儿 call:有 tool_call 没结果
        s.few_shots = vec![ChatMessage::Assistant {
            content: String::new(),
            reasoning: None,
            tool_calls: vec![ToolCall {
                id: "fs_x".into(),
                name: "now".into(),
                args: serde_json::json!({}),
                is_incomplete: false,
            }],
            reasoning_state: None,
        }];
        assert!(s.validate(&registry).is_err(), "孤儿 tool_call 必须被拒");

        // 白名单外的工具
        s.few_shots = vec![
            ChatMessage::Assistant {
                content: String::new(),
                reasoning: None,
                tool_calls: vec![ToolCall {
                    id: "fs_y".into(),
                    name: "ghost".into(),
                    args: serde_json::json!({}),
                    is_incomplete: false,
                }],
                reasoning_state: None,
            },
            ChatMessage::tool_result("fs_y", "ok"),
        ];
        assert!(s.validate(&registry).is_err(), "白名单外工具必须被拒");

        // 非 fs_ 前缀
        s.few_shots = vec![
            ChatMessage::Assistant {
                content: String::new(),
                reasoning: None,
                tool_calls: vec![ToolCall {
                    id: "call_1".into(),
                    name: "now".into(),
                    args: serde_json::json!({}),
                    is_incomplete: false,
                }],
                reasoning_state: None,
            },
            ChatMessage::tool_result("call_1", "ok"),
        ];
        assert!(s.validate(&registry).is_err(), "示例 id 必须 fs_ 前缀");
    }
}
