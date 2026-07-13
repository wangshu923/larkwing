//! 工具运行时(PLAN §8):一个 Tool trait + 静态注册表,通用循环的"手脚"。
//! 纪律:工具按"能力轴"做正交原语(一个原语 ≈ 人类助理心中"一个动作"),不按任务做;
//! 加任务能力 = 本目录加一个文件 + builtin() 里一行注册,循环与 engine 永不改。
//! job 型不另设 trait:就是一个秒回"已启动"的阻塞工具(JobRunner 后置,见 PLAN §8)。

mod briefing;
mod desktop;
mod end_conversation;
mod fs;
mod media_control;
mod media_play;
mod media_search;
mod now;
mod pdf;
mod qr;
mod recall;
mod remember;
mod reminder;
mod send_file;
mod todo;
mod watch;
mod weather;
mod web;
mod web_render;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::llm::ToolDef;
use crate::store::Store;

/// 常驻基础工具(PLAN §9):信息纪律件套 + 会话收尾原语,**每个场景自动在场**,白名单无需
/// 声明 —— 运行时法条(engine/context::LAWS)点名了它们,法条全场景生效,工具就得全场景在。
/// end_conversation = 免唤醒连续对话的显式关闭信号(LAWS「聊完了就收尾」点名),同 remember 一族
/// 全场景可用。
pub const BASE_TOOLS: &[&str] = &[
    "remember",
    "recall",
    "briefing_write",
    "briefing_lookup",
    "briefing_remove",
    "end_conversation",
];

/// 静态规格:给模型看的(name/description/parameters)+ 给运行时的(timeout)
/// + 给 UI 的(ui_key,i18n 键 —— core 不产用户可见文案,文案在前端字典)。
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    /// JSON Schema(object 形)。
    pub parameters: serde_json::Value,
    pub timeout: Duration,
    pub ui_key: &'static str,
}

impl ToolSpec {
    pub fn def(&self) -> ToolDef {
        ToolDef {
            name: self.name.into(),
            description: self.description.into(),
            parameters: self.parameters.clone(),
        }
    }
}

/// 从工具入参里**宽容**地取一个布尔值。模型(尤其流式 JSON)常把 schema 里声明为
/// boolean 的参数发成字符串 `"true"` / `"false"`、甚至数字 1/0 —— `Value::as_bool`
/// 只认真正的 JSON bool,认不出就静默回落默认值(实锤:`audio_only` 被发成字符串
/// `"true"` → 当成 false → 放本地歌却弹出全屏视频框,当电影放了)。这里照"Quirks 数据
/// 修正"(§4.4)在入口兜底:真 bool 原样;字符串认 true/false/1/0/yes/no;数字非零为真;
/// 缺省 / null / 认不出 → 回落 `default`。新加 boolean 入参一律走它,别再裸 `as_bool`。
pub(crate) fn arg_bool(args: &serde_json::Value, key: &str, default: bool) -> bool {
    match args.get(key) {
        Some(serde_json::Value::Bool(b)) => *b,
        Some(serde_json::Value::String(s)) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "y" => true,
            "false" | "0" | "no" | "n" | "" => false,
            _ => default,
        },
        Some(serde_json::Value::Number(n)) => n.as_f64().map(|v| v != 0.0).unwrap_or(default),
        _ => default,
    }
}

/// 每次执行的现场:多用户与会话归属由此带入,工具自身无状态。
pub struct ToolCtx {
    pub user_id: i64,
    pub conv_id: i64,
    pub store: Store,
    /// 影音运行时(搜/放/控三工具用;其余工具无视)。
    pub media: crate::media::MediaRuntime,
    /// 壳层网页渲染器(web_render 专用;None = 壳层没注入〔core 单测/eval/headless〕,
    /// 工具如实说没有渲染组件,§3.5)。
    pub web: Option<Arc<dyn crate::webrender::WebRenderer>>,
}

