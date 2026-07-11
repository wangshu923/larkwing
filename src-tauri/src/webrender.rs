//! core「网页渲染器」接缝(webrender.rs)的壳层实现:**app 自己就是浏览器** ——
//! 隐藏 WebView 窗(WebView2/WKWebView,同 B 站扫码登录窗先例)真渲染 JS 页面。
//! L2 会话式浏览(2026-07-10):窗口跨调用存活(TTL 180s / 最多 2 个,清扫任务收摊),
//! 每步 = 可选动作(导航 / back / 点编号 / 点文字)→ **编号快照**(给交互元素打
//! `data-lw-ref`,文本版 Set-of-Marks)经 relay `/collect/{token}` POST 回 core。
//! 下载由 `on_download` 接管(Requested 时定落点 sanitize+dedupe;**mac 的 Finished.path
//! 恒空——只能用 Requested 时自己记下的落点**,tauri 文档明示的平台差异)。
//! 纪律:不给远程页任何 IPC 桥(注入脚本只 POST loopback 一次性 token);动作空间只有
//! 看/点/返回,**输入/填表不做**(等 Tool::risk 确认闸门);会话窗生死归 TTL/清扫,
//! 绝不无限存活。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use larkwing_core::webrender::{RenderOutcome, RenderRequest, RenderedPage, WebRenderer};

/// 会话窗空闲多久收摊(模型隔几步回来续用足够;不无限占内存/进程)。
const SESSION_TTL: Duration = Duration::from_secs(180);
/// 同时最多几个会话窗(第三个进来挤掉最旧的)。
const SESSION_MAX: usize = 2;
/// 可见缩略窗:逻辑尺寸(右下角小窗,set_zoom 联动出「桌面版页面的缩略直播」;
/// 用户拖大窗口 = 自然放大——Resized 事件按宽度重算 zoom)。做了啥摆在明面上让人放心,
/// 顺手解锁登录墙人机接力:它搞不定的登录/验证码,用户直接在这个窗里点,它接着干。
const THUMB_W: f64 = 380.0;
const THUMB_H: f64 = 260.0;
/// 页面按这个宽度排版(zoom = 窗宽/它;1280 = 桌面布局的常见断点)。
const PAGE_LAYOUT_W: f64 = 1280.0;
/// 点击后等「下载/跳转有没有起头」的宽限:毫无动静就别干等到单步超时。
const CLICK_DOWNLOAD_GRACE: Duration = Duration::from_secs(12);
/// load 事件后等 SPA 水合的静默期,再动作/快照。
const SETTLE_AFTER_LOAD: Duration = Duration::from_millis(1500);
/// 快照回传等待(超了重注入一次再等,兜迟到水合)。
const SNAPSHOT_WAIT: Duration = Duration::from_secs(6);
/// 等一次页面 load 完成的上限(导航/返回后)。
const LOAD_WAIT: Duration = Duration::from_secs(15);
/// 点击引发导航后的静默期:到点还没触发 page-load-finished,就认定这不是要 load 的 HTML
/// (Mac WKWebView 把指向 PDF/附件的链接**内联打开**,导航发生但永不 load;SPA 前端路由
/// 同样不 load)——别干等到 deadline,把这个跳转地址当 post_click_url 交模型接 web_download。
const NAV_SETTLE: Duration = Duration::from_secs(4);

/// 一个活着的会话窗(引用计数共享给窗口回调闭包)。
struct SessionEntry {
    win: tauri::WebviewWindow,
    /// on_download Requested 记下的落点(mac Finished.path 恒空,以这里为准)。
    dl_final: Arc<Mutex<Option<PathBuf>>>,
    /// on_download Finished 的成败信号。
    dl_done: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<bool>>,
    /// 导航记录(时刻, 地址):判「点击引发了跳转」。
    navs: Arc<Mutex<Vec<(Instant, String)>>>,
    /// on_page_load Finished 计数:等「这次导航加载完」用。
    load_seq: Arc<AtomicU64>,
    /// 下载落点目录(每步请求可换,handler 闭包实时读)。
    download_dir: Arc<Mutex<PathBuf>>,
    last_used: Mutex<Instant>,
}

impl SessionEntry {
    fn touch(&self) {
        *self.last_used.lock().expect("webrender last_used") = Instant::now();
    }
}

