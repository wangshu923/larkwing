//! 壳:只做装配(开库 → 解析 key → 造 provider/Engine → 注册 commands)和窗口,不写业务。

mod commands;
mod logkeep;

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
    .plugin(tauri_plugin_autostart::init(
      tauri_plugin_autostart::MacosLauncher::LaunchAgent,
      Some(vec!["--autostart"]),
    ))
    // 外部链接交系统浏览器:WebView 里 window.open 是 no-op(Win 真机尤甚),
    // 统一走 opener 插件(前端经 backend.openExternal 调 plugin:opener|open_url)。
    .plugin(tauri_plugin_opener::init())
    .on_window_event(|window, event| {
      // 关主窗 / 悬浮窗 = 隐藏到托盘,不退进程(PLAN §12;真退出走托盘菜单 quit)。
      // media-login 等其它窗口照常关闭。
      if let WindowEvent::CloseRequested { api, .. } = event {
        let label = window.label();
        if label == "main" || label == "float" {
          api.prevent_close();
          let _ = window.hide();
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

      // ---- 装配:数据目录 / 日志 / store / provider / engine ----
      let data_dir = app.path().app_data_dir()?;
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

      let store = Store::open(&data_dir.join("larkwing.db"))?;
      store.users.ensure_default_user()?; // 首启零配置

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
      app.manage(AppState { engine, media, voice });

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
      }

      // ---- 系统托盘(PLAN §12 常驻锚点):左键唤主窗;菜单文案由前端 set_tray_menu
      //      注入(§6 core 不产文案),这里只建图标 + 交互 ----
      let tray = TrayIconBuilder::with_id("tray")
        // 托盘用专属单色字形(非整块应用图标);macOS 模板模式按菜单栏明暗自动染色,
        // 与原生托盘观感一致。Windows/Linux 忽略模板,直接显示白色字形(深色托盘可见)。
        .icon(tauri::include_image!("icons/tray.png"))
        .icon_as_template(true)
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

      // 开机静默启动(--autostart,PLAN §12):不弹主窗,只留托盘(+ 悬浮窗按前端 enabled)
      if std::env::args().any(|a| a == "--autostart") {
        if let Some(win) = app.get_webview_window("main") {
          let _ = win.hide();
        }
      }

      Ok(())
    })
    .invoke_handler(tauri::generate_handler![
      commands::boot,
      commands::send_message,
      commands::inject_message,
      commands::cancel_generation,
      commands::new_conversation,
      commands::list_conversations,
      commands::load_conversation,
      commands::delete_conversation,
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
      commands::media_login,
      commands::voice_listen_start,
      commands::voice_listen_stop,
      commands::voice_status,
      commands::voice_wake_set,
      commands::voice_follow_up,
      commands::voice_refresh_prompts,
      commands::voice_wake_resume,
      commands::voice_wake_suspend,
      commands::voice_calibrate_wake,
      commands::voice_calibrate_cancel,
      commands::list_family,
      commands::add_family,
      commands::remove_family,
      commands::voice_enroll,
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
      commands::list_briefings,
      commands::delete_briefing,
      commands::list_reminders,
      commands::cancel_reminder,
      commands::list_fsops,
      commands::fsops_undo,
      commands::fsops_redo,
      commands::autostart_enabled,
      commands::set_autostart,
      commands::set_tray_menu,
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
  }
}
