//! 边界薄层:只做翻译和转发,不写业务(PLAN §5)。
//! command 面就是前端能做的全集;错误统一 AppError { kind, message }。

use std::sync::Arc;

use tauri::ipc::Channel;
use tauri::{Manager, State};

use larkwing_core::engine::{
    AppError, BootSnapshot, DayUsage, Engine, FloatIdle, MsgStats, ProviderPatch, ProviderView,
    SettingEntry, TurnEvent,
};
use larkwing_core::llm::AccountBalance;
use larkwing_core::media::{CookieRec, MediaRuntime};
use larkwing_core::store::{Briefing, Conversation, Memory, Message, UsageTotals, User};
use larkwing_core::voice::{FamilyMember, VoiceRuntime, VoiceStatus};

pub struct AppState {
    pub engine: Arc<Engine>,
    pub media: MediaRuntime,
    pub voice: VoiceRuntime,
}

/// §7「开窗秒显」:一个 IPC 来回画出首屏。
#[tauri::command]
pub fn boot(state: State<'_, AppState>) -> Result<BootSnapshot, AppError> {
    state.engine.boot()
}

/// 流式走 Tauri v2 Channel(按调用隔离,不用全局事件广播)。
/// command 立即返回;TurnEvent 持续推送直到 Done/Failed/Cancelled。
/// meta = 输入形态(语音会话模式,PLAN §11):省略 = 打字默认形。
#[tauri::command]
pub async fn send_message(
    state: State<'_, AppState>,
    conv_id: i64,
    text: String,
    meta: Option<larkwing_core::engine::UserMeta>,
    on_event: Channel<TurnEvent>,
) -> Result<(), AppError> {
    let mut rx = state.engine.send_message(conv_id, text, meta).await?;
    tauri::async_runtime::spawn(async move {
        while let Some(ev) = rx.recv().await {
            // 前端不听了(窗口刷新等):drop rx 即可,落库由 engine 侧保证
            if on_event.send(ev).is_err() {
                break;
            }
        }
    });
    Ok(())
}

/// 停止按钮;幂等。
#[tauri::command]
pub async fn cancel_generation(state: State<'_, AppState>, conv_id: i64) -> Result<(), AppError> {
    state.engine.cancel(conv_id).await;
    Ok(())
}

#[tauri::command]
pub fn new_conversation(state: State<'_, AppState>) -> Result<Conversation, AppError> {
    state.engine.new_conversation()
}

#[tauri::command]
pub fn list_conversations(state: State<'_, AppState>) -> Result<Vec<Conversation>, AppError> {
    state.engine.list_conversations()
}

#[tauri::command]
pub fn load_conversation(
    state: State<'_, AppState>,
    conv_id: i64,
) -> Result<Vec<Message>, AppError> {
    state.engine.load_conversation(conv_id)
}

/// 先取消在飞 → 级联删消息 → 清会话槽。
#[tauri::command]
pub async fn delete_conversation(
    state: State<'_, AppState>,
    conv_id: i64,
) -> Result<(), AppError> {
    state.engine.delete_conversation(conv_id).await
}

#[tauri::command]
pub fn set_api_key(state: State<'_, AppState>, key: String) -> Result<(), AppError> {
    state.engine.set_api_key(&key)
}

#[tauri::command]
pub fn set_skin(state: State<'_, AppState>, skin_id: String) -> Result<(), AppError> {
    state.engine.set_skin(&skin_id)
}

#[tauri::command]
pub fn list_settings(state: State<'_, AppState>) -> Result<Vec<SettingEntry>, AppError> {
    state.engine.list_settings()
}

#[tauri::command]
pub fn set_setting(state: State<'_, AppState>, key: String, value: String) -> Result<(), AppError> {
    state.engine.set_setting(&key, &value)
}

#[tauri::command]
pub fn rename_user(state: State<'_, AppState>, name: String) -> Result<User, AppError> {
    state.engine.rename_user(&name)
}

/// 回忆页:小本本全量(当前用户)。
#[tauri::command]
pub fn list_memories(state: State<'_, AppState>) -> Result<Vec<Memory>, AppError> {
    state.engine.list_memories()
}

