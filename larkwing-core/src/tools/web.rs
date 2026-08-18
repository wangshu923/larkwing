//! 能力轴:外网(搜/读/存)。**搜索即抓取**:web_search 一次调用带回正文证据片段
//! (robot 的"链接堆 + 模型串行 fetch"病根在此修掉);web_fetch 留给"用户给了具体
//! 链接"的场景(带页内链接,配合 web_download 走"打开页面→挑链接→落盘"的下载流,
//! 单据/附件类);web_download 把 URL 存成本地文件。客户端共享/自持在工具单例字段
//! (app 级无归属资产,不进 ToolCtx)。
//! watch-item(PLAN §10):网页内容是不可信文本,注入风险记档;结果只作观察喂回。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use futures_util::future::join_all;

use crate::web::{clip, WebClient};

use super::{Tool, ToolCtx, ToolRisk, ToolSpec};

/// 默认带正文的条数与单篇预算(证据片段,不是整页)。
const CONTENT_TOP_N: usize = 3;
const PIECE_MAX_CHARS: usize = 1200;
const FETCH_MAX_CHARS: usize = 6000;
/// **同步档**体积闸:回合内下完、当场返回路径。发票 PDF → pdf_to_png 那类「下完立刻接
/// 下一步」的链子依赖这个即时性,别改大(大了会把回合卡死在下载上)。
const DOWNLOAD_SYNC_MAX_BYTES: u64 = 50 * 1024 * 1024;
/// **后台档**体积闸:Content-Length 超过同步档就转 job(进度/取消/收尾汇报走 bgtasks)。
/// 数值与 `media::TORRENT_MAX_BYTES` 同口径(都是「影视量级」这一个理由,不另造第二个数)。
const DOWNLOAD_JOB_MAX_BYTES: u64 = 50 * 1024 * 1024 * 1024;

// ---------------------------------------------------------------------------
// web_search
// ---------------------------------------------------------------------------

pub(super) struct WebSearch {
    spec: ToolSpec,
    web: Arc<WebClient>,
}

impl WebSearch {
    pub(super) fn new(web: Arc<WebClient>) -> WebSearch {
        WebSearch {
            spec: ToolSpec {
                name: "web_search",
                description: "上网搜索并带回网页正文片段(天气、新闻、常识查证、用药禁忌这类\
                              要查外部信息的问题)。结果自带前几条的正文摘录,通常不用再单独\
                              读网页;答的时候提一句来源网站名。纯闲聊和你本来就知道的事别搜。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "搜索关键词,中文即可;查时效信息可带上地点/日期"
                        },
                        "count": {
                            "type": "integer",
                            "description": "返回几条,默认 5",
                            "minimum": 1,
                            "maximum": 8
                        },
                        "fetch_content": {
                            "type": "boolean",
                            "description": "是否抓取前几条的正文片段,默认 true;只要链接列表时设 false"
                        }
                    },
                    "required": ["query"]
                }),
                timeout: std::time::Duration::from_secs(40),
                ui_key: "tool.web_search",
            },
            web,
        }
    }
}

#[async_trait]
impl Tool for WebSearch {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: serde_json::Value, _ctx: &ToolCtx) -> anyhow::Result<String> {
        let query = args
            .get("query")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .context("缺少 query 参数")?;
        let count = args
            .get("count")
            .and_then(serde_json::Value::as_u64)
            .map(|n| n.clamp(1, 8) as usize)
            .unwrap_or(5);
        // 宽容解析(同 audio_only 坑):字符串 "false" 也认得,不静默回落默认。
        let with_content = super::arg_bool(&args, "fetch_content", true);

        let hits = self.web.search(query, count).await?;
        if hits.is_empty() {
            return Ok("没搜到相关结果,换个关键词试试".into());
        }

        // 搜索即抓取:前 N 条并发取正文(失败的静默降级为只有摘要)
        let texts: Vec<Option<String>> = if with_content {
            join_all(hits.iter().take(CONTENT_TOP_N).map(|h| {
                let web = self.web.clone();
                let url = h.url.clone();
                async move {
                    match web.fetch_text(&url).await {
                        Ok((_, text)) => Some(clip(&text, PIECE_MAX_CHARS)),
                        Err(e) => {
                            tracing::debug!(url, "正文抓取失败,只给摘要: {e:#}");
                            None
                        }
                    }
                }
            }))
            .await
        } else {
            Vec::new()
        };

        let mut out = String::new();
        for (i, hit) in hits.iter().enumerate() {
            out.push_str(&format!("【{}】{}\n{}\n", i + 1, hit.title, hit.url));
            if !hit.snippet.is_empty() {
                out.push_str(&format!("摘要: {}\n", hit.snippet));
            }
            if let Some(Some(text)) = texts.get(i) {
                out.push_str(&format!("正文片段: {text}\n"));
            }
            out.push('\n');
        }
        Ok(out.trim_end().to_string())
    }
}

