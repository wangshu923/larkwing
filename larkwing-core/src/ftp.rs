//! FTP 下载(RFC 959)。**FTP 是开放标准,任何客户端都能连** —— 协议里没有 User-Agent
//! 这类东西给服务器识别客户端,所以不存在「只有某个下载器能下」。
//!
//! 那为什么有些 `ftp://` 链接「只有迅雷下得动」?**因为那台服务器已经死了。** 迅雷早把
//! 「这个 URL → 这个文件哈希」记进索引,从自己的 P2SP 缓存把字节给你、根本没碰 FTP。
//! 我们没有那个缓存池,所以**死链谁也变不出来**;活着的服务器我们照常下。判据看错误:
//! 连不上 = 服务器死了;连上但 550 = 文件名对不上(见下面编码那条)。
//!
//! 为什么还值得做:国内影视资源圈一直在用 ftp 直链(浏览器 2021 年前后删掉 FTP 支持,
//! 但那是浏览器的事,服务器还在),而**迅雷/快车专用链拆开后大量就是 ftp://**
//! (`tools::normalize_link` 拆的就是它们)。
//!
//! ⚠️ **中文文件名编码是已知边界**:中文 FTP 服务器历史上大量用 GBK 存文件名,而
//! suppaftp 的路径参数只收 `&str`(UTF-8)。要发 GBK 字节只能 `from_utf8_unchecked` ——
//! 那是 **UB,不进产品**。故本版只发 UTF-8(配 `OPTS UTF8 ON`),撞上只认 GBK 的老服务器
//! 会 550,错误话术里点明这一条,别让人以为是链接死了。真需要 GBK 再给 suppaftp 提个
//! 收字节的 PR(或最小 patch),不在这儿塞 unsafe。
//!
//! **不走 `net::Client`**(§4.6 管的是出站 HTTP):FTP 不是 HTTP,代理这一块因此缺失
//! (FTP over SOCKS 要另接)。如实记档。
//!
//! **断点续传(同一次下载内)**:影视量级的 ftp 文件一下几十分钟,数据连接被中间设备掐掉、
//! 服务器主动断、半路停滞都是常态。传输中断不再直接判失败,而是重新连接登录 → `REST <已落盘字节>`
//! → `RETR`,数据追加写进同一个临时件,最多续 `RESUME_MAX_ATTEMPTS` 次、两次之间指数退避;
//! **偏移量永远取临时件的真实长度**(不信内存计数)。服务器不支持 REST(没回 350)= 立即退回
//! 明白话,不空转重试。每次续传都进日志(§3.5 重试不静默)。**跨次调用的续传(重启程序后接着下)
//! 不在此列**——临时件由调用方在失败时清理,现状不变。

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use suppaftp::tokio::AsyncFtpStream;
use suppaftp::types::FileType;
use suppaftp::FtpError;

use crate::files::human_size;

/// 建连 + 登录的总超时。**死链是这里最常见的情形**,要快点失败并给明白话,
/// 别让用户对着转圈等。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// 取体积(SIZE)/ REST / 请求文件 这类单条命令的超时:连上了就该很快。
const CMD_TIMEOUT: Duration = Duration::from_secs(15);
/// 传输中多久没有新字节判「停滞」——**不再直接判失败**,而是断开重连续传(见下面两常量)。
/// FTP 被动模式的数据连接被中间设备掐掉是常见死法,掐掉后往往一个字节都不再来、也不关连接。
const STALL_TIMEOUT: Duration = Duration::from_secs(90);
/// 断点续传:**同一次下载内**,传输中断(数据连接被掐 / 停滞 / 流提前结束 / 续传阶段重连失败)
/// 最多重连续传几次。**§4.11 待用户确认** —— 先按「够救几次掐线、别对死服务器空转」定 5。
pub const RESUME_MAX_ATTEMPTS: u32 = 5;
/// 两次续传之间的指数退避:2s → 4s → 8s → 封顶 15s。**§4.11 待用户确认**。
/// (资源站常限「同 IP 连接数」,掐线后立刻重连多半 530,退避是给服务器时间回收旧连接。)
const RESUME_BACKOFF_BASE: Duration = Duration::from_secs(2);
const RESUME_BACKOFF_CAP: Duration = Duration::from_secs(15);
/// 用户取消的统一话术(job 收尾按 `ticket.is_cancelled()` 分流,不靶这句;三处检查点共用一份免漂)。
const CANCELLED: &str = "按要求停下了";

/// 一个 ftp:// 目标(凭证已解出)。
#[derive(Clone)]
pub struct FtpTarget {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub pass: String,
    /// 服务器上的路径(已百分号解码,不含前导 `/` 之外的处理)。
    pub path: String,
    /// 落盘用的文件名(已净化)。
    pub filename: String,
}

/// **手写 Debug 遮住密码**(同 `web::HttpCred`):dytt 那类链接把账号密码写在 URL 里,
/// 派生 Debug 会把它们吐进日志/错误链。
impl std::fmt::Debug for FtpTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FtpTarget")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("pass", &"<已隐去>")
            .field("path", &self.path)
            .finish()
    }
}

