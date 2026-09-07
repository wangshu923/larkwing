//! 片头 / 片尾(OP/ED):把三路来源汇成一份「本集怎么跳」(`SkipInfo`,随 NowPlaying 过桥)。
//!
//! 来源与权威度,由高到低:
//!   1. **手标**(用户说「片头到这里」,锚在某集、从那集起生效;用户纠错永远赢);
//!   2. **B 站 OP/ED 段**(番剧 playurl 的 clip_info_list,人工标注、逐集精确);
//!   3. **章节元数据**(mkv/mp4 里名为 OP/ED 的章节);
//!   4. **指纹检测**(相邻集音频公共段,见 `introdetect`)。
//! 前端消费:自然播进片头段 → 跳到段尾;自然越过片尾起点且有下一集 → 3 秒倒计时切集(用户拍板
//! 2026-09-07)。全部纯函数,不碰 IO;数据形状与判定单源在此。

use serde::Serialize;

use super::probe::Chapter;
use crate::store::SkipRow;

/// 一段 [start, end)(秒)。
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Seg {
    pub start: f64,
    pub end: f64,
}

/// 过桥给前端的「本集怎么跳」。`intro` = 片头段(跳到 end);`outro_start` = 片尾起点(到线切下一集)。
/// `source` = 这份信息主要来自谁(manual / bili / chapter / detected;片头片尾来源不同时取片头的)。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SkipInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub intro: Option<Seg>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outro_start: Option<f64>,
    pub source: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegKind {
    Intro,
    Outro,
}

/// 自动来源(数值越大越权威)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AutoSource {
    Detected,
    Chapter,
    Bili,
}

impl AutoSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            AutoSource::Detected => "detected",
            AutoSource::Chapter => "chapter",
            AutoSource::Bili => "bili",
        }
    }
}

/// 自动来源给的一段。
#[derive(Debug, Clone, PartialEq)]
pub struct AutoSeg {
    pub kind: SegKind,
    pub start: f64,
    pub end: f64,
    pub source: AutoSource,
}

/// 一条手标规则(从 `episode_id` 那一集起生效;None = 那一项没标)。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ManualRule {
    pub episode_id: String,
    pub intro_start: Option<f64>,
    pub intro_end: Option<f64>,
    pub outro_start: Option<f64>,
    /// 标记时那一集的总长:片尾按「距结尾多少秒」套到别的集上。
    pub duration: Option<f64>,
}

impl ManualRule {
    pub fn from_row(r: &SkipRow) -> ManualRule {
        ManualRule {
            episode_id: r.episode_id.clone(),
            intro_start: r.intro_start,
            intro_end: r.intro_end,
            outro_start: r.outro_start,
            duration: r.duration,
        }
    }
}

/// 片头段的最短长度(秒):再短的「片头」不值得跳,也多半是标错 / 测错。
pub const MIN_SEG_SECS: f64 = 3.0;

/// 对本集生效的手标规则 = 锚点集在队列里**不晚于**本集的最近一条(锚点已不在队列 → 忽略该条)。
pub fn effective_manual<'a>(
    order: &[&str],
    current: &str,
    manual: &'a [ManualRule],
) -> Option<&'a ManualRule> {
    let cur_pos = order.iter().position(|id| *id == current)?;
    manual
        .iter()
        .filter_map(|r| {
            order.iter().position(|id| *id == r.episode_id).filter(|p| *p <= cur_pos).map(|p| (p, r))
        })
        .max_by_key(|(p, _)| *p)
        .map(|(_, r)| r)
}

/// 本集的解析:手标 > 自动(Bili > Chapter > Detected)。`order` = 队列集 id 顺序、`current` = 本集 id、
/// `duration` = 本集总长(片尾距结尾换算 + 越界剔除)。没有任何可用信息 → None。
pub fn resolve(
    order: &[&str],
    current: &str,
    duration: Option<f64>,
    manual: &[ManualRule],
    auto: &[AutoSeg],
) -> Option<SkipInfo> {
    let rule = effective_manual(order, current, manual);
    let best = |kind: SegKind| {
        auto.iter()
            .filter(|s| s.kind == kind && s.end > s.start + MIN_SEG_SECS)
            .max_by(|a, b| {
                a.source.cmp(&b.source).then_with(|| {
                    (a.end - a.start).partial_cmp(&(b.end - b.start)).unwrap_or(std::cmp::Ordering::Equal)
                })
            })
    };

    let mut source: Option<&str> = None;
    // 片头
    let intro = match rule.and_then(|r| r.intro_end.map(|e| (r.intro_start.unwrap_or(0.0), e))) {
        Some((s, e)) => {
            source = Some("manual");
            Some(Seg { start: s, end: e })
        }
        None => best(SegKind::Intro).map(|a| {
            source = Some(a.source.as_str());
            Seg { start: a.start, end: a.end }
        }),
    };
    // 片尾:手标存的是标记那集的绝对秒 + 那集总长 → 距结尾秒数套到本集
    let outro_start = match rule.and_then(|r| r.outro_start.map(|o| (o, r.duration))) {
        Some((o, rule_dur)) => {
            source.get_or_insert("manual");
            match (rule_dur, duration) {
                (Some(rd), Some(d)) if rd > 0.0 && d > 0.0 => Some((d - (rd - o)).max(0.0)),
                _ => Some(o),
            }
        }
        None => best(SegKind::Outro).map(|a| {
            source.get_or_insert(a.source.as_str());
            a.start
        }),
    };

    // 越界 / 太短剔除:片头必须 ≥ MIN_SEG_SECS 且不出片长;片尾起点得在 0 与结尾之间
    let intro = intro.filter(|s| {
        s.end > s.start + MIN_SEG_SECS && s.start >= 0.0 && duration.map_or(true, |d| s.end < d)
    });
    let outro_start =
        outro_start.filter(|o| *o > 0.0 && duration.map_or(true, |d| *o < d - 1.0));
    if intro.is_none() && outro_start.is_none() {
        return None;
    }
    Some(SkipInfo { intro, outro_start, source: source.unwrap_or("detected").to_string() })
}

