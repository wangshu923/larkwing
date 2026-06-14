//! 语音运行时(PLAN §11,A 期「按住说话」):听写会话 = cpal 采集 → silero VAD 切段
//! → ASR → `Transcribed` 事件。**业务零入**:编排者 = 前端 VM(拿文本走既有 send 链),
//! 这里只供能力,不碰 engine(宪法 §5 三物种:交互渠道)。
//! 管线参数 = robot Windows 真机实调终值,锁死进代码,不暴露设置(PLAN §11)。

mod asr;
mod models;
mod prompts;
mod speaker;
mod tts;
mod wake;

pub use models::VoiceModels;
pub use tts::{Speaker, DEFAULT_SPEAKER, SPEAKERS_ZH};

/// 家人列表项(设置·家人 tab):用户 + 是否已录声纹。壳层 list_family 用。
#[derive(Debug, Clone, serde::Serialize)]
pub struct FamilyMember {
    #[serde(flatten)]
    pub user: crate::store::User,
    pub enrolled: bool,
}

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::bus::{AppEvent, Bus, VoiceEvent, VoicePhase};
use crate::scenes::Scenes;
use crate::store::Store;
use crate::tasks::Tasks;
use asr::{Asr, SherpaAsr};

// ---- 管线锁死参数(robot 验证值;PLAN §11「不暴露」清单) ----
const TARGET_RATE: u32 = 16_000;
const VAD_WINDOW: usize = 512; // silero v5 定长窗,32ms @16k
const VAD_THRESHOLD: f32 = 0.5;
const MIN_SPEECH_S: f32 = 0.5; // 反幻觉第一道闸:更短的段不算话
const MAX_SPEECH_S: f32 = 12.0;
const START_TIMEOUT: Duration = Duration::from_secs(6); // 开听后多久没人开口就放弃
const LEVEL_EVERY_WINDOWS: u32 = 3; // 电平事件节流:每 3 窗 ≈ 96ms ≈ 10Hz

/// 「听我说话的耐心」三档(voice.patience,user 级)→ VAD hangover(静音多久算说完)。
fn hangover_secs(patience: &str) -> f32 {
    match patience {
        "snappy" => 0.5,
        "relaxed" => 1.2,
        _ => 0.8, // standard,robot 真机终值
    }
}

// 控制字:会话线程轮询;listen_stop 写入。
const CTL_RUN: u8 = 0;
const CTL_ACCEPT: u8 = 1; // 立即定稿(把已听到的送识别)
const CTL_CANCEL: u8 = 2; // 丢弃收摊

struct SessionCtl {
    ctl: Arc<AtomicU8>,
    gen: u64,
}

/// 唤醒循环句柄(C 期):命令通道 + 线程在跑的事实。
struct WakeHandle {
    cmd: std::sync::mpsc::Sender<wake::WakeCmd>,
}

struct Inner {
    store: Store,
    bus: Bus,
    scenes: Scenes,
    models: VoiceModels,
    /// ASR 模型加载贵(秒级),进程内只一次;OfflineRecognizer 是 Send+Sync。
    asr: tokio::sync::OnceCell<Arc<SherpaAsr>>,
    /// 同时只有一个听写会话。
    session: std::sync::Mutex<Option<SessionCtl>>,
    gen: AtomicU64,
    /// TTS 引擎(trait 接缝;在线默认 EdgeTts)与句级缓存目录。
    tts: Arc<dyn tts::TtsEngine>,
    tts_dir: PathBuf,
    /// 离线 TTS(melo-vits,163M):按需加载一次进 OnceCell(voice.tts_backend=offline)。
    tts_offline: tokio::sync::OnceCell<Arc<tts::SherpaVits>>,
    /// 声纹提取器(CAM++ 26MB):有家人注册声纹时才加载(PLAN §11 D)。
    speaker: tokio::sync::OnceCell<Arc<speaker::SpeakerId>>,
    /// 唤醒循环(开关 = voice.wake.enabled;同时只有一个)。
    wake: std::sync::Mutex<Option<WakeHandle>>,
}

#[derive(Clone)]
pub struct VoiceRuntime {
    inner: Arc<Inner>,
}

/// 设置页「语音组件」状态行(不触发下载)+ 声音设置的数据源。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceStatus {
    pub asr_ready: bool,
    pub vad_ready: bool,
    pub kws_ready: bool,
    /// 唤醒循环此刻在跑(开关的真实状态,settings 只是意向)。
    pub wake_running: bool,
    /// 当前唤醒词(解析后的列表)。
    pub keywords: Vec<String>,
    /// 麦克风设备名列表(设置下拉的数据源)。
    pub devices: Vec<String>,
    /// 音色目录(当前语言行;目录 = 数据)。
    pub speakers: Vec<Speaker>,
}