/// 解析 `ftp://[用户[:密码]@]主机[:端口]/路径`。
///
/// dytt 那类链接的典型形状 = 凭证内嵌 + 非标准端口 + 中文文件名,例如
/// `ftp://用户:密码@主机:6163/影片名.mkv`。没带凭证的按匿名登录
/// (`anonymous`),调用方也可以从「设置·下载认证」按 host 补(见 `with_cred`)。
pub fn parse_ftp_url(url: &str) -> Result<FtpTarget> {
    let parsed = reqwest::Url::parse(url.trim()).context("这不是一个能解析的 ftp 地址")?;
    anyhow::ensure!(parsed.scheme() == "ftp", "不是 ftp:// 地址(收到 {}://)", parsed.scheme());
    let host = parsed
        .host_str()
        .filter(|h| !h.is_empty())
        .context("ftp 地址里没有主机名")?
        .to_string();
    let port = parsed.port().unwrap_or(21);
    // URL 里的凭证:百分号解码(密码里有 @ / : 时站点会编码)
    let user = match parsed.username() {
        "" => "anonymous".to_string(),
        u => crate::web::percent_decode(u),
    };
    let pass = match parsed.password() {
        Some(p) if !p.is_empty() => crate::web::percent_decode(p),
        // 匿名 FTP 的惯例口令(RFC 1635):给个邮箱形,不少服务器要求非空
        _ => "anonymous@".to_string(),
    };
    let path = crate::web::percent_decode(parsed.path());
    anyhow::ensure!(
        !path.is_empty() && path != "/",
        "ftp 地址里没有文件路径(我只下具体文件,不下整个目录)"
    );
    // 文件名 = 路径末段;净化 + Windows 保留名规避走 files 那套(与 web_download 同口径)
    let raw = path.rsplit('/').next().unwrap_or_default();
    let filename = crate::files::sanitize_filename(raw);
    anyhow::ensure!(!filename.is_empty(), "从 ftp 路径里认不出文件名: {path}");
    Ok(FtpTarget { host, port, user, pass, path, filename })
}

impl FtpTarget {
    /// URL 里没带凭证时,用「设置·下载认证」里配的那条补上(自家 NAS 的 FTP 就靠它;
    /// 密码全程不经模型 —— 同 web_download,§7.7 凭证不过桥)。
    pub fn with_cred(mut self, cred: Option<&crate::web::HttpCred>) -> FtpTarget {
        if self.user == "anonymous" {
            if let Some(c) = cred {
                if !c.user.trim().is_empty() {
                    self.user = c.user.clone();
                    self.pass = c.password.clone();
                }
            }
        }
        self
    }

    /// 给凭证查找用的 host 键(带端口,与 `web::cred_for` 同口径)。
    pub fn cred_host(&self) -> String {
        if self.port == 21 {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

/// 连上 + 登录 + 二进制 + 被动模式。**被动模式是必须的** —— 家庭 NAT 后面主动模式
/// (服务器回连客户端)几乎必失败。
async fn open(t: &FtpTarget) -> Result<AsyncFtpStream> {
    let mut ftp = tokio::time::timeout(
        CONNECT_TIMEOUT,
        AsyncFtpStream::connect((t.host.as_str(), t.port)),
    )
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "连 {}:{} 超时({} 秒)。这个 ftp 服务器多半已经关了 —— 这类资源站的服务器\
             是短命的,链接还挂在网页上、机器早没了。(迅雷那种「死链也能下」是从它自己的\
             缓存里拿,我们没有那个缓存池,变不出来。)",
            t.host,
            t.port,
            CONNECT_TIMEOUT.as_secs()
        )
    })?
    .with_context(|| format!("连不上 ftp 服务器 {}:{}", t.host, t.port))?;

    tokio::time::timeout(CONNECT_TIMEOUT, ftp.login(&t.user, &t.pass))
        .await
        .map_err(|_| anyhow::anyhow!("登录超时"))?
        .with_context(|| {
            format!("ftp 登录被拒(账号 {});这类链接的账号密码常写在地址里,可能已经改了", t.user)
        })?;
    // 告诉服务器用 UTF-8 传文件名。不支持就报错,忽略即可(老服务器按自己的编码来,
    // 撞上就是下面 RETR 的 550 —— 那条错误话术会点明 GBK)。
    let _ = ftp.site("OPTS UTF8 ON").await;
    ftp.transfer_type(FileType::Binary)
        .await
        .context("ftp 切二进制模式失败")?;
    ftp.set_mode(suppaftp::Mode::Passive);
    Ok(ftp)
}

/// 探体积(给「同步档 / 后台档」分档用)。取不到 = None(有些服务器不支持 SIZE),
/// **不当失败** —— 照 web_download 的口径,没有 Content-Length 也照下。
pub async fn probe_size(t: &FtpTarget) -> Result<Option<u64>> {
    let mut ftp = open(t).await?;
    let got = tokio::time::timeout(CMD_TIMEOUT, ftp.size(&t.path)).await;
    let _ = ftp.quit().await;
    Ok(match got {
        Ok(Ok(n)) => Some(n as u64),
        // SIZE 不被支持 / 被拒:不判死,交给 RETR 去试(它才是真正的判据)
        _ => None,
    })
}

