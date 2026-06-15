//! 唤醒录音标定(PLAN §11 后续):录几遍唤醒词 → 一次扫描同时定「拼写(B)」和「阈值(A)」。
//!
//! 设计要点(见会话决策):
//! - sherpa KWS **不暴露分数**,只给命中/不命中 → 标定 = 二值阈值扫描(非读连续分)。
//! - **拼写轴(B)**:canonical 读音 + `to_pinyin_multi` 异读,经模型词表 split_syllable 编码、
//!   去重;只采纳「比 canonical 在更严阈值上仍稳触发」的异读(≥1 档),否则留 canonical。
//! - **阈值轴(A)**:对每个阈值建一个含全部候选拼写(@v{i} 标签)的 spotter,喂正/负样本,
//!   得 recall[拼写][阈值] 与 false-accept[拼写][阈值];**偏召回**选阈(用户痛点是「叫不应」):
//!   取「负样本不误触的最宽松档」上方一档作余量,封顶在召回悬崖。
//! - 写回:阈值经 `threshold_to_sensitivity` 落到既有 `voice.wake.sensitivity`(滑块随之更新);
//!   非 canonical 拼写落到 `voice.wake.spelling`(词→token 行覆盖)。
//! - **忠于生产**:喂 spotter 的是 RAW 16k(生产 wake 循环也喂 raw,未过 AGC);100ms 块 + 0.5s
//!   静音尾 flush,与 examples/kws_replay 同配方。

use std::collections::HashSet;
use std::path::Path;

use anyhow::{anyhow, bail, Result};
use pinyin::{ToPinyin, ToPinyinMulti};

use super::wake;

/// 阈值扫描格(生产范围 [0.1,0.5];真声「旺财」分多落 ~0.3,故密布决策边界)。
pub(super) const GRID: [f32; 9] = [0.10, 0.15, 0.20, 0.25, 0.30, 0.35, 0.40, 0.45, 0.50];
/// 召回达标线(n 条正样本里至少这个比例要触发)。
const TARGET_RECALL: f32 = 0.8;
/// 采纳异读拼写的门槛:它的「召回悬崖」要比 canonical 高至少这么多档,才值得换(防抖动)。
const ADOPT_MARGIN: i32 = 1;
/// 候选拼写上限(canonical + 异读;2 字唤醒词通常 1~4 个,封顶防多音字组合爆炸)。
const MAX_VARIANTS: usize = 6;

/// 一个候选拼写 = 一串模型 token(如 "x iǎo q ī")+ 是否为规范读音。
#[derive(Debug, Clone)]
pub(super) struct Variant {
    pub tokens: String,
    pub canonical: bool,
}

/// 标定产出:落到设置的灵敏度 + 可选拼写覆盖行 + 该档召回 + 结论(key,非文案)。
#[derive(Debug, Clone)]
pub(super) struct CalibOutcome {
    pub sensitivity: u32,
    /// 非 canonical 被采纳时 = "tok… @词"整行;否则 None(用 canonical 编码即可)。
    pub spelling: Option<String>,
    pub recall: f32,
    /// "good" | "noisy" | "hard"(前端字典渲染文案)。
    pub verdict: &'static str,
}

// ---- 灵敏度 ↔ 阈值(分段映射;mod.rs wake_threshold 走 sensitivity_to_threshold) ----

/// 灵敏度(0~100)→ KWS 阈值。中点 50=0.2;灵敏半区 [50,100]→[0.2,0.1],稳重半区 [0,50]→[0.5,0.2]。
pub(super) fn sensitivity_to_threshold(sens: f32) -> f32 {
    let sens = sens.clamp(0.0, 100.0);
    let thr = if sens >= 50.0 {
        0.2 - (sens - 50.0) / 50.0 * 0.1
    } else {
        0.5 - sens / 50.0 * 0.3
    };
    thr.clamp(0.1, 0.5)
}

/// 阈值 → 灵敏度(上式的逆),四舍五入到 5 的整数倍(对齐滑块步进),clamp [0,100]。
pub(super) fn threshold_to_sensitivity(thr: f32) -> u32 {
    let thr = thr.clamp(0.1, 0.5);
    let sens = if thr <= 0.2 {
        50.0 + (0.2 - thr) / 0.1 * 50.0
    } else {
        (0.5 - thr) / 0.3 * 50.0
    };
    ((sens / 5.0).round() * 5.0).clamp(0.0, 100.0) as u32
}

