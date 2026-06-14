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
use super::prompts::{PromptBank, PromptKind};
use super::speaker::SpeakerId;
use super::{collect_utterance, hangover_secs, new_vad, open_capture, peak_normalize, CaptureOut};
use crate::bus::{VoiceEvent, VoicePhase};

pub(super) enum WakeCmd {
    Stop,
    /// 丢帧(听写中 / TTS 在念):防 KWS 自激与误吃。
    Suspend(bool),
    /// 回合念完:开 6s 跟进窗(免唤醒接话)。
    FollowUp,
    /// 回合失败/取消/不念:直接回待唤醒。
    Resume,
}

// ---- KWS 参数 ----
// threshold 不再锁死:由「唤醒灵敏度」滑块经 voice.wake.sensitivity 映射(见 mod.rs
// wake_threshold,默认对齐 robot 实战 0.45),建 spotter 时由 deps 传入。
// score 暂不暴露:关键词加分提召回,误触靠 threshold 拦。
const KWS_SCORE: f32 = 1.5;
const WAKE_START_TIMEOUT: Duration = Duration::from_secs(6); // 应答后没人开口
const FOLLOW_UP_WINDOW: Duration = Duration::from_secs(6); // 跟进窗(robot 终值)
const AWAIT_TURN_CAP: Duration = Duration::from_secs(180); // 前端没回信的兜底
const WATCHDOG_SILENCE: Duration = Duration::from_secs(30); // 监听态多久无帧 → 重开采集(robot 同款)

pub(super) struct WakeDeps {
    pub rt: super::VoiceRuntime,
    pub kws_dir: PathBuf,
    pub vad_model: PathBuf,
    pub asr: Arc<SherpaAsr>,
    pub prompts: PromptBank,
    pub keywords_buf: String,
    /// KWS 触发阈值(唤醒灵敏度滑块映射而来;建 spotter 时锁定,改灵敏度需重启循环)。
    pub kws_threshold: f32,
    /// 声纹(有家人注册才有;None = 不识别说话人,走会话用户)。
    pub speaker: Option<Arc<SpeakerId>>,
}

enum Phase {
    Watch,
    AwaitTurn(Instant),
    FollowUp,
}

