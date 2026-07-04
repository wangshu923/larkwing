//! 声纹识别(PLAN §11 D):CAM++ 提取 192 维 embedding;注册库现读现算余弦。
//! robot 同款立场:不强制识别成家庭成员——访客/电视声达不到阈值就 fallback,
//! 绝不误归类。注册/识别都在 voice(它有 PCM + store),出 user_id 给 send 链。

use std::path::Path;

use anyhow::{anyhow, Result};

/// 余弦相似度阈值(robot 默认):同人 >0.7,不同人 <0.4,0.5 是稳妥分界。
pub const THRESHOLD: f32 = 0.5;
/// 「明显高于第二名」的差值门槛。最高分与第二名之差 < 此值 = 两个家人咬得太近 →
/// 判「认不出」(回落会话主人),避免把记忆写到错的人名下。只有一个人注册时无第二名,过阈即可。
/// 2026-07-04 用户拍板「均衡:0.5 阈值 + 0.10 差值」(§4.2「宁可不认、绝不错认」+ §4.11)。
pub const MARGIN: f32 = 0.10;

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

    /// 识别:embedding 与注册库逐个余弦 → `pick` 按「过阈 + 拉得开第二名」裁决(宁可不认)。
    /// 命中返回 user_id;含糊/不达阈返回 None(fallback 会话用户)。
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
        let mut scored: Vec<(i64, f32)> =
            library.iter().map(|(uid, r)| (*uid, cosine(&emb, r))).collect();
        let id = pick(&mut scored);
        match id {
            Some(uid) => tracing::info!(user = uid, scores = ?scored, "声纹命中"),
            None => tracing::debug!(scores = ?scored, "声纹未达置信(过阈/差值不足),归会话用户"),
        }
        id
    }
}

/// 置信裁决(§4.2「宁可不认、绝不错认」):最高分 ≥ THRESHOLD **且**比第二名高出 ≥ MARGIN
/// 才认;否则 None。就一个人注册时无第二名,过阈即可。就地按分降序排(调用方只用于日志)。
fn pick(scored: &mut [(i64, f32)]) -> Option<i64> {
    scored.sort_by(|a, b| b.1.total_cmp(&a.1));
    let (best_id, best) = *scored.first()?;
    if best < THRESHOLD {
        return None;
    }
    if let Some(&(_, second)) = scored.get(1) {
        if best - second < MARGIN {
            return None; // 两人咬太近 → 认不出
        }
    }
    Some(best_id)
}

/// 多段注册取平均(3 段更稳,§4.2「绝不错认」的可靠度杠杆):逐维求均值。空 → None
/// (调用方保证非空);维度不一致按最短截断(防脏数据 panic)。
pub fn mean_embedding(embeds: &[Vec<f32>]) -> Option<Vec<f32>> {
    let dim = embeds.iter().map(|e| e.len()).min()?;
    if dim == 0 {
        return None;
    }
    let mut acc = vec![0f32; dim];
    for e in embeds {
        for (a, x) in acc.iter_mut().zip(e.iter()) {
            *a += *x;
        }
    }
    let n = embeds.len() as f32;
    for a in &mut acc {
        *a /= n;
    }
    Some(acc)
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
    fn pick_requires_threshold_and_margin() {
        // 单人:过阈即认,低于阈值不认(无第二名不查差值)
        assert_eq!(pick(&mut [(1, 0.62)]), Some(1));
        assert_eq!(pick(&mut [(1, 0.40)]), None, "低于阈值不认");
        // 两人拉得开(差 ≥ 0.10):认最高
        assert_eq!(pick(&mut [(1, 0.72), (2, 0.55)]), Some(1));
        assert_eq!(pick(&mut [(2, 0.85), (1, 0.70)]), Some(2), "顺序无关,按分裁决");
        // 两人咬太近(差 < 0.10):宁可不认(归会话用户)
        assert_eq!(pick(&mut [(1, 0.62), (2, 0.60)]), None);
        // 第二名也过阈但差够 → 仍认最高
        assert_eq!(pick(&mut [(1, 0.90), (2, 0.60)]), Some(1));
        assert_eq!(pick(&mut []), None);
    }

    #[test]
    fn mean_embedding_averages_and_handles_edges() {
        let m = mean_embedding(&[vec![1.0, 0.0], vec![0.0, 1.0]]).unwrap();
        assert!((m[0] - 0.5).abs() < 1e-6 && (m[1] - 0.5).abs() < 1e-6);
        assert_eq!(mean_embedding(&[]), None, "空 = None");
        // 维度不一致按最短截断,不 panic
        let m2 = mean_embedding(&[vec![2.0, 4.0], vec![4.0]]).unwrap();
        assert_eq!(m2.len(), 1);
        assert!((m2[0] - 3.0).abs() < 1e-6);
    }
}
