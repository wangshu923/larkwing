//! relay · ffmpeg 机器:自适应 / HLS 的段生成(`gen_video_*`)、命令构造(`build_*_frag_cmd`)、整段收集(`run_ffmpeg_collect`,stderr 并发排空)。

use super::*;

/// 生成视频 init(ftyp+moov):切 0.1s 分片、取首个 moof 之前的部分。注册自适应播放时调一次,
/// 既拿到 init 字节缓存(vinit 端点直接回),又能从中解出精确 codec 串(avcC)。失败 → None
/// (调用方回落 muxed HLS,不硬走分离路)。`copy_video` 与后续各段一致 → init 与段的 avcC 匹配。
pub async fn gen_video_init(
    ffmpeg: &Path,
    path: &Path,
    copy_video: bool,
    enc: VideoEncoder,
) -> Option<Vec<u8>> {
    let cmd = build_frag_cmd(ffmpeg, path, 0.0, 0.1, copy_video, true, enc, 0);
    let full = run_ffmpeg_collect(cmd, 8 * 1024 * 1024).await?;
    let moof = super::probe::first_moof_offset(&full)?;
    Some(full[..moof].to_vec())
}

/// 切一段视频出来(moof+mdat,不含 init)。P2 的**运行时接缝抽查**用它:注册时拿第一段量一下
/// 真实时长,与计划对不上就整条降级 —— 段字节本来就要产出,量它是纯解析(`fragment_duration`)、
/// 不额外起进程。与 `/la/v{N}` 端点走同一条 `build_frag_cmd`,量的就是真会播的那份字节。
pub async fn gen_video_segment(
    ffmpeg: &Path,
    path: &Path,
    start: f64,
    dur: f64,
    copy_video: bool,
    enc: VideoEncoder,
) -> Option<Vec<u8>> {
    let cmd = build_frag_cmd(ffmpeg, path, start, dur, copy_video, true, enc, 0);
    let full = run_ffmpeg_collect(cmd, 256 * 1024 * 1024).await?;
    let moof = super::probe::first_moof_offset(&full)?;
    let end = super::probe::moof_segment_end(&full, moof);
    Some(full[moof..end].to_vec())
}

/// 音频统一响度:先下混到立体声、整段统一提量、再限幅防削顶 —— 修「转码后整段偏小(人声+音效一起小)」。
/// 做的是「整段一起抬」,**不挑人声**(与用户讨论:不是对白被埋,是整体衰减 —— 5.1→立体声下混归一化
/// 约 −8dB + AC3/DTS 的 DRC 被 ffmpeg 套上)。`volume` 必须在下混**之后**提量才有效(故都塞进 `-af`,
/// 别用 `-ac 2`——那是输出选项、在滤镜之后跑,会把提过的量又除回去);`alimiter` 只削最尖的峰值
/// (防「系统音量开太大被吓到」),音效整体力度保留。**值是真机可调项**:实际增益/限幅只能
/// Windows 真机 + 真 5.1 片验(§8.1)。改这一处即改全部转码音频(build_frag_cmd 的 HLS 段 /
/// FileRemux 的 `/m/` 流式混流共用)。
/// **增益 2026-07-04 由 +8dB 降到 +5dB**:真机反馈响的时候「偶尔轻微破音」—— +8dB 把响段推进
/// alimiter 太狠,重限幅出失真。−3dB 后仍明显提量(从下混/DRC 的偏小里拉回来),破音余量更足。
/// 还破就继续调小这一个数;想更响调回去。
pub(crate) const AUDIO_LOUDNESS_AF: &str =
    "aformat=channel_layouts=stereo,volume=5dB,alimiter=limit=0.95";

