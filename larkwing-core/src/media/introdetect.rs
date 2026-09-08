//! 片头 / 片尾**指纹检测**(Plex / Jellyfin 式):片头曲在相邻集里的音频逐字节相同 → 取相邻集开头
//! 几分钟与结尾几分钟的音频算指纹(`fingerprint`),找最长公共段,公共段落在本集里的位置就是本集的
//! 片头 / 片尾。**逐集精确**:冷开场、火影式换曲、片尾后带预告全都自然处理(预告各集不同、不进公共段)。
//!
//! 静默后台活:本地视频剧集每集开播时触发,本集没测过才跑、一次只跑一个;失败 / 算不出 = 这一集没有
//! 自动跳,不打扰用户(§3.5 的「不静默」约束的是用户主动操作,这是锦上添花的机器活;日志留痕)。
//! 结果落 `media_skip`(source=detected,本集与邻居各一行);跑完时本集还在放就重算并广播 Skip。
//! 网络流不在此(没有本地音频;B 站番剧另有平台标注)。

use std::sync::atomic::Ordering;

use anyhow::{Context, Result};

use super::{fingerprint, is_local_path, EpisodeRef, MediaRuntime};
use crate::components::Component;
use crate::store::SkipRow;
use crate::lockext::LockExt;

/// 开头取多久去比(秒):OP 常见在前 5 分钟内结束,冷开场最长见过 6 分钟(B 站标注实测 345 s 起的 OP)。
pub(super) const HEAD_SECS: u32 = 360;
/// 结尾取多久去比(秒):ED + 预告一般不到 4 分钟。
pub(super) const TAIL_SECS: u32 = 240;
/// 两个邻居各给一个候选时,起止点都在这个容差内才算「一致」;不一致宁可不跳 —— 误跳正片比不跳糟。
pub(super) const AGREE_TOL_S: f64 = 2.0;

/// 一集的比对原料。
struct EpAudio {
    idx: usize,
    /// 结尾那段在整集里的起点(秒);None = 时长不知道,没取结尾。
    tail_from: Option<f64>,
    head: fingerprint::Fingerprint,
    tail: Option<fingerprint::Fingerprint>,
    duration: Option<f64>,
}

