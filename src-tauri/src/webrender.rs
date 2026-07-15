//! core「网页渲染器」接缝(webrender.rs)的壳层实现:**app 自己就是浏览器** ——
//! 隐藏 WebView 窗(WebView2/WKWebView,同 B 站扫码登录窗先例)真渲染 JS 页面。
//! L2 会话式浏览(2026-07-10):窗口跨调用存活(TTL 180s / 最多 2 个,清扫任务收摊),
//! 每步 = 可选动作(导航 / back / 点编号 / 点文字)→ **编号快照**(给交互元素打
//! `data-lw-ref`,文本版 Set-of-Marks)经 relay `/collect/{token}` POST 回 core。
//! 下载由 `on_download` 接管(Requested 时定落点 sanitize+dedupe;**mac 的 Finished.path
//! 恒空——只能用 Requested 时自己记下的落点**,tauri 文档明示的平台差异)。
//! 完全操作第一批(2026-07-14)开了填字/填表/选下拉/按键/提交/滚动;**文件上传
//! (2026-07-15)**:Rust 读文件 → base64 分片 eval 暂存进页面(`window.__lwUpStage`)→
//! 输入脚本组装 File + DataTransfer 赋给 `input.files` + 派发 change(Playwright 同思路
//! 的纯 JS 版;凭证代填 / CDP 可信输入仍不做)。纪律:不给远程页任何 IPC 桥(注入脚本
//! 只 POST loopback 一次性 token);会话窗生死归 TTL/清扫,绝不无限存活。

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

    // ---- 3. 动作:back > 输入(type/fill/select/press)> click > scroll ----
    drain_downloads(&entry).await; // 上一步残留的下载信号别混进本步
    let mut download = None;
    let mut post_click_url = None;
    let mut skip_snapshot = false;
    let did_click = req.click_ref.is_some() || req.click_text.is_some();
    let did_upload = req.upload_ref.is_some();
    let did_input = did_upload
        || req.type_ref.is_some()
        || !req.fill.is_empty()
        || req.select_ref.is_some()
        || req.press_key.is_some();
    if req.back {
        task.step("step.render_back", serde_json::Value::Null);
        let seq0 = entry.load_seq.load(Ordering::Relaxed);
        let _ = entry.win.eval("try{history.back()}catch(e){}");
        wait_for_load(&entry, seq0, deadline).await;
    } else if did_input {
        task.step(
            if did_upload { "step.render_upload" } else { "step.render_input" },
            serde_json::Value::Null,
        );
        // 上传先把文件字节暂存进页面世界(分片 eval),输入脚本再组装成 File 赋给上传框
        if did_upload {
            stage_upload_files(&entry, &req.upload_paths).await?;
        }
        let act_moment = Instant::now();
        let seq0 = entry.load_seq.load(Ordering::Relaxed);
        let js = build_input_script(&req);
        entry.win.eval(js.as_str()).context("输入指令没发出去")?;
        // submit / 回车可能引发导航或下载 → 走点击同款结局等待;否则给短静默让框架状态落定。
        let may_navigate = req.submit || req.press_key.as_deref() == Some("Enter");
        if may_navigate {
            let (dl, navigated, loaded) = wait_click_outcome(&entry, act_moment, seq0, deadline).await;
            download = dl;
            if navigated && download.is_none() {
                post_click_url = last_nav_after(&entry.navs, act_moment);
                skip_snapshot = !loaded;
            }
            if navigated && loaded {
                tokio::time::sleep(
                    SETTLE_AFTER_LOAD
                        .min(deadline.saturating_duration_since(tokio::time::Instant::now())),
                )
                .await;
            }
        } else {
            tokio::time::sleep(
                SETTLE_AFTER_LOAD.min(deadline.saturating_duration_since(tokio::time::Instant::now())),
            )
            .await;
        }
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
    } else if let Some(dir) = req.scroll.as_deref() {
        task.step("step.render_scroll", serde_json::Value::Null);
        let _ = entry.win.eval(build_scroll_script(dir).as_str());
        tokio::time::sleep(
            Duration::from_millis(500)
                .min(deadline.saturating_duration_since(tokio::time::Instant::now())),
        )
        .await;
    }
    // wait_text:动作后等这段文字出现(SPA 异步内容),再快照;超时也照常快照(不算错)。
    if let Some(needle) = req.wait_text.as_deref() {
        if !skip_snapshot {
            wait_for_text(&media, &entry, needle, deadline).await;
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
    // 点击/提交引发跳转会把页面世界里的 __lwClick 冲掉 → 以「动作后出现导航」补判 acted
    if (did_click || did_input) && post_click_url.is_some() {
        if let Some(p) = page.as_mut() {
            if !p.clicked {
                p.clicked = true;
                p.clicked_desc = "(动作后页面跳转)".into();
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
    // 截图(模型自行决定要不要;没打开窗就没得截 —— 到这步窗必在,截不到只因平台/组件不支持)。
    // 工具结果多媒体第一个消费者:图随 ToolOutput 图片 part 回给模型,非视觉模型出向层降级。
    let screenshot = if req.screenshot { capture_screenshot(&entry.win).await } else { None };
    entry.touch();
    Ok(RenderOutcome { page, download, post_click_url, session: Some(sid), screenshot })
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

/// 动作后等某段文字出现再快照(SPA 异步内容的显式等待)。复用一次性 collect 信箱:注入轮询
/// 脚本,页面文本含目标 / 或轮询到顶就 POST 回来;超时也返回(照常快照,不算错)。
async fn wait_for_text(
    media: &larkwing_core::media::MediaRuntime,
    entry: &Arc<SessionEntry>,
    needle: &str,
    deadline: tokio::time::Instant,
) {
    let (post_url, rx) = match media.webrender_collect().await {
        Ok(v) => v,
        Err(_) => return,
    };
    let needle_js = serde_json::to_string(needle).unwrap_or_else(|_| "\"\"".into());
    let post_js = serde_json::to_string(&post_url).unwrap_or_else(|_| "\"\"".into());
    let js = format!(
        r#"(function() {{
  var NEEDLE = {needle_js}, POST = {post_js}, tries = 0;
  function done() {{ try {{ fetch(POST, {{ method: 'POST', headers: {{ 'Content-Type': 'text/plain' }}, body: '1' }}); }} catch (e) {{}} }}
  function check() {{
    var body = document.body ? document.body.innerText : '';
    if (body.indexOf(NEEDLE) >= 0 || tries++ > 50) done();
    else setTimeout(check, 200);
  }}
  check();
}})();"#
    );
    let _ = entry.win.eval(js.as_str());
    let until = (tokio::time::Instant::now() + LOAD_WAIT).min(deadline);
    let _ = tokio::time::timeout_at(until, rx).await;
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
///
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

/// 上传暂存:把本机文件读进来,base64 **分片** eval 进页面世界的 `window.__lwUpStage`
/// (b64 字母表无引号/反斜杠,直接嵌进 JS 字符串安全;分片防单条 eval 过大,两端 WebView
/// 都稳)。输入脚本里 `stagedFiles()` 组装成 File。总量闸复验(工具层验过元数据,这里按
/// 真实字节再验一道,单源 `webrender::UPLOAD_MAX_BYTES`)。
async fn stage_upload_files(entry: &Arc<SessionEntry>, paths: &[PathBuf]) -> Result<()> {
    /// 每条 eval 的 base64 字符数(≈1.5MB 原始字节)。
    const CHUNK: usize = 2 * 1024 * 1024;
    let mut total: u64 = 0;
    entry.win.eval("window.__lwUpStage = [];").context("上传暂存没建起来")?;
    for (i, p) in paths.iter().enumerate() {
        let bytes = tokio::fs::read(p).await.with_context(|| format!("读不了 {}", p.display()))?;
        total += bytes.len() as u64;
        anyhow::ensure!(
            total <= larkwing_core::webrender::UPLOAD_MAX_BYTES,
            "这批文件加起来超过上传上限,传不了"
        );
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| format!("file{i}"));
        let name_js = serde_json::to_string(&name).unwrap_or_else(|_| "\"file\"".into());
        let mime_js = serde_json::to_string(upload_mime(&name)).unwrap_or_else(|_| "\"\"".into());
        entry
            .win
            .eval(format!("window.__lwUpStage.push({{name:{name_js},type:{mime_js},parts:[]}});").as_str())
            .context("上传暂存没建起来")?;
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        for chunk in b64.as_bytes().chunks(CHUNK) {
            let s = std::str::from_utf8(chunk).expect("base64 是纯 ASCII");
            entry
                .win
                .eval(format!("window.__lwUpStage[{i}].parts.push(\"{s}\");").as_str())
                .context("上传分片没送进页面")?;
        }
    }
    Ok(())
}

/// 上传文件的 MIME(File.type;站点常拿它对 accept 校验)。图片族复用 core 的单源映射,
/// 其余补常见文档/媒体,认不出回落 octet-stream(多数站点只看扩展名,无碍)。
fn upload_mime(name: &str) -> &'static str {
    if let Some(m) = larkwing_core::attach::image_mime_by_ext(name) {
        return m;
    }
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "pdf" => "application/pdf",
        "txt" | "md" | "log" => "text/plain",
        "csv" => "text/csv",
        "json" => "application/json",
        "xml" => "text/xml",
        "html" | "htm" => "text/html",
        "zip" => "application/zip",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        _ => "application/octet-stream",
    }
}