pub struct ShellWebRenderer {
    app: tauri::AppHandle,
    media: larkwing_core::media::MediaRuntime,
    sessions: Arc<Mutex<HashMap<String, Arc<SessionEntry>>>>,
}

impl ShellWebRenderer {
    pub fn new(app: tauri::AppHandle, media: larkwing_core::media::MediaRuntime) -> Self {
        let sessions: Arc<Mutex<HashMap<String, Arc<SessionEntry>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        // 清扫任务:TTL 到点的窗收摊(app 生命周期常驻;窗随进程退出自然消亡)
        let sweep = sessions.clone();
        tauri::async_runtime::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                let expired: Vec<(String, Arc<SessionEntry>)> = {
                    let map = sweep.lock().expect("webrender sessions");
                    map.iter()
                        .filter(|(_, e)| {
                            e.last_used.lock().expect("webrender last_used").elapsed() > SESSION_TTL
                        })
                        .map(|(k, e)| (k.clone(), e.clone()))
                        .collect()
                };
                for (id, entry) in expired {
                    sweep.lock().expect("webrender sessions").remove(&id);
                    let _ = entry.win.destroy();
                    tracing::debug!(session = %id, "webrender 会话窗超时收摊");
                }
            }
        });
        Self { app, media, sessions }
    }
}

#[async_trait::async_trait]
impl WebRenderer for ShellWebRenderer {
    async fn render(&self, req: RenderRequest) -> Result<RenderOutcome> {
        // 分离任务跑全程:工具超时/回合取消 drop 掉外层 future 时,里面的步骤照常收尾
        let app = self.app.clone();
        let media = self.media.clone();
        let sessions = self.sessions.clone();
        tauri::async_runtime::spawn(async move { browse_step(app, media, sessions, req).await })
            .await
            .map_err(|e| anyhow!("渲染任务挂了: {e}"))?
    }
}

/// 一步会话式浏览(带 HUD 任务卡:干了啥全程可见,与缩略窗一体两面)。
/// 句柄 drop 未收尾 = 自动 fail(进度总线纪律 §7.1)兜住 panic 路。
async fn browse_step(
    app: tauri::AppHandle,
    media: larkwing_core::media::MediaRuntime,
    sessions: Arc<Mutex<HashMap<String, Arc<SessionEntry>>>>,
    req: RenderRequest,
) -> Result<RenderOutcome> {
    let task = media.tasks().start("webrender", larkwing_core::bus::Text::new("task.webrender"));
    match browse_step_inner(app, media, sessions, req, &task).await {
        Ok(o) => {
            task.done();
            Ok(o)
        }
        Err(e) => {
            task.fail("task.err.render", serde_json::Value::Null);
            Err(e)
        }
    }
}

