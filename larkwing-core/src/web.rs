//! 联网问答的地基:搜索源解析 + 正文抽取 + 短 TTL 缓存。
//! robot web 插件的病根 = 搜索只回链接堆、模型还要串行 fetch(多一轮往返、摘要看引擎
//! 脸色)→ 这里**搜索即抓取**:工具一次调用带回正文证据片段。
//! 源 = Bing 中文优先 → 搜狗(国内直连稳)→ DDG(有代理时好用),按序尝试;选择器没法
//! 数据化(是代码不是数据),站点改版坏了改这里 —— 与 bilibili 搜索同一立场,诚实记档。
//!
//! **搜索请求必须长得像浏览器(2026-07-27 实测破案,别退回去)**:曾经只带 UA、无 cookie
//! 地裸 GET,被 Bing 判成机器人,三种坏法——① 人机验证页(解析出 0 条);② 国内版
//! `cc=cn&rdr=1` 市场重定向死循环;③ **最阴的一种:重定向链上把 query 弄丢,回一个
//! 「看起来完全正常、内容却是别的」的结果页**(真机实锤:搜「周深 悬崖之上 歌词 完整版」
//! 回来的是百度百科「周」字条目,还写着"约 175,000 个结果")。③ 会解析出一堆形似合法的
//! 命中原样喂给模型当证据(同期实锤:模型据此编造 .lrc 时间轴)。同机同 IP 同 UA 对照:
//! 裸 GET → 验证页;cookie 罐 + 完整浏览器头 + 跟随重定向 → 6 跳后 10 条全对(2/2 稳定)。
//! 故 `WebClient` 的 net::Client **必须**开 cookie_store + 带 `BROWSER_HEADERS`;
//! 光有 cookie 或光有头都不行(两者缺一都实测复现了坏法)。③ 另配 `looks_relevant` 闸。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use scraper::{Html, Selector};

/// 同 URL 正文短缓存:防同一回合/相邻回合重复抓(任务 HUD 不掺和,这层全静默)。
const CACHE_TTL: Duration = Duration::from_secs(600);
/// 像真浏览器的 UA(裸 reqwest 常被搜索页拒);web_download 与壳层 webrender 隐藏窗同款
/// (单源,§4.11——渲染窗与抓取端 UA 一致,免得同一站点见到两副面孔)。
/// 需要认证的下载目标(WebDAV / 带账号的直链)的一条凭证。
///
/// **为什么不做成工具参数**:密码走参数 = 进 LLM 上下文 + 落 `messages.payload` +
/// 之后每轮回放给供应商(§7.7「凭证不过桥」)。所以走 settings→keyring,工具运行时按
/// URL 的 host 现查,模型全程看不见密码。
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct HttpCred {
    /// 主机名(如 `dav.jianguoyun.com`、`192.168.1.10:5244`);大小写不敏感。
    pub host: String,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub password: String,
}

/// **手写 Debug 遮住密码**(照微信 `Target` 的先例):派生 Debug 的话,任何
/// `{:?}` / `tracing` 字段 / 被 `anyhow` 包进错误链的地方都会把明文密码吐进日志。
impl std::fmt::Debug for HttpCred {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpCred")
            .field("host", &self.host)
            .field("user", &self.user)
            .field("password", &"<已隐去>")
            .finish()
    }
}

/// 读全部凭证(keyring 优先,mac dev 回落 settings 明文,同 §6.3)。坏 JSON = 当没配,
/// 只 warn 不砸下载(§3.5 里「被动读取失败」允许 console-only)。
pub fn load_http_creds(settings: &crate::store::settings::SettingsRepo) -> Vec<HttpCred> {
    let raw = crate::secrets::get(settings, "net.http_creds").unwrap_or_default();
    if raw.trim().is_empty() {
        return Vec::new();
    }
    match serde_json::from_str::<Vec<HttpCred>>(&raw) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("下载认证配置读不出来(当作没配): {e}");
            Vec::new()
        }
    }
}

/// 给这个 URL 挑凭证:按 host 精确匹配(含端口),大小写不敏感。没有 = None(匿名下)。
pub fn cred_for(creds: &[HttpCred], url: &str) -> Option<HttpCred> {
    let host = reqwest::Url::parse(url).ok()?;
    let want = match host.port() {
        Some(p) => format!("{}:{p}", host.host_str()?),
        None => host.host_str()?.to_string(),
    };
    let want = want.to_ascii_lowercase();
    creds
        .iter()
        .find(|c| {
            let h = c.host.trim().to_ascii_lowercase();
            // 配 host 时带不带端口都认(用户填 `nas:5244` 或 `nas` 都行)
            h == want || want.split(':').next() == Some(h.as_str())
        })
        .cloned()
}