impl Phase {
    fn label(&self) -> &'static str {
        match self {
            Phase::Watch => "Watch",
            Phase::AwaitTurn(_) => "AwaitTurn",
            Phase::FollowUp => "FollowUp",
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
    deps.rt.wake_cleanup();
    tracing::info!("免手唤醒已停止");
}

fn wake_loop(d: &WakeDeps, cmd: &Receiver<WakeCmd>) -> Result<()> {
    // ---- KWS(int8 三件套 + 词表;keywords_buf 免落盘) ----
    let mut kcfg = sherpa_onnx::KeywordSpotterConfig::default();
    let p = |name: &str| Some(d.kws_dir.join(name).to_string_lossy().into_owned());
    kcfg.model_config.transducer.encoder = p("encoder-epoch-12-avg-2-chunk-16-left-64.int8.onnx");
    kcfg.model_config.transducer.decoder = p("decoder-epoch-12-avg-2-chunk-16-left-64.int8.onnx");
    kcfg.model_config.transducer.joiner = p("joiner-epoch-12-avg-2-chunk-16-left-64.int8.onnx");
    kcfg.model_config.tokens = p("tokens.txt");
    kcfg.keywords_threshold = d.kws_threshold;
    kcfg.keywords_score = KWS_SCORE;
    kcfg.keywords_buf = Some(d.keywords_buf.clone());
    let spotter =
        sherpa_onnx::KeywordSpotter::create(&kcfg).ok_or_else(|| anyhow!("KWS 创建失败"))?;
    let kstream = spotter.create_stream();

    let hangover = hangover_secs(&d.rt.patience());
    let vad = new_vad(&d.vad_model, hangover)?;
    let mut pipe = open_capture(d.rt.input_device())?;
    let mut dump = WatchDump::from_env(); // 诊断:设了 LARKWING_KWS_DUMP_DIR 才落盘
    let mut suspended = false;
    let mut phase = Phase::Watch;
    let mut last_state: &'static str = "init"; // 状态切换日志(判喊话时循环是否在 Watch)
    let mut last_frame = Instant::now(); // watchdog:上次从麦克风拿到帧的时刻
    let mut force_reopen = false; // 流断开/无帧 → 下一轮重开采集
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
                Ok(WakeCmd::FollowUp) => {
                    if matches!(phase, Phase::AwaitTurn(_)) {
                        phase = Phase::FollowUp;
                    }
                }
                Ok(WakeCmd::Resume) => {
                    pipe.drain();
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
            match open_capture(d.rt.input_device()) {
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
                    kstream.accept_waveform(super::TARGET_RATE as i32, &s16k);
                    while spotter.is_ready(&kstream) {
                        spotter.decode(&kstream);
                    }
                    let hit = spotter
                        .get_result(&kstream)
                        .map(|r| r.keyword)
                        .filter(|k| !k.is_empty());
                    if let Some(keyword) = hit {
                        tracing::info!(%keyword, "唤醒命中");
                        spotter.reset(&kstream);
                        phase = on_wake(d, &pipe, &vad, hangover)?;
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
            Phase::FollowUp => {
                // 跟进窗:免唤醒接话;安静结束不追问(robot 纪律:对话自然结束别烦人)
                pipe.drain();
                vad.reset();
                d.rt.publish(VoiceEvent::State { phase: VoicePhase::Listening });
                let out =
                    collect_utterance(&pipe, &vad, &d.rt, None, FOLLOW_UP_WINDOW, hangover)?;
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

/// 唤醒命中后的一轮交互:应答 → 听 →(两段式兜底)→ 产出或告退。
fn on_wake(
    d: &WakeDeps,
    pipe: &super::CapturePipe,
    vad: &sherpa_onnx::VoiceActivityDetector,
    hangover: f32,
) -> Result<Phase> {
    d.rt.publish(VoiceEvent::WakeTriggered);
    ack_and_drain(&d.prompts, pipe, PromptKind::Ack); // 边播应答音边清麦,播完即开录(0 间隙)

    for attempt in 0..2 {
        vad.reset();
        d.rt.publish(VoiceEvent::State { phase: VoicePhase::Listening });
        let out = collect_utterance(pipe, vad, &d.rt, None, WAKE_START_TIMEOUT, hangover)?;
        if let Some((text, speaker_id)) = transcribe(d, out)? {
            d.rt.publish(VoiceEvent::Transcribed { text, via: "wake".into(), speaker_id });
            d.rt.publish(VoiceEvent::State { phase: VoicePhase::Idle });
            return Ok(Phase::AwaitTurn(Instant::now()));
        }
        if attempt == 0 {
            // 第一次没听到/没听出字:出声追问,立即重听(绝不静默失败)
            d.rt.publish(VoiceEvent::ListenEnded { reason: "no_speech_retry".into() });
            ack_and_drain(&d.prompts, pipe, PromptKind::Retry);
        }
    }
    // 两轮都空:有声告退(robot 是安静退,用户点名要出声)
    d.prompts.play_blocking(PromptKind::Farewell);
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

/// 单音节按词表切。**先试整音节**:零声母音节(韵母开头,如 ài/ān/ér)本身就是
/// 词表里的一个 token,必须整体匹配——否则会被下面的拆分逻辑错切成「首字符+剩余」
/// (ài→à+i),喂给模型的是它不认的 token,唤醒永不命中。有声母音节(xiǎo)整音节
/// 不在词表,自然落到拆分:声母(2 字优先,如 zh/ch/sh)+ 带调韵母。
fn split_syllable(s: &str, vocab: &HashSet<String>) -> Option<Vec<String>> {
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
