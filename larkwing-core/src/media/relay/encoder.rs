//! relay · H.264 编码器:硬件优先探测(试编码一帧)+ 各编码器参数收口 `apply_video_encode`(所有转码点唯一出口)。

use super::*;

/// 视频转码用哪个 H.264 编码器。**探测出来的、每 entry 固定**(init 与各段必须同编码器,否则
/// avcC 配置不一致、MSE 拼不上)。有硬件编码器就用 GPU(省 CPU,§硬件加速),没有则回落软件
/// libx264 —— 回落时**逐字节等同旧行为、零回归**。选择由 `detect_video_encoder` 试编码探出、
/// `Relay` 进程级缓存(`Inner.hw_encoder`);"播放失败兜底重放"强制走 `Software`(最兼容)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoEncoder {
    /// libx264(软件,永远可用的回落)。
    Software,
    /// NVIDIA NVENC(`h264_nvenc`)。
    Nvenc,
    /// Intel Quick Sync(`h264_qsv`)。
    Qsv,
    /// AMD AMF(`h264_amf`)。
    Amf,
    /// Apple VideoToolbox(`h264_videotoolbox`,Mac 开发机)。
    VideoToolbox,
}

/// 追加视频编码参数到 ffmpeg 命令。**所有转码点共用这一处**(§4.8 单源):build_frag_cmd(HLS 段 /
/// 自适应视频段)、FileRemux(/m/)。质量目标 crf/cq≈23;硬件路加 `-profile:v high` 保 WebView2
/// 能解;`-pix_fmt yuv420p` 强制 8bit(10bit HEVC 一并压回,浏览器只认 8bit)。
/// **Software 分支保持与旧代码逐字节一致**(veryfast+crf23+yuv420p),不碰已验证的软件路。
pub(super) fn apply_video_encode(cmd: &mut tokio::process::Command, enc: VideoEncoder) {
    cmd.args(video_encode_args(enc));
}

/// 每档编码器的完整参数序列(含收尾的 `-pix_fmt yuv420p`)。apply_video_encode(播放转码链)
/// 与 ffmpeg_run 的「`-c:v h264` 占位替换」(media/edit.rs)共用这一份 —— 改质量旋钮只改
/// 这里(§4.8 单源);序列顺序与历史逐字节一致,老单测钉着。`[1]` 恒为编码器名(报告用)。
pub fn video_encode_args(enc: VideoEncoder) -> &'static [&'static str] {
    match enc {
        VideoEncoder::Software => {
            &["-c:v", "libx264", "-preset", "veryfast", "-crf", "23", "-pix_fmt", "yuv420p"]
        }
        // p5 = 速度/画质折中(p1 最快~p7 最慢);vbr+cq 恒定质量(-b:v 0 让 cq 纯控质量不设码率)。
        VideoEncoder::Nvenc => &[
            "-c:v", "h264_nvenc", "-preset", "p5", "-tune", "hq", "-rc", "vbr", "-cq", "23",
            "-b:v", "0", "-profile:v", "high", "-pix_fmt", "yuv420p",
        ],
        VideoEncoder::Qsv => &[
            "-c:v", "h264_qsv", "-preset", "veryfast", "-global_quality", "23",
            "-profile:v", "high", "-pix_fmt", "yuv420p",
        ],
        VideoEncoder::Amf => &[
            "-c:v", "h264_amf", "-rc", "cqp", "-qp_i", "23", "-qp_p", "23", "-profile:v", "high",
            "-pix_fmt", "yuv420p",
        ],
        VideoEncoder::VideoToolbox => &[
            "-c:v", "h264_videotoolbox", "-q:v", "60", "-profile:v", "high",
            "-pix_fmt", "yuv420p",
        ],
    }
}

/// 试编码一帧探这台机器能不能真用某编码器:编译进 ≠ 能用(如 h264_nvenc 编进了但没 N 卡 →
/// 运行时失败)。`color` 源出一帧喂给编码器 `-f null` 丢弃,退出码成功即可用。带 10s 超时防卡。
async fn probe_encoder(ffmpeg: &Path, name: &str) -> bool {
    let mut cmd = tokio::process::Command::new(ffmpeg);
    cmd.arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-nostdin")
        .arg("-f")
        .arg("lavfi")
        .arg("-i")
        .arg("color=c=black:s=256x256:r=5:d=0.4")
        .arg("-frames:v")
        .arg("1")
        .arg("-c:v")
        .arg(name)
        .arg("-f")
        .arg("null")
        .arg("-");
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    super::no_console(&mut cmd);
    matches!(
        tokio::time::timeout(std::time::Duration::from_secs(10), cmd.status()).await,
        Ok(Ok(s)) if s.success()
    )
}

/// 按平台优先级逐个试编码,取第一个真能用的硬件编码器;都不行回落 `Software`。
/// **探测不假设**(§4.11):从不硬认"有 GPU",全靠实测。整进程探一次(Relay 缓存)。
pub(super) async fn detect_video_encoder(ffmpeg: &Path) -> VideoEncoder {
    let candidates: &[(&str, VideoEncoder)] = if cfg!(target_os = "windows") {
        &[
            ("h264_nvenc", VideoEncoder::Nvenc),
            ("h264_qsv", VideoEncoder::Qsv),
            ("h264_amf", VideoEncoder::Amf),
        ]
    } else if cfg!(target_os = "macos") {
        &[("h264_videotoolbox", VideoEncoder::VideoToolbox)]
    } else {
        &[]
    };
    for (name, enc) in candidates {
        if probe_encoder(ffmpeg, name).await {
            tracing::info!(encoder = name, "硬件视频编码器可用,转码走 GPU");
            return *enc;
        }
    }
    tracing::info!("无可用硬件视频编码器,转码回落 libx264(软件)");
    VideoEncoder::Software
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_video_encode_maps_each_encoder() {
        let args_for = |enc| {
            let mut c = tokio::process::Command::new("ffmpeg");
            apply_video_encode(&mut c, enc);
            c.as_std().get_args().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<String>>()
        };
        // 软件路 = 旧行为逐字节一致(libx264 veryfast crf23 + yuv420p),防回归。
        let sw = args_for(VideoEncoder::Software);
        assert_eq!(
            sw.iter().map(String::as_str).collect::<Vec<_>>(),
            ["-c:v", "libx264", "-preset", "veryfast", "-crf", "23", "-pix_fmt", "yuv420p"]
        );
        // 各硬件路:首个是 -c:v <对应编码器> + high profile + 末尾 -pix_fmt yuv420p。
        for (enc, name) in [
            (VideoEncoder::Nvenc, "h264_nvenc"),
            (VideoEncoder::Qsv, "h264_qsv"),
            (VideoEncoder::Amf, "h264_amf"),
            (VideoEncoder::VideoToolbox, "h264_videotoolbox"),
        ] {
            let a = args_for(enc);
            assert_eq!(a[0].as_str(), "-c:v");
            assert_eq!(a[1].as_str(), name);
            assert!(a.iter().any(|s| s == "high"), "{name} 应带 -profile:v high");
            assert_eq!(a.last().unwrap().as_str(), "yuv420p", "{name} 末尾应 -pix_fmt yuv420p");
        }
    }
}