pub const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                  (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";

/// 真浏览器顶层导航会带的一整套头(UA 之外的另一半)。少了它们搜索源就翻脸——见模块
/// 顶部记档。`Sec-Fetch-Site: none` = "用户自己敲的地址",与我们每次都是独立请求相符。
const BROWSER_HEADERS: &[(&str, &str)] = &[
    ("accept", "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8"),
    ("accept-language", "zh-CN,zh;q=0.9,en;q=0.8"),
    ("upgrade-insecure-requests", "1"),
    ("sec-fetch-dest", "document"),
    ("sec-fetch-mode", "navigate"),
    ("sec-fetch-site", "none"),
    ("sec-fetch-user", "?1"),
];

fn browser_headers() -> reqwest::header::HeaderMap {
    let mut h = reqwest::header::HeaderMap::new();
    for (k, v) in BROWSER_HEADERS {
        // 静态常量,不合法就是代码写错了(与 sel() 同款立场)
        let name = reqwest::header::HeaderName::from_static(k);
        h.insert(name, reqwest::header::HeaderValue::from_static(v));
    }
    h
}

/// 搜索源。加一个源 = 加一支枚举 + 一个 `search_*` + 一个 `parse_*`,调度不动。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Bing,
    Sogou,
    Ddg,
}

impl Source {
    /// 只进日志与「都失败了」的死因串(给模型看的观察),不是用户可见文案。
    fn name(self) -> &'static str {
        match self {
            Source::Bing => "Bing",
            Source::Sogou => "搜狗",
            Source::Ddg => "DDG",
        }
    }
}

/// 尝试顺序按「实测可靠度 × 快」排(2026-07-27 同机实测):搜狗 0.96s 零跳、国内直连稳、
/// 给真实直链 → Bing 索引质量最好但**冷启要跑 6 跳 cookie 引导 ≈ 26s**(热 3.8s),当不了
/// 第一源(每进程第一次搜索都得先白烧一次超时)→ DDG 垫底(国内常不通,通的时候 ~1s)。
const SEARCH_SOURCES: &[Source] = &[Source::Sogou, Source::Bing, Source::Ddg];

