//! 能力轴:影音(放)。job 型姿态:解析完成即返回"已开播",播放本身在 UI 进行;
//! 组件缺席时第一次调用会触发用时下载(进度在 HUD),超时也不浪费 —— 下载分离
//! spawn,回合结束后继续走完,下次再试直接命中。

use async_trait::async_trait;

use super::{Tool, ToolCtx, ToolSpec};

pub(super) struct MediaPlay {
    spec: ToolSpec,
}

impl MediaPlay {
    pub(super) fn new() -> MediaPlay {
        MediaPlay {
            spec: ToolSpec {
                name: "media_play",
                description: "播放音视频:网络页面链接(通常来自 media_search)或**本地文件\
                              绝对路径**(配合任务需知里的目录 + fs_list/fs_find 找到的文件,\
                              含 NAS 路径)。放歌/听故事/白噪音用 audio_only=true(只出声音);\
                              看视频/动画片用 false。开始播放后简短告诉用户放的是什么就好。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "url": {
                            "type": "string",
                            "description": "网络页面链接(https://…)或本地文件绝对路径(D:\\…、\\\\nas\\…、/Users/…)"
                        },
                        "audio_only": {
                            "type": "boolean",
                            "description": "true=只放声音(听歌/故事);false=带画面(默认)"
                        }
                    },
                    "required": ["url"]
                }),
                // 首次含组件下载(几十 MB):给足额度;之后的解析只要几秒
                timeout: std::time::Duration::from_secs(180),
                ui_key: "tool.media_play",
            },
        }
    }
}

#[async_trait]
impl Tool for MediaPlay {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> anyhow::Result<String> {
        let url = args
            .get("url")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| {
                s.starts_with("http://")
                    || s.starts_with("https://")
                    || crate::media::is_local_path(s)
            })
            .ok_or_else(|| {
                anyhow::anyhow!("缺少合法的 url 参数(http(s) 链接或本地文件绝对路径)")
            })?;
        let audio_only =
            args.get("audio_only").and_then(serde_json::Value::as_bool).unwrap_or(false);

        let np = ctx.media.play(url, audio_only).await?;
        let mut out = format!("已开始播放《{}》", np.title);
        if let Some(author) = &np.author {
            out.push_str(&format!("(UP主: {author})"));
        }
        if let Some(d) = np.duration_seconds {
            let (m, s) = ((d as i64) / 60, (d as i64) % 60);
            out.push_str(&format!(",时长 {m}:{s:02}"));
        }
        Ok(out)
    }
}