/// 续传策略。生产恒走 `Default`(= 顶部那几个常量);单测注入零退避 / 短停滞,不真等几十秒。
#[derive(Clone, Debug)]
struct ResumePolicy {
    max_attempts: u32,
    backoff_base: Duration,
    backoff_cap: Duration,
    stall_timeout: Duration,
}

impl Default for ResumePolicy {
    fn default() -> Self {
        ResumePolicy {
            max_attempts: RESUME_MAX_ATTEMPTS,
            backoff_base: RESUME_BACKOFF_BASE,
            backoff_cap: RESUME_BACKOFF_CAP,
            stall_timeout: STALL_TIMEOUT,
        }
    }
}

impl ResumePolicy {
    /// 第 `attempt`(1 起)次续传前等多久:base × 2^(attempt−1),封顶 cap。
    fn backoff(&self, attempt: u32) -> Duration {
        let shift = attempt.saturating_sub(1).min(16);
        self.backoff_base
            .saturating_mul(1u32 << shift)
            .min(self.backoff_cap)
    }
}

/// 一次「连接 → (REST) → RETR → 读到底」尝试的结局,供续传循环分流。
enum Fault {
    /// 可续传的中断:数据连接被掐 / 停滞 / 流提前结束 / 续传阶段重连或登录失败。
    Transient(anyhow::Error),
    /// 服务器不支持从中间接着传(REST 没回 350,或 REST 之后的 RETR 被拒)—— 重试无意义。
    NoResume(anyhow::Error),
    /// 明确失败,不重试:用户取消 / 超体积上限 / 550 找不到文件 / 首连失败 / 本地写盘失败。
    Fatal(anyhow::Error),
}

/// 服务器回话原文(带三位应答码),给错误话术用;解不出 UTF-8 就只报码。
fn reply_text(resp: &suppaftp::types::Response) -> String {
    resp.as_string()
        .unwrap_or_else(|_| resp.status.code().to_string())
}

/// 一次下载的全部状态(跨续传保持:文件句柄 / 打点阈值)。
struct Transfer<'a> {
    t: &'a FtpTarget,
    /// 临时件句柄:首连时建(截断),之后每次续传都在**同一个句柄**上追加。
    ///
    /// 句柄用 `Arc<Mutex<..>>` 包着是为了能借进 `spawn_blocking`(写盘不占 tokio worker,
    /// § 效率审计 2026-09-08);**刻意仍是 `std::fs::File` 而不是 `tokio::fs::File`** ——
    /// 后者自带内部缓冲,而 `on_disk()`(REST 偏移的唯一来源)读的是文件真实长度:
    /// 缓冲里压着没落盘的字节 = 偏移偏小 = 续传重复追加同一段,拼出来的文件是坏的。
    /// std 的 write 直落 OS,「已落盘字节 == metadata().len()」这条不变量才成立。
    file: std::sync::Arc<std::sync::Mutex<std::fs::File>>,
    cap: u64,
    /// 调用方探到的体积(SIZE);None = 服务器不支持 SIZE,那就只能信流的 EOF。
    expected: Option<u64>,
    progress: Option<&'a crate::bgtasks::BgTicket>,
    policy: &'a ResumePolicy,
    /// 下一次打点的累计字节阈值(跨续传保持,免得每次重连都先打一枪)。
    next_beat: u64,
}

