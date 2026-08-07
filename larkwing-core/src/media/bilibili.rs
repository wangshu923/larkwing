//! B 站源:公开搜索 API(robot bilibili/api.py 移植)。搜索不走 yt-dlp ——
//! 直调 API 快,且返回结构化的标题/UP主/时长,正好喂播放卡片;流解析才归 yt-dlp。
//! 已知风险(robot 注释原样继承):B 站可能收紧 WBI 签名,届时此处拿到 -412/-403,
//! 错误按 RiskControl 上抛(带登录态时概率显著降低),签名实现参考 yt-dlp。

use anyhow::{anyhow, Result};
use async_trait::async_trait;

use super::{EpisodeRef, MediaHit, MediaSource, SearchError};

const SEARCH_URL: &str = "https://api.bilibili.com/x/web-interface/search/type";
/// 视频详情(分P `pages` + 合集 `ugc_season`):非 WBI 端点,UA+Referer 即可,多集发现走它。
const VIEW_URL: &str = "https://api.bilibili.com/x/web-interface/view";
/// 番剧(PGC)整季详情:`ep_id=` / `season_id=` 二选一,一次回整季 `episodes`。番剧与 UGC 稿件
/// 是**两套内容体系**——番剧集的 bvid 拿去问 VIEW_URL 是 -404,多集发现只能走这个端点。
/// 免 WBI 签名(与 VIEW_URL 同)。
const PGC_SEASON_URL: &str = "https://api.bilibili.com/pgc/view/web/season";
/// 裸 UA 常被 412,挂一个像真浏览器的(robot 同款手法,版本号更新)。
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                  (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";

pub struct Bilibili {
    net: crate::net::Client,
}

impl Bilibili {
    pub fn new() -> Bilibili {
        let net = crate::net::Client::new(|b| {
            b.connect_timeout(std::time::Duration::from_secs(10))
                .timeout(std::time::Duration::from_secs(15))
        });
        Bilibili { net }
    }

    /// 番剧整季发现。与 UGC 那条同款「尽力件」纪律:任何一步不顺一律 `Ok(None)` 退化成单集。
    async fn pgc_episodes(
        &self,
        pgc: &PgcRef,
        cookie_header: Option<&str>,
    ) -> Result<Option<(String, Vec<EpisodeRef>)>> {
        let (param, value) = pgc.query();
        let resp = self
            .net
            .send(PGC_SEASON_URL, |c| {
                let req = c
                    .get(PGC_SEASON_URL)
                    .query(&[(param, value)])
                    .header("User-Agent", UA)
                    .header("Referer", "https://www.bilibili.com/");
                match cookie_header {
                    Some(cookie) => req.header("Cookie", cookie),
                    None => req,
                }
            })
            .await
            .map_err(|e| anyhow!("番剧 season 请求失败: {e}"))?;
        if resp.status().as_u16() != 200 {
            return Ok(None); // 含 412/403 风控:静默退化单集(播放路径会引导扫码登录)
        }
        let payload: serde_json::Value =
            resp.json().await.map_err(|e| anyhow!("番剧 season 响应不是 JSON: {e}"))?;
        if payload["code"].as_i64().unwrap_or(-1) != 0 {
            return Ok(None);
        }
        // 番剧的载荷在 `result`(UGC 在 `data`),别抄错。
        Ok(parse_season(&payload["result"]))
    }
}

#[async_trait]
impl MediaSource for Bilibili {
    fn id(&self) -> &'static str {
        "bilibili"
    }

    fn login_url(&self) -> &'static str {
        "https://passport.bilibili.com/login"
    }

    fn cookie_url(&self) -> &'static str {
        "https://www.bilibili.com"
    }

    fn login_cookie(&self) -> &'static str {
        "SESSDATA"
    }

    async fn search(
        &self,
        keyword: &str,
        limit: usize,
        cookie_header: Option<&str>,
    ) -> Result<Vec<MediaHit>, SearchError> {
        let keyword = keyword.trim();
        if keyword.is_empty() {
            return Ok(Vec::new());
        }
        let resp = self
            .net
            .send(SEARCH_URL, |c| {
                let req = c
                    .get(SEARCH_URL)
                    .query(&[
                        ("search_type", "video"),
                        ("keyword", keyword),
                        ("page", "1"),
                        ("order", "totalrank"),
                    ])
                    .header("User-Agent", UA)
                    .header("Referer", "https://www.bilibili.com/");
                match cookie_header {
                    Some(cookie) => req.header("Cookie", cookie),
                    None => req,
                }
            })
            .await
            .map_err(|e| SearchError::Other(anyhow!("搜索请求失败: {e}")))?;
        let status = resp.status().as_u16();
        if status == 412 || status == 403 {
            return Err(SearchError::RiskControl);
        }
        if status != 200 {
            return Err(SearchError::Other(anyhow!("搜索 HTTP {status}")));
        }
        let payload: serde_json::Value =
            resp.json().await.map_err(|e| SearchError::Other(anyhow!("搜索响应不是 JSON: {e}")))?;
        let code = payload["code"].as_i64().unwrap_or(-1);
        if code == -412 || code == -403 || code == -101 {
            return Err(SearchError::RiskControl);
        }
        if code != 0 {
            let msg = payload["message"].as_str().unwrap_or("?");
            return Err(SearchError::Other(anyhow!("搜索 code={code} message={msg}")));
        }
        Ok(parse_results(&payload, limit))
    }

    /// 多集发现,**两条端点各管一套内容体系**:番剧(ep/ss)走 PGC season;UGC 稿件(BV)走 view
    /// API 的 `pages`(分P)与 `ugc_season`(合集)。**尽力件**——拿不到(短链 / 风控 / 非视频)
    /// 一律 `Ok(None)` 退化成单集,绝不挡播放(风控后续由 resolve 的 AuthRequired 引导登录,
    /// 登录重放时带 cookie 再发现一次)。
    async fn episodes(
        &self,
        page_url: &str,
        cookie_header: Option<&str>,
    ) -> Result<Option<(String, Vec<EpisodeRef>)>> {
        // 番剧优先判:它的 URL 里没有 BV 号,落到下面的 extract_bvid 只会一路 None。
        if let Some(pgc) = extract_pgc(page_url) {
            return self.pgc_episodes(&pgc, cookie_header).await;
        }
        let Some(bvid) = extract_bvid(page_url) else {
            return Ok(None); // b23.tv 短链 / av 号 → 不在分P/合集发现范围
        };
        let resp = self
            .net
            .send(VIEW_URL, |c| {
                let req = c
                    .get(VIEW_URL)
                    .query(&[("bvid", bvid.as_str())])
                    .header("User-Agent", UA)
                    .header("Referer", "https://www.bilibili.com/");
                match cookie_header {
                    Some(cookie) => req.header("Cookie", cookie),
                    None => req,
                }
            })
            .await
            .map_err(|e| anyhow!("view 请求失败: {e}"))?;
        if resp.status().as_u16() != 200 {
            return Ok(None); // 含 412/403 风控:静默退化单集(resolve 路径会处理登录)
        }
        let payload: serde_json::Value =
            resp.json().await.map_err(|e| anyhow!("view 响应不是 JSON: {e}"))?;
        if payload["code"].as_i64().unwrap_or(-1) != 0 {
            return Ok(None);
        }
        Ok(parse_view(&payload["data"], &bvid))
    }
}

