//! 边界薄层:只做翻译和转发,不写业务(PLAN §5)。
//! command 面就是前端能做的全集;错误统一 AppError { kind, message }。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tauri::ipc::Channel;
use tauri::{Manager, State};
use tokio_util::sync::CancellationToken;

use larkwing_core::bus::{Bus, Text};
use larkwing_core::channels::{self, ChannelState, ChannelStatus};
use larkwing_core::datadir::{self, Pointer};
use larkwing_core::tasks::Tasks;

use larkwing_core::engine::{
    AppError, BootSnapshot, DayUsage, Engine, FloatIdle, ModelMeta, MsgStats, ProviderPatch,
    ProviderView, SettingEntry, TurnEvent,
};
use larkwing_core::llm::catalog::ModelOverride;
use larkwing_core::llm::AccountBalance;
use larkwing_core::media::{CookieRec, MediaRuntime};
use larkwing_core::store::{
    Briefing, ClonedVoice, Conversation, FsOpRow, Memory, Message, SearchHit, UsageTotals, User,
};
use larkwing_core::voice::{FamilyMember, VoiceRuntime, VoiceStatus};

pub struct AppState {
    pub engine: Arc<Engine>,
    pub media: MediaRuntime,
    pub voice: VoiceRuntime,
    pub channels: ChannelSup,
    /// 当前生效的数据根(boot 时由 datadir 指针解析得到)。
    pub data_root: PathBuf,
    /// 锚点 = OS 默认 app_data_dir(住指针 location.json;搬家命令据此读写指针)。
    pub anchor: PathBuf,
    /// 事件总线(搬家进度起一个临时 Tasks 推 HUD)。
    pub bus: Bus,
    /// boot 时数据位置失效(盘没插/被删)的记录:Some=前端弹恢复弹窗(§3.5 不静默)。
    pub data_missing: Option<PathBuf>,
    /// boot 时「从备份恢复」落位结果:Some("ok"/"failed") = 本次启动应用过恢复负载,
    /// 前端 boot 检查据此弹一句结果提示(§3.5 失败绝不静默);None = 无事。
    pub restore_outcome: Option<&'static str>,
}

/// 远程渠道的 shell-side 监督器(§6.1:停旧起新的编排在壳层,顶层 spawn 用 tauri runtime;
/// core 的 channels::run 不依赖 tauri)。boot 与"保存配置后"各 restart 一次。
pub struct ChannelSup {
    engine: Arc<Engine>,
    voice: larkwing_core::voice::VoiceRuntime,
    media: larkwing_core::media::MediaRuntime,
    status: ChannelStatus,
    ct: Mutex<CancellationToken>,
}

impl ChannelSup {
    pub fn new(
        engine: Arc<Engine>,
        voice: larkwing_core::voice::VoiceRuntime,
        media: larkwing_core::media::MediaRuntime,
    ) -> ChannelSup {
        ChannelSup {
            engine,
            voice,
            media,
            status: ChannelStatus::default(),
            ct: Mutex::new(CancellationToken::new()),
        }
    }

    /// 取消当前所有渠道任务,按最新 settings 重新拉起(幂等)。
    pub fn restart(&self) {
        let new_ct = CancellationToken::new();
        let old = {
            let mut g = self.ct.lock().expect("channels ct lock");
            std::mem::replace(&mut *g, new_ct.clone())
        };
        old.cancel();
        if let Ok(mut m) = self.status.lock() {
            m.clear();
        }
        let (engine, status) = (self.engine.clone(), self.status.clone());
        let (voice, media) = (self.voice.clone(), self.media.clone());
        tauri::async_runtime::spawn(async move {
            channels::run(engine, voice, media, status, new_ct).await;
        });
    }

    fn status_snapshot(&self) -> std::collections::HashMap<String, ChannelState> {
        self.status.lock().map(|m| m.clone()).unwrap_or_default()
    }
}

/// §7「开窗秒显」:一个 IPC 来回画出首屏。
#[tauri::command]
pub fn boot(state: State<'_, AppState>) -> Result<BootSnapshot, AppError> {
    state.engine.boot()
}

/// 浏览器采集推流(层1 AEC 采集端):前端 getUserMedia 消完回声的 16k mono **i16 LE** 帧,
/// raw body 免 JSON(~10Hz × 3.2KB)。采集源=cpal 时无 tap 在收,推了也只是丢弃——
/// 源的取舍由 `voice.capture.source` 决定,这条命令只管搬运。
#[tauri::command]
pub fn voice_push_audio(
    state: State<'_, AppState>,
    request: tauri::ipc::Request<'_>,
) -> Result<(), AppError> {
    if let tauri::ipc::InvokeBody::Raw(bytes) = request.body() {
        let pcm: Vec<f32> = bytes
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32_768.0)
            .collect();
        state.voice.push_audio(pcm);
    }
    Ok(())
}

