//! `Asr` trait + sherpa-onnx 实现(PLAN §11)。引擎可换是接缝承诺;默认 SenseVoice(快),
//! 用户可在设置切 Whisper-small(对孩子/口音更稳)/ FireRedASR2-CTC(中文最准)——换的是
//! models.rs 的数据 + 这里加一个构造分支(架构不同),trait 面与 `transcribe` 不动(AGENT §7.5)。

use std::path::Path;

use anyhow::{Context, Result};

use super::models::AsrModel;

pub trait Asr: Send + Sync {
    /// 整段识别:16kHz mono f32([-1,1])进,文本出(空串 = 没识别出话)。
    fn transcribe(&self, samples_16k: &[f32]) -> Result<String>;
}

pub struct SherpaAsr {
    rec: sherpa_onnx::OfflineRecognizer,
}

impl SherpaAsr {
    /// 按选中的档加载对应架构(文件名是各模型固定的,与 models.rs 的 spec 文件清单对齐)。
    pub fn load(model: AsrModel, model_dir: &Path, lang: &str) -> Result<SherpaAsr> {
        match model {
            AsrModel::SenseVoice => Self::sense_voice(model_dir, lang),
            // tiny / small / medium 同 Whisper 架构,只差文件名前缀(与 models.rs spec 对齐)。
            AsrModel::WhisperTiny => Self::whisper(model_dir, lang, "tiny"),
            AsrModel::WhisperSmall => Self::whisper(model_dir, lang, "small"),
            AsrModel::WhisperMedium => Self::whisper(model_dir, lang, "medium"),
            AsrModel::FireRedCtc => Self::fire_red_ctc(model_dir),
        }
    }

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

    /// Whisper 形(多语 tiny/small/medium):encoder/decoder 在 `whisper` 子结构,tokens 在
    /// model_config 顶层;文件名前缀按档(`{size}-encoder.int8.onnx` 等,与 models.rs spec 对齐)。
    /// language 指定中文(避免自动语言检测的歧义);task 留默认 = transcribe(转写,不翻译)。
    /// `num_threads` 上调 = Whisper autoregressive 吃 CPU,多线程缓解延迟(真机调优项,AGENT §8.1)。
    pub fn whisper(model_dir: &Path, lang: &str, size: &str) -> Result<SherpaAsr> {
        let mut cfg = sherpa_onnx::OfflineRecognizerConfig::default();
        cfg.model_config.whisper.encoder =
            Some(model_dir.join(format!("{size}-encoder.int8.onnx")).to_string_lossy().into_owned());
        cfg.model_config.whisper.decoder =
            Some(model_dir.join(format!("{size}-decoder.int8.onnx")).to_string_lossy().into_owned());
        cfg.model_config.whisper.language = Some(lang.to_string());
        cfg.model_config.tokens =
            Some(model_dir.join(format!("{size}-tokens.txt")).to_string_lossy().into_owned());
        cfg.model_config.num_threads = 4;
        let t0 = std::time::Instant::now();
        let rec = sherpa_onnx::OfflineRecognizer::create(&cfg).context("ASR 模型加载失败")?;
        tracing::info!(ms = t0.elapsed().as_millis() as u64, size, "ASR 模型加载完成(Whisper)");
        Ok(SherpaAsr { rec })
    }

    /// FireRedASR2-CTC 形:单 CTC `model` 文件 + tokens(均在 model_config 顶层);CPU 上快。
    pub fn fire_red_ctc(model_dir: &Path) -> Result<SherpaAsr> {
        let mut cfg = sherpa_onnx::OfflineRecognizerConfig::default();
        cfg.model_config.fire_red_asr_ctc.model =
            Some(model_dir.join("model.int8.onnx").to_string_lossy().into_owned());
        cfg.model_config.tokens = Some(model_dir.join("tokens.txt").to_string_lossy().into_owned());
        cfg.model_config.num_threads = 4;
        let t0 = std::time::Instant::now();
        let rec = sherpa_onnx::OfflineRecognizer::create(&cfg).context("ASR 模型加载失败")?;
        tracing::info!(ms = t0.elapsed().as_millis() as u64, "ASR 模型加载完成(FireRed-CTC)");
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
