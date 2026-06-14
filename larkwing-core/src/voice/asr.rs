//! `Asr` trait + sherpa-onnx 实现(PLAN §11)。引擎可换是接缝承诺;当前目录默认
//! SenseVoice(中文行起点,A 期实测三选一后可能换 FireRed/Paraformer——换的是
//! models.rs 的数据 + 这里加一个构造分支,trait 面不动)。

use std::path::Path;

use anyhow::{Context, Result};

pub trait Asr: Send + Sync {
    /// 整段识别:16kHz mono f32([-1,1])进,文本出(空串 = 没识别出话)。
    fn transcribe(&self, samples_16k: &[f32]) -> Result<String>;
}

pub struct SherpaAsr {
    rec: sherpa_onnx::OfflineRecognizer,
}

impl SherpaAsr {
    /// SenseVoice 形:model_dir 含 `model.int8.onnx` + `tokens.txt`。
    /// lang 来自语言目录(zh);use_itn 开(数字/时间转书写形,屏幕显示更顺眼)。
    pub fn sense_voice(model_dir: &Path, lang: &str) -> Result<SherpaAsr> {
        let mut cfg = sherpa_onnx::OfflineRecognizerConfig::default();
        cfg.model_config.sense_voice.model =
            Some(model_dir.join("model.int8.onnx").to_string_lossy().into_owned());
        cfg.model_config.sense_voice.language = Some(lang.to_string());
        cfg.model_config.sense_voice.use_itn = true;
        cfg.model_config.tokens = Some(model_dir.join("tokens.txt").to_string_lossy().into_owned());
        let t0 = std::time::Instant::now();
        let rec = sherpa_onnx::OfflineRecognizer::create(&cfg).context("ASR 模型加载失败")?;
        tracing::info!(ms = t0.elapsed().as_millis() as u64, "ASR 模型加载完成(SenseVoice)");
        Ok(SherpaAsr { rec })
    }
}

impl Asr for SherpaAsr {
    fn transcribe(&self, samples_16k: &[f32]) -> Result<String> {
        let t0 = std::time::Instant::now();
        let stream = self.rec.create_stream();
        stream.accept_waveform(16_000, samples_16k);
        self.rec.decode(&stream);
        let text = stream
            .get_result()
            .map(|r| r.text.trim().to_string())
            .unwrap_or_default();
        tracing::info!(
            ms = t0.elapsed().as_millis() as u64,
            audio_ms = (samples_16k.len() as u64) * 1000 / 16_000,
            chars = text.chars().count(),
            "ASR 识别完成"
        );
        Ok(text)
    }
}