// ---------------------------------------------------------------------------
// web_fetch
// ---------------------------------------------------------------------------

pub(super) struct WebFetch {
    spec: ToolSpec,
    web: Arc<WebClient>,
}

impl WebFetch {
    pub(super) fn new(web: Arc<WebClient>) -> WebFetch {
        WebFetch {
            spec: ToolSpec {
                name: "web_fetch",
                description: "读一个具体网页的正文和页内链接(用户给了链接,或 web_search 的\
                              正文片段不够、要看某条的全文时)。长文一次给一段,没读完会在\
                              结果末尾标注「继续读带 offset=N」,带上 offset 再调就接着读。\
                              要从页面里找「下载/查看」按钮背后的地址时也用它:结果末尾列出\
                              页内链接,挑中的交给 web_download 下载。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "url": { "type": "string", "description": "http(s) 网页链接" },
                        "offset": {
                            "type": "integer",
                            "minimum": 0,
                            "description": "长文续读:从第几个字开始(上次结果末尾给出的数);默认 0 从头读"
                        }
                    },
                    "required": ["url"]
                }),
                timeout: std::time::Duration::from_secs(25),
                ui_key: "tool.web_fetch",
            },
            web,
        }
    }
}

#[async_trait]
impl Tool for WebFetch {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: serde_json::Value, _ctx: &ToolCtx) -> anyhow::Result<String> {
        let url = args
            .get("url")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| s.starts_with("http://") || s.starts_with("https://"))
            .context("缺少合法的 url 参数(需要 http(s) 链接)")?;
        let offset = super::arg_u64(&args, "offset", 0) as usize;
        let page = self.web.fetch_page(url).await?;
        // 全文在 WebClient 的短缓存里(缓存单元就是整页成品):续读命中缓存零重抓,
        // 这里只换切片起点。字符计数与 clip 同口径(CJK 安全)。
        let total = page.text.chars().count();
        anyhow::ensure!(
            offset == 0 || offset < total,
            "全文约 {total} 字,offset={offset} 超出末尾——已经读完了"
        );
        let slice: String = page.text.chars().skip(offset).take(FETCH_MAX_CHARS).collect();
        let end = offset + slice.chars().count();
        let mut out = format!("《{}》\n{}\n", page.title, url);
        if offset > 0 || end < total {
            out.push_str(&format!("(全文约 {total} 字,本段第 {offset}–{end} 字)\n"));
        }
        out.push('\n');
        out.push_str(&slice);
        if end < total {
            out.push_str(&format!("\n\n…(未完,继续读带 offset={end})"));
        }
        // 页内链接整页相同,只随首段给一次(续读段重复列出白吃 token)
        if offset == 0 && !page.links.is_empty() {
            out.push_str("\n\n【页内链接】(要下载哪个就把链接交给 web_download)\n");
            for l in &page.links {
                out.push_str(&format!("- {} → {}\n", l.text, l.url));
            }
        }
        Ok(out.trim_end().to_string())
    }
}

// ---------------------------------------------------------------------------
// web_download
// ---------------------------------------------------------------------------

pub(super) struct WebDownload {
    spec: ToolSpec,
    net: crate::net::Client,
}

impl WebDownload {
    pub(super) fn new() -> WebDownload {
        WebDownload {
            spec: ToolSpec {
                name: "web_download",
                description: "把一个链接指向的文件下载到本机(PDF/图片/压缩包/影音等)。配合 \
                              web_fetch:先读页面挑出下载链接,再用这个存盘。默认存到系统\
                              「下载」文件夹,同名不覆盖(自动加「 (2)」)。\
                              **也认 ftp:// 直链**(国内影视资源站常给这种,账号密码写在\
                              地址里也没关系,原样传进来)**和迅雷/快车/旋风专用链**\
                              (thunder:// flashget:// qqdl://)——专用链会自动拆成真实地址;\
                              拆出来若是磁力链会告诉你改用 torrent_download。\
                              ftp 连不上通常是那台服务器已经关了(这类资源站服务器很短命),\
                              不是链接格式问题,如实告诉用户就好。**大文件自动转后台**:小文件当场下完\
                              回路径(可以接着 pdf_to_png 之类);超过 50MB 的转后台跑、\
                              本工具立即返回,进度在任务条上、**跑完自动回来汇报**。\
                              需要账号的地址(WebDAV / 自家 NAS / 网盘挂载)由用户预先在\
                              「设置 · 系统 · 下载认证」里配好账号,这里会自动带上——\
                              **不要向用户索要密码、也不要把密码写进参数**;遇到 401 就\
                              让用户去那里配。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "url": { "type": "string", "description": "http(s) 文件直链,或 thunder:// 等下载器专用链" },
                        "dir": {
                            "type": "string",
                            "description": "存到哪个文件夹(绝对路径);省略 = 系统「下载」文件夹"
                        }
                    },
                    "required": ["url"]
                }),
                timeout: Duration::from_secs(300),
                ui_key: "tool.web_download",
            },
            // 下载客户端与页面抓取分家:页面 15s 总超时对大文件太短。UA 同款(裸 UA 常被拒)。
            net: download_client(Some(Duration::from_secs(280))),
        }
    }
}