#[tauri::command]
pub fn delete_memory(state: State<'_, AppState>, id: i64) -> Result<(), AppError> {
    state.engine.delete_memory(id)
}

/// 回忆页「家里的事」分组:家庭备忘(任务需知)。
#[tauri::command]
pub fn list_briefings(state: State<'_, AppState>) -> Result<Vec<Briefing>, AppError> {
    state.engine.list_briefings()
}

#[tauri::command]
pub fn delete_briefing(state: State<'_, AppState>, id: i64) -> Result<(), AppError> {
    state.engine.delete_briefing(id)
}

/// 灯带初值:今日 token/费用累计(此后的增量走 TurnEvent::Usage)。
#[tauri::command]
pub fn usage_today(state: State<'_, AppState>) -> Result<DayUsage, AppError> {
    Ok(state.engine.usage_today())
}

/// 灯带"话题"段初值:当前会话累计(开机/切话题时取;此后随 TurnEvent::Usage 推送)。
#[tauri::command]
pub fn usage_conversation(
    state: State<'_, AppState>,
    conv_id: i64,
) -> Result<UsageTotals, AppError> {
    Ok(state.engine.usage_conversation(conv_id))
}

/// 历史/提醒气泡的 hover 读数(PLAN §11 D):load 会话后回填,让自启回合也能看读数。
#[tauri::command]
pub fn conversation_stats(
    state: State<'_, AppState>,
    conv_id: i64,
) -> Result<Vec<MsgStats>, AppError> {
    state.engine.conversation_stats(conv_id)
}

/// 悬浮窗待机轮播数据(PLAN §12):下个提醒 + 最近一句(只读;余额/今日花费复用现成命令)。
#[tauri::command]
pub fn float_idle(state: State<'_, AppState>) -> Result<FloatIdle, AppError> {
    state.engine.float_idle()
}

/// 主选供应商的账户余额;null = 不支持/查不到(锦上添花,永不报错)。
#[tauri::command]
pub async fn llm_balance(state: State<'_, AppState>) -> Result<Option<AccountBalance>, AppError> {
    Ok(state.engine.llm_balance().await)
}

#[tauri::command]
pub fn list_providers(state: State<'_, AppState>) -> Result<Vec<ProviderView>, AppError> {
    state.engine.list_providers()
}

#[tauri::command]
pub fn save_provider(
    state: State<'_, AppState>,
    patch: ProviderPatch,
) -> Result<Vec<ProviderView>, AppError> {
    state.engine.save_provider(patch)
}

#[tauri::command]
pub fn remove_provider(
    state: State<'_, AppState>,
    id: String,
) -> Result<Vec<ProviderView>, AppError> {
    state.engine.remove_provider(&id)
}

/// 开听写会话(PLAN §11 A 期):立即返回,进展全走 app_event 的 Voice 车道
/// (Preparing→Listening→…→Transcribed/ListenEnded)。首次使用会触发模型用时下载。
#[tauri::command]
pub fn voice_listen_start(state: State<'_, AppState>) -> Result<(), AppError> {
    let voice = state.voice.clone();
    tauri::async_runtime::spawn(async move {
        // 错误已在 runtime 内部翻译成 ListenEnded{error} 事件,这里只兜日志
        if let Err(e) = voice.listen_start().await {
            tracing::error!(err = %format!("{e:#}"), "voice_listen_start 失败");
        }
    });
    Ok(())
}

/// 停止听写:accept = 立即定稿(已听到的送识别);false = 取消丢弃。幂等。
#[tauri::command]
pub fn voice_listen_stop(state: State<'_, AppState>, accept: bool) -> Result<(), AppError> {
    state.voice.listen_stop(accept);
    Ok(())
}

/// 设置页「语音组件」状态行 + 麦克风设备列表(不触发下载)。
#[tauri::command]
pub fn voice_status(state: State<'_, AppState>) -> Result<VoiceStatus, AppError> {
    Ok(state.voice.status())
}