/// 输入类动作脚本(完全操作第一批):按编号 type 填字 / fill 批量 / select 选下拉 /
/// press 按键,可选 submit 提交表单。React/Vue 受控组件走「原生 value setter + input 事件」
/// (直接改 `.value` 不触发框架状态更新,业界事实标准);填后 blur 触发 on-blur 校验;下张
/// 快照回读 value = 天然「填对没」校验。合成键盘事件 isTrusted=false **不触发原生提交** →
/// 提交走 `requestSubmit()`。**上传(UP)**:组装 `stage_upload_files` 暂存的分片成 File,
/// DataTransfer 赋给 `input.files` + 派发 input/change(编号不是文件框时就近找一个:
/// 自身 → 后代 → label 指向的控件——「上传」按钮和隐藏 input 分家是常见页型);单选框
/// 给了多个文件只传第一个、note 如实说。结果写 window.__lwClick(clicked=acted/desc/stale/
/// note),下张快照带回(与点击同一回传口,壳层/工具复用 clicked/stale/note 呈现)。
fn build_input_script(req: &RenderRequest) -> String {
    let ref_js = req.type_ref.map(|n| n.to_string()).unwrap_or_else(|| "null".into());
    let txt_js =
        serde_json::to_string(req.type_text.as_deref().unwrap_or("")).unwrap_or_else(|_| "\"\"".into());
    let fill_js = serde_json::to_string(
        &req.fill
            .iter()
            .map(|f| serde_json::json!({ "ref": f.ref_no, "value": f.value }))
            .collect::<Vec<_>>(),
    )
    .unwrap_or_else(|_| "[]".into());
    let sel_js = req.select_ref.map(|n| n.to_string()).unwrap_or_else(|| "null".into());
    let opt_js = serde_json::to_string(req.select_option.as_deref().unwrap_or(""))
        .unwrap_or_else(|_| "\"\"".into());
    let key_js =
        serde_json::to_string(req.press_key.as_deref().unwrap_or("")).unwrap_or_else(|_| "\"\"".into());
    let up_js = req.upload_ref.map(|n| n.to_string()).unwrap_or_else(|| "null".into());
    let submit = req.submit;
    format!(
        r#"(function() {{
  var REF = {ref_js}; var TXT = {txt_js}; var FILL = {fill_js};
  var SEL = {sel_js}; var OPT = {opt_js}; var KEY = {key_js}; var UP = {up_js}; var SUBMIT = {submit};
  var stale = false, acted = false, desc = '', note = '', last = null;
  function byRef(n) {{ var el = document.querySelector('[data-lw-ref="' + n + '"]'); if (!el) stale = true; return el; }}
  function stagedFiles() {{
    var st = window.__lwUpStage || []; var out = [];
    try {{
      for (var i = 0; i < st.length; i++) {{
        var bin = atob((st[i].parts || []).join(''));
        var u8 = new Uint8Array(bin.length);
        for (var k = 0; k < bin.length; k++) u8[k] = bin.charCodeAt(k);
        out.push(new File([u8], st[i].name || ('file' + i), {{ type: st[i].type || '' }}));
      }}
    }} catch (e) {{ out = []; }}
    try {{ delete window.__lwUpStage; }} catch (e) {{}}
    return out;
  }}
  function setVal(el, v) {{
    try {{ el.focus(); }} catch (e) {{}}
    if (el.isContentEditable) {{
      try {{ document.execCommand('selectAll', false, null); document.execCommand('insertText', false, v); }}
      catch (e) {{ el.textContent = v; el.dispatchEvent(new Event('input', {{ bubbles: true }})); }}
      return;
    }}
    var proto = el.tagName === 'TEXTAREA' ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
    try {{ Object.getOwnPropertyDescriptor(proto, 'value').set.call(el, v); }} catch (e) {{ el.value = v; }}
    el.dispatchEvent(new Event('input', {{ bubbles: true }}));
    el.dispatchEvent(new Event('change', {{ bubbles: true }}));
    try {{ el.blur(); }} catch (e) {{}}
  }}
  if (UP !== null) {{
    var eu = byRef(UP);
    if (eu) {{
      var fi = null;
      if (eu.tagName === 'INPUT' && (eu.getAttribute('type') || '').toLowerCase() === 'file') fi = eu;
      else if (eu.querySelector) fi = eu.querySelector('input[type=file]');
      if (!fi && eu.tagName === 'LABEL' && eu.control) fi = eu.control;
      var files = stagedFiles();
      if (!fi) {{ note = '[' + UP + '] 不是文件上传框(里面也没找到)——从快照里挑标「文件上传框」的编号'; }}
      else if (!files.length) {{ note = '文件没送进页面(可能页面刚跳转把暂存冲掉了)——再试一次'; }}
      else {{
        var use = files;
        if (!fi.multiple && files.length > 1) {{ use = [files[0]]; note = '这个框只收一个文件,先传了第一个:' + use[0].name; }}
        try {{
          var dt = new DataTransfer();
          for (var w = 0; w < use.length; w++) dt.items.add(use[w]);
          fi.files = dt.files;
          fi.dispatchEvent(new Event('input', {{ bubbles: true }}));
          fi.dispatchEvent(new Event('change', {{ bubbles: true }}));
          acted = true; last = fi;
          var nm = []; for (var v = 0; v < use.length; v++) nm.push(use[v].name);
          desc = '传文件[' + UP + ']:' + nm.join('、').slice(0, 60);
        }} catch (e) {{ note = '上传框不认这批文件(' + ((e && e.message) || 'DataTransfer 失败') + ')'; }}
      }}
    }}
  }}
  else if (REF !== null) {{ var e0 = byRef(REF); if (e0) {{ setVal(e0, TXT); acted = true; last = e0; desc = '填入[' + REF + ']'; }} }}
  else if (FILL && FILL.length) {{ for (var i = 0; i < FILL.length; i++) {{ var ei = byRef(FILL[i].ref); if (ei) {{ setVal(ei, FILL[i].value); acted = true; last = ei; }} }} desc = '批量填 ' + FILL.length + ' 项'; }}
  else if (SEL !== null) {{
    var s = byRef(SEL);
    if (s && s.tagName === 'SELECT') {{
      var hit = -1, o = s.options;
      for (var j = 0; j < o.length; j++) {{ if ((o[j].text || '').trim() === OPT || o[j].value === OPT) {{ hit = j; break; }} }}
      if (hit < 0) for (var k = 0; k < o.length; k++) {{ if ((o[k].text || '').indexOf(OPT) >= 0) {{ hit = k; break; }} }}
      if (hit >= 0) {{ s.selectedIndex = hit; s.dispatchEvent(new Event('input', {{ bubbles: true }})); s.dispatchEvent(new Event('change', {{ bubbles: true }})); acted = true; last = s; desc = '选中[' + SEL + ']→' + (o[hit].text || '').trim().slice(0, 20); }}
    }}
  }}
  else if (KEY) {{
    var t = document.activeElement || document.body;
    ['keydown', 'keypress', 'keyup'].forEach(function(ty) {{ try {{ t.dispatchEvent(new KeyboardEvent(ty, {{ key: KEY, bubbles: true }})); }} catch (e) {{}} }});
    acted = true; desc = '按键 ' + KEY;
  }}
  if (SUBMIT) {{
    var f = (last && last.form) || (document.activeElement && document.activeElement.form) || document.querySelector('form');
    if (f) {{ setTimeout(function() {{ try {{ f.requestSubmit(); }} catch (e) {{ try {{ f.submit(); }} catch (e2) {{}} }} }}, 60); desc += ' + 提交'; acted = true; }}
  }}
  window.__lwClick = {{ clicked: acted, desc: desc, stale: stale, note: note }};
}})();"#
    )
}