#[async_trait]
impl Tool for WebDownload {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn risk(&self) -> ToolRisk {
        ToolRisk::Mutating
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> anyhow::Result<String> {
        let raw = args
            .get("url")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .context("缺少 url 参数")?;
        // thunder:// / flashget:// / qqdl:// 先拆封(§4.4 同族);不是专用链就原样。
        let normalized = super::normalize_link(raw);
        let url = normalized.as_str();
        if url.starts_with("magnet:") || url.starts_with("ed2k://") {
            anyhow::bail!(
                "这个(拆开后)是 {} 链接,不是能直接下的文件地址。磁力链用 torrent_download 下;\
                 ed2k(电驴)我们放不了也下不了,要告诉用户。",
                if url.starts_with("magnet:") { "磁力" } else { "电驴 ed2k" }
            );
        }
        let dir = match args.get("dir").and_then(serde_json::Value::as_str).map(str::trim) {
            Some(d) if !d.is_empty() => {
                let p = PathBuf::from(super::expand_home(d)); // 「~/xxx」宽容展开(§4.4)
                anyhow::ensure!(p.is_absolute(), "dir 需要绝对路径,收到: {d}");
                p
            }
            _ => default_download_dir(),
        };
        // 落盘 = 存入(§7.2 授权圈;缺省「下载」夹在出厂基线内,零打扰)。授权过了才建目录。
        super::guard::ensure(
            ctx,
            super::guard::Access::Create,
            &[dir.to_string_lossy().into_owned()],
        )
        .await?;
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("建不了目标文件夹 {}", dir.display()))?;

        // ftp:// 走单独的协议客户端(不是 HTTP,net::Client 管不着;详见 crate::ftp)。
        // 迅雷/快车专用链拆开后大量就是 ftp,normalize_link 已经把它们拆到这里了。
        if url.starts_with("ftp://") {
            return self.run_ftp(ctx, url, &dir).await;
        }
        anyhow::ensure!(
            url.starts_with("http://") || url.starts_with("https://"),
            "url 需要 http(s) 或 ftp:// 文件地址(或能拆出地址的 thunder:// 专用链),收到: {raw}"
        );
        // 认证按 host 现查(WebDAV / 带账号的直链)。**密码不经模型**(§7.7):它既不在
        // 工具参数里、也不出现在任何回给模型的文本里。
        let cred = crate::web::cred_for(&crate::web::load_http_creds(&ctx.store.settings), url);
        let resp = self.get_with_cred(url, cred.as_ref()).await?;
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            anyhow::bail!(
                "这个地址要账号密码(HTTP 401){}。让用户去「设置 · 系统 · 下载认证」\
                 加一条这个网站的账号,加完再下一次。",
                if cred.is_some() { ",而已配的账号被拒了(密码可能不对)" } else { "" }
            );
        }
        anyhow::ensure!(status.is_success(), "下载失败 HTTP {status}");

        let len = resp.content_length();
        // 大文件转后台:同步档的 300s 工具预算装不下(2GB 按 5MB/s 就要 400s),
        // 转 job 后进度/取消/收尾汇报全走 bgtasks(与 torrent_download 同一套)。
        if let Some(n) = len {
            anyhow::ensure!(
                n <= DOWNLOAD_JOB_MAX_BYTES,
                "文件 {} 超过 {} 上限,不下了",
                super::fs::human_size(n),
                super::fs::human_size(DOWNLOAD_JOB_MAX_BYTES)
            );
            if n > DOWNLOAD_SYNC_MAX_BYTES {
                let name = pick_filename(&resp);
                drop(resp); // 断掉这条连接,job 里重开(省得把 resp 搬进 spawn)
                return self.spawn_job(ctx, url, &dir, name, n, cred);
            }
        }

