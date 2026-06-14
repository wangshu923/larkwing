//! `TtsEngine` trait + EdgeTts(msedge-tts,微软免费在线)+ **按 hash(音色|语速|文本) 落盘缓存**
//! (宪法 §7「少重复 TTS」兑现;blob 不进库走文件)。trait 是接缝承诺:离线 VITS 是
//! PLAN §11 D 期目录档,届时加实现不动调用方。非官方 API 风险记档(PLAN §11 watch)。

use std::path::Path;

use anyhow::{anyhow, ensure, Context, Result};
use sha2::Digest;

pub trait TtsEngine: Send + Sync {
    /// 整句合成到音频字节(格式见 ext)。rate_pct = 语速偏移(%,-15/0/+15 三档映射)。
    fn synthesize(&self, text: &str, voice: &str, rate_pct: i32) -> Result<Vec<u8>>;
    /// 产物容器扩展名(relay 按它给 Content-Type,缓存按它命名):mp3 | wav。
    fn ext(&self) -> &'static str;
}

/// 微软 Edge 朗读 API(与 robot 的 edge-tts 同一服务,长期稳定)。
/// 每次合成新建 websocket(简单可靠;句级缓存已大幅摊薄连接成本,池化等真瓶颈再说)。
pub struct EdgeTts;

impl TtsEngine for EdgeTts {
    fn synthesize(&self, text: &str, voice: &str, rate_pct: i32) -> Result<Vec<u8>> {
        let mut client =
            msedge_tts::tts::client::connect().context("TTS 服务连接失败(需要网络)")?;
        let cfg = msedge_tts::tts::SpeechConfig {
            voice_name: voice.to_string(),
            audio_format: "audio-24khz-48kbitrate-mono-mp3".to_string(),
            pitch: 0,
            rate: rate_pct,
            volume: 0,
        };
        let t0 = std::time::Instant::now();
        let audio = client.synthesize(text, &cfg).context("TTS 合成失败")?;
        ensure!(!audio.audio_bytes.is_empty(), "TTS 返回了空音频");
        tracing::info!(
            ms = t0.elapsed().as_millis() as u64,
            chars = text.chars().count(),
            bytes = audio.audio_bytes.len(),
            "TTS 合成完成(edge)"
        );
        Ok(audio.audio_bytes)
    }

    fn ext(&self) -> &'static str {
        "mp3"
    }
}

/// 本地离线 VITS(PLAN §11 D;melo-tts 中英双语,断网也能说)。出 wav(WebView 原生可播),
/// 免 mp3 编码依赖。模型贵(163M),加载一次进 OnceCell。
pub struct SherpaVits {
    tts: sherpa_onnx::OfflineTts,
}

impl SherpaVits {
    pub fn load(model_dir: &Path) -> Result<SherpaVits> {
        let p = |n: &str| Some(model_dir.join(n).to_string_lossy().into_owned());
        let mut cfg = sherpa_onnx::OfflineTtsConfig::default();
        cfg.model.vits.model = p("model.onnx");
        cfg.model.vits.lexicon = p("lexicon.txt");
        cfg.model.vits.tokens = p("tokens.txt");
        cfg.model.num_threads = 2;
        // 数字/日期/电话读法规则(melo 离线也要把「3点」读对)
        let fsts = ["date.fst", "number.fst", "phone.fst"]
            .iter()
            .map(|f| model_dir.join(f).to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(",");
        cfg.rule_fsts = Some(fsts);
        let t0 = std::time::Instant::now();
        let tts = sherpa_onnx::OfflineTts::create(&cfg).ok_or_else(|| anyhow!("离线 TTS 加载失败"))?;
        tracing::info!(ms = t0.elapsed().as_millis() as u64, "离线 TTS 模型加载完成(melo-vits)");
        Ok(SherpaVits { tts })
    }
}

impl TtsEngine for SherpaVits {
    fn synthesize(&self, text: &str, _voice: &str, rate_pct: i32) -> Result<Vec<u8>> {
        let cfg = sherpa_onnx::GenerationConfig {
            sid: 0,                                  // melo 单说话人
            speed: 1.0 + rate_pct as f32 / 100.0,    // 语速档(舒缓/标准/轻快)
            ..Default::default()
        };
        let t0 = std::time::Instant::now();
        let audio = self
            .tts
            .generate_with_config(text, &cfg, None::<fn(&[f32], f32) -> bool>)
            .ok_or_else(|| anyhow!("离线 TTS 合成失败"))?;
        let samples = audio.samples();
        ensure!(!samples.is_empty(), "离线 TTS 返回了空音频");
        let wav = pcm_f32_to_wav(samples, audio.sample_rate() as u32);
        tracing::info!(
            ms = t0.elapsed().as_millis() as u64,
            chars = text.chars().count(),
            "TTS 合成完成(离线 vits)"
        );
        Ok(wav)
    }

