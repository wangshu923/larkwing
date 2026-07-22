//! App 级事件总线:PLAN §5「全局事件车道」的兑现。回合内事件走 TurnEvent/Channel
//! (按调用隔离),**会话之外**的事(任务进度、播放器指令)走这条广播车道。
//! 人格中立底座(宪法 §5):事件只带 key/params/数据,文案由前端字典渲染。

use serde::Serialize;
use tokio::sync::broadcast;

/// 文案引用:key 进前端字典,params 是命名插值参数(vue-i18n 形)。
/// core 不产用户可见文案的铁规在类型层固化 —— 这里没有放句子的地方。
#[derive(Debug, Clone, Serialize)]
pub struct Text {
    pub key: String,
    #[serde(skip_serializing_if = "serde_json::Value::is_null")]
    pub params: serde_json::Value,
}

impl Text {
    pub fn new(key: impl Into<String>) -> Text {
        Text { key: key.into(), params: serde_json::Value::Null }
    }

    pub fn with(key: impl Into<String>, params: serde_json::Value) -> Text {
        Text { key: key.into(), params }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Running,
    Done,
    Failed,
}

/// 失败任务的「重试」载体:带上重放这件事所需的最小入参。UI 据此显重试钮,点击直连重放
/// (按钮直连、不绕 LLM,同嘴控哲学 §7.1)。无 JobRunner 时的轻量重放口(PLAN §10)。
/// 没有此字段的失败 = 不可重放(被 drop / 需登录),不显重试钮。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum TaskRetry {
    /// 重放一次影音播放:入参 = 当初 media_play 的 page_url + audio_only。
    MediaPlay { page_url: String, audio_only: bool },
    /// 重下一个组件(yt-dlp / ffmpeg…):入参 = 组件名;前端按钮直连 `retry_download`。
    /// 把「下载」这类 job 也纳入「失败可重试」(原仅影音);未来别的下载照此加分支。
    Download { component: String },
    /// 重下一个语音模型(ASR/VAD/KWS/离线嗓音/克隆嗓音/声纹):入参 = ModelSpec/TarModelSpec/
    /// TreeModelSpec 的 `id`;前端按钮直连 `retry_voice_model`(0.2.0 里程碑「留待真需要」项兑现)。
    VoiceModel { id: String },
}

/// 任务进度快照(HUD 的词汇):前端按 task_id upsert,每条事件都是全量快照,
/// 不做增量补丁 —— 错过任意一条,下一条就把状态追平。
#[derive(Debug, Clone, Serialize)]
pub struct TaskView {
    pub task_id: u64,
    /// download | resolve | …(前端按 kind 选图标,未知 kind 用通用图标)
    pub kind: String,
    /// 标题行。
    pub label: Text,
    pub state: TaskState,
    /// 0..=1;None = 不定态(转圈)。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<f32>,
    /// 当前到哪一步(用户准则 2026-06-12:任务要能写"到哪一步了")。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step: Option<Text>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Text>,
    /// 失败且可重放时带上(UI 显「重试」按钮);None = 不可重试。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry: Option<TaskRetry>,
}

/// 播放器车道。Play/Control 是 core → UI 的指令;UI 本地按钮直接操作播放元素,不绕这里。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum MediaEvent {
    /// 解析完成,前端把 stream_url 挂上 <audio>/<video>。
    Play(crate::media::NowPlaying),
    /// 模型侧的播放控制(用户用嘴说"暂停/大点声/倍速/跳到第几秒/单曲循环/随机放"):
    /// pause | resume | stop | louder | softer | volume | speed | seek |
    /// loop_one | loop_all | loop_off | shuffle_on | shuffle_off;volume/speed/seek 带 value。
    /// 循环/随机已先落 core 状态,事件只为前端对齐 el.loop/按钮态。
    Control {
        action: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        value: Option<f64>,
    },
    /// 登录态缺失/失效:UI 出"扫码登录"入口;话术由模型按人格组织(中性事件喂回模型)。
    AuthRequired { source: String },
    /// 建议气泡素材:还没登录、首次播放成功后提示一次(登录 = 更高画质)。
    LoginHint { source: String },
    LoggedIn { source: String },
}