        let name = pick_filename(&resp);
        // 先写临时件再改名:半截下载绝不顶着正式名躺在下载夹里
        let part = part_path(&dir);
        let total = match stream_to_file(resp, &part, DOWNLOAD_SYNC_MAX_BYTES, None).await {
            Ok(n) => n,
            Err(e) => {
                let _ = std::fs::remove_file(&part);
                return Err(e.context(
                    "(如果是因为超过同步下载上限:这个服务器没报文件大小,\
                     所以没能自动转后台——可以让用户确认后重试)",
                ));
            }
        };
        let dest = crate::files::dedupe_path(&dir.join(&name));
        if let Err(e) = std::fs::rename(&part, &dest) {
            let _ = std::fs::remove_file(&part);
            return Err(anyhow::anyhow!(e).context("落盘改名失败"));
        }
        Ok(format!("已下载到 {}({})", dest.display(), super::fs::human_size(total)))
    }
}

impl WebDownload {
    /// ftp:// 分支。与 http 档位口径完全一致(小文件回合内下完、大文件转后台 job),
    /// 只换协议客户端。凭证优先取 URL 里内嵌的(dytt 那类链接的常态),没有则按 host
    /// 查「设置·下载认证」(自家 NAS 的 FTP 靠这条)。
    async fn run_ftp(&self, ctx: &ToolCtx, url: &str, dir: &std::path::Path) -> anyhow::Result<String> {
        let t = crate::ftp::parse_ftp_url(url)?;
        let creds = crate::web::load_http_creds(&ctx.store.settings);
        let host_key = t.cred_host();
        let cred = creds.iter().find(|c| {
            let h = c.host.trim().to_ascii_lowercase();
            h == host_key.to_ascii_lowercase() || h == t.host.to_ascii_lowercase()
        });
        let t = t.with_cred(cred);

        // 探体积决定档位。取不到(服务器不支持 SIZE)= 走同步档 + 硬闸,与 http 侧
        // 「没有 Content-Length」同口径。
        let size = crate::ftp::probe_size(&t).await?;
        if let Some(n) = size {
            anyhow::ensure!(
                n <= DOWNLOAD_JOB_MAX_BYTES,
                "文件 {} 超过 {} 上限,不下了",
                super::fs::human_size(n),
                super::fs::human_size(DOWNLOAD_JOB_MAX_BYTES)
            );
            if n > DOWNLOAD_SYNC_MAX_BYTES {
                return self.spawn_ftp_job(ctx, t, dir, n);
            }
        }
        let part = part_path(dir);
        let total =
            match crate::ftp::download_to(&t, &part, DOWNLOAD_SYNC_MAX_BYTES, None).await {
                Ok(n) => n,
                Err(e) => {
                    let _ = std::fs::remove_file(&part);
                    return Err(e);
                }
            };
        let dest = crate::files::dedupe_path(&dir.join(&t.filename));
        if let Err(e) = std::fs::rename(&part, &dest) {
            let _ = std::fs::remove_file(&part);
            return Err(anyhow::anyhow!(e).context("落盘改名失败"));
        }
        Ok(format!("已下载到 {}({})", dest.display(), super::fs::human_size(total)))
    }

    /// 大 ftp 文件转后台(影视资源基本都走这条)。
    fn spawn_ftp_job(
        &self,
        ctx: &ToolCtx,
        t: crate::ftp::FtpTarget,
        dir: &std::path::Path,
        total: u64,
    ) -> anyhow::Result<String> {
        let size = super::fs::human_size(total);
        let ticket = ctx.media.bg().submit(
            format!("下载文件({})", t.filename),
            (ctx.user_id, ctx.conv_id),
            100,
        )?;
        let ticket_id = ticket.id();
        // HUD 卡(原先只登记 bgtasks、屏幕上其实看不见,话术却说「在任务条上」——补齐)
        // + 停止钮(bind_bg,§7 通用件)。
        let task = ctx
            .media
            .tasks()
            .start("download", crate::bus::Text::new("task.web_download"));
        task.bind_bg(ticket_id);
        let dir_owned = dir.to_path_buf();
        let name = t.filename.clone();
        let join = tokio::spawn(async move {
            let part = part_path(&dir_owned);
            let outcome = async {
                let got = crate::ftp::download_to(
                    &t,
                    &part,
                    DOWNLOAD_JOB_MAX_BYTES,
                    Some((&ticket, total)),
                )
                .await?;
                let dest = crate::files::dedupe_path(&dir_owned.join(&name));
                std::fs::rename(&part, &dest).context("落盘改名失败")?;
                Ok::<_, anyhow::Error>((dest, got))
            }
            .await;
            // 用户点了停(HUD 停止钮 / task_cancel)→ 传输层 bail「按要求停下了」落进 Err 臂:
            // 按停下收尾,别把主动叫停报成「下载失败」(评审实锤:文案自相矛盾)。
            let cancelled = ticket.is_cancelled();
            let (ok, text) = match outcome {
                Ok((dest, got)) => (
                    true,
                    format!(
                        "《{name}》下好了({}),存到 {}。把结果简短告诉用户。",
                        super::fs::human_size(got),
                        dest.display()
                    ),
                ),
                Err(_) if cancelled => {
                    let _ = std::fs::remove_file(&part);
                    (false, format!("《{name}》的下载按要求停下了,没下完的部分已清理。"))
                }
                Err(e) => {
                    let _ = std::fs::remove_file(&part);
                    tracing::warn!("ftp job 失败: {e:#}");
                    (false, format!("《{name}》没下成:{e:#}。把原因如实告诉用户。"))
                }
            };
            if ok {
                task.done();
            } else if cancelled {
                task.fail("task.err.cancelled", serde_json::Value::Null);
            } else {
                task.fail("task.err.download", serde_json::Value::Null);
            }
            ticket.finish(ok, text);
        });
        ctx.media.bg().attach_abort(ticket_id, join.abort_handle());
        Ok(format!(
            "这个文件有 {size},已经转后台下了(存到 {})。进度在屏幕任务条上;\
             **跑完会自动回来汇报**,到时再转述。现在告诉用户已经开工就好。",
            dir.display()
        ))
    }