/// 从页面 URL 抽 BV 号(`BV` 后接的字母数字)。没有 → None(短链 / av / 番剧 ep)。
fn extract_bvid(url: &str) -> Option<String> {
    let i = url.find("BV")?;
    let rest = &url[i..];
    let end = rest[2..]
        .find(|c: char| !c.is_ascii_alphanumeric())
        .map(|e| e + 2)
        .unwrap_or(rest.len());
    let bvid = &rest[..end];
    (bvid.len() >= 5).then(|| bvid.to_string())
}

/// 番剧(PGC)链接的两种形态。UGC 稿件(BV 号)不在此列 —— 两套内容体系、两套端点。
#[derive(Debug, PartialEq)]
enum PgcRef {
    /// `/bangumi/play/ep742483` → 单集
    Ep(String),
    /// `/bangumi/play/ss44871` → 整季
    Season(String),
}

impl PgcRef {
    /// 两种形态查同一个 season 端点,只是参数名不同。
    fn query(&self) -> (&'static str, &str) {
        match self {
            PgcRef::Ep(id) => ("ep_id", id),
            PgcRef::Season(id) => ("season_id", id),
        }
    }
}

/// 从页面 URL 抽番剧的 ep / ss 号。**必须落在 `/bangumi/play/` 路径下**——否则别处
/// 碰巧出现的 "ep"/"ss" 字样会被误认(`extract_bvid` 按 `BV` 大写字样找不会撞,这里两个
/// 小写字母太常见,得靠路径约束)。
fn extract_pgc(url: &str) -> Option<PgcRef> {
    let rest = url.split("/bangumi/play/").nth(1)?;
    // 号后面常跟 ?spm_id_from=… 之类跟踪参数,取到第一个非数字为止。
    let digits = |s: &str| -> Option<String> {
        let n: String = s.chars().take_while(char::is_ascii_digit).collect();
        (!n.is_empty()).then_some(n)
    };
    if let Some(s) = rest.strip_prefix("ep") {
        return digits(s).map(PgcRef::Ep);
    }
    if let Some(s) = rest.strip_prefix("ss") {
        return digits(s).map(PgcRef::Season);
    }
    None
}

