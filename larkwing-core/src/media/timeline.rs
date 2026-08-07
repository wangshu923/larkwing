//! 时间轴契约(P2 地基,2026-08-07)。**音画不同步的历史事故,根子全是同一件事:两条轨对「零点」
//! 的理解不一样。** 这个文件是那条契约的家 —— 零点怎么定、段时间戳谁来写、产出怎么自校。
//!
//! 历史长相(同一个病的不同外壳):`?t=` 重启式 seek(视频回退关键帧、音频不回退)· 逐段独立编码
//! AAC 的 priming 被钉固定网格(**实测每 6s 快 0.08s、10min 漂 8s**)· ffmpeg 输入 seek 把 tfdt
//! 重置为 0(不改则各段堆在 0 秒)· 非默认音轨 `-map 0:a:1` 残留非零 tfdt。
//!
//! 对策不是「小心一点」,是**产出即校验**:切完的段自己对账,对不上就整条降级到已验证的兜底路,
//! 而不是等真机上人耳听出来(2026-08-07 用户明确要求:这块踩过很多次)。

/// 段时长对账的容差(秒)。**按真实测量定的,不是拍脑袋**(2026-08-07,真 ffmpeg copy 切段):
/// 首段实测比计划长 **45ms**(ffmpeg 分片时长报数只到 10ms + 一帧),中间段一秒不差 ——
/// 那 45ms 是**常量偏移不是漂移**,卡太紧会把好片子误降级成 muxed(白掉画质)。
/// 取 0.12s ≈ 24fps 的三帧:容得下这类一次性偏移,又仍在人眼可觉的唇音失配门限附近
///(约 45ms 声超前 / 125ms 声滞后),且对「每段 80ms」那种真漂移(0.2.6 实测)第二段就报警。
pub const SEAM_TOL: f64 = 0.12;

/// 一段对不上的账。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Drift {
    /// 第几段开始超容差(0 起)。
    pub seg: usize,
    /// 到这一段为止的**累计**漂移(秒;正 = 实际比计划长,负 = 短)。
    pub secs: f64,
}

/// 累计漂移检测:逐段把「实际产出的时长」与「计划的时长」相减、**累加**,任一段的累计量超过
/// `tol` 即判失败(返回段号与当时的累计漂移)。
///
/// **为什么看累计而不是单段**:0.2.6 那个真事故里每段只多 0.08s —— 单看哪一段都"差不多",
/// 但它每段都同向偏,10 分钟攒到 8 秒。单段容差放得下的正是它,累计放不下。
///
/// 两边长度不等(段没切完 / 多切了)按较短的那个对到哪算到哪 —— 这里只管时间轴,数量对不上
/// 是调用方的事。
pub fn cumulative_drift(planned: &[f64], measured: &[f64], tol: f64) -> Option<Drift> {
    let mut acc = 0.0f64;
    for (i, (p, m)) in planned.iter().zip(measured.iter()).enumerate() {
        acc += m - p;
        if acc.abs() > tol {
            return Some(Drift { seg: i, secs: acc });
        }
    }
    None
}

// ── 契约①「零点唯一」的归属:**ffmpeg 已经在做,我们的责任是别再做第二遍** ──
//
// 这里原本有个 `zero_point(video_start, audio_start) = min(两者)`,打算让所有轨都减掉它。
// 2026-08-07 实测把它否掉了 —— 结论留在这儿,免得以后有人"补上"这个看起来天经地义的步骤:
//
// · 容器声明的起点确实各不相同:mkv 视频 0.000 / 音频 **−0.023**(音频 priming 更早开始);
//   **MPEG-TS 视频 1.423 / 音频 1.400** —— TS 的起始 PTS 天生非 0。
// · 但 **ffmpeg 报出来的时间已经归一过**,用的正是「取最早那条轨当 0」这条规则:同一个 TS,
//   `showinfo` 报的关键帧是 0 / 2 / 4…,`-ss` 也吃这条 0 基轴(两者同轴,实测)。
// · 端到端复核(`copy_segments_have_no_cumulative_drift` 跑 matroska + **mpegts** 两遍):
//   TS 那遍实测段长 `[6,6,6,6,6,6,4.0]` 对计划 `[6,6,6,6,6,6,4.01]` —— **零偏差**。
//
// **所以我们再减一次 t0 就是双重修正,反而凭空造出 1.4 秒的偏移。** 规则本身没错,只是实现者
// 是 ffmpeg。真需要自己动手的场合只有「绕过 ffmpeg 直接读容器原生时间戳」——BMFF 的 moov
// 那条路自成一套、不与 ffmpeg 路混用;将来若自己解析字幕时间轴,再把这个函数按需请回来。

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_drift_when_segments_come_out_as_planned() {
        let planned = vec![6.0; 10];
        assert_eq!(cumulative_drift(&planned, &planned, SEAM_TOL), None);
        // 每段来回抖一帧、不同向 → 累计不涨,不该误报(正常的取整抖动)
        let measured = vec![6.04, 5.96, 6.04, 5.96, 6.04, 5.96, 6.04, 5.96, 6.04, 5.96];
        assert_eq!(cumulative_drift(&planned, &measured, SEAM_TOL), None);
    }

    #[test]
    fn catches_the_2026_06_20_style_accumulating_drift() {
        // 真事故复刻:每段多 0.08s(逐段独立编码 AAC 的 priming),单段看着"差不多",累计要命。
        let planned = vec![6.0; 100];
        let measured = vec![6.08; 100];
        let d = cumulative_drift(&planned, &measured, SEAM_TOL).expect("这种漂移必须抓住");
        // 0.08×2 = 0.16 > 0.12 容差 → 第 2 段(0 起 = 1)就报警,离出事还早得很。
        assert_eq!(d.seg, 1, "两段之内必须发现 —— 真事故 10 分钟能漂 8 秒");
        assert!((d.secs - 0.16).abs() < 1e-9);
        // 容差放大到一段之上时,它仍然会在攒够的那一段被抓住(累计的意义)
        let d2 = cumulative_drift(&planned, &measured, 0.5).expect("累计早晚超");
        assert_eq!(d2.seg, 6, "0.08×7 = 0.56 > 0.5 → 第 7 段(0 起 = 6)");
    }

    #[test]
    fn reports_shortfall_as_negative_drift() {
        // 反向也要抓:实际比计划短(段被截断)同样是时间轴对不上。
        let planned = vec![6.0; 5];
        let measured = vec![5.0; 5];
        let d = cumulative_drift(&planned, &measured, SEAM_TOL).expect("短了也是漂");
        assert_eq!(d.seg, 0);
        assert!(d.secs < 0.0, "负 = 实际比计划短");
    }

    #[test]
    fn length_mismatch_checks_the_overlap_only() {
        // 段没切完(measured 少)→ 对到哪算到哪,不 panic、不误判。
        assert_eq!(cumulative_drift(&[6.0, 6.0, 6.0], &[6.0], SEAM_TOL), None);
        assert_eq!(cumulative_drift(&[], &[6.0], SEAM_TOL), None);
    }
}