/// 滚动翻页脚本:按视口高度上/下滚约一屏(够屏外内容;配合快照的 scroll_hint)。
fn build_scroll_script(dir: &str) -> String {
    let sign = if dir.eq_ignore_ascii_case("up") { "-" } else { "" };
    format!(
        r#"(function() {{ try {{ var se = document.scrollingElement || document.documentElement; window.scrollBy(0, {sign}(se.clientHeight || 600) * 0.9); }} catch (e) {{}} }})();"#
    )
}

/// 截当前渲染窗为 PNG,转 `data:image/png;base64,…`(工具结果多媒体第一个消费者)。
/// 平台原生 FFI(wry/tauri 无官方截图 API):Mac `WKWebView.takeSnapshot` / Win WebView2
/// `CapturePreview`。截不到(平台不支持 / 组件缺 / 失败 / 超时)→ None,工具如实说、不塞空图(§3.5)。
/// **只截当前可视视口**(两端 API 皆如此),不含滚动到屏外的内容;渲染窗是可见缩略窗故截得到。
async fn capture_screenshot(win: &tauri::WebviewWindow) -> Option<String> {
    let png = capture_png(win).await?;
    Some(to_data_url(&png))
}

/// PNG bytes → data URL。
fn to_data_url(png: &[u8]) -> String {
    use base64::Engine;
    format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(png))
}