/// 解析 `pgc/view/web/season` 的 `result`(注意番剧走 `result`、UGC 走 `data`)。纯函数、可测。
/// 只收 `episodes`(正片);`section`(PV / 特别篇 / 预告)刻意不并进队列——「自动播下一集」
/// 要的是正片顺序,混进花絮会把连播打乱。<2 集 → None(不成系列,同 `parse_view`)。
fn parse_season(result: &serde_json::Value) -> Option<(String, Vec<EpisodeRef>)> {
    let mut eps = Vec::new();
    for (i, ep) in result["episodes"].as_array()?.iter().enumerate() {
        // ep 号既是集身份也是可播地址的唯一来源,缺了就拼不出地址 → 跳过(同 ugc_season 滤 bvid)。
        let Some(id) = ep["ep_id"].as_i64().or_else(|| ep["id"].as_i64()) else { continue };
        eps.push(EpisodeRef {
            id: format!("ep{id}"),
            // **必须是 bangumi/play/ep 形**:番剧集自带的 bvid 在 UGC view 端点是 -404、
            // yt-dlp 也放不了,存 bvid 等于存了个放不出来的地址。
            url: format!("https://www.bilibili.com/bangumi/play/ep{id}"),
            title: episode_title(ep, i),
        });
    }
    if eps.len() < 2 {
        return None;
    }
    let key = result["season_id"]
        .as_i64()
        .map(|i| format!("bili:pgc:{i}"))
        // 缺 season_id 也别丢掉队列:拿首集 ep 号当季身份(首集不会变)。
        .unwrap_or_else(|| format!("bili:pgc:{}", eps[0].id));
    Some((key, eps))
}

/// 集名:番剧的 `title` 是集号("1" / "OVA" / "特别篇"),`long_title` 才是副标题。
/// 数字集号补成「第N集」,非数字原样留(别把 "OVA" 硬套成「第OVA集」);都空 → 按序号兜底。
fn episode_title(ep: &serde_json::Value, i: usize) -> String {
    let num = ep["title"].as_str().unwrap_or("").trim();
    let sub = ep["long_title"].as_str().unwrap_or("").trim();
    let head = if num.is_empty() {
        format!("第{}集", i + 1)
    } else if num.chars().all(|c| c.is_ascii_digit()) {
        format!("第{num}集")
    } else {
        num.to_string()
    };
    if sub.is_empty() {
        head
    } else {
        format!("{head} {sub}")
    }
}

