//! 能力轴:把本机音频「听」成带时间戳的句子。`read_image` 的耳朵版 —— 模型自己决定
//! 要不要听、听哪一段(§5 正交原语:歌、语音留言、录音、有声书都是同一个动作)。
//!
//! 出的是「第几秒 → 听成了什么」,**时间轴来自音频本身**:配歌词时模型拿自己知道的
//! 正确歌词按这条轴对上去写 .lrc —— 两边的弱点正好互补(模型不知道时间、ASR 不认字),
//! 而且不碰 §7.1「绝不编造时间轴」那条线(编的是凭空,这条是听来的)。
//! 顺带白拿「听核」:文件名/标签跟里面唱的对不对得上,听一句就知道。
//!
//! 机器 = 现成件三段:ffmpeg 解码(media)→ silero VAD 切句 → SenseVoice/FireRed 逐段转写
//! (voice)。零新组件、零新模型;识别质量跟着设置里的「识别模型」走。

use anyhow::Context;
use async_trait::async_trait;

use super::{Tool, ToolCtx, ToolSpec};

/// 一次最多听多长(秒):一首歌几分钟绰绰有余;有声书那种得靠 `from` 分段听,
/// 免得一次解码几百 MB PCM 堵死内存(16k f32 ≈ 64KB/秒 → 10 分钟 ≈ 38MB)。
const MAX_SECS: u32 = 600;
/// 一页多少句(fs 读类同款「一页 + 报总数 + 给续读起点」形态)。
const PAGE_LINES: usize = 200;

pub(super) struct ReadAudio {
    spec: ToolSpec,
}

impl ReadAudio {
    pub(super) fn new() -> ReadAudio {
        ReadAudio {
            spec: ToolSpec {
                name: "read_audio",
                description: "听本机的音频文件,返回「第几秒说/唱了什么」(逐句带时间戳)。\
                              用来:给歌配歌词时拿到真实的时间轴(自己知道歌词、听它对轴)、\
                              核对文件名跟里面唱的是不是一首、听录音/语音留言的内容。\
                              识别是听来的、会有错字(唱歌尤其),别把它当权威歌词;\
                              时间戳是准的,写 .lrc 只能抄这里的时间,绝不许自己编。\
                              一次最多听 10 分钟,长音频用 from 分段听。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "音频/视频文件绝对路径(支持 ~ 开头 = 用户主目录)"
                        },
                        "from": {
                            "type": "number",
                            "description": "从第几秒开始听(缺省 0)。长音频分段听时填上一段的结尾"
                        },
                        "offset": {
                            "type": "integer",
                            "description": "从第几句开始返回(0 起,缺省 0)。结果说「继续带 offset=N」就填那个 N"
                        }
                    },
                    "required": ["path"]
                }),
                // 听完整首歌 = 解码 + 逐段识别,比别的读类工具慢得多;回合内等,不转后台
                //(结果得当场回到模型手里才接得上下一步,§7.1 read 类工具口径)。
                timeout: std::time::Duration::from_secs(300),
                ui_key: "tool.read_audio",
            },
        }
    }
}

/// 秒 → `[mm:ss.xx]`(.lrc 的原生形,模型抄过去就能用)。
fn stamp(sec: f64) -> String {
    let cs = (sec * 100.0).round().max(0.0) as u64; // 百分之一秒
    let (m, s, c) = (cs / 6000, (cs / 100) % 60, cs % 100);
    format!("[{m:02}:{s:02}.{c:02}]")
}

#[async_trait]
impl Tool for ReadAudio {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> anyhow::Result<String> {
        let path = args
            .get("path")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(super::expand_home) // 「~/xxx」宽容展开(§4.4)
            .context("缺少 path 参数")?;
        super::guard::ensure(ctx, super::guard::Access::Read, std::slice::from_ref(&path)).await?;
        let from = args.get("from").and_then(serde_json::Value::as_f64).unwrap_or(0.0).max(0.0);
        let offset = super::arg_u64(&args, "offset", 0) as usize;

        let Some(voice) = ctx.voice.clone() else {
            anyhow::bail!("这台机器上语音组件没就绪,听不了音频");
        };
        let p = std::path::PathBuf::from(&path);
        anyhow::ensure!(p.is_file(), "{path} 不是文件或不存在");

        let pcm = ctx.media.decode_file_pcm16k(&p, from, MAX_SECS).await?;
        let heard_secs = pcm.len() as f64 / 16_000.0;
        let lines = voice.transcribe_timed(pcm).await?;
        if lines.is_empty() {
            return Ok(format!(
                "听完了(从第 {from:.0} 秒起 {heard_secs:.0} 秒),但没听出人声 —— \
                 可能是纯音乐/伴奏,或者这段里没人说话。"
            ));
        }