    /// 带认证的 GET(没凭证 = 匿名,与从前逐字节一致)。
    async fn get_with_cred(
        &self,
        url: &str,
        cred: Option<&crate::web::HttpCred>,
    ) -> anyhow::Result<reqwest::Response> {
        self.net
            .send(url, |c| match cred {
                Some(cd) => c.get(url).basic_auth(&cd.user, Some(&cd.password)),
                None => c.get(url),
            })
            .await
            .context("下载请求失败")
    }

    /// 大文件:登记进后台差事处,立即返回。
    fn spawn_job(
        &self,
        ctx: &ToolCtx,
        url: &str,
        dir: &std::path::Path,
        name: String,
        total: u64,
        cred: Option<crate::web::HttpCred>,
    ) -> anyhow::Result<String> {
        let size = super::fs::human_size(total);
        let ticket = ctx.media.bg().submit(
            format!("下载文件({name})"),
            (ctx.user_id, ctx.conv_id),
            100,
        )?;
        let ticket_id = ticket.id();
        // HUD 卡 + 停止钮(§7 通用件;原先只登记 bgtasks,屏幕上其实看不见)
        let task = ctx
            .media
            .tasks()
            .start("download", crate::bus::Text::new("task.web_download"));
        task.bind_bg(ticket_id);
        let net = download_client(None); // 后台档:不设总超时(见 download_client 注释)
        let url_owned = url.to_string();
        let dir_owned = dir.to_path_buf();
        let name_owned = name.clone();
        let join = tokio::spawn(async move {
            let part = part_path(&dir_owned);
            let report = async {
                let resp = net
                    .send(&url_owned, |c| match cred.as_ref() {
                        Some(cd) => c.get(&url_owned).basic_auth(&cd.user, Some(&cd.password)),
                        None => c.get(&url_owned),
                    })
                    .await
                    .context("下载请求失败")?;
                anyhow::ensure!(resp.status().is_success(), "下载失败 HTTP {}", resp.status());
                let got =
                    stream_to_file(resp, &part, DOWNLOAD_JOB_MAX_BYTES, Some((&ticket, total)))
                        .await?;
                let dest = crate::files::dedupe_path(&dir_owned.join(&name_owned));
                std::fs::rename(&part, &dest).context("落盘改名失败")?;
                // 回一个 (路径, 字节) 元组,**不要**把两者拼成一个字符串再 split ——
                // mac/Linux 的文件名允许含 `|`,拼串会在这种路径上切错。
                Ok::<_, anyhow::Error>((dest, got))
            }
            .await;
            // 同 ftp job:主动叫停按「停下」收尾,不冒充「下载失败」。
            let cancelled = ticket.is_cancelled();
            let (ok, text) = match report {
                Ok((dest, got)) => (
                    true,
                    format!(
                        "《{name_owned}》下好了({}),存到 {}。把结果简短告诉用户。",
                        super::fs::human_size(got),
                        dest.display()
                    ),
                ),
                Err(_) if cancelled => {
                    let _ = std::fs::remove_file(&part);
                    (false, format!("《{name_owned}》的下载按要求停下了,没下完的部分已清理。"))
                }
                Err(e) => {
                    let _ = std::fs::remove_file(&part);
                    tracing::warn!("web_download job 失败: {e:#}");
                    (false, format!("《{name_owned}》没下成:{e:#}。把原因如实告诉用户。"))
                }
            };
            if ok {
                task.done();
            } else if cancelled {
                task.fail("task.err.cancelled", serde_json::Value::Null);
            } else {
                task.fail("task.err.download", serde_json::Value::Null);
            }
            ticket.finish(ok, text);
        });
        ctx.media.bg().attach_abort(ticket_id, join.abort_handle());
        Ok(format!(
            "这个文件有 {size},已经转后台下了(存到 {})。进度在屏幕任务条上;\
             **跑完会自动回来汇报**,到时再转述。现在告诉用户已经开工就好。",
            dir.display()
        ))
    }
}

