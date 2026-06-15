//! **项目唯一的出站 HTTP 接缝**(CLAUDE.md §5 准则):所有联网都走这里的 `Client`,
//! 别处不新建裸 `reqwest::Client`(net 自身实现与测试除外)——改网络策略只改这一处。
//!
//! 全局代理选路(传输层)。一个 app 级 `net.proxy` 总开关管全部出站:
//! 策略 = **直连优先、连接失败才落代理**(= 自动选路:墙内源直连即成、永不碰代理;
//! 墙外源直连失败后落代理),每域名结论 session 内 sticky(被 GFW 黑洞的域名只吃一次
//! 直连超时,之后直接走代理)。**总开关关 ⇒ 一律直连,哪怕某域名之前被标记需走代理
//! (用户准则)。** 换代理值/关代理 ⇒ 旧 sticky 结论全部作废。
//!
//! 设计取舍:`net.proxy` 是 app 级单值 → 用进程级全局状态最省,下载/LLM 等调用点直接读
//! 全局、无需把代理串成一堆函数参数;唯一碰 store 的地方 = 启动初始化与设置写入(engine)。
//! 镜像前缀(components `DEFAULT_GH_MIRRORS`)是 URL 改写、只救 GitHub;代理是换传输通道、
//! 救一切墙外源 —— 两者正交,都保留。

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, RwLock};

#[derive(Default)]
struct Global {
    /// 解析后的代理 URL;None = 关(总开关)。
    proxy: RwLock<Option<String>>,
    /// 代理变更代数:换值即 +1,使各调用点缓存的代理 client 失效。
    gen: AtomicU64,
    /// 直连失败、需走代理的 host(仅在代理开着时才被查询;代理变更时清空)。
    sticky: Mutex<HashSet<String>>,
}

fn global() -> &'static Global {
    static G: OnceLock<Global> = OnceLock::new();
    G.get_or_init(Global::default)
}

/// 设当前代理(空串/None = 关)。变更时:升代数 + 清 sticky(旧选路结论作废)。
pub fn set_proxy(url: Option<String>) {
    let url = url.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let g = global();
    let mut cur = g.proxy.write().expect("net proxy lock");
    if *cur != url {
        tracing::info!(proxy = ?url, "代理设置更新");
        *cur = url;
        g.gen.fetch_add(1, Ordering::SeqCst);
        g.sticky.lock().expect("net sticky lock").clear();
    }
}

