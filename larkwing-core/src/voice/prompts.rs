//! 唤醒短句音频银行(PLAN §11 C):应答"哎"/追问/告退 —— **延迟敏感短句走 core 原生直出**
//! (播放分两路的另一路):预合成(进 TTS 缓存)→ symphonia 解码 → 裁首尾静音
//! (robot 坑:"哎"被 TTS padding 到 1.3s)→ PCM 内存常驻 → cpal 输出流直接播,
//! 同进程精确知道播完时刻 → 应答一停立即开录(0 间隙)。
//! 话术全部来自场景数据(人格中立底座:这里没有任何一句写死的话)。

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::scenes::SceneVoice;

pub(super) struct Clip {
    pub samples: Vec<f32>,
    pub rate: u32,
}

#[derive(Default)]
pub(super) struct PromptBank {
    acks: Vec<Clip>,
    retry: Vec<Clip>,
    farewell: Vec<Clip>,
    rotor: AtomicUsize,
}

/// 可热替换的应答音银行(问题1-B):运行时换音色 → 后台 prepare 新银行整体替换;
/// 唤醒线程每次取最新快照来播。KWS 检测与麦克风不受影响(不重启唤醒循环)。
pub(super) type SharedPromptBank = Arc<std::sync::Mutex<Arc<PromptBank>>>;

#[derive(Clone, Copy)]
pub(super) enum PromptKind {
    Ack,
    Retry,
    Farewell,
}

impl PromptBank {
    /// 预合成 + 解码 + 裁静音,best-effort:单句失败只少一条(断网 = 类目可能为空,
    /// 唤醒流程降级为无声 + 屏幕提示,绝不因此挂掉)。
    pub async fn prepare(rt: &super::VoiceRuntime, sv: &SceneVoice) -> PromptBank {
        let mut bank = PromptBank::default();
        for (kind, texts) in [
            (PromptKind::Ack, &sv.wake_acks),
            (PromptKind::Retry, &sv.retry),
            (PromptKind::Farewell, &sv.farewell),
        ] {
            for text in texts {
                match Self::clip_for(rt, text).await {
                    Ok(clip) => bank.bucket_mut(kind).push(clip),
                    Err(e) => {
                        tracing::warn!(text, err = %format!("{e:#}"), "唤醒短句预合成失败(降级无声)")
                    }
                }
            }
        }
        tracing::info!(
            acks = bank.acks.len(),
            retry = bank.retry.len(),
            farewell = bank.farewell.len(),
            "唤醒短句银行就绪"
        );
        bank
    }

    async fn clip_for(rt: &super::VoiceRuntime, text: &str) -> Result<Clip> {
        let path = rt.tts_to_file(text).await?; // 进同一个 TTS 缓存,二次启动零合成
        // 产物容器随音色引擎而变:在线 edge = mp3,离线 vits / 克隆音色 = wav。按扩展名选解码器,
        // 别一律当 mp3——否则克隆/离线音色的 wav 解不开 → 短句银行空 → 唤醒「叫不应」一声不出。
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("mp3").to_ascii_lowercase();
        let bytes = tokio::fs::read(&path).await?;
        tokio::task::spawn_blocking(move || -> Result<Clip> {
            let (pcm, rate) = if ext == "wav" { decode_wav(&bytes)? } else { decode_mp3(&bytes)? };
            let samples = trim_silence(&pcm, rate);
            anyhow::ensure!(!samples.is_empty(), "解码出空音频");
            Ok(Clip { samples, rate })
        })
        .await
        .context("解码任务挂了")?
    }

    fn bucket_mut(&mut self, kind: PromptKind) -> &mut Vec<Clip> {
        match kind {
            PromptKind::Ack => &mut self.acks,
            PromptKind::Retry => &mut self.retry,
            PromptKind::Farewell => &mut self.farewell,
        }
    }

    fn bucket(&self, kind: PromptKind) -> &[Clip] {
        match kind {
            PromptKind::Ack => &self.acks,
            PromptKind::Retry => &self.retry,
            PromptKind::Farewell => &self.farewell,
        }
    }

    /// 轮换取一条(均匀换着说,不需要真随机)。空类目 = None(降级无声)。
    pub fn pick(&self, kind: PromptKind) -> Option<&Clip> {
        let bucket = self.bucket(kind);
        if bucket.is_empty() {
            return None;
        }
        Some(&bucket[self.rotor.fetch_add(1, Ordering::Relaxed) % bucket.len()])
    }

