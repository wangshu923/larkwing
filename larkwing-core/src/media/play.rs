//! 点播与队列推进:一次 `play()` 从哪起、放完了下一个是谁。
//!
//! 队列机器**来源无关**(`Playlist` 是 app 级瞬态、派生可丢);剧集发现分三路
//! (B 站 UGC 合集 / 番剧 PGC / 本地同文件夹),成队规则在 `queue.rs`。

use super::progress::resume_position;
use super::queue::{audio_folder_files, is_dir_path, local_episodes, shuffle_advance, shuffle_back, shuffle_seed, single_key};
use super::*;

/// 切集目标:相对挪一格(上/下一集)或第 N 集绝对定位(1 起数;嘴控「看第五集」)。
#[derive(Debug, Clone, Copy)]
enum EpisodeTarget {
    Delta(i32),
    Nth(usize),
}

impl MediaRuntime {
    /// 播放(用户发起):先**发现剧集队列**(B 站合集/分P → view API;本地 → 同文件夹扫描),
    /// 套用**续播规则**定起播集(见 `build_queue`),再把那一集交给 `play_entry` 现取现播。
    /// `restart=true`(用户说「从头/重新看」)= 忽略续播存档、从第一集起;`episode=Some(N)`(用户点名
    /// 「看第五集」)= 放第 N 集。单个内容(电影/单曲):队列为空,电影按文件身份续播集内位置。
    /// 错误向上抛(工具层转成喂模型的观察)。
    pub async fn play(
        &self,
        user_id: i64,
        page_url: &str,
        audio_only: bool,
        restart: bool,
        episode: Option<usize>,
    ) -> Result<PlayOutcome> {
        self.prefetch_ffmpeg(); // 后台预取(首次播放任何媒体即触发),不阻塞本次播放
        // 新播放请求 = 新内容意图:播放模式/音轨/倍速都复位;切集不经这里 —— 三者跨集粘住
        // (2026-09-07 用户实锤「1.5 倍看剧下一集变 1.0」:复位口径是「新点播」,不是「新一集」)。
        // 模式先归零,队列建好后再按内容定默认(歌单 = 列表循环,见 PlayMode::default_for)。
        *self.inner.mode.lk() = PlayMode::Once;
        *self.inner.audio_track.lk() = 0;
        *self.inner.audio_track_lang.lk() = None;
        *self.inner.rate.lk() = 1.0;
        // 目录入参 = 音频文件夹:强制只出声;≥2 首由 build_queue 组队连播,恰 1 首退化成放
        // 那一首,一首没有如实退回(播放链吃不了目录,绝不喂它;§3.5 不静默)。
        let single_fallback;
        let (page_url, audio_only) = if is_dir_path(page_url) {
            let dir = std::path::Path::new(page_url);
            // 扫盘挪 spawn_blocking(同 1700 行的 sniff_container 口径):NAS/SMB 上几千文件的
            // 文件夹 read_dir 是秒级阻塞,不该压在 tokio worker 上(§ 效率审计 2026-09-08)。
            let files = {
                let d = dir.to_path_buf();
                tokio::task::spawn_blocking(move || audio_folder_files(&d))
                    .await
                    .unwrap_or_default()
            };
            match files.len() {
                0 => anyhow::bail!("这个文件夹里没有能播放的音频文件"),
                1 => {
                    single_fallback = dir.join(&files[0]).to_string_lossy().into_owned();
                    (single_fallback.as_str(), true)
                }
                _ => (page_url, true),
            }
        } else {
            (page_url, audio_only)
        };
        let (pos, target, resume_at) =
            self.build_queue(page_url, audio_only, restart, episode).await?;
        *self.inner.mode.lk() = PlayMode::default_for(audio_only, pos.is_some());
        self.play_entry(user_id, &target, audio_only, pos, resume_at).await
    }

