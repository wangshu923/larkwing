//! 解码成 16k 单声道 PCM —— 语音链(渠道语音消息 / `read_audio`)的入口。
//! 住在 media 是因为它要 ffmpeg;voice 只管拿到 PCM 之后的事。

use super::*;

impl MediaRuntime {
    /// 音频字节(手机语音消息的 ogg/opus 等)→ 16k 单声道 f32 PCM(喂本地 ASR)。
    /// ffmpeg 组件解码:落临时文件(解封装要可 seek 输入更稳)→ `-f f32le` 读 stdout;
    /// `-t max_secs` 双保险截断(调用方已按时长挡长语音)。用完即删,失败如实报错。
    pub async fn decode_audio_pcm16k(&self, bytes: Vec<u8>, max_secs: u32) -> Result<Vec<f32>> {
        use std::sync::atomic::{AtomicU64, Ordering};
        let ffmpeg = self.ensure_component(Component::Ffmpeg).await?;
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let tmp = std::env::temp_dir().join(format!(
            "lw-voicemsg-{}-{}.bin",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        tokio::fs::write(&tmp, &bytes).await.context("语音临时文件写入失败")?;
        let mut cmd = tokio::process::Command::new(&ffmpeg);
        cmd.arg("-hide_banner")
            .arg("-i")
            .arg(&tmp)
            .arg("-t")
            .arg(max_secs.to_string())
            .arg("-f")
            .arg("f32le")
            .arg("-ar")
            .arg("16000")
            .arg("-ac")
            .arg("1")
            .arg("pipe:1");
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        no_console(&mut cmd);
        let out = tokio::time::timeout(std::time::Duration::from_secs(30), cmd.output()).await;
        let _ = tokio::fs::remove_file(&tmp).await;
        let out = out.context("ffmpeg 解码超时")?.context("ffmpeg 起不来")?;
        anyhow::ensure!(out.status.success(), "ffmpeg 解码失败(退出码 {:?})", out.status.code());
        anyhow::ensure!(!out.stdout.is_empty(), "解码出的音频为空");
        Ok(out.stdout.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect())
    }

    /// 本机音频文件 → 16k 单声道 f32 PCM(read_audio 的耳朵喂料)。直接 `-i 路径`,
    /// **不经内存和临时件**(整首歌/整章有声书动辄几十 MB,bytes 版那条路是给手机语音消息的)。
    /// `from_secs` 从第几秒起、`max_secs` 最多解多长(两道都由调用方按量约束定)。
    pub async fn decode_file_pcm16k(
        &self,
        path: &std::path::Path,
        from_secs: f64,
        max_secs: u32,
    ) -> Result<Vec<f32>> {
        let ffmpeg = self.ensure_component(Component::Ffmpeg).await?;
        let mut cmd = tokio::process::Command::new(&ffmpeg);
        cmd.arg("-hide_banner");
        if from_secs > 0.0 {
            cmd.arg("-ss").arg(format!("{from_secs:.3}")); // 输入 seek:长音频只解要听的那段
        }
        cmd.arg("-i")
            .arg(path)
            .arg("-t")
            .arg(max_secs.to_string())
            .arg("-vn") // 带封面图的音乐文件很常见,别把封面当视频流解
            .arg("-f")
            .arg("f32le")
            .arg("-ar")
            .arg("16000")
            .arg("-ac")
            .arg("1")
            .arg("pipe:1");
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        no_console(&mut cmd);
        // 解码本身很快(实时的几十倍),给足余量兜住机械盘/网络盘
        let out = tokio::time::timeout(std::time::Duration::from_secs(120), cmd.output())
            .await
            .context("ffmpeg 解码超时")?
            .context("ffmpeg 起不来")?;
        anyhow::ensure!(out.status.success(), "ffmpeg 解码失败(退出码 {:?})", out.status.code());
        anyhow::ensure!(!out.stdout.is_empty(), "解码出的音频为空(这个文件里没有能听的声音?)");
        Ok(out.stdout.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect())
    }
}