// ---- 拼写轴(B):候选生成 ----

/// 一个唤醒词的候选拼写集:canonical(各字规范读音)在前,加 `to_pinyin_multi` 异读的笛卡尔
/// 组合;每个组合经词表 split_syllable 编码,不可编码的丢弃,按 token 串去重,封顶 MAX_VARIANTS。
/// 非中文词或一个都编不出 → 空(调用方应回落/报错)。
pub(super) fn candidate_spellings(word: &str, vocab: &HashSet<String>) -> Vec<Variant> {
    let chars: Vec<char> = word.chars().collect();
    if chars.is_empty() {
        return Vec::new();
    }
    // 每个字的读音(canonical 在 [0],其余异读去重);任一字无拼音 → 整词无候选
    let mut per_char: Vec<Vec<String>> = Vec::with_capacity(chars.len());
    for &c in &chars {
        let mut readings: Vec<String> = Vec::new();
        if let Some(p) = c.to_pinyin() {
            readings.push(p.with_tone().to_string());
        }
        if let Some(multi) = c.to_pinyin_multi() {
            for p in multi {
                let s = p.with_tone().to_string();
                if !readings.contains(&s) {
                    readings.push(s);
                }
            }
        }
        if readings.is_empty() {
            return Vec::new();
        }
        per_char.push(readings);
    }
    // 笛卡尔积(组合数封顶);组合 [0] = 各字 readings[0] = canonical
    let mut combos: Vec<Vec<String>> = vec![Vec::new()];
    for choices in &per_char {
        let mut next = Vec::new();
        for prefix in &combos {
            for ch in choices {
                let mut v = prefix.clone();
                v.push(ch.clone());
                next.push(v);
            }
        }
        combos = next;
        if combos.len() > 64 {
            combos.truncate(64);
        }
    }
    // 编码 + 去重(按 token 串)
    let mut variants: Vec<Variant> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for combo in &combos {
        let is_canonical = combo.iter().enumerate().all(|(i, s)| s == &per_char[i][0]);
        let mut tokens: Vec<String> = Vec::new();
        let mut ok = true;
        for syll in combo {
            match wake::split_syllable(syll, vocab) {
                Some(mut t) => tokens.append(&mut t),
                None => {
                    ok = false;
                    break;
                }
            }
        }
        if !ok || tokens.is_empty() {
            continue;
        }
        let tok_str = tokens.join(" ");
        if seen.insert(tok_str.clone()) {
            variants.push(Variant { tokens: tok_str, canonical: is_canonical });
        } else if is_canonical {
            // canonical 与某异读编出同一串 token → 把那条标成 canonical
            if let Some(v) = variants.iter_mut().find(|v| v.tokens == tok_str) {
                v.canonical = true;
            }
        }
        if variants.len() >= MAX_VARIANTS {
            break;
        }
    }
    // canonical 排前(稳定 tie-break)
    variants.sort_by_key(|v| !v.canonical);
    variants
}

// ---- 阈值轴(A):扫描 ----

/// 喂一段音频(100ms 块 + 0.5s 静音尾 flush),返回命中的 @标签集合(如 {"v0","v2"})。
fn detect(spotter: &sherpa_onnx::KeywordSpotter, samples: &[f32]) -> HashSet<String> {
    let rate = super::TARGET_RATE as i32;
    let chunk = (super::TARGET_RATE as usize / 10).max(1);
    let tail = vec![0f32; super::TARGET_RATE as usize / 2];
    let st = spotter.create_stream();
    let mut hits = HashSet::new();
    for data in [samples, tail.as_slice()] {
        for c in data.chunks(chunk) {
            st.accept_waveform(rate, c);
            while spotter.is_ready(&st) {
                spotter.decode(&st);
            }
            if let Some(r) = spotter.get_result(&st) {
                if !r.keyword.is_empty() {
                    hits.insert(r.keyword.clone());
                    spotter.reset(&st);
                }
            }
        }
    }
    hits
}

