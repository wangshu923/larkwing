//! 联网问答的地基:搜索源解析 + 正文抽取 + 短 TTL 缓存。
//! robot web 插件的病根 = 搜索只回链接堆、模型还要串行 fetch(多一轮往返、摘要看引擎
//! 脸色)→ 这里**搜索即抓取**:工具一次调用带回正文证据片段。
//! 源 = Bing 中文优先、DDG 兜底,按序尝试;选择器没法数据化(是代码不是数据),
//! 站点改版坏了改这里 —— 与 bilibili 搜索同一立场,诚实记档。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use scraper::{Html, Selector};

/// 同 URL 正文短缓存:防同一回合/相邻回合重复抓(任务 HUD 不掺和,这层全静默)。
const CACHE_TTL: Duration = Duration::from_secs(600);
/// 像真浏览器的 UA(裸 reqwest 常被搜索页拒);web_download 与壳层 webrender 隐藏窗同款
/// (单源,§4.11——渲染窗与抓取端 UA 一致,免得同一站点见到两副面孔)。
pub const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                  (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// 页内链接(锚文本 + 绝对地址):web_fetch 靠它让模型从页面里挑出「下载/跳转」
/// 目标(下载页这类"再点一下"的流程),交给 web_download 落盘。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PageLink {
    pub text: String,
    pub url: String,
}

/// 一次抓取的成品(缓存单元):标题 + 正文 + 页内链接。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Page {
    pub title: String,
    pub text: String,
    #[serde(default)]
    pub links: Vec<PageLink>,
}

/// 页内链接收集上限(给模型的预算闸,取文档序前 N 条)。
const LINKS_MAX: usize = 25;

pub struct WebClient {
    net: crate::net::Client,
    cache: Mutex<HashMap<String, (Instant, String)>>,
}

impl Default for WebClient {
    fn default() -> Self {
        Self::new()
    }
}

impl WebClient {
    pub fn new() -> WebClient {
        let net = crate::net::Client::new(|b| {
            b.user_agent(UA).connect_timeout(Duration::from_secs(8)).timeout(Duration::from_secs(15))
        });
        WebClient { net, cache: Mutex::new(HashMap::new()) }
    }

    /// 搜索:Bing(中文质量好)→ DDG html 版兜底。全军覆没才报错。
    pub async fn search(&self, query: &str, count: usize) -> Result<Vec<SearchHit>> {
        // 命中即返回;否则把 Bing 的死因带去兜底分支(单赋值,无 Option 摆设)
        let bing_err: anyhow::Error = match self.search_bing(query, count).await {
            Ok(hits) if !hits.is_empty() => return Ok(hits),
            Ok(_) => anyhow::anyhow!("Bing 返回空结果"),
            Err(e) => {
                tracing::warn!("Bing 搜索失败,换 DDG: {e:#}");
                e
            }
        };
        match self.search_ddg(query, count).await {
            Ok(hits) if !hits.is_empty() => Ok(hits),
            Ok(_) => bail!("两个搜索源都没有结果(Bing: {bing_err})"),
            Err(e) => bail!("两个搜索源都失败(Bing: {bing_err};DDG: {e})"),
        }
    }

    async fn search_bing(&self, query: &str, count: usize) -> Result<Vec<SearchHit>> {
        let url = "https://www.bing.com/search";
        let html = self
            .net
            .send(url, |c| c.get(url).query(&[("q", query), ("setlang", "zh-hans"), ("count", "10")]))
            .await?
            .error_for_status()?
            .text()
            .await?;
        Ok(parse_bing(&html, count))
    }

    async fn search_ddg(&self, query: &str, count: usize) -> Result<Vec<SearchHit>> {
        let url = "https://html.duckduckgo.com/html/";
        let html = self
            .net
            .send(url, |c| c.get(url).query(&[("q", query)]))
            .await?
            .error_for_status()?
            .text()
            .await?;
        Ok(parse_ddg(&html, count))
    }

    /// 抓正文(带短缓存):返回 (标题, 正文)。cap 由调用方按用途裁。
    pub async fn fetch_text(&self, url: &str) -> Result<(String, String)> {
        let page = self.fetch_page(url).await?;
        Ok((page.title, page.text))
    }