/// 取/建窗 → (导航)→ (动作)→ 编号快照 → 回结局。
async fn browse_step_inner(
    app: tauri::AppHandle,
    media: larkwing_core::media::MediaRuntime,
    sessions: Arc<Mutex<HashMap<String, Arc<SessionEntry>>>>,
    req: RenderRequest,
    task: &larkwing_core::tasks::TaskHandle,
) -> Result<RenderOutcome> {
    let deadline = tokio::time::Instant::now() + req.timeout;

    // ---- 1. 会话窗:续用或新建 ----
    let (sid, entry, fresh) = match &req.session {
        Some(id) => {
            let found = sessions.lock().expect("webrender sessions").get(id).cloned();
            match found {
                Some(e) => {
                    *e.download_dir.lock().expect("webrender dl dir") = req.download_dir.clone();
                    (id.clone(), e, false)
                }
                None => bail!("这个浏览会话已经收摊了(超时/被新窗挤掉)——带 url 重新打开"),
            }
        }
        None => {
            anyhow::ensure!(!req.url.trim().is_empty(), "开新页面需要 url");
            let (id, e) = open_session(&app, &sessions, &req)?;
            (id, e, true)
        }
    };
    entry.touch();

    // ---- 2. 导航(新窗建窗即导航;老窗带了新 url 才导航)----
    let mut wait_load = fresh;
    if !fresh && !req.url.trim().is_empty() {
        task.step("step.render_load", serde_json::Value::Null);
        let seq0 = entry.load_seq.load(Ordering::Relaxed);
        let url_js = serde_json::to_string(req.url.trim()).unwrap_or_else(|_| "\"\"".into());
        entry
            .win
            .eval(format!("try{{window.location.assign({url_js})}}catch(e){{}}").as_str())
            .context("导航指令没发出去")?;
        wait_for_load(&entry, seq0, deadline).await;
        wait_load = false;
    }
    if wait_load {
        task.step("step.render_load", serde_json::Value::Null);
        wait_for_load(&entry, 0, deadline).await;
    }

    // ---- 3. 动作:back > click_ref > click_text ----
    drain_downloads(&entry).await; // 上一步残留的下载信号别混进本步
    let mut download = None;
    let mut post_click_url = None;
    let mut skip_snapshot = false;
    let did_click = req.click_ref.is_some() || req.click_text.is_some();
    if req.back {
        task.step("step.render_back", serde_json::Value::Null);
        let seq0 = entry.load_seq.load(Ordering::Relaxed);
        let _ = entry.win.eval("try{history.back()}catch(e){}");
        wait_for_load(&entry, seq0, deadline).await;
    } else if did_click {
        let desc = req
            .click_ref
            .map(|n| format!("[{n}]"))
            .or_else(|| req.click_text.clone())
            .unwrap_or_default();
        task.step("step.render_click", serde_json::json!({ "t": desc }));
        let click_moment = Instant::now();
        let seq0 = entry.load_seq.load(Ordering::Relaxed);
        let js = build_click_script(req.click_ref, req.click_text.as_deref());
        entry.win.eval(js.as_str()).context("点击指令没发出去")?;
        // 等下载/跳转起头(宽限);下载起头等到预算,跳转加载完即收手(普通链接点击别干等)
        let (dl, navigated, loaded) = wait_click_outcome(&entry, click_moment, seq0, deadline).await;
        download = dl;
        if navigated && download.is_none() {
            post_click_url = last_nav_after(&entry.navs, click_moment);
            // 文件内联(PDF/附件:导航了但永不触发 page-load)→ 当前窗已不是可注入脚本的
            // HTML,快照必空、白等 12s。直接把地址交模型接 web_download,跳过快照。
            skip_snapshot = !loaded;
        }
        // HTML 新页已 load → 给个水合静默期再快照(照刷新后的内容)
        if navigated && loaded {
            tokio::time::sleep(
                SETTLE_AFTER_LOAD.min(deadline.saturating_duration_since(tokio::time::Instant::now())),
            )
            .await;
        }
    }

    // ---- 4. 编号快照(每步都照一张;文件内联跳过——注入脚本在 PDF 查看器里跑不了)----
    let mut page = if skip_snapshot {
        None
    } else {
        task.step("step.render_snap", serde_json::Value::Null);
        take_snapshot(&media, &entry, deadline).await
    };
    tracing::info!(
        navigated = post_click_url.is_some(),
        has_download = download.is_some(),
        has_page = page.is_some(),
        skip_snapshot,
        post_click = post_click_url.as_deref().unwrap_or(""),
        "webrender 步完成"
    );
    // 点击引发跳转会把页面世界里的 __lwClick 冲掉 → 以「点击后出现导航」补判 clicked
    if did_click && post_click_url.is_some() {
        if let Some(p) = page.as_mut() {
            if !p.clicked {
                p.clicked = true;
                p.clicked_desc = "(点击后页面跳转)".into();
            }
        }
    }
    // 窗题跟页面走(页面自己的标题 = 数据,不是我们产的文案 §6.6;缩略窗一眼认得出在看啥)
    if let Some(p) = &page {
        let t: String = p.title.trim().chars().take(40).collect();
        if !t.is_empty() {
            let _ = entry.win.set_title(&t);
        }
    }
    entry.touch();
    Ok(RenderOutcome { page, download, post_click_url, session: Some(sid) })
}

