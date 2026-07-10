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
/// web_download 体积闸(与渠道发文件的上限同数量级,守内存/磁盘两头)。
const DOWNLOAD_MAX_BYTES: u64 = 50 * 1024 * 1024;

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
                              正文片段不够、要看某条的全文时)。要从页面里找「下载/查看」\
                              按钮背后的地址时也用它:结果末尾列出页内链接,挑中的交给 \
                              web_download 下载。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "url": { "type": "string", "description": "http(s) 网页链接" }
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
        let page = self.web.fetch_page(url).await?;
        let mut out = format!("《{}》\n{}\n\n{}", page.title, url, clip(&page.text, FETCH_MAX_CHARS));
        if !page.links.is_empty() {
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
                description: "把一个链接指向的文件下载到本机(PDF/图片/压缩包等)。配合 \
                              web_fetch:先读页面挑出下载链接,再用这个存盘。默认存到系统\
                              「下载」文件夹,同名不覆盖(自动加「 (2)」);下载完把落盘路径\
                              告诉用户。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "url": { "type": "string", "description": "http(s) 文件直链" },
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
            net: crate::net::Client::new(|b| {
                b.user_agent(crate::web::UA)
                    .connect_timeout(Duration::from_secs(10))
                    .timeout(Duration::from_secs(280))
            }),
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

    async fn run(&self, args: serde_json::Value, _ctx: &ToolCtx) -> anyhow::Result<String> {
        let url = args
            .get("url")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| s.starts_with("http://") || s.starts_with("https://"))
            .context("缺少合法的 url 参数(需要 http(s) 直链)")?;
        let dir = match args.get("dir").and_then(serde_json::Value::as_str).map(str::trim) {
            Some(d) if !d.is_empty() => {
                let p = PathBuf::from(d);
                anyhow::ensure!(p.is_absolute(), "dir 需要绝对路径,收到: {d}");
                p
            }
            _ => default_download_dir(),
        };
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("建不了目标文件夹 {}", dir.display()))?;

        let resp = self.net.send(url, |c| c.get(url)).await.context("下载请求失败")?;
        let status = resp.status();
        anyhow::ensure!(status.is_success(), "下载失败 HTTP {status}");
        // 服务器自报体积先拦一道(实际下多少仍按流式计数硬闸)
        if let Some(len) = resp.content_length() {
            anyhow::ensure!(
                len <= DOWNLOAD_MAX_BYTES,
                "文件 {} 超过 {} 上限,不下了",
                super::fs::human_size(len),
                super::fs::human_size(DOWNLOAD_MAX_BYTES)
            );
        }
        let name = pick_filename(&resp);

        // 先写临时件再改名:半截下载绝不顶着正式名躺在下载夹里
        let part = dir.join(format!(
            ".lw-download-{}-{}.part",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        let written = stream_to_file(resp, &part).await;
        let total = match written {
            Ok(n) => n,
            Err(e) => {
                let _ = std::fs::remove_file(&part);
                return Err(e);
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
async fn stream_to_file(mut resp: reqwest::Response, dest: &std::path::Path) -> anyhow::Result<u64> {
    use std::io::Write;
    let mut f = std::fs::File::create(dest)
        .with_context(|| format!("建不了文件 {}", dest.display()))?;
    let mut total: u64 = 0;
    while let Some(chunk) = resp.chunk().await.context("下载中断")? {
        total += chunk.len() as u64;
        anyhow::ensure!(
            total <= DOWNLOAD_MAX_BYTES,
            "文件超过 {} 上限,已停止",
            super::fs::human_size(DOWNLOAD_MAX_BYTES)
        );
        f.write_all(&chunk)?;
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
        ToolCtx { user_id: 1, conv_id: 1, media: MediaRuntime::detached(store.clone()), store, web: None }
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