/// 截图上限:窗不可见时 WebView2 completion 可能不触发(#579),靠它兜成 None、绝不挂死。
const SHOT_TIMEOUT: Duration = Duration::from_secs(8);

// ───────────────────────── macOS:WKWebView.takeSnapshot ─────────────────────────
#[cfg(target_os = "macos")]
async fn capture_png(win: &tauri::WebviewWindow) -> Option<Vec<u8>> {
    use block2::RcBlock;
    use objc2_app_kit::NSImage;
    use objc2_foundation::NSError;
    use objc2_web_kit::WKWebView;
    use std::cell::RefCell;

    let (tx, rx) = tokio::sync::oneshot::channel::<Option<Vec<u8>>>();
    // with_webview 闭包在**主线程**跑、且阻塞调用线程到闭包返回 → 闭包只**发起**截图立刻返回,
    // completion 稍后在主线程 runloop 触发(绝不在闭包内同步等 completion,那要主线程 = 死锁)。
    let dispatched = win.with_webview(move |pw| {
        let ptr = pw.inner() as *const WKWebView; // inner() = *mut c_void = WKWebView
        if ptr.is_null() {
            let _ = tx.send(None);
            return;
        }
        let webview: &WKWebView = unsafe { &*ptr };
        // completion 是 dyn Fn(可多次调),oneshot 只发一次 → RefCell<Option<Sender>> take;只主线程用,无需 Send。
        let slot = RefCell::new(Some(tx));
        let completion = RcBlock::new(move |image: *mut NSImage, _err: *mut NSError| {
            let bytes = ns_image_to_png(image);
            if let Some(tx) = slot.borrow_mut().take() {
                let _ = tx.send(bytes);
            }
        });
        // 第一个参数 None ⇒ 默认配置 = 截当前可视 bounds。
        unsafe { webview.takeSnapshotWithConfiguration_completionHandler(None, &completion) };
    });
    if dispatched.is_err() {
        return None;
    }
    // 只在异步上下文 await(绝不在主线程阻塞);发起失败时 tx 已 drop → rx 立刻 Err → None。
    match tokio::time::timeout(SHOT_TIMEOUT, rx).await {
        Ok(Ok(bytes)) => bytes,
        _ => None,
    }
}

