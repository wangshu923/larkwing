//! 壳:只做装配(开库 → 解析 key → 造 provider/Engine → 注册 commands)和窗口,不写业务。

mod commands;
// 前台全屏检测(悬浮窗让位):仅 Windows 编译,Mac 原生 space 已天然不覆盖别 app 全屏。
#[cfg(windows)]
mod fullscreen;
#[cfg(windows)]
mod winmax;
mod logkeep;
mod nativelog;
mod webrender;

use larkwing_core::engine::Engine;
use larkwing_core::scenes::Scenes;
use larkwing_core::store::Store;
use tauri::tray::{MouseButton, TrayIconBuilder, TrayIconEvent};
use tauri::{Emitter, LogicalSize, Manager, PhysicalPosition, WindowEvent};

use commands::AppState;

/// 保活 tracing 的后台写线程,随 app 生命周期。
struct LogGuard(#[allow(dead_code)] tracing_appender::non_blocking::WorkerGuard);

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
  // rustls 0.23 不再自动选 crypto provider:依赖树里 aws-lc-rs(reqwest 拉)与 ring
  // (msedge-tts→rustls-platform-verifier 拉)同时存在,不在进程级装一个默认 provider,
  // msedge-tts 合成走 ClientConfig::with_platform_verifier()→ClientConfig::builder() 时
  // 会因"两个 feature 都在、无法自动裁决"而 panic(表现:免手唤醒应答音 / 语音回复全静默)。
  // 这里装配最前装一次,所有 rustls 消费方(reqwest 的 get_default + msedge-tts)统一用它。
  let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

  // 终端 CTRL+C 兜底:正常退出走托盘菜单「退出」(§12 托盘锚点;关窗=隐藏)。但从终端起
  // 的开发/调试场景,GUI 事件循环不接管 SIGINT,进程停不掉。ctrlc 用独立线程接管信号
  // (跨平台:Unix SIGINT / Windows CTRL_C_EVENT,后者仅在带控制台时有效),硬退出。
  // release 的 windows_subsystem="windows" 无控制台,本就收不到信号,不影响托盘退出语义。
  let _ = ctrlc::set_handler(|| std::process::exit(0));

  // WebView2 跟 Chromium 自动播放策略:工具触发的播放(IPC 事件)不算用户手势,
  // 不放开会被静音拦截。WKWebView(Mac 开发机)不吃这套,失败兜底 = 播放条点 ▶。
  #[cfg(windows)]
  std::env::set_var(
    "WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS",
    "--autoplay-policy=no-user-gesture-required",
  );

  tauri::Builder::default()
    // 单实例(放最前,让二次启动尽早退出、不做多余装配):已在运行时再点快捷方式 /
    // 重复启动,操作系统会把第二个进程的命令行交给"已在运行的那个实例"的这个回调,
    // 然后第二个进程退出 —— 这里把已运行实例的主窗唤到前台,即"点一下就回到正在跑的程序"。
    // 沿用 --autostart 静默语义(§12 开机自启不弹窗):若这次是自启触发,只确保进程在、
    // 不打扰用户;其余(双击图标等)一律唤主窗。
    .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
      if args.iter().any(|a| a == "--autostart") {
        return;
      }
      show_window(app, "main");
    }))
    .plugin(tauri_plugin_autostart::init(
      tauri_plugin_autostart::MacosLauncher::LaunchAgent,
      Some(vec!["--autostart"]),
    ))
    // 外部链接交系统浏览器:WebView 里 window.open 是 no-op(Win 真机尤甚),
    // 统一走 opener 插件(前端经 backend.openExternal 调 plugin:opener|open_url)。
    .plugin(tauri_plugin_opener::init())
    // 数据目录「搬家」:原生目录选择器(pick_data_folder 命令在 Rust 侧调其 DialogExt)。
    .plugin(tauri_plugin_dialog::init())
    .on_window_event(|window, event| {
      // 关主窗 / 悬浮窗 = 隐藏到托盘,不退进程(PLAN §12;真退出走托盘菜单 quit)。
      // media-login 等其它窗口照常关闭。
      if let WindowEvent::CloseRequested { api, .. } = event {
        let label = window.label();
        if label == "main" || label == "float" {
          api.prevent_close();
          let _ = window.hide();
          // 主窗藏托盘后通知前端暂停动画(透明窗 RAF 不会被自动节流,§8.1 / usePageVisible)。
          // 只为 main 发:否则关悬浮窗会误停主窗动画。
          if label == "main" {
            let _ = window.emit("lw:win-visible", false);
          }
        }
      }
    })
    .setup(|app| {
      if cfg!(debug_assertions) {
        app.handle().plugin(
          tauri_plugin_log::Builder::default()
            .level(log::LevelFilter::Info)
            .build(),
        )?;
      }

      // 一键更新(仅桌面;mobile 不支持该插件)。endpoint/pubkey 走 tauri.conf;代理在前端
      // check() 运行时按用户设置(net.proxy_*)传(updater 不走 net::Client,见 §4.6 张力注解)。
      #[cfg(desktop)]
      app.handle().plugin(tauri_plugin_updater::Builder::new().build())?;

      // ---- 装配:数据目录 / 日志 / store / provider / engine ----
      // 数据目录「搬家」(datadir):锚点 = OS 默认 app_data_dir(永远找得到、住指针);
      // 真实数据根由锚点的 location.json 指针决定。没搬过家 → 用锚点;搬过 → 用记的路径;
      // 路径失效(盘没插)→ 回落锚点 + data_missing,前端弹恢复弹窗(绝不静默重建,§3.5)。
      let anchor = app.path().app_data_dir()?;
      std::fs::create_dir_all(&anchor)?;
      let resolution = larkwing_core::datadir::resolve(&anchor);
      let data_missing = resolution.missing.clone();
      let data_dir = resolution.root.clone();
      std::fs::create_dir_all(&data_dir)?;
      std::fs::create_dir_all(data_dir.join("logs"))?;

      let file_appender = tracing_appender::rolling::daily(data_dir.join("logs"), "larkwing.log");
      let (writer, guard) = tracing_appender::non_blocking(file_appender);
      tracing_subscriber::fmt()
        .with_env_filter(
          tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(writer)
        .with_ansi(false)
        .try_init()
        .ok();
      app.manage(LogGuard(guard));
      logkeep::spawn(data_dir.join("logs")); // 滚动之外的管护:压缩历史日志、清理 30 天前的
      // 原生库(sherpa/ORT/espeak)的真实报错只走 stderr,GUI 子系统下会蒸发 → 落盘 native.log。
      // dev 不启用:终端里 stderr 本来就看得见,别把 panic 从开发者眼前抢走。
      if !cfg!(debug_assertions) {
        nativelog::redirect_stderr(&data_dir.join("logs"), env!("CARGO_PKG_VERSION"));
      }
      if let Some(m) = &data_missing {
        tracing::warn!(missing = %m.display(), root = %data_dir.display(),
          "数据位置失效(盘没插/被删),已回落默认根;前端将提示恢复(退出去插回磁盘 / 恢复默认)");
      }

      // 「从备份恢复」落位:上次运行 restore_data 暂存的负载在开库前换进来(运行中不能覆盖
      // 已打开的 DB)。成功 = 现库已挪成 pre-restore 保险副本;失败 = 老数据仍权威。
      // 结果带给前端 boot 检查弹一句(§3.5 失败绝不静默)。
      let restore_outcome = match larkwing_core::datadir::apply_pending_restore(&data_dir) {
        None => None,
        Some(Ok(())) => {
          tracing::info!("已从备份恢复数据(原数据留有 *.pre-restore-* 保险副本)");
          Some("ok")
        }
        Some(Err(e)) => {
          tracing::error!(err = %e, "从备份恢复失败,继续用原数据");
          Some("failed")
        }
      };

      let store = Store::open(&data_dir.join("larkwing.db"))?;
      store.users.ensure_default_user()?; // 首启零配置
      // 把 settings 里的 legacy 明文密钥迁到 keyring(§6.3;幂等,keyring 不可用则原地留 settings)
      larkwing_core::secrets::migrate(&store.settings);

      // 开机自启:正式版「首启默认开一次」(用户决策 2026-06-17;§7.6 常驻临场强默认)。
      // 关闭入口走设置页开关(已有);这里只在「从未默认过」时落一次产品默认 = ON,之后全交用户。
      // 用 auto-launch 自己的 enable() 写(而非装机时直接写注册表)——保证写入值的格式与
      // is_enabled()/disable() 完全一致,零漂移(§6.8 薄封装防漂移)。
      // dev 版不碰:自启会指向临时调试程序、连不上本地前端(前端开关同样 dev 禁用)。
      //
      // ⚠️ 标记带版本号(.v2,2026-06-19 修升级丢自启 bug):升级走 Tauri NSIS「跑旧版卸载器」流程,
      // 旧版卸载钩子会无条件删自启注册表项;但本标记存数据目录、升级不动用户数据 → 标记残留 →
      // 新版首启误判「已默认过」而跳过 → 自启被删了又不补 = 升级后自启莫名变关(用户实测)。两手根治:
      //   ① installer-hooks.nsh 改「升级不删、真卸载才删」——护住「以后」的升级;
      //   ② 这里把标记键名升一版——让已装机器在「修好版」首启把默认自启「再开一次」,补上升级当跳
      //      被旧卸载器删掉的那次(旧卸载器是被替换的旧版本编进去的,①管不到这一跳)。
      // 升版只重置一次:重开后落 .v2,之后照旧「不与用户手动关掉打架」。旧键 .defaulted 留作惰性垃圾、无害。
      // 仅 Windows 正式版真机能验(§8.1)。
      if !cfg!(debug_assertions) {
        use tauri_plugin_autostart::ManagerExt;
        const AUTOSTART_DEFAULTED: &str = "system.autostart.defaulted.v2";
        let already = store.settings.get(None, AUTOSTART_DEFAULTED).ok().flatten();
        if already.as_deref() != Some("1") {
          match app.autolaunch().enable() {
            Ok(()) => tracing::info!("首启/升级修复:按产品默认开启开机自启"),
            Err(e) => tracing::warn!("默认开启开机自启失败(忽略,不反复试): {e:#}"),
          }
          // 成败都落标记:避免每次启动重试,也不与用户日后手动关掉打架。
          let _ = store.settings.set(None, AUTOSTART_DEFAULTED, "1");
        }
      }

      // 全局事件车道(PLAN §5 预留位启用):core 的 Bus → Tauri 全局事件 "app_event"。
      // 回合内事件仍走 send_message 的 Channel,互不相干。
      let bus = larkwing_core::bus::Bus::new();
      let media = larkwing_core::media::MediaRuntime::new(
        data_dir.join("media"),
        store.clone(),
        bus.clone(),
      );

      // 语音运行时(PLAN §11):听写/唤醒能力 + 模型用时下载;只供能力不碰 engine。
      // 场景数据两处共用(engine 拼上下文 / voice 取唤醒话术——人格数据)。
      let scenes = Scenes::builtin();
      let voice = larkwing_core::voice::VoiceRuntime::new(
        data_dir.join("voice"),
        store.clone(),
        bus.clone(),
        scenes.clone(),
      );

      // 供应商解析(env key / llm.providers / 单 key 兜底)收口在 engine.reload_providers
      let engine = Engine::with_media(store, scenes, media.clone());
      engine.reload_providers()?;
      // 网页渲染器注入(webrender 接缝):web_render 工具的机器件 —— 隐藏 WebView 窗
      // 当 JS 渲染器(app 自己就是浏览器);core 侧没注入时该工具如实说没有渲染组件。
      engine.set_web_renderer(std::sync::Arc::new(webrender::ShellWebRenderer::new(
        app.handle().clone(),
        media.clone(),
      )));
      // 确认中枢注入 voice(§7.8 口头确认):同一份实例,语音听音的 resolve 与桌面卡/
      // 渠道回话先到先得(voice 不碰 engine,只吃这个平级 confirm 件——set_web_renderer 同款接缝)。
      voice.set_confirmer(engine.confirmer().clone());
      tracing::info!(
        data_dir = %data_dir.display(),
        has_key = engine.has_provider(),
        "larkwing 装配完成"
      );
      // 任务调度器(提醒/定时):常驻轮询循环,真相在 jobs 表
      tauri::async_runtime::spawn(larkwing_core::scheduler::run(engine.clone()));
      // 免手唤醒开机自启(设置开着才会真启动;失败只记日志不挡开机)
      let voice_boot = voice.clone();
      tauri::async_runtime::spawn(async move { voice_boot.boot_wake_if_enabled().await });
      // 远程渠道(Telegram/钉钉):shell-side 监督器,boot 起启用项(读 settings 决定起哪些)。
      // voice/media 只为手机语音消息转写(本地 ASR + ffmpeg 解码)——装配在壳层,core 内不互持。
      let channels = commands::ChannelSup::new(engine.clone(), voice.clone(), media.clone());
      channels.restart();
      app.manage(AppState {
        engine,
        media,
        voice,
        channels,
        data_root: data_dir.clone(),
        anchor: anchor.clone(),
        bus: bus.clone(),
        data_missing,
        restore_outcome,
      });

      let forward = app.handle().clone();
      let mut bus_rx = bus.subscribe();
      tauri::async_runtime::spawn(async move {
        use tokio::sync::broadcast::error::RecvError;
        loop {
          match bus_rx.recv().await {
            Ok(ev) => {
              let _ = forward.emit("app_event", &ev);
            }
            Err(RecvError::Lagged(n)) => {
              // 事件都是全量快照,丢几条无碍 —— 下一条把状态追平
              tracing::debug!(missed = n, "app_event 转发滞后");
            }
            Err(RecvError::Closed) => break,
          }
        }
      });

      // ---- 窗口:落在用户当前所在的显示器并居中 ----
      if let Some(win) = app.get_webview_window("main") {
        // 选"用户当前所在"的显示器:优先光标所在屏 → 主显示器 → 窗口当前屏。
        // 不能用 current_monitor 直接定,否则系统把窗口初始丢在哪块屏就用哪块(常跑到副屏)。
        let monitor = app
          .cursor_position()
          .ok()
          .and_then(|p| {
            app.available_monitors().ok().and_then(|list| {
              list.into_iter().find(|m| {
                let mp = m.position();
                let ms = m.size();
                p.x >= mp.x as f64
                  && p.x < mp.x as f64 + ms.width as f64
                  && p.y >= mp.y as f64
                  && p.y < mp.y as f64 + ms.height as f64
              })
            })
          })
          .or_else(|| app.primary_monitor().ok().flatten())
          .or_else(|| win.current_monitor().ok().flatten());

        if let Some(mon) = monitor {
          let scale = mon.scale_factor();
          let msz = mon.size();
          let mpos = mon.position();
          let lw = msz.width as f64 / scale; // 逻辑像素
          let lh = msz.height as f64 / scale;
          let w = (lw * 0.60).clamp(760.0, 1360.0);
          let h = (lh * 0.62).clamp(560.0, 1020.0);
          let _ = win.set_size(LogicalSize::new(w, h));

          // 居中到这块屏(物理坐标,跨屏才准)
          let pw = w * scale;
          let ph = h * scale;
          let x = mpos.x as f64 + (msz.width as f64 - pw) / 2.0;
          let y = mpos.y as f64 + (msz.height as f64 - ph) / 2.0;
          let _ = win.set_position(PhysicalPosition::new(x, y));
        }

        // Windows:无边框窗最大化会盖任务栏 + 任务栏点击失灵(Tauri #7103,上游未修)
        // → 挂 WM_GETMINMAXINFO subclass 把最大化尺寸钉到工作区(winmax.rs;只能 Windows 真机验 §8.1)。
        #[cfg(windows)]
        if let Ok(hwnd) = win.hwnd() {
          winmax::fix_maximize(hwnd.0 as isize);
        }
      }

      // ---- 系统托盘(PLAN §12 常驻锚点):左键唤主窗;菜单文案由前端 set_tray_menu
      //      注入(§6 core 不产文案),这里只建图标 + 交互 ----
      // 托盘图标分平台(刻意区别):
      //   macOS 菜单栏惯例 = 单色字形 + 模板模式(按明暗自动染色),tray.png 是白色字形;
      //   Windows/Linux 通知区惯例 = 彩色小图标,用彩色翅膀 tray-color.png(更像"我们的 logo")。
      #[cfg(target_os = "macos")]
      let tray = TrayIconBuilder::with_id("tray")
        .icon(tauri::include_image!("icons/tray.png"))
        .icon_as_template(true);
      #[cfg(not(target_os = "macos"))]
      let tray = TrayIconBuilder::with_id("tray")
        .icon(tauri::include_image!("icons/tray-color.png"));
      let tray = tray
        .tooltip("Larkwing")
        .show_menu_on_left_click(false)
        .on_tray_icon_event(|tray, event| {
          // 放宽:不限 button_state(mac 上 Up/Down 行为与 Win 不同),任何左键点击都唤主窗
          if let TrayIconEvent::Click { button: MouseButton::Left, .. } = event {
            show_window(tray.app_handle(), "main");
          }
        })
        .on_menu_event(|app, event| match event.id.as_ref() {
          "open" => show_window(app, "main"),
          // 重开悬浮窗:交给主窗(它持 ui.float.enabled 真相 + 显隐策略),发事件让它置位 + show。
          // 主窗哪怕藏在托盘里 JS 仍活着,收得到。
          "show_float" => {
            let _ = app.emit("lw:show-float", ());
          }
          "quit" => app.exit(0),
          _ => {}
        })
        .build(app)?;
      app.manage(tray);

      // 悬浮窗初始落右下角(visible:false;显隐 / 展开尺寸由前端按 §12 规则控制)
      if let Some(f) = app.get_webview_window("float") {
        if let (Ok(Some(mon)), Ok(fsz)) = (f.current_monitor(), f.outer_size()) {
          let mpos = mon.position();
          let msz = mon.size();
          let margin = (24.0 * mon.scale_factor()) as i32;
          let x = mpos.x + msz.width as i32 - fsz.width as i32 - margin;
          let y = mpos.y + msz.height as i32 - fsz.height as i32 - margin * 2;
          let _ = f.set_position(PhysicalPosition::new(x, y));
        }
      }

      // 别的程序全屏 → 悬浮窗让位(仅 Windows;Mac 原生 space 已天然不覆盖别 app 全屏):
      // 固定间隔轮询前台是否全屏(几次 Win32 调用,1.5s 一跳 ≈ 0 CPU,不碰 §8.1 的 RAF 空烧坑),
      // 只在「全屏态真变化」时把事实推给主窗 —— 显隐决策仍归主窗 JS(applyFloat,与 lw:show-float 同构)。
      #[cfg(windows)]
      {
        let handle = app.handle().clone();
        std::thread::spawn(move || {
          let mut last: Option<bool> = None;
          loop {
            std::thread::sleep(std::time::Duration::from_millis(1500));
            let fs = fullscreen::foreground_fullscreen();
            if last != Some(fs) {
              last = Some(fs);
              let _ = handle.emit("lw:foreground-fullscreen", fs);
            }
          }
        });
      }

      // 开机静默启动(--autostart,PLAN §12):不弹主窗,只留托盘(+ 悬浮窗按前端 enabled)
      if std::env::args().any(|a| a == "--autostart") {
        if let Some(win) = app.get_webview_window("main") {
          let _ = win.hide();
          // 权威告知前端「此刻隐藏」(与 show_window 的 true 成对)。不发的话前端只能靠
          // document.hidden(透明窗会误报可见)+ 异步 isHidden 竞态判断初值,一旦误判可见就会在隐藏
          // 窗里排下永不触发的 rAF、之后 show 不再翻转 → 动画冻住(开机自启 bug 根因之一)。
          let _ = win.emit("lw:win-visible", false);
        }
      }

      Ok(())
    })
    .invoke_handler(tauri::generate_handler![
      commands::boot,
      commands::send_message,
      commands::send_overheard,
      commands::inject_message,
      commands::cancel_generation,
      commands::new_conversation,
      commands::list_conversations,
      commands::load_conversation,
      commands::delete_conversation,
      commands::rename_conversation,
      commands::set_conversation_pinned,
      commands::set_api_key,
      commands::usage_today,
      commands::usage_conversation,
      commands::conversation_stats,
      commands::conversation_trace,
      commands::float_idle,
      commands::llm_balance,
      commands::set_skin,
      commands::skin,
      commands::list_settings,
      commands::set_setting,
      commands::ensure_app_keypair,
      commands::rename_user,
      commands::list_providers,
      commands::save_provider,
      commands::remove_provider,
      commands::model_meta,
      commands::set_model_override,
      commands::media_login,
      commands::media_retry,
      commands::retry_download,
      commands::retry_voice_model,
      commands::media_advance,
      commands::media_auto_next,
      commands::media_mode,
      commands::report_media_state,
      commands::media_log,
      commands::media_replay_compat,
      commands::attachment_url,
      commands::remote_status,
      commands::reload_channels,
      commands::weixin_login_start,
      commands::weixin_login_poll,
      commands::weixin_accounts,
      commands::weixin_unbind,
      commands::voice_listen_start,
      commands::voice_listen_stop,
      commands::voice_status,
      commands::voice_wake_set,
      commands::voice_follow_up,
      commands::voice_refresh_prompts,
      commands::voice_wake_resume,
      commands::voice_confirm_listen,
      commands::voice_wake_suspend,
      commands::voice_push_audio,
      commands::voice_calibrate_wake,
      commands::voice_calibrate_cancel,
      commands::list_family,
      commands::add_family,
      commands::remove_family,
      commands::rename_family,
      commands::list_channel_chats,
      commands::bind_channel_chat,
      commands::voice_enroll,
      commands::voice_unenroll,
      commands::tts_synthesize,
      commands::voice_preview,
      commands::list_voice_clones,
      commands::voice_clone_record,
      commands::voice_clone_import,
      commands::voice_clone_save,
      commands::rename_voice_clone,
      commands::delete_voice_clone,
      commands::list_memories,
      commands::delete_memory,
      commands::memory_maintenance_log,
      commands::list_briefings,
      commands::delete_briefing,
      commands::list_todos,
      commands::finish_todo,
      commands::list_diary,
      commands::delete_diary,
      commands::list_reminders,
      commands::cancel_reminder,
      commands::list_fsops,
      commands::fsops_undo,
      commands::fsops_redo,
      commands::confirm_action,
      commands::list_confirms,
      commands::autostart_enabled,
      commands::set_autostart,
      commands::set_tray_menu,
      commands::quit_app,
      commands::relaunch_app,
      commands::data_location,
      commands::pick_data_folder,
      commands::relocate_precheck,
      commands::relocate_data,
      commands::cleanup_old_data,
      commands::keep_old_data,
      commands::data_reset_to_default,
      commands::reveal_data_dir,
      commands::backup_data,
      commands::pick_backup_file,
      commands::restore_precheck,
      commands::restore_data,
      commands::search_messages,
    ])
    .run(tauri::generate_context!())
    .expect("error while running tauri application");
}

/// 唤出 / 聚焦指定窗口(托盘左键、菜单"打开"共用)。
fn show_window(app: &tauri::AppHandle, label: &str) {
  // mac:窗口 hide 到托盘后 app 可能在后台,先取消隐藏 app,否则点托盘 show 窗口无反应
  #[cfg(target_os = "macos")]
  let _ = app.show();
  if let Some(win) = app.get_webview_window(label) {
    let _ = win.show();
    let _ = win.unminimize();
    let _ = win.set_focus();
    // 重新可见 → 通知前端恢复动画(与 CloseRequested 的暂停成对)
    if label == "main" {
      let _ = win.emit("lw:win-visible", true);
    }
  }
}