/// web_download 的 HTTP 客户端。同步档与后台档共用 UA / 连接超时,**只有总超时不同**:
/// 同步档 280s(回合内够用);后台档 `None` = 不设总超时 —— 几 GB 的文件跑几十分钟是
/// 常态,280s 会把它腰斩;停不下来那头由票据取消 + bgtasks 的卡死看门狗兜。
fn download_client(total_timeout: Option<Duration>) -> crate::net::Client {
    crate::net::Client::new(move |b| {
        let b = b.user_agent(crate::web::UA).connect_timeout(Duration::from_secs(10));
        match total_timeout {
            Some(t) => b.timeout(t),
            None => b,
        }
    })
}

/// 临时件路径:半截下载绝不顶着正式名躺在下载夹里。
fn part_path(dir: &std::path::Path) -> PathBuf {
    dir.join(format!(
        ".lw-download-{}-{}.part",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
    ))
}

use crate::files::{default_download_dir, sanitize_filename};

/// 文件名:Content-Disposition(filename* 优先)→ 最终 URL 末段 → 兜底名;
/// 非法字符替换、Windows 保留名规避(files::validate_name 口径),无扩展名按 MIME 补。
fn pick_filename(resp: &reqwest::Response) -> String {
    let cd_name = resp
        .headers()
        .get(reqwest::header::CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .and_then(cd_filename);
    let url_name = || {
        resp.url()
            .path_segments()
            .and_then(|mut s| s.next_back())
            .filter(|s| !s.is_empty())
            .map(crate::web::percent_decode)
    };
    let raw = cd_name.or_else(url_name).unwrap_or_default();
    let mut name = sanitize_filename(&raw);
    if !name.contains('.') {
        let mime = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if let Some(ext) = ext_for_mime(mime) {
            name = format!("{name}.{ext}");
        }
    }
    name
}

/// Content-Disposition 里的文件名:RFC5987 `filename*=UTF-8''…`(百分号编码)优先,
/// 退回普通 `filename="…"`。解析尽力而为,取不出交回 None 走 URL 末段。
fn cd_filename(v: &str) -> Option<String> {
    let lower = v.to_ascii_lowercase();
    if let Some(pos) = lower.find("filename*=") {
        let raw = v[pos + "filename*=".len()..].split(';').next().unwrap_or("").trim();
        // 形如 UTF-8''%E9%99%84%E4%BB%B6.pdf(charset'lang'value)
        let enc = raw.splitn(3, '\'').nth(2).unwrap_or(raw);
        let name = crate::web::percent_decode(enc.trim_matches('"'));
        if !name.trim().is_empty() {
            return Some(name);
        }
    }
    if let Some(pos) = lower.find("filename=") {
        let raw = v[pos + "filename=".len()..].split(';').next().unwrap_or("").trim();
        let name = raw.trim_matches('"').trim();
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }
    None
}

/// MIME → 扩展名(只补常见几种,认不出就不补——名字没后缀也能存)。
fn ext_for_mime(ct: &str) -> Option<&'static str> {
    Some(match ct.split(';').next().unwrap_or("").trim() {
        "application/pdf" => "pdf",
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "application/zip" => "zip",
        "text/html" => "html",
        "text/plain" => "txt",
        _ => return None,
    })
}