/// NSImage → PNG(TIFF → NSBitmapImageRep → PNG;保留真实像素,Retina 屏为逻辑像素 2×)。
#[cfg(target_os = "macos")]
fn ns_image_to_png(image: *mut objc2_app_kit::NSImage) -> Option<Vec<u8>> {
    use objc2::AnyThread; // 提供 NSBitmapImageRep::alloc()
    use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep};
    use objc2_foundation::NSDictionary;

    if image.is_null() {
        return None;
    }
    let image: &objc2_app_kit::NSImage = unsafe { &*image };
    let tiff = image.TIFFRepresentation()?;
    let rep = NSBitmapImageRep::initWithData(NSBitmapImageRep::alloc(), &tiff)?;
    let props = NSDictionary::new();
    let png = unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &props) }?;
    Some(png.to_vec())
}

// ───────────────────────── Windows:WebView2.CapturePreview ─────────────────────────
#[cfg(target_os = "windows")]
async fn capture_png(win: &tauri::WebviewWindow) -> Option<Vec<u8>> {
    use webview2_com::CapturePreviewCompletedHandler;
    use webview2_com::Microsoft::Web::WebView2::Win32::COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG;
    use windows::Win32::UI::Shell::SHCreateMemStream;

    let (tx, rx) = tokio::sync::oneshot::channel::<Option<Vec<u8>>>();
    let dispatched = win.with_webview(move |pw| {
        let controller = pw.controller();
        let webview = match unsafe { controller.CoreWebView2() } {
            Ok(w) => w,
            Err(_) => {
                let _ = tx.send(None);
                return;
            }
        };
        let stream = match unsafe { SHCreateMemStream(None) } {
            Some(s) => s,
            None => {
                let _ = tx.send(None);
                return;
            }
        };
        let stream_for_read = stream.clone(); // 引用计数 +1,供 completion 里读
        // create 收 FnOnce(windows::core::Result<()>) -> Result<()>:webview2-com 已把原生
        // Invoke(errorcode: HRESULT) 包成 Result(Ok=截图成功)。直接 move 走 tx / stream(仅主线程用)。
        let handler = CapturePreviewCompletedHandler::create(Box::new(
            move |result: windows::core::Result<()>| -> windows::core::Result<()> {
                let bytes = if result.is_ok() {
                    unsafe { read_stream_all(&stream_for_read) }.ok()
                } else {
                    None
                };
                let _ = tx.send(bytes);
                Ok(())
            },
        ));
        // 发起后立即返回;completion 稍后在主线程 message loop 触发。发起 Err ⇒ handler 连 tx 一起
        // drop ⇒ rx.await 立刻 Err ⇒ None。
        let _ = unsafe {
            webview.CapturePreview(COREWEBVIEW2_CAPTURE_PREVIEW_IMAGE_FORMAT_PNG, &stream, &handler)
        };
    });
    if dispatched.is_err() {
        return None;
    }
    match tokio::time::timeout(SHOT_TIMEOUT, rx).await {
        Ok(Ok(bytes)) => bytes,
        _ => None,
    }
}

