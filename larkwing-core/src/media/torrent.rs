//! 能力轴:影音(下·BT)。把用户给的磁力链 / `.torrent` 文件下载成本地文件。
//!
//! **与 `media_download` 正交**:那个解析网页拿音轨流,这个走 BT 网络拿整个文件。
//! 下完落进文件夹 → `media_play` 那条本地播放链(按需 HLS 转码 / 音轨切换 / 整夹连播 /
//! 续播记忆 / `fs_find` 本地优先命中)**全部白拿,零改**。
//!
//! **链接由用户提供,我们不去任何站找**(§7.1 版权口径「不接野源」的兑现方式 = 不写死
//! 任何站的选择器、不内置片源搜索、不维护片源目录;定位同 `web_download` —— 用户给什么
//! 就下什么,我们是下载器不是内容源)。
//!
//! **做种策略(§4.11,2026-07-29 用户拍板「A」)**:下载期正常上传,**下完立即停止做种**。
//! 不是可以随便调的旋钮 —— BT 的 choke/unchoke 是 tit-for-tat 互惠:peer 只把带宽给
//! 「也在给自己传」的那几个,所以下载期不传 = 下载速度显著下降甚至下不动(librqbit 的
//! `disable-upload` feature 与 per-torrent `ratelimits.upload_bps` 都在,要改回来很容易,
//! 但要连带接受速度代价)。下完 `Session::pause` 停止分发,不做长期种。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use librqbit::{AddTorrent, AddTorrentOptions, AddTorrentResponse, ManagedTorrent, Session, SessionOptions};

use crate::bgtasks::BgTicket;
use crate::bus::Text;
use crate::files::human_size;
use crate::tasks::Tasks;

/// 单个种子的体积上限(§4.11 用户拍板):`media_download` 的 500MB 是「一首歌」口径,
/// 影视量级完全不同,单独一个数。超了如实退回,不偷偷下半个盘。
pub const TORRENT_MAX_BYTES: u64 = 50 * 1024 * 1024 * 1024; // 50 GB

/// 同时下几个种子:再多带宽只是被分摊,还挤占 peer 连接数。
pub const MAX_CONCURRENT: usize = 3;

/// 磁力链从 DHT 取 metadata(种子信息)的超时。
/// **这是国内最容易卡死的一步** —— DHT 走 UDP,而运营商对 UDP 的限速正是冲着 P2P 来的;
/// `.torrent` 文件内嵌 metadata、不走这步,所以工具描述引导「有 .torrent 就用它,更稳」。
pub const METADATA_TIMEOUT: Duration = Duration::from_secs(60);

/// 一直没有新字节进来多久判失败(0 seeder / 被墙 / 防火墙没放行都是这个形态)。
/// 比 bgtasks 的 10min 卡死看门狗更早、且能给出**针对 BT 的**解释。
pub const NO_PROGRESS_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// 进度轮询间隔。
const POLL: Duration = Duration::from_secs(2);

/// 缺省只下视频文件 —— 影视种子里常年混着样片(sample)、nfo、字幕说明、
/// 「最新地址.txt」这类广告垃圾。想全下让模型传 `only="."`(匹配一切)。
pub const DEFAULT_ONLY_RE: &str = r"(?i)\.(mkv|mp4|avi|mov|m4v|ts|m2ts|wmv|flv|webm|rmvb|iso)$";

/// 一次 BT 下载的产出。
#[derive(Debug, Clone)]
pub struct TorrentDone {
    /// 种子名(多文件种子 = 文件夹名)。
    pub name: String,
    /// 落盘目录(多文件种子 = `dir/<name>/`)。
    pub dir: PathBuf,
    pub bytes: u64,
    /// 实际下下来的文件相对名(已按 `only` 过滤)。
    pub files: Vec<String>,
}