    /// 播一条短句并**阻塞到播完**(Retry/Farewell 用,无延迟敏感)。
    pub fn play_blocking(&self, kind: PromptKind) -> bool {
        let Some(clip) = self.pick(kind) else { return false };
        if let Err(e) = play_pcm_blocking(&clip.samples, clip.rate) {
            tracing::warn!(err = %format!("{e:#}"), "短句播放失败");
            return false;
        }
        true
    }

    /// 后台播一条短句,返回"输出已收尾"信号(wake 用)。要点:播放不再阻塞 wake 线程,
    /// 后者可在播放期间持续清麦(实时清回声、不积压),信号一亮即开录 —— 比"阻塞播完再
    /// 一次性大清"少丢用户紧接应答音抢说的头几个字(#5)。None = 该类目无音频(降级无声)。
    pub fn play_async(&self, kind: PromptKind) -> Option<Arc<AtomicBool>> {
        let clip = self.pick(kind)?;
        let samples = clip.samples.clone(); // 短句几 KB,克隆进后台线程
        let rate = clip.rate;
        let ready = Arc::new(AtomicBool::new(false));
        let ready2 = ready.clone();
        std::thread::spawn(move || {
            if let Err(e) = play_pcm_blocking_signaled(&samples, rate, Some(&ready2)) {
                tracing::warn!(err = %format!("{e:#}"), "短句播放失败");
            }
            ready2.store(true, Ordering::Release); // 失败兜底:别让 wake 线程死等
        });
        Some(ready)
    }
}

/// mp3 → f32 mono PCM(symphonia,进程内,无 ffmpeg 冷启动)。
fn decode_mp3(bytes: &[u8]) -> Result<(Vec<f32>, u32)> {
    use symphonia::core::audio::SampleBuffer;
    use symphonia::core::errors::Error as SymErr;
    use symphonia::core::io::MediaSourceStream;

    let mss =
        MediaSourceStream::new(Box::new(std::io::Cursor::new(bytes.to_vec())), Default::default());
    let mut hint = symphonia::core::probe::Hint::new();
    hint.with_extension("mp3");
    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &Default::default(), &Default::default())
        .context("mp3 探测失败")?;
    let mut format = probed.format;
    let track = format.default_track().ok_or_else(|| anyhow!("没有音轨"))?;
    let track_id = track.id;
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &Default::default())
        .context("mp3 解码器创建失败")?;
    let mut rate = track.codec_params.sample_rate.unwrap_or(24_000);
    let mut out: Vec<f32> = Vec::new();
    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymErr::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(SymErr::ResetRequired) => break,
            Err(e) => return Err(e.into()),
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(SymErr::DecodeError(_)) => continue, // 坏帧跳过
            Err(e) => return Err(e.into()),
        };
        let spec = *decoded.spec();
        rate = spec.rate;
        let chans = spec.channels.count().max(1);
        let mut sbuf = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
        sbuf.copy_interleaved_ref(decoded);
        if chans == 1 {
            out.extend_from_slice(sbuf.samples());
        } else {
            out.extend(sbuf.samples().chunks(chans).map(|f| f.iter().sum::<f32>() / chans as f32));
        }
    }
    Ok((out, rate))
}

/// wav → f32 mono PCM(离线 vits / 克隆音色的产物;`tts::pcm_f32_to_wav` 的逆)。自带解析,
/// 免给 symphonia 加 wav/pcm 特性(本项目按 mp3-only 编)。逐子块扫到 data 读 16-bit PCM:
/// 唤醒短句走 cpal 原生直出需先解成 PCM(`<audio>` 那条路浏览器自解,故仅此处要解 wav)。
fn decode_wav(bytes: &[u8]) -> Result<(Vec<f32>, u32)> {
    anyhow::ensure!(
        bytes.len() >= 44 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WAVE",
        "不是 WAV(RIFF/WAVE 头缺失)"
    );
    let rate = u32::from_le_bytes([bytes[24], bytes[25], bytes[26], bytes[27]]); // fmt 子块固定在最前
    let mut i = 12; // 跳过 RIFF 头,逐子块扫
    while i + 8 <= bytes.len() {
        let id = &bytes[i..i + 4];
        let len =
            u32::from_le_bytes([bytes[i + 4], bytes[i + 5], bytes[i + 6], bytes[i + 7]]) as usize;
        let body = i + 8;
        if id == b"data" {
            let end = (body + len).min(bytes.len());
            let pcm = bytes[body..end]
                .chunks_exact(2)
                .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0)
                .collect();
            return Ok((pcm, rate));
        }
        i = body + len + (len & 1); // 子块按偶数字节对齐
    }
    bail!("WAV 没有 data 子块")
}