/// 从 IStream 读全部字节(seek 到尾求长度 → 回 0 → 循环读;Read 返回 HRESULT 用 `.ok()`)。
#[cfg(target_os = "windows")]
unsafe fn read_stream_all(
    stream: &windows::Win32::System::Com::IStream,
) -> windows::core::Result<Vec<u8>> {
    use windows::Win32::System::Com::{STREAM_SEEK_END, STREAM_SEEK_SET};

    let mut size: u64 = 0;
    stream.Seek(0, STREAM_SEEK_END, Some(&mut size))?;
    stream.Seek(0, STREAM_SEEK_SET, None)?;
    let mut buf = vec![0u8; size as usize];
    let mut total = 0usize;
    while total < buf.len() {
        let mut read: u32 = 0;
        let remaining = (buf.len() - total) as u32;
        stream
            .Read(buf[total..].as_mut_ptr() as *mut core::ffi::c_void, remaining, Some(&mut read))
            .ok()?;
        if read == 0 {
            break;
        }
        total += read as usize;
    }
    buf.truncate(total);
    Ok(buf)
}

// ───────────────── 其它平台(Linux 开发)兜底 ─────────────────
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
async fn capture_png(_win: &tauri::WebviewWindow) -> Option<Vec<u8>> {
    None
}