impl Transfer<'_> {
    /// 已落盘字节数 = REST 偏移的**唯一**来源(读文件真实长度,不信内存计数——
    /// 写盘半途失败 / 上一轮死在哪都由它兜)。
    fn on_disk(&self) -> Result<u64> {
        let f = self.file.lock().expect("ftp 临时件锁 poisoned");
        Ok(f.metadata().context("读不到临时文件长度")?.len())
    }

    /// 追加一段到临时件末尾 —— **在阻塞线程池上写**,别让慢盘 / NAS 的 write 把 tokio worker
    /// 按住(§ 效率审计 2026-09-08)。读缓冲一并借进闭包再还回来:免掉每段一次分配 / 拷贝。
    /// 返回 `(还回来的缓冲, 写盘结果)`。
    async fn write_chunk(&self, buf: Vec<u8>, n: usize) -> (Vec<u8>, std::io::Result<()>) {
        use std::io::Write;
        let file = self.file.clone();
        match tokio::task::spawn_blocking(move || {
            let res = file.lock().expect("ftp 临时件锁 poisoned").write_all(&buf[..n]);
            (buf, res)
        })
        .await
        {
            Ok(out) => out,
            // 阻塞任务本身没了(panic / 运行时正在关):当写盘失败处理,由调用方 Fatal 收
            Err(e) => (Vec::new(), Err(std::io::Error::other(e.to_string()))),
        }
    }

    fn cancelled(&self) -> bool {
        self.progress.map(|tk| tk.is_cancelled()).unwrap_or(false)
    }

    /// 一次尝试:连 →(offset>0 则 REST)→ RETR → 追加写到文件末尾直到 EOF,返回累计字节数。
    /// `resuming` = 这不是首连:首连失败按死链快失败(原话术),续传阶段连不上 / 登不上
    /// (资源站常限同 IP 连接数,旧连接没回收就 530)算一次可重试的中断。
    async fn attempt(&mut self, offset: u64, resuming: bool) -> Result<u64, Fault> {
        use tokio::io::AsyncReadExt;

        if self.cancelled() {
            return Err(Fault::Fatal(anyhow::anyhow!(CANCELLED)));
        }
        let t = self.t;
        let mut ftp = match open(t).await {
            Ok(ftp) => ftp,
            Err(e) if resuming => return Err(Fault::Transient(e)),
            Err(e) => return Err(Fault::Fatal(e)),
        };

        if offset > 0 {
            let off = usize::try_from(offset)
                .map_err(|_| Fault::Fatal(anyhow::anyhow!("续传偏移 {offset} 超出本机可表示范围")))?;
            match tokio::time::timeout(CMD_TIMEOUT, ftp.resume_transfer(off)).await {
                Ok(Ok(())) => tracing::debug!(offset, "ftp REST 已接受"),
                // 非 350 = 服务器不认 REST(502/500/501/504 都有见过),重试无意义
                Ok(Err(FtpError::UnexpectedResponse(resp))) => {
                    return Err(Fault::NoResume(anyhow::anyhow!(
                        "REST {offset} 被拒(服务器回:{})",
                        reply_text(&resp)
                    )));
                }
                Ok(Err(e)) => {
                    return Err(Fault::Transient(
                        anyhow::anyhow!(e).context("发 REST 时连接断了"),
                    ));
                }
                Err(_) => {
                    return Err(Fault::Transient(anyhow::anyhow!(
                        "发 REST 后服务器 {} 秒没回应",
                        CMD_TIMEOUT.as_secs()
                    )));
                }
            }
        }

        let mut stream = match tokio::time::timeout(CMD_TIMEOUT, ftp.retr_as_stream(&t.path)).await {
            Ok(Ok(s)) => s,
            Ok(Err(FtpError::UnexpectedResponse(resp))) => {
                // 550 恒 = 文件不在(首连 / 续传都一样);续传阶段的其他拒绝 = 接受了 REST 却不肯
                // 从中间发(RFC 3659:有的服务器 REST 回 350、到 RETR 才回 554),同归「不支持续传」。
                if offset > 0 && resp.status != suppaftp::Status::FileUnavailable {
                    return Err(Fault::NoResume(anyhow::anyhow!(
                        "REST {offset} 之后 RETR 被拒(服务器回:{})",
                        reply_text(&resp)
                    )));
                }
                let msg = if resp.status == suppaftp::Status::FileUnavailable {
                    format!(
                        "服务器上找不到这个文件({})。两种可能:① 链接里的文件名已经变了/被删了;\
                         ② **文件名是中文、而这台服务器用 GBK 编码存文件名** —— 这种我们当前下不了\
                         (只发 UTF-8 文件名),不是链接死了。(服务器回:{})",
                        t.path,
                        reply_text(&resp)
                    )
                } else {
                    format!("服务器拒绝了下载请求(回:{})", reply_text(&resp))
                };
                return Err(Fault::Fatal(anyhow::anyhow!(msg)));
            }
            Ok(Err(e)) => {
                let e = anyhow::anyhow!(e)
                    .context("ftp 数据连接建不起来(被动模式的数据端口被防火墙 / NAT 拦了?)");
                return Err(if resuming { Fault::Transient(e) } else { Fault::Fatal(e) });
            }
            Err(_) => {
                let e = anyhow::anyhow!("请求文件超时");
                return Err(if resuming { Fault::Transient(e) } else { Fault::Fatal(e) });
            }
        };

        // 累计 = 已落盘 + 本轮收到;每个字节都追加到同一个句柄末尾
        let mut got = offset;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = match tokio::time::timeout(self.policy.stall_timeout, stream.read(&mut buf)).await
            {
                Ok(Ok(n)) => n,
                Ok(Err(e)) => {
                    return Err(Fault::Transient(anyhow::anyhow!(e).context(format!(
                        "ftp 传输中断(已收到 {})",
                        human_size(got)
                    ))));
                }
                Err(_) => {
                    return Err(Fault::Transient(anyhow::anyhow!(
                        "下到 {} 之后 {} 秒没有新数据(数据连接多半被中间设备掐了)",
                        human_size(got),
                        self.policy.stall_timeout.as_secs()
                    )));
                }
            };
            if n == 0 {
                break;
            }
            got += n as u64;
            if got > self.cap {
                return Err(Fault::Fatal(anyhow::anyhow!(
                    "文件超过 {} 上限,已停止",
                    human_size(self.cap)
                )));
            }
            let (returned, wrote) = self.write_chunk(buf, n).await;
            buf = returned; // 缓冲还回来接着读(写失败时下面就退出了,不会再用它)
            if let Err(e) = wrote {
                return Err(Fault::Fatal(anyhow::anyhow!(e).context("写临时文件失败")));
            }
            if let Some(tk) = self.progress {
                if tk.is_cancelled() {
                    return Err(Fault::Fatal(anyhow::anyhow!(CANCELLED)));
                }
                if got >= self.next_beat {
                    self.next_beat = got + 1024 * 1024;
                    let (pct, text) = match self.expected {
                        Some(exp) => (
                            got.saturating_mul(100).checked_div(exp).unwrap_or(0) as usize,
                            format!("{} / {}", human_size(got), human_size(exp)),
                        ),
                        None => (0, human_size(got)),
                    };
                    tk.beat(pct, text);
                }
            }
        }

        // EOF。对着 SIZE 核对:比体积少 = 服务器半路关了数据连接(续传接着下);续传后比体积多 =
        // 服务器接受了 REST 却从头发,拼出来的文件不可信,整份作废(首连多出来的不管,SIZE 偶有虚报)。
        if let Some(exp) = self.expected {
            if got < exp {
                return Err(Fault::Transient(anyhow::anyhow!(
                    "数据流提前结束:只收到 {} / {}",
                    human_size(got),
                    human_size(exp)
                )));
            }
            if offset > 0 && got > exp {
                return Err(Fault::Fatal(anyhow::anyhow!(
                    "续传后收到的比文件体积还多({} > {}):服务器没从断点接着发,这份不可信,请整份重下",
                    human_size(got),
                    human_size(exp)
                )));
            }
        }
        // 收尾必须做:不 finalize 服务器不会回最终响应(suppaftp 明示)。收尾 / 告别失败不影响
        // 已落盘的字节,忽略。
        let _ = tokio::time::timeout(CMD_TIMEOUT, ftp.finalize_retr_stream(stream)).await;
        let _ = tokio::time::timeout(CMD_TIMEOUT, ftp.quit()).await;
        Ok(got)
    }
}

