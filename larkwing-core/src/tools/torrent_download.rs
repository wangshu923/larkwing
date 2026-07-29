//! 能力轴:影音(下·BT)。把**用户给的**磁力链 / `.torrent` 文件下载成本地文件。
//!
//! 定位同 `web_download`:**链接由用户提供,我们只负责下**。工具不去任何站找资源、
//! 不内置片源搜索(§7.1 版权口径的兑现方式)。下完的东西走 `media_play` 放 —— 本地
//! 播放链现成,这个工具只管把文件弄到硬盘上。

use async_trait::async_trait;

use super::{Tool, ToolCtx, ToolRisk, ToolSpec};
use crate::files::default_download_dir;
use crate::media::{TorrentLink, DEFAULT_ONLY_RE};

pub(super) struct TorrentDownload {
    spec: ToolSpec,
}

impl TorrentDownload {
    pub(super) fn new() -> TorrentDownload {
        TorrentDownload {
            spec: ToolSpec {
                name: "torrent_download",
                description: "用 BT 下载**用户给出的**磁力链(magnet:…)或本地 .torrent 文件。\
                              自己不去任何网站找资源、也没有搜索功能——链接必须是用户提供的,\
                              没有链接就问用户要。**有 .torrent 文件就优先用文件**(比磁力链\
                              可靠得多:磁力链要先从 DHT 网络查种子信息,这一步在国内经常\
                              超时);.torrent 传本机绝对路径(网上的 .torrent 先用 \
                              web_download 存下来再给我)。存到哪:用户点名了目录、或任务\
                              需知里记了影片目录就传 dir;都没有就省略(落系统「下载」文件夹)。\
                              默认只下里面的视频文件(自动滤掉样片、nfo、广告 txt);要连\
                              字幕之类一起下就传 only(比如 only=\".\" 表示全下,\
                              only=\"\\\\.srt$\" 表示只要字幕)。**下载在后台跑、本工具立即\
                              返回**:进度在屏幕任务条上,用户问「下到哪了」看〔此刻〕背景\
                              或用 task_status,要停用 task_cancel;**跑完会自动回来汇报**,\
                              到时再转述。下好之后用 media_play 放那个文件夹。\
                              速度取决于这个种子有多少人在做种,快慢我们决定不了;\
                              没人做种的种子会下不动、会如实告诉你。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "link": {
                            "type": "string",
                            "description": "用户给的磁力链(magnet:?xt=urn:btih:…)或本机 .torrent 文件绝对路径(支持 ~ 开头)"
                        },
                        "dir": {
                            "type": "string",
                            "description": "存到哪个文件夹(绝对路径,支持 ~ 开头);省略 = 系统「下载」文件夹"
                        },
                        "only": {
                            "type": "string",
                            "description": "只下文件名匹配这个正则的文件;省略 = 只下视频文件。全下传 \".\""
                        }
                    },
                    "required": ["link"]
                }),
                // 只等「建引擎 + 登记」,真下载在后台;不需要大额度
                timeout: std::time::Duration::from_secs(60),
                ui_key: "tool.torrent_download",
            },
        }
    }
}

#[async_trait]
impl Tool for TorrentDownload {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn risk(&self) -> ToolRisk {
        ToolRisk::Mutating
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> anyhow::Result<String> {
        let raw = args
            .get("link")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!("缺少 link 参数(要用户给的磁力链或本机 .torrent 路径)")
            })?;
        // 磁力链形状 / .torrent 读盘都在这儿同步做完 —— 参数不对要当场退回给模型,
        // 别等进了后台 job 才炸(那时只能靠收尾汇报,绕一大圈)。
        let link = TorrentLink::parse(raw)?;

        let dir = match args.get("dir").and_then(serde_json::Value::as_str).map(str::trim) {
            Some(d) if !d.is_empty() => {
                let p = std::path::PathBuf::from(super::expand_home(d)); // 「~/xxx」宽容展开(§4.4)
                anyhow::ensure!(p.is_absolute(), "dir 需要绝对路径,收到: {d}");
                p
            }
            _ => default_download_dir(),
        };

        // 省略 only = 缺省视频白名单;显式传了就用用户/模型给的(空串当没传)
        let only = match args.get("only").and_then(serde_json::Value::as_str).map(str::trim) {
            Some(s) if !s.is_empty() => Some(s.to_string()),
            _ => Some(DEFAULT_ONLY_RE.to_string()),
        };

        let outcome = ctx
            .media
            .torrent_download(link, &dir, only, (ctx.user_id, ctx.conv_id))
            .await?;
        Ok(match outcome {
            crate::media::TorrentOutcome::Started { label, dir } => format!(
                "已经开始下了({label}),存到 {}。BT 要先找到做种的人才会起速,\
                 前一会儿可能没动静是正常的。进度在屏幕任务条上;**跑完会自动回来汇报**,\
                 到时再转述。现在告诉用户已经开工、跑完会说一声就好——别自己反复查进度。",
                dir.display()
            ),
        })
    }
}
