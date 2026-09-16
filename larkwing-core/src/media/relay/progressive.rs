//! relay · /m/ 渐进混流:ffmpeg 现拼 fMP4 流(`?t=` 重启式 seek);`spawn_stream` 弃读即杀 child、stderr 并发排空。

use super::*;

#[derive(serde::Deserialize, Default)]
pub(super) struct RemuxQuery {
    /// 起播秒(seek = 换 src 重启混流,前端自己记位移)。
    #[serde(default)]
    t: f64,
}

/// 混流:经 ffmpeg 拼 fMP4 吐 stdout。无总长、不可 Range —— <video> 按渐进流播;两种来源:
///   Remux      两路网络上游 `-c copy`(B 站 DASH);
///   FileRemux  单个本地文件,视频 `-c:v copy` + 音轨转 AAC(AC3/DTS 本地片,见 probe.rs)。
/// 共用 stream_ffmpeg 起进程吐流。child 的生死跟着搬运任务走(响应体被 drop → 搬运 send
/// 失败 → 任务退出 → child drop → kill_on_drop 收尸),与 llm 取消同一个所有权手法。
pub(super) async fn remux(
    State(state): State<Arc<Inner>>,
    AxPath(token): AxPath<String>,
    Query(q): Query<RemuxQuery>,
) -> Response {
    let Some(entry) = lookup(&state, &token) else { return bad(StatusCode::NOT_FOUND) };
    let mut cmd = match entry.as_ref() {
        Entry::Remux { video, audio, ffmpeg } => {
            let mut cmd = tokio::process::Command::new(ffmpeg);
            cmd.arg("-hide_banner").arg("-loglevel").arg("error").arg("-nostdin");
            for up in [video, audio] {
                if q.t > 0.0 {
                    cmd.arg("-ss").arg(format!("{:.3}", q.t));
                }
                if !up.headers.is_empty() {
                    let joined: String =
                        up.headers.iter().map(|(k, v)| format!("{k}: {v}\r\n")).collect();
                    cmd.arg("-headers").arg(joined);
                }
                cmd.arg("-i").arg(&up.url);
            }
            cmd.arg("-map").arg("0:v:0").arg("-map").arg("1:a:0")
                .arg("-c").arg("copy"); // 纯复制不转码:CPU 几乎零开销
            cmd
        }
        Entry::FileRemux { path, ffmpeg, transcode_video, transcode_audio, enc, audio_track } => {
            let mut cmd = tokio::process::Command::new(ffmpeg);
            cmd.arg("-hide_banner").arg("-loglevel").arg("error").arg("-nostdin");
            if q.t > 0.0 {
                cmd.arg("-ss").arg(format!("{:.3}", q.t)); // 输入 seek(对 copy 是关键帧对齐)
            }
            cmd.arg("-i").arg(path);
            // 首条视频可选(纯音频不报错)+ 首条音轨可选(无声轨不报错);字幕等不带。
            cmd.arg("-map").arg("0:v:0?").arg("-map").arg(format!("0:a:{audio_track}?"));
            if *transcode_video {
                // HEVC/AV1 等转 H.264:硬件优先(省 CPU),回落 libx264;yuv420p 把 10bit 压回 8bit
                //(浏览器只认),否则 H.264 10bit 一样放不了。参数收口 apply_video_encode(§4.8 单源)。
                apply_video_encode(&mut cmd, *enc);
            } else {
                cmd.arg("-c:v").arg("copy"); // 视频兼容:原样搬,不掉画质、CPU 近零
            }
            if *transcode_audio {
                // 下混立体声(源可能 5.1,直出多声道 AAC 浏览器放不了)+ 统一响度(修转码后整段偏小)。
                cmd.arg("-c:a").arg("aac").arg("-af").arg(AUDIO_LOUDNESS_AF).arg("-b:a").arg("256k");
            } else {
                cmd.arg("-c:a").arg("copy");
            }
            // 注:拖动 seek 后的音画同步是 /m/ 重启式 seek 的固有难题(copy 视频回退关键帧),
            // 网络 DASH 两路输入同样存在;修法是方向决策(见与用户的讨论),不在此处堆 flag。
            cmd
        }
        _ => return bad(StatusCode::NOT_FOUND),
    };
    // 两条路都吐流式 fMP4(渐进播);HLS / 自适应的段走 `run_ffmpeg_collect` 整段收(也是 fMP4)。
    cmd.arg("-movflags")
        .arg("frag_keyframe+empty_moov+default_base_moof")
        .arg("-f")
        .arg("mp4")
        .arg("pipe:1");
    // cors=true:crossorigin 接管 <video> 做响度均衡时,/m/(本地混流/网络 DASH 混流)也要放行。
    stream_ffmpeg(cmd, "video/mp4", true)
}

/// 给一个**已配好程序+参数+输出格式(`-f … pipe:1`)**的 ffmpeg 命令收口 stdio、起进程、把
/// stdout 搬成 HTTP 流(唯一调用方 = `/m/` 渐进混流;段类走 `run_ffmpeg_collect`)。
/// child 生死跟搬运任务走:响应体被 drop → send 失败 → **当场 `start_kill`** → 收尸 → 任务退出。
fn stream_ffmpeg(cmd: tokio::process::Command, content_type: &'static str, cors: bool) -> Response {
    spawn_stream(cmd, content_type, cors)
        .map(|(resp, _task)| resp)
        .unwrap_or_else(|| bad(StatusCode::INTERNAL_SERVER_ERROR))
}

