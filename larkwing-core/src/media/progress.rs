//! 续播进度:集身份 + 集内秒数落盘(**按家记不按人**,只存相对名绝不落绝对路径)。
//!
//! 读侧三闸(<30s 当没看 / 离结尾 <90s 当看完 / 短于 10 分钟不记)单源在 mod.rs 常量。

use super::queue::{pl_key, single_episode_id, single_key};
use super::*;

/// 续播读侧:这条进度值得「接着放」吗?看完 / 看了不到 30 秒 / 离结尾不到 90 秒 / 内容太短 → None。
pub(super) fn resume_position(p: &crate::store::Progress) -> Option<f64> {
    if p.finished || p.position_seconds < RESUME_HEAD_S {
        return None;
    }
    if p.duration_seconds > 0.0
        && (p.duration_seconds < RESUME_MIN_DURATION_S
            || p.position_seconds >= p.duration_seconds - RESUME_TAIL_S)
    {
        return None;
    }
    Some(p.position_seconds)
}

impl MediaRuntime {
    /// 起播成功:登记续播身份(前端心跳据此落集内位置)+ 落「现在放到这一集 / 这部」。
    /// 剧集按队列身份;单部视频 / 单个长音频按文件 / 链接身份(电影看一半、评书听一半明天接着);
    /// **歌不记**(重听从头是常识)—— 边界按时长:短于 RESUME_MIN_DURATION_S 的音频算歌
    /// (2026-09-16 ★ 用户拍板取代原「放歌一律不记」;起播时时长未知就先登记,读侧 resume_position
    /// 与写侧 persist_progress 都按回报时长再闸,短的至多留一行位置 0 的记录)。
    /// 失败不挡播放 —— 续播是锦上添花,只 warn。
    pub(super) fn track_progress(&self, np: &NowPlaying, page_url: &str, resume_at: Option<f64>, user_id: i64) {
        let audio = matches!(np.kind, MediaKind::Audio);
        let known_short = np.duration_seconds.is_some_and(|d| d > 0.0 && d < RESUME_MIN_DURATION_S);
        let target = match &np.playlist {
            Some(p) => {
                let guard = self.inner.playlist.lk();
                guard.as_ref().and_then(|pl| pl.entries.get(p.index)).map(|e| {
                    let series_title = guard.as_ref().and_then(|pl| pl.series_title.clone());
                    (pl_key(&guard), e.id.clone(), e.title.clone(), series_title.unwrap_or_default())
                })
            }
            None if audio && known_short => None,
            None => Some((
                single_key(page_url),
                single_episode_id(page_url),
                np.title.clone(),
                np.title.clone(),
            )),
        };
        let Some((key, episode_id, title, series_title)) = target else {
            *self.inner.progress.lk() = None;
            return;
        };
        if let Err(e) = self.inner.store.media_progress.set_episode(
            &key,
            &episode_id,
            &title,
            &series_title,
            resume_at.unwrap_or(0.0),
            Some(user_id),
        ) {
            tracing::warn!("续播进度落不了盘(不影响播放): {e:#}");
        }
        *self.inner.progress.lk() =
            Some(ProgressTarget { key, episode_id, title: np.title.clone() });
        *self.inner.progress_at.lk() = Some(std::time::Instant::now());
    }