/// 裁首尾静音(robot 验证参数:幅值阈 0.015≈500/32768,留 30ms 余量)。
pub(super) fn trim_silence(pcm: &[f32], rate: u32) -> Vec<f32> {
    const THRESHOLD: f32 = 0.015;
    let margin = (rate as usize) * 30 / 1000;
    let first = pcm.iter().position(|s| s.abs() > THRESHOLD);
    let last = pcm.iter().rposition(|s| s.abs() > THRESHOLD);
    match (first, last) {
        (Some(a), Some(b)) => pcm[a.saturating_sub(margin)..(b + margin + 1).min(pcm.len())].to_vec(),
        _ => pcm.to_vec(), // 全静音(不该发生):原样返回,让上游 ensure 报警
    }
}

/// 阻塞播放(Retry/Farewell 用)。
pub(super) fn play_pcm_blocking(samples: &[f32], rate: u32) -> Result<()> {
    play_pcm_blocking_signaled(samples, rate, None)
}

/// 阻塞播放一段 PCM 到默认输出设备(cpal 直出;输入输出独立流,无 PortAudio 双工坑)。
/// ready:输出回调读完整段(应答音实际收尾)时置位 —— 调用方据此"播完即开录"。
pub(super) fn play_pcm_blocking_signaled(
    samples: &[f32],
    rate: u32,
    ready: Option<&AtomicBool>,
) -> Result<()> {
    let host = cpal::default_host();
    let device = host.default_output_device().ok_or_else(|| anyhow!("没有输出设备"))?;
    let supported = device.default_output_config().context("读取输出配置失败")?;
    let out_rate: u32 = supported.sample_rate();
    let channels = supported.channels().max(1) as usize;
    let cfg: cpal::StreamConfig = supported.config();

    let data: Arc<Vec<f32>> = Arc::new(if rate != out_rate {
        sherpa_onnx::LinearResampler::create(rate as i32, out_rate as i32)
            .ok_or_else(|| anyhow!("重采样器创建失败({rate}→{out_rate})"))?
            .resample(samples, true)
    } else {
        samples.to_vec()
    });
    let total = data.len();

    let pos = Arc::new(AtomicUsize::new(0));
    let done = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    let (pos2, done2, data2) = (pos.clone(), done.clone(), data.clone());
    if supported.sample_format() != cpal::SampleFormat::F32 {
        // mac CoreAudio / Win WASAPI 共享模式输出都是 f32;真撞上再补格式分支
        bail!("不支持的输出格式 {:?}", supported.sample_format());
    }
    let stream = device
        .build_output_stream(
            &cfg,
            move |out: &mut [f32], _: &cpal::OutputCallbackInfo| {
                let mut p = pos2.load(Ordering::Relaxed);
                for frame in out.chunks_mut(channels) {
                    let s = if p < data2.len() {
                        let v = data2[p];
                        p += 1;
                        v
                    } else {
                        0.0
                    };
                    for o in frame.iter_mut() {
                        *o = s;
                    }
                }
                pos2.store(p, Ordering::Relaxed);
                if p >= data2.len() {
                    let (m, cv) = &*done2;
                    *m.lock().expect("done lock") = true;
                    cv.notify_all();
                }
            },
            |e| tracing::warn!("短句输出流错误: {e}"),
            None,
        )
        .context("打开输出流失败")?;
    stream.play().context("启动输出流失败")?;

    let (m, cv) = &*done;
    let cap = Duration::from_millis(total as u64 * 1000 / out_rate.max(1) as u64 + 1500);
    let guard = m.lock().expect("done lock");
    let _ = cv.wait_timeout_while(guard, cap, |fin| !*fin).expect("condvar");
    // 输出回调已读完整段 = 应答音收尾。先放行开录(wake 据此 0 间隙起听),再 sleep 让设备
    // 缓冲里最后一截放净 —— 这点尾音此刻已在录,靠硬件 AEC 抑回声(宪法 §9 假设家用麦带 AEC)。
    if let Some(r) = ready {
        r.store(true, Ordering::Release);
    }
    std::thread::sleep(Duration::from_millis(80)); // 设备缓冲里最后一截放完
    drop(stream);
    Ok(())
}

