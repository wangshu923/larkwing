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
use std::time::Duration;
use crate::lockext::{LockExt, RwLockExt};

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
    let mut cur = g.proxy.wr();
    if *cur != url {
        tracing::info!(proxy = ?url.as_deref().map(scrub_secrets), "代理设置更新");
        *cur = url;
        g.gen.fetch_add(1, Ordering::SeqCst);
        g.sticky.lk().clear();
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
    global().proxy.rd().clone()
}
fn gen_now() -> u64 {
    global().gen.load(Ordering::SeqCst)
}
fn prefers_proxy(host: &str) -> bool {
    global().sticky.lk().contains(host)
}
fn mark_proxy(host: &str) {
    global().sticky.lk().insert(host.to_string());
}
fn unmark_proxy(host: &str) {
    global().sticky.lk().remove(host);
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
        let mut cache = self.proxy_cache.lk();
        if cache.0 != gen {
            let built = match reqwest::Proxy::all(&url) {
                Ok(p) => (self.configure)(reqwest::Client::builder()).proxy(p).build().ok(),
                Err(e) => {
                    tracing::warn!(proxy = %scrub_secrets(&url), err = %scrub_secrets(&e.to_string()), "代理 URL 非法,忽略(本次直连)");
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

/// 大文件下载专用的 `Client`(**构造单源**:web_download 与音乐下载两处曾各写一份)。
///
/// - **连接超时固定 10s**:死链快失败,别让用户干等。
/// - `total_timeout = Some(t)`:回合内同步档(现两处都是 280s,回合内够用)。
/// - `total_timeout = None`:后台档**不设总超时** —— 几 GB 的文件跑几十分钟是常态,
///   总超时会把它腰斩;停不下来那头由票据取消 + bgtasks 的卡死看门狗兜。
/// - `ua = Some(…)`:抓页面那一路要装成浏览器;`None` = 不设 UA
///   (音乐下载的防盗链 Referer/UA 由 yt-dlp 逐请求给,客户端层不写死)。
///
/// 走这里 = 仍是 `net::Client`(§4.6 出站唯一接缝:全局代理总开关 / 直连优先 / per-host sticky)。
pub(crate) fn download_client(
    ua: Option<&'static str>,
    total_timeout: Option<Duration>,
) -> Client {
    Client::new(move |b| {
        let b = b.connect_timeout(Duration::from_secs(DOWNLOAD_CONNECT_TIMEOUT_SECS));
        let b = match ua {
            Some(u) => b.user_agent(u),
            None => b,
        };
        match total_timeout {
            Some(t) => b.timeout(t),
            None => b,
        }
    })
}

/// 下载类连接超时(单源;见 `download_client`)。
const DOWNLOAD_CONNECT_TIMEOUT_SECS: u64 = 10;

/// 渲染后的错误/日志文本里把 URL 携带的凭证抹掉(**唯一收口**,2026-08-22)。
///
/// **为什么必须有这道**:Telegram 把 bot token 放在 URL 路径里(`/bot{token}/getUpdates`,共 6 处),
/// 而 `reqwest` 的 `Error` Display **必带完整 URL** —— 0.12.28 的 `src/error.rs` 里就是
/// `write!(f, " for url ({url})")`。于是任何 `format!("{e:#}")` 都会把 token 写进
/// ① `larkwing.log`;② `ctx.set_state` 的渠道状态行(**设置页直接显示**);
/// ③ `send_file` 失败时的工具结果 —— 那是**喂给模型的观察、还会落进 tool 行**。
/// 钉钉的 Stream 握手 URL 同理带 `?ticket=<凭证>`。
///
/// 放在 net(出站 HTTP 的唯一接缝 §4.6)而不是 channels:引擎侧渲染工具错误也要用它,
/// 而 engine 不该反向依赖 channels(§6.1 方向)。收在「错误变成字符串」这一层而不是逐个调用点 map_err:错误对象本身不出进程,只以字符串出去,
/// 所以这一层盖住就没有漏网的;新增日志点照抄 `scrub(&e)` 即可。
pub(crate) fn scrub_secrets(s: &str) -> String {
    const MASK: &str = "<已隐去>";
    // ⓪ URL 里的 userinfo(`scheme://user:pass@host`)—— 代理地址就常是这形状,而它会被打进日志
    //    (`代理 URL 非法,忽略`)。只在 `://` 与随后第一个 `/`/空白之前找 `@`,免得把正文里的
    //    邮箱地址也抹掉。
    let s = &{
        let mut out = String::with_capacity(s.len());
        let mut rest = s;
        while let Some(i) = rest.find("://") {
            let (head, tail) = rest.split_at(i + 3);
            out.push_str(head);
            let stop = tail
                .find(|c: char| c == '/' || c.is_whitespace() || c == ')' || c == '"')
                .unwrap_or(tail.len());
            match tail[..stop].find('@') {
                Some(at) => {
                    out.push_str(MASK);
                    out.push('@');
                    rest = &tail[at + 1..];
                }
                None => {
                    out.push_str(&tail[..stop]);
                    rest = &tail[stop..];
                }
            }
        }
        out.push_str(rest);
        out
    }[..];
    // ① Telegram 的 `/bot<token>` 路径段(api 与 file 两种 URL 同形)。
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("/bot") {
        let (head, tail) = rest.split_at(i + 4);
        out.push_str(head);
        // token 到下一个 `/` 或空白/括号为止;紧跟 `/` 说明不是 token(如 `/bot/xxx`),原样留。
        let end = tail
            .find(|c: char| c == '/' || c.is_whitespace() || c == ')' || c == '"')
            .unwrap_or(tail.len());
        if end == 0 {
            rest = tail;
            continue;
        }
        out.push_str(MASK);
        rest = &tail[end..];
    }
    out.push_str(rest);
    // ② query 里的敏感参数(钉钉 ticket、各家 token/secret/sign…)。
    const SENSITIVE: [&str; 7] = ["ticket", "token", "access_token", "session", "secret", "sign", "key"];
    let Some(q) = out.find('?') else { return out };
    let (head, query) = out.split_at(q + 1);
    let mut scrubbed = String::with_capacity(out.len());
    scrubbed.push_str(head);
    for (i, pair) in query.split('&').enumerate() {
        if i > 0 {
            scrubbed.push('&');
        }
        match pair.split_once('=') {
            Some((k, _)) if SENSITIVE.iter().any(|s| k.to_ascii_lowercase().contains(s)) => {
                scrubbed.push_str(k);
                scrubbed.push('=');
                scrubbed.push_str(MASK);
            }
            _ => scrubbed.push_str(pair),
        }
    }
    scrubbed
}

/// `scrub_secrets(&format!("{e:#}"))` 的简写 —— 渠道里渲染错误一律走它,别再裸 `format!`。
pub(crate) fn scrub(e: &anyhow::Error) -> String {
    scrub_secrets(&format!("{e:#}"))
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

    /// 凭证脱敏:用**真实形状**的 reqwest 错误串钉住 —— reqwest 0.12 的 Error Display 是
    /// 「…… for url (<完整 URL>)」,Telegram 的 token 就在路径里。
    #[test]
    fn scrub_strips_bot_token_and_sensitive_query() {
        const TOKEN: &str = "123456789:AAEhBOweik6ad9r_jsbHNTaWzS0jLmNQpEo";
        let err = format!(
            "getUpdates 请求失败: error sending request for url \
             (https://api.telegram.org/bot{TOKEN}/getUpdates?offset=1&timeout=50)"
        );
        let got = scrub_secrets(&err);
        assert!(!got.contains(TOKEN), "token 必须被抹掉:{got}");
        assert!(got.contains("/bot<已隐去>/getUpdates"), "只抹 token、保留可诊断的形状:{got}");
        assert!(got.contains("offset=1"), "无害参数不动:{got}");

        // 文件下载 URL 同形(/file/bot<token>/<path>)
        let dl = format!("下载失败 for url (https://api.telegram.org/file/bot{TOKEN}/photos/x.jpg)");
        assert!(!scrub_secrets(&dl).contains(TOKEN));

        // 钉钉 Stream 握手:凭证在 query
        let ding = "连接失败 for url (wss://x.dingtalk.com/connect?ticket=abc123XYZ&uid=7)";
        let got = scrub_secrets(ding);
        assert!(!got.contains("abc123XYZ"), "ticket 必须被抹掉:{got}");
        assert!(got.contains("uid=7"), "无害参数不动:{got}");

        // 幂等 + 不误伤普通文本
        assert_eq!(scrub_secrets(&got), got, "抹过再抹结果不变");
        assert_eq!(scrub_secrets("钉钉回复失败:HTTP 500"), "钉钉回复失败:HTTP 500");
        // `/bot/` 后面没东西 = 不是 token,原样留(别把正常路径抹花)
        assert_eq!(scrub_secrets("see /bot/docs"), "see /bot/docs");

        // 代理地址里的 userinfo(会被打进日志)—— 只抹 URL 里的,正文邮箱不动
        let px = scrub_secrets("代理 URL 非法 http://alice:s3cr3t@127.0.0.1:7890 忽略");
        assert!(!px.contains("s3cr3t"), "代理口令必须抹掉:{px}");
        assert!(px.contains("@127.0.0.1:7890"), "主机保留可诊断:{px}");
        assert_eq!(
            scrub_secrets("联系 a@b.com 报错"),
            "联系 a@b.com 报错",
            "正文里的邮箱不该被当 userinfo"
        );
    }
}