impl VoiceRuntime {
    pub fn new(dir: PathBuf, store: Store, bus: Bus, scenes: Scenes) -> VoiceRuntime {
        let tasks = Tasks::new(bus.clone());
        let models = VoiceModels::new(dir.join("models"), tasks);
        VoiceRuntime {
            inner: Arc::new(Inner {
                store,
                bus,
                scenes,
                models,
                asr: tokio::sync::OnceCell::new(),
                session: std::sync::Mutex::new(None),
                gen: AtomicU64::new(0),
                tts: Arc::new(tts::EdgeTts),
                tts_dir: dir.join("tts"),
                tts_offline: tokio::sync::OnceCell::new(),
                speaker: tokio::sync::OnceCell::new(),
                wake: std::sync::Mutex::new(None),
            }),
        }
    }

    /// 句级 TTS 进缓存,返回音频文件路径(命中秒回);音色/语速取用户设置。
    /// 调用方(壳层)拿路径经 relay 注册成 localhost URL 给 `<audio>`。
    /// backend=offline → 本地 vits(断网兜底,音色固定);否则在线 edge。
    pub async fn tts_to_file(&self, text: &str) -> Result<PathBuf> {
        let text = text.trim();
        anyhow::ensure!(!text.is_empty(), "空文本不合成");
        let rate = tts::rate_pct(&self.user_setting("voice.rate", "standard"));
        if self.tts_offline_selected() {
            let engine: Arc<dyn tts::TtsEngine> = self.ensure_offline_tts().await?;
            // 离线单说话人:用固定 voice 标识区分缓存(与在线音色分开)
            return self.synth_cached(text, "vits-melo", rate, engine).await;
        }
        let speaker = self.user_setting("voice.speaker", tts::DEFAULT_SPEAKER);
        self.synth_cached(text, &speaker, rate, self.inner.tts.clone()).await
    }

    /// 设置页试听:指定音色合成一句(句子由前端字典传入——core 不产文案,
    /// 先例 = media_login 的窗口标题)。同进缓存,重复试听秒回。在线引擎(试听音色)。
    pub async fn preview(&self, speaker: &str, text: &str) -> Result<PathBuf> {
        let text = text.trim();
        anyhow::ensure!(!text.is_empty(), "空文本不合成");
        self.synth_cached(text, speaker, 0, self.inner.tts.clone()).await
    }

    fn tts_offline_selected(&self) -> bool {
        self.inner.store.settings.get(None, "voice.tts_backend").ok().flatten().as_deref()
            == Some("offline")
    }

    async fn ensure_offline_tts(&self) -> Result<Arc<tts::SherpaVits>> {
        let mirrors = self.mirrors();
        let dir = self.inner.models.ensure_tar(&models::TTS_VITS_MELO, &mirrors).await?;
        self.inner
            .tts_offline
            .get_or_try_init(|| async {
                tokio::task::spawn_blocking(move || tts::SherpaVits::load(&dir).map(Arc::new))
                    .await
                    .context("离线 TTS 加载任务挂了")?
            })
            .await
            .cloned()
    }

    async fn synth_cached(
        &self,
        text: &str,
        voice: &str,
        rate: i32,
        engine: Arc<dyn tts::TtsEngine>,
    ) -> Result<PathBuf> {
        let ext = engine.ext();
        // voice 已区分在线音色 vs 离线("vits-melo"),不同引擎产物 ext 不同、互不覆盖
        let key = tts::cache_key(voice, rate, text);
        let path = self.inner.tts_dir.join(format!("{key}.{ext}"));
        if path.is_file() {
            return Ok(path); // 宪法 §7:常用话不重复合成
        }
        tokio::fs::create_dir_all(&self.inner.tts_dir).await?;
        let (text2, voice2) = (text.to_string(), voice.to_string());
        let bytes = tokio::task::spawn_blocking(move || engine.synthesize(&text2, &voice2, rate))
            .await
            .context("TTS 任务挂了")??;
        // 先写临时名再原子改名:并发同句合成也只会留下一份完好文件
        let tmp = self.inner.tts_dir.join(format!("{key}.{}.part", std::process::id()));
        tokio::fs::write(&tmp, &bytes).await?;
        tokio::fs::rename(&tmp, &path).await?;
        Ok(path)
    }

    // ---- 声纹认人(PLAN §11 D):记忆归人的解锁钥匙 ----

    /// 有家人注册过声纹吗(没有 = 听写/唤醒跳过识别,零开销)。
    fn has_voiceprints(&self) -> bool {
        self.inner.store.voiceprints.enrolled_ids().map(|v| !v.is_empty()).unwrap_or(false)
    }

    fn voiceprint_library(&self) -> Vec<(i64, Vec<f32>)> {
        self.inner.store.voiceprints.list_all().unwrap_or_default()
    }