/// BT 引擎。**懒建**(见 `MediaRuntime.inner.torrent` 的 `OnceCell`)——不用 BT 的用户
/// 零成本,更重要的是**不会平白发 DHT 包**(那是会被运营商画像的流量特征)。
pub struct TorrentEngine {
    session: Arc<Session>,
}

impl TorrentEngine {
    /// `state_dir` 只放 session 自己的状态(DHT 路由表等),**不是下载落盘处**
    /// (落盘每次由 `output_folder` 显式指定)。随数据根搬家(§6.2)。
    pub async fn new(state_dir: PathBuf) -> Result<TorrentEngine> {
        std::fs::create_dir_all(&state_dir)
            .with_context(|| format!("建不了 BT 状态目录 {}", state_dir.display()))?;
        let opts = SessionOptions {
            // 不持久化 session:重启不自动续跑上次的种子(§6.4「入槽资格 = 派生的、可丢的」;
            // 也避免「关了 app 还在偷偷做种」这种用户预期外的行为)。
            persistence: None,
            disable_dht_persistence: true,
            ..Default::default()
        };
        let session = Session::new_with_opts(state_dir, opts)
            .await
            .context("BT 引擎启动失败")?;
        Ok(TorrentEngine { session })
    }

    /// 下载一个种子到 `out_dir`,阻塞到下完/失败/被取消。进度经 `ticket`(模型可见)
    /// 与 `task`(HUD 任务条)双路上报。
    pub async fn download(
        &self,
        link: &TorrentLink,
        out_dir: &Path,
        only: Option<String>,
        ticket: &BgTicket,
        tasks: &Tasks,
    ) -> Result<TorrentDone> {
        std::fs::create_dir_all(out_dir)
            .with_context(|| format!("建不了下载目录 {}", out_dir.display()))?;

        let add = match link {
            TorrentLink::Magnet(m) => AddTorrent::from_url(m.as_str()),
            TorrentLink::File(bytes) => AddTorrent::from_bytes(bytes.clone()),
        };
        let opts = AddTorrentOptions {
            only_files_regex: only,
            output_folder: Some(out_dir.to_string_lossy().into_owned()),
            // 永不覆盖(§7.2 可逆三规①)。撞上同名 = 如实退回,不静默改写用户已有的文件。
            overwrite: false,
            ..Default::default()
        };

        let handle = match self.session.add_torrent(add, Some(opts)).await? {
            AddTorrentResponse::Added(_, h) => h,
            AddTorrentResponse::AlreadyManaged(_, h) => h,
            AddTorrentResponse::ListOnly(_) => {
                anyhow::bail!("这个种子只列出了文件、没能开始下载")
            }
        };

        // 1) 等 metadata（磁力链要从 DHT 取；.torrent 文件是现成的，这步瞬间过）
        let task = tasks.start("torrent", Text::new("task.torrent"));
        task.step_progress("step.torrent_meta", serde_json::Value::Null, 0.0);
        if tokio::time::timeout(METADATA_TIMEOUT, handle.wait_until_initialized())
            .await
            .is_err()
        {
            self.discard(&handle).await;
            task.fail("task.err.torrent_meta", serde_json::Value::Null);
            anyhow::bail!(
                "{}秒内没能拿到这个种子的信息。磁力链要先从 DHT 网络取种子信息,而 DHT 走 UDP、\
                 国内运营商常对它限速 —— 如果能拿到 .torrent 文件,用文件比磁力链稳得多\
                 (信息内嵌、不用查 DHT)。也可能这个种子已经没人做种了。",
                METADATA_TIMEOUT.as_secs()
            );
        }

        // 2) 算「真正要下的」体积与文件清单。
        // ⚠️ **不能直接用 `stats().total_bytes` / `file_infos`** —— 两者都是**整个种子**的
        // (`total_bytes` = `lengths.total_length()`,`file_infos` 由完整 metadata 构造,
        // 都不按 `only_files` 过滤)。缺省只下视频、种子里塞着样片/nfo 时直接用会三处错:
        // ① 按整种子体积误拒(只想要其中一集也被 50GB 闸拦下);② 进度分母偏大 ——
        // `progress_bytes` 只涨选中部分,百分比**永远到不了 100%**;③ 报给用户的文件数
        // 是种子里的总数,不是实际下的那几个。故按 `only_files` 自己过滤一遍。
        let selected = handle.only_files(); // 先取,别在 with_metadata 闭包里再拿锁
        let (total, files) = handle
            .with_metadata(|m| {
                let mut bytes = 0u64;
                let mut names = Vec::new();
                for (i, f) in m.file_infos.iter().enumerate() {
                    // 显式 match 而非 `is_none_or`:后者 Rust 1.82 才稳定,本项目 MSRV 1.77.2
                    let want = match selected.as_ref() {
                        Some(s) => s.contains(&i),
                        None => true, // 没设过滤 = 整个种子都要
                    };
                    if want {
                        bytes += f.len;
                        names.push(f.relative_filename.to_string_lossy().into_owned());
                    }
                }
                (bytes, names)
            })
            .unwrap_or((0, Vec::new()));
        anyhow::ensure!(
            !files.is_empty(),
            "这个种子里没有匹配的文件可下(缺省只下视频文件;要全下传 only=\".\")"
        );
        if total > TORRENT_MAX_BYTES {
            self.discard(&handle).await;
            task.fail("task.err.torrent_size", serde_json::Value::Null);
            anyhow::bail!(
                "要下的部分有 {},超过单个种子 {} 的上限,没有下。\
                 (要么换个小点的版本,要么用 only 参数只挑其中某个文件)",
                human_size(total),
                human_size(TORRENT_MAX_BYTES)
            );
        }
        let name = handle.name().unwrap_or_else(|| "(未命名)".to_string());
        tracing::info!(name = %name, bytes = total, files = files.len(), "BT:开始下载");

        // 3) 轮询进度到完成
        let mut last_bytes = 0u64;
        let mut last_move = Instant::now();
        loop {
            if ticket.is_cancelled() {
                self.discard(&handle).await;
                task.fail("task.err.cancelled", serde_json::Value::Null);
                anyhow::bail!("按要求停下了(已下的部分留在 {} 没删)", out_dir.display());
            }
            let st = handle.stats();
            if st.finished {
                break;
            }
            let got = st.progress_bytes;
            if got > last_bytes {
                last_bytes = got;
                last_move = Instant::now();
            } else if last_move.elapsed() > NO_PROGRESS_TIMEOUT {
                self.discard(&handle).await;
                task.fail("task.err.torrent_stall", serde_json::Value::Null);
                anyhow::bail!(
                    "下了 {} 之后 {} 分钟一个字节都没进来,停了。常见原因:这个种子没人做种了、\
                     防火墙没放行(Windows 首次要点「允许访问」)、或者运营商把 BT 流量掐了。",
                    human_size(got),
                    NO_PROGRESS_TIMEOUT.as_secs() / 60
                );
            }
            let pct = got.saturating_mul(100).checked_div(total).unwrap_or(0) as usize;
            let (dn, peers) = match st.live.as_ref() {
                Some(l) => (l.download_speed.to_string(), l.snapshot.peer_stats.live),
                None => ("连接中".to_string(), 0),
            };
            ticket.beat(pct, format!("《{name}》 {dn} · {peers} 个来源"));
            task.step_progress(
                "step.torrent",
                serde_json::json!({ "n": name, "pct": pct, "sp": dn, "pe": peers }),
                pct as f32 / 100.0,
            );
            tokio::time::sleep(POLL).await;
        }

        // 4) 下完 → 立刻停止做种(拍板项 ① 的兑现点)
        if let Err(e) = self.session.pause(&handle).await {
            tracing::warn!("BT:下完停做种失败(不影响文件): {e:#}");
        }
        task.done();
        tracing::info!(name = %name, "BT:下载完成,已停止做种");
        // 落点:多文件种子 librqbit 建同名子文件夹,单文件种子直接落 out_dir
        // → `out_dir.join(name)` 两种形都对(前者=文件夹、后者=文件本身),
        // 而 media_play 两种都吃(is_dir_path / is_local_path 分派)。
        Ok(TorrentDone { name: name.clone(), dir: out_dir.join(&name), bytes: total, files })
    }