        let total = lines.len();
        anyhow::ensure!(offset < total, "一共听出 {total} 句,offset={offset} 超出末尾");
        let page: Vec<String> = lines
            .into_iter()
            .skip(offset)
            .take(PAGE_LINES)
            .map(|(at, text)| format!("{} {text}", stamp(at + from))) // 时间戳按整文件算,不按这一段
            .collect();
        let end = offset + page.len();

        let mut out = format!(
            "听出 {total} 句(第 {from:.0}-{:.0} 秒;识别会有错字,时间戳可信):\n{}",
            from + heard_secs,
            page.join("\n")
        );
        if end < total {
            out.push_str(&format!("\n…(这是第 {}-{end} 句;继续带 offset={end})", offset + 1));
        }
        if heard_secs >= f64::from(MAX_SECS) - 1.0 {
            out.push_str(&format!(
                "\n(只听了 {MAX_SECS} 秒就到上限了,后面还有的话带 from={:.0} 接着听)",
                from + heard_secs
            ));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamp_formats_lrc_timestamps() {
        assert_eq!(stamp(0.0), "[00:00.00]");
        assert_eq!(stamp(12.34), "[00:12.34]");
        assert_eq!(stamp(61.5), "[01:01.50]");
        assert_eq!(stamp(-1.0), "[00:00.00]", "负数不该出现,出现也别崩");
        assert_eq!(stamp(3599.99), "[59:59.99]");
    }

    /// 真音频探针(手动跑):`LW_AUDIO_PROBE=/路径/某首歌.mp3 cargo test -p larkwing-core
    /// --lib read_audio::tests::real -- --ignored --nocapture`。
    /// **这就是「先听一首看看值不值得做」那一步** —— 直接把逐句转写打出来,拿真儿歌/真流行歌
    /// 各跑一首,看错字率决定要不要往下投(会按需下载 ffmpeg + VAD/ASR 模型,首跑慢)。
    #[tokio::test]
    #[ignore]
    async fn real_audio_transcribe_probe() {
        let Ok(path) = std::env::var("LW_AUDIO_PROBE") else {
            eprintln!("设 LW_AUDIO_PROBE=音频路径 再跑");
            return;
        };
        let dir = std::env::temp_dir().join("lw-audio-probe");
        std::fs::create_dir_all(&dir).unwrap();
        let store = crate::store::Store::open(&dir.join("t.db")).unwrap();
        store.users.ensure_default_user().unwrap();
        let media = crate::media::MediaRuntime::detached(store.clone());
        // 指向真实数据目录的 voice/ 就不必重下模型(LW_VOICE_DIR=…/larkwing/voice)
        let voice_dir = std::env::var("LW_VOICE_DIR").map(std::path::PathBuf::from).unwrap_or(dir.join("voice"));
        let voice = crate::voice::VoiceRuntime::new(
            voice_dir,
            store,
            crate::bus::Bus::new(),
            crate::scenes::Scenes::builtin(),
        );
        let t0 = std::time::Instant::now();
        let pcm = media.decode_file_pcm16k(std::path::Path::new(&path), 0.0, MAX_SECS).await.unwrap();
        let secs = pcm.len() as f64 / 16_000.0;
        let lines = voice.transcribe_timed(pcm).await.unwrap();
        eprintln!("—— {path}\n音频 {secs:.1}s,听出 {} 句,耗时 {:?}", lines.len(), t0.elapsed());
        for (at, text) in &lines {
            eprintln!("{} {text}", stamp(*at));
        }
    }

    /// 没注入语音运行时(core 单测/eval/headless)= 如实说听不了,不装能听(§3.5)。
    #[tokio::test]
    async fn without_voice_runtime_it_says_so() {
        let dir = std::env::temp_dir().join(format!("lw-readaudio-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = crate::store::Store::open(&dir.join("t.db")).unwrap();
        store.users.ensure_default_user().unwrap();
        let f = dir.join("歌.mp3");
        std::fs::write(&f, b"not really audio").unwrap();
        let ctx = ToolCtx {
            user_id: 1,
            conv_id: 1,
            media: crate::media::MediaRuntime::detached(store.clone()),
            store,
            web: None,
            voice: None,
            confirm: None,
            grants: Default::default(),
            agent: None,
        };
        let e = ReadAudio::new()
            .run(serde_json::json!({ "path": f.to_string_lossy() }), &ctx)
            .await
            .unwrap_err();
        assert!(format!("{e:#}").contains("语音组件没就绪"), "{e:#}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