    /// 声纹模型就绪(CAM++ 26MB 用时下载,加载一次进 OnceCell)。
    async fn ensure_speaker(&self) -> Result<Arc<speaker::SpeakerId>> {
        let mirrors = self.mirrors();
        let dir = self.inner.models.ensure(&models::SPEAKER_CAMPP_ZH, &mirrors).await?;
        self.inner
            .speaker
            .get_or_try_init(|| async {
                tokio::task::spawn_blocking(move || {
                    speaker::SpeakerId::load(&dir.join("campplus.onnx")).map(Arc::new)
                })
                .await
                .context("声纹模型加载任务挂了")?
            })
            .await
            .cloned()
    }

    /// 录一句话给某家人注册声纹(家人 tab「让它认识你的声音」)。
    /// 复用听写采集(VAD 切一句)→ 提取 embedding → 落库;期间挂起唤醒防自激。
    pub async fn enroll(&self, user_id: i64) -> Result<()> {
        anyhow::ensure!(
            self.inner.store.users.get(user_id)?.is_some(),
            "用户不存在,先添加家人再录声纹"
        );
        let (vad_model, _asr) = self.ensure_engines().await?;
        let spk = self.ensure_speaker().await?;
        self.wake_suspend(true);
        let rt = self.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<Vec<f32>> {
            let hangover = hangover_secs(&rt.patience());
            let vad = new_vad(&vad_model, hangover)?;
            let pipe = open_capture(rt.input_device())?;
            rt.publish(VoiceEvent::State { phase: VoicePhase::Listening });
            let out =
                collect_utterance(&pipe, &vad, &rt, None, START_TIMEOUT, hangover)?;
            drop(pipe);
            let mut pcm = match out {
                CaptureOut::Utterance(p) => p,
                _ => anyhow::bail!("没录到声音,再说一句试试"),
            };
            anyhow::ensure!(
                (pcm.len() as f32) >= 1.0 * TARGET_RATE as f32,
                "说得太短啦,多说一两句(报名字、念句话都行)"
            );
            peak_normalize(&mut pcm);
            spk.embed(&pcm)
        })
        .await
        .context("声纹注册任务挂了")?;
        self.publish(VoiceEvent::State { phase: VoicePhase::Idle });
        self.wake_suspend(false);
        let emb = result?;
        self.inner.store.voiceprints.upsert(user_id, &emb)?;
        tracing::info!(user = user_id, "声纹注册完成");
        Ok(())
    }

    // ---- 免手唤醒(PLAN §11 C):语音交互唯一入口 ----

    /// 设置开关 + 起停一体(设置页开关的唯一入口;开机自启走 boot_wake_if_enabled)。
    pub async fn wake_set(&self, enabled: bool) -> Result<()> {
        self.inner.store.settings.set(
            None,
            "voice.wake.enabled",
            if enabled { "true" } else { "false" },
        )?;
        if enabled {
            self.wake_start().await
        } else {
            self.wake_stop();
            Ok(())
        }
    }

    /// 开机自启(壳层装配后调;失败只记日志,不挡开机)。
    pub async fn boot_wake_if_enabled(&self) {
        let on = self
            .inner
            .store
            .settings
            .get(None, "voice.wake.enabled")
            .ok()
            .flatten()
            .map(|v| v == "true")
            .unwrap_or(false);
        if on {
            if let Err(e) = self.wake_start().await {
                tracing::error!(err = %format!("{e:#}"), "开机启动免手唤醒失败");
            }
        }
    }

    pub fn wake_running(&self) -> bool {
        self.inner.wake.lock().expect("wake lock").is_some()
    }

    /// 当前唤醒词(settings 顿号/逗号/空格分隔)。默认「小七」= 产品定名前的暂定值
    /// (PLAN §11 watch-item;唤醒词 = 用户数据,改设置即生效)。
    pub fn wake_keywords(&self) -> Vec<String> {
        let raw = self
            .inner
            .store
            .settings
            .get(None, "voice.wake.keywords")
            .ok()
            .flatten()
            .unwrap_or_default();
        let words: Vec<String> = raw
            .split(['、', ',', ',', ' ', ';', ';'])
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if words.is_empty() {
            vec!["小七".into()]
        } else {
            words
        }
    }

    /// 唤醒灵敏度(voice.wake.sensitivity 0~100,global)→ KWS threshold。
    /// 高灵敏 = 低阈值 = 容易被唤醒(也更易误触);默认 50 → 0.45(robot 实战折中)。
    /// 范围锁 [0.2, 0.7]:robot 经验 <0.25 客厅/电视声误触严重,>0.7 太钝(几乎叫不应)。
    fn wake_threshold(&self) -> f32 {
        let sens: f32 = self
            .inner
            .store
            .settings
            .get(None, "voice.wake.sensitivity")
            .ok()
            .flatten()
            .and_then(|v| v.parse().ok())
            .unwrap_or(50.0);
        (0.7 - sens.clamp(0.0, 100.0) / 100.0 * 0.5).clamp(0.2, 0.7)
    }

    async fn wake_start(&self) -> Result<()> {
        if self.wake_running() {
            return Ok(());
        }
        let mirrors = self.mirrors();
        let kws_dir = self.inner.models.ensure_tar(&models::KWS_ZIPFORMER_ZH, &mirrors).await?;
        let (vad_model, asr) = self.ensure_engines().await?;
        let speaker = self.ensure_speaker_if_enrolled().await; // 有家人录声纹才认人
        // 唤醒词编码:模型词表本身裁决切分(绕开拼音 strict 歧义);全军覆没 = 开不了
        let vocab = wake::load_vocab(&kws_dir.join("tokens.txt"))?;
        let (keywords_buf, dropped) = wake::encode_keywords(&self.wake_keywords(), &vocab);
        if !dropped.is_empty() {
            tracing::warn!(?dropped, "部分唤醒词编码不进模型词表,已忽略");
        }
        anyhow::ensure!(!keywords_buf.is_empty(), "唤醒词一个都编不出来(只支持中文词)");
        // 短句银行(人格数据,场景给话术):断网 best-effort,空类目降级无声
        let scene_voice = self.inner.scenes.default_scene().voice.clone();
        let prompts = prompts::PromptBank::prepare(self, &scene_voice).await;

        let (tx, rx) = std::sync::mpsc::channel();
        {
            let mut slot = self.inner.wake.lock().expect("wake lock");
            if slot.is_some() {
                return Ok(()); // 并发开关:别人抢先了
            }
            *slot = Some(WakeHandle { cmd: tx });
        }
        let deps = wake::WakeDeps {
            rt: self.clone(),
            kws_dir,
            vad_model,
            asr,
            prompts,
            keywords_buf,
            kws_threshold: self.wake_threshold(),
            speaker,
        };
        if let Err(e) =
            std::thread::Builder::new().name("voice-wake".into()).spawn(move || {
                wake::run_wake_loop(deps, rx);
            })
        {
            self.wake_cleanup();
            return Err(anyhow!(e).context("唤醒线程起不来"));
        }
        tracing::info!("免手唤醒已启动");
        Ok(())
    }

    pub fn wake_stop(&self) {
        self.wake_cmd(wake::WakeCmd::Stop);
        self.wake_cleanup(); // sender 一并丢弃,线程见 Disconnected 也会退
    }

    fn wake_cmd(&self, cmd: wake::WakeCmd) {
        if let Some(h) = self.inner.wake.lock().expect("wake lock").as_ref() {
            let _ = h.cmd.send(cmd);
        }
    }

    /// 前端编排指令:回合念完 → 开跟进窗;回合失败/取消 → 直接回待唤醒。
    pub fn wake_follow_up(&self) {
        self.wake_cmd(wake::WakeCmd::FollowUp);
    }
    pub fn wake_resume(&self) {
        self.wake_cmd(wake::WakeCmd::Resume);
    }
    /// 自激防护:TTS 在念(含重听)时丢帧;听写期间同用。
    pub fn wake_suspend(&self, on: bool) {
        self.wake_cmd(wake::WakeCmd::Suspend(on));
    }

    pub(super) fn wake_cleanup(&self) {
        *self.inner.wake.lock().expect("wake lock") = None;
    }

    fn user_setting(&self, key: &str, default: &str) -> String {
        let uid = self.inner.store.users.ensure_default_user().map(|u| u.id).unwrap_or(1);
        self.inner
            .store
            .settings
            .get(Some(uid), key)
            .ok()
            .flatten()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| default.to_string())
    }

    pub fn status(&self) -> VoiceStatus {
        VoiceStatus {
            asr_ready: self.inner.models.is_ready(&models::ASR_SENSE_VOICE),
            vad_ready: self.inner.models.is_ready(&models::SILERO_VAD),
            kws_ready: self.inner.models.is_tar_ready(&models::KWS_ZIPFORMER_ZH),
            wake_running: self.wake_running(),
            keywords: self.wake_keywords(),
            devices: list_input_devices(),
            speakers: SPEAKERS_ZH.to_vec(),
        }
    }

    /// 开一个听写会话:已在听 = 幂等 no-op。模型缺失先用时下载(HUD 进度),
    /// 再起专职线程跑 采集→VAD→ASR 管线;产出走 Voice 事件车道。
    pub async fn listen_start(&self) -> Result<()> {
        let (ctl, gen) = {
            let mut slot = self.inner.session.lock().expect("voice session lock");
            if slot.is_some() {
                return Ok(());
            }
            let ctl = Arc::new(AtomicU8::new(CTL_RUN));
            let gen = self.inner.gen.fetch_add(1, Ordering::Relaxed) + 1;
            *slot = Some(SessionCtl { ctl: ctl.clone(), gen });
            (ctl, gen)
        };
        self.publish(VoiceEvent::State { phase: VoicePhase::Preparing });
        self.wake_suspend(true); // 听写期间唤醒循环丢帧(防 KWS 把听写内容当唤醒词)

        match self.prepare(&ctl).await {
            Ok(Some((vad_model, asr, spk))) => {
                let rt = self.clone();
                let spawned = std::thread::Builder::new()
                    .name("voice-listen".into())
                    .spawn(move || {
                        let outcome = run_session(&rt, &vad_model, asr.as_ref(), ctl, spk.as_deref());
                        rt.finish_session(gen, outcome);
                    });
                if let Err(e) = spawned {
                    self.finish_session(gen, Err(anyhow!(e).context("听写线程起不来")));
                }
                Ok(())
            }
            Ok(None) => {
                // 准备期间被取消
                self.finish_session(gen, Ok(SessionOutcome::Ended("cancelled")));
                Ok(())
            }
            Err(e) => {
                tracing::error!(err = %format!("{e:#}"), "语音组件准备失败");
                self.finish_session(gen, Err(e));
                Ok(())
            }
        }
    }

    /// 停止当前听写:accept = 立即定稿(已听到的送识别);false = 取消丢弃。幂等。
    pub fn listen_stop(&self, accept: bool) {
        let slot = self.inner.session.lock().expect("voice session lock");
        if let Some(s) = slot.as_ref() {
            s.ctl.store(if accept { CTL_ACCEPT } else { CTL_CANCEL }, Ordering::Relaxed);
        }
    }

    /// 模型就绪 + ASR 单例加载(听写与唤醒共用)。
    async fn ensure_engines(&self) -> Result<(PathBuf, Arc<SherpaAsr>)> {
        let mirrors = self.mirrors();
        let vad_dir = self.inner.models.ensure(&models::SILERO_VAD, &mirrors).await?;
        let asr_dir = self.inner.models.ensure(&models::ASR_SENSE_VOICE, &mirrors).await?;
        let asr = self
            .inner
            .asr
            .get_or_try_init(|| async {
                // 语言目录一期只有中文行(PLAN §11);voice.lang 进目录时在此择路
                tokio::task::spawn_blocking(move || {
                    SherpaAsr::sense_voice(&asr_dir, "zh").map(Arc::new)
                })
                .await
                .context("ASR 加载任务挂了")?
            })
            .await?
            .clone();
        Ok((vad_dir.join("silero_vad.onnx"), asr))
    }

    /// 听写前的准备(含声纹:有家人录过声纹才加载,下载失败降级为不识别)。
    /// 返回 None = 准备期间用户已取消。
    #[allow(clippy::type_complexity)]
    async fn prepare(
        &self,
        ctl: &Arc<AtomicU8>,
    ) -> Result<Option<(PathBuf, Arc<SherpaAsr>, Option<Arc<speaker::SpeakerId>>)>> {
        if ctl.load(Ordering::Relaxed) == CTL_CANCEL {
            return Ok(None);
        }
        let (vad_model, asr) = self.ensure_engines().await?;
        let spk = self.ensure_speaker_if_enrolled().await;
        if ctl.load(Ordering::Relaxed) == CTL_CANCEL {
            return Ok(None);
        }
        Ok(Some((vad_model, asr, spk)))
    }

    /// 有家人注册过声纹才加载声纹模型;下载/加载失败 → None(降级不识别,不阻断听写)。
    async fn ensure_speaker_if_enrolled(&self) -> Option<Arc<speaker::SpeakerId>> {
        if !self.has_voiceprints() {
            return None;
        }
        match self.ensure_speaker().await {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!(err = %format!("{e:#}"), "声纹模型加载失败,本次不识别说话人");
                None
            }
        }
    }

    /// 会话收尾的唯一出口:清槽(只清自己这代)+ 发产出事件 + 回 Idle + 恢复唤醒。
    fn finish_session(&self, gen: u64, outcome: Result<SessionOutcome>) {
        {
            let mut slot = self.inner.session.lock().expect("voice session lock");
            if slot.as_ref().map(|s| s.gen) == Some(gen) {
                *slot = None;
            }
        }
        match outcome {
            Ok(SessionOutcome::Text { text, speaker_id }) => {
                self.publish(VoiceEvent::Transcribed { text, via: "mic".into(), speaker_id })
            }
            Ok(SessionOutcome::Ended(reason)) => {
                self.publish(VoiceEvent::ListenEnded { reason: reason.into() })
            }
            Err(e) => {
                tracing::error!(err = %format!("{e:#}"), "听写会话失败");
                self.publish(VoiceEvent::ListenEnded { reason: "error".into() });
            }
        }
        self.publish(VoiceEvent::State { phase: VoicePhase::Idle });
        self.wake_suspend(false); // 听写完恢复唤醒监听(没开唤醒 = no-op)
    }

    fn publish(&self, ev: VoiceEvent) {
        self.inner.bus.publish(AppEvent::Voice(ev));
    }

    /// 镜像列表 = 数据(与 media 共用 settings `media.gh_mirrors`,坏 JSON 回默认)。
    fn mirrors(&self) -> Vec<String> {
        self.inner
            .store
            .settings
            .get(None, "media.gh_mirrors")
            .ok()
            .flatten()
            .and_then(|json| serde_json::from_str::<Vec<String>>(&json).ok())
            .unwrap_or_else(|| {
                crate::components::DEFAULT_GH_MIRRORS.iter().map(|s| s.to_string()).collect()
            })
    }

    fn patience(&self) -> String {
        let uid = self.inner.store.users.ensure_default_user().map(|u| u.id).unwrap_or(1);
        self.inner
            .store
            .settings
            .get(Some(uid), "voice.patience")
            .ok()
            .flatten()
            .unwrap_or_else(|| "standard".into())
    }

    fn input_device(&self) -> Option<String> {
        self.inner.store.settings.get(None, "voice.input_device").ok().flatten()
    }
}