    /// 放弃一个种子:停 + 从 session 摘掉。**不删已下的字节**(用户的东西不替他删,
    /// 要清理走 fs_trash;半成品留着下次还能续)。
    async fn discard(&self, handle: &Arc<ManagedTorrent>) {
        let _ = self.session.pause(handle).await;
        if let Err(e) = self.session.delete(handle.id().into(), false).await {
            tracing::warn!("BT:摘除种子失败: {e:#}");
        }
    }
}

/// `torrent_download` 的回执。BT 慢且不可预测 → **恒走后台 job**(同 `download_all`):
/// 工具秒回,进度进登记处,收尾自动唤回合汇报。
#[derive(Debug, Clone)]
pub enum TorrentOutcome {
    Started { label: String, dir: PathBuf },
}

impl super::MediaRuntime {
    /// BT 引擎懒建。状态目录挂数据根(§6.2 随搬家走)。
    async fn torrent_engine(&self) -> Result<&TorrentEngine> {
        self.inner
            .torrent
            .get_or_try_init(|| TorrentEngine::new(self.inner.dir.join("torrent")))
            .await
    }

    /// 下一个种子。**立即返回**,活在后台跑(BT 动辄几十分钟,绝不占着回合)。
    pub async fn torrent_download(
        &self,
        link: TorrentLink,
        dir: &Path,
        only: Option<String>,
        origin: (i64, i64),
    ) -> Result<TorrentOutcome> {
        // 并发闸:再多带宽只是被分摊。满了如实退回(bgtasks 满了也是这个口径,不排队)。
        let running = self.inner.bg.running_count_of("下载种子");
        anyhow::ensure!(
            running < MAX_CONCURRENT,
            "已经有 {running} 个种子在下了(一次最多 {MAX_CONCURRENT} 个,再多只是分摊带宽)。\
             等一个下完,或者用 task_cancel 停掉一个再来。"
        );
        // 引擎在这里就建好 —— 起不来要让模型当场知道,别等到后台 job 里才炸。
        self.torrent_engine().await?;

        let label = link.label();
        let ticket = self.inner.bg.submit(format!("下载种子({label})"), origin, 100)?;
        let ticket_id = ticket.id();
        let this = self.clone();
        let dir_owned = dir.to_path_buf();
        let label_owned = label.clone();
        let join = tokio::spawn(async move {
            let tasks = this.inner.tasks.clone();
            // engine 上面已经 init 过,这里必然命中缓存。
            // 成败**由 Result 决定**,不靠在文案里找关键字 —— 那样改一次措辞就静默坏掉。
            let outcome: Result<_> = async {
                let eng = this.torrent_engine().await.context("BT 引擎起不来")?;
                eng.download(&link, &dir_owned, only, &ticket, &tasks).await
            }
            .await;
            let (ok, report) = match outcome {
                Ok(done) => {
                    tracing::info!(name = %done.name, "BT:job 完成");
                    (
                        true,
                        format!(
                            "种子下好了:《{}》({},{} 个文件)存到 {}。已经停止做种。\
                             要看的话直接用 media_play 放这个路径就行。把结果简短告诉用户。",
                            done.name,
                            human_size(done.bytes),
                            done.files.len(),
                            done.dir.display()
                        ),
                    )
                }
                Err(e) => {
                    tracing::warn!("BT:job 失败: {e:#}");
                    (false, format!("种子({label_owned})没下成:{e:#}。把原因如实告诉用户。"))
                }
            };
            ticket.finish(ok, report);
        });
        self.inner.bg.attach_abort(ticket_id, join.abort_handle());
        Ok(TorrentOutcome::Started { label, dir: dir.to_path_buf() })
    }
}