// —— 唤醒反馈音「叮」= **降级兜底**(2026-07-11 起;v0.2.13 的「命中即叮」已被
// 「命中即人声应答」取代,用户拍板「立刻出声应答、录音不断」)——
// 只在应答音银行没就绪(首启预合成中/断网降级)时响:喊了名字绝不能没动静(§3.5)。
// **纯正弦短音**仍是刻意的:silero VAD 不把它当人声、ASR 不转字、浏览器采集 AEC3 直接消。
// 参数是单一真相源、可调(§4.11):清亮不刺耳、短到不打断人说话。
const CHIRP_RATE: u32 = 24_000;
const CHIRP_FREQ: f32 = 1_320.0; // ≈E6
const CHIRP_SECS: f32 = 0.11;
const CHIRP_GAIN: f32 = 0.28;

fn chirp_pcm() -> Vec<f32> {
    let n = (CHIRP_RATE as f32 * CHIRP_SECS) as usize;
    (0..n)
        .map(|i| {
            let t = i as f32 / CHIRP_RATE as f32;
            let attack = (t / 0.004).min(1.0); // 4ms 淡入,起头不爆音
            let decay = (-t / (CHIRP_SECS * 0.35)).exp(); // 钟形衰减,收尾不咔哒
            (2.0 * std::f32::consts::PI * CHIRP_FREQ * t).sin() * attack * decay * CHIRP_GAIN
        })
        .collect()
}

/// 非阻塞播「叮」:开条短线程 cpal 直出,绝不拖住唤醒确认层的静默续录。best-effort(播不出只少一声)。
pub(super) fn play_chirp_async() {
    std::thread::spawn(|| {
        if let Err(e) = play_pcm_blocking(&chirp_pcm(), CHIRP_RATE) {
            tracing::warn!(err = %format!("{e:#}"), "唤醒提示音播放失败");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chirp_is_short_bounded_pure_tone() {
        // 即时反馈音必须短、幅值受控(不刺耳)、无 NaN —— 参数漂了这条会亮。
        let pcm = chirp_pcm();
        assert_eq!(pcm.len(), (CHIRP_RATE as f32 * CHIRP_SECS) as usize);
        assert!(pcm.iter().all(|s| s.is_finite() && s.abs() <= CHIRP_GAIN + 1e-6));
        assert!(pcm.iter().any(|s| s.abs() > 0.05), "不能是静音");
    }

    #[test]
    fn decode_wav_roundtrips_pcm_f32_to_wav() {
        // 克隆/离线音色出 wav,唤醒短句要能解回 PCM(否则「叫不应」无声)。与 tts::pcm_f32_to_wav 对偶。
        let samples = vec![0.0f32, 0.5, -0.5, 0.25, -0.25, 1.0, -1.0];
        let wav = crate::voice::tts::pcm_f32_to_wav(&samples, 24_000);
        let (pcm, rate) = decode_wav(&wav).expect("解码 wav");
        assert_eq!(rate, 24_000);
        assert_eq!(pcm.len(), samples.len());
        for (a, b) in pcm.iter().zip(&samples) {
            assert!((a - b).abs() < 1e-3, "{a} vs {b}"); // i16 量化误差
        }
    }

    #[test]
    fn trim_cuts_padding_but_keeps_margin() {
        let rate = 16_000u32;
        let margin = (rate as usize) * 30 / 1000;
        // 0.5s 静音 + 0.2s 响声 + 0.6s 静音(robot:"哎"1.3s→0.4s 的坑形)
        let mut pcm = vec![0.001f32; 8000];
        pcm.extend(vec![0.5f32; 3200]);
        pcm.extend(vec![0.001f32; 9600]);
        let out = trim_silence(&pcm, rate);
        assert!(out.len() >= 3200 && out.len() <= 3200 + 2 * margin + 2, "len={}", out.len());
        assert!(out.iter().any(|s| s.abs() > 0.4), "响声主体还在");

        let silent = vec![0.0f32; 1000];
        assert_eq!(trim_silence(&silent, rate).len(), 1000, "全静音原样返回");
    }
}