/// 免手唤醒开关(PLAN §11 C):写设置 + 起停一体(首次开会下 KWS 模型 + 预合成应答音)。
/// 返回最新状态(wake_running 是事实,settings 只是意向)。
#[tauri::command]
pub async fn voice_wake_set(
    state: State<'_, AppState>,
    enabled: bool,
) -> Result<VoiceStatus, AppError> {
    state.voice.wake_set(enabled).await.map_err(AppError::internal)?;
    Ok(state.voice.status())
}

/// 前端编排指令:唤醒回合念完 → 开 6s 跟进窗(免唤醒接话)。
#[tauri::command]
pub fn voice_follow_up(state: State<'_, AppState>) -> Result<(), AppError> {
    state.voice.wake_follow_up();
    Ok(())
}

/// 前端编排指令:唤醒回合失败/取消/被忽略 → 直接回待唤醒。
#[tauri::command]
pub fn voice_wake_resume(state: State<'_, AppState>) -> Result<(), AppError> {
    state.voice.wake_resume();
    Ok(())
}

/// 自激防护:TTS 在念(含重听)时唤醒循环丢帧。
#[tauri::command]
pub fn voice_wake_suspend(state: State<'_, AppState>, on: bool) -> Result<(), AppError> {
    state.voice.wake_suspend(on);
    Ok(())
}

// ---- 家人 / 声纹(PLAN §11 D;多用户落地) ----

/// 家人列表(含"是否已录声纹"标记)。
#[tauri::command]
pub fn list_family(state: State<'_, AppState>) -> Result<Vec<FamilyMember>, AppError> {
    let users = state.engine.list_users()?;
    Ok(users.into_iter().map(|(u, enrolled)| FamilyMember { user: u, enrolled }).collect())
}

/// 添加家人。
#[tauri::command]
pub fn add_family(state: State<'_, AppState>, name: String) -> Result<User, AppError> {
    state.engine.create_user(&name)
}

/// 删除家人(守住至少留一人;记忆/声纹随人走)。
#[tauri::command]
pub fn remove_family(state: State<'_, AppState>, id: i64) -> Result<(), AppError> {
    state.engine.delete_user(id)
}

/// 给某家人录声纹:立即返回,录音/识别进展走 app_event 的 voice 车道
/// (Listening→Idle);完成或失败由前端据 voice 事件 + 重新拉 list_family 反映。
#[tauri::command]
pub fn voice_enroll(state: State<'_, AppState>, user_id: i64) -> Result<(), AppError> {
    let voice = state.voice.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(e) = voice.enroll(user_id).await {
            tracing::error!(err = %format!("{e:#}"), "声纹注册失败");
        }
    });
    Ok(())
}

/// 句级 TTS(PLAN §11 B 期):合成进缓存(命中秒回)→ relay 注册 → 返回可挂
/// `<audio>` 的 localhost URL。切句/编排在前端(useSpeech),这里只管单句。
#[tauri::command]
pub async fn tts_synthesize(state: State<'_, AppState>, text: String) -> Result<String, AppError> {
    let path = state.voice.tts_to_file(&text).await.map_err(AppError::internal)?;
    state.media.file_url(path).await.map_err(AppError::internal)
}

/// 设置页音色试听:句子由前端字典传入(core 不产文案,先例 = media_login title)。
#[tauri::command]
pub async fn voice_preview(
    state: State<'_, AppState>,
    speaker: String,
    text: String,
) -> Result<String, AppError> {
    let path = state.voice.preview(&speaker, &text).await.map_err(AppError::internal)?;
    state.media.file_url(path).await.map_err(AppError::internal)
}