    /// 抓整页成品(带短缓存):标题 + 正文 + 页内链接。web_fetch 用它;搜索的正文
    /// 片段路径走 `fetch_text` 薄壳(链接用不上,但共享同一份缓存)。
    pub async fn fetch_page(&self, url: &str) -> Result<Page> {
        if let Some(hit) = self.cache_get(url) {
            if let Ok(page) = serde_json::from_str::<Page>(&hit) {
                return Ok(page);
            }
        }
        let resp = self.net.send(url, |c| c.get(url)).await.context("页面请求失败")?;
        let status = resp.status();
        anyhow::ensure!(status.is_success(), "页面 HTTP {status}");
        // 重定向后以最终地址为基准解析相对链接(下载类站点常见跳转);bytes() 前先取
        let final_url = resp.url().to_string();
        let ctype = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        // 体积闸:10MB 封顶,防超大页面拖死
        let bytes = resp.bytes().await?;
        anyhow::ensure!(bytes.len() <= 10 * 1024 * 1024, "页面超过 10MB,放弃");
        // 搜索结果/页内链接常直指 PDF 等文件:当 HTML 解析只会出乱码,如实拦下指路
        if let Some(hint) = non_page_hint(&ctype, &bytes) {
            bail!("{hint}");
        }
        let html = String::from_utf8_lossy(&bytes);
        let page = extract_page(&html, &final_url);
        anyhow::ensure!(!page.text.trim().is_empty(), "页面没有可读正文(可能是纯脚本应用)");
        self.cache_put(url, &page);
        Ok(page)
    }

    fn cache_get(&self, url: &str) -> Option<String> {
        let mut cache = self.cache.lock().expect("web cache lock poisoned");
        cache.retain(|_, (at, _)| at.elapsed() < CACHE_TTL);
        cache.get(url).map(|(_, v)| v.clone())
    }

    fn cache_put(&self, url: &str, page: &Page) {
        let json = match serde_json::to_string(page) {
            Ok(j) => j,
            Err(_) => return, // 序列化失败只丢缓存,不丢结果
        };
        let mut cache = self.cache.lock().expect("web cache lock poisoned");
        cache.insert(url.to_string(), (Instant::now(), json));
    }
}

/// 直链不是网页(PDF/压缩包/图片…)→ 给模型一句指路话术(该下载走 web_download,
/// PDF 下完用 fs_read_text 读),绝不把二进制硬当 HTML 解析出乱码。误拦比漏拦贵——
/// 只认「明确的二进制 Content-Type / %PDF 魔数」;text/*、html/xml/json、没报
/// Content-Type 的一律照旧当页面解析。
fn non_page_hint(ctype: &str, bytes: &[u8]) -> Option<String> {
    if ctype == "application/pdf" || bytes.starts_with(b"%PDF-") {
        return Some(
            "这个链接是 PDF 文件不是网页——用 web_download 下载到本机,再用 fs_read_text 读内容"
                .into(),
        );
    }
    let page_like = ctype.is_empty()
        || ctype.starts_with("text/")
        || ctype.contains("html")
        || ctype.contains("xml")
        || ctype.contains("json");
    if page_like {
        return None;
    }
    Some(format!("这个链接不是网页(内容类型 {ctype})——要保存这个文件的话用 web_download 下载"))
}

fn sel(s: &str) -> Selector {
    Selector::parse(s).expect("静态选择器必须合法")
}

/// Bing 结果页:li.b_algo → h2>a(标题/链接)+ .b_caption p(摘要)。
fn parse_bing(html: &str, count: usize) -> Vec<SearchHit> {
    let doc = Html::parse_document(html);
    let (item, link, cap) = (sel("li.b_algo"), sel("h2 a"), sel(".b_caption p"));
    doc.select(&item)
        .filter_map(|it| {
            let a = it.select(&link).next()?;
            let url = a.value().attr("href")?.to_string();
            if !url.starts_with("http") {
                return None;
            }
            Some(SearchHit {
                title: a.text().collect::<String>().trim().to_string(),
                url,
                snippet: it
                    .select(&cap)
                    .next()
                    .map(|p| p.text().collect::<String>().trim().to_string())
                    .unwrap_or_default(),
            })
        })
        .take(count)
        .collect()
}

/// DDG html 版:.result → a.result__a(标题;href 藏在 uddg= 跳转参数里)+ .result__snippet。
fn parse_ddg(html: &str, count: usize) -> Vec<SearchHit> {
    let doc = Html::parse_document(html);
    let (item, link, snip) = (sel("div.result"), sel("a.result__a"), sel(".result__snippet"));
    doc.select(&item)
        .filter_map(|it| {
            let a = it.select(&link).next()?;
            let raw = a.value().attr("href")?;
            let url = decode_uddg(raw)?;
            Some(SearchHit {
                title: a.text().collect::<String>().trim().to_string(),
                url,
                snippet: it
                    .select(&snip)
                    .next()
                    .map(|p| p.text().collect::<String>().trim().to_string())
                    .unwrap_or_default(),
            })
        })
        .take(count)
        .collect()
}