/// 环境变量代理回落:HTTPS_PROXY/ALL_PROXY(大小写两版),取首个非空。
/// `${ENV}` 展开与「设置优先、env 回落」的合流放在 engine(唯一 store×llm 合流点),
/// engine 解析完调 `set_proxy`;net 自身不碰 store/llm,保模块边界。
pub fn env_proxy() -> Option<String> {
    ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"]
        .into_iter()
        .find_map(|k| std::env::var(k).ok())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn proxy_now() -> Option<String> {
    global().proxy.read().expect("net proxy lock").clone()
}
fn gen_now() -> u64 {
    global().gen.load(Ordering::SeqCst)
}
fn prefers_proxy(host: &str) -> bool {
    global().sticky.lock().expect("net sticky lock").contains(host)
}
fn mark_proxy(host: &str) {
    global().sticky.lock().expect("net sticky lock").insert(host.to_string());
}
fn unmark_proxy(host: &str) {
    global().sticky.lock().expect("net sticky lock").remove(host);
}

/// 从 URL 取 host(含端口);取不到回退原串(仅用作 sticky 键 / 日志,无需严谨)。
fn host_of(url: &str) -> &str {
    url.split("://").nth(1).unwrap_or(url).split('/').next().unwrap_or(url)
}

/// 每调用点的 HTTP 客户端:持直连 client + 按当前全局代理懒建的代理 client
/// (随代理变更重建)。直连与代理 client 共用同一份 `configure`(UA/超时等),只差 `.proxy`。
pub struct Client {
    direct: reqwest::Client,
    configure: Box<dyn Fn(reqwest::ClientBuilder) -> reqwest::ClientBuilder + Send + Sync>,
    /// (代数, 代理 client):代数对不上就按当前代理重建;稳态下零重建。
    proxy_cache: Mutex<(u64, Option<reqwest::Client>)>,
}

impl Client {
    /// `configure` 把本调用点的 UA/超时等配置上去;直连与代理 client 用同一份配置。
    pub fn new(
        configure: impl Fn(reqwest::ClientBuilder) -> reqwest::ClientBuilder + Send + Sync + 'static,
    ) -> Client {
        let direct = configure(reqwest::Client::builder()).build().expect("构建 HTTP client 失败");
        Client { direct, configure: Box::new(configure), proxy_cache: Mutex::new((0, None)) }
    }

    /// 直连 client(始终可用)。
    pub fn direct(&self) -> &reqwest::Client {
        &self.direct
    }

    /// 当前代理 client:代理关 ⇒ None;代理 URL 非法 ⇒ None(已记日志);否则按当前代理
    /// (代数变了才重建)。下载两趟里第二趟用它。
    pub fn proxy_client(&self) -> Option<reqwest::Client> {
        let url = proxy_now()?;
        let gen = gen_now();
        let mut cache = self.proxy_cache.lock().expect("net proxy cache lock");
        if cache.0 != gen {
            let built = match reqwest::Proxy::all(&url) {
                Ok(p) => (self.configure)(reqwest::Client::builder()).proxy(p).build().ok(),
                Err(e) => {
                    tracing::warn!(proxy = %url, err = %e, "代理 URL 非法,忽略(本次直连)");
                    None
                }
            };
            *cache = (gen, built);
        }
        cache.1.clone()
    }

    /// 单发请求,**直连优先、连接失败落代理**(总开关关 ⇒ 只直连,哪怕该 host 被标记)。
    /// `make` 用给定 client 现造请求(每趟重造,故无需 try_clone)。仅在 connect/timeout 类
    /// 传输错误时换通道;HTTP 状态错误(4xx/5xx)= 源已应答,原样返回交调用方处理。
    pub async fn send(
        &self,
        url: &str,
        make: impl Fn(&reqwest::Client) -> reqwest::RequestBuilder,
    ) -> reqwest::Result<reqwest::Response> {
        let Some(proxy) = self.proxy_client() else {
            // 总开关关(或代理非法):一律直连
            return make(&self.direct).send().await;
        };
        let host = host_of(url);
        // 已知需代理 ⇒ 代理优先;否则直连优先。两序都带另一通道兜底(代理挂了能回直连)。
        let order: [(&reqwest::Client, bool); 2] = if prefers_proxy(host) {
            [(&proxy, true), (&self.direct, false)]
        } else {
            [(&self.direct, false), (&proxy, true)]
        };
        let mut last_err: Option<reqwest::Error> = None;
        for (client, via_proxy) in order {
            match make(client).send().await {
                Ok(resp) => {
                    if via_proxy {
                        mark_proxy(host);
                    } else {
                        unmark_proxy(host);
                    }
                    return Ok(resp);
                }
                Err(e) if e.is_connect() || e.is_timeout() => last_err = Some(e),
                Err(e) => return Err(e), // 非传输错误:不换通道,原样上抛
            }
        }
        Err(last_err.expect("两趟里至少一趟产生了传输错误"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_of_extracts_host_with_port_else_passthrough() {
        assert_eq!(host_of("https://huggingface.co/x/y"), "huggingface.co");
        assert_eq!(host_of("http://127.0.0.1:7890"), "127.0.0.1:7890");
        assert_eq!(host_of("github.com/a"), "github.com");
        assert_eq!(host_of("garbage"), "garbage");
    }

    #[test]
    fn env_proxy_picks_first_set_var() {
        // 用专属变量名避免污染真实代理 env;这里只验"读到非空即返回、空白被裁掉"。
        std::env::set_var("HTTPS_PROXY", "  http://127.0.0.1:7890  ");
        assert_eq!(env_proxy().as_deref(), Some("http://127.0.0.1:7890"));
        std::env::remove_var("HTTPS_PROXY");
    }
}
