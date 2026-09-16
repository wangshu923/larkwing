//! relay · webrender 回传信箱:`POST /collect/{token}` 一次性投递(回环、512KB、CORS 预检兜底)。

use super::*;

/// webrender 回传:一次性 token → 取走信箱、投递页面 JSON。未知/已用 token 一律 404
/// (页面世界不可信,不给探测面);载荷缓冲上限防恶意页灌爆(注入脚本自身只发 ~10KB)。
const COLLECT_MAX_BYTES: usize = 512 * 1024;

pub(super) async fn collect(
    State(state): State<Arc<Inner>>,
    AxPath(token): AxPath<String>,
    body: String,
) -> Response {
    if body.len() > COLLECT_MAX_BYTES {
        return bad(StatusCode::PAYLOAD_TOO_LARGE);
    }
    let sender = state.collect.lk().remove(&token);
    let Some(tx) = sender else { return bad(StatusCode::NOT_FOUND) };
    let _ = tx.send(body); // 发起方已放弃(超时收摊)= 静默丢,无人可通知
    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .header("access-control-allow-origin", "*")
        .body(Body::empty())
        .unwrap_or_else(|_| bad(StatusCode::INTERNAL_SERVER_ERROR))
}

pub(super) async fn collect_preflight() -> Response {
    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .header("access-control-allow-origin", "*")
        .header("access-control-allow-methods", "POST, OPTIONS")
        .header("access-control-allow-headers", "Content-Type")
        .header("access-control-max-age", "86400")
        .body(Body::empty())
        .unwrap_or_else(|_| bad(StatusCode::INTERNAL_SERVER_ERROR))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// webrender 回传信箱:注册 → 页面 POST → 接收端拿到原文;token 一次性(二投 404);
    /// 发起方放弃(drop rx)的死项被下次注册清扫。
    #[tokio::test]
    async fn collect_mailbox_roundtrip_once_and_sweeps_dead() {
        let relay = Relay::start().await.unwrap();
        let (url, rx) = relay.register_collect();
        let http = reqwest::Client::new();
        let resp = http.post(&url).body("{\"title\":\"页\"}").send().await.unwrap();
        assert!(resp.status().is_success());
        assert_eq!(rx.await.unwrap(), "{\"title\":\"页\"}");
        // 同 token 再投 = 404(一次性)
        let resp = http.post(&url).body("x").send().await.unwrap();
        assert_eq!(resp.status().as_u16(), 404);
        // 放弃的信箱:drop rx → 下次注册清扫,不堆积
        let (_url2, rx2) = relay.register_collect();
        drop(rx2);
        let _ = relay.register_collect();
        assert_eq!(relay.inner.collect.lk().len(), 1, "死项被清扫,只剩新注册的");
    }
}
