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

    /// 加载时校验(PLAN §8):白名单引用必须已注册;few-shot 引用工具 ⊆ 基础工具∪白名单、
    /// call/result 配对完整且有序(孤儿 tool_call 会被严格端点 400)、示例 id 用 fs_ 前缀。
    pub fn validate(&self, registry: &Tools) -> Result<()> {
        for name in &self.tools {
            ensure!(registry.get(name).is_some(), "场景 {} 白名单引用未注册工具 {name}", self.id);
        }
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
        assert!(s.persona.contains("7274"));
        assert!(!s.opening_line.is_empty());
        // remember/briefing 三件套是常驻基础工具,白名单只声明场景特有的
        assert_eq!(
            s.tools,
            ["now", "weather", "media_search", "media_play", "media_control", "fs_list", "fs_find",
             "fs_read_text", "fs_move", "fs_copy", "fs_mkdir", "fs_trash", "fs_write_text",
             "fs_append", "fs_edit", "fs_undo",
             "reminder_set", "reminder_list", "reminder_cancel", "watch_set", "web_search", "web_fetch"]
        );
        assert!(!s.few_shots.is_empty(), "companion 必须带 few-shot 示范");
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
            },
            ChatMessage::ToolResult { call_id: "fs_y".into(), content: "ok".into() },
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
            },
            ChatMessage::ToolResult { call_id: "call_1".into(), content: "ok".into() },
        ];
        assert!(s.validate(&registry).is_err(), "示例 id 必须 fs_ 前缀");
    }
}
