//! 嘴控 / 按钮 / 快捷键汇同一执行口,以及前端播放态的镜像(`NowPlaying` 全量)。
//!
//! 校验收口 core:相对量(「再大一点」)不做增量动作 —— 模型从「此刻」背景自己算
//! 绝对值。倍速 / 播放模式 / 音轨 / 字幕都是**队列级粘住**:新点播复位、切集沿用。

use super::*;

/// 一条音轨的短描述(喂模型/观察文本):标题 > 语言码 > 「音轨N」。
fn track_desc(t: &probe::AudioTrack, n: usize) -> String {
    match (&t.title, &t.lang) {
        (Some(ti), _) => ti.clone(),
        (None, Some(l)) => l.clone(),
        _ => format!("音轨{n}"),
    }
}

/// 音轨清单话术:`1=chi 2=eng`(〔此刻〕与报错共用;模型按语言挑轨号)。
fn track_menu(tracks: &[probe::AudioTrack]) -> String {
    tracks
        .iter()
        .enumerate()
        .map(|(i, t)| format!("{}={}", i + 1, track_desc(t, i + 1)))
        .collect::<Vec<_>>()
        .join(" ")
}

/// 秒 → 「M:SS」/「H:MM:SS」钟表格式(喂模型的进度表示;比裸秒数好读好算)。
pub(super) fn fmt_clock(secs: f64) -> String {
    let s = secs.max(0.0).round() as u64;
    let (h, m, sec) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{sec:02}")
    } else {
        format!("{m}:{sec:02}")
    }
}