/// 构建「区间 [start, start+dur) 的自包含分片 mp4」命令(ftyp+moov+moof+mdat 吐 stdout)。
/// HLS 的 init(取 0.1s)与各段(取 HLS_SEG)都用它。
///
/// **视频、音频一律转码(不 copy),这是按需 fMP4-HLS 在 WebView2/Chromium MSE 上稳的前提**
/// (三处实证,2026-06-20 Mac Chromium MSE 复现):
/// ① **视频转码**:`-ss` + `-c:v copy` 只能落到关键帧、切不准 → 段时长漂(实测请求 6s 出 8s)、
///    段间重叠/错位;转码则每段从干净 IDR 起、恰好 dur 秒、编码配置(avcC)跨段一致 → MSE 拼得上。
/// ② **音频必转码**:视频转码 + 音频 `copy` 时,fragmented muxer 把样本时长写成 2×(段被拉长一倍,
///    实测),两轨都转码即消失。
/// ③ **下混立体声 + 统一响度(`-af AUDIO_LOUDNESS_AF`)**:多声道 AAC 声道布局不明确会被 MSE **拒绝 append**
///    整个 init(报在 video 轨上,正是用户「video:2 code=3014 黑屏」)→ aformat 下混立体声永远能 append;
///    顺带整段提量 + 限幅,修「转码后整段偏小」(替掉原来的 `-ac 2`,见 AUDIO_LOUDNESS_AF 说明)。
/// 代价 = 已是 H.264 的片子(仅因容器 mkv / 音轨 AC3 才进 HLS)也被重编视频,弱机吃 CPU;
/// 但这些片子当前本就黑屏(mpegts 链路),不是回退。**0.2.6 起「视频已兼容」的片走音视频分离
/// 的 `FileAdaptive` 路(视频 `-c:v copy`、音频一整条连续编码),省 CPU + 治漂移**(见下);
/// 本函数的「muxed HLS(视频转码 + 逐段音频)」只兜「视频轨也不兼容(HEVC/AV1)且不走分离路」的老链路。
///
/// `copy_video`:true = `-c:v copy`(视频已兼容,零重编码,须段界落关键帧);false = 转 H.264。
/// `video_only`:true = `-an`(分离路的视频段,音频另走连续流);false = 带音轨(老 muxed HLS)。
#[allow(clippy::too_many_arguments)]
pub(super) fn build_frag_cmd(
    ffmpeg: &Path,
    path: &Path,
    start: f64,
    dur: f64,
    copy_video: bool,
    video_only: bool,
    enc: VideoEncoder,
    audio_track: usize,
) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(ffmpeg);
    cmd.arg("-hide_banner").arg("-loglevel").arg("error").arg("-nostdin");
    if start > 0.0 {
        // copy 必须精确落在关键帧上:高精度 + 微正 margin(COPY_SS_EPS),防浮点舍入落到**前一个**
        // 关键帧(关键帧间距 ≫ margin,不会误跳到下一个)。转码不挑关键帧(从新 IDR 重编),用 .3 即可。
        let ss = if copy_video {
            format!("{:.6}", start + COPY_SS_EPS)
        } else {
            format!("{start:.3}")
        };
        cmd.arg("-ss").arg(ss);
    }
    cmd.arg("-i").arg(path).arg("-t").arg(format!("{dur:.6}"));
    cmd.arg("-map").arg("0:v:0?");
    if !video_only {
        cmd.arg("-map").arg(format!("0:a:{audio_track}?")); // 选中的音轨(多音轨片按此切换)
    }
    if copy_video {
        cmd.arg("-c:v").arg("copy"); // 视频已兼容:原样搬,不掉画质、CPU 近零
    } else {
        apply_video_encode(&mut cmd, enc); // 硬件优先(省 CPU),回落 libx264 与旧行为一致
    }
    if !video_only {
        cmd.arg("-c:a").arg("aac").arg("-af").arg(AUDIO_LOUDNESS_AF).arg("-b:a").arg("256k");
    }
    // 单 moof/段:`-frag_duration` 给个远超段长(600s)的值 → ffmpeg 不在段内再分片,整段就一个
    // moof+mdat,便于把 tfdt 改成累计值(见 probe::patch_segment_tfdt)。default_base_moof 让 trun
    // 数据偏移相对 moof → 切掉 init 后偏移仍对。
    cmd.arg("-movflags").arg("empty_moov+default_base_moof")
        .arg("-frag_duration").arg("600000000")
        .arg("-f").arg("mp4").arg("pipe:1");
    cmd
}