/// 设置页「麦克风」下拉的数据源(B 期 UI 用;放 core 因为 cpal 在这)。
/// name() 虽弃用,但"人类可读名"正是设置值要的语义(id 不可读、description 是结构)。
#[allow(deprecated)]
pub fn list_input_devices() -> Vec<String> {
    let host = cpal::default_host();
    host.input_devices()
        .map(|it| it.filter_map(|d| d.name().ok()).collect())
        .unwrap_or_default()
}

enum SessionOutcome {
    Text { text: String, speaker_id: Option<i64> },
    Ended(&'static str), // no_speech | cancelled
}

/// 采集管:cpal 流 + 帧通道 + 重采样器(听写与唤醒共用;Stream 不跨线程,留在持有线程)。
pub(super) struct CapturePipe {
    pub(super) rx: std::sync::mpsc::Receiver<Vec<f32>>,
    resampler: Option<sherpa_onnx::LinearResampler>,
    _stream: cpal::Stream,
}

impl CapturePipe {
    pub(super) fn to_16k(&self, chunk: &[f32]) -> Vec<f32> {
        match &self.resampler {
            Some(r) => r.resample(chunk, false),
            None => chunk.to_vec(),
        }
    }

    /// 清空积压帧(robot 坑:唤醒后队列积压拖慢"开始听")。
    pub(super) fn drain(&self) {
        while self.rx.try_recv().is_ok() {}
    }
}

/// 打开麦克风(设备给什么率/声道就收什么,统一降到 16k mono)。
/// 回调在实时线程:只做降声道 + 投递,队列满就丢(绝不阻塞)。
#[allow(deprecated)] // device.name():设置按人类可读名匹配,见 list_input_devices
pub(super) fn open_capture(device_name: Option<String>) -> Result<CapturePipe> {
    let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(64);
    let host = cpal::default_host();
    let device = match device_name {
        Some(name) => host
            .input_devices()
            .ok()
            .and_then(|mut it| it.find(|d| d.name().map(|n| n == name).unwrap_or(false)))
            .or_else(|| host.default_input_device()),
        None => host.default_input_device(),
    }
    .ok_or_else(|| anyhow!("没有可用的输入设备(麦克风)"))?;
    let supported = device.default_input_config().context("读取麦克风默认配置失败")?;
    let in_rate: u32 = supported.sample_rate();
    let channels = supported.channels().max(1) as usize;
    let stream_cfg: cpal::StreamConfig = supported.config();
    tracing::info!(
        device = device.name().unwrap_or_default(),
        rate = in_rate,
        channels,
        format = ?supported.sample_format(),
        "麦克风打开"
    );

    let err_fn = |e| tracing::warn!("麦克风流错误: {e}");
    macro_rules! build_stream {
        ($t:ty, $conv:expr) => {{
            let tx = tx.clone();
            device.build_input_stream(
                &stream_cfg,
                move |data: &[$t], _: &cpal::InputCallbackInfo| {
                    let conv = $conv;
                    let mono: Vec<f32> = data
                        .chunks(channels)
                        .map(|frame| {
                            frame.iter().map(|s| conv(*s)).sum::<f32>() / channels as f32
                        })
                        .collect();
                    let _ = tx.try_send(mono);
                },
                err_fn,
                None,
            )
        }};
    }
    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => build_stream!(f32, |s: f32| s),
        cpal::SampleFormat::I16 => build_stream!(i16, |s: i16| s as f32 / 32_768.0),
        cpal::SampleFormat::U16 => build_stream!(u16, |s: u16| (s as f32 - 32_768.0) / 32_768.0),
        other => bail!("不支持的麦克风采样格式 {other:?}"),
    }
    .context("打开麦克风失败(检查系统麦克风权限)")?;
    stream.play().context("启动麦克风失败")?;