impl MediaRuntime {
    /// 模型侧播放控制(用户用嘴说"暂停/大点声/倍速/跳到 90 秒");播放条的循环/随机按钮
    /// 经壳层命令也汇到这(同一校验/执行口)。speed/seek 带 value,其余不带;
    /// 词表和校验收口在这,前端只执行不判断。循环/随机先落 core 状态(auto_next/「此刻」背景
    /// 读它),再随 Control 事件让前端对齐 el.loop/按钮态。
    pub fn control(&self, action: &str, value: Option<f64>) -> Result<String> {
        match action {
            "pause" | "resume" | "stop" | "louder" | "softer" => {}
            // 片头 / 片尾手标(嘴控「片头到这里」/ 播放器菜单):落库 + 重算 + 广播 Skip,不发 Control
            "intro_start" | "intro_end" | "outro_start" => return self.mark_skip(action, value),
            "skip_clear" => return self.clear_skip_marks(),
            // 「跳过片头」= 定位到本集片头段尾(没有信息如实说;标一个就有)
            "skip_intro" => {
                let intro = self
                    .inner
                    .skip_ctx
                    .lk()
                    .as_ref()
                    .and_then(|c| c.current.as_ref())
                    .and_then(|s| s.intro);
                let Some(seg) = intro else {
                    anyhow::bail!("这一集还没有片头信息,可以放到片头结束时说「片头到这里」标一个");
                };
                self.publish(MediaEvent::Control { action: "seek".into(), value: Some(seg.end) });
                return Ok(format!("已跳过片头,从 {} 接着放", fmt_clock(seg.end)));
            }
            // 播放模式:五个动作名保留当嘴控词汇(模型按用户口语对号),落到一个 PlayMode 并**归一**:
            // 单曲上「循环放」= 循环这一首;「取消循环 / 别随机了」= 回该内容的默认(歌单 = 列表循环,
            // 单曲 / 视频 = 放完就停)。结果态经 `MediaEvent::Mode` 发给前端(不发 Control,前端不猜)。
            "loop_one" | "loop_all" | "loop_off" | "shuffle_on" | "shuffle_off" => {
                let mut guard = self.inner.playlist.lk();
                let (has_queue, audio_only) =
                    guard.as_ref().map(|pl| (true, pl.audio_only)).unwrap_or((false, false));
                let next = match action {
                    "loop_one" => PlayMode::LoopOne,
                    "loop_all" if has_queue => PlayMode::LoopAll,
                    "loop_all" => PlayMode::LoopOne,
                    "shuffle_on" => {
                        // 单曲/没在放列表:开随机没意义,如实退回(§3.5);关随机幂等、不吵。
                        anyhow::ensure!(has_queue, "现在没有在放多首的列表,没法随机播放");
                        PlayMode::Shuffle
                    }
                    _ => PlayMode::default_for(audio_only, has_queue),
                };
                if next == PlayMode::Shuffle {
                    if let Some(pl) = guard.as_mut() {
                        pl.played = vec![pl.index]; // 新一轮履历从当前这首起算
                    }
                }
                drop(guard);
                *self.inner.mode.lk() = next;
                self.publish(MediaEvent::Mode { mode: next.as_str().into() });
                return Ok("ok".into());
            }
            "volume" => {
                let v = value.context("volume 需要 value(0–100)")?;
                anyhow::ensure!((0.0..=100.0).contains(&v), "音量范围 0–100,收到 {v}");
            }
            "speed" => {
                let v = value.context("speed 需要 value(倍速)")?;
                anyhow::ensure!(
                    SPEED_RANGE.contains(&v),
                    "倍速范围 {}–{},收到 {v}",
                    SPEED_RANGE.start(),
                    SPEED_RANGE.end()
                );
                // 先落 core 状态(切集 / 自动续播的 NowPlaying 据此捎带),再随 Control 事件让前端对齐。
                *self.inner.rate.lk() = v;
            }
            "seek" => {
                let v = value.context("seek 需要 value(秒)")?;
                anyhow::ensure!(v >= 0.0, "定位秒数不能为负");
            }
            // P4 字幕:0 = 关,N = 第 N 条(1 起,与音轨同口径)。**纯显示层** —— 不重建管线、
            // 不动字节,故 core 只校验形状,选哪条由前端按 NowPlaying.subtitles 对号入座。
            "subtitle" => {
                let v = value.context("subtitle 需要 value(0=关,1=第一条字幕)")?;
                anyhow::ensure!(
                    v >= 0.0 && v.fract() == 0.0,
                    "字幕序号要整数(0=关,1 起),收到 {v}"
                );
            }
            other => anyhow::bail!(
                "未知动作 {other},可用: pause/resume/stop/louder/softer/volume/speed/seek/\
                 loop_one/loop_all/loop_off/shuffle_on/shuffle_off/audio_track/subtitle/\
                 intro_start/intro_end/outro_start/skip_intro/skip_clear"
            ),
        }
        self.publish(MediaEvent::Control { action: action.into(), value });
        Ok("ok".into())
    }

    /// 新一集 / 新文件的音轨选择,结果回写(状态别悬空):显式切过轨就按**语言**对号 ——
    /// 选中轨号在这个文件里若不再是那个语言,找第一条同语言的;没标语言的文件退回按轨号;
    /// 越界(新一集音轨数变少)回 0。没显式选过 = 纯钳位(老行为)。
    pub(super) fn pick_audio_track(&self, tracks: &[probe::AudioTrack]) -> usize {
        let total = tracks.len();
        let mut idx = self.inner.audio_track.lk();
        if let Some(lang) = self.inner.audio_track_lang.lk().as_deref() {
            let same = |t: &probe::AudioTrack| t.lang.as_deref() == Some(lang);
            if !tracks.get(*idx).is_some_and(same) {
                if let Some(j) = tracks.iter().position(same) {
                    *idx = j;
                }
            }
        }
        if *idx >= total.max(1) {
            *idx = 0;
        }
        *idx
    }

    /// 播放器「此刻」位置(秒):最近回报值 + 播放中按倍速外推(与 playback_summary 同口径)。
    pub(super) fn current_position(&self) -> Option<f64> {
        let pb = self.inner.playback.lk().clone();
        let mut cur = pb.position_secs?;
        if !pb.paused {
            if let Some(at) = pb.at {
                cur += at.elapsed().as_secs_f64() * pb.rate.unwrap_or(1.0);
            }
        }
        if let Some(d) = pb.duration_secs {
            cur = cur.min(d);
        }
        Some(cur.max(0.0))
    }

