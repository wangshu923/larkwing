//! 能力轴:影音(搜)。robot bili_search 的精神续作:返回候选让模型挑,
//! 挑完把 url 交给 media_play。结果是喂模型的观察(JSON),不是 UI 文案。

use async_trait::async_trait;

use super::{Tool, ToolCtx, ToolSpec};

pub(super) struct MediaSearch {
    spec: ToolSpec,
}

impl MediaSearch {
    pub(super) fn new() -> MediaSearch {
        MediaSearch {
            spec: ToolSpec {
                name: "media_search",
                description: "按关键词搜可播放的歌曲/视频/儿歌/故事,返回候选列表(标题、作者、\
                              时长、url)。用户想听歌、看视频、放白噪音时先用它搜,再从结果里挑\
                              最合适的一条(优先官方/原唱/时长合理的),把 url 交给 media_play。\
                              用户只是聊到音乐话题、没让你放,就别搜。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "搜索关键词,中文即可,如「恭喜发财 刘德华」「小猪佩奇 第一集」"
                        },
                        "limit": {
                            "type": "integer",
                            "description": "最多返回几条,默认 5",
                            "minimum": 1,
                            "maximum": 10
                        }
                    },
                    "required": ["query"]
                }),
                timeout: std::time::Duration::from_secs(20),
                ui_key: "tool.media_search",
            },
        }
    }
}

#[async_trait]
impl Tool for MediaSearch {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> anyhow::Result<String> {
        let query = args
            .get("query")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!("缺少 query 参数"))?;
        let limit = args
            .get("limit")
            .and_then(serde_json::Value::as_u64)
            .map(|n| n.clamp(1, 10) as usize)
            .unwrap_or(5);

        match ctx.media.search(query, limit).await {
            Ok(hits) if hits.is_empty() => Ok("没搜到相关内容,换个关键词试试".into()),
            Ok(hits) => Ok(serde_json::to_string(&hits)?),
            // 风控:事件已发(UI 出扫码入口),这里给模型一句可转述的观察
            Err(crate::media::SearchError::RiskControl) => Ok(
                "搜索被站点风控拦下了。屏幕上已经出现登录入口,请用户扫码登录后再试一次"
                    .into(),
            ),
            Err(crate::media::SearchError::Other(e)) => Err(e),
        }
    }
}