/// 扫码登录:开一扇加载站点登录页的窗口,轮询原生 CookieManager(SESSDATA 是
/// HttpOnly,JS 拿不到,必须走原生),扫码成功 → cookie 入库 → 自动关窗。
/// title 由前端字典传入(文案唯一产地在前端;原生窗口标题没法事后翻译)。
#[tauri::command]
pub async fn media_login(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    source: String,
    title: String,
) -> Result<(), AppError> {
    const LABEL: &str = "media-login";
    let spec = state
        .media
        .login_spec(&source)
        .ok_or_else(|| AppError::internal(format!("未知媒体源 {source}")))?;
    if let Some(win) = app.get_webview_window(LABEL) {
        let _ = win.set_focus(); // 已开着 = 聚焦,不重复开
        return Ok(());
    }
    let login_url: tauri::Url = spec.login_url.parse().map_err(AppError::internal)?;
    let cookie_url: tauri::Url = spec.cookie_url.parse().map_err(AppError::internal)?;
    tauri::WebviewWindowBuilder::new(&app, LABEL, tauri::WebviewUrl::External(login_url))
        .title(title)
        .inner_size(460.0, 640.0)
        .build()
        .map_err(AppError::internal)?;

    // cookie 域兜底:host 去掉 www. 前缀加点(原生 API 偶尔不回 domain 字段)
    let fallback_domain = cookie_url
        .host_str()
        .map(|h| format!(".{}", h.trim_start_matches("www.")))
        .unwrap_or_default();
    let media = state.media.clone();
    tauri::async_runtime::spawn(async move {
        for _ in 0..200 {
            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
            // 用户手动关窗 = 放弃登录,轮询随之收摊
            let Some(win) = app.get_webview_window(LABEL) else { return };
            let cookies = match win.cookies_for_url(cookie_url.clone()) {
                Ok(c) => c,
                Err(e) => {
                    tracing::debug!("登录 cookie 轮询失败: {e}");
                    continue;
                }
            };
            let logged_in = cookies
                .iter()
                .any(|c| c.name() == spec.login_cookie && !c.value().trim().is_empty());
            if !logged_in {
                continue;
            }
            let recs: Vec<CookieRec> = cookies
                .iter()
                .map(|c| CookieRec {
                    name: c.name().to_string(),
                    value: c.value().to_string(),
                    domain: c.domain().map(str::to_string).unwrap_or_else(|| fallback_domain.clone()),
                    path: c.path().unwrap_or("/").to_string(),
                })
                .collect();
            if let Err(e) = media.set_cookies(&spec.source, recs) {
                tracing::error!("登录态入库失败: {e:#}");
            }
            let _ = win.close();
            return;
        }
        // 5 分钟没扫:关窗收摊(可以再点一次重来)
        if let Some(win) = app.get_webview_window(LABEL) {
            let _ = win.close();
        }
    });
    Ok(())
}

// ---- 开机启动 + 托盘菜单(PLAN §12 常驻临场) ----

use tauri_plugin_autostart::ManagerExt;

/// 当前是否已设开机自启(读 OS:注册表 / 登录项 / .desktop;OS 是真相源,不进 DB)。
#[tauri::command]
pub fn autostart_enabled(app: tauri::AppHandle) -> Result<bool, AppError> {
    app.autolaunch().is_enabled().map_err(AppError::internal)
}

/// 设 / 撤开机自启(各平台差异由插件兜)。
#[tauri::command]
pub fn set_autostart(app: tauri::AppHandle, on: bool) -> Result<(), AppError> {
    let mgr = app.autolaunch();
    if on { mgr.enable() } else { mgr.disable() }.map_err(AppError::internal)
}

/// 托盘菜单文案注入(§6 core 不产文案):前端 boot 后把字典文案传进来建菜单。
/// setup 只建图标 + 左键唤主窗;菜单(打开 / 退出)等这里来。
#[tauri::command]
pub fn set_tray_menu(app: tauri::AppHandle, open: String, quit: String) -> Result<(), AppError> {
    use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
    let tray = app.tray_by_id("tray").ok_or_else(|| AppError::internal("托盘未就绪"))?;
    let menu = Menu::with_items(
        &app,
        &[
            &MenuItem::with_id(&app, "open", open, true, None::<&str>).map_err(AppError::internal)?,
            &PredefinedMenuItem::separator(&app).map_err(AppError::internal)?,
            &MenuItem::with_id(&app, "quit", quit, true, None::<&str>).map_err(AppError::internal)?,
        ],
    )
    .map_err(AppError::internal)?;
    tray.set_menu(Some(menu)).map_err(AppError::internal)?;
    Ok(())
}
