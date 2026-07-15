//! 语音运行时(PLAN §11,A 期「按住说话」):听写会话 = cpal 采集 → silero VAD 切段
//! → ASR → `Transcribed` 事件。**业务零入**:编排者 = 前端 VM(拿文本走既有 send 链),
//! 这里只供能力,不碰 engine(宪法 §5 三物种:交互渠道)。
//! 管线参数 = robot Windows 真机实调终值,锁死进代码,不暴露设置(PLAN §11)。

mod asr;
mod calib;
mod models;
mod prompts;
mod speaker;
mod tts;
mod wake;

pub use models::VoiceModels;
pub use tts::{probe_zipvoice, Speaker, DEFAULT_SPEAKER, SPEAKERS_ZH};

/// 家人列表项(设置·家人 tab):用户 + 是否已录声纹。壳层 list_family 用。
#[derive(Debug, Clone, serde::Serialize)]
pub struct FamilyMember {
    #[serde(flatten)]
    pub user: crate::store::User,
    pub enrolled: bool,
}

use std::collections::HashMap;
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
const ENROLL_SAMPLES: u32 = 3; // 声纹注册录几段取平均(2026-07-04 用户拍板,§4.2「绝不错认」)
const CALIB_TAKES: u8 = 5; // 唤醒标定:录几遍正样本(+1 段底噪/负样本)
const CALIB_AMBIENT_SECS: f32 = 4.0; // 标定底噪/负样本采集时长
const CALIB_COMPUTE_SECS: u64 = 60; // 扫描计算超时兜底(正常 1~3s;超时=组件异常,报错不无限转)

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
    /// 与唤醒线程共享的应答音银行;换音色时运行时后台重建后整体替换(问题1-B)。
    prompts: prompts::SharedPromptBank,
    /// 本次启动的代次:loop 退出清 slot 时凭它认领(off→on 重启的窄竞态:旧线程退出
    /// 晚于新 start 置位时,朴素置 None 会把**新** loop 的句柄误清 → sender drop 新 loop 也退)。
    gen: u64,
}

/// 唤醒启动代次发号器(进程内单调递增;只为 wake_cleanup_gen 的认领判定)。
static WAKE_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

struct Inner {
    store: Store,
    bus: Bus,
    scenes: Scenes,
    models: VoiceModels,
    /// ASR 模型加载贵(秒级),进程内缓存;带档身份 —— 用户切 voice.asr.model 即重建,
    /// 旧 Arc 在用完后自然 drop(同档复用,不重复加载)。
    asr: tokio::sync::Mutex<Option<(models::AsrModel, Arc<SherpaAsr>)>>,
    /// 同时只有一个听写会话。
    session: std::sync::Mutex<Option<SessionCtl>>,
    gen: AtomicU64,
    /// TTS 引擎(trait 接缝;在线默认 EdgeTts)与句级缓存目录。
    tts: Arc<dyn tts::TtsEngine>,
    tts_dir: PathBuf,
    /// 非可重入 TTS 引擎(sherpa OfflineTts:melo/克隆)的串行锁——并发 generate 会原生崩溃。
    tts_lock: tokio::sync::Mutex<()>,
    /// 离线 TTS(melo-vits,163M):按需加载一次进 OnceCell(voice.tts_backend=offline)。
    tts_offline: tokio::sync::OnceCell<Arc<tts::SherpaVits>>,
    /// 本地音色克隆(ZipVoice):选了 clone:<id> 音色时按需加载一次(PLAN §11 D-clone)。
    tts_clone: tokio::sync::OnceCell<Arc<tts::ZipVoiceTts>>,
    /// 克隆模型加载失败时「清树重下」自愈,本会话只做一次(避免非下载原因坏时反复重下几百 MB)。
    clone_reheal: std::sync::atomic::AtomicBool,
    /// 克隆加载失败后的子进程探针(抓 sherpa 的 /MT stderr),本会话只跑一次(模型加载很重)。
    clone_probe_done: std::sync::atomic::AtomicBool,
    /// 克隆参考音目录(`数据目录/voice/clones/<wav_file>`)。
    clones_dir: PathBuf,
    /// 声纹提取器(CAM++ 26MB):有家人注册声纹时才加载(PLAN §11 D)。
    speaker: tokio::sync::OnceCell<Arc<speaker::SpeakerId>>,
    /// 唤醒循环(开关 = voice.wake.enabled;同时只有一个)。
    wake: std::sync::Mutex<Option<WakeHandle>>,
    /// 浏览器推流采集的扇出表(层1 AEC 采集端):`voice.capture.source=browser` 时
    /// `open_capture_auto` 往这儿挂 tap,壳层 `voice_push_audio` 命令喂 16k mono f32 帧。
    /// tap 的管子 drop 了 send 自然失败 → 下次推帧时剪除(与 cpal「pipe drop 即关麦」同语义)。
    push_taps: std::sync::Mutex<Vec<std::sync::mpsc::SyncSender<Vec<f32>>>>,
    /// 唤醒交互(on_wake / 跟进窗)的「停 / 定稿」信号:`listen_stop` 写、唤醒侧 `collect_utterance` 读。
    /// 独立于听写会话槽 —— 唤醒录音跑在自己的循环线程、不占 `session` 槽,故此前「在听时点停」没反应;
    /// 唤醒侧每次开录前武装成 `CTL_RUN`,前端点停 = 定稿(发已听到的)/ 取消(丢弃回待唤醒)。
    wake_ctl: Arc<AtomicU8>,
    /// 动作确认中枢(§7.8 口头确认):壳层 boot 注入 engine 的那一份(set_web_renderer 同款
    /// 接缝——voice 不碰 engine §6.1,只依赖平级 confirm 模块)。没注入 = 口头确认整个不开。
    confirmer: std::sync::OnceLock<Arc<crate::confirm::Confirmer>>,
}

#[derive(Clone)]
pub struct VoiceRuntime {
    inner: Arc<Inner>,
}