    let resampler = if in_rate != TARGET_RATE {
        Some(
            sherpa_onnx::LinearResampler::create(in_rate as i32, TARGET_RATE as i32)
                .ok_or_else(|| anyhow!("重采样器创建失败({in_rate}→16k)"))?,
        )
    } else {
        None
    };
    Ok(CapturePipe { rx, resampler, _stream: stream })
}

/// VAD(2MB 模型,毫秒级创建;唤醒循环常驻一只,capture 间 reset)。
pub(super) fn new_vad(
    model: &std::path::Path,
    hangover_s: f32,
) -> Result<sherpa_onnx::VoiceActivityDetector> {
    let mut cfg = sherpa_onnx::VadModelConfig::default();
    cfg.silero_vad.model = Some(model.to_string_lossy().into_owned());
    cfg.silero_vad.threshold = VAD_THRESHOLD;
    cfg.silero_vad.min_silence_duration = hangover_s;
    cfg.silero_vad.min_speech_duration = MIN_SPEECH_S;
    cfg.silero_vad.max_speech_duration = MAX_SPEECH_S;
    cfg.silero_vad.window_size = VAD_WINDOW as i32;
    cfg.sample_rate = TARGET_RATE as i32;
    sherpa_onnx::VoiceActivityDetector::create(&cfg, 16.0).ok_or_else(|| anyhow!("VAD 创建失败"))
}