/// 扫描:每个阈值建一个含全部候选拼写(@v{i})的 spotter,喂全部正/负样本。
/// 返回 (recall[拼写][阈值]=命中正样本数, fa[拼写][阈值]=负样本是否误触)。
/// 一阈值一次建模(每档 spotter 复用于全部拼写+样本):9 次建模,KWS 模型小,秒级可接受。
fn sweep(
    kws_dir: &Path,
    variants: &[Variant],
    positives: &[Vec<f32>],
    negative: &[f32],
) -> Result<(Vec<Vec<u32>>, Vec<Vec<bool>>)> {
    let n_var = variants.len();
    let n_thr = GRID.len();
    let mut recall = vec![vec![0u32; n_thr]; n_var];
    let mut fa = vec![vec![false; n_thr]; n_var];
    let buf = variants
        .iter()
        .enumerate()
        .map(|(i, v)| format!("{} @v{i}", v.tokens))
        .collect::<Vec<_>>()
        .join("\n");
    let tag = |i: usize| format!("v{i}");
    for (ti, &thr) in GRID.iter().enumerate() {
        let cfg = wake::kws_config(kws_dir, &buf, thr, wake::KWS_SCORE);
        let spotter = sherpa_onnx::KeywordSpotter::create(&cfg)
            .ok_or_else(|| anyhow!("KWS 标定 spotter 创建失败(thr={thr})"))?;
        for utt in positives {
            let fired = detect(&spotter, utt);
            for i in 0..n_var {
                if fired.contains(&tag(i)) {
                    recall[i][ti] += 1;
                }
            }
        }
        let fired_neg = detect(&spotter, negative);
        for i in 0..n_var {
            if fired_neg.contains(&tag(i)) {
                fa[i][ti] = true;
            }
        }
    }
    Ok((recall, fa))
}

// ---- 决策 ----

#[derive(Debug, Clone)]
struct VariantDecision {
    i_pick: usize,
    recall_at_pick: f32,
    /// 「召回悬崖」档位(越高 = 越严的阈值仍稳触发 = 拼写越贴声);None → -1。
    quality: i32,
    verdict: &'static str,
}

/// 单拼写的阈值决策。偏召回(用户痛点是叫不应):
/// 取「负样本不误触的最宽松档」上方一档作余量,封顶在召回达标的最高档。
fn decide(recall: &[u32], fa: &[bool], n_pos: u32) -> VariantDecision {
    let n = GRID.len();
    let target = (TARGET_RECALL * n_pos as f32).ceil() as u32;
    let ratio = |i: usize| recall[i] as f32 / n_pos.max(1) as f32;
    // 召回随阈值↑非增:i_recall = 仍达标的最高档
    let i_recall = (0..n).rev().find(|&i| recall[i] >= target);
    // 误触随阈值↑非增:i_fa = 不误触的最低档(它及以上都不误触)
    let i_fa = (0..n).find(|&i| !fa[i]);
    let (i_pick, verdict) = match (i_recall, i_fa) {
        // 正常:窗口 [i_fa, i_recall] 非空 → 取 i_fa 上方一档(召回最优 + 一档误触余量)
        (Some(ir), Some(ifa)) if ifa <= ir => (ifa.saturating_add(1).min(ir), "good"),
        // 误触区与召回区重叠(吵/词易混):取召回达标最高档(此区内误触最少),警告
        (Some(ir), Some(_)) => (ir, "noisy"),
        // 负样本在每个阈值都误触(负样本里疑似混入了唤醒词/词太常见):同上,警告
        (Some(ir), None) => (ir, "noisy"),
        // 最宽松档都达不到召回(麦太弱/词太难听清):取最宽松,警告
        (None, _) => (0, "hard"),
    };
    VariantDecision {
        i_pick,
        recall_at_pick: ratio(i_pick),
        quality: i_recall.map(|i| i as i32).unwrap_or(-1),
        verdict,
    }
}