    /// 发现并装配剧集队列,返回 `(起播集的队列位置, 该集可播地址, 集内续播位)`。
    ///
    /// **续播规则(2026-09-07 改)**:传进来的路径 / 链接**只用来认剧,不再从「传的是第几集」推断意图**
    /// —— 模型习惯拿上次用过的某一集路径当整部剧的把手,老规则把它当「点名要看那集」,进度都不查
    /// (真机「昨晚看到 4 集今天回到 2 集」的病灶)。现在:
    ///   · `episode=Some(N)`(用户点名)→ 第 N 集从头,不查进度;越界如实退回;
    ///   · `restart` → 第一集从头;
    ///   · 否则有进度就接上:停在第 i 集 → 第 i 集 + 集内位置(够长才续,`resume_position`);
    ///     第 i 集已看完 → 第 i+1 集从头;末集看完 = 整部从头(resumed=false);
    ///   · 没进度(第一次看)→ 传的那个文件所在的集,匹配不上 → 第一集。
    /// 单集 / 发现失败 → 清队列;电影按文件身份查集内位置。决策落一行 info 日志(排查续播问题靠它)。
    async fn build_queue(
        &self,
        page_url: &str,
        audio_only: bool,
        restart: bool,
        episode: Option<usize>,
    ) -> Result<(Option<PlaylistPos>, String, Option<f64>)> {
        let discovered = if is_local_path(page_url) {
            // 同上:整个文件夹的 read_dir + 自然排序进阻塞线程池,失败(任务没了)= 当没发现剧集,
            // 与 read_dir 读不了时的现行退化路一致(单集播放)。
            let p = std::path::PathBuf::from(page_url);
            tokio::task::spawn_blocking(move || local_episodes(&p)).await.unwrap_or(None)
        } else if let Some(source) = self.source_of_url(page_url) {
            let cookie =
                cookies::load(&self.inner.store, source.id()).map(|c| cookies::header_value(&c));
            match source.episodes(page_url, cookie.as_deref()).await {
                Ok(d) => d,
                Err(e) => {
                    tracing::info!("剧集发现失败,按单集处理: {e:#}");
                    None
                }
            }
        } else {
            None
        };

        // 不成系列(单集 / 发现失败 / <2 集)→ 清队列,退化成单集播放;电影按文件身份续播集内位置。
        let Some(Series { key, title: series_title, entries }) =
            discovered.filter(|s| s.entries.len() >= 2)
        else {
            *self.inner.playlist.lk() = None;
            anyhow::ensure!(episode.is_none(), "这不是多集内容,没有「第几集」可选");
            let resume_at = if restart || audio_only {
                None
            } else {
                let key = single_key(page_url);
                let prog = self.inner.store.media_progress.get(&key).ok().flatten();
                let at = prog.as_ref().and_then(resume_position);
                tracing::info!(
                    key = %key,
                    stored = ?prog.as_ref().map(|p| (p.position_seconds, p.duration_seconds, p.finished)),
                    resume_at = ?at,
                    "续播决策(单部)"
                );
                at
            };
            return Ok((None, page_url.to_string(), resume_at));
        };

        let total = entries.len();
        // requested = 传进来的那个文件 / 链接落在队列的第几集(本地按绝对路径、B 站按 page_url 精确匹配;
        // 分P 的 P1 用裸 bvid url 对齐)—— 只在**没有进度**时当起点用。
        let requested = entries.iter().position(|e| e.url == page_url);
        let prog = self.inner.store.media_progress.get(&key).ok().flatten();
        let stored = match &prog {
            Some(p) => entries.iter().position(|e| e.id == p.episode_id).map(|i| (i, p)),
            None => None,
        };
        let (index, resumed, resume_at) = match (episode, restart, stored) {
            (Some(n), _, _) => {
                anyhow::ensure!((1..=total).contains(&n), "这部一共 {total} 集,没有第 {n} 集");
                (n - 1, false, None)
            }
            (None, true, _) => (0, false, None),
            (None, false, Some((i, p))) if p.finished => {
                // 上次那集看完了:接下一集;末集看完 = 整部看完,从头(不算「接着上次」)
                if i + 1 < total {
                    (i + 1, true, None)
                } else {
                    (0, false, None)
                }
            }
            (None, false, Some((i, p))) => {
                let at = resume_position(p);
                (i, i != 0 || at.is_some(), at)
            }
            (None, false, None) => (requested.unwrap_or(0), false, None),
        };
        tracing::info!(
            key = %key,
            requested = ?requested,
            stored = ?prog.as_ref().map(|p| (p.episode_id.as_str(), p.position_seconds, p.finished)),
            episode,
            restart,
            chosen = index,
            resume_at = ?resume_at,
            "续播决策"
        );
        let target = entries[index].url.clone();
        *self.inner.playlist.lk() = Some(Playlist {
            series_key: key,
            series_title,
            entries,
            index,
            audio_only,
            played: Vec::new(),
        });
        Ok((Some(PlaylistPos { index, total, resumed }), target, resume_at))
    }