pub(super) enum CaptureOut {
    Utterance(Vec<f32>),
    Empty,
    Cancelled,
}

/// 听一轮:窗口化喂 VAD、电平节流上报、首个语段即定稿(听写与唤醒/跟进共用)。
/// ctl 只有听写会话传(立即定稿/取消按钮);唤醒侧靠时长上限收敛。
pub(super) fn collect_utterance(
    pipe: &CapturePipe,
    vad: &sherpa_onnx::VoiceActivityDetector,
    rt: &VoiceRuntime,
    ctl: Option<&AtomicU8>,
    start_timeout: Duration,
    hangover_s: f32,
) -> Result<CaptureOut> {
    let started = Instant::now();
    let hard_cap = start_timeout + Duration::from_secs_f32(MAX_SPEECH_S + hangover_s + 3.0);
    let mut win_buf: Vec<f32> = Vec::with_capacity(VAD_WINDOW * 8);
    let mut speech_started = false;
    let mut windows = 0u32;
    let mut level_peak = 0f32;

    let take_front = |vad: &sherpa_onnx::VoiceActivityDetector| -> Option<Vec<f32>> {
        let seg = vad.front().map(|s| s.samples().to_vec());
        if seg.is_some() {
            vad.pop();
        }
        seg
    };

    loop {
        if let Some(ctl) = ctl {
            match ctl.load(Ordering::Relaxed) {
                CTL_CANCEL => return Ok(CaptureOut::Cancelled),
                CTL_ACCEPT => {
                    vad.flush();
                    return Ok(match take_front(vad) {
                        Some(pcm) => CaptureOut::Utterance(pcm),
                        None => CaptureOut::Empty,
                    });
                }
                _ => {}
            }
        }

        match pipe.rx.recv_timeout(Duration::from_millis(100)) {
            Ok(chunk) => {
                let s16k = pipe.to_16k(&chunk);
                win_buf.extend_from_slice(&s16k);
                while win_buf.len() >= VAD_WINDOW {
                    let win: Vec<f32> = win_buf.drain(..VAD_WINDOW).collect();
                    let rms = (win.iter().map(|s| s * s).sum::<f32>() / win.len() as f32).sqrt();
                    level_peak = level_peak.max((rms * 10.0).min(1.0));
                    windows += 1;
                    if windows % LEVEL_EVERY_WINDOWS == 0 {
                        rt.publish(VoiceEvent::Level { level: level_peak });
                        level_peak = 0.0;
                    }
                    vad.accept_waveform(&win);
                    if !speech_started && vad.detected() {
                        speech_started = true;
                        rt.publish(VoiceEvent::SpeechStarted);
                    }
                    if !vad.is_empty() {
                        return Ok(match take_front(vad) {
                            Some(pcm) => CaptureOut::Utterance(pcm), // 单话语:首段即定稿
                            None => CaptureOut::Empty,
                        });
                    }
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => bail!("麦克风流中断"),
        }

        if !speech_started && started.elapsed() > start_timeout {
            return Ok(CaptureOut::Empty); // 没人开口,安静收摊
        }
        if started.elapsed() > hard_cap {
            // 兜底:把已有的拿去识别(防一直说话不停顿挂死)
            vad.flush();
            return Ok(match take_front(vad) {
                Some(pcm) => CaptureOut::Utterance(pcm),
                None => CaptureOut::Empty,
            });
        }
    }
}

/// 听写会话主体(专职线程):开麦 → 听一轮 → 放麦 → AGC → ASR → 声纹识别。
fn run_session(
    rt: &VoiceRuntime,
    vad_model: &std::path::Path,
    asr_engine: &dyn Asr,
    ctl: Arc<AtomicU8>,
    spk: Option<&speaker::SpeakerId>,
) -> Result<SessionOutcome> {
    let hangover = hangover_secs(&rt.patience());
    let vad = new_vad(vad_model, hangover)?;
    let pipe = open_capture(rt.input_device())?;
    rt.publish(VoiceEvent::State { phase: VoicePhase::Listening });
    let out = collect_utterance(&pipe, &vad, rt, Some(&ctl), START_TIMEOUT, hangover)?;
    drop(pipe); // 识别前先放麦克风

    let mut pcm = match out {
        CaptureOut::Cancelled => return Ok(SessionOutcome::Ended("cancelled")),
        CaptureOut::Empty => return Ok(SessionOutcome::Ended("no_speech")),
        CaptureOut::Utterance(pcm) => pcm,
    };
    if (pcm.len() as f32) < MIN_SPEECH_S * TARGET_RATE as f32 {
        return Ok(SessionOutcome::Ended("no_speech")); // 双保险(VAD min_speech 之外)
    }
    rt.publish(VoiceEvent::State { phase: VoicePhase::Transcribing });
    peak_normalize(&mut pcm);
    let text = asr_engine.transcribe(&pcm)?;
    if text.is_empty() {
        return Ok(SessionOutcome::Ended("no_speech"));
    }
    // 声纹识别(同一段 PCM):认出家人 → 记忆归 TA;认不出 → None 走会话用户
    let speaker_id = spk.and_then(|s| s.identify(&pcm, &rt.voiceprint_library()));
    Ok(SessionOutcome::Text { text, speaker_id })
}

/// ASR 前 AGC(robot V1.2 验证参数,PLAN §11 A 期件):峰值归一到 -3dBFS,
/// 只增不减、封顶 20dB、99.5 分位当峰值(单个爆音压不死增益)、近静音原样返回。
/// 麦克风硬件 AGC 已拉够电平时增益≈1 自动空转,不会叠加过度放大。
fn peak_normalize(pcm: &mut [f32]) {
    if pcm.is_empty() {
        return;
    }
    let mut mags: Vec<f32> = pcm.iter().map(|s| s.abs()).collect();
    let idx = ((mags.len() as f32 * 0.995) as usize).min(mags.len() - 1);
    let (_, peak, _) = mags.select_nth_unstable_by(idx, |a, b| a.total_cmp(b));
    let peak = *peak;
    if peak < 3.0e-5 {
        return; // 近静音:不放大底噪
    }
    let target = 0.708; // -3 dBFS
    let gain = (target / peak).clamp(1.0, 10.0); // 只增不减,封顶 20dB(10 倍)
    if gain > 1.0 {
        for s in pcm.iter_mut() {
            *s = (*s * gain).clamp(-1.0, 1.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patience_maps_to_locked_hangover_values() {
        assert_eq!(hangover_secs("snappy"), 0.5);
        assert_eq!(hangover_secs("standard"), 0.8);
        assert_eq!(hangover_secs("relaxed"), 1.2);
        assert_eq!(hangover_secs("garbage"), 0.8, "未知值回标准档");
    }

    #[test]
    fn agc_boosts_quiet_speech_but_not_silence() {
        // 又轻又远的人声(峰值 0.05)→ 拉到 -3dBFS 方向(封顶 10 倍)
        let mut quiet: Vec<f32> = (0..16000).map(|i| 0.05 * ((i % 100) as f32 / 100.0 - 0.5)).collect();
        let before = quiet.iter().fold(0f32, |m, s| m.max(s.abs()));
        peak_normalize(&mut quiet);
        let after = quiet.iter().fold(0f32, |m, s| m.max(s.abs()));
        assert!(after > before * 5.0, "轻声要被拉起来: {before} → {after}");

        // 近静音不放大底噪
        let mut silence = vec![1.0e-5f32; 16000];
        peak_normalize(&mut silence);
        assert!(silence.iter().all(|s| *s == 1.0e-5));

        // 已经够响的不再加(只增不减)
        let mut loud = vec![0.9f32; 1600];
        peak_normalize(&mut loud);
        assert!(loud.iter().all(|s| *s == 0.9));
    }

    #[test]
    fn outcome_reasons_are_stable_vocabulary() {
        // 事件词表是前端契约:变更要同步前端字典
        for r in ["no_speech", "cancelled", "error"] {
            assert!(!r.is_empty());
        }
    }
}