/// 把库里的检测行变成自动段(只取本集那一行)。
pub fn detected_segs(rows: &[SkipRow], current: &str) -> Vec<AutoSeg> {
    let mut out = Vec::new();
    for r in rows.iter().filter(|r| r.source == "detected" && r.episode_id == current) {
        if let (Some(s), Some(e)) = (r.intro_start, r.intro_end) {
            out.push(AutoSeg { kind: SegKind::Intro, start: s, end: e, source: AutoSource::Detected });
        }
        if let (Some(s), Some(e)) = (r.outro_start, r.outro_end) {
            out.push(AutoSeg { kind: SegKind::Outro, start: s, end: e, source: AutoSource::Detected });
        }
    }
    out
}

/// 章节元数据 → 自动段:只认**标题**说得清是片头 / 片尾的章节(OP / Opening / Intro / 片头 …;
/// ED / Ending / Outro / 片尾 …)。没标题的章节边界不猜 —— 误判代价是跳掉正片。
pub fn chapters_to_auto(chapters: &[Chapter]) -> Vec<AutoSeg> {
    chapters
        .iter()
        .filter_map(|c| {
            let kind = chapter_kind(&c.title)?;
            Some(AutoSeg { kind, start: c.start, end: c.end, source: AutoSource::Chapter })
        })
        .collect()
}

/// 章节名 → 片头 / 片尾 / 都不是。词边界判定:`op` / `ed` 这种两字母 token 必须独立成词
/// (「OP」「OP1」「ED (TV size)」算;「open」「edit」不算)。
pub fn chapter_kind(title: &str) -> Option<SegKind> {
    let t = title.trim().to_lowercase();
    if t.is_empty() {
        return None;
    }
    const INTRO_WORDS: &[&str] = &["opening", "intro", "片头", "オープニング", "主题歌", "主題歌", "主题曲"];
    const OUTRO_WORDS: &[&str] = &["ending", "outro", "片尾", "エンディング", "credits", "片尾曲"];
    if INTRO_WORDS.iter().any(|w| t.contains(w)) || short_token(&t, "op") {
        return Some(SegKind::Intro);
    }
    if OUTRO_WORDS.iter().any(|w| t.contains(w)) || short_token(&t, "ed") {
        return Some(SegKind::Outro);
    }
    None
}

