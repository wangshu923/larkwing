//! 能力轴:外网(搜/读)。**搜索即抓取**:web_search 一次调用带回正文证据片段
//! (robot 的"链接堆 + 模型串行 fetch"病根在此修掉);web_fetch 留给"用户给了具体
//! 链接"的场景。两工具共享一个 WebClient(连接池 + 短 TTL 正文缓存 —— app 级
//! 无归属资产,住工具单例字段,不进 ToolCtx)。
//! watch-item(PLAN §10):网页内容是不可信文本,注入风险记档;结果只作观察喂回。

use std::sync::Arc;

use anyhow::Context;
use async_trait::async_trait;
use futures_util::future::join_all;

use crate::web::{clip, WebClient};

use super::{Tool, ToolCtx, ToolSpec};

/// 默认带正文的条数与单篇预算(证据片段,不是整页)。
const CONTENT_TOP_N: usize = 3;
const PIECE_MAX_CHARS: usize = 1200;
const FETCH_MAX_CHARS: usize = 6000;

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
        let with_content =
            args.get("fetch_content").and_then(serde_json::Value::as_bool).unwrap_or(true);

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
                description: "读一个具体网页的正文(用户给了链接,或 web_search 的正文片段\
                              不够、要看某条的全文时)。",
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
        let (title, text) = self.web.fetch_text(url).await?;
        Ok(format!("《{}》\n{}\n\n{}", title, url, clip(&text, FETCH_MAX_CHARS)))
    }
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
        ToolCtx { user_id: 1, conv_id: 1, media: MediaRuntime::detached(store.clone()), store }
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
}