/// 跨拼写择优:质量(召回悬崖档)最高者;异读须比 canonical 高 ≥ADOPT_MARGIN 档才采纳,否则留 canonical。
fn pick_best(variants: &[Variant], decisions: &[VariantDecision]) -> (usize, VariantDecision) {
    let canon = variants.iter().position(|v| v.canonical);
    let best_alt = (0..variants.len())
        .filter(|&i| Some(i) != canon)
        .max_by_key(|&i| decisions[i].quality);
    match (canon, best_alt) {
        (Some(c), Some(a)) if decisions[a].quality >= decisions[c].quality + ADOPT_MARGIN => {
            (a, decisions[a].clone())
        }
        (Some(c), _) => (c, decisions[c].clone()),
        (None, Some(a)) => (a, decisions[a].clone()), // 规范读音编不出 → 用最佳异读
        (None, None) => (0, decisions[0].clone()),
    }
}

/// 总入口:候选拼写 → 扫描 → 逐拼写决策 → 择优 → 产出(灵敏度 + 可选拼写覆盖 + 召回 + 结论)。
pub(super) fn calibrate(
    kws_dir: &Path,
    word: &str,
    vocab: &HashSet<String>,
    positives: &[Vec<f32>],
    negative: &[f32],
) -> Result<CalibOutcome> {
    let variants = candidate_spellings(word, vocab);
    if variants.is_empty() {
        bail!("唤醒词「{word}」编不出模型 token(只支持中文词)");
    }
    if positives.is_empty() {
        bail!("没有可用的录音样本");
    }
    let (recall, fa) = sweep(kws_dir, &variants, positives, negative)?;
    let n_pos = positives.len() as u32;
    let decisions: Vec<VariantDecision> =
        (0..variants.len()).map(|i| decide(&recall[i], &fa[i], n_pos)).collect();
    let (best, dec) = pick_best(&variants, &decisions);
    let spelling = if variants[best].canonical {
        None
    } else {
        Some(format!("{} @{}", variants[best].tokens, word))
    };
    tracing::info!(
        word,
        sensitivity = threshold_to_sensitivity(GRID[dec.i_pick]),
        threshold = GRID[dec.i_pick],
        recall = dec.recall_at_pick,
        verdict = dec.verdict,
        adopted_alt = spelling.is_some(),
        variants = variants.len(),
        "唤醒标定完成"
    );
    Ok(CalibOutcome {
        sensitivity: threshold_to_sensitivity(GRID[dec.i_pick]),
        spelling,
        recall: dec.recall_at_pick,
        verdict: dec.verdict,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vocab(tokens: &[&str]) -> HashSet<String> {
        tokens.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn sensitivity_threshold_roundtrip() {
        // 正向锚点(对齐 mod.rs wake_threshold 文档)
        assert!((sensitivity_to_threshold(50.0) - 0.2).abs() < 1e-6);
        assert!((sensitivity_to_threshold(100.0) - 0.1).abs() < 1e-6);
        assert!((sensitivity_to_threshold(0.0) - 0.5).abs() < 1e-6);
        // 每个扫描档反映射回灵敏度再正向,误差不超过一个四舍五入档(<0.03)
        for &thr in GRID.iter() {
            let s = threshold_to_sensitivity(thr) as f32;
            let back = sensitivity_to_threshold(s);
            assert!((back - thr).abs() <= 0.031, "thr={thr} → sens={s} → {back}");
        }
    }

    #[test]
    fn candidate_canonical_only_when_no_heteronym() {
        // 旺(wàng)财(cái):无多音 → 1 个候选,且是 canonical
        let v = vocab(&["w", "àng", "c", "ái"]);
        let cands = candidate_spellings("旺财", &v);
        assert_eq!(cands.len(), 1, "无异读应只 1 候选: {cands:?}");
        assert!(cands[0].canonical);
        assert_eq!(cands[0].tokens, "w àng c ái");
    }

    #[test]
    fn candidate_expands_heteronyms_exactly_one_canonical() {
        // 还:hái / huán 两读,词表两条都支持 → 2 候选,恰一个 canonical(规范读音由词典定,不断言哪个)
        let v = vocab(&["h", "ái", "uán"]);
        let cands = candidate_spellings("还", &v);
        assert_eq!(cands.len(), 2, "两读都该编出: {cands:?}");
        assert_eq!(cands.iter().filter(|v| v.canonical).count(), 1, "恰一个 canonical");
        let toks: HashSet<&str> = cands.iter().map(|v| v.tokens.as_str()).collect();
        assert!(toks.contains("h ái") && toks.contains("h uán"));
    }

    #[test]
    fn candidate_drops_unencodable_reading_keeps_encodable() {
        // 只给 hái 的 token,huán 编不出 → 只剩 1 个(canonical 若是 hái 则保留)
        let v = vocab(&["h", "ái"]);
        let cands = candidate_spellings("还", &v);
        assert_eq!(cands.len(), 1, "huán 编不出应丢: {cands:?}");
        assert_eq!(cands[0].tokens, "h ái");
    }

    #[test]
    fn candidate_empty_for_non_chinese() {
        let v = vocab(&["h", "ái"]);
        assert!(candidate_spellings("hi", &v).is_empty());
        assert!(candidate_spellings("", &v).is_empty());
    }

    #[test]
    fn decide_good_picks_one_step_above_fa_floor() {
        // 召回:低阈高、index4 起跌破 4/5;负样本全程不误触 → i_fa=0 → i_pick=1
        let recall = [5, 5, 5, 5, 4, 3, 1, 0, 0];
        let fa = [false; 9];
        let d = decide(&recall, &fa, 5);
        assert_eq!(d.verdict, "good");
        assert_eq!(d.i_pick, 1, "i_fa(0) 上方一档");
        assert!((d.recall_at_pick - 1.0).abs() < 1e-6);
        assert_eq!(d.quality, 4, "召回悬崖在 index4");
    }

    #[test]
    fn decide_respects_fa_floor() {
        // 负样本在最低两档误触 → i_fa=2;召回达标到 index4 → 窗口[2,4],i_pick=min(3,4)=3
        let recall = [5, 5, 5, 5, 4, 2, 0, 0, 0];
        let fa = [true, true, false, false, false, false, false, false, false];
        let d = decide(&recall, &fa, 5);
        assert_eq!(d.verdict, "good");
        assert_eq!(d.i_pick, 3);
    }

    #[test]
    fn decide_noisy_when_fa_overlaps_recall() {
        // 召回只到 index1;误触到 index2(i_fa=3 > i_recall=1)→ noisy,取召回最高档
        let recall = [5, 4, 3, 2, 1, 0, 0, 0, 0];
        let fa = [true, true, true, false, false, false, false, false, false];
        let d = decide(&recall, &fa, 5);
        assert_eq!(d.verdict, "noisy");
        assert_eq!(d.i_pick, 1);
    }

    #[test]
    fn decide_hard_when_recall_never_meets_target() {
        let recall = [3, 2, 1, 0, 0, 0, 0, 0, 0];
        let fa = [false; 9];
        let d = decide(&recall, &fa, 5);
        assert_eq!(d.verdict, "hard");
        assert_eq!(d.i_pick, 0, "最宽松档");
        assert_eq!(d.quality, -1);
    }

    #[test]
    fn pick_best_keeps_canonical_without_margin() {
        let variants = vec![
            Variant { tokens: "a".into(), canonical: true },
            Variant { tokens: "b".into(), canonical: false },
        ];
        // 异读只高 0 档(平手)→ 不采纳,留 canonical
        let decisions = vec![
            VariantDecision { i_pick: 2, recall_at_pick: 1.0, quality: 4, verdict: "good" },
            VariantDecision { i_pick: 2, recall_at_pick: 1.0, quality: 4, verdict: "good" },
        ];
        assert_eq!(pick_best(&variants, &decisions).0, 0);
    }

    #[test]
    fn pick_best_adopts_clearly_better_alternate() {
        let variants = vec![
            Variant { tokens: "a".into(), canonical: true },
            Variant { tokens: "b".into(), canonical: false },
        ];
        // 异读高 2 档(≥ADOPT_MARGIN)→ 采纳
        let decisions = vec![
            VariantDecision { i_pick: 0, recall_at_pick: 0.8, quality: 2, verdict: "good" },
            VariantDecision { i_pick: 3, recall_at_pick: 1.0, quality: 4, verdict: "good" },
        ];
        assert_eq!(pick_best(&variants, &decisions).0, 1);
    }
}