/// 新建会话窗(超上限先挤掉最旧的)。
fn open_session(
    app: &tauri::AppHandle,
    sessions: &Arc<Mutex<HashMap<String, Arc<SessionEntry>>>>,
    req: &RenderRequest,
) -> Result<(String, Arc<SessionEntry>)> {
    // 挤位:最旧的先收摊(锁内选人,锁外销毁)
    let evict: Vec<(String, Arc<SessionEntry>)> = {
        let map = sessions.lock().expect("webrender sessions");
        if map.len() >= SESSION_MAX {
            let mut all: Vec<_> = map.iter().map(|(k, e)| (k.clone(), e.clone())).collect();
            all.sort_by_key(|(_, e)| *e.last_used.lock().expect("webrender last_used"));
            all.truncate(map.len() + 1 - SESSION_MAX);
            all
        } else {
            Vec::new()
        }
    };
    for (id, e) in evict {
        sessions.lock().expect("webrender sessions").remove(&id);
        let _ = e.win.destroy();
        tracing::debug!(session = %id, "webrender 会话窗被新窗挤掉");
    }

    let url: tauri::Url = req.url.trim().parse().context("url 不合法")?;
    // 初始窗题 = 站点 host(页面自己的信息,不是我们产的文案 §6.6;快照后换成页面标题)
    let host = url.host_str().unwrap_or("…").to_string();
    let sid = format!(
        "lw-render-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );

    let dl_final: Arc<Mutex<Option<PathBuf>>> = Arc::new(Mutex::new(None));
    let (dl_tx, dl_rx) = tokio::sync::mpsc::unbounded_channel::<bool>();
    let download_dir = Arc::new(Mutex::new(req.download_dir.clone()));
    let navs: Arc<Mutex<Vec<(Instant, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let load_seq = Arc::new(AtomicU64::new(0));

    let dl_slot = dl_final.clone();
    let dl_dir = download_dir.clone();
    let nav_slot = navs.clone();
    let seq_slot = load_seq.clone();

    // 右下角缩略位(定位失败回落一个无害默认;margin 给任务栏/dock 留身位)
    let (px, py) = thumb_position(app).unwrap_or((80.0, 80.0));
    let win = tauri::WebviewWindowBuilder::new(app, &sid, tauri::WebviewUrl::External(url))
        .title(&host)
        .inner_size(THUMB_W, THUMB_H)
        .position(px, py)
        // 可见任务窗(2026-07-10 用户拍板「做了啥展示给用户也放心」):真缩略直播,
        // 不抢焦点(用户正在打字)、置顶、不进任务栏(Win 防闪)
        .visible(true)
        .focused(false)
        .always_on_top(true)
        .skip_taskbar(true)
        // 与抓取端同一副面孔(core web::UA 单源;WKWebView 默认 UA 会被"浏览器版本过低"拒,
        // B 站登录窗同款教训)
        .user_agent(larkwing_core::web::UA)
        // 弹窗驯服(先于页面脚本):「下载/查看」最常见的实现是 window.open / target=_blank
        // 开新页——隐藏窗没有弹窗语义,不驯服就石沉大海;改成本窗跳转后,指向 attachment
        // 的地址自然走 on_download。
        .initialization_script(POPUP_TAME_JS)
        .on_navigation(move |u| {
            nav_slot.lock().expect("webrender nav slot").push((Instant::now(), u.to_string()));
            true // 只观察不拦
        })
        .on_download(move |_wv, ev| {
            match ev {
                tauri::webview::DownloadEvent::Requested { destination, .. } => {
                    let suggested = destination
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let name = larkwing_core::files::sanitize_filename(&suggested);
                    let dir = dl_dir.lock().expect("webrender dl dir").clone();
                    let _ = std::fs::create_dir_all(&dir);
                    let dest = larkwing_core::files::dedupe_path(&dir.join(name));
                    *destination = dest.clone();
                    *dl_slot.lock().expect("webrender dl slot") = Some(dest);
                }
                tauri::webview::DownloadEvent::Finished { success, .. } => {
                    let _ = dl_tx.send(success);
                }
                _ => {}
            }
            true // 放行下载
        })
        .on_page_load(move |_win, payload| {
            if matches!(payload.event(), tauri::webview::PageLoadEvent::Finished) {
                seq_slot.fetch_add(1, Ordering::Relaxed);
            }
        })
        .build()
        .context("渲染窗开不起来")?;

    // 缩略直播:zoom = 窗宽 / 页面排版宽(WebView2 ZoomFactor / WKWebView pageZoom,
    // 跨导航持续)。用户拖大窗口 = 自然放大(Resized 按新宽重算);关窗 = 会话收摊。
    let _ = win.set_zoom(THUMB_W / PAGE_LAYOUT_W);
    {
        let sessions_evt = sessions.clone();
        let sid_evt = sid.clone();
        let win_evt = win.clone();
        win.on_window_event(move |ev| match ev {
            tauri::WindowEvent::Resized(sz) => {
                let scale = win_evt.scale_factor().unwrap_or(1.0);
                let w = sz.to_logical::<f64>(scale).width;
                if w > 1.0 {
                    let _ = win_evt.set_zoom((w / PAGE_LAYOUT_W).clamp(0.15, 1.5));
                }
            }
            tauri::WindowEvent::Destroyed => {
                // 用户手动关窗 / 被销毁:注册表同步摘除(幂等——清扫/挤位路径已先摘)
                if let Ok(mut m) = sessions_evt.lock() {
                    m.remove(&sid_evt);
                }
            }
            _ => {}
        });
    }

    let entry = Arc::new(SessionEntry {
        win,
        dl_final,
        dl_done: tokio::sync::Mutex::new(dl_rx),
        navs,
        load_seq,
        download_dir,
        last_used: Mutex::new(Instant::now()),
    });
    sessions.lock().expect("webrender sessions").insert(sid.clone(), entry.clone());
    Ok((sid, entry))
}

/// 右下角缩略位(逻辑坐标;主显示器,给任务栏/dock 留 72px 身位)。
fn thumb_position(app: &tauri::AppHandle) -> Option<(f64, f64)> {
    let m = app.primary_monitor().ok().flatten()?;
    let scale = m.scale_factor();
    let size = m.size().to_logical::<f64>(scale);
    let pos = m.position().to_logical::<f64>(scale);
    Some((pos.x + size.width - THUMB_W - 16.0, pos.y + size.height - THUMB_H - 72.0))
}

/// 等 load_seq 越过 seq0(页面加载完)+ 水合静默期;超时就带着现状走(SPA 常态,不算错)。
async fn wait_for_load(entry: &Arc<SessionEntry>, seq0: u64, deadline: tokio::time::Instant) {
    let cap = tokio::time::Instant::now() + LOAD_WAIT;
    let until = cap.min(deadline);
    while tokio::time::Instant::now() < until {
        if entry.load_seq.load(Ordering::Relaxed) > seq0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    tokio::time::sleep(SETTLE_AFTER_LOAD).await;
}

/// 清空残留下载信号 + 上一步的落点记录(每步动作只认自己触发的下载)。
async fn drain_downloads(entry: &Arc<SessionEntry>) {
    let mut rx = entry.dl_done.lock().await;
    while rx.try_recv().is_ok() {}
    *entry.dl_final.lock().expect("webrender dl slot") = None;
}

/// 点击后的等待。分寸(真机两轮教训的合成):
/// - 下载**起头了**(Requested 到过)→ 等到单步预算(服务端出文件/大文件都要时间);
/// - 跳转发生且**新页已加载完**、没有下载起头 → 立即收手(普通链接点击,别干等);
/// - 跳转在途(nav 到了、load 没完)→ 继续等(重定向链的尽头可能就是附件);
/// - 宽限内毫无动静 → 收手。
/// 返回 (下载产物, 点击后是否导航, 新页是否已 load-finished)。loaded 用来区分「HTML 新页
/// (要快照)」与「PDF/附件内联(注入脚本跑不了,跳过快照,把地址交 web_download)」——
/// Mac WKWebView 点指向 PDF 的链接会内联打开、永不 load。
async fn wait_click_outcome(
    entry: &Arc<SessionEntry>,
    click_moment: Instant,
    seq0: u64,
    deadline: tokio::time::Instant,
) -> (Option<PathBuf>, bool, bool) {
    let grace = tokio::time::Instant::now() + CLICK_DOWNLOAD_GRACE;
    let mut nav_since: Option<tokio::time::Instant> = None; // 首次检测到导航的时刻(判静默期)
    let mut rx = entry.dl_done.lock().await;
    loop {
        let tick = tokio::time::Instant::now() + Duration::from_millis(400);
        let loaded = entry.load_seq.load(Ordering::Relaxed) > seq0;
        match tokio::time::timeout_at(tick.min(deadline), rx.recv()).await {
            Ok(Some(true)) => {
                let path = entry.dl_final.lock().expect("webrender dl slot").clone();
                return (path, last_nav_after(&entry.navs, click_moment).is_some(), loaded);
            }
            Ok(Some(false)) => {
                // 下载失败:清掉半截文件,如实两手空空
                if let Some(p) = entry.dl_final.lock().expect("webrender dl slot").take() {
                    let _ = std::fs::remove_file(p);
                }
                return (None, last_nav_after(&entry.navs, click_moment).is_some(), loaded);
            }
            Ok(None) => return (None, false, false), // 窗没了
            Err(_) => {
                let started = entry.dl_final.lock().expect("webrender dl slot").is_some();
                let navigated = last_nav_after(&entry.navs, click_moment).is_some();
                let now = tokio::time::Instant::now();
                if now >= deadline {
                    return (None, navigated, loaded);
                }
                if started {
                    continue; // 下载在途:等到预算(大文件慢慢下)
                }
                if navigated {
                    let since = *nav_since.get_or_insert(now);
                    // HTML 整页导航 load 完 → 就位;或导航后静默期到(PDF/附件内联打开永不 load)
                    // → 别干等,把跳转地址交 post_click_url(caller 接 web_download)。
                    if loaded || since.elapsed() >= NAV_SETTLE {
                        return (None, true, loaded);
                    }
                } else if now >= grace {
                    return (None, false, loaded); // 毫无动静(纯展开菜单/无效点击)
                }
            }
        }
    }
}

/// 照一张编号快照:注册一次性信箱 → 注入快照脚本 → 等回传(超时重注入一次)。
async fn take_snapshot(
    media: &larkwing_core::media::MediaRuntime,
    entry: &Arc<SessionEntry>,
    deadline: tokio::time::Instant,
) -> Option<RenderedPage> {
    let (post_url, rx) = match media.webrender_collect().await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("webrender 回传信箱没备好: {e:#}");
            return None;
        }
    };
    let js = build_snapshot_script(&post_url);
    let _ = entry.win.eval(js.as_str());
    let first = tokio::time::Instant::now() + SNAPSHOT_WAIT;
    let mut rx = rx;
    match tokio::time::timeout_at(first.min(deadline), &mut rx).await {
        Ok(Ok(json)) => return serde_json::from_str(&json).ok(),
        Ok(Err(_)) => return None,
        Err(_) => {}
    }
    // 迟到水合兜底:重注入同一 token 再等一小段
    let _ = entry.win.eval(js.as_str());
    let second = tokio::time::Instant::now() + SNAPSHOT_WAIT;
    match tokio::time::timeout_at(second.min(deadline), rx).await {
        Ok(Ok(json)) => serde_json::from_str(&json).ok(),
        _ => None,
    }
}

fn last_nav_after(
    navs: &Arc<Mutex<Vec<(Instant, String)>>>,
    after: Instant,
) -> Option<String> {
    navs.lock()
        .expect("webrender nav slot")
        .iter()
        .rev()
        .find(|(at, _)| *at > after)
        .map(|(_, u)| u.clone())
}

/// 初始化脚本(先于页面脚本,每次导航都注入):把「开新页」驯服成本窗跳转。
/// window.open → location.assign;target=_blank 锚点在捕获阶段改 _self。
const POPUP_TAME_JS: &str = r#"(function() {
  try {
    window.open = function(u) { try { if (u) window.location.assign(u); } catch (e) {} return null; };
  } catch (e) {}
  try {
    document.addEventListener('click', function(ev) {
      var n = ev.target;
      while (n && n !== document) {
        if (n.tagName === 'A' && n.getAttribute && n.getAttribute('target') === '_blank') {
          n.setAttribute('target', '_self');
          break;
        }
        n = n.parentNode;
      }
    }, true);
  } catch (e) {}
})();"#;