    /// 切音轨(n 从 1 数;嘴控 media_control 的 audio_track 与播放条按钮汇到同一口)。
    /// mac + 原生直传:发 Control 事件让前端 `audioTracks` 就地启停(无缝、不重载);
    /// 其余管线:按选中轨重建(ffmpeg `-map`),`NowPlaying.resume_at` 带上当前位置接着放。
    /// 返回给模型的观察文本(§3.5 各种没得切都如实说)。
    pub async fn set_audio_track(&self, n: usize) -> Result<String> {
        let Some(cur) = self.inner.current_local.lk().clone() else {
            anyhow::bail!("现在没有在放本地内容,切换不了音轨(网络流的音轨由来源决定)");
        };
        anyhow::ensure!(
            self.inner.playback.lk().title.is_some(),
            "现在没有在播放,切换不了音轨"
        );
        let total = cur.tracks.len();
        anyhow::ensure!(total >= 2, "这个文件没有可切换的音轨(读到 {total} 条)");
        anyhow::ensure!(
            (1..=total).contains(&n),
            "一共 {total} 条音轨({}),没有第 {n} 条",
            track_menu(&cur.tracks)
        );
        let idx = n - 1;
        let prev = *self.inner.audio_track.lk();
        if idx == prev {
            return Ok(format!("已经在第 {n} 条音轨({})了", track_desc(&cur.tracks[idx], n)));
        }
        let prev_lang = self.inner.audio_track_lang.lk().clone();
        *self.inner.audio_track.lk() = idx;
        // 记下语言码:切集时按语言对号(两集轨序不同也不串)。
        *self.inner.audio_track_lang.lk() = cur.tracks[idx].lang.clone();
        // 统一走「重建 + 原位续播」(mac 直传也一样):真机实锤 WKWebView **播放中**改
        // audioTracks.enabled 不重新路由音频(静音且切回也不恢复);loadedmetadata 时的收敛
        // 有效 → 重载后由起播收敛把新轨启起来,本地文件重载亚秒级,与 Windows 同一条路。
        let resume = self.current_position();
        let pos = self.inner.playlist.lk().as_ref().map(|pl| PlaylistPos {
            index: pl.index,
            total: pl.entries.len(),
            resumed: false,
        });
        if let Err(e) = self.play_local(&cur.page_url, cur.audio_only, pos, true, resume).await {
            // 重建失败:选择回滚(老管线还在播旧轨,状态别悬空指向没生效的轨)
            *self.inner.audio_track.lk() = prev;
            *self.inner.audio_track_lang.lk() = prev_lang;
            return Err(e);
        }
        Ok(format!(
            "已切到第 {n} 条音轨({}),从刚才的位置接着放",
            track_desc(&cur.tracks[idx], n)
        ))
    }

    /// 当前倍速(NowPlaying 镜像用;新点播复位 1,切集沿用)。
    pub(super) fn rate(&self) -> f64 {
        *self.inner.rate.lk()
    }

    /// 当前队列的整份清单(剧集列表面板 / 曲目列表按需取;None = 没在放多集内容)。
    pub fn playlist_view(&self) -> Option<PlaylistView> {
        let guard = self.inner.playlist.lk();
        guard.as_ref().map(|pl| PlaylistView {
            index: pl.index,
            title: pl.series_title.clone(),
            entries: pl
                .entries
                .iter()
                .map(|e| PlaylistEntryView { title: e.title.clone() })
                .collect(),
        })
    }

    /// 播放模式镜像(NowPlaying 每次捎带全量,前端以此对齐 el.loop/按钮态,零猜测)。
    pub(super) fn play_mode_str(&self) -> String {
        self.inner.mode.lk().as_str().to_string()
    }

