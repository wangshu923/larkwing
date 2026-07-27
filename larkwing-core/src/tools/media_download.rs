//! 能力轴:影音(存)。把网络页面的音轨下载成本地音频文件 —— 与 media_play(放)正交:
//! 放是转瞬的流,这个落成用户自己的文件(之后本地优先秒开、离线可放、整夹连播都吃到)。
//! 格式跟着源走(有无损存 .flac,否则 .m4a),全程不转码。合集要不要整批下是**用户的
//! 决定**:description 引导模型先问,工具只认确认后的 all 参数。

use async_trait::async_trait;

use super::{Tool, ToolCtx, ToolRisk, ToolSpec};
use crate::files::{default_download_dir, human_size};
use crate::media::DownloadOutcome;

pub(super) struct MediaDownload {
    spec: ToolSpec,
}

impl MediaDownload {
    pub(super) fn new() -> MediaDownload {
        MediaDownload {
            spec: ToolSpec {
                name: "media_download",
                description: "把音乐/音频下载成本地文件:传入网络页面链接(通常来自 \
                              media_search 的结果),把它的音轨存到电脑上(只存声音,不含画面)。\
                              格式自动挑源里最好的音质——有无损存 .flac,否则存 .m4a,不做有损\
                              转码;会自动在旁边配同名 .lrc 歌词(视频字幕或公共歌词库),找不到\
                              会如实说。**尽量带上 title/artist(干净的歌名/歌手,从用户的话和\
                              搜索结果辨认)**:文件会存成「歌手 - 歌名」并写对标签,歌词也靠它\
                              找;拿不准就省略,别把视频标题原样搬进去。存到哪:用户点名了目录、\
                              或任务需知里记了音乐目录就传 dir;都没有就省略(落系统「下载」文件\
                              夹)。**链接是合集/分P(多首)而用户没说清楚时,先问用户「只下这一\
                              首还是整个合集」,用户明确要全部才传 all=true**——整批在后台慢慢下\
                              (进度在屏幕任务条),本工具立即返回。**下合集的一段用 all=true + \
                              from/to**(第几首到第几首,1 起含两端):超过一次上限的大合集分几\
                              批下(如实告诉用户分几批),「只要合集里第 N 首」= from=N to=N,\
                              不用自己去网页扒分P链接。本地已有的文件不需要下载(要挪/复制用 \
                              fs_move/fs_copy)。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "url": {
                            "type": "string",
                            "description": "网络页面链接(https://…),通常来自 media_search 的结果"
                        },
                        "title": {
                            "type": "string",
                            "description": "干净的歌名(不含歌手名、修饰词、书名号;拿不准就省略)"
                        },
                        "artist": {
                            "type": "string",
                            "description": "演唱者/歌手名(注意不是视频上传者;拿不准就省略)"
                        },
                        "dir": {
                            "type": "string",
                            "description": "存到哪个文件夹(绝对路径,支持 ~ 开头);省略 = 系统「下载」文件夹"
                        },
                        "all": {
                            "type": "boolean",
                            "description": "true=把整个合集/分P全部下载(仅在用户明确要全部时用;此时 artist 对整批生效、title 忽略);默认 false=只下这一首"
                        },
                        "from": {
                            "type": "integer",
                            "description": "配合 all=true:从合集第几首开始(1 起,含);省略=从头"
                        },
                        "to": {
                            "type": "integer",
                            "description": "配合 all=true:下到合集第几首(含);省略=到最后"
                        }
                    },
                    "required": ["url"]
                }),
                // 首次含组件下载(几十 MB)+ 大文件拉流:给足额度
                timeout: std::time::Duration::from_secs(300),
                ui_key: "tool.media_download",
            },
        }
    }
}

#[async_trait]
impl Tool for MediaDownload {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn risk(&self) -> ToolRisk {
        ToolRisk::Mutating
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> anyhow::Result<String> {
        let url = args
            .get("url")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!("缺少合法的 url 参数(需要 http(s) 页面链接)"))?;
        if crate::media::is_local_path(url) {
            anyhow::bail!("这是本地路径,文件已经在电脑上,不需要下载——要挪/复制用 fs_move/fs_copy");
        }
        anyhow::ensure!(
            url.starts_with("http://") || url.starts_with("https://"),
            "url 不合法(需要 http(s) 页面链接),收到: {url}"
        );
        let dir = match args.get("dir").and_then(serde_json::Value::as_str).map(str::trim) {
            Some(d) if !d.is_empty() => {
                let p = std::path::PathBuf::from(super::expand_home(d)); // 「~/xxx」宽容展开(§4.4)
                anyhow::ensure!(p.is_absolute(), "dir 需要绝对路径,收到: {d}");
                p
            }
            _ => default_download_dir(),
        };
        let all = super::arg_bool(&args, "all", false);
        let opt_str = |key: &str| {
            args.get(key)
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        let meta = crate::media::TrackMeta { title: opt_str("title"), artist: opt_str("artist") };

        let from = super::arg_u64(&args, "from", 0);
        let to = super::arg_u64(&args, "to", 0);
        if !all && (from > 0 || to > 0) {
            anyhow::bail!(
                "from/to 要配 all=true 用(下合集里的第几首到第几首;只要某一首 = \
                 all=true + from=N to=N)"
            );
        }
        let range = ((from > 0).then_some(from as usize), (to > 0).then_some(to as usize));

        let outcome = if all {
            ctx.media
                .download_all(url, &dir, meta.artist, (ctx.user_id, ctx.conv_id), range)
                .await?
        } else {
            ctx.media.download_audio(url, &dir, &meta).await?
        };
        Ok(match outcome {
            DownloadOutcome::Done(f) => {
                let quality = if f.lossless {
                    format!("{} 无损", f.ext.to_uppercase())
                } else {
                    f.ext.to_uppercase()
                };
                let mut out = format!(
                    "已下载:《{}》 → {}({quality},{})",
                    f.title,
                    f.path.display(),
                    human_size(f.bytes)
                );
                if !f.remuxed {
                    out.push_str(";这次按原始流原样保存(没写歌名标签),一般也能正常播放");
                }
                out.push_str(match f.lyrics {
                    crate::media::LyricsResult::Cc => ";歌词已配好(来自视频字幕,同名 .lrc)",
                    crate::media::LyricsResult::Lib => ";歌词已配好(同名 .lrc)",
                    crate::media::LyricsResult::LibPlain => {
                        ";歌词已配好(纯文本、无逐句时间轴,同名 .lrc)"
                    }
                    crate::media::LyricsResult::Existed => ";旁边已有同名歌词文件,没动它",
                    crate::media::LyricsResult::NotFound => {
                        ";这首没找到歌词(歌名/歌手给得更准可能找得到)"
                    }
                });
                out
            }
            DownloadOutcome::AwaitingLogin { detail } => format!(
                "这个音源需要登录才能拿到,不是出错了。请提示用户点一下登录、用手机扫码;\
                 登录完成后再让我下载一次就行。(原因:{detail})"
            ),
            DownloadOutcome::BatchStarted { total, dir, scope } => format!(
                "已开始在后台下载{scope},这批 {total} 首,存到 {}(每首顺带配歌词,配不上的\
                 跳过)。进度在屏幕任务条上;**跑完会自动回来一条结果汇报**(下好几首、没成的\
                 点名),到时再转述。现在告诉用户已经开工、跑完会说一声就好。",
                dir.display()
            ),
        })
    }
}