/// 工具风险分级(预留 slot,PLAN §8):`Safe` = 读/记类;`Mutating` = 会改动用户文件
/// (move/copy/trash/write…)。**引擎当前不据此拦截**(用户拍板:不强制确认、不设门禁)——
/// 留作未来「执行前确认闸门」的判据 + 操作日志/HUD 措辞的元数据。纯数据,加它零行为变化。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolRisk {
    Safe,
    Mutating,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> &ToolSpec;

    /// 风险分级(预留 slot):默认 `Safe`,会动用户文件的工具覆盖成 `Mutating`。
    fn risk(&self) -> ToolRisk {
        ToolRisk::Safe
    }

    /// 错误也是观察:Err 会被 engine 变成错误 ToolResult 喂回模型(模型自行换路),
    /// 不打断回合。取消语义:future 可能被 drop(回合取消),实现必须 drop-safe。
    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> anyhow::Result<String>;
}

/// 静态注册表(Scenes 同款)。注册表本身无依赖 —— 工具执行所需的一切经 ToolCtx 按次传入。
#[derive(Clone, Default)]
pub struct Tools {
    by_name: HashMap<&'static str, Arc<dyn Tool>>,
}

impl Tools {
    pub fn builtin() -> Tools {
        let mut tools = Tools::default();
        tools.register(Arc::new(now::Now::new()));
        tools.register(Arc::new(end_conversation::EndConversation::new()));
        tools.register(Arc::new(remember::Remember::new()));
        tools.register(Arc::new(recall::Recall::new()));
        tools.register(Arc::new(todo::NoteTodo::new()));
        tools.register(Arc::new(todo::FinishTodo::new()));
        tools.register(Arc::new(briefing::BriefingWrite::new()));
        tools.register(Arc::new(briefing::BriefingLookup::new()));
        tools.register(Arc::new(briefing::BriefingRemove::new()));
        tools.register(Arc::new(media_search::MediaSearch::new()));
        tools.register(Arc::new(media_play::MediaPlay::new()));
        tools.register(Arc::new(media_control::MediaControl::new()));
        tools.register(Arc::new(desktop::Open::new()));
        tools.register(Arc::new(desktop::SystemVolume::new()));
        tools.register(Arc::new(desktop::Power::new()));
        tools.register(Arc::new(fs::FsList::new()));
        tools.register(Arc::new(fs::FsFind::new()));
        tools.register(Arc::new(fs::FsReadText::new()));
        tools.register(Arc::new(fs::FsMove::new()));
        tools.register(Arc::new(fs::FsCopy::new()));
        tools.register(Arc::new(fs::FsMkdir::new()));
        tools.register(Arc::new(fs::FsTrash::new()));
        tools.register(Arc::new(fs::FsWriteText::new()));
        tools.register(Arc::new(fs::FsAppend::new()));
        tools.register(Arc::new(fs::FsEdit::new()));
        tools.register(Arc::new(fs::FsUndo::new()));
        tools.register(Arc::new(reminder::ReminderSet::new()));
        tools.register(Arc::new(reminder::ReminderList::new()));
        tools.register(Arc::new(reminder::ReminderCancel::new()));
        // 天气 + 条件提醒共享一个 WeatherClient(连接池);源按 settings 里和风 key 有无现选
        let weather_client = Arc::new(crate::weather::WeatherClient::new());
        tools.register(Arc::new(weather::WeatherTool::new(weather_client.clone())));
        tools.register(Arc::new(watch::WatchSet::new(weather_client)));
        // 搜/读两工具共享一个客户端(连接池 + 正文短缓存);下载自持长超时客户端
        let web_client = Arc::new(crate::web::WebClient::new());
        tools.register(Arc::new(web::WebSearch::new(web_client.clone())));
        tools.register(Arc::new(web::WebFetch::new(web_client)));
        tools.register(Arc::new(web::WebDownload::new()));
        tools.register(Arc::new(qr::QrDecode::new()));
        tools.register(Arc::new(pdf::PdfToPng::new()));
        tools.register(Arc::new(send_file::SendFile::new()));
        tools.register(Arc::new(web_render::WebRender::new()));
        tools
    }

