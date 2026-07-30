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

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use suppaftp::tokio::AsyncFtpStream;
use suppaftp::types::FileType;

/// 建连 + 登录的总超时。**死链是这里最常见的情形**,要快点失败并给明白话,
/// 别让用户对着转圈等。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// 取体积(SIZE)的超时:连上了就该很快。
const CMD_TIMEOUT: Duration = Duration::from_secs(15);
/// 传输中多久没有新字节判失败(FTP 被动模式的数据连接被中间设备掐掉是常见死法)。
const STALL_TIMEOUT: Duration = Duration::from_secs(90);

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

/// 下载到 `dest`(调用方负责临时件与改名)。`cap` = 体积硬闸;
/// `progress` = 后台档才传 `(票据, 预期总字节)`,每 ~1MB 打点并查取消。
pub async fn download_to(
    t: &FtpTarget,
    dest: &Path,
    cap: u64,
    progress: Option<(&crate::bgtasks::BgTicket, u64)>,
) -> Result<u64> {
    use std::io::Write;
    use tokio::io::AsyncReadExt;

    let mut ftp = open(t).await?;
    let mut stream = tokio::time::timeout(CMD_TIMEOUT, ftp.retr_as_stream(&t.path))
        .await
        .map_err(|_| anyhow::anyhow!("请求文件超时"))?
        .with_context(|| {
            format!(
                "服务器上找不到这个文件({})。两种可能:① 链接里的文件名已经变了/被删了;\
                 ② **文件名是中文、而这台服务器用 GBK 编码存文件名** —— 这种我们当前下不了\
                 (只发 UTF-8 文件名),不是链接死了。",
                t.path
            )
        })?;

    let mut f = std::fs::File::create(dest)
        .with_context(|| format!("建不了文件 {}", dest.display()))?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut total: u64 = 0;
    let mut next_beat: u64 = 0;
    loop {
        let n = tokio::time::timeout(STALL_TIMEOUT, stream.read(&mut buf))
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "下了 {} 之后 {} 秒没有新数据,停了。ftp 的数据连接被中间设备掐掉是常见死法,\
                     可以再试一次。",
                    crate::files::human_size(total),
                    STALL_TIMEOUT.as_secs()
                )
            })?
            .context("ftp 传输中断")?;
        if n == 0 {
            break;
        }
        total += n as u64;
        anyhow::ensure!(
            total <= cap,
            "文件超过 {} 上限,已停止",
            crate::files::human_size(cap)
        );
        f.write_all(&buf[..n])?;
        if let Some((ticket, expect)) = progress {
            if ticket.is_cancelled() {
                anyhow::bail!("按要求停下了");
            }
            if total >= next_beat {
                next_beat = total + 1024 * 1024;
                let pct = total.saturating_mul(100).checked_div(expect).unwrap_or(0);
                ticket.beat(
                    pct as usize,
                    format!(
                        "{} / {}",
                        crate::files::human_size(total),
                        crate::files::human_size(expect)
                    ),
                );
            }
        }
    }
    f.flush()?;
    // 收尾必须做:不 finalize 服务器不会回最终响应(suppaftp 明示)
    let _ = ftp.finalize_retr_stream(stream).await;
    let _ = ftp.quit().await;
    Ok(total)
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
}