    /// 起播时乐观 seed「正在放」(前端随后经 report 校准;这步只是让模型立刻就知道在放什么)。
    /// `pos` = (index, total):在播剧集时把「第N/共M集」一并记下,喂模型「此刻」背景。
    /// 音量跨播放粘住(前端基准如此)→ seed 保留旧值;进度/倍速是新内容的事,清零等回报。
    pub(super) fn seed_playing(&self, title: &str, pos: Option<(usize, usize)>) {
        let mut guard = self.inner.playback.lk();
        let volume_pct = guard.volume_pct;
        *guard = Playback {
            title: Some(title.to_string()),
            paused: false,
            pos,
            volume_pct,
            ..Playback::default()
        };
    }

    /// 前端回报播放态(`report_media_state` 命令 → 这里):前端是播放真相源,
    /// ended/stop/pause/resume/音量/倍速/seek/心跳全经此校准 core 快照。
    /// 集数位置(pos)由 core 起播/切集时 seed,前端回报不带 → 这里**保留**已有 pos;
    /// 音量也粘住(idle 不清,与前端「跨播放粘住」一致);其余 idle 清空。
    pub fn set_playback(&self, r: PlaybackReport) {
        {
            let mut guard = self.inner.playback.lk();
            let volume_pct =
                r.volume.map(|v| v.clamp(0.0, 100.0).round() as u8).or(guard.volume_pct);
            *guard = match r.status.as_str() {
                "idle" => Playback { volume_pct, ..Playback::default() },
                // paused 只认显式;playing / loading / 其它 → 正在播
                s => Playback {
                    title: r.title.clone(),
                    paused: s == "paused",
                    pos: guard.pos,
                    volume_pct,
                    position_secs: r.position,
                    duration_secs: r.duration.filter(|d| *d > 0.0),
                    rate: r.rate,
                    at: Some(std::time::Instant::now()),
                },
            };
        }
        // 同一条心跳顺手落集内进度(续播「第几秒」的唯一数据源;节拍与闸在 persist_progress)
        self.persist_progress(&r);
    }