/// 会话有动静(engine 自启回合完成):UI 据此刷新列表/重拉当前会话。
/// PLAN §5 全局事件车道的本职用途 —— 自启回合没有 invoke、无 Channel 可挂。
#[derive(Debug, Clone, Serialize)]
pub struct ConversationActivity {
    pub conv_id: i64,
    /// reminder | …(将来:主动问候/任务完成回报,前端按 kind 选表现)
    pub kind: String,
    /// 回合终态:用户不在该会话界面时,前端据此在列表项打彩色标(完成 / 失败)。
    /// 「疑问 / 等用户回话」暂不做(系统当前无此语义,见会话列表 badge 决策)。
    pub outcome: TurnOutcome,
}

/// 后台回合的终态(会话列表 badge 用)。done = 正常收尾;failed = 出错收尾。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnOutcome {
    Done,
    Failed,
}

/// 听写会话阶段(PLAN §11):前端 mood/麦克风按钮据此切表现。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VoicePhase {
    Idle,
    /// 组件/模型准备中(首次用时下载,进度另走 Task 车道)。
    Preparing,
    Listening,
    Transcribing,
}

/// 语音车道(PLAN §11):听写/唤醒会话的状态与产出;编排者 = 前端 VM,core 只供能力。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum VoiceEvent {
    State { phase: VoicePhase },
    /// 实时电平(0..=1,~10Hz 节流),驱动波形动画;只在 Listening 期发。
    Level { level: f32 },
    /// VAD 判到开口(UI 把"聆听中"换成"在听你说")。
    SpeechStarted,
    /// 喊名命中(C 期):前端开全区间 duck(到回待唤醒才恢复)。
    WakeTriggered,
    /// KWS 报了候选、确认层在核(命中→静默续录到断句→ASR 三段式,§8.2 精度方向):
    /// 前端提前 duck + 轻视觉「在听」;**不出声**(出声等确认——误唤醒最烦的是突然出声)。
    WakeCandidate,
    /// 确认层拒绝(转写里没有唤醒词 = KWS 幻听):前端恢复 duck、视觉回 idle。零打扰。
    WakeRejected,
    /// 呼名+续句(「天天,暂停」/「看天天向上」):整句交模型仲裁是不是在叫它
    /// (前端调 send_overheard 起临时回合)。text = 含前文的整句转写;
    /// speaker_id = 声纹认出的家人,None = 没认出/没开声纹。
    Overheard {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        speaker_id: Option<i64>,
    },
    /// 识别定稿:前端拿文本走既有 send 链。via: mic(听写,屏幕排版)| wake(语音会话,必念)。
    /// speaker_id = 声纹识别出的家人(PLAN §11 D),None = 没认出/没开声纹 → 走会话用户。
    Transcribed {
        text: String,
        via: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        speaker_id: Option<i64>,
    },
    /// 没有产出文本的收尾:no_speech | cancelled | error(听写)
    /// | no_speech_retry(唤醒首轮没听清,追问后再听)| farewell(两轮没听到,有声告退)
    /// | follow_up_idle(跟进窗口安静结束)| wake_done(回合周期收尾兜底)。
    ListenEnded { reason: String },
    /// 唤醒常驻的**权威开关广播**(core 起/停时发,boot 自动恢复也发):前端 wakeArmed 与
    /// mic bridge(browser 采集源的开麦条件)靠它跟随。缘起 2026-07-11 真机实锤「开着但聋」:
    /// 开机自启 core 的 wake_start(加载模型/短句银行,秒级)比前端首查 voiceStatus 慢 →
    /// armed 定格 false → browser 源永不开麦,KWS 空转;手动 off→on 靠设置页自发事件才活。
    /// keywords 随 running=true 带出(前端「听哪个词」跟新);false 时为空。
    WakeRunning { running: bool, keywords: Vec<String> },
    /// 唤醒录音标定:正在录第 step/total 段(step 从 1 计;total 含末尾 1 段底噪)。
    CalibProgress { step: u8, total: u8 },
    /// 唤醒标定收尾:ok=成功落定;sensitivity=落定灵敏度(滑块应刷新);recall=该档召回(0..1);
    /// adopted_spelling=是否采用了更贴发音的异读拼写;verdict=结论 key
    /// (good | noisy | hard | cancelled | error,前端字典渲染文案,core 不产文案)。
    CalibResult {
        ok: bool,
        sensitivity: u32,
        recall: f32,
        adopted_spelling: bool,
        verdict: String,
    },
    /// 声纹注册进展(家人页「让它认识 TA 的声音」,PLAN §11 D 第二步)。core 只发数据,
    /// 文案走前端字典(§6.6)。stage: preparing(下载/加载声纹模型)| recording(录第
    /// done+1/total 段,请说话)| saved(成功落库)| failed(出错,前端 toast 请重试,§3.5)。
    /// user_id = 给谁录(多张家人卡时前端据此只在对应卡显进度)。
    Enroll {
        user_id: i64,
        stage: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        done: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        total: Option<u32>,
    },
}