/// 多个候选段(各来自一个邻居)合成一个:一个 → 就它;两个 → 起止都在容差内取平均,否则 None。
pub(super) fn merge_candidates(cands: &[(f64, f64)], tol: f64) -> Option<(f64, f64)> {
    match cands {
        [] => None,
        [one] => Some(*one),
        [a, b, ..] => {
            if (a.0 - b.0).abs() <= tol && (a.1 - b.1).abs() <= tol {
                Some(((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0))
            } else {
                None
            }
        }
    }
}

impl MediaRuntime {
    /// 本地视频剧集开播时调:本集没有检测结果且 ffmpeg 在手且没人在跑 → 后台起一趟。
    pub(super) fn maybe_spawn_detect(&self) {
        let (key, entries, index) = {
            let guard = self.inner.playlist.lk();
            let Some(pl) = guard.as_ref() else { return };
            (pl.series_key.clone(), pl.entries.clone(), pl.index)
        };
        if entries.len() < 2 || !is_local_path(&entries[index].url) {
            return;
        }
        // 绝不为它下载 ffmpeg(播放本身早已预取;新装机器第一次看片没自动跳,之后有)
        if self.inner.components.ready(Component::Ffmpeg).is_none() {
            return;
        }
        let has_row = self
            .inner
            .store
            .media_skip
            .list(&key)
            .unwrap_or_default()
            .iter()
            .any(|r| r.source == "detected" && r.episode_id == entries[index].id);
        if has_row {
            return;
        }
        if self.inner.detect_busy.swap(true, Ordering::SeqCst) {
            return; // 上一趟还在跑,这集下次开播再来
        }
        let rt = self.clone();
        tokio::spawn(async move {
            match rt.detect_series(&key, &entries, index).await {
                Ok(found) => tracing::info!(key = %key, ep = index + 1, found, "片头片尾指纹检测完成"),
                Err(e) => tracing::info!(key = %key, ep = index + 1, "片头片尾指纹检测没跑成: {e:#}"),
            }
            rt.inner.detect_busy.store(false, Ordering::SeqCst);
        });
    }

    /// 比本集与前后邻居:开头找片头、结尾找片尾;结果落库(本集 + 邻居);本集还在放就广播。
    /// 返回是否测出了至少一段。
    async fn detect_series(&self, key: &str, entries: &[EpisodeRef], index: usize) -> Result<bool> {
        let ffmpeg = self.inner.components.ready(Component::Ffmpeg).context("ffmpeg 不在手")?;
        let mut idxs = vec![index];
        if index > 0 {
            idxs.push(index - 1);
        }
        if index + 1 < entries.len() {
            idxs.push(index + 1);
        }
        let mut eps: Vec<EpAudio> = Vec::with_capacity(idxs.len());
        for i in idxs {
            let path = std::path::Path::new(&entries[i].url);
            let duration = self.probe_with_ffmpeg(&ffmpeg, path).await.duration_seconds;
            let head_pcm = self
                .decode_file_pcm16k(path, 0.0, HEAD_SECS)
                .await
                .with_context(|| format!("解码第 {} 集开头失败", i + 1))?;
            let tail_from = duration.map(|d| (d - TAIL_SECS as f64).max(0.0));
            let tail_pcm = match tail_from {
                Some(from) => self.decode_file_pcm16k(path, from, TAIL_SECS).await.ok(),
                None => None,
            };
            let (head, tail) = tokio::task::spawn_blocking(move || {
                (
                    fingerprint::fingerprint_f32(&head_pcm),
                    tail_pcm.map(|t| fingerprint::fingerprint_f32(&t)),
                )
            })
            .await
            .context("指纹计算线程崩了")?;
            eps.push(EpAudio { idx: i, tail_from, head, tail, duration });
        }
        // 比对是纯 CPU(O(N²) 汉明,秒级):整段挪到阻塞线程,别占 tokio 工人
        let cur_idx = index;
        let cur_duration = eps[0].duration;
        let cmp = tokio::task::spawn_blocking(move || compare_neighbors(&eps)).await.context("比对线程崩了")?;

        // 邻居自己那一侧的段(顺手也存,下一集开播就不用再算)
        let mut neighbor_rows: Vec<SkipRow> = cmp
            .neighbors
            .iter()
            .filter(|n| n.intro.is_some() || n.outro.is_some())
            .map(|n| SkipRow {
                series_key: key.to_string(),
                episode_id: entries[n.idx].id.clone(),
                source: "detected".into(),
                intro_start: n.intro.map(|s| s.0),
                intro_end: n.intro.map(|s| s.1),
                outro_start: n.outro.map(|s| s.0),
                outro_end: n.outro.map(|s| s.1),
                duration: n.duration,
                updated_at: 0,
            })
            .collect();
        let intro_cands = cmp.intro_cands;
        let outro_cands = cmp.outro_cands;
        let intro = merge_candidates(&intro_cands, AGREE_TOL_S);
        let outro = merge_candidates(&outro_cands, AGREE_TOL_S);
        if intro.is_none() && outro.is_none() {
            // 也记一行空结果?不记:下次开播会再试一次(邻居可能刚下载好)。
            return Ok(false);
        }
        // 两个邻居意见不合(merge 给 None 而候选非空)→ 本集不写那一项,邻居行也别把分歧存下去
        if intro.is_none() && !intro_cands.is_empty() {
            tracing::info!(ep = cur_idx + 1, "两侧邻居对片头位置不一致,这一集不自动跳片头");
            neighbor_rows.iter_mut().for_each(|r| {
                r.intro_start = None;
                r.intro_end = None;
            });
        }
        if outro.is_none() && !outro_cands.is_empty() {
            tracing::info!(ep = cur_idx + 1, "两侧邻居对片尾位置不一致,这一集不自动跳片尾");
            neighbor_rows.iter_mut().for_each(|r| {
                r.outro_start = None;
                r.outro_end = None;
            });
        }
        let cur_row = SkipRow {
            series_key: key.to_string(),
            episode_id: entries[cur_idx].id.clone(),
            source: "detected".into(),
            intro_start: intro.map(|s| s.0),
            intro_end: intro.map(|s| s.1),
            outro_start: outro.map(|s| s.0),
            outro_end: outro.map(|s| s.1),
            duration: cur_duration,
            updated_at: 0,
        };
        self.inner.store.media_skip.upsert(&cur_row)?;
        for r in neighbor_rows.iter().filter(|r| r.intro_end.is_some() || r.outro_start.is_some()) {
            // 邻居已有检测行(它自己开播时测的)就不覆盖 —— 它那次是以自己为主角、两侧对照,更可信
            let exists = self
                .inner
                .store
                .media_skip
                .list(key)?
                .iter()
                .any(|x| x.source == "detected" && x.episode_id == r.episode_id);
            if !exists {
                self.inner.store.media_skip.upsert(r)?;
            }
        }
        // 本集还在放 → 重算并广播(前端拿到新的 skip;正播在片头里也来得及跳)
        let still_here = self
            .inner
            .playlist
            .lk()
            .as_ref()
            .is_some_and(|pl| pl.series_key == key && pl.index == index);
        if still_here {
            self.refresh_skip();
        }
        Ok(true)
    }
}

/// 一个邻居的比对结果:它自己那一侧的片头 / 片尾段(绝对秒)。
struct NeighborSegs {
    idx: usize,
    duration: Option<f64>,
    intro: Option<(f64, f64)>,
    outro: Option<(f64, f64)>,
}

/// 本集与全部邻居的比对结果:本集侧候选(每个邻居一个)+ 邻居侧各自的段。
struct Comparison {
    intro_cands: Vec<(f64, f64)>,
    outro_cands: Vec<(f64, f64)>,
    neighbors: Vec<NeighborSegs>,
}

/// 纯 CPU:`eps[0]` 是本集,其余是邻居;开头找片头、结尾找片尾。
fn compare_neighbors(eps: &[EpAudio]) -> Comparison {
    let opts = fingerprint::MatchOpts::default();
    let (cur, neighbors) = eps.split_first().expect("至少有本集");
    let mut out = Comparison { intro_cands: Vec::new(), outro_cands: Vec::new(), neighbors: Vec::new() };
    for nb in neighbors {
        let mut n = NeighborSegs { idx: nb.idx, duration: nb.duration, intro: None, outro: None };
        if let Some(seg) = fingerprint::common_segment(&cur.head, &nb.head, &opts) {
            out.intro_cands.push((seg.a_start_secs, seg.a_start_secs + seg.len_secs));
            n.intro = Some((seg.b_start_secs, seg.b_start_secs + seg.len_secs));
            tracing::debug!(ep = cur.idx + 1, nb = nb.idx + 1, start = seg.a_start_secs, len = seg.len_secs, ber = seg.ber, "开头公共段");
        }
        if let (Some(ct), Some(nt), Some(cf), Some(nf)) = (&cur.tail, &nb.tail, cur.tail_from, nb.tail_from) {
            if let Some(seg) = fingerprint::common_segment(ct, nt, &opts) {
                out.outro_cands.push((cf + seg.a_start_secs, cf + seg.a_start_secs + seg.len_secs));
                n.outro = Some((nf + seg.b_start_secs, nf + seg.b_start_secs + seg.len_secs));
                tracing::debug!(ep = cur.idx + 1, nb = nb.idx + 1, start = cf + seg.a_start_secs, len = seg.len_secs, ber = seg.ber, "结尾公共段");
            }
        }
        out.neighbors.push(n);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_candidates_requires_agreement() {
        assert_eq!(merge_candidates(&[], 2.0), None);
        assert_eq!(merge_candidates(&[(10.0, 100.0)], 2.0), Some((10.0, 100.0)), "一个邻居就认");
        assert_eq!(
            merge_candidates(&[(10.0, 100.0), (11.0, 101.5)], 2.0),
            Some((10.5, 100.75)),
            "两侧一致取平均"
        );
        assert_eq!(merge_candidates(&[(10.0, 100.0), (40.0, 130.0)], 2.0), None, "两侧不合宁可不跳");
    }
}