    fn ext(&self) -> &'static str {
        "wav"
    }
}

/// f32 PCM([-1,1])→ 16-bit WAV 字节(44 字节头 + i16 LE;WebView <audio> 原生可播)。
fn pcm_f32_to_wav(samples: &[f32], rate: u32) -> Vec<u8> {
    let data_len = samples.len() * 2;
    let mut buf = Vec::with_capacity(44 + data_len);
    let byte_rate = rate * 2; // mono, 16-bit
    buf.extend_from_slice(b"RIFF");
    buf.extend_from_slice(&((36 + data_len) as u32).to_le_bytes());
    buf.extend_from_slice(b"WAVE");
    buf.extend_from_slice(b"fmt ");
    buf.extend_from_slice(&16u32.to_le_bytes()); // PCM fmt chunk size
    buf.extend_from_slice(&1u16.to_le_bytes()); // PCM
    buf.extend_from_slice(&1u16.to_le_bytes()); // mono
    buf.extend_from_slice(&rate.to_le_bytes());
    buf.extend_from_slice(&byte_rate.to_le_bytes());
    buf.extend_from_slice(&2u16.to_le_bytes()); // block align
    buf.extend_from_slice(&16u16.to_le_bytes()); // bits/sample
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&(data_len as u32).to_le_bytes());
    for &s in samples {
        buf.extend_from_slice(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
    }
    buf
}

/// 音色目录(中文行,策展自 robot 实测音色表;目录 = 数据,加语言 = 加一组)。
#[derive(Debug, Clone, serde::Serialize)]
pub struct Speaker {
    pub id: &'static str,
    pub name: &'static str,
}

pub const SPEAKERS_ZH: &[Speaker] = &[
    Speaker { id: "zh-CN-XiaoxiaoNeural", name: "晓晓 · 温柔" },
    Speaker { id: "zh-CN-XiaoyiNeural", name: "晓伊 · 可爱" },
    Speaker { id: "zh-CN-XiaohanNeural", name: "晓涵 · 讲故事" },
    Speaker { id: "zh-CN-XiaochenNeural", name: "晓辰 · 活泼" },
    Speaker { id: "zh-CN-YunxiNeural", name: "云希 · 少年" },
    Speaker { id: "zh-CN-YunjianNeural", name: "云健 · 沉稳" },
];

pub const DEFAULT_SPEAKER: &str = "zh-CN-XiaoxiaoNeural";

/// 语速三档(voice.rate,user 级)→ edge 语速偏移百分比。
pub fn rate_pct(rate: &str) -> i32 {
    match rate {
        "slow" => -15,
        "fast" => 15,
        _ => 0, // standard
    }
}

/// 缓存键:音色|语速|文本 的 SHA-256(同句换音色 = 另一份缓存,语义正确)。
pub fn cache_key(voice: &str, rate_pct: i32, text: &str) -> String {
    let mut h = sha2::Sha256::new();
    h.update(voice.as_bytes());
    h.update(b"|");
    h.update(rate_pct.to_le_bytes());
    h.update(b"|");
    h.update(text.as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_key_varies_by_voice_rate_text() {
        let a = cache_key("v1", 0, "你好");
        assert_eq!(a, cache_key("v1", 0, "你好"), "同参定键");
        assert_ne!(a, cache_key("v2", 0, "你好"));
        assert_ne!(a, cache_key("v1", 15, "你好"));
        assert_ne!(a, cache_key("v1", 0, "你好呀"));
    }

    #[test]
    fn rate_tiers_are_locked() {
        assert_eq!(rate_pct("slow"), -15);
        assert_eq!(rate_pct("standard"), 0);
        assert_eq!(rate_pct("fast"), 15);
        assert_eq!(rate_pct("junk"), 0);
    }

    #[test]
    fn speaker_catalog_has_default() {
        assert!(SPEAKERS_ZH.iter().any(|s| s.id == DEFAULT_SPEAKER));
    }

    #[test]
    fn wav_header_is_well_formed() {
        let pcm = vec![0.0f32, 0.5, -0.5, 1.0, -1.0];
        let wav = pcm_f32_to_wav(&pcm, 24_000);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(wav.len(), 44 + pcm.len() * 2, "44 头 + i16 数据");
        // 数据区第一个样本 0.0 → 0i16
        assert_eq!(i16::from_le_bytes([wav[44], wav[45]]), 0);
        // 削顶:1.0 → 32767,-1.0 → -32767
        let last = wav.len() - 2;
        assert_eq!(i16::from_le_bytes([wav[last], wav[last + 1]]), -32767);
    }
}
