//! 免手唤醒循环(PLAN §11 C):KWS 常驻线程状态机。
//! Watch(喂 KWS)→ 命中 → 应答音直出(0 间隙开录)→ 听一轮(两段式有声兜底:
//! 没听清→追问重听;再空→有声告退,绝不静默)→ Transcribed{via:wake} → AwaitTurn
//! (回合进行中丢帧防自激)→ 前端念完发 FollowUp → 跟进窗(6s 免唤醒接话)→ 没声回 Watch。
//! 编排者仍是前端 VM(它才知道回合/念话何时结束);本线程只管声学侧。

use std::collections::HashSet;
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};

use super::asr::{Asr, SherpaAsr};
use super::prompts::{PromptBank, PromptKind, SharedPromptBank};
use super::speaker::SpeakerId;
use super::{collect_utterance, hangover_secs, new_vad, peak_normalize, CaptureOut};
use crate::bus::{VoiceEvent, VoicePhase};

pub(super) enum WakeCmd {
    Stop,
    /// 丢帧(听写中 / TTS 在念):防 KWS 自激与误吃。
    Suspend(bool),
    /// 回合念完:开跟进窗(免唤醒接话)。媒体在播 → 短窗(窗内媒体一直被 duck 压着,能短就短)。
    FollowUp { media_playing: bool },
    /// 回合失败/取消/不念:直接回待唤醒。
    Resume,
}

// ---- KWS 参数 ----
// threshold 不再锁死:由「唤醒灵敏度」滑块经 voice.wake.sensitivity 映射(见 mod.rs
// wake_threshold,默认对齐 robot 实战 0.2),建 spotter 时由 deps 传入。
// score 暂不暴露:真机实测它对召回无正增益(s2.5/s3.5 在 t0.45 仍不应,只有降阈管用),
// 唤醒难易完全由 threshold 拦。
pub(super) const KWS_SCORE: f32 = 1.5;
const WAKE_START_TIMEOUT: Duration = Duration::from_secs(6); // 应答后没人开口
const FOLLOW_UP_WINDOW: Duration = Duration::from_secs(6); // 跟进窗(robot 终值)
/// 媒体在播时的跟进窗(2026-07-10 用户拍板「暂定 3s」):窗内电影/音乐一直被压到 20%,短些少打扰。
const FOLLOW_UP_WINDOW_MEDIA: Duration = Duration::from_secs(3);
const AWAIT_TURN_CAP: Duration = Duration::from_secs(180); // 前端没回信的兜底
const WATCHDOG_SILENCE: Duration = Duration::from_secs(30); // 监听态多久无帧 → 重开采集(robot 同款)

// ---- 唤醒确认层参数(2026-07-06,精度方向;§8.2 当年作废的是「召回方向」两段式,不是一回事)----
// 背景:唤醒词是用户数据,可能被起成高频常用词(实锤「天天」——「天天向上」「我们天天…」
// 全线误触)。KWS 保持拉满召回当**候选探测器**,命中后不立刻出声:静默续录到断句 → ASR
// 整句 → 拼音三段式分路。确认层 fail-open:ASR 挂了宁可当经典唤醒,绝不因它叫不应。
/// 命中后无人声多久判「孤立呼名」(短=应答快;太短会把慢半拍的续句错判孤立)。
const CONFIRM_CONT_WINDOW: Duration = Duration::from_millis(600);
/// 续句收口:有人声后静音多久算说完。原 450ms「只要断句不用等长停顿」在真机把
/// 自然说话的思考停顿当成了句尾——长指令被切半截(2026-07-11 用户实锤)→ 对齐听写链
/// hangover 标准档 0.8s(同一个人自然说话,凭什么唤醒第一句要求更快收口);代价 =
/// 带续句时应答/直达晚 ~0.35s,换「不截断」。
const CONFIRM_CLOSE: Duration = Duration::from_millis(800);
/// 续句录制上限:再长也切在这,把已有的整句拿去仲裁(别没完没了录)。原 6s 顶不住
/// 长指令(同上实锤)→ 对齐听写链 MAX_SPEECH_S=12s。
const CONFIRM_TAIL_MAX: Duration = Duration::from_secs(12);
/// 预滚环秒数:命中点往回带的音频(盖住唤醒词本身 + 一点前文,给 ASR/仲裁上下文)。
const CONFIRM_RING_S: f32 = 2.0;
/// 三段式尾随阈值:唤醒词后 ≤N 个音节(语气词「呀/啊」)= 孤立呼名;更多 = 续句交仲裁。
const WAKE_TRAIL_MAX: usize = 1;

pub(super) struct WakeDeps {
    pub rt: super::VoiceRuntime,
    pub kws_dir: PathBuf,
    pub vad_model: PathBuf,
    pub asr: Arc<SherpaAsr>,
    /// 应答音银行(可热替换:运行时换音色后台重建,唤醒线程每轮取最新快照)。
    pub prompts: SharedPromptBank,
    pub keywords_buf: String,
    /// KWS 触发阈值(唤醒灵敏度滑块映射而来;建 spotter 时锁定,改灵敏度需重启循环)。
    pub kws_threshold: f32,
    /// 声纹(有家人注册才有;None = 不识别说话人,走会话用户)。
    pub speaker: Option<Arc<SpeakerId>>,
    /// 启动代次:loop 退出时凭它认领清理(见 `wake_cleanup_gen`)。
    pub gen: u64,
}

enum Phase {
    Watch,
    AwaitTurn(Instant),
    /// window = 本轮跟进窗长(FollowUp 指令按「媒体在不在播」定,见 FOLLOW_UP_WINDOW*)。
    FollowUp { window: Duration },
}

impl Phase {
    fn label(&self) -> &'static str {
        match self {
            Phase::Watch => "Watch",
            Phase::AwaitTurn(_) => "AwaitTurn",
            Phase::FollowUp { .. } => "FollowUp",
        }
    }
}

/// 临时诊断(生产默认零开销):设了环境变量 `LARKWING_KWS_DUMP_DIR=<目录>` 时,把
/// Watch 阶段「原样喂给 KWS 的 16k mono 音频」连续写成 wav,并每 2s 报一次电平。
/// 用法:把产物 wav 离线喂 `examples/kws_replay`——命中 = 采集没问题(真因在循环时序);
/// 不命中 = 采集质量(声道下混/重采样)坐实。peak 接近 0 = 信号过弱(疑下混砍电平/死声道)。
struct WatchDump {
    file: std::fs::File,
    path: PathBuf,
    data_bytes: u32,
    win_peak: f32,
    win_sumsq: f64,
    win_n: usize,
    last_log: Instant,
}