/// copy 段 `-ss` 的微正 margin(秒):够跨过浮点舍入、又远小于任何关键帧间距(≥ 一帧 ~16ms)。
const COPY_SS_EPS: f64 = 0.001;

/// 构建音频段命令(`-vn` 纯音频 → AAC 立体声 + 响度,单 moof 分片吐 stdout)。`ss>0` 从该秒输入 seek
/// (段带左预卷时用),`dur` = 要切的时长(含预卷)。init 段取 `ss=0,dur=0.1`。tfdt 由 ffmpeg 归零,
/// 前端 timestampOffset + appendWindow 定位/裁剪。
pub(super) fn build_audio_frag_cmd(
    ffmpeg: &Path,
    path: &Path,
    ss: f64,
    dur: f64,
    audio_track: usize,
    // P3「原声优先」:true = `-c:a copy` 原样搬(**不能带 `-af`** —— 滤镜要解码,与 copy 互斥),
    // 原声道原动态一个字节不动;响度改由播放端 WebAudio 补。false = 老路(转 AAC + 响度链)。
    copy_audio: bool,
) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(ffmpeg);
    cmd.arg("-hide_banner").arg("-loglevel").arg("error").arg("-nostdin");
    // **copy 路必须两段式 seek(2026-08-22 真机破案)**:单靠输入侧 `-ss` 时,容器级 seek 落到前一个
    // **视频**关键帧,`-c:a copy` 不解码所以早出来的那截不会被裁;`-t` 又是按输出时间轴计数、
    // 不丢负 PTS → 段内容 = 计划 + δ(δ = ss − 前一个关键帧,真机 BD 片实测达 7.3s)。随后 mp4
    // muxer 的 `avoid_negative_ts` 把它整体平移、`tfdt` 归 0 —— **「内容实际从哪开始」的信息就此
    // 丢失**(`zero_tfdt` 因而是 no-op)→ 前端按 `grid − preroll` 摆放 = 整段晚 δ、**每个接缝重播
    // δ 秒**(表现:音画不同步 + 声音一段段重复)。故粗切留 `AUDIO_FINE_SEEK`,余量交给输出侧 `-ss`
    // 按时间戳精确丢弃(音频每包都是同步点,不需解码,实测只多 0.04s)。
    // 转码路本就是精确 seek(解码丢弃到 ss),**一字不动 = 零回归**。
    let fine = if copy_audio && ss > 0.0 { AUDIO_FINE_SEEK.min(ss) } else { 0.0 };
    if ss - fine > 0.0 {
        cmd.arg("-ss").arg(format!("{:.6}", ss - fine));
    }
    cmd.arg("-i").arg(path);
    if fine > 0.0 {
        cmd.arg("-ss").arg(format!("{fine:.6}"));
    }
    cmd.arg("-t").arg(format!("{dur:.6}")).arg("-vn")
        .arg("-map").arg(format!("0:a:{audio_track}?")) // 显式选轨(原 -vn 默认挑轨,多音轨不可控)
        .arg("-c:a");
    if copy_audio {
        cmd.arg("copy"); // 原样搬:不带 -af(滤镜要解码,与 copy 互斥)、不设码率
    } else {
        cmd.arg("aac").arg("-af").arg(AUDIO_LOUDNESS_AF).arg("-b:a").arg("256k");
    }
    cmd.arg("-movflags").arg("empty_moov+default_base_moof")
        .arg("-frag_duration").arg("600000000")
        .arg("-f").arg("mp4").arg("pipe:1");
    cmd
}

