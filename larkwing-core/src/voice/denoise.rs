//! 参考音的**清理(去噪)与体检(测量)**——克隆录入链专用(PLAN §11 D-clone)。
//!
//! 由来(2026-08-21 真机「克隆音色播放莎莎声很严重」追因):零样本克隆把「录音条件」
//! 当音色一起学走 —— 参考音底噪 −49dB,合成输出底噪就是 −48~−50dB(实测一比一跟随),
//! 安静间隙里就是明显的沙沙。故治法在**参考音**(一次性、离线、录入时做),不在输出侧
//! (每次合成都要处理,还治不了模型照着脏参考幻觉出的伪影)。
//!
//! 两件事、两种性质:
//! - **去噪 = GTCRN 神经模型**(sherpa-onnx 同一原生依赖,§7.5「推理统一 sherpa-onnx」;
//!   模型 = 数据,用时下载 §6.9)。**推理期零旋钮**:换会议室/客厅/厨房都是同一个模型、
//!   同一套零配置,不像谱减法要按环境调参(那条路已实验否决:尾音处「音乐噪声」伪影)。
//! - **体检 = 绝对量测量**(纯函数,可测):削波程度、底噪水平。同样与环境无关 ——
//!   量的是「这段录音本身好不好」,不是「这个房间该怎么滤」。
//!
//! 阈值 = 产品决策(§4.11,2026-08-21 用户拍板),单源在本文件顶部。

use std::path::Path;

use anyhow::{anyhow, Result};

/// 去噪后底噪仍高于此值 → 判「环境太吵」(§4.11 用户拍板 −55 dB:落在实测「明显有莎莎」
/// 的 −49 与「用户耳测认可」的 −60 之间;更严会让普通家庭客厅反复被要求重录)。
pub const NOISE_FLOOR_MAX_DB: f32 = -55.0;

/// 近满幅样本占比超过此值 → 判「输入侧削波」(§4.11 用户拍板万分之一:8 秒 16k 录音里
/// 约 13 个满幅样本;既抓真的爆音失真,又不为一两下偶发杂音烦用户)。
pub const CLIP_MAX_RATIO: f32 = 1.0e-4;

/// 「近满幅」判据:留一丝余量(我们自己的归一封顶在 0.99,故量到的一定来自输入侧)。
const CLIP_LEVEL: f32 = 0.995;

/// 底噪估计的分析帧长(ms)与取的分位:20ms 帧 RMS 的 10 分位 = 安静帧的水平,
/// 不被语音帧拉高(整段 RMS 会把说话声算进去,量不出「间隙有多干净」)。
const FRAME_MS: usize = 20;
const FLOOR_PERCENTILE: f32 = 0.10;

/// 静音下限:全零/极短音频返回它,避免 log10(0) = −inf 过桥变 JSON null。
const FLOOR_MIN_DB: f32 = -120.0;

/// 参考音体检结果(过桥给前端:core 只给数据 + issue **key**,文案在前端字典 §6.6)。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RefAudioCheck {
    /// 去噪后的底噪(dBFS,越小越干净)。
    pub noise_floor_db: f32,
    /// 归一**前**测得的近满幅样本占比(输入侧削波信号)。
    pub clip_ratio: f32,
    /// 是否真跑了神经去噪(模型没下下来 = false,如实告知不静默 §3.5)。
    pub denoised: bool,
    /// 体检结论 key:`None` = 没问题;`"clipped"` / `"noisy"` / `"noDenoise"`。
    pub issue: Option<&'static str>,
}

impl RefAudioCheck {
    /// 判结论:削波优先报(失真已烤进波形、去噪救不了),其次底噪,最后「没能去噪」。
    /// 一次只报一条 —— 给用户一个明确可执行的动作,不堆诊断(§3 收敛)。
    pub fn verdict(noise_floor_db: f32, clip_ratio: f32, denoised: bool) -> RefAudioCheck {
        let issue = if clip_ratio > CLIP_MAX_RATIO {
            Some("clipped")
        } else if noise_floor_db > NOISE_FLOOR_MAX_DB {
            Some("noisy")
        } else if !denoised {
            Some("noDenoise")
        } else {
            None
        };
        RefAudioCheck { noise_floor_db, clip_ratio, denoised, issue }
    }
}

/// 近满幅样本占比 —— 输入侧削波的信号。**必须在归一之前量**:归一后峰值恒被推到目标
/// 附近,量不出输入侧本来削没削(而我们自己的归一按真峰封顶、恒不产生削波,见
/// `voice::peak_normalize`)。空音频 = 0。
pub fn clip_ratio(pcm: &[f32]) -> f32 {
    if pcm.is_empty() {
        return 0.0;
    }
    let hot = pcm.iter().filter(|s| s.abs() >= CLIP_LEVEL).count();
    hot as f32 / pcm.len() as f32
}

