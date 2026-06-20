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

    /// 多集发现:view API 一次拿回 `pages`(分P)与 `ugc_season`(合集)。**尽力件**——
    /// 拿不到(短链 / 风控 / 非视频)一律 `Ok(None)` 退化成单集,绝不挡播放(风控后续由 resolve
    /// 的 AuthRequired 引导登录,登录重放时带 cookie 再发现一次)。
    async fn episodes(
        &self,
        page_url: &str,
        cookie_header: Option<&str>,
    ) -> Result<Option<(String, Vec<EpisodeRef>)>> {
        let Some(bvid) = extract_bvid(page_url) else {
            return Ok(None); // b23.tv 短链 / av 号 / 番剧 ep → 不在分P/合集发现范围
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
}