    /// 前端心跳 / 暂停 / 停止 → 集内位置落盘(节拍 PROGRESS_PERSIST_EVERY;暂停 / 停止立刻)。
    /// 只认标题对得上的回报(切集瞬间迟到的旧心跳不许写进新一集);短内容不记;
    /// 停播后清身份。`finished` = 播到片尾区,下次从下一集开头起。
    pub(super) fn persist_progress(&self, r: &PlaybackReport) {
        let Some(target) = self.inner.progress.lk().clone() else { return };
        let idle = r.status == "idle";
        if !idle && r.title.as_deref() != Some(target.title.as_str()) {
            return; // 别的内容的回报(切集 / 换片竞态),不是这一行的
        }
        let Some(pos) = r.position.filter(|p| p.is_finite() && *p >= 0.0) else {
            if idle {
                *self.inner.progress.lk() = None;
            }
            return;
        };
        let dur = r.duration.filter(|d| d.is_finite() && *d > 0.0).unwrap_or(0.0);
        let immediate = idle || r.status == "paused";
        if !immediate {
            let mut at = self.inner.progress_at.lk();
            if at.is_some_and(|t| t.elapsed() < PROGRESS_PERSIST_EVERY) {
                return;
            }
            *at = Some(std::time::Instant::now());
        }
        // 短内容(歌 / 短片)不记集内位置:重听从头是常识,记了反而怪 —— 但「看完了」照记:
        // 5–12 分钟一集的儿童动画正是这类,原先整段早退连 `finished` 也不写,末集看完再「接着看」
        // 回的还是末集、走不到「整部从头」(2026-09-16 体检修)。位置写 0 = 不续播;片尾判定照
        // RESUME_TAIL_S、但不超过一半时长(两分钟的短片别刚开头就算看完)。
        let short = dur > 0.0 && dur < RESUME_MIN_DURATION_S;
        let tail = if short { RESUME_TAIL_S.min(dur / 2.0) } else { RESUME_TAIL_S };
        let finished = dur > 0.0 && pos >= dur - tail;
        let stored_pos = if short { 0.0 } else { pos };
        if let Err(e) = self.inner.store.media_progress.set_position(
            &target.key,
            &target.episode_id,
            stored_pos,
            dur,
            finished,
        ) {
            tracing::warn!("集内进度落不了盘: {e:#}");
        }
        if idle {
            *self.inner.progress.lk() = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::testkit::*;

    /// 短于 10 分钟的集:位置不记(重看从头是常识)、但**看完了要记**(2026-09-16 体检修):原先整段早退,
    /// 末集看完再「接着看」回的还是末集;片尾判定按 RESUME_TAIL_S 但不超过一半时长。
    #[tokio::test]
    async fn short_episode_records_finished_but_not_position() {
        let (rt, _rx) = runtime("short-finished");
        let mp = &rt.inner.store.media_progress;
        let arm = |ep: &str| {
            mp.set_episode("local:short", ep, ep, "短剧", 0.0, Some(1)).unwrap();
            *rt.inner.progress.lk() = Some(ProgressTarget {
                key: "local:short".into(),
                episode_id: ep.into(),
                title: ep.into(),
            });
            *rt.inner.progress_at.lk() = None;
        };
        let report = |pos: f64, dur: f64, ep: &str| PlaybackReport {
            status: "paused".into(),
            title: Some(ep.into()),
            position: Some(pos),
            duration: Some(dur),
            ..Default::default()
        };
        // 5 分钟一集,播到 2:00 暂停:位置不记(0)、没看完
        arm("ep1");
        rt.persist_progress(&report(120.0, 300.0, "ep1"));
        let p = mp.get("local:short").unwrap().unwrap();
        assert_eq!((p.position_seconds, p.finished), (0.0, false));
        // 播到 4:40(距结尾 20s):看完了,位置仍不记
        rt.persist_progress(&report(280.0, 300.0, "ep1"));
        let p = mp.get("local:short").unwrap().unwrap();
        assert_eq!((p.position_seconds, p.finished), (0.0, true), "短集看完要记 finished");
        // 2 分钟短片播到 0:40:片尾闸夹到一半时长(60s),还不算看完;1:10 才算
        arm("ep2");
        rt.persist_progress(&report(40.0, 120.0, "ep2"));
        assert!(!mp.get("local:short").unwrap().unwrap().finished, "刚开头不能算看完");
        rt.persist_progress(&report(70.0, 120.0, "ep2"));
        assert!(mp.get("local:short").unwrap().unwrap().finished);
    }

    /// 单个长音频也续(2026-09-16 ★ 用户拍板「短于 10 分钟的音频才算歌」):有声书 / 评书听一半明天接着;
    /// 歌听一半再放照样从头(读侧时长闸 + 写侧位置写 0)。
    #[tokio::test]
    async fn single_long_audio_resumes_but_songs_do_not() {
        let (rt, _rx) = runtime("audio-single-resume");
        let np = |o: PlayOutcome| match o {
            PlayOutcome::Playing(np) => np,
            other => panic!("应为 Playing,实际 {other:?}"),
        };
        let paused = |title: &str, pos: f64, dur: f64| PlaybackReport {
            status: "paused".into(),
            title: Some(title.into()),
            position: Some(pos),
            duration: Some(dur),
            ..Default::default()
        };
        // 有声书(单文件、所在文件夹只有它 → 不成队列):30 分钟长,听到 10 分钟处暂停
        let d1 = std::env::temp_dir().join(format!("lw-audiobook-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d1);
        std::fs::create_dir_all(&d1).unwrap();
        let book = touch(&d1, "评书.m4b").to_string_lossy().to_string();
        let first = np(rt.play(1, &book, true, false, None).await.unwrap());
        assert!(first.playlist.is_none() && first.resume_at.is_none());
        rt.set_playback(paused(&first.title, 600.0, 1800.0));
        assert_eq!(np(rt.play(1, &book, true, false, None).await.unwrap()).resume_at, Some(600.0), "长音频接着上次");
        assert!(np(rt.play(1, &book, true, true, None).await.unwrap()).resume_at.is_none(), "「从头听」不续");
        // 歌(3 分钟):听到一半停,再放照样从头
        let d2 = std::env::temp_dir().join(format!("lw-song-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d2);
        std::fs::create_dir_all(&d2).unwrap();
        let song = touch(&d2, "歌.mp3").to_string_lossy().to_string();
        let s = np(rt.play(1, &song, true, false, None).await.unwrap());
        rt.set_playback(paused(&s.title, 100.0, 200.0));
        assert!(np(rt.play(1, &song, true, false, None).await.unwrap()).resume_at.is_none(), "歌不续");
    }

    /// 集内续播(2026-09-07):前端心跳 / 暂停落「第几秒」,再放接着那一秒;播到片尾区 = 看完 →
    /// 下次从下一集开头;末集看完 = 整部从头;别的内容的迟到心跳写不进;电影按文件身份同样续。
    #[tokio::test]
    async fn position_resume_finished_and_movie_progress() {
        let (rt, _rx) = runtime("posresume");
        let dir = std::env::temp_dir().join(format!("lw-pos-resume-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mk = |n: &str| {
            let f = dir.join(n);
            std::fs::write(&f, b"x").unwrap();
            f.to_string_lossy().to_string()
        };
        let e1 = mk("剧 第1集.mp4");
        let _e2 = mk("剧 第2集.mp4");
        let _e3 = mk("剧 第3集.mp4");
        let np_of = |o: PlayOutcome| match o {
            PlayOutcome::Playing(np) => np,
            other => panic!("应为 Playing,实际 {other:?}"),
        };
        let report = |title: &str, status: &str, pos: f64, dur: f64| PlaybackReport {
            status: status.into(),
            title: Some(title.into()),
            position: Some(pos),
            duration: Some(dur),
            ..PlaybackReport::default()
        };

        let np = np_of(rt.play(1, &e1, false, false, None).await.unwrap());
        assert_eq!(np.playlist.unwrap().index, 0);
        // 暂停在 700s(总长 1400)→ 立刻落盘;再放 = 第1集 700s 接着放
        rt.set_playback(report(&np.title, "paused", 700.0, 1400.0));
        let np = np_of(rt.play(1, &e1, false, false, None).await.unwrap());
        let p = np.playlist.unwrap();
        assert_eq!((p.index, p.resumed, np.resume_at), (0, true, Some(700.0)));
        // 别的内容的心跳(换片 / 切集竞态)写不进这一行
        rt.set_playback(report("别的片", "playing", 900.0, 1400.0));
        assert_eq!(
            rt.inner.store.media_progress.get(&pl_key(&rt.inner.playlist.lk())).unwrap().unwrap().position_seconds,
            700.0
        );
        // 播到片尾区(1350/1400)= 看完 → 再放 = 第2集从头
        rt.set_playback(report(&np.title, "paused", 1350.0, 1400.0));
        let np = np_of(rt.play(1, &e1, false, false, None).await.unwrap());
        let p = np.playlist.unwrap();
        assert_eq!((p.index, p.resumed, np.resume_at), (1, true, None), "上集看完接下一集");
        // 看了不到 30 秒不算(下次仍从头);短内容(歌)不记
        rt.set_playback(report(&np.title, "paused", 12.0, 1400.0));
        assert_eq!(np_of(rt.play(1, &e1, false, false, None).await.unwrap()).resume_at, None);
        // 末集看完 → 整部从头,不算「接着上次」
        let np = np_of(rt.play(1, &e1, false, false, Some(3)).await.unwrap());
        rt.set_playback(report(&np.title, "idle", 1390.0, 1400.0));
        let np = np_of(rt.play(1, &e1, false, false, None).await.unwrap());
        let p = np.playlist.unwrap();
        assert_eq!((p.index, p.resumed, np.resume_at), (0, false, None), "整部看完从头");

        // 单部电影:按文件身份续;restart 忽略;放歌(audio_only)不记
        let movie = mk("某电影.mp4");
        let np = np_of(rt.play(1, &movie, false, false, None).await.unwrap());
        assert!(np.playlist.is_none());
        rt.set_playback(report(&np.title, "paused", 2500.0, 6000.0));
        assert_eq!(np_of(rt.play(1, &movie, false, false, None).await.unwrap()).resume_at, Some(2500.0));
        assert_eq!(np_of(rt.play(1, &movie, false, true, None).await.unwrap()).resume_at, None, "从头看");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