/// 退避等待,期间每 250ms 看一眼取消旗标(用户点了停,不该还干等十几秒)。
async fn wait_or_cancel(wait: Duration, progress: Option<&crate::bgtasks::BgTicket>) -> Result<()> {
    let deadline = Instant::now() + wait;
    loop {
        if progress.map(|tk| tk.is_cancelled()).unwrap_or(false) {
            return Err(anyhow::anyhow!(CANCELLED));
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok(());
        }
        tokio::time::sleep((deadline - now).min(Duration::from_millis(250))).await;
    }
}

/// 下载到 `dest`(调用方负责临时件与改名;失败时临时件也由调用方清理,现状不变)。
/// `cap` = 体积硬闸;`expected` = 调用方 SIZE 探到的体积(None = 服务器不支持 SIZE);
/// `progress` = 后台档才传票据,每 ~1MB 打点并查取消。
///
/// 传输中断自动断点续传(见模块注释),最多 `RESUME_MAX_ATTEMPTS` 次;服务器不支持 REST 立即退回。
pub async fn download_to(
    t: &FtpTarget,
    dest: &Path,
    cap: u64,
    expected: Option<u64>,
    progress: Option<&crate::bgtasks::BgTicket>,
) -> Result<u64> {
    download_with(t, dest, cap, expected, progress, &ResumePolicy::default()).await
}