/// 旁听仲裁(唤醒确认层「呼名+续句」):整句交模型判是不是叫它。临时回合、无 Channel
/// (engine 内消费);终态经全局车道 kind=overheard(转正)/ overheard_dismissed(蒸发)。
#[tauri::command]
pub async fn send_overheard(
    state: State<'_, AppState>,
    conv_id: i64,
    text: String,
    speaker: Option<i64>,
) -> Result<(), AppError> {
    state.engine.send_overheard(conv_id, text, speaker).await
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
    attachments: Option<Vec<larkwing_core::engine::InAttachment>>,
    on_event: Channel<TurnEvent>,
) -> Result<(), AppError> {
    let mut rx = state
        .engine
        .send_message(conv_id, text, meta, attachments.unwrap_or_default())
        .await?;
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

/// 插队(PLAN §9 B):把消息塞进正在跑的回合,下一轮 LLM 带上(不打断)。
/// 返回 false = 没在飞 / 回合正收尾,前端改用普通 send 起新回合。
#[tauri::command]
pub async fn inject_message(
    state: State<'_, AppState>,
    conv_id: i64,
    text: String,
    meta: Option<larkwing_core::engine::UserMeta>,
    attachments: Option<Vec<larkwing_core::engine::InAttachment>>,
) -> Result<bool, AppError> {
    Ok(state.engine.inject(conv_id, text, meta, attachments.unwrap_or_default()).await)
}

/// 停止按钮;幂等。
#[tauri::command]
pub async fn cancel_generation(state: State<'_, AppState>, conv_id: i64) -> Result<(), AppError> {
    state.engine.cancel(conv_id).await;
    Ok(())
}

#[tauri::command]
pub fn new_conversation(
    state: State<'_, AppState>,
    channel: Option<String>,
) -> Result<Conversation, AppError> {
    state
        .engine
        .new_conversation(channel.as_deref().unwrap_or(larkwing_core::store::chat::CHANNEL_UI))
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

/// 跨会话搜索聊天记录(子串匹配,排除工具 / 系统事件行)。最近命中在前,封顶 limit。
#[tauri::command]
pub fn search_messages(
    state: State<'_, AppState>,
    query: String,
    limit: i64,
) -> Result<Vec<SearchHit>, AppError> {
    state.engine.search_messages(&query, limit.clamp(1, 200))
}

/// 先取消在飞 → 级联删消息 → 清会话槽。
#[tauri::command]
pub async fn delete_conversation(
    state: State<'_, AppState>,
    conv_id: i64,
) -> Result<(), AppError> {
    state.engine.delete_conversation(conv_id).await
}

/// 用户右键重命名会话。
#[tauri::command]
pub fn rename_conversation(
    state: State<'_, AppState>,
    conv_id: i64,
    title: String,
) -> Result<(), AppError> {
    state.engine.rename_conversation(conv_id, &title)
}

/// 用户右键钉住 / 取消钉住会话。
#[tauri::command]
pub fn set_conversation_pinned(
    state: State<'_, AppState>,
    conv_id: i64,
    pinned: bool,
) -> Result<(), AppError> {
    state.engine.set_conversation_pinned(conv_id, pinned)
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
pub fn skin(state: State<'_, AppState>) -> Result<String, AppError> {
    state.engine.skin()
}

#[tauri::command]
pub fn list_settings(state: State<'_, AppState>) -> Result<Vec<SettingEntry>, AppError> {
    state.engine.list_settings()
}

#[tauri::command]
pub fn set_setting(state: State<'_, AppState>, key: String, value: String) -> Result<(), AppError> {
    state.engine.set_setting(&key, &value)
}

/// 全局应用公钥(Ed25519):没有就生成、有就回存量。前端在服务页展示给用户复制到服务控制台
/// (和风 JWT 等)。私钥永不过桥。
#[tauri::command]
pub fn ensure_app_keypair(state: State<'_, AppState>) -> Result<String, AppError> {
    state.engine.ensure_app_keypair()
}

#[tauri::command]
pub fn rename_user(state: State<'_, AppState>, name: String) -> Result<User, AppError> {
    state.engine.rename_user(&name)
}

/// 回忆页:小本本全量。user_id 省略 = 当前主人;传家人 id = 主人查看 TA 的记忆(§渠道归人第二步)。
#[tauri::command]
pub fn list_memories(
    state: State<'_, AppState>,
    user_id: Option<i64>,
) -> Result<Vec<Memory>, AppError> {
    state.engine.list_memories(user_id)
}

/// 删记忆。user_id 省略 = 当前主人;传家人 id = 主人删 TA 的记忆(主人管理面)。
#[tauri::command]
pub fn delete_memory(
    state: State<'_, AppState>,
    user_id: Option<i64>,
    id: i64,
) -> Result<(), AppError> {
    state.engine.delete_memory(user_id, id)
}

/// 记忆维护流水(§13.7 调阈值用:回看每轮衰减/下沉/升层/合并/硬清了多少)。limit 缺省 50。
#[tauri::command]
pub fn memory_maintenance_log(
    state: State<'_, AppState>,
    limit: Option<i64>,
) -> Result<Vec<larkwing_core::store::MaintenanceLog>, AppError> {
    state.engine.list_memory_maintenance(limit.unwrap_or(50).clamp(1, 500))
}

/// 回忆页「家里的事」分组:家庭备忘(任务需知)。user_id 省略 = 当前主人;传家人 id =
/// TA 视角(home 共享那份对谁都在,个人 scope 跟着切)。
#[tauri::command]
pub fn list_briefings(
    state: State<'_, AppState>,
    user_id: Option<i64>,
) -> Result<Vec<Briefing>, AppError> {
    state.engine.list_briefings(user_id)
}

#[tauri::command]
pub fn delete_briefing(state: State<'_, AppState>, id: i64) -> Result<(), AppError> {
    state.engine.delete_briefing(id)
}

/// 回忆页「没办完的事」分组:开着的待办。user_id 省略 = 当前主人;传家人 id = 主人查看 TA 的。
#[tauri::command]
pub fn list_todos(
    state: State<'_, AppState>,
    user_id: Option<i64>,
) -> Result<Vec<larkwing_core::store::Todo>, AppError> {
    state.engine.list_todos(user_id)
}

/// 回忆页勾掉一件待办(办完 / 不用了)。user_id 语义同上(主人管理面)。
#[tauri::command]
pub fn finish_todo(
    state: State<'_, AppState>,
    user_id: Option<i64>,
    id: i64,
) -> Result<(), AppError> {
    state.engine.finish_todo(user_id, id)
}

/// 回忆页「这些日子」:家庭日记流(日期新→旧)。home 共有一本,不随「看谁的」切换。
#[tauri::command]
pub fn list_diary(
    state: State<'_, AppState>,
) -> Result<Vec<larkwing_core::store::DiaryEntry>, AppError> {
    state.engine.list_diary(120)
}

/// 回忆页右键删掉一天的日记。
#[tauri::command]
pub fn delete_diary(state: State<'_, AppState>, id: i64) -> Result<bool, AppError> {
    state.engine.delete_diary(id)
}

/// 提醒页:当前用户待触发的提醒(定时任务,按时间升序)。
#[tauri::command]
pub fn list_reminders(state: State<'_, AppState>) -> Result<Vec<larkwing_core::engine::ReminderItem>, AppError> {
    state.engine.list_reminders()
}

/// 提醒页「取消」:撤掉一条提醒(按当前用户限定)。
#[tauri::command]
pub fn cancel_reminder(state: State<'_, AppState>, id: i64) -> Result<(), AppError> {
    state.engine.cancel_reminder(id)
}

/// 操作记录页(文件能力,PLAN §9):当前用户最近的文件操作批次(最近在前)。
#[tauri::command]
pub fn list_fsops(state: State<'_, AppState>) -> Result<Vec<FsOpRow>, AppError> {
    state.engine.list_fsops()
}

/// 操作记录页「撤销」:把某批文件操作退回去(功能性,非安全承诺)。
#[tauri::command]
pub fn fsops_undo(state: State<'_, AppState>, id: i64) -> Result<(), AppError> {
    state.engine.fsops_undo(id)
}

/// 操作记录页「重做」:把撤销过的那批再做一遍。
#[tauri::command]
pub fn fsops_redo(state: State<'_, AppState>, id: i64) -> Result<(), AppError> {
    state.engine.fsops_redo(id)
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

/// 历史回放的「想了想」轨迹(PLAN §9 思考漏出):load 会话后回填到代表气泡。
#[tauri::command]
pub fn conversation_trace(
    state: State<'_, AppState>,
    conv_id: i64,
) -> Result<Vec<larkwing_core::engine::TurnTrace>, AppError> {
    state.engine.conversation_trace(conv_id)
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

/// 设置页「高级」:某模型的目录猜测 + 当前用户覆盖。
#[tauri::command]
pub fn model_meta(state: State<'_, AppState>, model: String) -> Result<ModelMeta, AppError> {
    Ok(state.engine.model_meta(&model))
}

/// 设置页「高级」:upsert 一条模型覆盖(空壳 = 删该条)。
#[tauri::command]
pub fn set_model_override(
    state: State<'_, AppState>,
    over: ModelOverride,
) -> Result<(), AppError> {
    state.engine.set_model_override(over)
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

/// 前端编排指令:唤醒回合念完 → 开跟进窗(免唤醒接话;媒体在播 → 3s 短窗,少压音量)。
#[tauri::command]
pub fn voice_follow_up(state: State<'_, AppState>, media_playing: bool) -> Result<(), AppError> {
    state.voice.wake_follow_up(media_playing);
    Ok(())
}

/// 换音色/语速/在线离线档后调用:唤醒在跑就后台重建应答音银行并热替换(不重启唤醒/麦)。
/// 问题1-B:让"它的声音"等设置对唤醒应答音也实时生效。没开唤醒则 no-op。
#[tauri::command]
pub async fn voice_refresh_prompts(state: State<'_, AppState>) -> Result<(), AppError> {
    state.voice.refresh_prompts().await;
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

/// 录音标定唤醒(PLAN §11 后续):立即返回,录音/计算进展走 app_event 的 voice 车道
/// (CalibProgress / State / CalibResult);首次会触发 KWS/ASR/VAD 模型用时下载。
#[tauri::command]
pub fn voice_calibrate_wake(state: State<'_, AppState>) -> Result<(), AppError> {
    let voice = state.voice.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(e) = voice.calibrate_wake().await {
            tracing::error!(err = %format!("{e:#}"), "唤醒标定失败");
        }
    });
    Ok(())
}

/// 取消进行中的唤醒标定(幂等)。
#[tauri::command]
pub fn voice_calibrate_cancel(state: State<'_, AppState>) -> Result<(), AppError> {
    state.voice.calibrate_cancel();
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

/// 删除家人(守住至少留一人;记忆/声纹/渠道指认随人走)。
#[tauri::command]
pub fn remove_family(state: State<'_, AppState>, id: i64) -> Result<(), AppError> {
    state.engine.delete_user(id)
}

/// 给某家人改名(家人卡片行内改;rename_user 改的是默认用户,这条按 id)。
#[tauri::command]
pub fn rename_family(state: State<'_, AppState>, id: i64, name: String) -> Result<(), AppError> {
    state.engine.rename_family(id, &name)
}

/// 渠道对话列表(家人页「远程对话」区:哪条 TG/钉钉对话是谁在用)。
#[tauri::command]
pub fn list_channel_chats(
    state: State<'_, AppState>,
) -> Result<Vec<larkwing_core::store::ChannelThread>, AppError> {
    state.engine.list_channel_chats()
}

/// 指认某条渠道对话归哪位家人(user_id 空 = 取消指认,回落会话归属者)。
#[tauri::command]
pub fn bind_channel_chat(
    state: State<'_, AppState>,
    id: i64,
    user_id: Option<i64>,
) -> Result<(), AppError> {
    state.engine.bind_channel_chat(id, user_id)
}

/// 给某家人录声纹:立即返回,录音/识别进展走 app_event 的 voice 车道
/// (Listening→Idle);完成或失败由前端据 voice 事件 + 重新拉 list_family 反映。
#[tauri::command]
pub fn voice_enroll(state: State<'_, AppState>, user_id: i64) -> Result<(), AppError> {
    let voice = state.voice.clone();
    tauri::async_runtime::spawn(async move {
        // 终态(saved/failed)由 enroll 内部经 Enroll 事件推前端;这里只兜底日志(§3.5)。
        if let Err(e) = voice.enroll(user_id).await {
            tracing::error!(err = %format!("{e:#}"), "声纹注册失败");
        }
    });
    Ok(())
}

/// 忘掉某家人的声纹(家人页「忘掉声音」):只删声纹,人 / 记忆不动。同步返回。
#[tauri::command]
pub fn voice_unenroll(state: State<'_, AppState>, user_id: i64) -> Result<(), AppError> {
    state.engine.forget_voiceprint(user_id)
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
    // 试听失败以前只在前端 console 冒(正式版无 devtools)→ 日志里查不到真因。这里落 warn,
    // 让「合成报错 / 参考音坏 / 模型没下全」在 logs/larkwing.log 里现形(§3.5)。
    let path = state.voice.preview(&speaker, &text).await.map_err(|e| {
        tracing::warn!(speaker = %speaker, err = %format!("{e:#}"), "试听合成失败");
        AppError::internal(e)
    })?;
    state.media.file_url(path).await.map_err(|e| {
        tracing::warn!(speaker = %speaker, err = %format!("{e:#}"), "试听 URL 生成失败");
        AppError::internal(e)
    })
}

// ---- 音色克隆(PLAN §11 D-clone) ----

/// `voice_clone_record` 的返回:录完待确认的草稿(尚未落库)。
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloneDraft {
    pub clone_id: String,
    pub transcript: String,
}

/// 列出克隆音色(内置预置 + 用户自录,混在音色列表)。
#[tauri::command]
pub fn list_voice_clones(state: State<'_, AppState>) -> Result<Vec<ClonedVoice>, AppError> {
    state.voice.list_clones().map_err(AppError::internal)
}

/// 录一段参考音 → 自动转写,返回 (clone_id, 文字稿) 供前端过目/修改;**此刻不写库**。
/// 录音/电平进展走 voice 车道事件(Listening→Idle);命令 await 到录完+转写才返回。
#[tauri::command]
pub async fn voice_clone_record(state: State<'_, AppState>) -> Result<CloneDraft, AppError> {
    let (clone_id, transcript) =
        state.voice.clone_record().await.map_err(AppError::internal)?;
    Ok(CloneDraft { clone_id, transcript })
}

/// 导入本地音频文件(前端解码/重采样成 16k 单声道 wav 的 base64)→ 转写,返回草稿(未落库)。
#[tauri::command]
pub async fn voice_clone_import(
    state: State<'_, AppState>,
    wav_base64: String,
) -> Result<CloneDraft, AppError> {
    let (clone_id, transcript) =
        state.voice.clone_import(&wav_base64).await.map_err(AppError::internal)?;
    Ok(CloneDraft { clone_id, transcript })
}

/// 确认录入:用(可能改过的)文字稿 + 名字落库,返回新音色。
#[tauri::command]
pub fn voice_clone_save(
    state: State<'_, AppState>,
    clone_id: String,
    name: String,
    transcript: String,
) -> Result<ClonedVoice, AppError> {
    state.voice.clone_save(&clone_id, &name, &transcript).map_err(AppError::internal)
}

/// 重命名克隆音色。
#[tauri::command]
pub fn rename_voice_clone(
    state: State<'_, AppState>,
    clone_id: String,
    name: String,
) -> Result<(), AppError> {
    state.voice.rename_clone(&clone_id, &name).map_err(AppError::internal)
}

/// 删除克隆音色(内置不可删,连参考音一并删)。
#[tauri::command]
pub fn delete_voice_clone(state: State<'_, AppState>, clone_id: String) -> Result<(), AppError> {
    state.voice.delete_clone(&clone_id).map_err(AppError::internal)
}

/// RFC 6265 式域匹配(够用版):host 与 cookie 域相等,或是其子域(点开头域去点比后缀)。
/// 只给 mac 的 cookie 轮询自滤用(见 media_login 内注释)。
#[cfg(target_os = "macos")]
fn cookie_domain_matches(host: &str, domain: Option<&str>) -> bool {
    let Some(d) = domain.map(|d| d.trim_start_matches('.')).filter(|d| !d.is_empty()) else {
        return false;
    };
    host == d || host.ends_with(&format!(".{d}"))
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
    let builder = tauri::WebviewWindowBuilder::new(&app, LABEL, tauri::WebviewUrl::External(login_url))
        .title(title)
        .inner_size(460.0, 640.0);
    // mac WKWebView 默认 UA 缺 Version/Safari 版本段,B 站登录页判「浏览器版本过低」拒开;
    // 补成真 Safari 的冻结形 UA(各段 Apple 已冻结不陈化)。Windows WebView2 自带现代
    // Chrome UA、扫码已真机验过 → 不动。
    #[cfg(target_os = "macos")]
    let builder = builder.user_agent(
        "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.6 Safari/605.1.15",
    );
    builder.build().map_err(AppError::internal)?;

    // cookie 域兜底:host 去掉 www. 前缀加点(原生 API 偶尔不回 domain 字段)
    let fallback_domain = cookie_url
        .host_str()
        .map(|h| format!(".{}", h.trim_start_matches("www.")))
        .unwrap_or_default();
    let media = state.media.clone();
    #[cfg(target_os = "macos")]
    let host = cookie_url.host_str().unwrap_or_default().to_string();
    tauri::async_runtime::spawn(async move {
        for _ in 0..200 {
            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
            // 用户手动关窗 = 放弃登录,轮询随之收摊
            let Some(win) = app.get_webview_window(LABEL) else { return };
            // mac 上 wry 的 cookies_for_url 按「cookie 域 == URL 域」精确比较,
            // `.bilibili.com` 的登录 cookie 对 www 主机永远匹配不上(2026-07-06 实锤,
            // 登录成功却读不到 SESSDATA)→ 取全量、自己按域后缀匹配;Windows 走
            // WebView2 原生匹配(已真机验过,且原生 API 偶尔不回 domain 字段,套自滤
            // 会误杀)→ 维持 cookies_for_url 不动。
            #[cfg(target_os = "macos")]
            let cookies = win.cookies().map(|all| {
                all.into_iter()
                    .filter(|c| cookie_domain_matches(&host, c.domain()))
                    .collect::<Vec<_>>()
            });
            #[cfg(not(target_os = "macos"))]
            let cookies = win.cookies_for_url(cookie_url.clone());
            let cookies = match cookies {
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

/// 失败任务「重试」(PLAN §10):直连重放,不绕 LLM(同嘴控的按钮直连哲学 §7.1)。
/// 当前唯一可重试 = 影音播放(解析 / 组件下载失败);重放走 media.play,进展 / 结果照常上事件车道
/// (新任务卡 → running;再失败 → 新 failed 卡又带重试)。spawn 后秒回,UI 靠 app_event 追平。
#[tauri::command]
pub fn media_retry(
    state: State<'_, AppState>,
    page_url: String,
    audio_only: bool,
) -> Result<(), AppError> {
    let media = state.media.clone();
    let user_id = state.engine.store().users.ensure_default_user()?.id;
    tauri::async_runtime::spawn(async move {
        // restart=false:重试沿用续播规则(本就是接着之前那次播放)
        if let Err(e) = media.play(user_id, &page_url, audio_only, false).await {
            // 失败已由 play() 内部 task.fail_retryable 上报 HUD;这里只留日志
            tracing::debug!("重试播放失败(已上报 HUD): {e:#}");
        }
    });
    Ok(())
}

/// 失败下载「重试」(PLAN §10):重下一个组件(yt-dlp/ffmpeg…),直连不绕 LLM。
/// 把「下载」这类 job 也纳入失败可重试(原仅影音);重下自带 HUD 任务(成功 done / 再败再冒重试卡)。
#[tauri::command]
pub fn retry_download(state: State<'_, AppState>, component: String) -> Result<(), AppError> {
    state.media.retry_component(&component);
    Ok(())
}

/// 失败语音模型下载「重试」(v0.2.4 补齐三型语音模型;同上直连哲学)。
#[tauri::command]
pub fn retry_voice_model(state: State<'_, AppState>, id: String) -> Result<(), AppError> {
    state.voice.retry_model(&id);
    Ok(())
}

/// 多集续播切集(PLAN §9 多集续播):前端 `ended` 自动下一集、播放器上/下一集按钮直连这里
/// (不绕 LLM,同 media_retry / 嘴控按钮哲学 §7.1)。delta = +1 下一集 / -1 上一集。
/// 越界(到头/到顶)在 advance 内报错,这里只记日志 —— 按钮路径没有模型可叙述。
#[tauri::command]
pub fn media_advance(state: State<'_, AppState>, delta: i32) -> Result<(), AppError> {
    let media = state.media.clone();
    let user_id = state.engine.store().users.ensure_default_user()?.id;
    tauri::async_runtime::spawn(async move {
        if let Err(e) = media.advance(user_id, delta).await {
            tracing::debug!("续播切集结束/失败(可能已到头或到顶): {e:#}");
        }
    });
    Ok(())
}

/// 前端回报播放器当下状态(播放真相在前端 WebView):playing/paused 时带标题,
/// ended/stop → idle;富字段(音量/进度/时长/倍速)一并回报,core 据此校准快照,
/// 下个回合装配时喂模型「此刻」背景(修「歌放完了模型却以为还在播」+ 让它知道当前
/// 音量/进度才能做绝对/相对调整)。只主窗(真播放位)回报,悬浮窗是镜像不报。
#[tauri::command]
pub fn report_media_state(
    state: State<'_, AppState>,
    report: larkwing_core::media::PlaybackReport,
) {
    state.media.set_playback(report);
}

/// 历史图片小票 → 可显缩略图的 localhost URL(重开会话回看发过的图,§1/§9)。
/// 前端 hydrate 历史时按需取;图 bytes 走文件不进库、不再喂 LLM。
#[tauri::command]
pub async fn attachment_url(state: State<'_, AppState>, file: String) -> Result<String, AppError> {
    state.media.attachment_url(&file).await.map_err(AppError::internal)
}

/// 前端播放层诊断 → 写进 `larkwing.log`(正式版 WebView 无 JS console,MSE 只在 Windows 真机能验;
/// 靠这条把「自适应为何卡/回落」的现场喂到日志,便于下一版真机定位)。仅记日志,不改状态。
#[tauri::command]
pub fn media_log(msg: String) {
    tracing::info!("[前端播放] {msg}");
}

/// 兜底重放:本地「音视频分离自适应」在前端手写 MSE 上播放失败时,前端调此命令,
/// 后端对同一文件强制回落 muxed HLS(能放的老路)。异步 spawn,不阻塞;失败只记日志。
#[tauri::command]
pub fn media_replay_compat(state: State<'_, AppState>, page_url: String, audio_only: bool) {
    let media = state.media.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(e) = media.replay_local_compat(&page_url, audio_only).await {
            tracing::warn!("兜底重放(muxed HLS)失败: {e:#}");
        }
    });
}

// ---- 远程渠道(PLAN 远程渠道:Telegram / 钉钉 bot) ----

/// 远程渠道设置页一行的视图:开关 / 是否已配凭证(**凭证本身永不过桥**,只报 bool)/ 白名单 / 连接态。
#[derive(serde::Serialize)]
pub struct RemoteChannelView {
    pub id: String,
    pub enabled: bool,
    pub configured: bool,
    pub allowed_chats: String,
    pub running: bool,
    pub last_error: Option<String>,
}

/// 远程渠道状态(设置页读):服务端读 settings + 实时连接态拼成视图,token/secret 不出 core。
#[tauri::command]
pub fn remote_status(state: State<'_, AppState>) -> Result<Vec<RemoteChannelView>, AppError> {
    let s = &state.engine.store().settings;
    let get = |k: &str| {
        s.get(None, k)
            .ok()
            .flatten()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    // 凭证已迁 keyring(不在 settings 明文)→ configured 检查走 secrets,否则永远 false
    let sec = |k: &str| larkwing_core::secrets::get(s, k).filter(|v| !v.trim().is_empty());
    let live = state.channels.status_snapshot();
    let view = |id: &str, enabled_k: &str, configured: bool, allowed: String| {
        let st = live.get(id);
        RemoteChannelView {
            id: id.into(),
            enabled: get(enabled_k).as_deref() == Some("1"),
            configured,
            allowed_chats: allowed,
            running: st.map(|s| s.running).unwrap_or(false),
            last_error: st.and_then(|s| s.last_error.clone()),
        }
    };
    Ok(vec![
        view(
            "telegram",
            "remote.telegram.enabled",
            sec("remote.telegram.token").is_some(),
            get("remote.telegram.allowed_chats").unwrap_or_default(),
        ),
        // 钉钉(Phase B):配置/状态先就位,适配器后接
        view(
            "dingtalk",
            "remote.dingtalk.enabled",
            sec("remote.dingtalk.app_key").is_some() && sec("remote.dingtalk.app_secret").is_some(),
            String::new(),
        ),
        // 微信(腾讯 iLink bot):扫码进绑定列表 → configured(旧单 token 兼容);
        // 白名单 = 手动附加名单(绑定者自动放行,不在这里)
        view(
            "weixin",
            "remote.weixin.enabled",
            sec("remote.weixin.accounts").is_some() || sec("remote.weixin.token").is_some(),
            get("remote.weixin.allowed_users").unwrap_or_default(),
        ),
    ])
}

/// 微信绑定列表(多绑定 = 一人一 bot,§7.7):只回绑定者 user_id,**不含 token**。
/// 空串项 = 旧版迁移来的无身份绑定。
#[tauri::command]
pub fn weixin_accounts(state: State<'_, AppState>) -> Result<Vec<String>, AppError> {
    Ok(larkwing_core::channels::weixin_accounts(&state.engine))
}

/// 解绑一个微信账号(user_id 空串 = 解绑旧迁移绑定);前端随后调 reload_channels 生效。
#[tauri::command]
pub fn weixin_unbind(state: State<'_, AppState>, user_id: String) -> Result<(), AppError> {
    larkwing_core::channels::weixin_unbind(&state.engine, &user_id).map_err(AppError::internal)
}

/// 微信扫码登录起手:拿二维码(SVG + 备用链接 + 轮询 qrcode)。协议/QR 流程在 core channels::weixin。
#[tauri::command]
pub async fn weixin_login_start() -> Result<larkwing_core::channels::QrStart, AppError> {
    larkwing_core::channels::weixin_qr_start().await.map_err(AppError::internal)
}

/// 微信扫码轮询一次:前端循环调,confirmed 时 core 已把 token/base_url/白名单落库。
/// `base_url` = 前端持有的当前轮询地址(redirect 时更新回传);`verify_code` = 手机上的配对码。
#[tauri::command]
pub async fn weixin_login_poll(
    state: State<'_, AppState>,
    qrcode: String,
    base_url: Option<String>,
    verify_code: Option<String>,
) -> Result<larkwing_core::channels::QrPoll, AppError> {
    larkwing_core::channels::weixin_qr_poll(
        &state.engine,
        &qrcode,
        base_url.as_deref(),
        verify_code.as_deref(),
    )
    .await
    .map_err(AppError::internal)
}

/// 保存远程渠道配置后调:停旧起新(类比 provider 保存即重建)。
#[tauri::command]
pub fn reload_channels(state: State<'_, AppState>) {
    state.channels.restart();
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
/// setup 只建图标 + 左键唤主窗;菜单(打开 / 显示悬浮窗 / 退出)等这里来。
/// show_float = 重开被关掉的悬浮窗(✕ 关掉后比去设置页更顺手,PLAN §12 收口)。
#[tauri::command]
pub fn set_tray_menu(
    app: tauri::AppHandle,
    open: String,
    show_float: String,
    quit: String,
) -> Result<(), AppError> {
    use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
    let tray = app.tray_by_id("tray").ok_or_else(|| AppError::internal("托盘未就绪"))?;
    let menu = Menu::with_items(
        &app,
        &[
            &MenuItem::with_id(&app, "open", open, true, None::<&str>).map_err(AppError::internal)?,
            &MenuItem::with_id(&app, "show_float", show_float, true, None::<&str>)
                .map_err(AppError::internal)?,
            &PredefinedMenuItem::separator(&app).map_err(AppError::internal)?,
            &MenuItem::with_id(&app, "quit", quit, true, None::<&str>).map_err(AppError::internal)?,
        ],
    )
    .map_err(AppError::internal)?;
    tray.set_menu(Some(menu)).map_err(AppError::internal)?;
    Ok(())
}

/// 退出整个程序(悬浮窗右键「退出」/ 复刻托盘 quit)。与托盘菜单 quit 同义 = app.exit(0)。
#[tauri::command]
pub fn quit_app(app: tauri::AppHandle) {
    app.exit(0);
}

/// 更新装完后重启(走核心 `app.restart()`,免再拉 plugin-process)。Windows 上 NSIS passive
/// 装前已自动退出 app、装后由安装器拉起,故这条主要给 mac/兜底用;`-> !` 不返回。
#[tauri::command]
pub fn relaunch_app(app: tauri::AppHandle) {
    app.restart();
}

// ---- 数据目录「搬家」(datadir;用户决策 2026-06-18) ----

/// 设置页「数据位置」一行 + boot 后检查:当前根 / 待清理旧根 / 失效路径。
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DataLocation {
    /// 当前生效的数据根(绝对路径)。
    pub root: String,
    /// 刚搬完待清理的旧根(Some = 前端弹「删除/保留」提示);清理后为 None。
    pub old_root: Option<String>,
    /// 数据位置失效(盘没插/被删)的记录(Some = 前端弹恢复弹窗);正常为 None。
    pub missing: Option<String>,
    /// 本次启动「从备份恢复」的落位结果("ok"/"failed";None = 无事)→ 前端 boot 弹一句结果。
    pub restored: Option<String>,
}

/// 读数据位置(指针每次现读,反映清理后的最新态)。
#[tauri::command]
pub fn data_location(state: State<'_, AppState>) -> DataLocation {
    let ptr = datadir::read_pointer(&state.anchor);
    DataLocation {
        root: state.data_root.to_string_lossy().into_owned(),
        old_root: ptr.old_root.filter(|s| !s.trim().is_empty()),
        missing: state.data_missing.as_ref().map(|p| p.to_string_lossy().into_owned()),
        restored: state.restore_outcome.map(str::to_owned),
    }
}

/// 唤起系统原生目录选择器(在 Rust 侧调 DialogExt,不走 JS 插件命令故无需 capability)。
/// 返回所选目录绝对路径;用户取消 = None。
#[tauri::command]
pub async fn pick_data_folder(app: tauri::AppHandle) -> Result<Option<String>, AppError> {
    use tauri_plugin_dialog::DialogExt;
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog().file().pick_folder(move |p| {
        let _ = tx.send(p);
    });
    let picked = rx.await.map_err(AppError::internal)?;
    Ok(picked
        .and_then(|fp| fp.into_path().ok())
        .map(|p| p.to_string_lossy().into_owned()))
}

/// 一键备份:在所选目录导出 `larkwing-backup-<时间戳>.zip`(DB 一致快照 + 克隆音色)。
/// 区别于搬家:纯导出拷贝,不翻指针 / 不重启。返回压缩包绝对路径(前端提示「已备份到…」)。
/// VACUUM + 打包是阻塞活 → spawn_blocking。
#[tauri::command]
pub async fn backup_data(
    state: State<'_, AppState>,
    dest_dir: String,
) -> Result<String, AppError> {
    let data_root = state.data_root.clone();
    let dest = PathBuf::from(dest_dir);
    let zip = tauri::async_runtime::spawn_blocking(move || datadir::backup_to(&data_root, &dest))
        .await
        .map_err(AppError::internal)? // join 错误
        .map_err(AppError::internal)?; // backup_to 错误
    tracing::info!(zip = %zip.display(), "数据备份完成");
    Ok(zip.to_string_lossy().into_owned())
}

/// 唤起系统原生文件选择器挑备份包(zip)。返回绝对路径;用户取消 = None。
#[tauri::command]
pub async fn pick_backup_file(app: tauri::AppHandle) -> Result<Option<String>, AppError> {
    use tauri_plugin_dialog::DialogExt;
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog().file().add_filter("zip", &["zip"]).pick_file(move |p| {
        let _ = tx.send(p);
    });
    let picked = rx.await.map_err(AppError::internal)?;
    Ok(picked
        .and_then(|fp| fp.into_path().ok())
        .map(|p| p.to_string_lossy().into_owned()))
}

/// 恢复预检结果(给前端确认弹窗:包概要 + 失败原因 code)。
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreCheckOut {
    pub ok: bool,
    /// 失败原因(RestoreBlock code → 前端 `settings.system.restoreErr.<reason>`);ok 时 None。
    pub reason: Option<String>,
    /// 包内数据库(解压后)字节数;ok 时有值。
    pub db_bytes: u64,
    /// 包内克隆音色文件个数。
    pub clones: u32,
}

/// 恢复预检(选完备份包、确认前调):zip 结构 + DB 魔数 + 迁移版本前向检查(备份来自
/// 更新版本 → 拒并明说,老程序开新库会坏)。纯校验不动数据。
#[tauri::command]
pub async fn restore_precheck(zip: String) -> Result<RestoreCheckOut, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let known = larkwing_core::store::migration_ids();
        match datadir::restore_precheck(Path::new(&zip), &known) {
            Ok(info) => RestoreCheckOut {
                ok: true,
                reason: None,
                db_bytes: info.db_bytes,
                clones: info.clones,
            },
            Err(b) => RestoreCheckOut { ok: false, reason: Some(b.code().into()), db_bytes: 0, clones: 0 },
        }
    })
    .await
    .map_err(AppError::internal)
}

/// 执行「从备份恢复」:重检 → 负载解到 `<root>/restore-pending/` → 立即重启;
/// 真正落位在下次 boot 开库前(运行中不能覆盖已打开的 DB),现库届时留保险副本
/// `larkwing.db.pre-restore-<时间戳>`。成功路径不返回(进程重启);失败返回 AppError。
#[tauri::command]
pub async fn restore_data(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    zip: String,
) -> Result<(), AppError> {
    let root = state.data_root.clone();
    tauri::async_runtime::spawn_blocking(move || {
        // 重检(前端已 precheck,这里防竞态/直接调用)。
        let known = larkwing_core::store::migration_ids();
        datadir::restore_precheck(Path::new(&zip), &known)
            .map_err(|b| anyhow::anyhow!("恢复预检未过: {}", b.code()))?;
        datadir::stage_restore(&root, Path::new(&zip))
    })
    .await
    .map_err(AppError::internal)? // join 错误
    .map_err(AppError::internal)?; // stage_restore 错误
    tracing::info!("恢复负载已就位,重启落位");
    app.restart(); // -> !,不返回
}

/// 搬家预检结果(给前端确认弹窗:目标路径 + 体积 + 失败原因 code)。
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RelocateCheck {
    pub ok: bool,
    /// 失败原因(MoveBlock code → 前端 `settings.dataLocation.err.<reason>`);ok 时 None。
    pub reason: Option<String>,
    /// 最终数据根(= 所选目录/Larkwing);ok 时 Some。
    pub new_root: Option<String>,
    /// 需要 / 可用字节(给前端显「约 X GB,目标剩余 Y GB」)。
    pub need_bytes: u64,
    pub free_bytes: u64,
}

/// 搬家预检(选完目录、确认前调:把目标路径 + 体积 + 可行性给前端)。
#[tauri::command]
pub fn relocate_precheck(state: State<'_, AppState>, picked: String) -> RelocateCheck {
    match datadir::precheck(&state.data_root, Path::new(&picked)) {
        Ok(plan) => RelocateCheck {
            ok: true,
            reason: None,
            new_root: Some(plan.new_root.to_string_lossy().into_owned()),
            need_bytes: plan.need_bytes,
            free_bytes: plan.free_bytes,
        },
        Err(b) => RelocateCheck {
            ok: false,
            reason: Some(b.code().into()),
            new_root: None,
            need_bytes: 0,
            free_bytes: 0,
        },
    }
}

/// 执行搬家:预检 → 拷贝/VACUUM(HUD 进度)→ 翻指针(提交点,记 old_root)→ 立即重启。
/// 成功路径不返回(进程重启);失败返回 AppError(前端兜底文案提示)。
#[tauri::command]
pub async fn relocate_data(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    picked: String,
) -> Result<(), AppError> {
    let current = state.data_root.clone();
    let anchor = state.anchor.clone();
    let bus = state.bus.clone();
    let picked = PathBuf::from(picked);

    // 重检(前端已 precheck,这里防竞态/直接调用)。
    let plan = datadir::precheck(&current, &picked)
        .map_err(|b| AppError::internal(format!("搬家预检未过: {}", b.code())))?;
    let new_root = plan.new_root.clone();

    // 拷贝/VACUUM 是阻塞活,放 spawn_blocking;进度走 Tasks → bus → app_event → HUD。
    let current_for_move = current.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let task = Tasks::new(bus).start("relocate", Text::new("task.relocate"));
        match datadir::perform_move(&current_for_move, &plan, &task) {
            Ok(()) => {
                task.done();
                Ok(())
            }
            Err(e) => {
                task.fail("task.err.relocate", serde_json::Value::Null);
                Err(e)
            }
        }
    })
    .await
    .map_err(AppError::internal)? // join 错误
    .map_err(AppError::internal)?; // perform_move 错误

    // 翻指针 = 提交点:记新根 + 旧根(供清理)。此刻起新位置权威。
    datadir::write_pointer(
        &anchor,
        &Pointer {
            data_root: Some(new_root.to_string_lossy().into_owned()),
            old_root: Some(current.to_string_lossy().into_owned()),
        },
    )
    .map_err(AppError::internal)?;

    tracing::info!(new_root = %new_root.display(), "数据搬家完成,重启生效");
    app.restart(); // -> !,不返回
}

/// 搬家后「删除旧数据」:删旧根数据(保留指针)→ 清指针 old_root 字段。
#[tauri::command]
pub fn cleanup_old_data(state: State<'_, AppState>) -> Result<(), AppError> {
    let ptr = datadir::read_pointer(&state.anchor);
    if let Some(old) = ptr.old_root.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        datadir::cleanup_old(Path::new(old), &state.anchor).map_err(AppError::internal)?;
    }
    datadir::write_pointer(&state.anchor, &Pointer { data_root: ptr.data_root, old_root: None })
        .map_err(AppError::internal)
}

/// 搬家后「保留旧数据」:只清指针 old_root 字段,不删盘(用户日后自己删)。
#[tauri::command]
pub fn keep_old_data(state: State<'_, AppState>) -> Result<(), AppError> {
    let ptr = datadir::read_pointer(&state.anchor);
    datadir::write_pointer(&state.anchor, &Pointer { data_root: ptr.data_root, old_root: None })
        .map_err(AppError::internal)
}

/// 数据位置失效时「恢复默认」:清指针 → 重启从锚点起(全新数据)。
#[tauri::command]
pub fn data_reset_to_default(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<(), AppError> {
    datadir::write_pointer(&state.anchor, &Pointer::default()).map_err(AppError::internal)?;
    app.restart(); // -> !,不返回
}

/// 在系统文件管理器里「显示」数据文件夹(走原生命令,绕开 opener scope 坑,§8.3)。
/// macOS 用 `open -R`(在 Finder 里定位并选中)= 标准「在文件夹中显示」语义,且对任何目录命名都安全
/// (历史坑:旧标识符 `com.larkwing.app` 末段以 `.app` 结尾 → `open <dir>` 被 LaunchServices 当应用包
/// 去启动、报 "executable is missing" 且 Finder 不弹;已改标识符为 com.larkwing.desktop 根治,`-R` 留作稳妥)。
#[tauri::command]
pub fn reveal_data_dir(state: State<'_, AppState>) -> Result<(), AppError> {
    let path = state.data_root.clone();
    let spawned = if cfg!(target_os = "windows") {
        std::process::Command::new("explorer").arg(&path).spawn()
    } else if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg("-R").arg(&path).spawn()
    } else {
        std::process::Command::new("xdg-open").arg(&path).spawn()
    };
    spawned.map(|_| ()).map_err(AppError::internal)
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::cookie_domain_matches;

    #[test]
    fn cookie_domain_matching_covers_dotted_and_host_only() {
        // 带点主域 cookie(SESSDATA 形)对子域必须命中 —— 2026-07-06 登录读不到 cookie 的 bug 本体
        assert!(cookie_domain_matches("www.bilibili.com", Some(".bilibili.com")));
        assert!(cookie_domain_matches("www.bilibili.com", Some("bilibili.com")));
        assert!(cookie_domain_matches("bilibili.com", Some(".bilibili.com")));
        // host-only 精确命中
        assert!(cookie_domain_matches("www.bilibili.com", Some("www.bilibili.com")));
        // 兄弟子域 / 无关域 / 后缀陷阱 / 缺失域都不命中
        assert!(!cookie_domain_matches("www.bilibili.com", Some("passport.bilibili.com")));
        assert!(!cookie_domain_matches("www.bilibili.com", Some("hdslb.com")));
        assert!(!cookie_domain_matches("notbilibili.com", Some("bilibili.com")));
        assert!(!cookie_domain_matches("www.bilibili.com", None));
        assert!(!cookie_domain_matches("www.bilibili.com", Some(".")));
    }
}