/// 流式写盘 + 体积硬闸(超限即停,调用方负责清理临时件)。返回写入字节数。
/// `progress` = 后台档才传:`(票据, 预期总字节)`,每 ~1MB 打一次点(顺带查取消)。
async fn stream_to_file(
    mut resp: reqwest::Response,
    dest: &std::path::Path,
    cap: u64,
    progress: Option<(&crate::bgtasks::BgTicket, u64)>,
) -> anyhow::Result<u64> {
    use std::io::Write;
    let mut f = std::fs::File::create(dest)
        .with_context(|| format!("建不了文件 {}", dest.display()))?;
    let mut total: u64 = 0;
    let mut next_beat: u64 = 0;
    while let Some(chunk) = resp.chunk().await.context("下载中断")? {
        total += chunk.len() as u64;
        anyhow::ensure!(
            total <= cap,
            "文件超过 {} 上限,已停止",
            super::fs::human_size(cap)
        );
        f.write_all(&chunk)?;
        if let Some((ticket, expect)) = progress {
            if ticket.is_cancelled() {
                anyhow::bail!("按要求停下了");
            }
            if total >= next_beat {
                next_beat = total + 1024 * 1024;
                let pct = total.saturating_mul(100).checked_div(expect).unwrap_or(0);
                ticket.beat(
                    pct as usize,
                    format!("{} / {}", super::fs::human_size(total), super::fs::human_size(expect)),
                );
            }
        }
    }
    f.flush()?;
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::MediaRuntime;
    use crate::store::Store;

    fn ctx(tag: &str) -> ToolCtx {
        let dir = std::env::temp_dir().join(format!("lw-webtool-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _ = std::fs::remove_file(dir.join("t.db"));
        let store = Store::open(&dir.join("t.db")).unwrap();
        ToolCtx { user_id: 1, conv_id: 1, media: MediaRuntime::detached(store.clone()), store, web: None, voice: None, confirm: None, grants: Default::default(), agent: None }
    }

    #[tokio::test]
    async fn web_fetch_reads_local_page_and_rejects_bad_url() {
        use axum::{routing::get, Router};
        async fn page() -> axum::response::Html<&'static str> {
            axum::response::Html(
                "<html><title>说明书</title><body><p>这一段是足够长的正文,用来验证抓取链路。</p></body></html>",
            )
        }
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, Router::new().route("/doc", get(page))).await.ok();
        });

        let ctx = ctx("fetch");
        let web = Arc::new(WebClient::new());
        let tool = WebFetch::new(web);
        let out = tool
            .run(serde_json::json!({"url": format!("http://127.0.0.1:{port}/doc")}), &ctx)
            .await
            .unwrap();
        assert!(out.contains("《说明书》") && out.contains("足够长的正文"));

        assert!(tool.run(serde_json::json!({"url": "ftp://x"}), &ctx).await.is_err());
    }

    #[tokio::test]
    async fn web_search_requires_query() {
        let ctx = ctx("search");
        let tool = WebSearch::new(Arc::new(WebClient::new()));
        assert!(tool.run(serde_json::json!({}), &ctx).await.is_err());
    }

    #[tokio::test]
    async fn web_fetch_lists_in_page_links() {
        use axum::{routing::get, Router};
        async fn page() -> axum::response::Html<&'static str> {
            axum::response::Html(
                "<html><title>附件页</title><body><p>这一段是足够长的正文,用来验证抓取链路。</p>\
                 <a href=\"/dl/fp1.pdf\">下载附件</a></body></html>",
            )
        }
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, Router::new().route("/v", get(page))).await.ok();
        });

        let ctx = ctx("fetch-links");
        let tool = WebFetch::new(Arc::new(WebClient::new()));
        let out = tool
            .run(serde_json::json!({"url": format!("http://127.0.0.1:{port}/v")}), &ctx)
            .await
            .unwrap();
        assert!(out.contains("【页内链接】"), "{out}");
        assert!(out.contains(&format!("下载附件 → http://127.0.0.1:{port}/dl/fp1.pdf")), "{out}");
    }

    #[tokio::test]
    async fn web_fetch_offset_paginates_long_page() {
        use axum::{routing::get, Router};
        async fn page() -> axum::response::Html<String> {
            axum::response::Html(format!(
                "<html><title>长文</title><body><p>{}</p><a href=\"/att.pdf\">附件</a></body></html>",
                "甲".repeat(7000)
            ))
        }
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, Router::new().route("/long", get(page))).await.ok();
        });

        let ctx = ctx("fetch-offset");
        let tool = WebFetch::new(Arc::new(WebClient::new()));
        let url = format!("http://127.0.0.1:{port}/long");

        // 首段:范围标注 + 续读指引 + 页内链接只随首段给
        let p1 = tool.run(serde_json::json!({"url": url}), &ctx).await.unwrap();
        assert!(p1.contains("(全文约 7000 字,本段第 0–6000 字)"), "范围标注");
        assert!(p1.contains("…(未完,继续读带 offset=6000)"));
        assert!(p1.contains("【页内链接】"));

        // 第二段(命中缓存零重抓):读到尾,无「未完」、不重复列链接
        let p2 = tool.run(serde_json::json!({"url": url, "offset": 6000}), &ctx).await.unwrap();
        assert!(p2.contains("(全文约 7000 字,本段第 6000–7000 字)"));
        assert!(!p2.contains("未完") && !p2.contains("【页内链接】"));
        assert_eq!(p2.chars().filter(|c| *c == '甲').count(), 1_000);

        // 超出末尾 = 明确报错(带总长)
        let err =
            tool.run(serde_json::json!({"url": url, "offset": 99_999}), &ctx).await.unwrap_err();
        assert!(err.to_string().contains("超出末尾"), "{err:#}");
    }

    #[tokio::test]
    async fn web_fetch_rejects_pdf_and_binary_with_guidance() {
        use axum::{http::header, routing::get, Router};
        async fn pdf() -> impl axum::response::IntoResponse {
            ([(header::CONTENT_TYPE, "application/pdf")], &b"%PDF-1.4 fake"[..])
        }
        async fn zip() -> impl axum::response::IntoResponse {
            ([(header::CONTENT_TYPE, "application/zip")], &b"PK\x03\x04junk"[..])
        }
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route("/f.pdf", get(pdf)).route("/a.zip", get(zip)),
            )
            .await
            .ok();
        });

        let ctx = ctx("fetch-binary");
        let tool = WebFetch::new(Arc::new(WebClient::new()));
        let err = tool
            .run(serde_json::json!({"url": format!("http://127.0.0.1:{port}/f.pdf")}), &ctx)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("PDF") && err.to_string().contains("web_download"),
            "{err:#}"
        );
        let err = tool
            .run(serde_json::json!({"url": format!("http://127.0.0.1:{port}/a.zip")}), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("web_download"), "{err:#}");
    }

    #[tokio::test]
    async fn web_download_saves_names_and_never_overwrites() {
        use axum::{http::header, routing::get, Router};
        async fn file() -> impl axum::response::IntoResponse {
            (
                [
                    (header::CONTENT_TYPE, "application/pdf"),
                    (header::CONTENT_DISPOSITION, "attachment; filename*=UTF-8''%E9%99%84%E4%BB%B6.pdf"),
                ],
                &b"%PDF-1.4 fake"[..],
            )
        }
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, Router::new().route("/f", get(file))).await.ok();
        });

        let ctx = ctx("download");
        let dir = std::env::temp_dir().join(format!("lw-dl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let tool = WebDownload::new();
        let args = serde_json::json!({
            "url": format!("http://127.0.0.1:{port}/f"),
            "dir": dir.to_string_lossy(),
        });
        let out1 = tool.run(args.clone(), &ctx).await.unwrap();
        assert!(out1.contains("附件.pdf"), "CD filename* 生效: {out1}");
        assert_eq!(std::fs::read(dir.join("附件.pdf")).unwrap(), b"%PDF-1.4 fake");
        // 再下同名 → 自动 (2),永不覆盖
        let out2 = tool.run(args, &ctx).await.unwrap();
        assert!(out2.contains("附件 (2).pdf"), "同名去重: {out2}");
        // 无临时件残留
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".part"))
            .collect();
        assert!(leftovers.is_empty(), "不留 .part 残件");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ftp:// 要被路由进 ftp 分支,而不是撞「需要 http(s)」的闸。
    /// 用「目录形」ftp 地址:parse_ftp_url 会在**建连之前**就退回,所以这个测试不联网、很快。
    #[tokio::test]
    async fn ftp_url_routes_into_ftp_branch() {
        let ctx = ctx("ftproute");
        let tool = WebDownload::new();
        let dir = std::env::temp_dir().join(format!("lw-ftproute-{}", std::process::id()));
        let err = tool
            .run(
                serde_json::json!({ "url": "ftp://h.example.com/", "dir": dir.to_string_lossy() }),
                &ctx,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("不下整个目录"), "该走 ftp 解析而非 http 闸: {err}");
        assert!(!err.contains("需要 http(s)"), "不该被 http 闸拦下: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 迅雷专用链拆出 ftp:// 后也要能进 ftp 分支(两件事的闭环)。
    #[tokio::test]
    async fn thunder_link_unwrapping_to_ftp_reaches_ftp_branch() {
        use base64::Engine;
        let inner = "ftp://h.example.com/"; // 同上:目录形,不建连
        let link = format!(
            "thunder://{}",
            base64::engine::general_purpose::STANDARD.encode(format!("AA{inner}ZZ"))
        );
        let ctx = ctx("ftpthunder");
        let tool = WebDownload::new();
        let dir = std::env::temp_dir().join(format!("lw-ftpth-{}", std::process::id()));
        let err = tool
            .run(serde_json::json!({ "url": link, "dir": dir.to_string_lossy() }), &ctx)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("不下整个目录"), "thunder→ftp 该闭环: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn filename_helpers_sanitize_and_parse() {
        assert_eq!(cd_filename("attachment; filename=\"a b.pdf\""), Some("a b.pdf".into()));
        assert_eq!(
            cd_filename("attachment; filename*=UTF-8''%E4%B8%AD.pdf; size=1"),
            Some("中.pdf".into())
        );
        assert_eq!(cd_filename("inline"), None);
        assert_eq!(sanitize_filename("a<b>:c.pdf"), "a_b__c.pdf");
        assert_eq!(sanitize_filename("  "), "下载文件");
        assert_eq!(sanitize_filename("CON.txt"), "_CON.txt", "Windows 保留名前缀规避");
        assert_eq!(ext_for_mime("application/pdf; charset=x"), Some("pdf"));
        assert_eq!(ext_for_mime("application/x-unknown"), None);
    }
}
