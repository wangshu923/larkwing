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
                description: "播放音视频:网络页面链接(通常来自 media_search)或**本地文件/\
                              音频文件夹绝对路径**(配合任务需知里的目录 + fs_list/fs_find 找到,\
                              含 NAS 路径)。放歌/听故事/白噪音用 audio_only=true(只出声音);\
                              看视频/动画片用 false。**连播**:B 站合集/分P、本地剧集文件夹放一集\
                              自动接着下一集;本地音频放一首自动接着同文件夹的下一首,给音频\
                              **文件夹路径**则把整夹当列表从头连着放——都记住上次放到哪,用户没指定\
                              第几集/首时默认接着上次;说「从头/重新看/从第一集」时传 restart=true。\
                              循环/随机用 media_control。开始播放后简短告诉用户放的是什么就好。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "url": {
                            "type": "string",
                            "description": "网络页面链接(https://…)或本地文件/音频文件夹绝对路径(盘符 D:\\…、UNC \\\\nas\\…、Unix /…;支持 ~ 开头 = 用户主目录)"
                        },
                        "audio_only": {
                            "type": "boolean",
                            "description": "true=只放声音(听歌/故事);false=带画面(默认)"
                        },
                        "restart": {
                            "type": "boolean",
                            "description": "true=忽略上次进度、从第一集重新放(用户说「从头/重新看」时用);默认 false=接着上次"
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
            .filter(|s| !s.is_empty())
            .map(super::expand_home) // 「~/xxx」宽容展开(§4.4),再验合法性
            .ok_or_else(|| {
                anyhow::anyhow!("缺少合法的 url 参数(http(s) 链接或本地文件/文件夹绝对路径)")
            })?;
        anyhow::ensure!(
            url.starts_with("http://")
                || url.starts_with("https://")
                || crate::media::is_local_path(&url),
            "url 不合法(要 http(s) 链接或本地文件/文件夹绝对路径),收到: {url}"
        );
        let url = url.as_str();
        // 宽容解析:模型常把 audio_only 发成字符串 "true"(裸 as_bool 认不出 → 回落 false →
        // 放歌弹全屏视频框)。走共享 arg_bool 兜底(§4.4 Quirks)。
        let audio_only = super::arg_bool(&args, "audio_only", false);
        let restart = super::arg_bool(&args, "restart", false);

        match ctx.media.play(ctx.user_id, url, audio_only, restart).await? {
            crate::media::PlayOutcome::Playing(np) => {
                // 多集:带上「第N/共M集(音频=首)」+ 续播时点明"接着上次"(让模型如实转述)。
                let unit =
                    if matches!(np.kind, crate::media::MediaKind::Audio) { "首" } else { "集" };
                let mut out = match &np.playlist {
                    Some(p) if p.resumed => format!(
                        "接着上次,从《{}》第{}{unit}继续播放(共{}{unit})",
                        np.title,
                        p.index + 1,
                        p.total
                    ),
                    Some(p) => format!(
                        "已开始播放《{}》(第{}{unit}/共{}{unit})",
                        np.title,
                        p.index + 1,
                        p.total
                    ),
                    None => format!("已开始播放《{}》", np.title),
                };
                if let Some(author) = &np.author {
                    out.push_str(&format!("(UP主: {author})"));
                }
                if let Some(d) = np.duration_seconds {
                    let (m, s) = ((d as i64) / 60, (d as i64) % 60);
                    out.push_str(&format!(",时长 {m}:{s:02}"));
                }
                if np.playlist.is_some() {
                    out.push_str(&format!(";放完会自动接着下一{unit}"));
                }
                Ok(out)
            }
            // 需要登录 ≠ 失败:引导用户点登录扫码,登录成功后会自动重放(§7.1,不用再说一遍)。
            crate::media::PlayOutcome::AwaitingLogin { detail } => Ok(format!(
                "这个内容需要登录才能播放,不是出错了。请提示用户点一下登录、用手机扫码登录一下;\
                 登录成功后会自动接着把它放出来,不用再说一遍。(原因:{detail})"
            )),
        }
    }
}