/// 快照脚本:给可见交互元素(可点 + 可填 + 可选 + 勾选)打 `data-lw-ref` 编号,抽
/// 「标题/正文/链接/编号元素(带 label/值/勾选态/选项)/滚动位置」+ 上一步动作报告
/// (__lwClick,读完即清)POST 回 loopback。文本版 Set-of-Marks:同一编号既能 click_ref
/// 点、也能 type_ref 填 / select_ref 选(壳层动作脚本按编号 querySelector 定位)。
fn build_snapshot_script(collect_url: &str) -> String {
    let post = serde_json::to_string(collect_url).unwrap_or_else(|_| "\"\"".into());
    format!(
        r#"(function() {{
  var POST = {post};
  function txt(el) {{ return ((el.innerText || '') + '').replace(/\s+/g, ' ').trim().slice(0, 40); }}
  function labelOf(el) {{
    try {{ if (el.id) {{ var l = document.querySelector('label[for="' + (window.CSS && CSS.escape ? CSS.escape(el.id) : el.id) + '"]'); if (l) return txt(l); }} }} catch (e) {{}}
    var p = el.closest ? el.closest('label') : null; if (p) return txt(p);
    var a = el.getAttribute('aria-label'); if (a) return a.trim().slice(0, 40);
    var ph = el.getAttribute('placeholder'); if (ph) return ph.trim().slice(0, 40);
    var nm = el.getAttribute('name'); if (nm) return nm.trim().slice(0, 40);
    return '';
  }}
  // 交互元素的类别 + 展示名 + 当前值 + 勾选态 + 选项。null = 不编号(跳过)。
  function describe(el) {{
    var tag = el.tagName;
    var type = (el.getAttribute('type') || '').toLowerCase();
    if (tag === 'BUTTON' || (tag === 'INPUT' && (type === 'button' || type === 'submit' || type === 'reset' || type === 'image'))) {{
      var bt = txt(el) || el.value || labelOf(el); return bt ? {{ role: 'button', text: bt }} : null;
    }}
    if (tag === 'A') {{ var at = txt(el); return at ? {{ role: el.getAttribute('href') ? 'link' : 'click', text: at }} : null; }}
    if (tag === 'INPUT' && (type === 'checkbox' || type === 'radio')) {{ return {{ role: type, text: labelOf(el) || type, checked: !!el.checked }}; }}
    if (tag === 'INPUT' && type === 'hidden') return null;
    if (tag === 'INPUT' && type === 'file') {{
      var names = []; try {{ for (var q = 0; q < el.files.length; q++) names.push(el.files[q].name); }} catch (e) {{}}
      return {{ role: 'file', text: labelOf(el) || '文件上传', value: names.join(', ').slice(0, 80), accept: (el.getAttribute('accept') || '').slice(0, 60), multiple: !!el.multiple }};
    }}
    if (tag === 'INPUT') {{ var sec = (type === 'password'); return {{ role: 'input', text: labelOf(el) || '输入框', value: sec ? '' : (el.value || '').slice(0, 80), secret: sec }}; }}
    if (tag === 'TEXTAREA') {{ return {{ role: 'textarea', text: labelOf(el) || '文本域', value: (el.value || '').slice(0, 80) }}; }}
    if (tag === 'SELECT') {{
      var opts = []; for (var i = 0; i < el.options.length && opts.length < 30; i++) opts.push((el.options[i].text || '').trim().slice(0, 40));
      var cur = el.selectedIndex >= 0 ? (el.options[el.selectedIndex].text || '').trim().slice(0, 40) : '';
      return {{ role: 'select', text: labelOf(el) || '下拉', value: cur, options: opts }};
    }}
    if (el.isContentEditable) {{ var et = txt(el); return {{ role: 'editable', text: labelOf(el) || '可编辑区', value: et.slice(0, 80) }}; }}
    if (el.getAttribute('role') === 'button' || el.hasAttribute('onclick')) {{ var ct = txt(el); return ct ? {{ role: 'click', text: ct }} : null; }}
    return null;
  }}
  var links = []; var seen = {{}};
  var anchors = document.querySelectorAll('a[href]');
  for (var i = 0; i < anchors.length && links.length < 25; i++) {{
    var h = anchors[i].href || '';
    if (!/^https?:/.test(h) || seen[h]) continue; seen[h] = 1;
    links.push({{ text: txt(anchors[i]) || h.split('/').pop().slice(0, 40), url: h }});
  }}
  var elements = [];
  var cands = document.querySelectorAll('button,a,[role="button"],[onclick],input,textarea,select,[contenteditable]');
  var no = 1;
  for (var j = 0; j < cands.length && elements.length < 60; j++) {{
    var el = cands[j];
    // 文件上传框豁免可见性过滤:常见页型就是 display:none 的隐藏 input + 好看的按钮
    var isFile = (el.tagName === 'INPUT' && (el.getAttribute('type') || '').toLowerCase() === 'file');
    if (el.offsetParent === null && !isFile) continue; // display:none 等不可见的不编号
    var d = describe(el);
    if (!d) continue;
    el.setAttribute('data-lw-ref', String(no));
    var h = (el.tagName === 'A' && el.href && /^https?:/.test(el.href)) ? el.href : null;
    elements.push({{ ref: no, role: d.role, text: d.text, href: h, value: d.value || '', checked: (d.checked === undefined ? null : d.checked), options: d.options || [], secret: !!d.secret, accept: d.accept || '', multiple: !!d.multiple }});
    no++;
  }}
  var scroll_hint = '';
  try {{
    var se = document.scrollingElement || document.documentElement;
    var vh = se.clientHeight || 1;
    var above = Math.round(se.scrollTop / vh);
    var below = Math.round((se.scrollHeight - se.scrollTop - vh) / vh);
    if (above > 0 || below > 0) scroll_hint = '上面约 ' + above + ' 屏 / 下面约 ' + below + ' 屏';
  }} catch (e) {{}}
  var click = window.__lwClick || {{}};
  try {{ delete window.__lwClick; }} catch (e) {{}}
  var payload = {{
    title: (document.title || '').slice(0, 200),
    text: (document.body ? document.body.innerText : '').slice(0, 8000),
    links: links,
    elements: elements,
    scroll_hint: scroll_hint,
    clicked: !!click.clicked,
    clicked_desc: click.desc || '',
    click_ref_stale: !!click.stale,
    click_note: click.note || ''
  }};
  try {{ fetch(POST, {{ method: 'POST', headers: {{ 'Content-Type': 'text/plain' }}, body: JSON.stringify(payload) }}); }} catch (e) {{}}
}})();"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 上传/快照脚本的形状守卫;`--nocapture` 时打印成品脚本(浏览器冒烟验 JS 语法/行为用:
    /// format! 模板的括号转义错误只有真跑 JS 才抓得到)。
    #[test]
    fn upload_and_snapshot_scripts_shape() {
        for (up, tag) in [(4u32, "UP4"), (5, "UP5"), (1, "UP1")] {
            let req = RenderRequest {
                upload_ref: Some(up),
                upload_paths: vec![PathBuf::from("/tmp/单据.pdf")],
                ..Default::default()
            };
            let js = build_input_script(&req);
            assert!(js.contains(&format!("var UP = {up}")), "{js}");
            assert!(js.contains("stagedFiles") && js.contains("DataTransfer"), "{js}");
            assert!(js.contains("note: note"), "{js}");
            println!("__SCRIPT_{tag}__\n{js}");
        }
        let snap = build_snapshot_script("http://127.0.0.1:1/collect/x");
        assert!(snap.contains("type === 'file'"), "{snap}");
        assert!(snap.contains("click_note"), "{snap}");
        println!("__SCRIPT_SNAP__\n{snap}\n__END__");
    }
}
