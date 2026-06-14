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
/// 像真浏览器的 UA(裸 reqwest 常被搜索页拒)。
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                  (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

pub struct WebClient {
    http: reqwest::Client,
    cache: Mutex<HashMap<String, (Instant, String)>>,
}

impl Default for WebClient {
    fn default() -> Self {
        Self::new()
    }
}

impl WebClient {
    pub fn new() -> WebClient {
        let http = reqwest::Client::builder()
            .user_agent(UA)
            .connect_timeout(Duration::from_secs(8))
            .timeout(Duration::from_secs(15))
            .build()
            .expect("reqwest client");
        WebClient { http, cache: Mutex::new(HashMap::new()) }
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
        let html = self
            .http
            .get("https://www.bing.com/search")
            .query(&[("q", query), ("setlang", "zh-hans"), ("count", "10")])
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        Ok(parse_bing(&html, count))
    }

    async fn search_ddg(&self, query: &str, count: usize) -> Result<Vec<SearchHit>> {
        let html = self
            .http
            .get("https://html.duckduckgo.com/html/")
            .query(&[("q", query)])
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        Ok(parse_ddg(&html, count))
    }

    /// 抓正文(带短缓存):返回 (标题, 正文)。cap 由调用方按用途裁。
    pub async fn fetch_text(&self, url: &str) -> Result<(String, String)> {
        if let Some(hit) = self.cache_get(url) {
            let (title, text) = split_cached(&hit);
            return Ok((title, text));
        }
        let resp = self.http.get(url).send().await.context("页面请求失败")?;
        let status = resp.status();
        anyhow::ensure!(status.is_success(), "页面 HTTP {status}");
        // 体积闸:10MB 封顶,防超大页面拖死
        let bytes = resp.bytes().await?;
        anyhow::ensure!(bytes.len() <= 10 * 1024 * 1024, "页面超过 10MB,放弃");
        let html = String::from_utf8_lossy(&bytes);
        let (title, text) = extract_text(&html);
        anyhow::ensure!(!text.trim().is_empty(), "页面没有可读正文(可能是纯脚本应用)");
        self.cache_put(url, &title, &text);
        Ok((title, text))
    }

    fn cache_get(&self, url: &str) -> Option<String> {
        let mut cache = self.cache.lock().expect("web cache lock poisoned");
        cache.retain(|_, (at, _)| at.elapsed() < CACHE_TTL);
        cache.get(url).map(|(_, v)| v.clone())
    }

    fn cache_put(&self, url: &str, title: &str, text: &str) {
        let mut cache = self.cache.lock().expect("web cache lock poisoned");
        cache.insert(url.to_string(), (Instant::now(), format!("{title}\u{0}{text}")));
    }
}

fn split_cached(cached: &str) -> (String, String) {
    match cached.split_once('\u{0}') {
        Some((t, x)) => (t.to_string(), x.to_string()),
        None => (String::new(), cached.to_string()),
    }
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

fn percent_decode(s: &str) -> String {
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

/// 正文抽取(readability 简化版):正文形元素的文本聚合;太少则退化为全文压平。
/// 不追求完美,目标是"给模型可读的证据",失败兜底永远有东西。
fn extract_text(html: &str) -> (String, String) {
    let doc = Html::parse_document(html);
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
        let (title, text) = extract_text(page);
        assert_eq!(title, "测试页");
        assert!(text.contains("第一段正文"));
        assert!(text.contains("小标题在此"));
        assert!(!text.contains("var x"), "脚本不进正文");
        assert!(!text.contains("短\n"), "过短碎片被滤");

        // 没有正文形元素 → 压平兜底
        let bare = "<html><title>裸</title><body><div>只有 div 包着的一行字而已呀</div></body></html>";
        let (_, fallback) = extract_text(bare);
        assert!(fallback.contains("只有 div"));
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