/// 解析 view API 的 `data`:**合集优先**(ugc_season,整季多个 BV),其次**分P**(单 BV 多 P)。
/// 单集(无合集 + ≤1 P)→ None。纯函数、可测。集身份 `id`:合集用 bvid、分P 用 `pN`;
/// 分P 的 P1 用**裸 bvid url**(对齐 build_queue 的 url 匹配),P2+ 带 `?p=N`。
fn parse_view(data: &serde_json::Value, bvid: &str) -> Option<(String, Vec<EpisodeRef>)> {
    // 合集(ugc_season):跨 sections 拍平 episodes,每集一个独立 BV。
    if let Some(season) = data.get("ugc_season").filter(|v| v.is_object()) {
        let mut eps = Vec::new();
        if let Some(sections) = season["sections"].as_array() {
            for sec in sections {
                let Some(arr) = sec["episodes"].as_array() else { continue };
                for ep in arr {
                    let Some(bv) = ep["bvid"].as_str().filter(|s| s.starts_with("BV")) else {
                        continue;
                    };
                    let title = ep["title"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("第{}集", eps.len() + 1));
                    eps.push(EpisodeRef {
                        id: bv.to_string(),
                        url: format!("https://www.bilibili.com/video/{bv}"),
                        title,
                    });
                }
            }
        }
        if eps.len() >= 2 {
            let key = season["id"]
                .as_i64()
                .map(|i| format!("bili:season:{i}"))
                .unwrap_or_else(|| format!("bili:bv:{bvid}"));
            return Some((key, eps));
        }
    }
    // 分P(单 BV 多 P)。
    if let Some(pages) = data["pages"].as_array().filter(|p| p.len() >= 2) {
        let eps = pages
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let page = p["page"].as_i64().unwrap_or((i + 1) as i64);
                let title = p["part"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("P{page}"));
                let url = if page <= 1 {
                    format!("https://www.bilibili.com/video/{bvid}")
                } else {
                    format!("https://www.bilibili.com/video/{bvid}?p={page}")
                };
                EpisodeRef { id: format!("p{page}"), url, title }
            })
            .collect();
        return Some((format!("bili:bv:{bvid}"), eps));
    }
    None
}

fn parse_results(payload: &serde_json::Value, limit: usize) -> Vec<MediaHit> {
    let empty = Vec::new();
    let items = payload["data"]["result"].as_array().unwrap_or(&empty);
    items
        .iter()
        .filter_map(|item| {
            let bvid = item["bvid"].as_str()?;
            if !bvid.starts_with("BV") {
                return None;
            }
            Some(MediaHit {
                url: format!("https://www.bilibili.com/video/{bvid}"),
                title: clean_title(item["title"].as_str().unwrap_or("")),
                author: item["author"].as_str().unwrap_or("").to_string(),
                duration_seconds: parse_duration(item["duration"].as_str().unwrap_or("")),
                source: "bilibili".into(),
            })
        })
        .take(limit)
        .collect()
}