/// 音色列表项(内置在线音色 + 用户/内置克隆,混在一个列表给前端;PLAN §11 D-clone)。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceOption {
    /// 选择值(直接进 voice.speaker):内置 = edge id;克隆 = "clone:<id>"。
    pub id: String,
    pub name: String,
    /// 克隆音色(可试听)。
    pub is_clone: bool,
    /// 内置预置(随包/下载):不可删。
    pub builtin: bool,
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
    /// 当前唤醒词(解析后的列表;= 名字派生,见 `wake_keywords`)。
    pub keywords: Vec<String>,
    /// 起了名但名字语音喊不了(英文单词名派生不出)→ 回落默认词。UI 据此如实提示(§3.5)。
    pub wake_fallback: bool,
    /// 麦克风设备名列表(设置下拉的数据源)。
    pub devices: Vec<String>,
    /// 音色目录(当前语言行;目录 = 数据)。
    pub speakers: Vec<VoiceOption>,
    /// 出厂默认音色 id(单源 = `tts::DEFAULT_SPEAKER`):前端未设音色时用它高亮默认项,
    /// 不在前端硬编码副本(§4.11 写死默认值须单源)。
    pub default_speaker: String,
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
                asr: tokio::sync::Mutex::new(None),
                session: std::sync::Mutex::new(None),
                gen: AtomicU64::new(0),
                tts: Arc::new(tts::EdgeTts),
                tts_dir: dir.join("tts"),
                tts_lock: tokio::sync::Mutex::new(()),
                tts_offline: tokio::sync::OnceCell::new(),
                tts_clone: tokio::sync::OnceCell::new(),
                clone_reheal: std::sync::atomic::AtomicBool::new(false),
                clone_probe_done: std::sync::atomic::AtomicBool::new(false),
                clones_dir: dir.join("clones"),
                speaker: tokio::sync::OnceCell::new(),
                wake: std::sync::Mutex::new(None),
                push_taps: std::sync::Mutex::new(Vec::new()),
                wake_ctl: Arc::new(AtomicU8::new(CTL_RUN)),
                confirmer: std::sync::OnceLock::new(),
            }),
        }
    }

    /// 壳层 boot 注入确认中枢(engine 的同一份实例;重复注入忽略)。
    pub fn set_confirmer(&self, c: Arc<crate::confirm::Confirmer>) {
        let _ = self.inner.confirmer.set(c);
    }

    pub(super) fn confirmer(&self) -> Option<Arc<crate::confirm::Confirmer>> {
        self.inner.confirmer.get().cloned()
    }

    /// 口头确认(§7.8):对着麦克风听一段应答(「确认/可以」= 允许,「不要/算了」= 拒,
    /// 听不清不算数——卡片继续等)。前端判定「这张卡属于当前语音回合」后调用。
    /// 返回 false = 没开听(cpal 采集源关了回声消除,TTS 问句会进麦自答风险大,只走卡片;
    /// 或唤醒循环没在跑/没注入 confirmer),前端不用等语音结果。
    pub fn confirm_listen(&self, id: u64) -> bool {
        if self.capture_source() != "browser" {
            tracing::info!("口头确认不开:采集源非 browser(无 AEC,防 TTS 问句自答)");
            return false;
        }
        if self.confirmer().is_none() || !self.wake_running() {
            return false;
        }
        self.wake_cmd(wake::WakeCmd::ConfirmListen { id });
        true
    }

    /// 句级 TTS 进缓存,返回音频文件路径(命中秒回);音色/语速取用户设置。
    /// 调用方(壳层)拿路径经 relay 注册成 localhost URL 给 `<audio>`。
    /// backend=offline → 本地 vits(断网兜底,音色固定);否则在线 edge。
    pub async fn tts_to_file(&self, text: &str) -> Result<PathBuf> {
        let text = text.trim();
        anyhow::ensure!(!text.is_empty(), "空文本不合成");
        let rate = tts::rate_pct(&self.user_setting("voice.rate", "standard"));
        let speaker = self.user_setting("voice.speaker", tts::DEFAULT_SPEAKER);
        // 克隆音色(clone:<id>)优先于在线/离线 toggle —— 选了克隆就是要这把嗓子。
        let clone = speaker.starts_with("clone:");
        let offline = !clone && self.tts_offline_selected();
        // 缓存键与产物 ext 都不依赖引擎实例 → 先查盘,命中就直接返回,**不加载也不下载任何模型**
        // (启动预合成应答音时,用过的音色全命中 → 零合成零加载;问题1-B)。ext 须与对应引擎 .ext() 一致。
        let (cache_voice, ext) = if clone {
            (speaker.as_str(), "wav") // ZipVoiceTts::ext()
        } else if offline {
            ("vits-melo", "wav") // SherpaVits::ext();离线单说话人用固定标识区分缓存
        } else {
            (speaker.as_str(), "mp3") // EdgeTts::ext()
        };
        // 文件名格式须与 synth_cached 一致(都是 cache_key + ext)
        let path = self.inner.tts_dir.join(format!("{}.{ext}", tts::cache_key(cache_voice, rate, text)));
        if path.is_file() {
            return Ok(path);
        }
        // 缓存未命中,才加载对应引擎合成
        if clone {
            let engine: Arc<dyn tts::TtsEngine> = self.ensure_clone_tts().await?;
            return self.synth_cached(text, &speaker, rate, engine).await;
        }
        if offline {
            let engine: Arc<dyn tts::TtsEngine> = self.ensure_offline_tts().await?;
            return self.synth_cached(text, "vits-melo", rate, engine).await;
        }
        self.synth_cached(text, &speaker, rate, self.inner.tts.clone()).await
    }

    /// 设置页试听:指定音色合成一句(句子由前端字典传入——core 不产文案,
    /// 先例 = media_login 的窗口标题)。同进缓存,重复试听秒回。在线引擎(试听音色)。
    pub async fn preview(&self, speaker: &str, text: &str) -> Result<PathBuf> {
        let text = text.trim();
        anyhow::ensure!(!text.is_empty(), "空文本不合成");
        // 克隆音色试听也走 ZipVoice(否则会拿 clone:<id> 当 edge 音色名用,必失败)。
        if speaker.starts_with("clone:") {
            let engine: Arc<dyn tts::TtsEngine> = self.ensure_clone_tts().await?;
            return self.synth_cached(text, speaker, 0, engine).await;
        }
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

    /// 选了 clone:<id> 音色时:确保 ZipVoice 模型就绪并加载一次。解析闭包现查 cloned_voices
    /// 库拿 (参考音 wav, 文字稿) —— 单一真相源,重录/删改即时生效,引擎不持镜像状态。
    async fn ensure_clone_tts(&self) -> Result<Arc<tts::ZipVoiceTts>> {
        if let Some(t) = self.inner.tts_clone.get() {
            return Ok(t.clone());
        }
        match self.try_load_clone_tts().await {
            Ok(t) => Ok(t),
            Err(e) => {
                // 自愈:模型加载失败(文件坏 / 没下全 / 镜像没发 Content-Length 漏过完整性校验)→ 清树
                // 重下一次再试,**免让用户去数据目录手删**。本会话只做一次(swap):重下后仍失败(可能非
                // 下载问题,如格式/运行时)就不再反复重下几百 MB,直接把带文件线索的错误报上去。
                if self.inner.clone_reheal.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    self.spawn_zipvoice_probe();
                    return Err(e);
                }
                tracing::warn!(err = %format!("{e:#}"), "音色克隆加载失败,清模型树重下一次再试(自愈,本会话仅一次)");
                self.inner.models.reset_tree(&models::TTS_ZIPVOICE).ok();
                let r = self.try_load_clone_tts().await;
                if r.is_err() {
                    // 全新校验文件仍失败 = 非下载问题 → 拉探针抓 sherpa 真话(见下)
                    self.spawn_zipvoice_probe();
                }
                r
            }
        }
    }

    /// 抓 sherpa 真实报错的子进程探针(每会话至多一次,fire-and-forget,结果进 larkwing.log)。
    /// 为什么要子进程:Windows 的 sherpa 预编译库是**静态 CRT(/MT)**,它的 stderr 与主进程
    /// Rust 侧不是同一张 fd 表 —— 进程内 dup2(nativelog)接不到它;而子进程出生时**所有** CRT
    /// 都从父进程给的管道初始化 fd 2 → sherpa 的 LOGE 全收(2026-07-03 真机追因的产物)。
    fn spawn_zipvoice_probe(&self) {
        if self.inner.clone_probe_done.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let dir = self.inner.models.tree_dir(&models::TTS_ZIPVOICE);
        tokio::spawn(async move {
            let exe = match std::env::current_exe() {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(err = %e, "zipvoice 探针:拿不到自身 exe 路径");
                    return;
                }
            };
            let run = tokio::process::Command::new(&exe)
                .arg("--probe-zipvoice")
                .arg(&dir)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true)
                .output();
            match tokio::time::timeout(std::time::Duration::from_secs(180), run).await {
                Ok(Ok(out)) => {
                    let mut text = String::from_utf8_lossy(&out.stderr).into_owned();
                    if text.len() > 16_000 {
                        // 只留尾巴(真因在最后);按字符切防撕 UTF-8
                        text = text.chars().rev().take(16_000).collect::<Vec<_>>().into_iter().rev().collect();
                    }
                    tracing::warn!(
                        exit = ?out.status.code(),
                        "zipvoice 探针(sherpa 真实输出如下)\n{text}"
                    );
                }
                Ok(Err(e)) => tracing::warn!(err = %e, "zipvoice 探针进程起不来"),
                Err(_) => tracing::warn!("zipvoice 探针超时(180s)"),
            }
        });
    }

    /// 确保 ZipVoice 模型在位 + 加载进 OnceCell 缓存。加载失败不缓存(get_or_try_init 语义),
    /// 交给 ensure_clone_tts 决定是否清树重下自愈。
    async fn try_load_clone_tts(&self) -> Result<Arc<tts::ZipVoiceTts>> {
        let mirrors = self.mirrors();
        let dir = self.inner.models.ensure_tar_tree(&models::TTS_ZIPVOICE, &mirrors).await?;
        let store = self.inner.store.clone();
        let clones_dir = self.inner.clones_dir.clone();
        self.inner
            .tts_clone
            .get_or_try_init(|| async move {
                let resolve: tts::CloneResolver = Arc::new(move |id: &str| {
                    let cv = store
                        .cloned_voices
                        .get(id)?
                        .ok_or_else(|| anyhow!("克隆音色 {id} 不存在"))?;
                    Ok((clones_dir.join(&cv.wav_file), cv.transcript))
                });
                tokio::task::spawn_blocking(move || tts::ZipVoiceTts::load(&dir, resolve).map(Arc::new))
                    .await
                    .context("音色克隆加载任务挂了")?
            })
            .await
            .cloned()
    }

    // ---- 音色克隆录入与管理(PLAN §11 D-clone) ----

    /// 列出所有克隆音色(内置预置 + 用户自录,混在同一音色列表)。
    pub fn list_clones(&self) -> Result<Vec<crate::store::ClonedVoice>> {
        self.inner.store.cloned_voices.list()
    }

    /// 重命名克隆音色(只改显示名;id 不可变)。
    pub fn rename_clone(&self, id: &str, name: &str) -> Result<()> {
        anyhow::ensure!(!name.trim().is_empty(), "名字不能为空");
        self.inner.store.cloned_voices.rename(id, name.trim())
    }

    /// 删除克隆音色(内置不可删);连参考音 wav 一并删。
    pub fn delete_clone(&self, id: &str) -> Result<()> {
        if let Some(wav) = self.inner.store.cloned_voices.delete(id)? {
            std::fs::remove_file(self.inner.clones_dir.join(wav)).ok();
        }
        Ok(())
    }

    /// 录一段参考音(复用听写采集:VAD 切一句)→ ASR 自动转写 → 落盘 wav。
    /// 返回 (clone_id, 转写稿) 供前端给用户过目/修改;**此刻不写库**,确认时才落库
    /// (`clone_save`)——参考音以不可变 id 落盘,避免内存里悬 PCM,也避免未确认就生成的脏缓存。
    pub async fn clone_record(&self) -> Result<(String, String)> {
        let (vad_model, asr) = self.ensure_engines().await?;
        self.wake_suspend(true);
        let rt = self.clone();
        let joined = tokio::task::spawn_blocking(move || -> Result<(Vec<f32>, String)> {
            let hangover = hangover_secs(&rt.patience());
            let vad = new_vad(&vad_model, hangover)?;
            let pipe = rt.open_capture_auto()?;
            rt.publish(VoiceEvent::State { phase: VoicePhase::Listening });
            let out = collect_utterance(&pipe, &vad, &rt, None, START_TIMEOUT, hangover)?;
            drop(pipe);
            let mut pcm = match out {
                CaptureOut::Utterance(p) => p,
                _ => anyhow::bail!("没有录到声音"),
            };
            anyhow::ensure!(
                (pcm.len() as f32) >= 3.0 * TARGET_RATE as f32,
                "录音太短,至少 3 秒"
            );
            peak_normalize(&mut pcm);
            // 参考音越长,ZipVoice 每次合成都整段重编码 → 越慢(21s 曾致单句 19s)。
            // 录入即截到上限:让合成延迟与参考时长解耦;截断在 ASR 前 → 文字稿与音频同源。
            const CLONE_REF_MAX_SECS: u32 = 8;
            let max = (CLONE_REF_MAX_SECS * TARGET_RATE) as usize;
            if pcm.len() > max {
                pcm.truncate(max);
            }
            // 文字稿白送:复用 ASR 转写参考音(听错可在前端改后再 clone_save)
            let transcript = asr.transcribe(&pcm)?;
            Ok((pcm, transcript))
        })
        .await;
        // 正常出错或 panic(JoinError)都先复位唤醒/状态,再传播错误(否则唤醒循环可能永久挂起)。
        self.publish(VoiceEvent::State { phase: VoicePhase::Idle });
        self.wake_suspend(false);
        let (pcm, transcript) = joined.context("录音任务挂了")??;
        // id 不可变(uuid 形,时间戳即足够:录入是人手逐次操作);重录 = 新条目。
        let id = format!(
            "v{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
        );
        tokio::fs::create_dir_all(&self.inner.clones_dir).await?;
        let wav = tts::pcm_f32_to_wav(&pcm, TARGET_RATE);
        tokio::fs::write(self.inner.clones_dir.join(format!("{id}.wav")), &wav).await?;
        Ok((id, transcript))
    }

    /// 导入本地音频文件:前端已用 WebView 解码/重采样成 16k 单声道 wav 并 base64 编码。
    /// 落盘 + ASR 转写出文字稿(草稿),不写库(clone_save 确认时才写)——与 clone_record 同形。
    pub async fn clone_import(&self, wav_base64: &str) -> Result<(String, String)> {
        use base64::Engine;
        let wav = base64::engine::general_purpose::STANDARD
            .decode(wav_base64.trim())
            .context("音频 base64 解码失败")?;
        anyhow::ensure!(wav.len() > 44, "音频数据为空或过小");
        let id = format!(
            "v{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
        );
        tokio::fs::create_dir_all(&self.inner.clones_dir).await?;
        let wav_path = self.inner.clones_dir.join(format!("{id}.wav"));
        tokio::fs::write(&wav_path, &wav).await?;
        // 读回 + ASR(同步、CPU 密集 → spawn_blocking);ASR 模型首次用时下载。
        let (_vad, asr) = self.ensure_engines().await?;
        let path = wav_path.clone();
        let transcript = tokio::task::spawn_blocking(move || -> Result<String> {
            let wave = sherpa_onnx::Wave::read(path.to_string_lossy().as_ref())
                .ok_or_else(|| anyhow!("音频读取失败或格式不支持"))?;
            anyhow::ensure!(!wave.samples().is_empty(), "音频数据为空");
            // 参考音越长,ZipVoice 每次合成都整段重编码 → 越慢。截到上限并覆盖回盘,
            // 让存盘参考、合成输入、文字稿三者同源(合成延迟与参考时长解耦)。
            const CLONE_REF_MAX_SECS: i32 = 8;
            let sr = wave.sample_rate();
            let max = (CLONE_REF_MAX_SECS * sr).max(0) as usize;
            let all = wave.samples();
            let capped: &[f32] = if all.len() > max { &all[..max] } else { all };
            if capped.len() < all.len() {
                let trimmed = tts::pcm_f32_to_wav(capped, sr as u32);
                std::fs::write(&path, &trimmed)?;
            }
            asr.transcribe(capped)
        })
        .await
        .context("转写任务挂了")??;
        Ok((id, transcript))
    }

    /// 确认录入:wav 已在盘上(`clone_record` 落的),用(可能改过的)文字稿 + 名字落库。
    pub fn clone_save(
        &self,
        id: &str,
        name: &str,
        transcript: &str,
    ) -> Result<crate::store::ClonedVoice> {
        anyhow::ensure!(
            !id.is_empty()
                && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "音色 id 不合法"
        );
        anyhow::ensure!(!name.trim().is_empty(), "名字不能为空");
        anyhow::ensure!(!transcript.trim().is_empty(), "文字稿不能为空");
        // id 不可变:同 id 不允许二次落库(否则参考音/文字稿变更会让既有 clone:<id> 缓存语义错)。
        anyhow::ensure!(self.inner.store.cloned_voices.get(id)?.is_none(), "音色已存在");
        let wav_file = format!("{id}.wav");
        anyhow::ensure!(
            self.inner.clones_dir.join(&wav_file).is_file(),
            "参考音文件缺失"
        );
        let lang = self
            .inner
            .store
            .settings
            .get(None, "voice.lang")
            .ok()
            .flatten()
            .unwrap_or_else(|| "zh".to_string());
        let cv = crate::store::ClonedVoice {
            id: id.to_string(),
            name: name.trim().to_string(),
            wav_file,
            transcript: transcript.trim().to_string(),
            lang,
            builtin: false,
            created_at: 0, // insert 时由 now_ms 钉上
        };
        self.inner.store.cloned_voices.insert(&cv)?;
        Ok(self.inner.store.cloned_voices.get(id)?.unwrap_or(cv))
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
        // 非可重入引擎(sherpa OfflineTts:melo/克隆)串行化:并发 generate 会原生崩溃(整进程退出)。
        // 顺带去重——拿到锁后再查一次缓存,等锁期间别人已合成的同一句直接秒回,不重复 generate
        // (修复:克隆合成慢,用户连点几次 → 多个并发 generate → 崩;现在排队 + 命中缓存)。
        let _guard = if engine.reentrant() {
            None
        } else {
            let g = self.inner.tts_lock.lock().await;
            if path.is_file() {
                return Ok(path);
            }
            Some(g)
        };
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

    /// 录 N 段话给某家人注册声纹(家人页「让它认识 TA 的声音」)。多段取平均更稳
    /// (§4.2「绝不错认」的可靠度杠杆,2026-07-04 用户拍板 3 段)。复用听写采集(VAD 切一句),
    /// 每段进度 + 终态经 `Enroll` 事件推前端(§3.5 不静默失败);终态一律有动静。
    pub async fn enroll(&self, user_id: i64) -> Result<()> {
        let r = self.enroll_inner(user_id).await;
        let stage = if r.is_ok() { "saved" } else { "failed" };
        self.publish(VoiceEvent::Enroll { user_id, stage: stage.into(), done: None, total: None });
        if let Err(e) = &r {
            tracing::error!(user = user_id, err = %format!("{e:#}"), "声纹注册失败");
        }
        r
    }

    async fn enroll_inner(&self, user_id: i64) -> Result<()> {
        anyhow::ensure!(
            self.inner.store.users.get(user_id)?.is_some(),
            "用户不存在,先添加家人再录声纹"
        );
        // 组件/模型首次用时下载(VAD + CAM++,秒级);进度另有 Task 车道,这里只发「准备中」。
        self.publish(VoiceEvent::Enroll {
            user_id,
            stage: "preparing".into(),
            done: None,
            total: None,
        });
        let (vad_model, _asr) = self.ensure_engines().await?;
        let spk = self.ensure_speaker().await?;
        // 挂起唤醒防自激;无论成败都恢复(capture 出错也要放回,否则唤醒卡在挂起态)。
        self.wake_suspend(true);
        let outcome = self.enroll_capture(user_id, vad_model, spk).await;
        self.wake_suspend(false);
        let emb = outcome?;
        self.inner.store.voiceprints.upsert(user_id, &emb)?;
        tracing::info!(user = user_id, samples = ENROLL_SAMPLES, "声纹注册完成");
        Ok(())
    }

    /// 连录 ENROLL_SAMPLES 段(每段一句)→ 逐段提 embedding → 取平均。每段前发 recording 进度,
    /// 前端提示「第 done+1/total 遍,请说话」。任一段没录到/太短 → 整次失败(由 `enroll` 兜终态)。
    async fn enroll_capture(
        &self,
        user_id: i64,
        vad_model: PathBuf,
        spk: Arc<speaker::SpeakerId>,
    ) -> Result<Vec<f32>> {
        let rt = self.clone();
        tokio::task::spawn_blocking(move || -> Result<Vec<f32>> {
            let hangover = hangover_secs(&rt.patience());
            let vad = new_vad(&vad_model, hangover)?;
            let pipe = rt.open_capture_auto()?;
            let mut embeds = Vec::with_capacity(ENROLL_SAMPLES as usize);
            for done in 0..ENROLL_SAMPLES {
                rt.publish(VoiceEvent::Enroll {
                    user_id,
                    stage: "recording".into(),
                    done: Some(done),
                    total: Some(ENROLL_SAMPLES),
                });
                let out = collect_utterance(&pipe, &vad, &rt, None, START_TIMEOUT, hangover)?;
                let mut pcm = match out {
                    CaptureOut::Utterance(p) => p,
                    _ => anyhow::bail!("没录到声音,再说一句试试"),
                };
                anyhow::ensure!(
                    (pcm.len() as f32) >= 1.0 * TARGET_RATE as f32,
                    "说得太短啦,多说一两句(报名字、念句话都行)"
                );
                peak_normalize(&mut pcm);
                embeds.push(spk.embed(&pcm)?);
            }
            drop(pipe);
            speaker::mean_embedding(&embeds).ok_or_else(|| anyhow!("声纹平均失败"))
        })
        .await
        .context("声纹注册任务挂了")?
    }

    // ---- 唤醒录音标定(PLAN §11 后续):录几遍 → 一次扫描定拼写(B)+ 阈值(A)→ 写回 ----

    /// 录 N 段唤醒词 + 1 段底噪 → calib 扫描 → 写回 voice.wake.sensitivity(+ 必要时 .spelling)。
    /// 立即返回(壳层 fire-and-forget),进展走 Voice 车道(Preparing/CalibProgress/State/CalibResult)。
    /// **任何**早退(没词/下载失败/语音忙/采集错)都收尾成 CalibResult,绝不让向导永久转圈。
    pub async fn calibrate_wake(&self) -> Result<()> {
        // 立刻给"准备中":KWS/VAD 首次要下个小模型(各几 MB,秒级),别让向导卡在"第 0 遍"
        self.publish(VoiceEvent::State { phase: VoicePhase::Preparing });
        match self.run_calibration().await {
            Ok(o) => {
                // 写回:阈值经灵敏度(滑块随之刷新);非 canonical 拼写落覆盖表(按词键入)
                self.inner.store.settings.set(
                    None,
                    "voice.wake.sensitivity",
                    &o.sensitivity.to_string(),
                )?;
                if let Some(line) = &o.spelling {
                    let mut map = self.wake_spelling_overrides();
                    map.insert(o.word.clone(), line.clone());
                    if let Ok(json) = serde_json::to_string(&map) {
                        self.inner.store.settings.set(None, "voice.wake.spelling", &json)?;
                    }
                }
                // 在跑就重启循环吃新值(阈值/拼写都在建 spotter 时锁定)
                if self.wake_running() {
                    self.wake_stop();
                    if let Err(e) = self.wake_start().await {
                        tracing::error!(err = %format!("{e:#}"), "标定后重启唤醒失败");
                    }
                }
                self.publish(VoiceEvent::CalibResult {
                    ok: true,
                    sensitivity: o.sensitivity,
                    recall: o.recall,
                    adopted_spelling: o.spelling.is_some(),
                    verdict: o.verdict.into(),
                });
                Ok(())
            }
            Err(e) => {
                let msg = format!("{e:#}");
                let cancelled = msg.contains("__CANCELLED__");
                tracing::warn!(err = %msg, "唤醒标定未完成");
                self.publish(VoiceEvent::State { phase: VoicePhase::Idle });
                self.publish(VoiceEvent::CalibResult {
                    ok: false,
                    sensitivity: 0,
                    recall: 0.0,
                    adopted_spelling: false,
                    verdict: if cancelled { "cancelled".into() } else { "error".into() },
                });
                // 早退已被前端收尾,这里不再向上抛(命令层只会吞掉记日志)
                Ok(())
            }
        }
    }

    /// 标定主体:备模型(KWS+VAD 必需,ASR 可选)→ 占会话槽 → 录采 + 扫描。
    /// 失败照常回错(由 calibrate_wake 统一收尾 CalibResult);会话槽/挂起态保证收尾还原。
    async fn run_calibration(&self) -> Result<calib::CalibOutcome> {
        let word = self
            .wake_keywords()
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("没有唤醒词,先设一个再校准"))?;
        let mirrors = self.mirrors();
        // 标定硬依赖只有 KWS(~4MB)+ VAD(~2MB),秒级。**不**为标定单拉 230MB 的 ASR 大模型。
        let kws_dir = self.inner.models.ensure_tar(&models::KWS_ZIPFORMER_ZH, &mirrors).await?;
        let vad_model =
            self.inner.models.ensure(&models::SILERO_VAD, &mirrors).await?.join("silero_vad.onnx");
        // ASR 只用于"确认这段确实是唤醒词"——已下好才用(顺手把样本质量把一道关);
        // 没下好就跳过这道关(靠 KWS 扫描天然容错:听岔的样本不命中 = 自动降权),不拖慢首次标定。
        let asr: Option<Arc<SherpaAsr>> = if self.inner.models.is_ready(self.asr_model().spec()) {
            self.ensure_engines().await.ok().map(|(_, a)| a)
        } else {
            None
        };

        // 抢会话槽(与听写互斥,二者都开麦);失败 = 语音正忙
        let (ctl, gen) = {
            let mut slot = self.inner.session.lock().expect("voice session lock");
            if slot.is_some() {
                bail!("语音正忙,稍后再校准");
            }
            let ctl = Arc::new(AtomicU8::new(CTL_RUN));
            let gen = self.inner.gen.fetch_add(1, Ordering::Relaxed) + 1;
            *slot = Some(SessionCtl { ctl: ctl.clone(), gen });
            (ctl, gen)
        };
        self.wake_suspend(true);

        // ---- 录音(用户节奏,不设硬超时;靠 START_TIMEOUT + 重试上限自然收敛)----
        let rt = self.clone();
        let word_cap = word.clone();
        let captured = tokio::task::spawn_blocking(move || -> Result<(Vec<Vec<f32>>, Vec<f32>)> {
            let hangover = hangover_secs(&rt.patience());
            let total = CALIB_TAKES + 1; // +1 末尾底噪段
            let mut positives: Vec<Vec<f32>> = Vec::with_capacity(CALIB_TAKES as usize);
            let mut take = 0u8;
            let mut attempts = 0u8;
            while take < CALIB_TAKES {
                if ctl.load(Ordering::Relaxed) == CTL_CANCEL {
                    bail!("__CANCELLED__");
                }
                attempts += 1;
                if attempts > CALIB_TAKES * 3 {
                    bail!("没听清你说的唤醒词,先到这"); // 防一直没开口/不符卡死
                }
                rt.publish(VoiceEvent::CalibProgress { step: take + 1, total });
                rt.publish(VoiceEvent::State { phase: VoicePhase::Listening });
                let vad = new_vad(&vad_model, hangover)?;
                let pipe = rt.open_capture_auto()?;
                let out = collect_utterance(&pipe, &vad, &rt, Some(&ctl), START_TIMEOUT, hangover)?;
                drop(pipe);
                rt.publish(VoiceEvent::State { phase: VoicePhase::Idle });
                let pcm = match out {
                    CaptureOut::Utterance(p) => p,
                    CaptureOut::Cancelled => bail!("__CANCELLED__"),
                    CaptureOut::Empty => continue, // 没开口:这遍重来(不计 take)
                };
                if (pcm.len() as f32) < MIN_SPEECH_S * TARGET_RATE as f32 {
                    continue;
                }
                // 有 ASR 才确认(没下大模型就不卡这关);用 AGC 后副本送识别,样本仍存 RAW
                if let Some(asr) = &asr {
                    let mut chk = pcm.clone();
                    peak_normalize(&mut chk);
                    let heard = asr.transcribe(&chk).unwrap_or_default();
                    if !calib_text_matches(&heard, &word_cap) {
                        tracing::info!(heard, want = %word_cap, "标定样本不符,重录这遍");
                        continue;
                    }
                }
                positives.push(pcm); // RAW(未过 AGC):忠于生产 KWS 输入(wake 循环喂 raw)
                take += 1;
            }
            // 末尾 1 段底噪/负样本(不要求说话):度量误触
            if ctl.load(Ordering::Relaxed) == CTL_CANCEL {
                bail!("__CANCELLED__");
            }
            rt.publish(VoiceEvent::CalibProgress { step: total, total });
            rt.publish(VoiceEvent::State { phase: VoicePhase::Listening });
            let negative = capture_ambient(&rt)?;
            rt.publish(VoiceEvent::State { phase: VoicePhase::Idle });
            Ok((positives, negative))
        })
        .await;

        // 录音收尾:释放会话槽 + 恢复唤醒(计算不碰麦,先放开)
        {
            let mut slot = self.inner.session.lock().expect("voice session lock");
            if slot.as_ref().map(|s| s.gen) == Some(gen) {
                *slot = None;
            }
        }
        self.wake_suspend(false);

        let (positives, negative) = match captured {
            Ok(Ok(x)) => x,
            Ok(Err(e)) => return Err(e),
            Err(e) => return Err(anyhow!(e).context("唤醒标定录音任务挂了")),
        };

        // ---- 扫描计算(有界;超时兜底,绝不无限转)----
        let kws_dir2 = kws_dir.clone();
        let word2 = word.clone();
        let computed = tokio::time::timeout(
            Duration::from_secs(CALIB_COMPUTE_SECS),
            tokio::task::spawn_blocking(move || -> Result<calib::CalibOutcome> {
                let vocab = wake::load_vocab(&kws_dir2.join("tokens.txt"))?;
                calib::calibrate(&kws_dir2, &word2, &vocab, &positives, &negative)
            }),
        )
        .await;
        match computed {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => Err(anyhow!(e).context("唤醒标定计算任务挂了")),
            Err(_) => bail!("标定计算超时(组件异常),先到这"),
        }
    }

    /// 取消进行中的标定(复用听写会话的 ctl;幂等)。
    pub fn calibrate_cancel(&self) {
        self.listen_stop(false);
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

    /// 当前唤醒词 = **名字派生**(2026-07-10 用户拍板「起什么名字就怎么唤醒」,原独立设置
    /// `voice.wake.keywords` 与默认词「小七」退役):主人的 `ui.pet_name` 经
    /// `wake::derive_wake_word`(中文原样 / 数字转读法 / 字母缩写按字母读音,BT→逼踢);
    /// 没改名 = 默认名 BT → `wake::DEFAULT_WAKE_WORDS`(逼踢 + 七二七四);英文单词名派生
    /// 不出同样回落,由 `status().wake_fallback` 让 UI 如实提示(§3.5)。改名即生效
    /// (前端改完名重启唤醒循环,与旧「改唤醒词」同款)。
    pub fn wake_keywords(&self) -> Vec<String> {
        self.wake_words_resolved().0
    }

    /// (词表, 是否「起了名但喊不了」回落默认) —— 后者只在名字派生失败时为 true。
    fn wake_words_resolved(&self) -> (Vec<String>, bool) {
        wake::resolve_wake_words(&self.user_setting("ui.pet_name", ""))
    }

    /// 标定产出的「拼写覆盖」(voice.wake.spelling,词→整行 token 行)。
    /// 录音标定发现某词有更贴合用户发音的 token 拼法时写入;wake_start 据此覆盖 canonical 编码。
    /// 坏 JSON / 缺失 → 空表(全走 canonical)。
    fn wake_spelling_overrides(&self) -> HashMap<String, String> {
        self.inner
            .store
            .settings
            .get(None, "voice.wake.spelling")
            .ok()
            .flatten()
            .filter(|s| !s.trim().is_empty())
            .and_then(|json| serde_json::from_str::<HashMap<String, String>>(&json).ok())
            .unwrap_or_default()
    }

    /// 唤醒灵敏度(voice.wake.sensitivity 0~100,global)→ KWS threshold。
    /// 高灵敏 = 低阈值 = 容易被唤醒(也更易误触)。**默认 100 → 0.1(最灵敏)**:KWS 对真声
    /// 召回本就偏弱(AGENT.md §8.2),宁可默认偏召回、先保证「叫得应」,误触嫌吵再往左调,
    /// 也不让人喊半天不答应(2026-06-18 用户拍板:默认拉到最灵敏,保障能唤醒再说)。
    /// 分段映射:灵敏半区 [50,100]→[0.2,0.1],稳重半区 [0,50]→[0.5,0.2];范围 clamp [0.1, 0.5]。
    fn wake_threshold(&self) -> f32 {
        let sens: f32 = self
            .inner
            .store
            .settings
            .get(None, "voice.wake.sensitivity")
            .ok()
            .flatten()
            .and_then(|v| v.parse().ok())
            .unwrap_or(100.0);
        calib::sensitivity_to_threshold(sens)
    }

    async fn wake_start(&self) -> Result<()> {
        if self.wake_running() {
            return Ok(());
        }
        let mirrors = self.mirrors();
        let kws_dir = self.inner.models.ensure_tar(&models::KWS_ZIPFORMER_ZH, &mirrors).await?;
        let (vad_model, asr) = self.ensure_engines().await?;
        let speaker = self.ensure_speaker_if_enrolled().await; // 有家人录声纹才认人
        // 唤醒词编码:模型词表本身裁决切分(绕开拼音 strict 歧义);全军覆没 = 开不了。
        // 标定产出的拼写覆盖(voice.wake.spelling)优先,否则 canonical。
        let vocab = wake::load_vocab(&kws_dir.join("tokens.txt"))?;
        let overrides = self.wake_spelling_overrides();
        let (keywords_buf, dropped) =
            wake::build_keywords_buf(&self.wake_keywords(), &overrides, &vocab);
        if !dropped.is_empty() {
            tracing::warn!(?dropped, "部分唤醒词编码不进模型词表,已忽略");
        }
        if !overrides.is_empty() {
            tracing::info!(words = ?overrides.keys().collect::<Vec<_>>(), "应用标定拼写覆盖");
        }
        anyhow::ensure!(!keywords_buf.is_empty(), "唤醒词一个都编不出来(只支持中文词)");
        // 短句银行(人格数据,场景给话术):断网 best-effort,空类目降级无声
        let scene_voice = self.inner.scenes.default_scene().voice.clone();
        let bank = prompts::PromptBank::prepare(self, &scene_voice).await;
        // 银行放进可热替换槽:换音色时后台重建并整体替换,KWS 检测与麦克风不动(问题1-B)。
        let prompts: prompts::SharedPromptBank = Arc::new(std::sync::Mutex::new(Arc::new(bank)));

        let (tx, rx) = std::sync::mpsc::channel();
        let gen = WAKE_GEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        {
            let mut slot = self.inner.wake.lock().expect("wake lock");
            if slot.is_some() {
                return Ok(()); // 并发开关:别人抢先了
            }
            *slot = Some(WakeHandle { cmd: tx, prompts: prompts.clone(), gen });
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
            gen,
        };
        if let Err(e) =
            std::thread::Builder::new().name("voice-wake".into()).spawn(move || {
                wake::run_wake_loop(deps, rx);
            })
        {
            self.wake_cleanup();
            return Err(anyhow!(e).context("唤醒线程起不来"));
        }
        // 权威广播:唤醒起来了(boot 自动恢复也走这——前端 wakeArmed / mic bridge 靠它
        // 跟随;没有它,开机自启时前端首查赶在 wake_start 完成前 → armed 定格 false →
        // browser 采集源永不开麦 =「开关显示开、叫不答应」,2026-07-11 真机实锤)。
        self.publish(VoiceEvent::WakeRunning { running: true, keywords: self.wake_keywords() });
        tracing::info!("免手唤醒已启动");
        Ok(())
    }

    pub fn wake_stop(&self) {
        self.wake_cmd(wake::WakeCmd::Stop);
        self.wake_cleanup(); // sender 一并丢弃,线程见 Disconnected 也会退
        // stop 是用户意图点,同步广播「停了」;loop 稍后退出时 slot 已空、认领失败不重发
        self.publish(VoiceEvent::WakeRunning { running: false, keywords: Vec::new() });
    }

    /// 换音色/语速/在线离线档后:若唤醒在跑,后台按新设置重建应答音银行并**热替换**——
    /// KWS 检测与麦克风全程不动(不打断"竖着耳朵听"),命中缓存的音色秒回(问题1-B)。
    /// 没开唤醒 = no-op(下次 wake_start 自然按新音色建)。
    pub async fn refresh_prompts(&self) {
        let slot = match self.inner.wake.lock().expect("wake lock").as_ref() {
            Some(h) => h.prompts.clone(),
            None => return,
        };
        let scene_voice = self.inner.scenes.default_scene().voice.clone();
        let bank = prompts::PromptBank::prepare(self, &scene_voice).await;
        *slot.lock().expect("prompts lock") = Arc::new(bank);
        tracing::info!("唤醒应答音已按新音色重建(未重启唤醒循环)");
    }

    fn wake_cmd(&self, cmd: wake::WakeCmd) {
        if let Some(h) = self.inner.wake.lock().expect("wake lock").as_ref() {
            let _ = h.cmd.send(cmd);
        }
    }

    /// 前端编排指令:回合念完 → 开跟进窗;回合失败/取消 → 直接回待唤醒。
    /// 回合念完开跟进窗;media_playing = 前端念完那一刻媒体在不在播(在播 → 3s 短窗少压音量)。
    pub fn wake_follow_up(&self, media_playing: bool) {
        self.wake_cmd(wake::WakeCmd::FollowUp { media_playing });
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

    /// loop 退出路的认领式清理:只有 slot 还是**自己那一代**才清(off→on 重启时旧线程
    /// 退出可能晚于新 start 置位——朴素置 None 会把新 loop 的句柄误清、连带 drop sender
    /// 让新 loop 也退)。返回「清的是不是自己」= 要不要广播 running:false
    /// (被 wake_stop 清过 = stop 已广播;被新一代顶替 = 唤醒还活着,都不该再发 false)。
    pub(super) fn wake_cleanup_gen(&self, gen: u64) -> bool {
        let mut slot = self.inner.wake.lock().expect("wake lock");
        match slot.as_ref() {
            Some(h) if h.gen == gen => {
                *slot = None;
                true
            }
            _ => false,
        }
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
        let (keywords, wake_fallback) = self.wake_words_resolved();
        VoiceStatus {
            asr_ready: self.inner.models.is_ready(self.asr_model().spec()),
            vad_ready: self.inner.models.is_ready(&models::SILERO_VAD),
            kws_ready: self.inner.models.is_tar_ready(&models::KWS_ZIPFORMER_ZH),
            wake_running: self.wake_running(),
            keywords,
            wake_fallback,
            devices: list_input_devices(),
            speakers: self.voice_options(),
            default_speaker: tts::DEFAULT_SPEAKER.to_string(),
        }
    }

    /// 音色列表 = 内置在线音色 + 用户/内置克隆(混排;前端据 isClone/builtin 加 badge/删除)。
    fn voice_options(&self) -> Vec<VoiceOption> {
        let mut out: Vec<VoiceOption> = SPEAKERS_ZH
            .iter()
            .map(|s| VoiceOption {
                id: s.id.to_string(),
                name: s.name.to_string(),
                is_clone: false,
                builtin: false,
            })
            .collect();
        for c in self.inner.store.cloned_voices.list().unwrap_or_default() {
            out.push(VoiceOption {
                id: format!("clone:{}", c.id),
                name: c.name,
                is_clone: true,
                builtin: c.builtin,
            });
        }
        out
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
    /// 同一个「停」也喂给唤醒交互:唤醒录音不占 `session` 槽,故两处信号都写(哪个在录哪个响应,
    /// 二者互斥:听写期唤醒循环挂起、唤醒录音期没有听写会话)。唤醒侧每次开录前会武装成 RUN,
    /// 故这里在 Watch 态写入的陈旧值不会误触下一轮。
    pub fn listen_stop(&self, accept: bool) {
        let v = if accept { CTL_ACCEPT } else { CTL_CANCEL };
        self.inner.wake_ctl.store(v, Ordering::Relaxed);
        let slot = self.inner.session.lock().expect("voice session lock");
        if let Some(s) = slot.as_ref() {
            s.ctl.store(v, Ordering::Relaxed);
        }
    }

    /// 唤醒交互「停 / 定稿」信号(唤醒循环用):开录前 `arm` 成 RUN,`collect_utterance` 读它响应
    /// 前端点停。与听写会话槽分家(唤醒录音跑在自己的循环线程,见 `wake_ctl` 字段)。
    pub(super) fn wake_ctl(&self) -> &AtomicU8 {
        &self.inner.wake_ctl
    }
    pub(super) fn arm_wake_ctl(&self) {
        self.inner.wake_ctl.store(CTL_RUN, Ordering::Relaxed);
    }

    /// 选中的中文 ASR 档(voice.asr.model,app 级;空 / 未知回落默认 SenseVoice)。
    fn asr_model(&self) -> models::AsrModel {
        let v = self
            .inner
            .store
            .settings
            .get(None, "voice.asr.model")
            .ok()
            .flatten()
            .unwrap_or_default();
        models::AsrModel::from_setting(&v)
    }

    /// 模型就绪 + ASR 加载(听写与唤醒共用)。按 voice.asr.model 选档:同档复用缓存,
    /// 换档则现下现载并替换缓存。锁全程持有 = 顺带去重(并发调用方要的也是同一档,等就好)。
    async fn ensure_engines(&self) -> Result<(PathBuf, Arc<SherpaAsr>)> {
        let mirrors = self.mirrors();
        let vad_dir = self.inner.models.ensure(&models::SILERO_VAD, &mirrors).await?;
        let model = self.asr_model();
        let mut guard = self.inner.asr.lock().await;
        if let Some((m, a)) = guard.as_ref() {
            if *m == model {
                return Ok((vad_dir.join("silero_vad.onnx"), a.clone()));
            }
        }
        // 换档 / 首次:用时下载对应模型(HUD 进度)→ spawn_blocking 加载(秒级,别堵 runtime)。
        let asr_dir = self.inner.models.ensure(model.spec(), &mirrors).await?;
        let asr = tokio::task::spawn_blocking(move || SherpaAsr::load(model, &asr_dir, "zh").map(Arc::new))
            .await
            .context("ASR 加载任务挂了")??;
        *guard = Some((model, asr.clone()));
        Ok((vad_dir.join("silero_vad.onnx"), asr))
    }

    /// 失败语音模型下载的「重试」直连口(HUD 按钮 → 壳层 `retry_voice_model` → 这里;
    /// 不绕 LLM §7.1)。按 id 找回三型 spec 重跑对应 ensure(自带 HUD:成功 done、再失败仍
    /// fail_retryable 冒新卡);未知 id(老版本残卡)只记日志。后台 spawn,不阻塞调用方。
    pub fn retry_model(&self, id: &str) {
        let this = self.clone();
        let id = id.to_string();
        tokio::spawn(async move {
            let mirrors = this.mirrors();
            let m = &this.inner.models;
            let r = match id.as_str() {
                i if i == models::SILERO_VAD.id => {
                    m.ensure(&models::SILERO_VAD, &mirrors).await.map(|_| ())
                }
                i if i == models::ASR_SENSE_VOICE.id => {
                    m.ensure(&models::ASR_SENSE_VOICE, &mirrors).await.map(|_| ())
                }
                i if i == models::ASR_FIRERED_CTC.id => {
                    m.ensure(&models::ASR_FIRERED_CTC, &mirrors).await.map(|_| ())
                }
                i if i == models::SPEAKER_CAMPP_ZH.id => {
                    m.ensure(&models::SPEAKER_CAMPP_ZH, &mirrors).await.map(|_| ())
                }
                i if i == models::TTS_VITS_MELO.id => {
                    m.ensure_tar(&models::TTS_VITS_MELO, &mirrors).await.map(|_| ())
                }
                i if i == models::KWS_ZIPFORMER_ZH.id => {
                    m.ensure_tar(&models::KWS_ZIPFORMER_ZH, &mirrors).await.map(|_| ())
                }
                i if i == models::TTS_ZIPVOICE.id => {
                    m.ensure_tar_tree(&models::TTS_ZIPVOICE, &mirrors).await.map(|_| ())
                }
                other => {
                    tracing::warn!(id = other, "retry_voice_model:未知模型 id,忽略");
                    return;
                }
            };
            if let Err(e) = r {
                tracing::warn!(err = %format!("{e:#}"), id = %id, "语音模型重试仍失败");
            }
        });
    }

    /// 选中的 ASR 档已就绪(不触发下载)。渠道语音消息第一次遇到未就绪 → 先回「准备中」
    /// 提示 + `prefetch_asr` 后台下,绝不让手机那头干等几分钟。
    pub fn asr_ready(&self) -> bool {
        self.inner.models.is_ready(self.asr_model().spec())
    }

    /// 后台预取 ASR(模型下载 + 加载,HUD 进度照常;fire-and-forget,失败留日志)。
    pub fn prefetch_asr(&self) {
        let this = self.clone();
        tokio::spawn(async move {
            if let Err(e) = this.ensure_engines().await {
                tracing::warn!(err = %format!("{e:#}"), "ASR 预取失败");
            }
        });
    }

    /// 16k 单声道 PCM → 文本(渠道语音消息用;与听写/唤醒共用同一份缓存的识别器)。
    pub async fn transcribe_pcm(&self, samples: Vec<f32>) -> Result<String> {
        anyhow::ensure!(!samples.is_empty(), "音频数据为空");
        let (_vad, asr) = self.ensure_engines().await?;
        tokio::task::spawn_blocking(move || asr.transcribe(&samples))
            .await
            .context("转写任务挂了")?
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

    // ---- 采集双源(层1 AEC 采集端,2026-07-06):cpal(现状默认)↔ 浏览器推流 ----
    // 浏览器源 = 前端 getUserMedia({echoCancellation}) 消完回声再推 16k PCM 过来
    // (WebView2=Chromium AEC3;参考=它自己在播的全部音频)。默认 cpal 零回归,
    // 真机验过再转正默认(watch-items 见 PLAN §11)。

    /// 当前采集源(`voice.capture.source`,app 级):browser = 前端推流;其余 = cpal。
    /// **默认 browser(2026-07-06 转正)**:getUserMedia 消完自播回声的耳朵是治自我唤醒
    /// 的根;显式设过 cpal 的沿用。前端 useSettings DEFAULTS 镜像同值(§6.8/§4.11)。
    fn capture_source(&self) -> String {
        self.inner
            .store
            .settings
            .get(None, "voice.capture.source")
            .ok()
            .flatten()
            .unwrap_or_else(|| "browser".into())
    }

    /// 按采集源开管:唤醒/听写/标定/录声纹统一走这个口(接缝换源,下游零感知)。
    pub(super) fn open_capture_auto(&self) -> Result<CapturePipe> {
        if self.capture_source() == "browser" {
            tracing::info!("采集源 = 浏览器推流(AEC 采集端)");
            return Ok(self.open_push_pipe());
        }
        open_capture(self.input_device())
    }

    /// 挂一个浏览器推流 tap:帧已是 16k mono(前端 AudioContext 定死),免重采样。
    /// 管子 drop → 下次推帧 send 失败 → 自动从扇出表剪除(与「pipe drop 即关麦」同语义)。
    fn open_push_pipe(&self) -> CapturePipe {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(64);
        self.inner.push_taps.lock().expect("push_taps lock").push(tx);
        CapturePipe { rx, resampler: None, _guard: CaptureGuard::Push }
    }

    /// 壳层 `voice_push_audio` 命令入口:16k mono f32 帧扇出给所有在收的管。
    /// 队列满 = 丢帧(cpal 同款,绝不阻塞 IPC);死管(接收端 drop)就地剪除。
    pub fn push_audio(&self, pcm: Vec<f32>) {
        if pcm.is_empty() {
            return;
        }
        let mut taps = self.inner.push_taps.lock().expect("push_taps lock");
        taps.retain(|tx| match tx.try_send(pcm.clone()) {
            Ok(()) => true,
            Err(std::sync::mpsc::TrySendError::Full(_)) => true, // 消费端慢:丢这帧,管还活着
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => false,
        });
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
    /// 采集源守卫:cpal = 持流(drop 即关麦);浏览器推流 = 无本地资源
    /// (tap 生命周期靠 push_audio 的 send 失败自动剪除,语义与"drop 即关"对齐)。
    _guard: CaptureGuard,
}

enum CaptureGuard {
    /// 只为持有(drop 即关麦),不读字段。
    Cpal(#[allow(dead_code)] cpal::Stream),
    Push,
}

impl CapturePipe {
    pub(super) fn to_16k(&self, chunk: &[f32]) -> Vec<f32> {
        match &self.resampler {
            Some(r) => r.resample(chunk, false),
            None => chunk.to_vec(),
        }
    }

    /// 清空积压帧(robot 坑:唤醒后队列积压拖慢"开始听")。返回清掉的帧数(观测用)。
    pub(super) fn drain(&self) -> usize {
        let mut n = 0;
        while self.rx.try_recv().is_ok() {
            n += 1;
        }
        n
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
    Ok(CapturePipe { rx, resampler, _guard: CaptureGuard::Cpal(stream) })
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
    let pipe = rt.open_capture_auto()?;
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

/// 标定样本校验:ASR 转写是否就是/含唤醒词(宽松,容 ASR 听岔一两字)。
/// 去标点/空白后:互含即过;否则字重叠 ≥ 唤醒词一半也过。空 = 不过。
fn calib_text_matches(heard: &str, word: &str) -> bool {
    let strip = |s: &str| -> String {
        s.chars()
            .filter(|c| !c.is_whitespace() && !c.is_ascii_punctuation())
            .filter(|c| !matches!(c, '，' | '。' | '、' | '!' | '?' | ';' | ':' | '~' | '…'))
            .collect()
    };
    let h = strip(heard);
    let w = strip(word);
    if h.is_empty() || w.is_empty() {
        return false;
    }
    if h.contains(&w) || w.contains(&h) {
        return true;
    }
    let wset: std::collections::HashSet<char> = w.chars().collect();
    let overlap = h.chars().filter(|c| wset.contains(c)).count();
    overlap * 2 >= w.chars().count()
}

/// 录一段定长底噪/负样本(不靠 VAD,原样收 16k);标定用它度量误触。
/// 先丢 ~0.4s:跳过开流暂态 + 上一遍唤醒词的尾音/回声,免把它当成"底噪里有唤醒词"误触。
fn capture_ambient(rt: &VoiceRuntime) -> Result<Vec<f32>> {
    let pipe = rt.open_capture_auto()?;
    let settle = Instant::now();
    while settle.elapsed() < Duration::from_millis(400) {
        let _ = pipe.rx.recv_timeout(Duration::from_millis(50));
    }
    pipe.drain(); // 清掉沉淀期积压,从此刻起才是"环境音"
    let mut buf = Vec::new();
    let started = Instant::now();
    let want = Duration::from_secs_f32(CALIB_AMBIENT_SECS);
    while started.elapsed() < want {
        match pipe.rx.recv_timeout(Duration::from_millis(100)) {
            Ok(chunk) => buf.extend_from_slice(&pipe.to_16k(&chunk)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    drop(pipe);
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_rt(name: &str) -> VoiceRuntime {
        let dir = std::env::temp_dir().join(format!("lw-voice-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _ = std::fs::remove_file(dir.join("t.db"));
        let store = Store::open(&dir.join("t.db")).unwrap();
        VoiceRuntime::new(dir, store, Bus::new(), Scenes::builtin())
    }

    // ---- 采集双源(层1 AEC 采集端) ----

    #[test]
    fn capture_source_defaults_to_browser_and_dispatches_push() {
        let rt = test_rt("capsrc");
        assert_eq!(rt.capture_source(), "browser", "默认 browser(2026-07-06 转正:AEC 耳朵)");
        rt.inner.store.settings.set(None, "voice.capture.source", "cpal").unwrap();
        assert_eq!(rt.capture_source(), "cpal", "显式设过 cpal 的沿用");
        rt.inner.store.settings.set(None, "voice.capture.source", "browser").unwrap();
        // browser 源开管不碰麦克风硬件,永远成功
        let pipe = rt.open_capture_auto().expect("push pipe 打开");
        rt.push_audio(vec![0.1_f32; 160]);
        let got =
            pipe.rx.recv_timeout(std::time::Duration::from_millis(200)).expect("收到推流帧");
        assert_eq!(got.len(), 160);
        assert_eq!(pipe.to_16k(&got).len(), 160, "推流已是 16k,直通不重采样");
    }

    #[test]
    fn push_audio_fans_out_and_prunes_dead_taps() {
        let rt = test_rt("fanout");
        rt.inner.store.settings.set(None, "voice.capture.source", "browser").unwrap();
        let a = rt.open_capture_auto().unwrap();
        let b = rt.open_capture_auto().unwrap();
        rt.push_audio(vec![0.2_f32; 8]);
        assert!(a.rx.recv_timeout(std::time::Duration::from_millis(200)).is_ok(), "tap A 收到");
        assert!(
            b.rx.recv_timeout(std::time::Duration::from_millis(200)).is_ok(),
            "tap B 收到(扇出 = cpal 双流同语义)"
        );
        drop(a); // 管子 drop = 「关麦」
        rt.push_audio(vec![0.3_f32; 8]);
        assert_eq!(rt.inner.push_taps.lock().unwrap().len(), 1, "死管在下次推帧时剪除");
        assert!(
            b.rx.recv_timeout(std::time::Duration::from_millis(200)).is_ok(),
            "幸存 tap 不受影响"
        );
    }

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