/// 底噪(dBFS):20ms 帧 RMS 的 10 分位。取安静帧的水平 = 「说话间隙有多干净」,
/// 这正是听感上「莎莎声」的来源;整段 RMS 做不到(被语音拉高)。
pub fn noise_floor_db(pcm: &[f32], rate: u32) -> f32 {
    if pcm.is_empty() || rate == 0 {
        return FLOOR_MIN_DB;
    }
    let frame = (rate as usize * FRAME_MS / 1000).max(1);
    let mut rms: Vec<f32> = pcm
        .chunks(frame)
        .filter(|c| c.len() == frame) // 末尾残帧不参与(样本少、RMS 不可比)
        .map(|c| (c.iter().map(|s| s * s).sum::<f32>() / c.len() as f32).sqrt())
        .collect();
    if rms.is_empty() {
        // 不足一帧:退化成整段 RMS(短录音本就会被「至少 3 秒」挡在前面)
        let all = (pcm.iter().map(|s| s * s).sum::<f32>() / pcm.len() as f32).sqrt();
        return db(all);
    }
    let idx = ((rms.len() as f32 * FLOOR_PERCENTILE) as usize).min(rms.len() - 1);
    let (_, floor, _) = rms.select_nth_unstable_by(idx, |a, b| a.total_cmp(b));
    db(*floor)
}

fn db(amp: f32) -> f32 {
    if amp <= 1.0e-9 {
        FLOOR_MIN_DB
    } else {
        (20.0 * amp.log10()).max(FLOOR_MIN_DB)
    }
}

/// 去噪并**保持采样率不变**:成功且采样率没变才采纳,返回是否真降噪了。
///
/// 为什么要这道闸:下游三处都按同一个采样率走 —— ASR 吃 16k、存盘 wav 头写的是这个数、
/// 克隆合成读参考音也按它。模型若换了采样率(GTCRN 是 16k 原生、实测同进同出,但那是
/// **实测不是契约**)而我们照旧按老数字写头,音频就变调 —— 那种静默损坏比不降噪糟得多,
/// 故**宁可放弃这次去噪**。`run` 收闭包 = 三个分支(没有模型 / 出错 / 换了采样率)都可测。
pub fn apply_denoise<F>(pcm: &mut Vec<f32>, rate: u32, run: Option<F>) -> bool
where
    F: FnOnce(&[f32], u32) -> Result<(Vec<f32>, u32)>,
{
    let Some(run) = run else { return false };
    match run(pcm, rate) {
        Ok((clean, out_rate)) if out_rate == rate => {
            *pcm = clean;
            true
        }
        Ok((_, out_rate)) => {
            tracing::warn!(out_rate, rate, "去噪换了采样率,放弃本次降噪(避免变调)");
            false
        }
        Err(e) => {
            tracing::warn!(err = %e, "参考音去噪失败,用原始录音");
            false
        }
    }
}

/// GTCRN 去噪器(app 级资产:模型加载一次、录入时反复用;沿 sherpa 对象单线程安全语义,
/// 上游已 impl Send+Sync → 可进 Arc 缓存)。
pub struct Denoiser {
    inner: sherpa_onnx::OfflineSpeechDenoiser,
}

impl Denoiser {
    /// 加载模型目录里的 gtcrn onnx(文件名与 `models::DENOISE_GTCRN` 单源对齐)。
    pub fn load(model_dir: &Path) -> Result<Denoiser> {
        let model = model_dir.join(super::models::DENOISE_GTCRN.files[0].name);
        let cfg = sherpa_onnx::OfflineSpeechDenoiserConfig {
            model: sherpa_onnx::OfflineSpeechDenoiserModelConfig {
                gtcrn: sherpa_onnx::OfflineSpeechDenoiserGtcrnModelConfig {
                    model: Some(model.to_string_lossy().to_string()),
                },
                num_threads: 2,
                ..Default::default()
            },
        };
        let inner = sherpa_onnx::OfflineSpeechDenoiser::create(&cfg)
            .ok_or_else(|| anyhow!("语音去噪模型加载失败:{}", model.display()))?;
        Ok(Denoiser { inner })
    }