/// 跑 ffmpeg、把 stdout 整段收进内存(封顶 cap;分片就几 MB~几十 MB,稳)。失败/空 → None(并记 stderr)。
pub(super) async fn run_ffmpeg_collect(mut cmd: tokio::process::Command, cap: usize) -> Option<Vec<u8>> {
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    super::no_console(&mut cmd);
    let mut child = cmd.spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let stderr = child.stderr.take()?;
    // **stderr 必须并发排空**(2026-08-22 审计):只读 stdout 的话,ffmpeg 一旦把 stderr 管道
    // 写满(管道缓冲 ~64KB)就会阻塞在写 stderr 上 → 它也不再产 stdout → 我们的 `stdout.read()`
    // 永远等下去,后面的 `child.wait()` 也永远等不到退出 = **整条 HTTP 请求永久挂死**
    // (播放器那边就是转圈到超时)。`-loglevel error` 平时只有几行,但坏文件/坏参数能刷满。
    // 有界收集:错误尾巴用不了那么多,别让一个疯狂刷 stderr 的 ffmpeg 把内存吃掉。
    let err_task = tokio::spawn(async move {
        let mut s = String::new();
        let _ = stderr.take(STDERR_TAIL_CAP as u64).read_to_string(&mut s).await;
        s
    });
    let mut buf = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    let mut truncated = false;
    loop {
        match stdout.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(k) => {
                buf.extend_from_slice(&chunk[..k]);
                if buf.len() > cap {
                    tracing::warn!("ffmpeg 输出超上限 {cap} 字节,截断");
                    truncated = true;
                    break;
                }
            }
        }
    }
    // 超上限提前收手后**必须杀掉它**:我们不再读 stdout 了,不杀它就会卡在写 stdout 上、
    // 于是 `wait()` 永远等不到退出(与上面同族的第二处挂死)。
    if truncated {
        let _ = child.start_kill();
    }
    let _ = child.wait().await;
    if buf.is_empty() {
        let err = err_task.await.unwrap_or_default();
        if !err.trim().is_empty() {
            tracing::warn!("HLS ffmpeg stderr: {}", err.trim());
        }
        return None;
    }
    Some(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **子进程 stderr 必须并发排空,否则整条请求永久挂死(2026-08-22 审计)**。
    ///
    /// 原先只在循环里读 stdout、`child.wait()` 之后才读 stderr:子进程把 stderr 管道写满
    /// (~64KB)就阻塞在写 stderr 上 → 它也不再产 stdout → `stdout.read()` 永远等 → `wait()`
    /// 也永远等不到退出。播放器那边表现为一直转圈到超时。
    /// 这里不用 ffmpeg,用 `sh` 造一个「先猛刷 stderr、再吐一点 stdout」的进程 —— 病灶与被测
    /// 程序无关,是**读管道的姿势**。修之前这条测试会卡到 timeout 而红。
    /// (`#[cfg(unix)]`:Windows 没有 sh;死锁逻辑与平台无关,mac/Linux 上钉住即可。)
    #[cfg(unix)]
    #[tokio::test]
    async fn collect_drains_stderr_concurrently_and_does_not_hang() {
        let mut cmd = tokio::process::Command::new("sh");
        // 200KB 到 stderr(远超管道缓冲),然后才往 stdout 写
        cmd.arg("-c").arg("head -c 200000 /dev/zero | tr '\\0' 'E' >&2; printf OK");
        let got = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            run_ffmpeg_collect(cmd, 8 * 1024 * 1024),
        )
        .await
        .expect("stderr 没被并发排空 → 卡在写管道上、整条请求挂死");
        assert_eq!(got.as_deref(), Some(&b"OK"[..]), "stdout 要完整收到");
    }

    /// 同族第二处:**超上限提前收手后必须杀掉子进程**。不再读 stdout 了却只 `wait()`,
    /// 子进程会卡在写 stdout 上、`wait()` 永远等不到退出。
    #[cfg(unix)]
    #[tokio::test]
    async fn collect_kills_child_when_output_exceeds_cap() {
        let mut cmd = tokio::process::Command::new("sh");
        // 无限往 stdout 灌:cap 一到就该被杀掉,而不是等它自己结束(它永远不结束)
        cmd.arg("-c").arg("yes X");
        let got = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            run_ffmpeg_collect(cmd, 64 * 1024),
        )
        .await
        .expect("超上限后没杀子进程 → wait() 永远等不到退出");
        assert!(got.is_some_and(|b| b.len() > 64 * 1024), "截断处收到的那截照样返回");
    }

    /// **契约③的音频半边(2026-08-22 真机实锤后补)**:copy 音频段必须**恰好**是计划的长度。
    ///
    /// 事故形态:`-ss` 放在 `-i` 之前 = 容器级 seek,落到前一个**视频**关键帧;`-c:a copy` 不解码,
    /// 所以早出来的那截不会被裁,而 `-t` 是按输出时间轴计数的、不丢负 PTS → 段内容 = 计划 + δ,
    /// 其中 δ = ss − 前一个视频关键帧。接着 mp4 muxer 的 `avoid_negative_ts` 把它整体平移、
    /// `tfdt` 归 0 —— **「内容实际从哪开始」的信息就此丢失**(`zero_tfdt` 因而是 no-op)。
    /// 前端按 `grid − preroll` 摆放 → 整段晚 δ、**每个接缝重播 δ 秒**。真机 BD 片实测 δ 达 7.3s
    /// (关键帧间距 3~14s),表现为「音画不同步 + 声音一段段重复」;切到转码路(精确 seek)反而正常。
    ///
    /// 关键设计:合成源的关键帧要**稀**(每 4s),δ 才够大到超出容差;音轨必须 **AAC**
    /// (`capability` 的 copy 前提之一),否则走的是转码路、测不到这条。
    ///
    /// 需 PATH 有 ffmpeg:
    /// `cargo test -p larkwing-core --lib media::relay::tests::audio_copy_segment -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn audio_copy_segment_is_exactly_planned_length() {
        let dir = std::env::temp_dir().join(format!("lw-aseg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("片子.mp4");
        // 40s;关键帧每 4s(-g 100 @25fps + 关掉场景切)→ 音频 6s 网格与关键帧刻意不对齐。
        let ok = std::process::Command::new("ffmpeg")
            .args([
                "-y", "-hide_banner", "-loglevel", "error",
                "-f", "lavfi", "-i", "testsrc=size=320x240:rate=25:duration=40",
                "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000:duration=40",
                "-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p",
                "-g", "100", "-keyint_min", "100", "-sc_threshold", "0",
                "-c:a", "aac", "-b:a", "128k", "-f", "mp4",
            ])
            .arg(&src)
            .status()
            .expect("run ffmpeg")
            .success();
        assert!(ok, "生成测试源失败");

        // AAC 在 mp4 里的 timescale = 采样率;段内 trun 的样本时长累加 = 真实时长。
        const TS: u32 = 48000;
        let ffmpeg = PathBuf::from("ffmpeg");
        for n in 1usize..=4 {
            let grid = n as f64 * AUDIO_SEG;
            let ss = grid - AUDIO_PREROLL;
            let cut = AUDIO_SEG + AUDIO_PREROLL;
            let cmd = build_audio_frag_cmd(&ffmpeg, &src, ss, cut, 0, true);
            let full = run_ffmpeg_collect(cmd, 16 * 1024 * 1024).await.expect("切音频段");
            let moof = super::super::probe::first_moof_offset(&full).expect("找 moof");
            let end = super::super::probe::moof_segment_end(&full, moof);
            let real = super::super::probe::fragment_duration(&full[moof..end], TS)
                .expect("量段内时长");
            // 容差放一帧多一点(1024/48000 ≈ 21ms);δ 是秒级,一眼分得开。
            assert!(
                (real - cut).abs() < 0.05,
                "a{n}(ss={ss}) 段长应 ≈{cut}s,实际 {real:.3}s —— 多出来的 {:.3}s \
                 是 -ss 落到前一个视频关键帧带进来的陈旧音频,会在接缝处重播",
                real - cut
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}