/// 对话回合此刻"在干嘛"(PLAN §12 修订:原 v1 头像不镜像思考/说话,现上总线 ——
/// 主窗用 per-turn 通道驱动自己的 mood,这条是给第二窗(悬浮窗)的全局快照,
/// 语音回合主窗失焦时尤其有用。非"任务知识"入码,只是一面状态镜。)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mood {
    Idle,
    Thinking,
    Speaking,
}

/// 会话被后台重新定题(engine/title.rs LLM 命名落库后):前端原位改列表项文字。
/// 只是标题变了,**不是**「会话有动静」—— 不打 badge、不重排(set_title_if 不动 updated_at)。
#[derive(Debug, Clone, Serialize)]
pub struct ConvTitle {
    pub conv_id: i64,
    pub title: String,
}

/// 总线事件:tagged 编码,加变体对前端是增量(未知 type 忽略,与 TurnEvent 同约定)。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum AppEvent {
    Task(TaskView),
    Media(MediaEvent),
    Conversation(ConversationActivity),
    ConvTitle(ConvTitle),
    Voice(VoiceEvent),
    /// 回合 mood(悬浮窗显示「正在想 / 正在说」;主窗不消费,用自己的 per-turn mood)。
    Mood(Mood),
    /// 动作确认卡(§7.8 确认闸):HUD 任务区 + 悬浮窗显卡可点;渠道回合由 outbound_loop
    /// 消费推回发起 chat。全量快照语义(state 翻终态 = 收卡)。
    Confirm(crate::confirm::ConfirmCard),
}

/// 广播总线:壳层订阅一次、转发成 Tauri 全局事件;core 各处只管 publish。
/// 没有订阅者时 publish 静默丢弃(测试/无头跑法天然兼容)。
#[derive(Clone)]
pub struct Bus {
    tx: broadcast::Sender<AppEvent>,
}

impl Default for Bus {
    fn default() -> Self {
        Self::new()
    }
}

impl Bus {
    pub fn new() -> Bus {
        let (tx, _) = broadcast::channel(256);
        Bus { tx }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<AppEvent> {
        self.tx.subscribe()
    }

    pub fn publish(&self, ev: AppEvent) {
        let _ = self.tx.send(ev); // 无人听 = 丢弃,不是错误
    }

    /// 当前订阅者数。确认闸用它判「有没有任何确认通道可达」(0 = headless/单测,
    /// 立即拒不白等);正式 app 里壳层恒订阅(转发 Tauri 事件),恒 ≥1。
    pub fn receiver_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publish_without_subscribers_is_fine() {
        Bus::new()
            .publish(AppEvent::Media(MediaEvent::Control { action: "pause".into(), value: None }));
    }

    #[tokio::test]
    async fn events_serialize_tagged_and_text_omits_null_params() {
        let bus = Bus::new();
        let mut rx = bus.subscribe();
        bus.publish(AppEvent::Task(TaskView {
            task_id: 7,
            kind: "download".into(),
            label: Text::new("task.download.ffmpeg"),
            state: TaskState::Running,
            progress: Some(0.5),
            step: Some(Text::with("step.download", serde_json::json!({"done": 12, "total": 40}))),
            error: None,
            retry: None,
        }));
        let ev = rx.recv().await.unwrap();
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["type"], "task");
        assert_eq!(v["data"]["label"]["key"], "task.download.ffmpeg");
        assert!(v["data"]["label"].get("params").is_none(), "null params 不序列化");
        assert_eq!(v["data"]["step"]["params"]["total"], 40);
        assert!(v["data"].get("error").is_none());
        assert!(v["data"].get("retry").is_none(), "无 retry 不序列化");
    }
}