/// 点击脚本:按编号(data-lw-ref,上次快照打的)或按文字(精确>包含、最内层优先、
/// button/a/input 优先、文字更短更 specific——治「点到展开菜单的容器」)。
/// 结果存 window.__lwClick,下一张快照带回(点击引发跳转时 stash 会被冲掉——
/// rust 侧以「点击后出现导航」补判 clicked)。
fn build_click_script(click_ref: Option<u32>, click_text: Option<&str>) -> String {
    let ref_js = click_ref.map(|n| n.to_string()).unwrap_or_else(|| "null".into());
    let text_js = serde_json::to_string(click_text.unwrap_or("")).unwrap_or_else(|_| "\"\"".into());
    format!(
        r#"(function() {{
  var REF = {ref_js}; var CLICK = {text_js};
  function txt(el) {{ return ((el.innerText || el.value || '') + '').replace(/\s+/g, ' ').trim().slice(0, 40); }}
  var target = null; var stale = false;
  if (REF !== null) {{
    target = document.querySelector('[data-lw-ref="' + REF + '"]');
    if (!target) stale = true; // 编号失效(页面变了):如实报,别瞎点
  }} else if (CLICK) {{
    var pri = {{ BUTTON: 0, A: 1, INPUT: 2 }};
    var all = document.querySelectorAll('button,a,[role="button"],[onclick],input[type="button"],input[type="submit"]');
    var hits = [];
    for (var k = 0; k < all.length && hits.length < 200; k++) {{
      if (txt(all[k]).indexOf(CLICK) >= 0) hits.push(all[k]);
    }}
    var best = null, bs = null;
    for (var m = 0; m < hits.length; m++) {{
      var el = hits[m], inner = false;
      for (var n = 0; n < hits.length; n++) {{
        if (n !== m && el !== hits[n] && el.contains(hits[n])) {{ inner = true; break; }}
      }}
      if (inner) continue; // 有更内层的命中:让给它(容器 vs 真按钮)
      var t = txt(el);
      var score = (t === CLICK ? 0 : 1) * 1000000 + (pri[el.tagName] !== undefined ? pri[el.tagName] : 3) * 10000 + Math.min(t.length, 9999);
      if (best === null || score < bs) {{ best = el; bs = score; }}
    }}
    target = best;
  }}
  window.__lwClick = {{
    clicked: !!target,
    desc: target ? (target.tagName + '「' + txt(target) + '」') : '',
    stale: stale
  }};
  if (target) setTimeout(function() {{ try {{ target.click(); }} catch (e) {{}} }}, 50);
}})();"#
    )
}