    /// 「此刻」播放器状态的一行背景:回合装配追加到末条 user 喂模型,让它任何时候都拿得到
    /// 当下真相(修「歌放完了却以为还在播」)。总返回一行(含空闲),由提示词法条约束「只在
    /// 跟播放有关时才参考、平时别主动提」。在播剧集时带「第N/共M集」;有回报时带**进度/音量/
    /// 倍速** —— 模型据此才能「音量调到 50」「快进 5 分钟」(相对操作 = 自己按当前值算绝对值)。
    pub fn playback_summary(&self) -> Option<String> {
        let pb = self.inner.playback.lk().clone();
        // 剧集补一段「(第N集/共M集)」,让模型知道进度(如被问"放到哪了""下一集")。
        let ep = pb
            .pos
            .map(|(i, n)| format!("(第{}集/共{n}集)", i + 1))
            .unwrap_or_default();
        // 进度 = 最近回报值,播放中按倍速外推到「此刻」(回报之间也准);夹到总长。
        let progress = pb
            .position_secs
            .map(|p| {
                let mut cur = p;
                if !pb.paused {
                    if let Some(at) = pb.at {
                        cur += at.elapsed().as_secs_f64() * pb.rate.unwrap_or(1.0);
                    }
                }
                match pb.duration_secs {
                    Some(d) => format!(",进度 {}/{}", fmt_clock(cur.min(d)), fmt_clock(d)),
                    None => format!(",已播到 {}", fmt_clock(cur)),
                }
            })
            .unwrap_or_default();
        let vol = pb.volume_pct.map(|v| format!(",音量 {v}%")).unwrap_or_default();
        let rate = pb
            .rate
            .filter(|r| (*r - 1.0).abs() > 0.011)
            .map(|r| format!(",{r} 倍速"))
            .unwrap_or_default();
        // 多音轨清单:模型据此把「换英文/国语」对到轨号(media_control 的 audio_track)。
        let audio = {
            let sel = *self.inner.audio_track.lk();
            self.inner
                .current_local
                .lk()
                .as_ref()
                .filter(|c| c.tracks.len() >= 2)
                .map(|c| {
                    format!(",音轨 {}/{}(可选: {})", sel + 1, c.tracks.len(), track_menu(&c.tracks))
                })
                .unwrap_or_default()
        };
        // 播放模式:模型据此答「现在是循环吗」、对「别循环了/换随机」给对动作。
        let mode = self.inner.mode.lk().ambient();
        Some(match (pb.title, pb.paused) {
            (None, _) => "播放器现在空闲,没有在播放任何内容".to_string(),
            (Some(t), false) => format!("播放器正在播放《{t}》{ep}{progress}{vol}{rate}{mode}{audio}"),
            (Some(t), true) => format!("播放器已暂停,停在《{t}》{ep}{progress}{vol}{rate}{mode}{audio}"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::testkit::*;

    #[tokio::test]
    async fn control_validates_action_and_publishes() {
        let (rt, mut rx) = runtime("ctl");
        rt.control("pause", None).unwrap();
        rt.control("speed", Some(1.5)).unwrap();
        rt.control("seek", Some(90.0)).unwrap();
        rt.control("volume", Some(50.0)).unwrap();
        assert!(rt.control("blast_off", None).is_err(), "未知动作被拒");
        assert!(rt.control("speed", None).is_err(), "speed 缺 value 被拒");
        assert!(rt.control("speed", Some(9.0)).is_err(), "倍速超界被拒");
        assert!(rt.control("speed", Some(0.25)).is_err(), "0.25 在 Chromium 上无声,下限 0.5");
        assert_eq!(rt.rate(), 1.5, "倍速先落 core 状态(切集的 NowPlaying 据此捎带)");
        assert!(rt.control("seek", Some(-3.0)).is_err(), "负秒数被拒");
        assert!(rt.control("volume", None).is_err(), "volume 缺 value 被拒");
        assert!(rt.control("volume", Some(120.0)).is_err(), "音量超 0–100 被拒");
        match rx.try_recv().unwrap() {
            AppEvent::Media(MediaEvent::Control { action, value }) => {
                assert_eq!(action, "pause");
                assert!(value.is_none());
            }
            other => panic!("应是 Control,实际 {other:?}"),
        }
    }

    /// 倍速是队列级粘住:切集不经 `play()` 所以沿用;**新点播**(`play()`)一进门就复位 1 ——
    /// 这里拿一个空文件夹当点播入参(会如实退回,但复位发生在退回之前)。
    #[tokio::test]
    async fn speed_resets_on_new_play_request() {
        let (rt, _rx) = runtime("speed-reset");
        rt.control("speed", Some(2.0)).unwrap();
        assert_eq!(rt.rate(), 2.0);
        let empty = std::env::temp_dir().join(format!("lw-speed-reset-{}", std::process::id()));
        std::fs::create_dir_all(&empty).unwrap();
        assert!(rt.play(1, &empty.to_string_lossy(), false, false, None).await.is_err());
        assert_eq!(rt.rate(), 1.0, "新点播复位倍速");
        let _ = std::fs::remove_dir_all(&empty);
    }

    /// 切集选音轨:显式切过轨按语言对号(轨序变了也不串),没同语言按轨号,越界回 0,
    /// 没显式选过纯钳位。
    #[test]
    fn pick_audio_track_follows_language_then_index() {
        let (rt, _rx) = runtime("pick-track");
        let tr = |lang: Option<&str>| probe::AudioTrack {
            codec: "aac".into(),
            lang: lang.map(Into::into),
            title: None,
            channels: Some(2),
        };
        let set = |idx: usize, lang: Option<&str>| {
            *rt.inner.audio_track.lk() = idx;
            *rt.inner.audio_track_lang.lk() = lang.map(Into::into);
        };
        set(1, Some("eng"));
        assert_eq!(rt.pick_audio_track(&[tr(Some("eng")), tr(Some("chi"))]), 0, "轨序反了按语言");
        set(1, Some("eng"));
        assert_eq!(rt.pick_audio_track(&[tr(Some("chi")), tr(Some("jpn"))]), 1, "没同语言按轨号");
        set(1, Some("eng"));
        assert_eq!(rt.pick_audio_track(&[tr(Some("chi"))]), 0, "越界回 0");
        set(3, None);
        assert_eq!(rt.pick_audio_track(&[tr(None), tr(None)]), 0, "没显式选过纯钳位");
        assert_eq!(*rt.inner.audio_track.lk(), 0, "结果回写");
    }

    #[test]
    fn mode_controls_need_right_context_and_show_in_summary() {
        let (rt, _rx) = runtime("mode-ctl");
        assert!(rt.control("shuffle_on", None).is_err(), "没队列不能开随机");
        rt.control("shuffle_off", None).unwrap(); // 关随机幂等不吵
        rt.control("loop_one", None).unwrap(); // 循环不需要队列(单曲循环)
        rt.set_playback(PlaybackReport {
            status: "playing".into(),
            title: Some("小星星".into()),
            ..Default::default()
        });
        assert!(rt.playback_summary().unwrap().contains("单曲循环中"));
        // 单曲上「循环放」归一成单曲循环(没有列表可循环)
        rt.control("loop_all", None).unwrap();
        assert_eq!(*rt.inner.mode.lk(), PlayMode::LoopOne);
        assert!(rt.playback_summary().unwrap().contains("单曲循环中"));
        rt.control("loop_off", None).unwrap();
        assert!(!rt.playback_summary().unwrap().contains("循环中"));

        // 歌单:默认列表循环;「取消循环」回的是默认(列表循环),随机可开、关随机也回默认
        *rt.inner.playlist.lk() = Some(mk_shuffle_playlist(3));
        *rt.inner.mode.lk() = PlayMode::default_for(true, true);
        assert!(rt.playback_summary().unwrap().contains("列表循环中"));
        rt.control("shuffle_on", None).unwrap();
        assert!(rt.playback_summary().unwrap().contains("随机播放中"));
        assert_eq!(rt.inner.playlist.lk().as_ref().unwrap().played, vec![0], "履历从当前起算");
        rt.control("shuffle_off", None).unwrap();
        assert_eq!(*rt.inner.mode.lk(), PlayMode::LoopAll);
        rt.control("loop_one", None).unwrap();
        rt.control("loop_off", None).unwrap();
        assert_eq!(*rt.inner.mode.lk(), PlayMode::LoopAll, "loop_off 回歌单默认");
        // 视频剧集的默认是放完就停
        assert_eq!(PlayMode::default_for(false, true), PlayMode::Once);
        assert_eq!(PlayMode::default_for(true, false), PlayMode::Once);
    }

    #[tokio::test]
    async fn set_audio_track_validation_and_rollback() {
        let (rt, mut rx) = runtime("atrack");
        assert!(rt.set_audio_track(2).await.is_err(), "没在放本地内容如实退回");
        // 注入现场:直传双音轨(chi/eng)
        *rt.inner.current_local.lk() = Some(CurrentLocal {
            page_url: "/x/双语片.mp4".into(),
            audio_only: false,
            tracks: vec![
                probe::AudioTrack { codec: "ac-3".into(), lang: Some("chi".into()), title: None, channels: Some(6) },
                probe::AudioTrack { codec: "ac-3".into(), lang: Some("eng".into()), title: None, channels: Some(2) },
            ],
        });
        assert!(rt.set_audio_track(2).await.is_err(), "没在播放(playback 空闲)也退回");
        rt.set_playback(PlaybackReport {
            status: "playing".into(),
            title: Some("双语片".into()),
            ..Default::default()
        });
        let err = rt.set_audio_track(9).await.unwrap_err().to_string();
        assert!(err.contains("一共 2 条"), "越界报错列清单: {err}");
        assert!(rt.set_audio_track(1).await.unwrap().contains("已经在"), "同轨幂等");
        // 「此刻」背景带音轨清单
        assert!(rt.playback_summary().unwrap().contains("音轨 1/2"), "{:?}", rt.playback_summary());
        // 切轨一律重建管线(mac 直传也不例外 —— WKWebView 播放中启停音轨不重路由,真机实锤);
        // 这里文件不存在 → 如实报错且选择回滚(真切换在 e2e/真机验)
        let _ = &mut rx;
        assert!(rt.set_audio_track(2).await.is_err());
        assert_eq!(*rt.inner.audio_track.lk(), 0, "失败回滚选择");
    }

    #[test]
    fn playback_snapshot_seed_report_and_summary() {
        let (rt, _rx) = runtime("playback");
        // 初始空闲(回合装配会据此告诉模型「没在播」)
        assert_eq!(
            rt.playback_summary().as_deref(),
            Some("播放器现在空闲,没有在播放任何内容")
        );
        // 起播乐观 seed → 正在播(单集:无集数)
        rt.seed_playing("天空之城", None);
        assert_eq!(rt.playback_summary().as_deref(), Some("播放器正在播放《天空之城》"));
        // 前端回报暂停(保留集数位置,这里无)
        rt.set_playback(report("paused", Some("天空之城")));
        assert_eq!(rt.playback_summary().as_deref(), Some("播放器已暂停,停在《天空之城》"));

        // 剧集:seed 带「第N/共M集」,且前端回报 playing 不丢集数位置
        rt.seed_playing("海底小纵队", Some((2, 12)));
        assert_eq!(
            rt.playback_summary().as_deref(),
            Some("播放器正在播放《海底小纵队》(第3集/共12集)")
        );
        rt.set_playback(report("playing", Some("海底小纵队")));
        assert_eq!(
            rt.playback_summary().as_deref(),
            Some("播放器正在播放《海底小纵队》(第3集/共12集)"),
            "前端回报不带集数 → 保留 seed 的位置"
        );

        // 前端回报 ended/stop → 空闲(修「歌放完了模型却以为还在播」),集数一并清
        rt.set_playback(report("idle", None));
        assert_eq!(
            rt.playback_summary().as_deref(),
            Some("播放器现在空闲,没有在播放任何内容")
        );
    }

    /// 富回报(音量/进度/倍速)进「此刻」摘要;暂停不外推、播放外推;音量跨播放/空闲粘住。
    #[test]
    fn playback_summary_carries_volume_progress_and_rate() {
        let (rt, _rx) = runtime("playback-rich");
        // 暂停态:进度用回报原值(不外推),音量/倍速如实标注
        rt.set_playback(PlaybackReport {
            status: "paused".into(),
            title: Some("天空之城".into()),
            volume: Some(40.0),
            position: Some(83.0),
            duration: Some(7083.0),
            rate: Some(1.5),
        });
        assert_eq!(
            rt.playback_summary().as_deref(),
            Some("播放器已暂停,停在《天空之城》,进度 1:23/1:58:03,音量 40%,1.5 倍速")
        );
        // 播放态:外推(elapsed≈0,仍是 1:23 量级);1 倍速不标注
        rt.set_playback(PlaybackReport {
            status: "playing".into(),
            title: Some("天空之城".into()),
            volume: Some(40.0),
            position: Some(83.0),
            duration: Some(7083.0),
            rate: Some(1.0),
        });
        let s = rt.playback_summary().unwrap();
        assert!(s.contains("进度 1:23/1:58:03"), "刚回报完外推≈0: {s}");
        assert!(s.contains("音量 40%") && !s.contains("倍速"), "1 倍速不标注: {s}");
        // 音量粘住:idle 清进度不清音量;下次 seed(新播放)也保留
        rt.set_playback(report("idle", None));
        assert_eq!(
            rt.playback_summary().as_deref(),
            Some("播放器现在空闲,没有在播放任何内容")
        );
        rt.seed_playing("新歌", None);
        assert_eq!(
            rt.playback_summary().as_deref(),
            Some("播放器正在播放《新歌》,音量 40%"),
            "音量跨播放粘住(前端基准语义),进度等回报"
        );
    }

    #[test]
    fn fmt_clock_formats() {
        assert_eq!(fmt_clock(0.0), "0:00");
        assert_eq!(fmt_clock(83.4), "1:23");
        assert_eq!(fmt_clock(3600.0), "1:00:00");
        assert_eq!(fmt_clock(7083.0), "1:58:03");
        assert_eq!(fmt_clock(-5.0), "0:00", "负数夹到 0");
    }
}