/// `stream_ffmpeg` 的本体,把搬运任务的句柄一并交出来(测试据此断言「弃读后任务真结束 = child 真被杀」)。
///
/// **两条纪律(2026-09-16 体检修,与 `run_ffmpeg_collect` 2026-08-22 那条互为镜像)**:
/// ① 客户端弃读(`?t=` 重启式 seek / 切集 / 关窗)后**必须主动杀 child**:原先只 `break` 然后去等
///    stderr 的 EOF,可 stdout 管道的读端还握在本任务手里、ffmpeg 一写满 64KB 就阻塞、永不退出,
///    stderr 也就永无 EOF → 任务永挂、child 永不 drop、`kill_on_drop` 永远轮不到 —— 每拖一次进度条
///    漏一个卡死的 ffmpeg + 一个 tokio task。
/// ② stderr **并发排空且有界**(`STDERR_TAIL_CAP`):串在 stdout 之后读的话,ffmpeg 刷满 stderr 管道
///    就整条挂死(坏文件 / 坏参数能刷满)。
fn spawn_stream(
    mut cmd: tokio::process::Command,
    content_type: &'static str,
    cors: bool,
) -> Option<(Response, tokio::task::JoinHandle<()>)> {
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    super::no_console(&mut cmd); // Windows 下不弹控制台黑框

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("ffmpeg 起不来: {e}");
            return None; // 调用方回 500
        }
    };
    let mut stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let err_task = tokio::spawn(async move {
        let mut s = String::new();
        let _ = stderr.take(STDERR_TAIL_CAP as u64).read_to_string(&mut s).await;
        s
    });

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(8);
    let task = tokio::spawn(async move {
        let mut buf = vec![0u8; 64 * 1024];
        let mut abandoned = false;
        loop {
            match stdout.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(Ok(bytes::Bytes::copy_from_slice(&buf[..n]))).await.is_err() {
                        abandoned = true; // 客户端不要了(暂停换台 / seek 重启 / 关窗)
                        break;
                    }
                }
            }
        }
        if abandoned {
            let _ = child.start_kill(); // 别等它自己退:它正卡在写我们不再读的那根管道上
        }
        drop(stdout); // 关掉读端:万一 kill 没到位(已退出 / 平台差异),它写下一块也拿 EPIPE 走人
        let _ = child.wait().await; // 收尸(kill_on_drop 只是兜底)
        if let Ok(err) = err_task.await {
            let err = err.trim();
            if !err.is_empty() && !abandoned {
                tracing::warn!("ffmpeg stderr: {err}");
            }
        }
    });

    let mut builder = Response::builder().status(StatusCode::OK).header("content-type", content_type);
    if cors {
        // 被 fetch 拉(跨源:app 源 ≠ relay 回环口)→ 必须放行。
        builder = builder.header("access-control-allow-origin", "*");
    }
    builder
        .body(Body::from_stream(tokio_stream_from(rx)))
        .ok()
        .map(|resp| (resp, task))
}

fn tokio_stream_from(
    rx: tokio::sync::mpsc::Receiver<Result<bytes::Bytes, std::io::Error>>,
) -> impl futures_util::Stream<Item = Result<bytes::Bytes, std::io::Error>> {
    futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **弃读必须杀 child(2026-09-16 体检修)**:`/m/` 每次 `?t=` 重启式 seek 都是「客户端掐断上一条响应」。
    /// 原先任务 break 后去等 stderr 的 EOF,可 stdout 读端还在本任务手里、子进程一写满管道就卡住、永不
    /// 退出 → 任务永挂、child 永不 drop、`kill_on_drop` 永远轮不到。这里用 `yes`(无限吐)当替身:读一块
    /// 就把响应体丢掉,搬运任务必须在几秒内自己结束(= child 已被杀并收尸)。修前这条卡到 timeout 而红。
    #[cfg(unix)]
    #[tokio::test]
    async fn stream_kills_child_when_client_drops_body() {
        use futures_util::StreamExt;
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c").arg("yes X");
        let (resp, task) = spawn_stream(cmd, "video/mp4", false).expect("起得来");
        let mut body = resp.into_body().into_data_stream();
        let first = body.next().await.expect("至少吐一块").expect("不是错误");
        assert!(!first.is_empty());
        drop(body); // 客户端不要了
        tokio::time::timeout(std::time::Duration::from_secs(10), task)
            .await
            .expect("弃读后搬运任务没结束 = child 没被杀、永挂")
            .expect("任务不该 panic");
    }

    /// 同族:stderr 猛刷时 stdout 照样吐得出来(`run_ffmpeg_collect` 2026-08-22 修过的病,`stream_ffmpeg`
    /// 原先原样还在 —— stderr 串在 stdout 之后读,子进程写满 stderr 管道就整条挂死)。
    #[cfg(unix)]
    #[tokio::test]
    async fn stream_drains_stderr_concurrently() {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c").arg("head -c 200000 /dev/zero | tr '\\0' 'E' >&2; printf OK");
        let (resp, task) = spawn_stream(cmd, "video/mp4", true).expect("起得来");
        assert_eq!(
            resp.headers().get("access-control-allow-origin").and_then(|v| v.to_str().ok()),
            Some("*"),
            "cors=true 要带放行头"
        );
        let bytes = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            axum::body::to_bytes(resp.into_body(), 1 << 20),
        )
        .await
        .expect("stderr 没并发排空 → 卡在写管道上")
        .expect("body 读得出");
        assert_eq!(&bytes[..], b"OK");
        tokio::time::timeout(std::time::Duration::from_secs(10), task)
            .await
            .expect("正常收尾任务要结束")
            .unwrap();
    }
}
