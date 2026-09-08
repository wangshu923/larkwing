//! 片头片尾的**运行时**半边:手标 / 重算 / 广播 / 清除。
//!
//! 四路来源(手标 > B 站 clip_info_list > mkv 章节 > 指纹检测)汇成一个结果的纯函数
//! 在 `skip.rs`;这里只管什么时候去问它、问完发给谁。

use super::control::fmt_clock;
use super::*;

impl MediaRuntime {
    /// 算本集怎么跳(起播 / 标记后 / 检测完重算):队列在手才有意义(单部电影不跳)。原料存进
    /// `skip_ctx` 供重算;结果既是 `NowPlaying.skip` 也是 `MediaEvent::Skip` 的载荷。
    pub(super) fn compute_skip(&self, auto: Vec<skip::AutoSeg>, duration: Option<f64>) -> Option<skip::SkipInfo> {
        let (key, order, current) = {
            let guard = self.inner.playlist.lk();
            let Some(pl) = guard.as_ref() else {
                *self.inner.skip_ctx.lk() = None;
                return None;
            };
            (
                pl.series_key.clone(),
                pl.entries.iter().map(|e| e.id.clone()).collect::<Vec<_>>(),
                pl.entries[pl.index].id.clone(),
            )
        };
        let rows = self.inner.store.media_skip.list(&key).unwrap_or_default();
        let manual: Vec<skip::ManualRule> =
            rows.iter().filter(|r| r.source == "manual").map(skip::ManualRule::from_row).collect();
        let mut segs = skip::detected_segs(&rows, &current);
        segs.extend(auto.iter().cloned());
        let order_refs: Vec<&str> = order.iter().map(String::as_str).collect();
        let info = skip::resolve(&order_refs, &current, duration, &manual, &segs);
        *self.inner.skip_ctx.lk() =
            Some(SkipCtx { auto, duration, current: info.clone() });
        info
    }

    /// 标记 / 检测结果变了:用存着的原料重算并广播(前端替换 `NowPlaying.skip`)。
    pub(super) fn refresh_skip(&self) -> Option<skip::SkipInfo> {
        let (auto, duration) = match self.inner.skip_ctx.lk().as_ref() {
            Some(c) => (c.auto.clone(), c.duration),
            None => return None,
        };
        // 时长以前端回报的为准(起播时探不出的 /m/ 混流路,播起来后前端知道)
        let duration = duration.or(self.inner.playback.lk().duration_secs);
        let info = self.compute_skip(auto, duration);
        self.publish(MediaEvent::Skip { skip: info.clone() });
        info
    }

    /// 手标片头 / 片尾(嘴控「片头到这里」/ 播放器菜单):锚在当前这一集、**从这集起**生效;只改被标的
    /// 那一项,其余继承本集在用的手标规则(第 26 集换片头不丢第 1 集标的片尾;继承的片尾按距结尾
    /// 换算到本集)。`value` 缺省 = 播放器此刻位置。
    pub(super) fn mark_skip(&self, action: &str, value: Option<f64>) -> Result<String> {
        let (key, order, current, index) = {
            let guard = self.inner.playlist.lk();
            let Some(pl) = guard.as_ref() else {
                anyhow::bail!("现在没有在放剧集,片头片尾标记只对多集内容有意义");
            };
            (
                pl.series_key.clone(),
                pl.entries.iter().map(|e| e.id.clone()).collect::<Vec<_>>(),
                pl.entries[pl.index].id.clone(),
                pl.index,
            )
        };
        let at = match value {
            Some(v) => v,
            None => self.current_position().context("不知道现在放到哪了,说个秒数吧")?,
        };
        anyhow::ensure!(at.is_finite() && at >= 0.0, "秒数不对: {at}");
        let duration = self
            .inner
            .playback
            .lk()
            .duration_secs
            .or_else(|| self.inner.skip_ctx.lk().as_ref().and_then(|c| c.duration));
        let rows = self.inner.store.media_skip.list(&key)?;
        let manual: Vec<skip::ManualRule> =
            rows.iter().filter(|r| r.source == "manual").map(skip::ManualRule::from_row).collect();
        let order_refs: Vec<&str> = order.iter().map(String::as_str).collect();
        let mut rule = skip::effective_manual(&order_refs, &current, &manual).cloned().unwrap_or_default();
        if rule.episode_id != current {
            // 从更早的锚点继承过来:片尾是「那集的绝对秒」,按距结尾换算成本集的
            if let (Some(o), Some(rd), Some(d)) = (rule.outro_start, rule.duration, duration) {
                if rd > 0.0 && d > 0.0 {
                    rule.outro_start = Some((d - (rd - o)).max(0.0));
                }
            }
            rule.episode_id = current.clone();
            rule.duration = duration.or(rule.duration);
        }
        match action {
            "intro_start" => rule.intro_start = Some(at),
            "intro_end" => {
                rule.intro_end = Some(at);
                if rule.intro_start.is_some_and(|s| s >= at) {
                    rule.intro_start = None; // 起点在终点之后没意义,退回从 0 算
                }
            }
            _ => {
                rule.outro_start = Some(at);
                rule.duration = duration.or(rule.duration);
            }
        }
        if rule.duration.is_none() {
            rule.duration = duration;
        }
        self.inner.store.media_skip.upsert(&crate::store::SkipRow {
            series_key: key,
            episode_id: current,
            source: "manual".into(),
            intro_start: rule.intro_start,
            intro_end: rule.intro_end,
            outro_start: rule.outro_start,
            outro_end: None,
            duration: rule.duration,
            updated_at: 0,
        })?;
        let info = self.refresh_skip();
        let mut out = format!("已记下,从第 {} 集起:", index + 1);
        match info {
            Some(s) => {
                if let Some(seg) = s.intro {
                    out.push_str(&format!("片头 {}–{}", fmt_clock(seg.start), fmt_clock(seg.end)));
                }
                if let Some(o) = s.outro_start {
                    if s.intro.is_some() {
                        out.push(',');
                    }
                    out.push_str(&format!("片尾从 {} 开始", fmt_clock(o)));
                }
                out.push_str(";以后这部剧自动跳。");
            }
            None => out.push_str("标记已保存,但片头段还不完整(只标了起点或落在片长外),标上终点才会跳。"),
        }
        Ok(out)
    }

    /// 清掉这部剧的全部手标(检测 / 平台标注留着)。
    pub(super) fn clear_skip_marks(&self) -> Result<String> {
        let key = self
            .inner
            .playlist
            .lk()
            .as_ref()
            .map(|pl| pl.series_key.clone())
            .context("现在没有在放剧集,没有可清除的标记")?;
        let n = self.inner.store.media_skip.clear_manual(&key)?;
        self.refresh_skip();
        Ok(if n > 0 { "已清除这部剧的手动片头片尾标记".into() } else { "这部剧没有手动标记".into() })
    }
}