impl WatchDump {
    fn from_env() -> Option<Self> {
        let dir = std::env::var("LARKWING_KWS_DUMP_DIR").ok().filter(|s| !s.is_empty())?;
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let path = PathBuf::from(dir).join(format!("kws_watch_{millis}.wav"));
        let mut file = match std::fs::File::create(&path) {
            Ok(f) => f,
            Err(e) => {
                tracing::error!("KWS 诊断 wav 建不了({}): {e}", path.display());
                return None;
            }
        };
        if let Err(e) = write_wav_header(&mut file, 0) {
            tracing::error!("KWS 诊断 wav 头写失败: {e}");
            return None;
        }
        tracing::warn!("⚑ KWS 诊断落盘开启 → {}(Watch 音频原样写入)", path.display());
        Some(Self {
            file,
            path,
            data_bytes: 0,
            win_peak: 0.0,
            win_sumsq: 0.0,
            win_n: 0,
            last_log: Instant::now(),
        })
    }

    /// 喂一段 16k mono 帧:写盘 + 累计电平;每 2s 报电平并回填 wav 长度(硬退出也可读)。
    fn push(&mut self, frames: &[f32]) {
        let mut buf = Vec::with_capacity(frames.len() * 2);
        for &s in frames {
            buf.extend_from_slice(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
            self.win_peak = self.win_peak.max(s.abs());
            self.win_sumsq += (s as f64) * (s as f64);
            self.win_n += 1;
        }
        match self.file.write_all(&buf) {
            Ok(()) => self.data_bytes = self.data_bytes.saturating_add(buf.len() as u32),
            Err(e) => tracing::error!("KWS 诊断 wav 写失败: {e}"),
        }
        if self.last_log.elapsed() >= Duration::from_secs(2) && self.win_n > 0 {
            let rms = (self.win_sumsq / self.win_n as f64).sqrt();
            let dbfs = 20.0 * self.win_peak.max(1e-6).log10();
            tracing::info!(
                "KWS 监听电平(2s 窗): peak={:.3} peak_dBFS={:.1} rms={:.4}",
                self.win_peak,
                dbfs,
                rms
            );
            self.win_peak = 0.0;
            self.win_sumsq = 0.0;
            self.win_n = 0;
            self.last_log = Instant::now();
            let _ = write_wav_sizes(&mut self.file, self.data_bytes);
            let _ = self.file.seek(SeekFrom::End(0)); // 回末尾续写,别覆盖音频
        }
    }
}

impl Drop for WatchDump {
    fn drop(&mut self) {
        let _ = self.file.flush();
        let _ = write_wav_sizes(&mut self.file, self.data_bytes);
        tracing::warn!("⚑ KWS 诊断 wav 写完 {} bytes → {}", self.data_bytes, self.path.display());
    }
}

/// 16k/mono/16-bit WAV 头(data_len 先占位,收尾回填)。
fn write_wav_header(f: &mut std::fs::File, data_len: u32) -> std::io::Result<()> {
    let sr = super::TARGET_RATE;
    f.write_all(b"RIFF")?;
    f.write_all(&(36 + data_len).to_le_bytes())?;
    f.write_all(b"WAVE")?;
    f.write_all(b"fmt ")?;
    f.write_all(&16u32.to_le_bytes())?;
    f.write_all(&1u16.to_le_bytes())?; // PCM
    f.write_all(&1u16.to_le_bytes())?; // mono
    f.write_all(&sr.to_le_bytes())?;
    f.write_all(&(sr * 2).to_le_bytes())?; // byte rate = sr * 1ch * 2byte
    f.write_all(&2u16.to_le_bytes())?; // block align
    f.write_all(&16u16.to_le_bytes())?; // bits
    f.write_all(b"data")?;
    f.write_all(&data_len.to_le_bytes())?;
    Ok(())
}

fn write_wav_sizes(f: &mut std::fs::File, data_len: u32) -> std::io::Result<()> {
    f.seek(SeekFrom::Start(4))?;
    f.write_all(&(36 + data_len).to_le_bytes())?;
    f.seek(SeekFrom::Start(40))?;
    f.write_all(&data_len.to_le_bytes())?;
    Ok(())
}

pub(super) fn run_wake_loop(deps: WakeDeps, cmd: Receiver<WakeCmd>) {
    if let Err(e) = wake_loop(&deps, &cmd) {
        tracing::error!(err = %format!("{e:#}"), "唤醒循环挂了");
        deps.rt.publish(VoiceEvent::ListenEnded { reason: "error".into() });
        deps.rt.publish(VoiceEvent::State { phase: VoicePhase::Idle });
    }
    // 认领式清理:清到自己 = 意外退出(错误/采集挂了),广播「停了」让前端开关/耳朵
    // 如实归位(§3.5 不静默聋);认领失败 = 被 wake_stop 清过(stop 已广播)或已被
    // off→on 的新一代顶替(唤醒还活着)——都不该再发 false。
    if deps.rt.wake_cleanup_gen(deps.gen) {
        deps.rt.publish(VoiceEvent::WakeRunning { running: false, keywords: Vec::new() });
    }
    tracing::info!("免手唤醒已停止");
}

fn wake_loop(d: &WakeDeps, cmd: &Receiver<WakeCmd>) -> Result<()> {
    // ---- KWS(int8 三件套 + 词表;keywords_buf 免落盘;配置与 calib 标定共用) ----
    let kcfg = kws_config(&d.kws_dir, &d.keywords_buf, d.kws_threshold, KWS_SCORE);
    let spotter =
        sherpa_onnx::KeywordSpotter::create(&kcfg).ok_or_else(|| anyhow!("KWS 创建失败"))?;
    let kstream = spotter.create_stream();
    // 诊断:打出实际生效的 threshold/score/编码词——滑块坏了就靠这行确认到底用了多少。
    tracing::info!(
        "KWS 建好(实际生效值):threshold={:.3} score={:.2} 唤醒词编码={:?}",
        d.kws_threshold,
        KWS_SCORE,
        d.keywords_buf
    );

    let hangover = hangover_secs(&d.rt.patience());
    let vad = new_vad(&d.vad_model, hangover)?;
    let mut pipe = d.rt.open_capture_auto()?;
    let mut dump = WatchDump::from_env(); // 诊断:设了 LARKWING_KWS_DUMP_DIR 才落盘
    let mut suspended = false;
    let mut phase = Phase::Watch;
    let mut last_state: &'static str = "init"; // 状态切换日志(判喊话时循环是否在 Watch)
    let mut last_frame = Instant::now(); // watchdog:上次从麦克风拿到帧的时刻
    let mut force_reopen = false; // 流断开/无帧 → 下一轮重开采集
    // 确认层预滚环:留住最近 CONFIRM_RING_S 的 16k 音频(唤醒词本身 + 一点前文),
    // 命中时连同静默续录一起送 ASR。容量 32k 样本(f32 128KB),纯内存滚动。
    let ring_cap = (CONFIRM_RING_S * super::TARGET_RATE as f32) as usize;
    let mut ring: Vec<f32> = Vec::with_capacity(ring_cap);
    tracing::info!("唤醒监听中(KWS 常驻)");

    loop {
        // 命令优先(非阻塞清空)
        loop {
            match cmd.try_recv() {
                Ok(WakeCmd::Stop) => return Ok(()),
                Ok(WakeCmd::Suspend(b)) => {
                    suspended = b;
                    if !b {
                        pipe.drain(); // 恢复时清积压,别把挂起期的声音当唤醒词
                    }
                }
                Ok(WakeCmd::FollowUp { media_playing }) => {
                    if matches!(phase, Phase::AwaitTurn(_)) {
                        phase = Phase::FollowUp {
                            window: if media_playing { FOLLOW_UP_WINDOW_MEDIA } else { FOLLOW_UP_WINDOW },
                        };
                    }
                }
                Ok(WakeCmd::Resume) => {
                    pipe.drain();
                    // 收尾必须知会前端(ListenEnded):前端靠它关唤醒区间/恢复媒体音量。
                    // 此前这里静默回 Watch → 失败/取消的唤醒回合 duck 永不恢复,媒体一直压在
                    // 20%(真机「音量隔了好几分钟才变大」一族的半边;另半边=前端漏发 Resume
                    // 卡到 AWAIT_TURN_CAP 兜底)。已在 Watch 则不发(无区间可关,免噪声)。
                    if !matches!(phase, Phase::Watch) {
                        d.rt.publish(VoiceEvent::ListenEnded { reason: "wake_done".into() });
                    }
                    phase = Phase::Watch;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return Ok(()),
            }
        }
        // 非纯监听态(挂起/回合中/跟进听)不累计"无帧":那些是故意不收帧
        if suspended || !matches!(phase, Phase::Watch) {
            last_frame = Instant::now();
        }
        // 状态切换日志:喊「旺财」时若不在 Watch(挂起/回合中/跟进),帧会被丢——
        // 这能把"采集质量"和"循环时序漏接"分开。
        let cur_state: &'static str = if suspended { "Suspended" } else { phase.label() };
        if cur_state != last_state {
            tracing::info!("唤醒循环状态: {last_state} → {cur_state}");
            last_state = cur_state;
        }
        // watchdog:监听态持续无帧 / 流断开 → 重开采集(sounddevice 偶发回调卡死、
        // 设备热插拔的唯一稳定恢复路径,robot 同款)。VAD/KWS 模型不动,只换采集流。
        if matches!(phase, Phase::Watch) && !suspended && (force_reopen || last_frame.elapsed() > WATCHDOG_SILENCE) {
            tracing::warn!(silent_s = last_frame.elapsed().as_secs(), force = force_reopen, "麦克风无帧,重开采集");
            force_reopen = false;
            match d.rt.open_capture_auto() {
                Ok(p) => pipe = p,
                Err(e) => {
                    tracing::error!(err = %format!("{e:#}"), "重开采集失败,2s 后重试");
                    std::thread::sleep(Duration::from_secs(2));
                }
            }
            last_frame = Instant::now();
            continue;
        }
        if suspended {
            pipe.drain();
            std::thread::sleep(Duration::from_millis(60));
            continue;
        }

        match phase {
            Phase::Watch => match pipe.rx.recv_timeout(Duration::from_millis(100)) {
                Ok(chunk) => {
                    last_frame = Instant::now();
                    let s16k = pipe.to_16k(&chunk);
                    if let Some(dp) = dump.as_mut() {
                        dp.push(&s16k); // 诊断落盘:原样记下喂给 KWS 的音频
                    }
                    // 预滚环滚动(确认层用;溢出裁头)
                    ring.extend_from_slice(&s16k);
                    if ring.len() > ring_cap {
                        ring.drain(..ring.len() - ring_cap);
                    }
                    kstream.accept_waveform(super::TARGET_RATE as i32, &s16k);
                    while spotter.is_ready(&kstream) {
                        spotter.decode(&kstream);
                    }
                    let hit = spotter
                        .get_result(&kstream)
                        .map(|r| r.keyword)
                        .filter(|k| !k.is_empty());
                    if let Some(keyword) = hit {
                        tracing::info!(%keyword, "唤醒命中(候选,进确认层)");
                        spotter.reset(&kstream);
                        phase = on_hit(d, &pipe, &vad, hangover, &ring)?;
                        ring.clear(); // 命中处理完清环:别把这轮音频带进下次判定
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    tracing::warn!("麦克风流断开,触发重开");
                    force_reopen = true; // 不 bail:热插拔/设备睡眠会断流,自愈而非整循环退出
                }
            },
            Phase::AwaitTurn(since) => {
                // 回合进行中(模型在想/TTS 在念):丢帧防自激,等前端指令
                pipe.drain();
                std::thread::sleep(Duration::from_millis(60));
                if since.elapsed() > AWAIT_TURN_CAP {
                    tracing::warn!("回合周期超时,回待唤醒(前端没回信)");
                    d.rt.publish(VoiceEvent::ListenEnded { reason: "wake_done".into() });
                    phase = Phase::Watch;
                }
            }
            Phase::FollowUp { window } => {
                // 跟进窗:免唤醒接话;安静结束不追问(robot 纪律:对话自然结束别烦人)
                pipe.drain();
                vad.reset();
                d.rt.arm_wake_ctl(); // 在听时点停也要响应(取消 → 走 None 安静回 Watch;定稿 → 正常识别)
                d.rt.publish(VoiceEvent::State { phase: VoicePhase::Listening });
                let out =
                    collect_utterance(&pipe, &vad, &d.rt, Some(d.rt.wake_ctl()), window, hangover)?;
                phase = match transcribe(d, out)? {
                    Some((text, speaker_id)) => {
                        d.rt.publish(VoiceEvent::Transcribed { text, via: "wake".into(), speaker_id });
                        d.rt.publish(VoiceEvent::State { phase: VoicePhase::Idle });
                        Phase::AwaitTurn(Instant::now())
                    }
                    None => {
                        d.rt.publish(VoiceEvent::ListenEnded { reason: "follow_up_idle".into() });
                        d.rt.publish(VoiceEvent::State { phase: VoicePhase::Idle });
                        Phase::Watch
                    }
                };
            }
        }
    }
}

/// 播一条短句的同时持续清麦(实时清回声、不积压),输出收尾(ready)即返回 —— 调用方紧接开录。
/// 比"阻塞播完再一次性 drain"少丢用户紧接应答音抢说的头几个字(#5)。无音频(降级)= 不阻塞、清一次。
fn ack_and_drain(prompts: &PromptBank, pipe: &super::CapturePipe, kind: PromptKind) {
    let mut drained = 0usize;
    if let Some(ready) = prompts.play_async(kind) {
        while !ready.load(Ordering::Acquire) {
            drained += pipe.drain();
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    drained += pipe.drain(); // 收尾再清一次,此后到的都是用户的话
    tracing::info!(drained, "应答音期间清麦帧数(#5 真机诊断:积压越大越说明阻塞期丢帧)");
}

// ---- 唤醒确认层(命中 → 静默续录 → ASR → 拼音三段式) ----

/// 三段式判定结果。
pub(super) enum Triage {
    /// 孤立呼名(唤醒词后 ≤WAKE_TRAIL_MAX 个音节):走经典唤醒(应答音 + 开录)。
    Wake,
    /// 呼名+续句(「天天暂停」「看天天向上」):整句交模型仲裁(是不是叫我由语义定,
    /// 规则层分不开——续句归因在单麦克风上无解,交给带上下文的模型)。
    Overheard,
    /// 转写里没有唤醒词:KWS 幻听(音乐/噪声),静默拒。
    Reject,
}

/// 字母 → 中文读法(中国人念字母缩写的通行读音,选贴近实际发音的字)。
/// 名字派生唤醒词与确认层转写归一**共用这一张表**:KWS 把「BT」编成「逼踢」的拼音去听,
/// ASR 转写出拉丁原文「BT」时也展开成同一串音节 —— 两头对得上,真呼叫才不会被当幻听。
pub(super) const LETTER_READINGS: [&str; 26] = [
    "诶",     // A
    "逼",     // B
    "西",     // C
    "迪",     // D
    "衣",     // E
    "艾弗",   // F
    "鸡",     // G
    "艾曲",   // H
    "艾",     // I
    "杰",     // J
    "开",     // K
    "艾勒",   // L
    "艾姆",   // M
    "恩",     // N
    "欧",     // O
    "批",     // P
    "抠",     // Q
    "阿尔",   // R
    "艾丝",   // S
    "踢",     // T
    "优",     // U
    "威",     // V
    "达不溜", // W
    "艾克斯", // X
    "歪",     // Y
    "贼",     // Z
];

/// 数字 → 中文读法(逐位念:7274 → 七二七四)。
pub(super) const DIGIT_READINGS: [&str; 10] =
    ["零", "一", "二", "三", "四", "五", "六", "七", "八", "九"];

/// 没改名(或名字派生不出)时的唤醒词:默认名 BT 的两种叫法 —— 小名「BT(逼踢)」+
/// 全号读法「七二七四」(2026-07-10 用户拍板:默认名 7274→BT,唤醒词 = 名字派生,
/// 原独立唤醒词设置与默认词「小七」一并退役)。改了名就纯跟名字,这组附带词作废。
pub(super) const DEFAULT_WAKE_WORDS: [&str; 2] = ["逼踢", "七二七四"];

/// 名字 → 可唤醒形(§8.2「起什么名字就怎么唤醒」):中文原样、数字转读法、字母缩写按
/// 字母读音(BT→逼踢);分隔符/标点/表情丢弃。英文单词式名字(≥3 个字母且含小写,如
/// Buddy)是按英文单词念的、字母读音救不了(英文 KWS 实测零召回,§7.5)→ None,
/// 调用方回落 `DEFAULT_WAKE_WORDS` 并让 UI 如实提示(§3.5 不静默哑掉)。
pub(super) fn derive_wake_word(name: &str) -> Option<String> {
    use pinyin::ToPinyin;
    let chars: Vec<char> = name.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if ch.is_ascii_alphabetic() {
            let mut j = i;
            while j < chars.len() && chars[j].is_ascii_alphabetic() {
                j += 1;
            }
            let run = &chars[i..j];
            if run.len() >= 3 && run.iter().any(|c| c.is_ascii_lowercase()) {
                return None; // 英文单词名:整个名字判「语音喊不了」
            }
            for c in run {
                out.push_str(LETTER_READINGS[(c.to_ascii_uppercase() as u8 - b'A') as usize]);
            }
            i = j;
        } else if ch.is_ascii_digit() {
            out.push_str(DIGIT_READINGS[(ch as u8 - b'0') as usize]);
            i += 1;
        } else {
            // 只收「有拼音」的字(KWS 按拼音 token 听);emoji/标点/空格当分隔符丢弃,
            // 否则一个表情就让 encode_keywords 整词丢弃、唤醒无声哑掉。
            if ch.to_pinyin().is_some() {
                out.push(ch);
            }
            i += 1;
        }
    }
    (!out.is_empty()).then_some(out)
}

/// 名字 → 唤醒词表(纯函数,可单测)。返回 (词表, 是否回落默认):
/// 空名 = 没改名 → 默认词组(不算回落);起了名但派生不出 → 默认词组 + fallback=true
/// (UI 据此如实提示「这个名字语音喊不了」)。
pub(super) fn resolve_wake_words(pet_name: &str) -> (Vec<String>, bool) {
    let name = pet_name.trim();
    if name.is_empty() {
        return (DEFAULT_WAKE_WORDS.iter().map(|s| s.to_string()).collect(), false);
    }
    match derive_wake_word(name) {
        Some(w) => (vec![w], false),
        None => (DEFAULT_WAKE_WORDS.iter().map(|s| s.to_string()).collect(), true),
    }
}

/// 转写归一成「音节串」:汉字 → 无调拼音;ASCII 字母/数字按中文读法展开(BT → bi ti、
/// 7 → qi,与名字派生同一张表 —— ASR 吐拉丁原文也能对上派生的唤醒词);标点/空白丢弃。
/// 同音字(甜甜/田田/天天)因此等价 —— 确认层不因 ASR 选错字而误杀(§8.2「宁松勿严」)。
pub(super) fn to_syllables(s: &str) -> Vec<String> {
    use pinyin::ToPinyin;
    fn push_reading(out: &mut Vec<String>, reading: &str) {
        use pinyin::ToPinyin;
        for c in reading.chars() {
            if let Some(py) = c.to_pinyin() {
                out.push(py.plain().to_string());
            }
        }
    }
    let mut out = Vec::new();
    for ch in s.chars() {
        if ch.is_ascii_alphabetic() {
            push_reading(&mut out, LETTER_READINGS[(ch.to_ascii_uppercase() as u8 - b'A') as usize]);
        } else if ch.is_ascii_digit() {
            push_reading(&mut out, DIGIT_READINGS[(ch as u8 - b'0') as usize]);
        } else if let Some(py) = ch.to_pinyin() {
            out.push(py.plain().to_string());
        }
    }
    out
}

/// 三段式判定(纯函数,可单测):按拼音在转写里找唤醒词(取**末次**出现——「天天天天」
/// 连喊、末次在句尾 = 孤立呼名),看其后还剩几个音节。只看尾随不看前文:中文里句尾的
/// 名字几乎都是呼语(「过来,天天」),句中的才是嵌入(「我们天天去公园」→ 尾随≥2)。
/// 多唤醒词取最像呼名的判定(Wake > Overheard > Reject)。
pub(super) fn triage_transcript(transcript: &str, keywords: &[String]) -> Triage {
    let syl = to_syllables(transcript);
    let mut best = Triage::Reject;
    for kw in keywords {
        let kws = to_syllables(kw);
        if kws.is_empty() || syl.len() < kws.len() {
            continue;
        }
        let mut last: Option<usize> = None;
        for i in 0..=(syl.len() - kws.len()) {
            if syl[i..i + kws.len()] == kws[..] {
                last = Some(i);
            }
        }
        let Some(i) = last else { continue };
        let trailing = syl.len() - (i + kws.len());
        if trailing <= WAKE_TRAIL_MAX {
            return Triage::Wake; // 最强判定,直接定
        }
        best = Triage::Overheard;
    }
    best
}

/// 确认层的静默续录:命中后接着收帧,VAD 只当「说没说话」的表(段产物丢弃)。
/// 停录条件:一直没人声(CONFIRM_CONT_WINDOW)/ 有人声后断句(CONFIRM_CLOSE)/ 上限。
/// 与 collect_utterance 不共用:它是「等 VAD 出段」语义,唤醒词尾音这类 < min_speech
/// 的短爆音会让它挂到 hard_cap(十几秒)才回 —— 确认层要的是快断句。
fn confirm_tail(
    pipe: &super::CapturePipe,
    vad: &sherpa_onnx::VoiceActivityDetector,
) -> Result<Vec<f32>> {
    let started = Instant::now();
    let mut tail: Vec<f32> = Vec::new();
    let mut win_buf: Vec<f32> = Vec::with_capacity(super::VAD_WINDOW * 8);
    let mut speech_seen = false;
    let mut last_speech = Instant::now();
    loop {
        match pipe.rx.recv_timeout(Duration::from_millis(50)) {
            Ok(chunk) => {
                let s16k = pipe.to_16k(&chunk);
                tail.extend_from_slice(&s16k);
                win_buf.extend_from_slice(&s16k);
                while win_buf.len() >= super::VAD_WINDOW {
                    let win: Vec<f32> = win_buf.drain(..super::VAD_WINDOW).collect();
                    vad.accept_waveform(&win);
                    if vad.detected() {
                        speech_seen = true;
                        last_speech = Instant::now();
                    }
                    while !vad.is_empty() {
                        vad.pop(); // 段产物不用(整段 tail 自己带),别让它堆积
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Err(anyhow!("麦克风流中断")),
        }
        if !speech_seen && started.elapsed() > CONFIRM_CONT_WINDOW {
            return Ok(tail); // 没有续句(孤立呼名的形状)
        }
        if speech_seen && last_speech.elapsed() > CONFIRM_CLOSE {
            return Ok(tail); // 续句说完(断句)
        }
        if started.elapsed() > CONFIRM_TAIL_MAX {
            return Ok(tail); // 说个不停:拿已有的整句去仲裁
        }
    }
}

/// KWS 命中后的确认层:不立刻出声 —— 静默续录到断句,ASR 整句,拼音三段式分路。
/// fail-open:ASR 挂了宁可当经典唤醒(绝不因确认层故障叫不应)。
fn on_hit(
    d: &WakeDeps,
    pipe: &super::CapturePipe,
    vad: &sherpa_onnx::VoiceActivityDetector,
    hangover: f32,
    ring: &[f32],
) -> Result<Phase> {
    d.rt.publish(VoiceEvent::WakeCandidate); // 前端:提前 duck + 轻视觉「在听」
    super::prompts::play_chirp_async(); // 即时「叮」:第一时间告诉用户听到了(真唤醒/旁听的判定仍在后台跑)
    vad.reset();
    let tail = confirm_tail(pipe, vad)?;
    vad.reset();
    let mut pcm = Vec::with_capacity(ring.len() + tail.len());
    pcm.extend_from_slice(ring);
    pcm.extend_from_slice(&tail);
    peak_normalize(&mut pcm);
    let transcript = match d.asr.transcribe(&pcm) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(err = %format!("{e:#}"), "确认层 ASR 失败,fail-open 当经典唤醒");
            return on_wake(d, pipe, vad, hangover);
        }
    };
    let keywords = d.rt.wake_keywords();
    match triage_transcript(&transcript, &keywords) {
        Triage::Wake => {
            tracing::info!(%transcript, "唤醒确认:孤立呼名 → 经典唤醒");
            on_wake(d, pipe, vad, hangover)
        }
        Triage::Overheard => {
            // 声纹同段识别(有注册才有):仲裁回合的记忆归人跟说话人走
            let speaker_id =
                d.speaker.as_ref().and_then(|s| s.identify(&pcm, &d.rt.voiceprint_library()));
            tracing::info!(%transcript, ?speaker_id, "唤醒确认:呼名+续句 → 交模型仲裁(旁听)");
            d.rt.publish(VoiceEvent::Overheard { text: transcript, speaker_id });
            pipe.drain();
            Ok(Phase::Watch)
        }
        Triage::Reject => {
            // 真机排「为什么叫了没应」全靠这行日志(§3.5 不静默失败的观测面)
            tracing::info!(%transcript, "唤醒确认:转写无唤醒词,判幻听拒绝");
            d.rt.publish(VoiceEvent::WakeRejected);
            pipe.drain();
            Ok(Phase::Watch)
        }
    }
}

/// 唤醒命中后的一轮交互:应答 → 听 →(两段式兜底)→ 产出或告退。
fn on_wake(
    d: &WakeDeps,
    pipe: &super::CapturePipe,
    vad: &sherpa_onnx::VoiceActivityDetector,
    hangover: f32,
) -> Result<Phase> {
    // 取当前应答音银行快照:运行时可能已按新音色热替换,本轮交互用这一份(下轮再取新的)。
    let prompts = d.prompts.lock().expect("prompts lock").clone();
    d.rt.publish(VoiceEvent::WakeTriggered);
    ack_and_drain(&prompts, pipe, PromptKind::Ack); // 边播应答音边清麦,播完即开录(0 间隙)

    // 武装「停 / 定稿」信号一次(整轮共用):清掉上一轮遗留;之后由 listen_stop 写(在听时前端点
    // ✕ / 定稿键)。**不在循环内重置** —— 追问间隙(「没听清」仍显停控件)用户点停也要认,重置会冲掉它。
    d.rt.arm_wake_ctl();
    for attempt in 0..2 {
        vad.reset();
        d.rt.publish(VoiceEvent::State { phase: VoicePhase::Listening });
        let out = collect_utterance(pipe, vad, &d.rt, Some(d.rt.wake_ctl()), WAKE_START_TIMEOUT, hangover)?;
        if matches!(out, CaptureOut::Cancelled) {
            // 用户点了「取消」(✕):安静回待唤醒,不追问不告退(定稿键走 CTL_ACCEPT → 落到下面正常识别)
            d.rt.publish(VoiceEvent::ListenEnded { reason: "wake_done".into() });
            d.rt.publish(VoiceEvent::State { phase: VoicePhase::Idle });
            pipe.drain();
            return Ok(Phase::Watch);
        }
        if let Some((text, speaker_id)) = transcribe(d, out)? {
            d.rt.publish(VoiceEvent::Transcribed { text, via: "wake".into(), speaker_id });
            d.rt.publish(VoiceEvent::State { phase: VoicePhase::Idle });
            return Ok(Phase::AwaitTurn(Instant::now()));
        }
        if attempt == 0 {
            // 第一次没听到/没听出字:出声追问,立即重听(绝不静默失败)
            d.rt.publish(VoiceEvent::ListenEnded { reason: "no_speech_retry".into() });
            ack_and_drain(&prompts, pipe, PromptKind::Retry);
        }
    }
    // 两轮都空:有声告退(robot 是安静退,用户点名要出声)
    prompts.play_blocking(PromptKind::Farewell);
    d.rt.publish(VoiceEvent::ListenEnded { reason: "farewell".into() });
    d.rt.publish(VoiceEvent::State { phase: VoicePhase::Idle });
    pipe.drain();
    Ok(Phase::Watch)
}

/// 采集产物 →(文本, 说话人);空段/空文本 = None。
fn transcribe(d: &WakeDeps, out: CaptureOut) -> Result<Option<(String, Option<i64>)>> {
    let mut pcm = match out {
        CaptureOut::Utterance(pcm) => pcm,
        CaptureOut::Empty | CaptureOut::Cancelled => return Ok(None),
    };
    if (pcm.len() as f32) < super::MIN_SPEECH_S * super::TARGET_RATE as f32 {
        return Ok(None);
    }
    d.rt.publish(VoiceEvent::State { phase: VoicePhase::Transcribing });
    peak_normalize(&mut pcm);
    let text = d.asr.transcribe(&pcm)?;
    if text.is_empty() {
        return Ok(None);
    }
    let speaker_id = d.speaker.as_ref().and_then(|s| s.identify(&pcm, &d.rt.voiceprint_library()));
    Ok(Some((text, speaker_id)))
}

// ---- 唤醒词 → KWS token 行 ----

/// 中文唤醒词 → keywords_buf("声母 带调韵母 … @原词"行;robot 坑:整字直拼会
/// token-not-in-vocab)。**用模型词表本身裁决切分**(先 2 字声母 zh/ch/sh,再 1 字,
/// 最后整音节),绕开拼音 strict 模式的 y/w 歧义;切不动的词整个丢弃并告警。
pub(super) fn encode_keywords(
    words: &[String],
    vocab: &HashSet<String>,
) -> (String, Vec<String>) {
    use pinyin::ToPinyin;
    let mut lines = Vec::new();
    let mut dropped = Vec::new();
    'word: for word in words {
        let word = word.trim();
        if word.is_empty() {
            continue;
        }
        let mut tokens: Vec<String> = Vec::new();
        for ch in word.chars() {
            let Some(py) = ch.to_pinyin() else {
                tracing::warn!(word, %ch, "唤醒词含非中文字符,整词丢弃");
                dropped.push(word.to_string());
                continue 'word;
            };
            match split_syllable(py.with_tone(), vocab) {
                Some(mut t) => tokens.append(&mut t),
                None => {
                    tracing::warn!(word, syllable = py.with_tone(), "音节切不进模型词表,整词丢弃");
                    dropped.push(word.to_string());
                    continue 'word;
                }
            }
        }
        if tokens.is_empty() {
            dropped.push(word.to_string());
            continue;
        }
        lines.push(format!("{} @{}", tokens.join(" "), word));
    }
    (lines.join("\n"), dropped)
}

/// 带「拼写覆盖」的 keywords_buf 构建:标定(calib)可为某个词产出一行更贴合用户发音的
/// token 行,存进 `voice.wake.spelling`(词→整行 "tok … @词")。命中覆盖 → 直接用该行;
/// 否则回落 canonical `encode_keywords`。覆盖按「词」键入,换了唤醒词旧覆盖自然失效(无对应键)。
pub(super) fn build_keywords_buf(
    words: &[String],
    overrides: &std::collections::HashMap<String, String>,
    vocab: &HashSet<String>,
) -> (String, Vec<String>) {
    let mut lines = Vec::new();
    let mut dropped = Vec::new();
    for word in words {
        let word = word.trim();
        if word.is_empty() {
            continue;
        }
        if let Some(line) = overrides.get(word).filter(|l| !l.trim().is_empty()) {
            lines.push(line.clone());
            continue;
        }
        let (buf, drp) = encode_keywords(std::slice::from_ref(&word.to_string()), vocab);
        if buf.is_empty() {
            dropped.extend(drp);
        } else {
            lines.push(buf);
        }
    }
    (lines.join("\n"), dropped)
}

/// 单音节按词表切。**先试整音节**:零声母音节(韵母开头,如 ài/ān/ér)本身就是
/// 词表里的一个 token,必须整体匹配——否则会被下面的拆分逻辑错切成「首字符+剩余」
/// (ài→à+i),喂给模型的是它不认的 token,唤醒永不命中。有声母音节(xiǎo)整音节
/// 不在词表,自然落到拆分:声母(2 字优先,如 zh/ch/sh)+ 带调韵母。
pub(super) fn split_syllable(s: &str, vocab: &HashSet<String>) -> Option<Vec<String>> {
    if vocab.contains(s) {
        return Some(vec![s.to_string()]);
    }
    let chars: Vec<char> = s.chars().collect();
    for plen in [2usize, 1] {
        if chars.len() > plen {
            let head: String = chars[..plen].iter().collect();
            let tail: String = chars[plen..].iter().collect();
            if vocab.contains(&head) && vocab.contains(&tail) {
                return Some(vec![head, tail]);
            }
        }
    }
    None
}

pub(super) fn load_vocab(tokens_txt: &std::path::Path) -> Result<HashSet<String>> {
    let text = std::fs::read_to_string(tokens_txt)
        .with_context(|| format!("读 {} 失败", tokens_txt.display()))?;
    Ok(text.lines().filter_map(|l| l.split_whitespace().next()).map(str::to_string).collect())
}

/// KWS spotter 配置:int8 三件套 + 词表 + 阈值/boost + keywords_buf(免落盘)。
/// 唤醒循环与 calib 标定共用同一份模型路径和默认参数 —— 改一处两处一致。
pub(super) fn kws_config(
    kws_dir: &std::path::Path,
    keywords_buf: &str,
    threshold: f32,
    score: f32,
) -> sherpa_onnx::KeywordSpotterConfig {
    let mut kcfg = sherpa_onnx::KeywordSpotterConfig::default();
    let p = |name: &str| Some(kws_dir.join(name).to_string_lossy().into_owned());
    kcfg.model_config.transducer.encoder = p("encoder-epoch-12-avg-2-chunk-16-left-64.int8.onnx");
    kcfg.model_config.transducer.decoder = p("decoder-epoch-12-avg-2-chunk-16-left-64.int8.onnx");
    kcfg.model_config.transducer.joiner = p("joiner-epoch-12-avg-2-chunk-16-left-64.int8.onnx");
    kcfg.model_config.tokens = p("tokens.txt");
    kcfg.keywords_threshold = threshold;
    kcfg.keywords_score = score;
    kcfg.keywords_buf = Some(keywords_buf.to_string());
    kcfg
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vocab(tokens: &[&str]) -> HashSet<String> {
        tokens.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn keywords_encode_with_initial_final_split() {
        // 模型词表风格:声母 + 带调韵母("x"+"iǎo","q"+"ī")
        let v = vocab(&["x", "iǎo", "q", "ī", "ài"]);
        let (buf, dropped) = encode_keywords(&["小七".into(), "小爱".into()], &v);
        assert!(dropped.is_empty(), "都应编码成功: {dropped:?}");
        let lines: Vec<&str> = buf.lines().collect();
        assert_eq!(lines[0], "x iǎo q ī @小七");
        assert_eq!(lines[1], "x iǎo ài @小爱", "零声母字(爱)整音节命中");
    }

    #[test]
    fn unencodable_words_are_dropped_loudly() {
        let v = vocab(&["x", "iǎo"]);
        let (buf, dropped) = encode_keywords(
            &["小七".into(), "hello".into(), "小".into()],
            &v,
        );
        assert_eq!(dropped, vec!["小七".to_string(), "hello".to_string()], "七编不出/非中文都丢");
        assert_eq!(buf, "x iǎo @小", "能编的留下");
    }

    #[test]
    fn two_char_initials_split_first() {
        // zh/ch/sh 双字声母优先(防 "zh"+"ōng" 被切成 "z"+"hōng")
        let v = vocab(&["zh", "ōng", "z", "hōng"]);
        let got = split_syllable("zhōng", &v).unwrap();
        assert_eq!(got, vec!["zh".to_string(), "ōng".to_string()]);
    }

    // ---- 确认层三段式(纯函数) ----

    fn kw(words: &[&str]) -> Vec<String> {
        words.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn triage_isolated_call_wakes() {
        let k = kw(&["天天"]);
        assert!(matches!(triage_transcript("天天", &k), Triage::Wake), "裸呼名");
        assert!(matches!(triage_transcript("天天。", &k), Triage::Wake), "标点丢弃");
        assert!(matches!(triage_transcript("天天呀", &k), Triage::Wake), "尾随语气词 ≤1 仍是呼名");
        assert!(matches!(triage_transcript("天天天天", &k), Triage::Wake), "连喊取末次出现,句尾=呼名");
        assert!(matches!(triage_transcript("过来天天", &k), Triage::Wake), "句尾呼语(前文不否决)");
    }

    #[test]
    fn triage_homophones_still_wake() {
        // ASR 选错字(甜甜/田田)按拼音等价 —— 不因错字误杀(§8.2 宁松勿严)
        let k = kw(&["天天"]);
        assert!(matches!(triage_transcript("甜甜", &k), Triage::Wake));
        assert!(matches!(triage_transcript("田田", &k), Triage::Wake));
    }

    #[test]
    fn triage_continuation_goes_overheard() {
        let k = kw(&["天天"]);
        assert!(matches!(triage_transcript("天天向上", &k), Triage::Overheard), "节目名");
        assert!(matches!(triage_transcript("看天天向上真好看", &k), Triage::Overheard));
        assert!(matches!(triage_transcript("我们天天去公园", &k), Triage::Overheard), "副词嵌入");
        assert!(matches!(triage_transcript("天天暂停", &k), Triage::Overheard), "连说指令 → 交模型仲裁执行");
        assert!(matches!(triage_transcript("甜甜圈好吃", &k), Triage::Overheard), "同音嵌入也交仲裁");
    }

    #[test]
    fn triage_no_keyword_rejects() {
        let k = kw(&["天天"]);
        assert!(matches!(triage_transcript("今天气不错", &k), Triage::Reject), "只有一个「天」不算");
        assert!(matches!(triage_transcript("", &k), Triage::Reject), "空转写=幻听");
        assert!(matches!(triage_transcript("音乐轰隆隆", &k), Triage::Reject));
    }

    #[test]
    fn triage_multi_keyword_prefers_wake() {
        // 多唤醒词:任一判成呼名即唤醒(Wake > Overheard > Reject)
        let k = kw(&["小七", "天天"]);
        assert!(matches!(triage_transcript("小七", &k), Triage::Wake));
        assert!(matches!(triage_transcript("天天向上", &k), Triage::Overheard));
        assert!(matches!(triage_transcript("小七和天天向上", &k), Triage::Overheard), "小七也带尾随");
    }

    #[test]
    fn syllables_expand_ascii_by_chinese_readings() {
        // ASCII 按中文读法展开(与名字派生同一张表):ASR 吐拉丁原文也对得上派生词
        assert_eq!(to_syllables("BT"), vec!["bi", "ti"]);
        assert_eq!(to_syllables("7274"), vec!["qi", "er", "qi", "si"]);
        assert_eq!(to_syllables("天天 OK 啦"), vec!["tian", "tian", "ou", "kai", "la"]);
    }

    #[test]
    fn derive_wake_word_from_name() {
        // 中文原样 / 数字读法 / 字母缩写按字母读音 / 分隔符与 emoji 丢弃
        assert_eq!(derive_wake_word("天天"), Some("天天".into()));
        assert_eq!(derive_wake_word("BT"), Some("逼踢".into()));
        assert_eq!(derive_wake_word("bt"), Some("逼踢".into()), "短字母串不看大小写");
        assert_eq!(derive_wake_word("小7"), Some("小七".into()));
        assert_eq!(derive_wake_word("7274"), Some("七二七四".into()));
        assert_eq!(derive_wake_word("BT-7274"), Some("逼踢七二七四".into()), "分隔符丢弃");
        assert_eq!(derive_wake_word("GPT"), Some("鸡批踢".into()), "全大写=缩写,逐字母读");
        assert_eq!(derive_wake_word("天天🐶"), Some("天天".into()), "emoji 当分隔符,不许它毒死整词");
        // 英文单词式(≥3 字母且含小写)是按单词念的,字母读音救不了 → None(回落默认词)
        assert_eq!(derive_wake_word("Buddy"), None);
        assert_eq!(derive_wake_word("Max"), None);
        assert_eq!(derive_wake_word("小Buddy"), None, "带单词段的名字整个判喊不了");
        assert_eq!(derive_wake_word(""), None);
        assert_eq!(derive_wake_word("!!"), None, "全是派生不出的字符");
    }

    #[test]
    fn resolve_wake_words_fallback_semantics() {
        // 空名 = 没改名 → 默认词组,不算回落;派生失败 → 默认词组 + fallback 提示位
        let (words, fb) = resolve_wake_words("");
        assert_eq!(words, DEFAULT_WAKE_WORDS.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert!(!fb);
        let (words, fb) = resolve_wake_words("天天");
        assert_eq!(words, vec!["天天".to_string()]);
        assert!(!fb);
        let (words, fb) = resolve_wake_words("Buddy");
        assert_eq!(words, DEFAULT_WAKE_WORDS.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert!(fb, "起了名但喊不了 → 回落 + 提示");
    }

    #[test]
    fn triage_matches_latin_transcript_against_derived_keyword() {
        // 名字 BT → 唤醒词「逼踢」;ASR 常吐拉丁原文「BT」——两头经同一张读音表归一后必须相等,
        // 否则确认层会把真呼叫当幻听拒掉(这正是派生表与 to_syllables 共用一张的原因)。
        let k = kw(&["逼踢"]);
        assert!(matches!(triage_transcript("BT", &k), Triage::Wake), "拉丁原文孤立呼名");
        assert!(matches!(triage_transcript("逼踢", &k), Triage::Wake));
        assert!(matches!(triage_transcript("必提", &k), Triage::Wake), "同音字等价(无调拼音)");
        assert!(matches!(triage_transcript("BT暂停", &k), Triage::Overheard), "呼名+续句交仲裁");
        let k74 = kw(&["七二七四"]);
        assert!(matches!(triage_transcript("7274", &k74), Triage::Wake), "数字原文对得上读法");
    }

    #[test]
    fn zero_initial_syllable_kept_whole_even_if_fragments_in_vocab() {
        // 真实模型词表同时含韵母 ài/ér 和它们的"碎片" à/i、é/r。零声母字必须整体
        // 匹配(ài/ér),不能被错拆成 à+i、é+r(修复前的 bug:plen=1 先于整音节)。
        let v = vocab(&["x", "iǎo", "ài", "à", "i", "n", "ǚ", "ér", "é", "r"]);
        let (buf, dropped) = encode_keywords(&["小爱".into(), "女儿".into()], &v);
        assert!(dropped.is_empty(), "都应编码成功: {dropped:?}");
        let lines: Vec<&str> = buf.lines().collect();
        assert_eq!(lines[0], "x iǎo ài @小爱", "ài 整体,不拆成 à i");
        assert_eq!(lines[1], "n ǚ ér @女儿", "ér 整体,不拆成 é r");
    }
}