/// DDG 跳转链接 `//duckduckgo.com/l/?uddg=<编码URL>&…` → 真实 URL;直链原样放行。
fn decode_uddg(href: &str) -> Option<String> {
    if href.starts_with("http") && !href.contains("duckduckgo.com/l/") {
        return Some(href.to_string());
    }
    let start = href.find("uddg=")? + 5;
    let end = href[start..].find('&').map(|i| start + i).unwrap_or(href.len());
    let decoded = percent_decode(&href[start..end]);
    decoded.starts_with("http").then_some(decoded)
}

pub(crate) fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 整页抽取(单次解析):正文 + 页内链接。
fn extract_page(html: &str, base_url: &str) -> Page {
    let doc = Html::parse_document(html);
    let (title, text) = extract_text_from(&doc);
    let links = extract_links(&doc, base_url);
    Page { title, text, links }
}

/// 正文抽取(readability 简化版):正文形元素的文本聚合;太少则退化为全文压平。
/// 不追求完美,目标是"给模型可读的证据",失败兜底永远有东西。
fn extract_text_from(doc: &Html) -> (String, String) {
    let title = doc
        .select(&sel("title"))
        .next()
        .map(|t| t.text().collect::<String>().trim().to_string())
        .unwrap_or_default();

    let mut parts: Vec<String> = Vec::new();
    for el in doc.select(&sel("p, h1, h2, h3, li, blockquote, td, pre")) {
        let t: String = el.text().collect::<String>().split_whitespace().collect::<Vec<_>>().join(" ");
        if t.chars().count() >= 8 {
            parts.push(t);
        }
    }
    let mut text = parts.join("\n");
    if text.chars().count() < 120 {
        // SPA 空壳/非常规结构:压平整个 body 文本兜底
        if let Some(body) = doc.select(&sel("body")).next() {
            text = body.text().collect::<String>().split_whitespace().collect::<Vec<_>>().join(" ");
        }
    }
    (title, text)
}

/// 页内链接:`<a href>` 解析成绝对地址(相对地址按最终 URL 拼),按文档序取前
/// `LINKS_MAX` 条;js/mailto/纯锚点丢弃、同地址去重。无文字的锚(图片按钮)用链接
/// 目标文件名顶名字 —— 图标式"下载"按钮常是这种。
fn extract_links(doc: &Html, base_url: &str) -> Vec<PageLink> {
    let base = url::Url::parse(base_url).ok();
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for a in doc.select(&sel("a[href]")) {
        let Some(href) = a.value().attr("href").map(str::trim) else { continue };
        if href.is_empty()
            || href.starts_with('#')
            || href.starts_with("javascript:")
            || href.starts_with("mailto:")
        {
            continue;
        }
        let abs = match url::Url::parse(href) {
            Ok(u) => u,
            Err(_) => match base.as_ref().and_then(|b| b.join(href).ok()) {
                Some(u) => u,
                None => continue,
            },
        };
        if !matches!(abs.scheme(), "http" | "https") {
            continue;
        }
        let abs_s = abs.to_string();
        if !seen.insert(abs_s.clone()) {
            continue;
        }
        let mut text: String =
            a.text().collect::<String>().split_whitespace().collect::<Vec<_>>().join(" ");
        if text.is_empty() {
            text = abs
                .path_segments()
                .and_then(|mut s| s.next_back())
                .map(percent_decode)
                .unwrap_or_default();
        }
        out.push(PageLink { text: clip(&text, 60), url: abs_s });
        if out.len() >= LINKS_MAX {
            break;
        }
    }
    out
}