    /// 去噪一整段(离线、零旋钮)。返回 (样本, 采样率) —— 模型有自己的工作采样率,
    /// 原样带回给调用方落盘/后续处理,别假设与入参相同。
    pub fn run(&self, pcm: &[f32], rate: u32) -> Result<(Vec<f32>, u32)> {
        let out = self.inner.run(pcm, rate as i32);
        anyhow::ensure!(!out.samples.is_empty(), "语音去噪返回了空音频");
        let out_rate = if out.sample_rate > 0 { out.sample_rate as u32 } else { rate };
        Ok((out.samples, out_rate))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 削波占比:必须在归一前量,数值 = 近满幅样本 / 总样本。
    #[test]
    fn clip_ratio_counts_near_full_scale() {
        let mut pcm = vec![0.2f32; 1000];
        for i in 0..5 {
            pcm[i * 100] = 1.0;
        }
        assert!((clip_ratio(&pcm) - 0.005).abs() < 1.0e-6, "5/1000");
        assert_eq!(clip_ratio(&vec![0.5f32; 100]), 0.0, "正常电平不算削波");
        assert_eq!(clip_ratio(&[]), 0.0, "空音频不炸");
        // 阈值语义:8 秒 16k 录音里 13 个满幅样本 ≈ 刚过万分之一
        let mut long = vec![0.3f32; 128_000];
        for i in 0..13 {
            long[i * 1000] = 0.999;
        }
        assert!(clip_ratio(&long) > CLIP_MAX_RATIO, "13/128000 该判削波");
    }

    /// 底噪要量「安静帧」的水平,不能被语音帧拉高 —— 这正是听感莎莎声的来源。
    #[test]
    fn noise_floor_measures_quiet_frames_not_speech() {
        let rate = 16_000u32;
        let frame = (rate as usize) * FRAME_MS / 1000; // 320
        let mut pcm = Vec::new();
        for i in 0..50 {
            // 半数语音帧(响)、半数间隙帧(底噪 0.001 = −60dB)
            let amp = if i % 2 == 0 { 0.3 } else { 0.001 };
            pcm.extend(std::iter::repeat(amp).take(frame));
        }
        let floor = noise_floor_db(&pcm, rate);
        assert!((floor - -60.0).abs() < 1.0, "该量到间隙的 −60dB,实测 {floor}");
        // 全静音不返回 -inf(过桥会变 JSON null)
        assert_eq!(noise_floor_db(&vec![0.0f32; frame * 3], rate), FLOOR_MIN_DB);
        assert_eq!(noise_floor_db(&[], rate), FLOOR_MIN_DB, "空音频不炸");
    }

    /// 采样率闸:没模型 / 出错 / 换了采样率 → 一律不采纳(保原样),只有同采样率的成功结果才用。
    /// 换采样率却照旧按老数字写 wav 头 = 变调,静默损坏比不降噪糟得多。
    #[test]
    fn apply_denoise_only_accepts_same_rate_success() {
        let orig = vec![0.3f32; 100];

        // 没有模型:原样不动
        let mut pcm = orig.clone();
        let none: Option<fn(&[f32], u32) -> Result<(Vec<f32>, u32)>> = None;
        assert!(!apply_denoise(&mut pcm, 16_000, none));
        assert_eq!(pcm, orig);

        // 成功且同采样率:采纳
        let mut pcm = orig.clone();
        assert!(apply_denoise(&mut pcm, 16_000, Some(|_: &[f32], r: u32| Ok((vec![0.1; 100], r)))));
        assert_eq!(pcm, vec![0.1f32; 100]);

        // 换了采样率:放弃(否则下游按老采样率写头 → 变调)
        let mut pcm = orig.clone();
        assert!(!apply_denoise(&mut pcm, 16_000, Some(|_: &[f32], _| Ok((vec![0.1; 50], 8_000)))));
        assert_eq!(pcm, orig, "换采样率必须放弃、保原样");

        // 出错:放弃
        let mut pcm = orig.clone();
        assert!(!apply_denoise(&mut pcm, 16_000, Some(|_: &[f32], _| Err(anyhow!("炸了")))));
        assert_eq!(pcm, orig);
    }

    /// 结论优先级:削波 > 底噪 > 没能去噪;都好 = 没结论(不打扰)。
    #[test]
    fn verdict_prioritizes_clipping_then_noise() {
        assert_eq!(RefAudioCheck::verdict(-70.0, 0.0, true).issue, None, "干净 = 不打扰");
        assert_eq!(RefAudioCheck::verdict(-49.0, 0.0, true).issue, Some("noisy"));
        assert_eq!(
            RefAudioCheck::verdict(-49.0, 0.01, true).issue,
            Some("clipped"),
            "削波优先(去噪救不了、且用户能直接改)"
        );
        assert_eq!(
            RefAudioCheck::verdict(-70.0, 0.0, false).issue,
            Some("noDenoise"),
            "模型没下下来要如实说(§3.5)"
        );
        // 阈值边界:恰在阈值上不报(> 才报)
        assert_eq!(RefAudioCheck::verdict(NOISE_FLOOR_MAX_DB, CLIP_MAX_RATIO, true).issue, None);
    }
}