/// 快照脚本:给可见交互元素打 `data-lw-ref` 编号(下一步 click_ref 引用),抽
/// 「标题/正文/链接/编号元素」+ 上一步点击报告(__lwClick,读完即清)POST 回 loopback。
fn build_snapshot_script(collect_url: &str) -> String {
    let post = serde_json::to_string(collect_url).unwrap_or_else(|_| "\"\"".into());
    format!(
        r#"(function() {{
  var POST = {post};
  function txt(el) {{ return ((el.innerText || el.value || '') + '').replace(/\s+/g, ' ').trim().slice(0, 40); }}
  function role(el) {{
    var t = el.tagName;
    if (t === 'BUTTON' || t === 'INPUT') return 'button';
    if (t === 'A') return el.getAttribute('href') ? 'link' : 'click';
    return 'click';
  }}
  var links = []; var seen = {{}};
  var anchors = document.querySelectorAll('a[href]');
  for (var i = 0; i < anchors.length && links.length < 25; i++) {{
    var h = anchors[i].href || '';
    if (!/^https?:/.test(h) || seen[h]) continue; seen[h] = 1;
    links.push({{ text: txt(anchors[i]) || h.split('/').pop().slice(0, 40), url: h }});
  }}
  var elements = [];
  var cands = document.querySelectorAll('button,a,[role="button"],[onclick],input[type="button"],input[type="submit"]');
  var no = 1;
  for (var j = 0; j < cands.length && elements.length < 40; j++) {{
    var el = cands[j];
    if (el.offsetParent === null) continue; // display:none 等不可见的不编号
    var t = txt(el);
    if (!t) continue;
    el.setAttribute('data-lw-ref', String(no));
    var h = (el.tagName === 'A' && el.href && /^https?:/.test(el.href)) ? el.href : null;
    elements.push({{ ref: no, role: role(el), text: t, href: h }});
    no++;
  }}
  var click = window.__lwClick || {{}};
  try {{ delete window.__lwClick; }} catch (e) {{}}
  var payload = {{
    title: (document.title || '').slice(0, 200),
    text: (document.body ? document.body.innerText : '').slice(0, 8000),
    links: links,
    elements: elements,
    clicked: !!click.clicked,
    clicked_desc: click.desc || '',
    click_ref_stale: !!click.stale
  }};
  try {{ fetch(POST, {{ method: 'POST', headers: {{ 'Content-Type': 'text/plain' }}, body: JSON.stringify(payload) }}); }} catch (e) {{}}
}})();"#
    )
}