/// 去掉搜索结果标题里的高亮标签(<em class="keyword">…</em>)+ HTML 实体解码。
fn clean_title(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find('<') {
        let (head, tail) = rest.split_at(start);
        out.push_str(head);
        match tail.find('>') {
            // 只剥 em / /em 标签,别的尖括号当正文保留(标题里真可能有 <3 这种)
            Some(end) if tail[1..end].trim_start_matches('/').starts_with("em") => {
                rest = &tail[end + 1..];
            }
            _ => {
                out.push('<');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    unescape(&out).trim().to_string()
}

fn unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

/// B 站 duration 形如 "3:45" / "1:23:45";解析不出 = 0。
fn parse_duration(s: &str) -> i64 {
    let mut total = 0i64;
    for part in s.trim().split(':') {
        match part.parse::<i64>() {
            Ok(n) => total = total * 60 + n,
            Err(_) => return 0,
        }
    }
    if s.trim().is_empty() {
        0
    } else {
        total
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_search_payload_and_cleans_titles() {
        let payload = serde_json::json!({
            "code": 0,
            "data": { "result": [
                {
                    "bvid": "BV1xx411c7mD",
                    "title": "<em class=\"keyword\">恭喜发财</em> 刘德华 &amp; 高清",
                    "author": "某音乐区UP",
                    "duration": "3:45"
                },
                { "bvid": "av123", "title": "不是BV的过滤掉", "author": "x", "duration": "1:00" },
                {
                    "bvid": "BV1yy411c7mE",
                    "title": "时长带小时 &lt;3",
                    "author": "y",
                    "duration": "1:02:03"
                }
            ]}
        });
        let hits = parse_results(&payload, 10);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].url, "https://www.bilibili.com/video/BV1xx411c7mD");
        assert_eq!(hits[0].title, "恭喜发财 刘德华 & 高清");
        assert_eq!(hits[0].duration_seconds, 225);
        assert_eq!(hits[1].title, "时长带小时 <3");
        assert_eq!(hits[1].duration_seconds, 3723);
    }

    #[test]
    fn limit_caps_results() {
        let payload = serde_json::json!({
            "code": 0,
            "data": { "result": [
                { "bvid": "BV1", "title": "a", "author": "", "duration": "0:10" },
                { "bvid": "BV2", "title": "b", "author": "", "duration": "0:10" }
            ]}
        });
        assert_eq!(parse_results(&payload, 1).len(), 1);
    }

    #[test]
    fn duration_edge_cases() {
        assert_eq!(parse_duration(""), 0);
        assert_eq!(parse_duration("abc"), 0);
        assert_eq!(parse_duration("45"), 45);
    }

    #[test]
    fn extract_bvid_from_urls() {
        assert_eq!(
            extract_bvid("https://www.bilibili.com/video/BV1xx411c7mD").as_deref(),
            Some("BV1xx411c7mD")
        );
        // 带 ?p / 其它 query 也能抽出
        assert_eq!(
            extract_bvid("https://www.bilibili.com/video/BV1xx411c7mD?p=3&t=10").as_deref(),
            Some("BV1xx411c7mD")
        );
        // 短链 / av 号 / 番剧 ep → 无 BV
        assert_eq!(extract_bvid("https://b23.tv/abcdef"), None);
        assert_eq!(extract_bvid("https://www.bilibili.com/bangumi/play/ep123"), None);
    }

    #[test]
    fn parse_view_prefers_ugc_season() {
        let data = serde_json::json!({
            "pages": [ {"page":1,"part":"正片"} ], // 只有 1 P,但属于合集 → 合集赢
            "ugc_season": {
                "id": 778899,
                "sections": [
                    {"episodes": [
                        {"bvid":"BV1aa","title":"第一集 出发"},
                        {"bvid":"BV1bb","title":"第二集 抵达"},
                        {"bvid":"BV1cc","title":""} // 空标题 → 兜底"第3集"
                    ]}
                ]
            }
        });
        let (key, eps) = parse_view(&data, "BV1aa").unwrap();
        assert_eq!(key, "bili:season:778899");
        assert_eq!(eps.len(), 3);
        assert_eq!(eps[0].id, "BV1aa");
        assert_eq!(eps[0].url, "https://www.bilibili.com/video/BV1aa");
        assert_eq!(eps[1].title, "第二集 抵达");
        assert_eq!(eps[2].title, "第3集", "空标题兜底");
    }

    #[test]
    fn parse_view_multipart_when_no_season() {
        let data = serde_json::json!({
            "pages": [
                {"cid":1,"page":1,"part":"第1集"},
                {"cid":2,"page":2,"part":"第2集"},
                {"cid":3,"page":3,"part":""} // 空 → "P3"
            ]
        });
        let (key, eps) = parse_view(&data, "BV1zz").unwrap();
        assert_eq!(key, "bili:bv:BV1zz");
        assert_eq!(eps.len(), 3);
        // P1 用裸 url(对齐 build_queue 的 url 匹配),P2+ 带 ?p=
        assert_eq!(eps[0].url, "https://www.bilibili.com/video/BV1zz");
        assert_eq!(eps[0].id, "p1");
        assert_eq!(eps[1].url, "https://www.bilibili.com/video/BV1zz?p=2");
        assert_eq!(eps[2].title, "P3", "空 part 兜底");
    }

    #[test]
    fn parse_view_single_video_is_none() {
        // 单 P、无合集 → 不成系列
        let data = serde_json::json!({ "pages": [ {"page":1,"part":"正片"} ] });
        assert!(parse_view(&data, "BV1solo").is_none());
        // 啥都没有也 None
        assert!(parse_view(&serde_json::json!({}), "BV1x").is_none());
    }

    #[test]
    fn extract_pgc_from_bangumi_urls() {
        assert_eq!(
            extract_pgc("https://www.bilibili.com/bangumi/play/ep742483"),
            Some(PgcRef::Ep("742483".into()))
        );
        assert_eq!(
            extract_pgc("https://www.bilibili.com/bangumi/play/ss44871"),
            Some(PgcRef::Season("44871".into()))
        );
        // 分享链常带跟踪参数,照样认得出号
        assert_eq!(
            extract_pgc("https://www.bilibili.com/bangumi/play/ep742483?spm_id_from=333.1007"),
            Some(PgcRef::Ep("742483".into()))
        );
        // UGC 稿件 / 短链 / bangumi 路径但没号 → 不是番剧
        assert_eq!(extract_pgc("https://www.bilibili.com/video/BV1xx411c7mD"), None);
        assert_eq!(extract_pgc("https://b23.tv/abcdef"), None);
        assert_eq!(extract_pgc("https://www.bilibili.com/bangumi/play/"), None);
        // 「ep」「ss」这两个字母组合出现在别处不该误认(必须在 bangumi 路径下)
        assert_eq!(extract_pgc("https://example.com/deep/ep123"), None);
    }

    /// 夹具形状照真实 `pgc/view/web/season` 返回(季 44871「安全警长啦咘啦哆」,52 集)。
    #[test]
    fn parse_season_builds_queue_from_pgc_episodes() {
        let result = serde_json::json!({
            "season_id": 44871,
            "season_title": "安全警长啦咘啦哆",
            "episodes": [
                {"id": 742483, "ep_id": 742483, "bvid": "BV1aM4y1B7L9",
                 "title": "1", "long_title": "小鸡弟弟失踪案",
                 "link": "https://www.bilibili.com/bangumi/play/ep742483"},
                {"id": 742484, "ep_id": 742484, "title": "2", "long_title": "犀牛弟弟被抓走了"},
                {"id": 742485, "ep_id": 742485, "title": "3", "long_title": ""}
            ]
        });
        let (key, eps) = parse_season(&result).unwrap();
        assert_eq!(key, "bili:pgc:44871", "季 key 用 pgc 前缀,绝不与 ugc_season 的 bili:season: 撞车");
        assert_eq!(eps.len(), 3);
        // 集身份 = ep 号;url 必须用 bangumi/play/ep 形 —— 番剧集自带的 bvid 在 UGC view
        // 端点是 -404、yt-dlp 也放不了,存 bvid 形等于存了个放不出来的地址。
        assert_eq!(eps[0].id, "ep742483");
        assert_eq!(eps[0].url, "https://www.bilibili.com/bangumi/play/ep742483");
        assert_eq!(eps[0].title, "第1集 小鸡弟弟失踪案");
        assert_eq!(eps[1].url, "https://www.bilibili.com/bangumi/play/ep742484");
        assert_eq!(eps[2].title, "第3集", "没副标题就只报集号");
    }

    #[test]
    fn parse_season_keeps_non_numeric_episode_labels() {
        let (_, eps) = parse_season(&serde_json::json!({
            "season_id": 3,
            "episodes": [
                {"ep_id": 1, "title": "OVA", "long_title": "特别篇"},
                {"ep_id": 2, "title": "", "long_title": ""}
            ]
        }))
        .unwrap();
        assert_eq!(eps[0].title, "OVA 特别篇", "非数字集号原样保留,别硬套「第X集」");
        assert_eq!(eps[1].title, "第2集", "两边都空 → 按序号兜底");
    }

    #[test]
    fn parse_season_edge_cases() {
        // 只有 1 集 / 没有 episodes → 不成系列
        assert!(parse_season(&serde_json::json!({
            "season_id": 1, "episodes": [{"ep_id": 9, "title": "1"}]
        }))
        .is_none());
        assert!(parse_season(&serde_json::json!({ "season_id": 1 })).is_none());
        // 拼不出可播地址的条目跳过(缺 ep 号)
        let (_, eps) = parse_season(&serde_json::json!({
            "season_id": 2,
            "episodes": [{"ep_id": 11, "title": "1"}, {"title": "坏条目"}, {"ep_id": 13, "title": "3"}]
        }))
        .unwrap();
        assert_eq!(eps.len(), 2, "缺 ep 号的条目跳过,不生成放不了的地址");
        // 缺 season_id 也别丢掉队列 —— 拿首集 ep 号当季身份(首集不会变)
        let (key, _) = parse_season(&serde_json::json!({
            "episodes": [{"ep_id": 5, "title": "1"}, {"ep_id": 6, "title": "2"}]
        }))
        .unwrap();
        assert_eq!(key, "bili:pgc:ep5");
    }

    /// 单测只管纯函数,而接线(端点 / 参数名 / 番剧载荷在 `result` 不在 `data`)才是最容易错的
    /// 一层 —— 这条打真网把 `episodes()` 整条链验穿。「番剧放一集就停」的回归守卫。
    /// `cargo test -p larkwing-core --lib media::bilibili::tests::real_bangumi_season -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "打真网(B 站 PGC 端点),开发机手动跑"]
    async fn real_bangumi_season() {
        let bili = Bilibili::new();
        // 安全警长啦咘啦哆(旧名「拉布拉多警长」)第 1 集,52 集的季。
        let ep_url = "https://www.bilibili.com/bangumi/play/ep742483";
        let (key, eps) = bili
            .episodes(ep_url, None)
            .await
            .expect("请求不该报错")
            .expect("番剧应发现出整季,不该退化成单集");
        println!("key={key} 共 {} 集", eps.len());
        for e in eps.iter().take(3) {
            println!("   {} | {} | {}", e.id, e.title, e.url);
        }
        assert_eq!(key, "bili:pgc:44871");
        assert!(eps.len() >= 50, "这季有 52 集,拿到 {} 集", eps.len());
        // 用户贴进来的那一集必须能在队列里精确匹配上 —— 否则 build_queue 认不出「点的是第几集」。
        assert!(eps.iter().any(|e| e.url == ep_url), "首集 url 应与用户贴的形态逐字一致");
        // ss 形(整季链接)走同一个端点、应得同一个季。
        let (key2, eps2) = bili
            .episodes("https://www.bilibili.com/bangumi/play/ss44871", None)
            .await
            .expect("请求不该报错")
            .expect("ss 形也该发现出整季");
        assert_eq!((key2, eps2.len()), (key, eps.len()), "ep 形与 ss 形应指向同一季");
    }
}