/// 按字符数截断(给模型的预算闸)。
pub fn clip(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let cut: String = s.chars().take(max_chars).collect();
    format!("{cut}…(已截断)")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bing_parsing_extracts_hits() {
        let html = r#"<html><body><ol>
          <li class="b_algo"><h2><a href="https://example.com/a">明天 天气 预报</a></h2>
            <div class="b_caption"><p>明天多云转晴,18-26 度。</p></div></li>
          <li class="b_algo"><h2><a href="javascript:void(0)">坏链接</a></h2></li>
          <li class="b_algo"><h2><a href="https://example.com/b">第二条</a></h2></li>
        </ol></body></html>"#;
        let hits = parse_bing(html, 5);
        assert_eq!(hits.len(), 2, "非 http 链接被过滤");
        assert_eq!(hits[0].url, "https://example.com/a");
        assert!(hits[0].title.contains("天气"));
        assert!(hits[0].snippet.contains("多云"));
    }

    #[test]
    fn ddg_parsing_decodes_uddg_redirect() {
        let html = r#"<div class="result">
            <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fnews%3Fid%3D7&rut=x">新闻标题</a>
            <a class="result__snippet">摘要文字</a>
          </div>"#;
        let hits = parse_ddg(html, 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].url, "https://example.com/news?id=7");
        assert_eq!(hits[0].title, "新闻标题");
    }

    #[test]
    fn extract_text_prefers_content_elements_and_falls_back() {
        let page = r#"<html><head><title>测试页</title><script>var x=1;</script></head>
          <body><nav>导航导航</nav>
          <p>这是第一段正文,讲了一件足够长的事情,超过八个字。</p>
          <h2>小标题在此</h2>
          <li>列表项也足够长才会被收进来哦</li>
          <p>短</p></body></html>"#;
        let Page { title, text, .. } = extract_page(page, "https://x.example.com/");
        assert_eq!(title, "测试页");
        assert!(text.contains("第一段正文"));
        assert!(text.contains("小标题在此"));
        assert!(!text.contains("var x"), "脚本不进正文");
        assert!(!text.contains("短\n"), "过短碎片被滤");

        // 没有正文形元素 → 压平兜底
        let bare = "<html><title>裸</title><body><div>只有 div 包着的一行字而已呀</div></body></html>";
        let fallback = extract_page(bare, "https://x.example.com/").text;
        assert!(fallback.contains("只有 div"));
    }

    #[test]
    fn extract_links_resolves_dedupes_and_names_blank_anchors() {
        let html = r##"<html><body>
          <a href="/dl/fp123.pdf">下载附件</a>
          <a href="https://other.com/x">外站</a>
          <a href="/dl/fp123.pdf">重复</a>
          <a href="#top">锚点</a>
          <a href="javascript:void(0)">JS</a>
          <a href="/img/fa%20piao.pdf"><img src="btn.png"></a>
        </body></html>"##;
        let page = extract_page(html, "https://inv.example.com/view?id=1");
        let urls: Vec<&str> = page.links.iter().map(|l| l.url.as_str()).collect();
        assert_eq!(
            urls,
            [
                "https://inv.example.com/dl/fp123.pdf",
                "https://other.com/x",
                "https://inv.example.com/img/fa%20piao.pdf"
            ],
            "相对转绝对、去重、js/锚点被滤"
        );
        assert_eq!(page.links[0].text, "下载附件");
        assert_eq!(page.links[2].text, "fa piao.pdf", "无文字锚用目标文件名(百分号解码)");
    }

    #[test]
    fn non_page_hint_flags_binary_only() {
        // PDF:按 Content-Type 或 %PDF 魔数认,话术指向 web_download + fs_read_text
        let pdf = non_page_hint("application/pdf", b"x").expect("CT 认出 PDF");
        assert!(pdf.contains("web_download") && pdf.contains("fs_read_text"));
        assert!(non_page_hint("", b"%PDF-1.4 junk").is_some(), "魔数兜住没报 CT 的");
        // 其他明确二进制 → 通用指路
        let zip = non_page_hint("application/zip", b"PK").expect("zip 拦下");
        assert!(zip.contains("web_download"));
        assert!(non_page_hint("application/octet-stream", &[0, 1]).is_some());
        assert!(non_page_hint("image/png", b"\x89PNG").is_some());
        // 页面类一律放行(text/*、html/xml/json、缺 CT)
        for ct in ["text/html", "text/plain", "application/xhtml+xml", "application/json", ""] {
            assert!(non_page_hint(ct, b"<html>hello</html>").is_none(), "{ct} 应放行");
        }
    }

    #[test]
    fn clip_and_percent_decode() {
        assert_eq!(clip("abc", 5), "abc");
        assert!(clip("一二三四五六", 3).starts_with("一二三"));
        assert_eq!(percent_decode("a%20b+c%E4%B8%AD"), "a b c中");
    }

    /// 缓存:同 URL 第二次不再打上游(本地假站点计数验证)。
    #[tokio::test]
    async fn fetch_text_caches_by_url() {
        use axum::{routing::get, Router};
        use std::sync::atomic::{AtomicUsize, Ordering};
        static HITS: AtomicUsize = AtomicUsize::new(0);

        async fn page() -> axum::response::Html<&'static str> {
            HITS.fetch_add(1, Ordering::Relaxed);
            axum::response::Html(
                "<html><title>缓存页</title><body><p>这一段正文足够长,用来测试缓存命中。</p></body></html>",
            )
        }
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, Router::new().route("/p", get(page))).await.ok();
        });

        let client = WebClient::new();
        let url = format!("http://127.0.0.1:{port}/p");
        let (t1, x1) = client.fetch_text(&url).await.unwrap();
        let (t2, x2) = client.fetch_text(&url).await.unwrap();
        assert_eq!((t1.as_str(), x1.as_str()), (t2.as_str(), x2.as_str()));
        assert_eq!(HITS.load(Ordering::Relaxed), 1, "第二次走缓存");
        assert_eq!(t1, "缓存页");
    }
}
