//! 能力轴:影音(配词)。给本机已有的音频文件配歌词 —— 旁挂同名 .lrc,**绝不改动音频
//! 原件**(用户的无损曲库一个字节不碰)。歌名/歌手优先读文件自带标签;缺标签的靠模型
//! 从文件名判断后带参重试(信息抽取归模型,§5)。与 media_download 的下载配词共用机器件。

use async_trait::async_trait;

use super::{Tool, ToolCtx, ToolRisk, ToolSpec};
use crate::media::{LyricsBatchOutcome, LyricsItem};

pub(super) struct LyricsFetch {
    spec: ToolSpec,
}

impl LyricsFetch {
    pub(super) fn new() -> LyricsFetch {
        LyricsFetch {
            spec: ToolSpec {
                name: "lyrics_fetch",
                description: "给本机已有的音频文件配歌词:在每个文件旁边生成同名 .lrc\
                              (播放器自动识别),**不改动音频文件本身**;已有 .lrc 的自动跳过。\
                              歌名/歌手优先读文件自带标签;结果里点名「缺歌名」的文件,从文件名\
                              判断出歌名/歌手后带 title/artist 对那几个重试。配合 fs_list/\
                              fs_find 找到音乐文件后把路径喂进来;一次最多 200 个,超过 20 个\
                              自动转后台(进度在屏幕任务条)。歌词来自公共歌词库,冷门/现场版\
                              可能找不到——找不到就如实告诉用户。实在要自己从网上扒歌词代写 \
                              .lrc 时,**绝不编造 [mm:ss] 时间轴**(网页上没有时间轴就写纯文本\
                              歌词,假时间轴会让播放器乱滚,比没有更糟)。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "files": {
                            "type": "array",
                            "description": "要配歌词的音频文件",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "path": {
                                        "type": "string",
                                        "description": "音频文件绝对路径(支持 ~ 开头)"
                                    },
                                    "title": {
                                        "type": "string",
                                        "description": "干净的歌名(文件标签缺失时才需要;从文件名判断)"
                                    },
                                    "artist": {
                                        "type": "string",
                                        "description": "演唱者/歌手名(同上,可选)"
                                    }
                                },
                                "required": ["path"]
                            }
                        }
                    },
                    "required": ["files"]
                }),
                // 回合内档最多 20 首 × 约半秒 + 首次 ffmpeg 组件下载
                timeout: std::time::Duration::from_secs(180),
                ui_key: "tool.lyrics_fetch",
            },
        }
    }
}

#[async_trait]
impl Tool for LyricsFetch {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn risk(&self) -> ToolRisk {
        ToolRisk::Mutating
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> anyhow::Result<String> {
        let files = args
            .get("files")
            .and_then(serde_json::Value::as_array)
            .filter(|a| !a.is_empty())
            .ok_or_else(|| anyhow::anyhow!("缺少 files 参数(音频文件列表)"))?;
        let mut items = Vec::with_capacity(files.len());
        for f in files {
            let path = f
                .get("path")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(super::expand_home) // 「~/xxx」宽容展开(§4.4)
                .ok_or_else(|| anyhow::anyhow!("files 里有一项缺 path"))?;
            let p = std::path::PathBuf::from(path);
            anyhow::ensure!(p.is_absolute(), "path 需要绝对路径,收到: {}", p.display());
            let opt = |key: &str| {
                f.get(key)
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
            };
            items.push(LyricsItem { path: p, title: opt("title"), artist: opt("artist") });
        }
        // 读音频标签 + 旁挂同名 .lrc = 存入档(§7.2 授权圈,含读;音频原件不动)
        let item_paths: Vec<String> =
            items.iter().map(|it| it.path.to_string_lossy().into_owned()).collect();
        super::guard::ensure(ctx, super::guard::Access::Create, &item_paths).await?;

        match ctx.media.lyrics_for_files(items, (ctx.user_id, ctx.conv_id)).await? {
            LyricsBatchOutcome::JobStarted { total } => Ok(format!(
                "已开始在后台给 {total} 个文件找歌词,进度在屏幕任务条上;**跑完会自动回来一条\
                 结果汇报**(成几个、哪些没配上会点名),到时再转述。现在告诉用户已经开工、\
                 跑完会说一声就好。"
            )),
            // 量是一等约束(§7.2):汇总数字 + 只点名要处理的(没找到/缺歌名/无效)。
            LyricsBatchOutcome::Report(report) => {
                Ok(crate::media::compose_batch_summary(&report))
            }
        }
    }
}