/// 用户给的东西:磁力链,或一个 `.torrent` 文件的内容。
#[derive(Debug, Clone)]
pub enum TorrentLink {
    Magnet(String),
    File(Vec<u8>),
}

impl TorrentLink {
    /// 解析工具入参。`.torrent` **在这里就读进内存**(工具层同步失败比后台 job 里失败
    /// 对模型友好得多),磁力链只做形状校验。
    pub fn parse(raw: &str) -> Result<TorrentLink> {
        let s = raw.trim();
        anyhow::ensure!(!s.is_empty(), "link 不能为空");
        if s.starts_with("magnet:") {
            // 只查 contains 不够:哈希段可能是任意 UTF-8(模型/用户手抄出错),下游 label()
            // 按字节切片就会 panic 在非 char 边界。这里把形状校严 —— BTv1 infohash 恒为
            // 40 位十六进制(或 32 位 base32),非 ASCII 一律当畸形退回。
            let hash = magnet_infohash(s)
                .ok_or_else(|| anyhow::anyhow!("这不是一个能用的磁力链(缺 btih 信息哈希),收到: {s}"))?;
            anyhow::ensure!(
                (hash.len() == 40 && hash.chars().all(|c| c.is_ascii_hexdigit()))
                    || (hash.len() == 32 && hash.chars().all(|c| c.is_ascii_alphanumeric())),
                "磁力链里的信息哈希不像真的(应是 40 位十六进制),收到: {hash}"
            );
            return Ok(TorrentLink::Magnet(s.to_string()));
        }
        if s.starts_with("http://") || s.starts_with("https://") {
            anyhow::bail!(
                "这是个网页/直链,不是磁力链。要下网页上的文件用 web_download;\
                 如果这个地址指向 .torrent 文件,先用 web_download 存到本地,再把本地路径给我。"
            );
        }
        // 剩下的当本地 .torrent 文件路径
        let p = PathBuf::from(crate::tools::expand_home(s));
        anyhow::ensure!(
            p.is_absolute(),
            ".torrent 需要绝对路径(或给磁力链 magnet:...),收到: {s}"
        );
        anyhow::ensure!(p.is_file(), "找不到这个 .torrent 文件:{}", p.display());
        let bytes = std::fs::read(&p).with_context(|| format!("读不了 {}", p.display()))?;
        anyhow::ensure!(!bytes.is_empty(), ".torrent 文件是空的:{}", p.display());
        Ok(TorrentLink::File(bytes))
    }