    /// 上/下一集(嘴控「下一集」、播放器按钮、`ended` 自动续播都汇到这):在**现有队列**里挪
    /// `delta`(±1),把那一集现取现播(不重建队列、流地址永不过期)。越界 = 报错喂回(到头/到顶)。
    pub async fn advance(&self, user_id: i64, delta: i32) -> Result<PlayOutcome> {
        self.switch_episode(user_id, EpisodeTarget::Delta(delta)).await
    }

    /// 跳到第 N 集(1 起数;嘴控「看第五集」):队列是 core 的单一真相(B 站合集/分P、本地剧集
    /// 同一套 entries,「第N/共M集」的 N/M 就来自它)—— 模型只说集数,不需要也不可能自己拼链接。
    /// 越界/没在放剧集 = 报错喂回;跳到当前集 = 从头重放该集(合「再放一遍这集」的口语义)。
    pub async fn jump_to_episode(&self, user_id: i64, episode: usize) -> Result<PlayOutcome> {
        self.switch_episode(user_id, EpisodeTarget::Nth(episode)).await
    }

    /// 切集共用体(相对挪 / 第 N 集绝对定位):算目标 index → 越界报错 → 切集即落续播进度 →
    /// 那一集现取现播(不重建队列、流地址永不过期)。
    async fn switch_episode(&self, user_id: i64, target: EpisodeTarget) -> Result<PlayOutcome> {
        let mode = *self.inner.mode.lk();
        let (target_url, audio_only, pos) = {
            let mut guard = self.inner.playlist.lk();
            let pl = guard
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("现在没有在播放剧集,没有可切换的集"))?;
            let total = pl.entries.len();
            let new = match target {
                // 随机播放中的「下一首」= 这轮没放过的里随机挑(用户点名要,放完一轮也接着挑,
                // 恒有下一首);「上一首」= 沿履历回退(现场心智 = 回到刚才那首)。
                EpisodeTarget::Delta(d) if mode == PlayMode::Shuffle => {
                    if d > 0 {
                        shuffle_advance(pl, true, shuffle_seed()).expect("wrap=true 恒有下一首")
                    } else {
                        shuffle_back(pl)
                            .ok_or_else(|| anyhow::anyhow!("随机播放刚开始,还没有可回退的上一首"))?
                    }
                }
                EpisodeTarget::Delta(d) => {
                    let n = pl.index as i32 + d;
                    if mode.wraps() {
                        // 列表循环 / 单曲循环:列表是环,到头/到顶都回卷(开着循环点「下一首」不该被「已是最后」拦下)
                        n.rem_euclid(total as i32) as usize
                    } else {
                        anyhow::ensure!(n >= 0, "已经是第一集了");
                        anyhow::ensure!((n as usize) < total, "已经是最后一集了,整季都放完啦");
                        n as usize
                    }
                }
                EpisodeTarget::Nth(n) => {
                    anyhow::ensure!(
                        (1..=total).contains(&n),
                        "这部一共 {total} 集,没有第 {n} 集"
                    );
                    let idx = n - 1;
                    // 点名跳集也记进随机履历(「上一首」能回来;auto_next 已推过的恰在末位,不重复)。
                    if mode == PlayMode::Shuffle && pl.played.last() != Some(&idx) {
                        pl.played.push(idx);
                    }
                    idx
                }
            };
            pl.index = new;
            let e = &pl.entries[pl.index];
            // 进度由 play_entry 起播成功时落(切集失败 = 进度停在原来那集,如实)。
            (
                e.url.clone(),
                pl.audio_only,
                PlaylistPos { index: pl.index, total, resumed: false },
            )
        };
        self.play_entry(user_id, &target_url, audio_only, Some(pos), None).await
    }

    /// 一集自然放完(前端 `ended` 的唯一 core 入口):按播放模式决定接下来放什么。
    /// Some = core 已接管(切下一首现取现播,Play 事件接力);None = 没有下一首,前端正常收尾。
    /// 只服务自动续播路;用户嘴控 next/prev 仍走 `advance`(放完就停时到头报错的反馈是对的)。
    /// (单曲循环由前端 `el.loop` 原生循环,ended 压根不触发;真走到这〔视频片尾倒计时〕= 重放本集。)
    pub async fn auto_next(&self, user_id: i64) -> Result<Option<PlayOutcome>> {
        let mode = *self.inner.mode.lk();
        let target = {
            let mut guard = self.inner.playlist.lk();
            let Some(pl) = guard.as_mut() else { return Ok(None) }; // 单集:交回前端收尾
            match mode {
                // 随机:这轮没放过的里挑;都放过 → 重开一轮(随机恒循环不停)。
                PlayMode::Shuffle => shuffle_advance(pl, true, shuffle_seed()),
                PlayMode::LoopOne => Some(pl.index),
                _ if pl.index + 1 < pl.entries.len() => Some(pl.index + 1),
                PlayMode::LoopAll => Some(0), // 列表循环:末集放完回卷到第一集
                PlayMode::Once => None,
            }
        };
        match target {
            Some(i) => self.switch_episode(user_id, EpisodeTarget::Nth(i + 1)).await.map(Some),
            None => Ok(None),
        }
    }

    /// 放**一集**(队列已定;不碰队列):本地直走文件端点,网络走 yt-dlp 解析 → 注册转发。
    /// `pos` = 这一集在队列里的位置(None = 单集),会写进 `NowPlaying.playlist`;`resume_at` = 集内续播位
    /// (前端加载完 seek 过去)。`play`/`advance` 共用。起播成功即登记续播身份 + 落「现在放到哪一集」。
    async fn play_entry(
        &self,
        user_id: i64,
        page_url: &str,
        audio_only: bool,
        pos: Option<PlaylistPos>,
        resume_at: Option<f64>,
    ) -> Result<PlayOutcome> {
        let outcome = self.play_entry_inner(user_id, page_url, audio_only, pos, resume_at).await?;
        if let PlayOutcome::Playing(np) = &outcome {
            self.track_progress(np, page_url, resume_at, user_id);
        }
        Ok(outcome)
    }

    async fn play_entry_inner(
        &self,
        user_id: i64,
        page_url: &str,
        audio_only: bool,
        pos: Option<PlaylistPos>,
        resume_at: Option<f64>,
    ) -> Result<PlayOutcome> {
        if is_local_path(page_url) {
            return self
                .play_local(page_url, audio_only, pos, true, resume_at)
                .await
                .map(PlayOutcome::Playing);
        }
        let ytdlp = self.ensure_component(Component::YtDlp).await?;

        let source = self.source_of_url(page_url).cloned();
        let source_id = source.as_ref().map(|s| s.id().to_string());
        let cookie_recs =
            source_id.as_deref().and_then(|id| cookies::load(&self.inner.store, id));
        let cookies_file = match (&source_id, &cookie_recs) {
            (Some(id), Some(recs)) => Some(cookies::export_file(&self.inner.dir, id, recs).await?),
            _ => None,
        };

        // 进度条预览的雪碧图:**与 yt-dlp 解析并行**去问源要(见 SPRITE_FETCH_TIMEOUT),等解析
        // 回来时多半已经在手。拿不到(源没实现 / 接口不顺 / 超时)= None = 前端只出时间气泡。
        // 放歌没画面,不问。解析半路失败时这个任务被丢下也无妨:自带超时,几秒内自行收尾。
        let sprite_task = match (&source, audio_only) {
            (Some(src), false) => {
                let src = src.clone();
                let url = page_url.to_string();
                let cookie = cookie_recs.as_deref().map(cookies::header_value);
                Some(tokio::spawn(async move {
                    match tokio::time::timeout(
                        SPRITE_FETCH_TIMEOUT,
                        src.sprites(&url, cookie.as_deref()),
                    )
                    .await
                    {
                        Ok(Ok(sheet)) => sheet,
                        Ok(Err(e)) => {
                            tracing::info!("雪碧图拿不到,进度条只出时间气泡: {e:#}");
                            None
                        }
                        Err(_) => {
                            tracing::info!("雪碧图请求超时,进度条只出时间气泡");
                            None
                        }
                    }
                }))
            }
            _ => None,
        };

        // 平台标注的片头 / 片尾(B 站番剧 clip_info_list):同雪碧图,与解析并行、同一超时;拿不到 = 空 =
        // 靠章节 / 指纹 / 手标(B 站约六成集有标注,空是常态,不 warn)。放歌不问。
        let clip_task = match (&source, audio_only) {
            (Some(src), false) => {
                let src = src.clone();
                let url = page_url.to_string();
                let cookie = cookie_recs.as_deref().map(cookies::header_value);
                Some(tokio::spawn(async move {
                    match tokio::time::timeout(SPRITE_FETCH_TIMEOUT, src.skip_clips(&url, cookie.as_deref()))
                        .await
                    {
                        Ok(Ok(v)) => v,
                        Ok(Err(e)) => {
                            tracing::info!("片头片尾标注拿不到,靠手标 / 章节: {e:#}");
                            Vec::new()
                        }
                        Err(_) => Vec::new(),
                    }
                }))
            }
            _ => None,
        };

        let task = self.inner.tasks.start("resolve", Text::new("task.resolve"));
        task.step("step.resolve", serde_json::Value::Null);
        let resolved =
            match resolver::resolve(&ytdlp, page_url, cookies_file.as_deref(), audio_only).await {
                Ok(r) => r,
                Err(resolver::ResolveError::AuthRequired(detail)) => {
                    // 需要登录 ≠ 失败:记下待重放 + 弹扫码气泡,登录成功后自动续上(见 set_cookies)。
                    if let Some(id) = &source_id {
                        self.record_pending(user_id, id, page_url, audio_only);
                        self.publish(MediaEvent::AuthRequired { source: id.clone() });
                        // 解析已得结论(需登录),不是失败:正常收尾,不标红 HUD、不喂模型「放失败了」。
                        task.done();
                        return Ok(PlayOutcome::AwaitingLogin { detail });
                    }
                    // 未知来源没有登录通道,只能如实退回(MVP 仅 bilibili,此分支基本走不到)。
                    task.fail("task.err.auth", serde_json::Value::Null);
                    anyhow::bail!("这个内容需要登录才能播放({detail})");
                }
                Err(e) => {
                    // 解析失败多为网络/瞬时:给重放口(UI 显「重试」,点击直连重播同一 url)
                    task.fail_retryable(
                        "task.err.resolve",
                        serde_json::Value::Null,
                        TaskRetry::MediaPlay { page_url: page_url.to_string(), audio_only },
                    );
                    anyhow::bail!("解析失败: {e}");
                }
            };

        let relay = self
            .inner
            .relay
            .get_or_try_init(relay::Relay::start)
            .await
            .context("转发服务起不来")?;

        let mut streams = resolved.streams.clone();
        let mut manifest_url: Option<String> = None;
        let stream_url = if streams.len() == 2 {
            // 音视频分离(B 站 DASH 常态)。**优先 DASH 直供**:不混流 → 前端 shaka 经 MSE 播两条流、
            // 播放器自己管时间轴 → 原生 seek + 音画同步(像 b 站网页;治混流 ?t= 重启 seek 的错位)。
            // 合成 MPD 需时长 + 探到 sidx;任一不满足 → 回落 ffmpeg 混流(老路,seek 有错位但至少能放)。
            let audio = streams.pop().expect("len==2");
            let video = streams.pop().expect("len==2");
            let dash = match resolved.duration_seconds {
                Some(dur) => match relay.register_dash(video.clone(), audio.clone(), dur).await {
                    Ok(url) => Some(url),
                    Err(e) => {
                        tracing::info!("DASH 直供不可用,回落 ffmpeg 混流: {e:#}");
                        None
                    }
                },
                None => {
                    tracing::info!("解析无时长,DASH 直供跳过,走 ffmpeg 混流");
                    None
                }
            };
            if let Some(url) = dash {
                manifest_url = Some(url.clone());
                url // stream_url 也存 manifest(前端优先用 manifest_url 走 shaka;此处只为非空)
            } else {
                let ffmpeg = match self.ensure_component(Component::Ffmpeg).await {
                    Ok(p) => p,
                    Err(e) => {
                        // 组件(ffmpeg)下载失败同属可重试:同样给重放口
                        task.fail_retryable(
                            "task.err.download",
                            serde_json::Value::Null,
                            TaskRetry::MediaPlay { page_url: page_url.to_string(), audio_only },
                        );
                        return Err(e);
                    }
                };
                relay.register_remux(video, audio, ffmpeg)
            }
        } else {
            relay.register_direct(streams.pop().expect("resolver 保证非空"))
        };
        task.done();

        // 雪碧图到手 → 注册进 relay 的 /thumb/ 端点(与本地 ffmpeg 抽帧同一端点、同一前端契约);
        // 没有 = None。有值 = 这片能出图,前端只认这一个信号(§3.5 不假装有图)。
        let thumb_url = match sprite_task {
            Some(handle) => handle
                .await
                .ok()
                .flatten()
                .map(|sheet| relay.register_sprites(Arc::new(sheet))),
            None => None,
        };

        let clips = match clip_task {
            Some(handle) => handle.await.unwrap_or_default(),
            None => Vec::new(),
        };
        let skip_info =
            if audio_only { None } else { self.compute_skip(clips, resolved.duration_seconds) };

        // 网络流没有本地音轨概念(来源已定轨)→ 清掉本地现场,切音轨会如实退回
        *self.inner.current_local.lk() = None;
        // 封面 = 源页面的封面图(B 站视频封面;放歌时就是那条视频的封面):经 relay 代取(带防盗链头,
        // 雪碧图同款),前端拿 `/cover/{token}`。没有 = None。
        let cover_url = resolved.thumbnail.clone().map(|url| {
            let headers = resolved.streams.first().map(|s| s.headers.clone()).unwrap_or_default();
            relay.register_cover(relay::CoverSrc::Remote { url, headers })
        });
        let np = NowPlaying {
            // 网络流的字幕另有来源(resolver 已解出平台字幕,配歌词在用),播放路本期不接。
            subtitles: Vec::new(),
            lyrics: None,
            kind: if audio_only { MediaKind::Audio } else { MediaKind::Video },
            title: resolved.title,
            author: resolved.uploader,
            album: None,
            cover_url,
            duration_seconds: resolved.duration_seconds,
            route: derive_route(&stream_url, manifest_url.as_deref()),
            stream_url,
            manifest_url,
            page_url: page_url.into(),
            source: source_id.clone().unwrap_or_else(|| "web".into()),
            playlist: pos,
            play_mode: self.play_mode_str(),
            rate: self.rate(),
            audio_tracks: Vec::new(),
            audio_track: 0,
            resume_at,
            // 网络流没帧可抽,预览图来自源的雪碧图(B 站 videoshot,见 MediaSource::sprites);
            // 源给不出 = None = 拖进度条只出时间气泡。
            thumb_url,
            skip: skip_info,
        };
        self.seed_playing(&np.title, pos.map(|p| (p.index, p.total)));
        self.publish(MediaEvent::Play(np.clone()));

        // 建议气泡素材:还没登录 → 每次启动至多提示一次"登录画质更清晰"
        if let Some(id) = source_id {
            if cookies_file.is_none() && !self.inner.login_hint_sent.swap(true, Ordering::Relaxed)
            {
                self.publish(MediaEvent::LoginHint { source: id });
            }
        }
        Ok(PlayOutcome::Playing(np))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::testkit::*;

    #[tokio::test]
    async fn auto_next_follows_play_mode() {
        let (rt, _rx) = runtime("auto-next-mode");
        // 三首歌单,当前在末首:放完就停 → None;列表循环 → 回卷 0;随机 → 重开一轮挑一首
        let mut pl = mk_shuffle_playlist(3);
        pl.index = 2;
        pl.played = vec![0, 1, 2];
        *rt.inner.playlist.lk() = Some(pl);
        let pick = |rt: &MediaRuntime, mode: PlayMode| {
            *rt.inner.mode.lk() = mode;
            let mut guard = rt.inner.playlist.lk();
            let pl = guard.as_mut().unwrap();
            match mode {
                PlayMode::Shuffle => shuffle_advance(pl, true, 5),
                PlayMode::LoopOne => Some(pl.index),
                _ if pl.index + 1 < pl.entries.len() => Some(pl.index + 1),
                PlayMode::LoopAll => Some(0),
                PlayMode::Once => None,
            }
        };
        assert_eq!(pick(&rt, PlayMode::Once), None);
        assert_eq!(pick(&rt, PlayMode::LoopAll), Some(0));
        assert_eq!(pick(&rt, PlayMode::LoopOne), Some(2));
        let s = pick(&rt, PlayMode::Shuffle).expect("随机恒有下一首");
        assert_ne!(s, 2, "重开一轮不紧挨着重复当前那首");
    }

    #[tokio::test]
    async fn play_dir_input_edges() {
        let (rt, _rx) = runtime("audio-dir-edge");
        // 没有音频的文件夹:如实退回
        let dir = std::env::temp_dir().join(format!("lw-audio-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        touch(&dir, "唯一.mp4");
        let err =
            rt.play(1, &dir.to_string_lossy(), false, false, None).await.unwrap_err().to_string();
        assert!(err.contains("没有能播放的音频"), "空音频文件夹如实退回: {err}");
        // 恰一首:退化成放那一首(单曲,无队列,仍强制只出声)
        let solo = std::env::temp_dir().join(format!("lw-audio-one-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&solo);
        std::fs::create_dir_all(&solo).unwrap();
        touch(&solo, "独一首.mp3");
        match rt.play(1, &solo.to_string_lossy(), false, false, None).await.unwrap() {
            PlayOutcome::Playing(np) => {
                assert!(matches!(np.kind, MediaKind::Audio), "目录入参强制只出声");
                assert!(np.playlist.is_none(), "单曲不出队列");
                assert_eq!(np.title, "独一首");
            }
            other => panic!("应为 Playing,实际 {other:?}"),
        }
    }

    #[tokio::test]
    async fn audio_autonext_loop_and_shuffle_flow() {
        let (rt, _rx) = runtime("audio-queue");
        let dir = std::env::temp_dir().join(format!("lw-audio-play-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for n in ["a.mp3", "b.mp3", "c.mp3"] {
            touch(&dir, n);
        }
        let plist = |o: PlayOutcome| match o {
            PlayOutcome::Playing(np) => np,
            other => panic!("应为 Playing,实际 {other:?}"),
        };

        // 目录入参:整夹组队、强制只出声、从第一首起;歌单默认列表循环,模式镜像随 Play 捎带
        let np = plist(rt.play(1, &dir.to_string_lossy(), false, false, None).await.unwrap());
        assert!(matches!(np.kind, MediaKind::Audio));
        let pos = np.playlist.expect("整夹应组队");
        assert_eq!((pos.index, pos.total), (0, 3));
        assert_eq!(np.play_mode, "loop_all", "歌单默认列表循环");

        // 顺序自动续播:0→1→2;列表循环 → 末首放完回卷到第一首
        let np = plist(rt.auto_next(1).await.unwrap().expect("有下一首"));
        assert_eq!(np.playlist.unwrap().index, 1);
        assert_eq!(plist(rt.auto_next(1).await.unwrap().unwrap()).playlist.unwrap().index, 2);
        let np = plist(rt.auto_next(1).await.unwrap().expect("列表循环回卷"));
        assert_eq!(np.playlist.unwrap().index, 0);
        let np = plist(rt.advance(1, -1).await.unwrap());
        assert_eq!(np.playlist.unwrap().index, 2, "列表是环,到顶回卷不报错");

        // 放完就停(视频剧集的默认;歌单只能内部置,嘴控没有这一档):末首放完 → 交回前端收尾,到头报错
        *rt.inner.mode.lk() = PlayMode::Once;
        assert!(rt.auto_next(1).await.unwrap().is_none(), "末首且放完就停 → 收尾");
        assert!(rt.advance(1, 1).await.is_err(), "放完就停:已是最后一首");

        // 单曲循环:auto_next 重放本首(正常由前端 el.loop 兜,真走到这也不跳集);到头照样回卷
        rt.control("loop_one", None).unwrap();
        let np = plist(rt.auto_next(1).await.unwrap().expect("重放本首"));
        assert_eq!(np.playlist.unwrap().index, 2);
        assert_eq!(np.play_mode, "loop_one");
        let np = plist(rt.advance(1, 1).await.unwrap());
        assert_eq!(np.playlist.unwrap().index, 0, "单曲循环下手动下一首照样回卷");

        // 随机:auto_next 挑「这轮没放过的」,一轮内不重复;放完一轮重开(随机恒循环不停)
        rt.control("shuffle_on", None).unwrap();
        let mut seen = vec![0usize]; // 当前第 1 首(index 0),新一轮履历从它起算
        for _ in 0..2 {
            let np = plist(rt.auto_next(1).await.unwrap().expect("随机还有没放过的"));
            let i = np.playlist.unwrap().index;
            assert!(!seen.contains(&i), "随机一轮内不重复,已放 {seen:?} 又放 {i}");
            assert_eq!(np.play_mode, "shuffle", "随机镜像随 Play 捎带");
            seen.push(i);
        }
        assert!(rt.auto_next(1).await.unwrap().is_some(), "随机放完一轮重开一轮");
        // 「别随机了」= 回歌单默认(列表循环),接着顺序放
        rt.control("shuffle_off", None).unwrap();
        assert_eq!(*rt.inner.mode.lk(), PlayMode::LoopAll);
        assert!(rt.auto_next(1).await.unwrap().is_some());

        // 新播放请求复位模式(音量粘住、模式不粘):歌单回到默认列表循环
        rt.control("loop_one", None).unwrap();
        let np = plist(rt.play(1, &dir.to_string_lossy(), false, true, None).await.unwrap());
        assert_eq!(np.play_mode, "loop_all", "新 play() 复位到歌单默认");
    }

    #[tokio::test]
    async fn local_series_autoadvance_and_resume_rule() {
        let (rt, _rx) = runtime("series");
        let dir = std::env::temp_dir().join(format!("lw-series-play-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mk = |n: &str| {
            let f = dir.join(n);
            std::fs::write(&f, b"x").unwrap();
            f.to_string_lossy().to_string()
        };
        let e1 = mk("剧 第1集.mp4");
        let _e2 = mk("剧 第2集.mp4");
        let e3 = mk("剧 第3集.mp4");
        let plist = |o: PlayOutcome| match o {
            PlayOutcome::Playing(np) => np.playlist.expect("应有队列位置"),
            other => panic!("应为 Playing,实际 {other:?}"),
        };

        // 起播第1集 → 三集队列,位置 0/3,非续播
        let pos = plist(rt.play(1, &e1, false, false, None).await.unwrap());
        assert_eq!((pos.index, pos.total, pos.resumed), (0, 3, false));

        // 自动/手动续播:下一集 → 1,再下一集 → 2
        assert_eq!(plist(rt.advance(1, 1).await.unwrap()).index, 1);
        assert_eq!(plist(rt.advance(1, 1).await.unwrap()).index, 2);
        // 末集再「下一集」= 越界报错(不重播)
        assert!(rt.advance(1, 1).await.is_err(), "末集再下一集应报错");
        // 上一集 → 回到 1
        assert_eq!(plist(rt.advance(1, -1).await.unwrap()).index, 1);

        // 进度此刻停在第2集 → 重放(点首集/没点集)续播跳回第2集
        let pos = plist(rt.play(1, &e1, false, false, None).await.unwrap());
        assert_eq!((pos.index, pos.resumed), (1, true), "应接着上次第2集");

        // restart=true → 回第1集、不续播
        let pos = plist(rt.play(1, &e1, false, true, None).await.unwrap());
        assert_eq!((pos.index, pos.resumed), (0, false));

        // 2026-09-07 改:传第3集的**路径**不再算点名(路径只用来认剧)—— 有进度就接进度(此刻第1集)。
        // 老规则把它当「点名要看那集」,模型拿上次的路径当剧的把手时进度就被绕过(真机回到第 2 集)。
        assert_eq!(plist(rt.play(1, &e3, false, false, None).await.unwrap()).index, 0, "路径不是点名");
        // 点名 = episode 参数:看第3集 → index 2;越界如实报
        assert_eq!(plist(rt.play(1, &e3, false, false, Some(3)).await.unwrap()).index, 2);
        let err = rt.play(1, &e1, false, false, Some(9)).await.unwrap_err().to_string();
        assert!(err.contains("一共 3 集"), "越界报错要说清共几集: {err}");

        // 第 N 集绝对定位(嘴控「看第一集」= 1 起数):跳到第1集 → index 0
        let pos = plist(rt.jump_to_episode(1, 1).await.unwrap());
        assert_eq!((pos.index, pos.total), (0, 3));
        // 跳到当前集 = 从头重放该集(不报错);越界/0 集如实报错
        assert_eq!(plist(rt.jump_to_episode(1, 1).await.unwrap()).index, 0);
        let err = rt.jump_to_episode(1, 9).await.unwrap_err().to_string();
        assert!(err.contains("一共 3 集"), "越界报错要说清共几集: {err}");
        assert!(rt.jump_to_episode(1, 0).await.is_err(), "第 0 集(1 起数)被拒");
        // 跳集也落续播进度:重放(自然起点)接到刚跳的第1集 → resumed=false(本就是首集)
        let pos = plist(rt.play(1, &e1, false, false, None).await.unwrap());
        assert_eq!((pos.index, pos.resumed), (0, false), "进度已被 jump 更新到第1集");

        // 没有队列时 advance / jump 都报错(没在放剧集)
        let (rt2, _rx2) = runtime("noqueue");
        assert!(rt2.advance(1, 1).await.is_err());
        assert!(rt2.jump_to_episode(1, 2).await.is_err());
    }
}