async fn download_with(
    t: &FtpTarget,
    dest: &Path,
    cap: u64,
    expected: Option<u64>,
    progress: Option<&crate::bgtasks::BgTicket>,
    policy: &ResumePolicy,
) -> Result<u64> {
    // 建文件本身也别在 tokio worker 上做(NAS 上开个文件也能顿一下);拿回 std 句柄接着用,
    // 无缓冲语义不变(见 `Transfer::file` 那条注释)。
    let file = tokio::fs::File::create(dest)
        .await
        .with_context(|| format!("建不了文件 {}", dest.display()))?
        .into_std()
        .await;
    let file = std::sync::Arc::new(std::sync::Mutex::new(file));
    let mut xfer = Transfer { t, file, cap, expected, progress, policy, next_beat: 0 };
    let mut resumes: u32 = 0;
    loop {
        // REST 偏移 = 临时件此刻的真实长度(每轮现读;上一轮写到哪就从哪接)
        let offset = xfer.on_disk()?;
        let fault = match xfer.attempt(offset, resumes > 0).await {
            Ok(total) => return Ok(total),
            Err(f) => f,
        };
        let on_disk = xfer.on_disk().unwrap_or(offset);
        match fault {
            Fault::Fatal(e) => return Err(e),
            Fault::NoResume(e) => {
                return Err(e.context(format!(
                    "这台服务器不支持断点续传,只能整份重下(这次传到 {} 断了、已下的作废;\
                     可以再试一次,运气好一口气下完)",
                    human_size(on_disk)
                )));
            }
            Fault::Transient(e) => {
                if resumes >= policy.max_attempts {
                    let of = expected
                        .map(|x| format!(" / {}", human_size(x)))
                        .unwrap_or_default();
                    return Err(e.context(format!(
                        "续传了 {resumes} 次仍没下完(落盘 {}{of}),放弃了;可以稍后再试",
                        human_size(on_disk)
                    )));
                }
                resumes += 1;
                let wait = policy.backoff(resumes);
                // §3.5 重试不静默:每次续传都留痕(几次 / 从哪接 / 等多久 / 为什么断)
                tracing::info!(
                    host = %t.host,
                    port = t.port,
                    attempt = resumes,
                    max = policy.max_attempts,
                    on_disk = on_disk,
                    wait_ms = wait.as_millis() as u64,
                    "ftp 传输中断,准备断点续传(REST {on_disk}): {e:#}"
                );
                wait_or_cancel(wait, progress).await?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dytt_style_url_with_creds_and_port() {
        // dytt/荐片 那类链接的典型形状:凭证内嵌 + 非标准端口 + 中文文件名
        let t = parse_ftp_url("ftp://ygdy8:ygdy8@yg45.example.net:6163/示例片名.HD.1080p.mkv")
            .unwrap();
        assert_eq!(t.host, "yg45.example.net");
        assert_eq!(t.port, 6163);
        assert_eq!(t.user, "ygdy8");
        assert_eq!(t.pass, "ygdy8");
        assert_eq!(t.filename, "示例片名.HD.1080p.mkv");
    }

    #[test]
    fn defaults_to_anonymous_and_port_21() {
        let t = parse_ftp_url("ftp://files.example.org/pub/a.iso").unwrap();
        assert_eq!(t.port, 21);
        assert_eq!(t.user, "anonymous");
        assert!(!t.pass.is_empty(), "匿名口令要非空(不少服务器要求)");
        assert_eq!(t.filename, "a.iso");
        assert_eq!(t.cred_host(), "files.example.org", "标准端口不带端口后缀");
    }

    #[test]
    fn percent_encoded_name_and_password_are_decoded() {
        // 中文文件名在网页上常是百分号编码的
        let t = parse_ftp_url("ftp://u:p%40ss@h.example.com/%E7%89%87%E5%90%8D.mkv").unwrap();
        assert_eq!(t.pass, "p@ss", "密码里的 %40 要解成 @");
        assert_eq!(t.filename, "片名.mkv");
    }

    #[test]
    fn rejects_non_ftp_and_directory_urls() {
        assert!(parse_ftp_url("https://a.b/c.mkv").is_err(), "http 不该被当 ftp");
        assert!(parse_ftp_url("ftp://h.example.com/").is_err(), "只下具体文件,不下目录");
        assert!(parse_ftp_url("ftp:///no-host.mkv").is_err(), "缺主机名要退回");
    }

    #[test]
    fn cred_host_includes_nonstandard_port() {
        let t = parse_ftp_url("ftp://h.example.com:2121/a.bin").unwrap();
        assert_eq!(t.cred_host(), "h.example.com:2121");
    }

    #[test]
    fn with_cred_fills_anonymous_only() {
        let c = crate::web::HttpCred {
            host: "nas.local".into(),
            user: "me".into(),
            password: "pw".into(),
        };
        // 匿名 → 用配好的账号补上(自家 NAS 场景)
        let a = parse_ftp_url("ftp://nas.local/movies/x.mkv").unwrap().with_cred(Some(&c));
        assert_eq!(a.user, "me");
        assert_eq!(a.pass, "pw");
        // URL 自带凭证 → 不覆盖(链接里的更具体)
        let b = parse_ftp_url("ftp://u:p@nas.local/movies/x.mkv").unwrap().with_cred(Some(&c));
        assert_eq!(b.user, "u");
        assert_eq!(b.pass, "p");
    }

    #[test]
    fn debug_hides_embedded_password() {
        let t = parse_ftp_url("ftp://u:s3cr3t-do-not-log@h.example.com/a.mkv").unwrap();
        let shown = format!("{t:?}");
        assert!(!shown.contains("s3cr3t"), "内嵌密码不许进日志: {shown}");
        assert!(shown.contains("h.example.com"), "host 该留着好排查: {shown}");
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let p = ResumePolicy::default();
        let secs: Vec<u64> = (1..=RESUME_MAX_ATTEMPTS).map(|n| p.backoff(n).as_secs()).collect();
        assert_eq!(secs, [2, 4, 8, 15, 15], "2s/4s/8s… 封顶 15s");
        // 测试用零退避策略真为零(否则下面的续传测试会白等)
        assert_eq!(fast_policy().backoff(3), Duration::ZERO);
    }

    // ───────────── 断点续传:最小假 FTP 服务器 + 端到端 ─────────────

    /// 最小 tokio TCP 假 FTP 服务器(RFC 959 应答码;只实现本模块会发的动词:
    /// USER/PASS/SITE/TYPE/PASV/SIZE/REST/RETR/QUIT)。剧本按 RETR 次序决定每次传输怎么收场,
    /// 拿它验续传机器,不碰真网。
    mod fake_ftp {
        use std::sync::{Arc, Mutex};
        use std::time::Duration;

        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::{TcpListener, TcpStream};

        /// 一次 RETR 的收场。
        #[derive(Clone, Copy)]
        pub enum Cut {
            /// 完整发完,回 226
            Full,
            /// 发 n 字节后**主动关掉数据连接**(控制连接回 426)
            Close(usize),
            /// 发 n 字节后**挂住不动**(数据连接不关、控制连接不回)= 停滞
            Hang(usize),
        }

        pub struct Script {
            pub data: Vec<u8>,
            /// 按 RETR 次序;用完了按 Full
            pub cuts: Vec<Cut>,
            pub rest_supported: bool,
        }

        pub struct Server {
            pub port: u16,
            /// 收到的全部控制命令(所有连接,按到达序)
            pub log: Arc<Mutex<Vec<String>>>,
        }

        impl Server {
            pub fn cmds(&self, verb: &str) -> Vec<String> {
                self.log
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|l| l.starts_with(verb))
                    .cloned()
                    .collect()
            }
            /// 只留 REST / RETR 两种,看续传时序
            pub fn transfer_seq(&self) -> Vec<String> {
                self.log
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|l| l.starts_with("REST ") || l.starts_with("RETR "))
                    .cloned()
                    .collect()
            }
        }

        struct Shared {
            script: Script,
            retr_count: Mutex<usize>,
        }

        pub async fn start(script: Script) -> Server {
            let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let log = Arc::new(Mutex::new(Vec::new()));
            let shared = Arc::new(Shared { script, retr_count: Mutex::new(0) });
            let log2 = log.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((sock, _)) = listener.accept().await else { break };
                    tokio::spawn(serve(sock, shared.clone(), log2.clone()));
                }
            });
            Server { port, log }
        }

        async fn serve(sock: TcpStream, shared: Arc<Shared>, log: Arc<Mutex<Vec<String>>>) {
            let (rd, mut wr) = sock.into_split();
            let mut rd = BufReader::new(rd);
            wr.write_all(b"220 fake ftp ready\r\n").await.ok();
            let mut pasv: Option<TcpListener> = None;
            let mut rest: usize = 0;
            // Hang 剧本挂住的数据连接:留着不关,控制连接一断(客户端放弃)随本任务一起丢
            let mut held: Vec<TcpStream> = Vec::new();
            let mut line = String::new();
            loop {
                line.clear();
                if rd.read_line(&mut line).await.unwrap_or(0) == 0 {
                    return;
                }
                let cmd = line.trim_end().to_string();
                log.lock().unwrap().push(cmd.clone());
                let (verb, arg) = cmd.split_once(' ').unwrap_or((cmd.as_str(), ""));
                let reply: String = match verb.to_ascii_uppercase().as_str() {
                    "USER" => "331 password please".into(),
                    "PASS" => "230 logged in".into(),
                    "SITE" | "OPTS" => "200 ok".into(),
                    "TYPE" => "200 type set".into(),
                    "SIZE" => format!("213 {}", shared.script.data.len()),
                    "PASV" => {
                        let l = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
                        let p = l.local_addr().unwrap().port();
                        pasv = Some(l);
                        format!("227 Entering Passive Mode (127,0,0,1,{},{})", p >> 8, p & 0xff)
                    }
                    "REST" => {
                        if shared.script.rest_supported {
                            rest = arg.parse().unwrap_or(0);
                            format!("350 Restarting at {rest}")
                        } else {
                            "502 REST not implemented".into()
                        }
                    }
                    "RETR" => {
                        let Some(l) = pasv.take() else {
                            wr.write_all(b"425 Use PASV first\r\n").await.ok();
                            continue;
                        };
                        let Ok(Ok((mut data, _))) =
                            tokio::time::timeout(Duration::from_secs(5), l.accept()).await
                        else {
                            wr.write_all(b"425 Can't open data connection\r\n").await.ok();
                            continue;
                        };
                        wr.write_all(b"150 Opening BINARY connection\r\n").await.ok();
                        let idx = {
                            let mut c = shared.retr_count.lock().unwrap();
                            let i = *c;
                            *c += 1;
                            i
                        };
                        let cut = shared.script.cuts.get(idx).copied().unwrap_or(Cut::Full);
                        let all = &shared.script.data;
                        // REST 只管紧接着的这一次传输(RFC 3659)
                        let start = rest.min(all.len());
                        rest = 0;
                        let end = match cut {
                            Cut::Full => all.len(),
                            Cut::Close(n) | Cut::Hang(n) => (start + n).min(all.len()),
                        };
                        data.write_all(&all[start..end]).await.ok();
                        match cut {
                            Cut::Hang(_) if end < all.len() => {
                                held.push(data); // 不关不回 = 停滞
                                continue;
                            }
                            Cut::Close(_) if end < all.len() => {
                                data.shutdown().await.ok();
                                drop(data);
                                "426 Connection closed; transfer aborted".into()
                            }
                            _ => {
                                data.shutdown().await.ok();
                                drop(data);
                                "226 Transfer complete".into()
                            }
                        }
                    }
                    "QUIT" => {
                        wr.write_all(b"221 Bye\r\n").await.ok();
                        return;
                    }
                    _ => "502 Command not implemented".into(),
                };
                wr.write_all(format!("{reply}\r\n").as_bytes()).await.ok();
            }
        }
    }

    use fake_ftp::{Cut, Script};

    /// 有规律但不周期对齐的样本(错位一个字节就对不上)。
    fn sample(len: usize) -> Vec<u8> {
        (0..len).map(|i| ((i * 7919) % 251) as u8).collect()
    }

    fn target(port: u16) -> FtpTarget {
        FtpTarget {
            host: "127.0.0.1".into(),
            port,
            user: "u".into(),
            pass: "p".into(),
            path: "/示例片名.bin".into(),
            filename: "示例片名.bin".into(),
        }
    }

    fn tmp(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("lw-ftp-{}-{tag}.part", std::process::id()))
    }

    /// 零退避 + 短停滞:只验机器,不真等。
    fn fast_policy() -> ResumePolicy {
        ResumePolicy {
            backoff_base: Duration::ZERO,
            backoff_cap: Duration::ZERO,
            stall_timeout: Duration::from_millis(400),
            ..ResumePolicy::default()
        }
    }

    async fn run(t: &FtpTarget, dest: &Path, expected: Option<u64>) -> Result<u64> {
        tokio::time::timeout(
            Duration::from_secs(30),
            download_with(t, dest, 1 << 30, expected, None, &fast_policy()),
        )
        .await
        .expect("测试卡住了(30s)")
    }

    #[tokio::test]
    async fn resumes_after_server_cuts_data_connection() {
        let data = sample(10_000);
        let srv = fake_ftp::start(Script {
            data: data.clone(),
            cuts: vec![Cut::Close(3000), Cut::Close(2500), Cut::Full],
            rest_supported: true,
        })
        .await;
        let t = target(srv.port);
        // SIZE 探测走同一台假服务器
        assert_eq!(probe_size(&t).await.unwrap(), Some(10_000));

        let dest = tmp("resume-ok");
        let got = run(&t, &dest, Some(10_000)).await.unwrap();
        assert_eq!(got, 10_000);
        assert_eq!(std::fs::read(&dest).unwrap(), data, "续传拼出来的字节必须与原文逐字节一致");
        // 时序:每次重新 RETR 之前必先 REST 到已落盘的字节数(3000,再 3000+2500)
        assert_eq!(
            srv.transfer_seq(),
            [
                "RETR /示例片名.bin",
                "REST 3000",
                "RETR /示例片名.bin",
                "REST 5500",
                "RETR /示例片名.bin",
            ]
        );
        let _ = std::fs::remove_file(&dest);
    }

    #[tokio::test]
    async fn stall_triggers_resume_too() {
        let data = sample(6_000);
        let srv = fake_ftp::start(Script {
            data: data.clone(),
            cuts: vec![Cut::Hang(2000), Cut::Full],
            rest_supported: true,
        })
        .await;
        let t = target(srv.port);
        let dest = tmp("resume-stall");
        let got = run(&t, &dest, Some(6_000)).await.unwrap();
        assert_eq!(got, 6_000);
        assert_eq!(std::fs::read(&dest).unwrap(), data);
        assert_eq!(srv.cmds("REST"), ["REST 2000"], "停滞后从挂住的位置接着传");
        assert_eq!(srv.cmds("RETR").len(), 2);
        let _ = std::fs::remove_file(&dest);
    }

    #[tokio::test]
    async fn server_without_rest_fails_fast_with_plain_words() {
        let data = sample(5_000);
        let srv = fake_ftp::start(Script {
            data,
            cuts: vec![Cut::Close(1000), Cut::Full],
            rest_supported: false,
        })
        .await;
        let t = target(srv.port);
        let dest = tmp("no-rest");
        let err = run(&t, &dest, Some(5_000)).await.unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("不支持断点续传"), "要点名不支持续传: {text}");
        assert!(text.contains("502"), "带上服务器原话方便排查: {text}");
        assert_eq!(srv.cmds("REST"), ["REST 1000"]);
        assert_eq!(srv.cmds("RETR").len(), 1, "REST 被拒后不许再发第二次 RETR(不空转)");
        // 临时件处置与现状一致:download_to 不动它,由调用方清理
        assert!(dest.exists());
        let _ = std::fs::remove_file(&dest);
    }

    #[tokio::test]
    async fn gives_up_after_max_resumes_and_names_the_count() {
        let data = sample(20_000);
        let srv = fake_ftp::start(Script {
            data,
            cuts: vec![Cut::Close(1000); 8], // 掐够 6 次(首连 + 5 次续传全掐)
            rest_supported: true,
        })
        .await;
        let t = target(srv.port);
        let dest = tmp("give-up");
        let err = run(&t, &dest, Some(20_000)).await.unwrap_err();
        let text = format!("{err:#}");
        assert!(
            text.contains(&format!("续传了 {RESUME_MAX_ATTEMPTS} 次仍没下完")),
            "错误话术要点名续传次数: {text}"
        );
        assert_eq!(
            srv.cmds("RETR").len(),
            1 + RESUME_MAX_ATTEMPTS as usize,
            "首连 + 最多 {RESUME_MAX_ATTEMPTS} 次续传,之后不再重试"
        );
        assert_eq!(
            srv.cmds("REST"),
            ["REST 1000", "REST 2000", "REST 3000", "REST 4000", "REST 5000"],
            "每次都从真实落盘长度接着要"
        );
        // 临时件处置与现状一致:留给调用方清理;长度 = 六轮各追加 1000 字节
        assert_eq!(std::fs::metadata(&dest).unwrap().len(), 6_000);
        let _ = std::fs::remove_file(&dest);
    }
}