/// `tok` 作为独立词出现(前后不是字母):「op」「op1」「op (tv size)」「第1话 op」都算,「open」不算。
fn short_token(t: &str, tok: &str) -> bool {
    let bytes = t.as_bytes();
    let mut from = 0;
    while let Some(i) = t[from..].find(tok) {
        let i = from + i;
        let before_ok = i == 0 || !bytes[i - 1].is_ascii_alphabetic();
        let after = i + tok.len();
        let after_ok = after >= bytes.len() || !bytes[after].is_ascii_alphabetic();
        if before_ok && after_ok {
            return true;
        }
        from = after;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(ep: &str, intro_end: Option<f64>, outro: Option<f64>, dur: Option<f64>) -> ManualRule {
        ManualRule { episode_id: ep.into(), intro_start: None, intro_end, outro_start: outro, duration: dur }
    }

    #[test]
    fn manual_rule_applies_from_anchor_onward() {
        let order = ["e1", "e2", "e3", "e4"];
        let manual = [rule("e1", Some(90.0), None, None), rule("e3", Some(60.0), None, None)];
        let pick = |cur: &str| resolve(&order, cur, Some(1400.0), &manual, &[]).unwrap().intro.unwrap().end;
        assert_eq!(pick("e1"), 90.0);
        assert_eq!(pick("e2"), 90.0, "e2 沿用 e1 起的规则");
        assert_eq!(pick("e3"), 60.0, "e3 起换了片头");
        assert_eq!(pick("e4"), 60.0);
        // 锚点不在队列 / 本集不在队列 → 手标不生效
        let orphan = [rule("gone", Some(30.0), None, None)];
        assert!(resolve(&order, "e2", None, &orphan, &[]).is_none());
        assert!(resolve(&order, "e9", None, &manual, &[]).is_none());
    }

    #[test]
    fn manual_beats_auto_and_auto_ranks_bili_chapter_detected() {
        let order = ["e1", "e2"];
        let auto = [
            AutoSeg { kind: SegKind::Intro, start: 0.0, end: 88.0, source: AutoSource::Detected },
            AutoSeg { kind: SegKind::Intro, start: 0.0, end: 89.0, source: AutoSource::Chapter },
            AutoSeg { kind: SegKind::Intro, start: 5.0, end: 95.0, source: AutoSource::Bili },
            AutoSeg { kind: SegKind::Outro, start: 1300.0, end: 1390.0, source: AutoSource::Detected },
        ];
        let s = resolve(&order, "e1", Some(1400.0), &[], &auto).unwrap();
        assert_eq!(s.intro, Some(Seg { start: 5.0, end: 95.0 }), "B 站标注最权威");
        assert_eq!(s.outro_start, Some(1300.0), "片尾只有检测给了就用检测");
        assert_eq!(s.source, "bili", "来源以片头的为主");

        let manual = [rule("e1", Some(70.0), None, None)];
        let s = resolve(&order, "e1", Some(1400.0), &manual, &auto).unwrap();
        assert_eq!(s.intro, Some(Seg { start: 0.0, end: 70.0 }), "手标压过一切自动来源");
        assert_eq!(s.source, "manual");
    }

    #[test]
    fn outro_from_manual_is_measured_from_the_end() {
        let order = ["e1", "e2"];
        // 第 1 集 1400s 长,片尾从 1310 起 = 距结尾 90s;第 2 集 1380s 长 → 1290
        let manual = [rule("e1", None, Some(1310.0), Some(1400.0))];
        let s = resolve(&order, "e2", Some(1380.0), &manual, &[]).unwrap();
        assert_eq!(s.outro_start, Some(1290.0));
        // 本集时长未知 → 原样用绝对秒
        assert_eq!(resolve(&order, "e2", None, &manual, &[]).unwrap().outro_start, Some(1310.0));
    }

    #[test]
    fn bounds_and_min_length_are_enforced() {
        let order = ["e1"];
        let too_short = [AutoSeg { kind: SegKind::Intro, start: 0.0, end: 2.0, source: AutoSource::Bili }];
        assert!(resolve(&order, "e1", Some(1400.0), &[], &too_short).is_none(), "2 秒不算片头");
        let beyond = [AutoSeg { kind: SegKind::Intro, start: 0.0, end: 1500.0, source: AutoSource::Bili }];
        assert!(resolve(&order, "e1", Some(1400.0), &[], &beyond).is_none(), "越过片长剔除");
        let outro_late = [rule("e1", None, Some(1399.5), Some(1400.0))];
        assert!(resolve(&order, "e1", Some(1400.0), &outro_late, &[]).is_none(), "片尾线贴着结尾没意义");
    }

    #[test]
    fn chapter_titles_classify_by_word_boundary() {
        assert_eq!(chapter_kind("OP"), Some(SegKind::Intro));
        assert_eq!(chapter_kind("OP1 (TV size)"), Some(SegKind::Intro));
        assert_eq!(chapter_kind("Opening"), Some(SegKind::Intro));
        assert_eq!(chapter_kind("片头曲"), Some(SegKind::Intro));
        assert_eq!(chapter_kind("ED"), Some(SegKind::Outro));
        assert_eq!(chapter_kind("Ending"), Some(SegKind::Outro));
        assert_eq!(chapter_kind("片尾"), Some(SegKind::Outro));
        assert_eq!(chapter_kind("open sea"), None, "open 不是 op");
        assert_eq!(chapter_kind("edit"), None, "edit 不是 ed");
        assert_eq!(chapter_kind("Part A"), None);
        assert_eq!(chapter_kind(""), None);
        let chapters = [
            Chapter { start: 0.0, end: 90.0, title: "OP".into() },
            Chapter { start: 90.0, end: 1300.0, title: "Part A".into() },
            Chapter { start: 1300.0, end: 1390.0, title: "ED".into() },
            Chapter { start: 1390.0, end: 1420.0, title: "Preview".into() },
        ];
        let auto = chapters_to_auto(&chapters);
        assert_eq!(auto.len(), 2);
        assert_eq!((auto[0].kind, auto[0].end), (SegKind::Intro, 90.0));
        assert_eq!((auto[1].kind, auto[1].start), (SegKind::Outro, 1300.0));
    }
}
