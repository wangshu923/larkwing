//! 声纹识别(PLAN §11 D):CAM++ 提取 192 维 embedding;注册库现读现算余弦。
//! robot 同款立场:不强制识别成家庭成员——访客/电视声达不到阈值就 fallback,
//! 绝不误归类。注册/识别都在 voice(它有 PCM + store),出 user_id 给 send 链。

use std::path::Path;

use anyhow::{anyhow, Result};

/// 余弦相似度阈值(robot 默认):同人 >0.7,不同人 <0.4,0.5 是稳妥分界。
pub const THRESHOLD: f32 = 0.5;

pub struct SpeakerId {
    extractor: sherpa_onnx::SpeakerEmbeddingExtractor,
}

impl SpeakerId {
    pub fn load(model: &Path) -> Result<SpeakerId> {
        let mut cfg = sherpa_onnx::SpeakerEmbeddingExtractorConfig::default();
        cfg.model = Some(model.to_string_lossy().into_owned());
        let t0 = std::time::Instant::now();
        let extractor = sherpa_onnx::SpeakerEmbeddingExtractor::create(&cfg)
            .ok_or_else(|| anyhow!("声纹模型加载失败"))?;
        tracing::info!(
            ms = t0.elapsed().as_millis() as u64,
            dim = extractor.dim(),
            "声纹模型加载完成(CAM++)"
        );
        Ok(SpeakerId { extractor })
    }

    /// 16kHz mono f32 → 192 维 embedding(注册与识别共用)。
    pub fn embed(&self, samples_16k: &[f32]) -> Result<Vec<f32>> {
        let stream = self.extractor.create_stream().ok_or_else(|| anyhow!("声纹流创建失败"))?;
        stream.accept_waveform(16_000, samples_16k);
        stream.input_finished();
        if !self.extractor.is_ready(&stream) {
            return Err(anyhow!("音频太短,提不出声纹(至少说一两秒)"));
        }
        self.extractor.compute(&stream).ok_or_else(|| anyhow!("声纹计算失败"))
    }

    /// 识别:embedding 与注册库逐个余弦,最高分 > 阈值 → user_id;否则 None(fallback)。
    pub fn identify(&self, samples_16k: &[f32], library: &[(i64, Vec<f32>)]) -> Option<i64> {
        if library.is_empty() {
            return None;
        }
        let emb = match self.embed(samples_16k) {
            Ok(e) => e,
            Err(e) => {
                tracing::debug!(err = %format!("{e:#}"), "声纹提取失败,跳过识别");
                return None;
            }
        };
        let (mut best_id, mut best_score) = (None, THRESHOLD);
        for (uid, ref_emb) in library {
            let score = cosine(&emb, ref_emb);
            if score > best_score {
                best_score = score;
                best_id = Some(*uid);
            }
        }
        if let Some(id) = best_id {
            tracing::info!(user = id, score = best_score, "声纹命中");
        }
        best_id
    }
}

/// 余弦相似度(维度不等返回 0,防脏数据 panic)。
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0f32;
    let mut na = 0f32;
    let mut nb = 0f32;
    for i in 0..a.len() {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    let denom = na.sqrt() * nb.sqrt();
    if denom == 0.0 {
        0.0
    } else {
        dot / denom
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_basics() {
        assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6, "同向 = 1");
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6, "正交 = 0");
        assert!((cosine(&[1.0, 2.0, 3.0], &[2.0, 4.0, 6.0]) - 1.0).abs() < 1e-6, "同向不同模 = 1");
        assert_eq!(cosine(&[1.0], &[1.0, 2.0]), 0.0, "维度不等 = 0(不 panic)");
        assert_eq!(cosine(&[], &[]), 0.0);
    }

    #[test]
    fn identify_picks_best_above_threshold() {
        // 不依赖真模型:直接喂 identify 的 library + 用 cosine 验证选择逻辑
        let alice = vec![1.0f32, 0.0, 0.0];
        let bob = vec![0.0f32, 1.0, 0.0];
        let lib = vec![(1i64, alice.clone()), (2i64, bob.clone())];
        // 模拟:query ≈ alice → 应选 1;但 identify 内部要提取,这里只能验 cosine 选择
        // (embed 依赖真模型,集成测留真机)。最近向量:
        let q = vec![0.9f32, 0.1, 0.0];
        let best = lib
            .iter()
            .filter(|(_, e)| cosine(&q, e) > THRESHOLD)
            .max_by(|a, b| cosine(&q, &a.1).total_cmp(&cosine(&q, &b.1)))
            .map(|(id, _)| *id);
        assert_eq!(best, Some(1));
    }
}