    /// 给用户看的简短标识(种子名要等 metadata,这个是立刻能说的)。
    pub fn label(&self) -> String {
        match self {
            // 按 **字符** 取前 8 个,不按字节切(字节切片在非 char 边界会 panic;
            // parse 已把哈希校成 ASCII,这里再稳一道 —— label 也可能被别处调用)。
            TorrentLink::Magnet(m) => magnet_infohash(m)
                .map(|h| format!("磁力链 {}…", h.chars().take(8).collect::<String>()))
                .unwrap_or_else(|| "磁力链".to_string()),
            TorrentLink::File(_) => "种子文件".to_string(),
        }
    }
}

/// 抽磁力链里 `xt=urn:btih:` 后面那段(到下一个 `&` 为止)。取不到 = None。
fn magnet_infohash(magnet: &str) -> Option<&str> {
    magnet
        .split("xt=urn:btih:")
        .nth(1)
        .and_then(|s| s.split('&').next())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_magnet_with_btih() {
        let m = "magnet:?xt=urn:btih:abcdef0123456789abcdef0123456789abcdef01&dn=x";
        assert!(matches!(TorrentLink::parse(m).unwrap(), TorrentLink::Magnet(_)));
    }

    /// 凭证的 Debug 必须遮住密码(派生 Debug 会把明文吐进日志/错误链)。
    #[test]
    fn http_cred_debug_hides_password() {
        let c = crate::web::HttpCred {
            host: "dav.example.com".into(),
            user: "me".into(),
            password: "s3cr3t-do-not-log".into(),
        };
        let shown = format!("{c:?}");
        assert!(!shown.contains("s3cr3t"), "密码不许出现在 Debug 里: {shown}");
        assert!(shown.contains("dav.example.com"), "host 该留着好排查: {shown}");
    }

    #[test]
    fn parse_rejects_magnet_without_infohash() {
        let e = TorrentLink::parse("magnet:?dn=x").unwrap_err().to_string();
        assert!(e.contains("btih"), "要点明缺 btih: {e}");
    }

    #[test]
    fn parse_points_http_at_web_download() {
        let e = TorrentLink::parse("https://example.com/a.torrent").unwrap_err().to_string();
        assert!(e.contains("web_download"), "http 链接要指路 web_download: {e}");
    }

    #[test]
    fn parse_rejects_relative_path() {
        let e = TorrentLink::parse("some/rel.torrent").unwrap_err().to_string();
        assert!(e.contains("绝对路径"), "相对路径要退回: {e}");
    }

    #[test]
    fn parse_reads_local_torrent_file() {
        let dir = std::env::temp_dir().join(format!("lw-tor-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("x.torrent");
        std::fs::write(&f, b"d8:announce4:teste").unwrap();
        let got = TorrentLink::parse(f.to_str().unwrap()).unwrap();
        assert!(matches!(got, TorrentLink::File(b) if !b.is_empty()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn label_shows_short_infohash() {
        // 32 位 base32 形(BTv1 的另一种写法)也认
        let m = "magnet:?xt=urn:btih:ABCDEF0123456789ABCDEF0123456789&dn=x";
        assert_eq!(TorrentLink::parse(m).unwrap().label(), "磁力链 ABCDEF01…");
    }

    /// 回归:哈希段是多字节字符时,label 曾按**字节**切片 → 非 char 边界 panic。
    /// 现在 parse 就该把它当畸形退回;label 自己也改成按字符取。
    #[test]
    fn multibyte_infohash_is_rejected_not_panicking() {
        let e = TorrentLink::parse("magnet:?xt=urn:btih:中文中文中文&dn=x")
            .unwrap_err()
            .to_string();
        assert!(e.contains("信息哈希"), "该当畸形退回: {e}");
        // label 直接喂多字节也不许 panic(它可能被别处调用)
        let l = TorrentLink::Magnet("magnet:?xt=urn:btih:中文中文中文".into()).label();
        assert!(l.starts_with("磁力链"), "label 要能安全截断: {l}");
    }

    #[test]
    fn short_or_nonhex_infohash_is_rejected() {
        for bad in [
            "magnet:?xt=urn:btih:deadbeef",                                  // 太短
            "magnet:?xt=urn:btih:zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz",  // 40 位但非 hex
        ] {
            assert!(TorrentLink::parse(bad).is_err(), "该退回: {bad}");
        }
    }

    #[test]
    fn default_only_re_keeps_video_drops_junk() {
        let re = regex_lite_match;
        assert!(re("Movie.2026.1080p.mkv"), "视频要留");
        assert!(re("a/b/film.mp4"), "子目录视频要留");
        assert!(!re("sample.txt"), "广告 txt 要滤");
        assert!(!re("poster.jpg"), "海报要滤");
        assert!(!re("movie.nfo"), "nfo 要滤");
    }

    /// 只验我们那条缺省正则的意图(librqbit 内部用 regex 匹配相对路径)。
    fn regex_lite_match(name: &str) -> bool {
        let lower = name.to_ascii_lowercase();
        ["mkv", "mp4", "avi", "mov", "m4v", "ts", "m2ts", "wmv", "flv", "webm", "rmvb", "iso"]
            .iter()
            .any(|e| lower.ends_with(&format!(".{e}")))
    }
}