/// 搜索单请求超时(比 `WebClient` 的 15s 通用档宽):要容得下 Bing 冷启那趟 ~26s 的
/// 重定向引导,否则它永远只会超时、白占一个源位。抓正文仍用通用档。
const SEARCH_TIMEOUT: Duration = Duration::from_secs(30);

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
        // cookie_store:进程内存态、不落盘;搜索源的 market 重定向要靠它才走得通(模块顶部)。
        // 重定向放宽到 20:Bing 冷启那趟引导实测 6 跳起、偶尔一路追加 `mkt=` 冲破默认 10 跳
        // (报 too many redirects)。抓正文也共用这条,多几跳无害。
        let net = crate::net::Client::new(|b| {
            b.user_agent(UA)
                .cookie_store(true)
                .redirect(reqwest::redirect::Policy::limited(20))
                .default_headers(browser_headers())
                .connect_timeout(Duration::from_secs(8))
                .timeout(Duration::from_secs(15))
        });
        WebClient { net, cache: Mutex::new(HashMap::new()) }
    }

    /// 搜索:按序试各源,第一个「有结果 + 过合理性闸」的胜出;全军覆没才报错(带各源死因)。
    pub async fn search(&self, query: &str, count: usize) -> Result<Vec<SearchHit>> {
        let mut why: Vec<String> = Vec::new();
        for &src in SEARCH_SOURCES {
            let name = src.name();
            match self.search_with(src, query, count).await {
                Ok(hits) if hits.is_empty() => {
                    tracing::warn!(source = name, "搜索没有结果,换下一个源");
                    why.push(format!("{name}: 没有结果"));
                }
                // 结构完整但内容跑题 = 被反爬降级/query 被弄丢的假结果页(模块顶部 ③)
                Ok(hits) if !looks_relevant(query, &hits) => {
                    tracing::warn!(source = name, "结果与查询无关(疑似被降级),换下一个源");
                    why.push(format!("{name}: 结果与查询无关(疑似被拦截)"));
                }
                Ok(hits) => return Ok(hits),
                Err(e) => {
                    tracing::warn!(source = name, "搜索失败,换下一个源: {e:#}");
                    why.push(format!("{name}: {e}"));
                }
            }
        }
        bail!("搜索源都没能给出可用结果({})", why.join(";"))
    }

    async fn search_with(&self, src: Source, query: &str, count: usize) -> Result<Vec<SearchHit>> {
        match src {
            Source::Bing => self.search_bing(query, count).await,
            Source::Sogou => self.search_sogou(query, count).await,
            Source::Ddg => self.search_ddg(query, count).await,
        }
    }

    async fn search_bing(&self, query: &str, count: usize) -> Result<Vec<SearchHit>> {
        let url = "https://www.bing.com/search";
        // 只带真浏览器会带的参数。**别再加 `count`**:实测它会把首次请求打进重定向死循环
        // /人机验证页(同一 cookie 罐 + 同一套头,带 count 0 跳进验证页、去掉 6 跳出 10 条),
        // 而它本来也没用——条数是解析时 `.take(count)` 裁的。setlang 无辜,留着表意。
        let html = self
            .net
            .send(url, |c| {
                c.get(url).query(&[("q", query), ("setlang", "zh-hans")]).timeout(SEARCH_TIMEOUT)
            })
            .await?
            .error_for_status()?
            .text()
            .await?;
        Ok(parse_bing(&html, count))
    }

    async fn search_sogou(&self, query: &str, count: usize) -> Result<Vec<SearchHit>> {
        let url = "https://www.sogou.com/web";
        let html = self
            .net
            .send(url, |c| c.get(url).query(&[("query", query)]).timeout(SEARCH_TIMEOUT))
            .await?
            .error_for_status()?
            .text()
            .await?;
        Ok(parse_sogou(&html, count))
    }

    async fn search_ddg(&self, query: &str, count: usize) -> Result<Vec<SearchHit>> {
        let url = "https://html.duckduckgo.com/html/";
        let html = self
            .net
            .send(url, |c| c.get(url).query(&[("q", query)]).timeout(SEARCH_TIMEOUT))
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

/// 搜狗结果页:div.vrwrap → h3(标题)+ .fz-mid(摘要)。取地址按「与标题同源」排序:
/// ① 标题自己的直链最准(音乐/视频垂类卡片一个块里并列好几首,块级 `data-url` 未必是
/// 第一条那首——实锤过标题 QQ音乐、地址却是酷狗);② 标题是 `/link?url=` 跳转时,块内
/// `data-url` 才是真实目标;③ 都没有就把跳转链补成绝对地址兜底(它 302 到真页,抓取
/// 跟随重定向照样读得到,只是给模型看的地址不好看)。
fn parse_sogou(html: &str, count: usize) -> Vec<SearchHit> {
    let doc = Html::parse_document(html);
    let (item, title, anchor, real, snip) =
        (sel("div.vrwrap"), sel("h3"), sel("a"), sel("[data-url]"), sel("div.fz-mid"));
    doc.select(&item)
        .filter_map(|it| {
            let h = it.select(&title).next()?;
            let title_text = squeeze(&h.text().collect::<String>());
            if title_text.is_empty() {
                return None;
            }
            let block_url = || {
                it.select(&real)
                    .filter_map(|e| e.value().attr("data-url"))
                    .find(|u| u.starts_with("http"))
                    .map(str::to_string)
            };
            let url = match h.select(&anchor).next().and_then(|a| a.value().attr("href")) {
                Some(href) if href.starts_with("http") => Some(href.to_string()),
                Some(href) => block_url().or_else(|| sogou_url(href)),
                None => block_url(),
            }?;
            Some(SearchHit {
                title: title_text,
                url,
                snippet: it
                    .select(&snip)
                    .next()
                    .map(|p| squeeze(&p.text().collect::<String>()))
                    .unwrap_or_default(),
            })
        })
        .take(count)
        .collect()
}

/// 搜狗标题链接:绝对地址原样放行;`/link?url=…` 补成绝对;其余(javascript: 等)丢弃。
fn sogou_url(href: &str) -> Option<String> {
    if href.starts_with("http") {
        return Some(href.to_string());
    }
    href.starts_with("/link?url=").then(|| format!("https://www.sogou.com{href}"))
}

/// 压掉换行/连续空白(结果页标题常带缩进换行)。
fn squeeze(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 结果合理性闸:搜索源被反爬降级时会回一个「结构完整、内容却是别的」的结果页(模块顶部
/// ③),解析器分不出真假。这里拿查询词做一道弱校验:**刻意宽松**——只要有一条命中沾上
/// 查询里的任意一个片段就放行。误杀的代价只是多退一个源,误放的代价是把垃圾当证据喂给
/// 模型(实锤过模型据此编造内容),所以宁可放过略偏的结果。查询抽不出信号片段则不设闸。
fn looks_relevant(query: &str, hits: &[SearchHit]) -> bool {
    let signals = query_signals(query);
    if signals.is_empty() {
        return true;
    }
    hits.iter().any(|h| {
        let hay = format!("{} {}", h.title, h.snippet).to_lowercase();
        signals.iter().any(|s| hay.contains(s))
    })
}

/// 查询的「信号片段」= ASCII 单词(≥2 字符,小写)+ 相邻两个汉字。中文没有词边界,双字
/// 窗口是最省事又够用的粒度(「今天天气怎么样」能靠「天气」对上)。**单个汉字太弱、
/// 刻意不收**——否则搜「周深 悬崖之上…」时百度百科的「周」字条目会被判成相关,而那
/// 正是本闸要拦的东西。
fn query_signals(query: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut run: Vec<char> = Vec::new();
    // 末尾补一个空格,让最后一段也走到 flush 分支
    for c in query.chars().chain(std::iter::once(' ')) {
        if is_cjk(c) {
            flush_word(&mut out, &mut word);
            run.push(c);
            continue;
        }
        for w in run.windows(2) {
            out.push(w.iter().collect());
        }
        run.clear();
        if c.is_ascii_alphanumeric() {
            word.push(c.to_ascii_lowercase());
        } else {
            flush_word(&mut out, &mut word);
        }
    }
    out.sort();
    out.dedup();
    out
}

fn flush_word(out: &mut Vec<String>, word: &mut String) {
    if word.chars().count() >= 2 {
        out.push(std::mem::take(word));
    } else {
        word.clear();
    }
}

/// 基本区够用(中文查询绝大多数落这);扩展区字落到非 CJK 分支只是少几个双字片段,
/// 与本闸「宁可放过」的取向一致。
fn is_cjk(c: char) -> bool {
    ('\u{4E00}'..='\u{9FFF}').contains(&c)
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
    fn sogou_parsing_prefers_real_url_over_redirect() {
        let html = r#"<html><body>
          <div class="vrwrap">
            <h3 class="vr-title"><a href="/link?url=DSOYnZeCC_roiy4">星海漫游 歌词_LRC下载_歌词网</a></h3>
            <div class="fz-mid space-txt clamp2">星海漫游,某歌手,星海漫游歌词</div>
            <div class="r-sech" data-url="http://www.example-lrc.com/geci_1.html"><span>推荐您搜索</span></div>
          </div>
          <div class="vrwrap">
            <h3 class="vr-title _music_title"><a href="https://music-a.example.com/song/9">星海漫游_歌曲在线播放_甲音乐</a></h3>
            <h3 class="vr-title _music_title"><a href="https://music-b.example.com/song/9">星海漫游_歌曲在线播放_乙音乐</a></h3>
            <div class="r-sech" data-url="https://music-b.example.com/song/9"></div>
          </div>
          <div class="vrwrap"><h3 class="vr-title"><a href="/link?url=ABC">没有 data-url 的一条</a></h3></div>
          <div class="vrwrap"><h3 class="vr-title"><a href="javascript:void(0)">坏链接</a></h3></div>
          <div class="vrwrap"><div class="fz-mid">没有标题的块</div></div>
        </body></html>"#;
        let hits = parse_sogou(html, 10);
        assert_eq!(hits.len(), 3, "坏链接与无标题块被滤掉: {hits:?}");
        assert_eq!(hits[0].url, "http://www.example-lrc.com/geci_1.html", "data-url 压过 /link 跳转");
        assert!(hits[0].title.contains("星海漫游"));
        assert!(hits[0].snippet.contains("某歌手"));
        // 垂类卡片一块多条:地址跟着「第一个标题自己的链接」走,不能被块级 data-url 带偏
        assert!(hits[1].title.contains("甲音乐"));
        assert_eq!(hits[1].url, "https://music-a.example.com/song/9", "标题与地址必须同源");
        assert_eq!(hits[2].url, "https://www.sogou.com/link?url=ABC", "没 data-url 就补成绝对地址");
    }

    /// 闸的立身之本:真机那次「搜歌词回来百度百科『周』字条目」必须被判为不相关。
    /// 夹具就是真机原样抓回来的那几条(标题 + 摘要开头)。
    #[test]
    fn relevance_gate_rejects_degraded_serp() {
        let hit = |t: &str, s: &str| SearchHit {
            title: t.into(),
            url: "https://example.com/x".into(),
            snippet: s.into(),
        };
        let query = "周深 悬崖之上 歌词 完整版";

        let degraded = vec![
            hit("周（汉语汉字）_百度百科", "周（读音zhōu）是汉字通用规范一级字（常用字）。此字始见于商代甲骨文。"),
            hit("周朝（中国历史朝代）_百度百科", "周族是居于今陕甘黄土高原、渭水流域一带的古老部族。"),
            hit("周的意思,周的解释,周的拼音,周的部首,周的笔顺-汉语国学", "〔周〕字拼音是（zhōu），部首是 口部，总笔画是 8画。"),
        ];
        assert!(!looks_relevant(query, &degraded), "首字退化的假结果页必须被拦下");

        // 另一种真机形态:query 在跳转链上被整个丢掉,回了个毫不相干的结果页
        let lost = vec![hit("New Microsoft Teams bulk installer is now available", "Deploy the new Teams client…")];
        assert!(!looks_relevant(query, &lost), "query 被丢掉的结果页必须被拦下");

        let good = vec![
            hit("周深悬崖之上歌词全文 - 抖音", "悬崖之上 演唱：周深"),
            hit("无关的一条", "随便什么"),
        ];
        assert!(looks_relevant(query, &good), "有一条沾上就放行(闸刻意宽松)");

        // 英文查询走 ASCII 单词那条路
        assert!(looks_relevant("rust borrow checker error", &[hit("Understanding the Rust Borrow Checker", "")]));
        assert!(!looks_relevant("rust borrow checker error", &[hit("今日天气预报", "多云转晴")]));
    }

    #[test]
    fn query_signals_drops_single_cjk_char() {
        // 单字不进信号集——否则「周」字条目会被判成「周深…」的相关结果,正是要拦的
        assert!(query_signals("周").is_empty());
        assert!(query_signals("a").is_empty(), "单字母同理");
        assert_eq!(query_signals("歌词"), vec!["歌词".to_string()]);
        // 无词边界的长串:滑动双字窗口,「天气」能对上
        assert!(query_signals("今天天气怎么样").contains(&"天气".to_string()));
        // 纯符号 → 抽不出信号 → 上层不设闸
        assert!(query_signals("?!  ").is_empty());
        assert!(looks_relevant("?!", &[]), "抽不出信号时不设闸");
    }

    /// 真网回归(开发机手动跑):
    /// `cargo test -p larkwing-core --lib web::tests::real_search -- --ignored --nocapture`
    /// 钉的是本次破案的两件事——搜索源没再把我们当机器人、拿回来的结果确实跟查询有关。
    #[tokio::test]
    #[ignore = "打真网(搜索引擎),开发机手动跑"]
    async fn real_search_returns_relevant_hits() {
        let web = WebClient::new();
        let query = "周深 悬崖之上 歌词";
        let hits = web.search(query, 5).await.expect("搜索应当成功");
        for h in &hits {
            println!("  {} | {}", h.title, h.url);
        }
        assert!(!hits.is_empty(), "应当有结果");
        assert!(looks_relevant(query, &hits), "结果必须与查询相关(不是被降级的假结果页)");
    }

    /// 真网体检(开发机手动跑):逐个源单独报告。「搜索又不对劲了」时先跑它,一眼看出
    /// 是哪家翻脸、翻的是哪种脸(报错 / 0 条 / 有结果但没过闸 = 被降级的假结果页)。
    /// `cargo test -p larkwing-core --lib web::tests::real_source_report -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "打真网(搜索引擎),开发机手动跑"]
    async fn real_source_report() {
        let web = WebClient::new();
        let query = "周深 悬崖之上 歌词";
        for &src in SEARCH_SOURCES {
            match web.search_with(src, query, 5).await {
                Ok(hits) => println!(
                    "{:>6}: {} 条 过闸={} 首条={}",
                    src.name(),
                    hits.len(),
                    looks_relevant(query, &hits),
                    hits.first().map(|h| h.title.as_str()).unwrap_or("-")
                ),
                Err(e) => println!("{:>6}: 失败 {e:#}", src.name()),
            }
        }
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