    fn register(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.spec().name;
        let dup = self.by_name.insert(name, tool);
        debug_assert!(dup.is_none(), "工具名重复注册: {name}");
    }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.by_name.get(name)
    }

    /// 白名单子集 = **常驻基础工具(固定在前)+ 场景声明(原序在后)**,去重;
    /// 顺序恒定 → 前缀不抖。未知名忽略并告警。
    pub fn subset(&self, allow: &[String]) -> Vec<Arc<dyn Tool>> {
        let mut names: Vec<&str> = BASE_TOOLS.to_vec();
        for name in allow {
            if !names.contains(&name.as_str()) {
                names.push(name);
            }
        }
        names
            .into_iter()
            .filter_map(|name| {
                let tool = self.by_name.get(name).cloned();
                if tool.is_none() {
                    tracing::warn!(tool = %name, "场景白名单引用了未注册的工具,忽略");
                }
                tool
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_registry_and_subset_base_first_scene_after() {
        let tools = Tools::builtin();
        for name in ["now", "remember", "recall", "briefing_write", "briefing_lookup",
                     "briefing_remove", "media_search", "media_play", "media_control"]
        {
            assert!(tools.get(name).is_some(), "{name} 必须已注册");
        }

        // 基础工具固定在前;场景声明原序在后;重复声明去重;未知名忽略
        let allow = vec!["remember".to_string(), "ghost".to_string(), "now".to_string()];
        let subset = tools.subset(&allow);
        let names: Vec<&str> = subset.iter().map(|t| t.spec().name).collect();
        assert_eq!(
            names,
            [
                "remember",
                "recall",
                "briefing_write",
                "briefing_lookup",
                "briefing_remove",
                "end_conversation",
                "now"
            ],
            "base 在前(声明里的 remember 被去重),场景序在后,ghost 被忽略"
        );
    }

    #[test]
    fn arg_bool_tolerates_stringified_and_numeric() {
        use serde_json::json;
        // 真 bool 原样
        assert!(arg_bool(&json!({ "x": true }), "x", false));
        assert!(!arg_bool(&json!({ "x": false }), "x", true));
        // 字符串(模型实锤发法):大小写 / 空白都认
        assert!(arg_bool(&json!({ "x": "true" }), "x", false), "字符串 true 应为真(audio_only 实锤)");
        assert!(arg_bool(&json!({ "x": " TRUE " }), "x", false));
        assert!(!arg_bool(&json!({ "x": "false" }), "x", true));
        assert!(arg_bool(&json!({ "x": "1" }), "x", false));
        assert!(!arg_bool(&json!({ "x": "0" }), "x", true));
        assert!(arg_bool(&json!({ "x": "yes" }), "x", false));
        // 数字非零为真
        assert!(arg_bool(&json!({ "x": 1 }), "x", false));
        assert!(!arg_bool(&json!({ "x": 0 }), "x", true));
        // 缺省 / null / 认不出 → 回落默认
        assert!(!arg_bool(&json!({}), "x", false));
        assert!(arg_bool(&json!({}), "x", true));
        assert!(arg_bool(&json!({ "x": null }), "x", true));
        assert!(arg_bool(&json!({ "x": "maybe" }), "x", true), "认不出回落默认");
    }

    #[test]
    fn specs_produce_valid_defs() {
        for tool in Tools::builtin().by_name.values() {
            let spec = tool.spec();
            let def = spec.def();
            assert!(!def.name.is_empty() && !def.description.is_empty());
            assert_eq!(def.parameters["type"], "object", "参数 schema 必须是 object 形");
            assert!(spec.ui_key.starts_with("tool."), "ui_key 是前端字典键");
            assert!(spec.timeout >= Duration::from_secs(1));
        }
    }
}
