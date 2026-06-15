//! B 站源:公开搜索 API(robot bilibili/api.py 移植)。搜索不走 yt-dlp ——
//! 直调 API 快,且返回结构化的标题/UP主/时长,正好喂播放卡片;流解析才归 yt-dlp。
//! 已知风险(robot 注释原样继承):B 站可能收紧 WBI 签名,届时此处拿到 -412/-403,
//! 错误按 RiskControl 上抛(带登录态时概率显著降低),签名实现参考 yt-dlp。

use anyhow::{anyhow, Result};
use async_trait::async_trait;

use super::{MediaHit, MediaSource, SearchError};

const SEARCH_URL: &str = "https://api.bilibili.com/x/web-interface/search/type";
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
}
