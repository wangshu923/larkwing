//! 能力轴:把一段话「读」成音频文件落到本机 —— `read_audio`(听)的镜像姊妹。
//! 正交原语:只管「文字 → 音频文件」,不管往哪送、也不在这边播 —— 发到家人手机走既有
//! `send_file`(说一段 → 存成文件 → 发出去是模型自己的组合,词汇不进本原语,§5)。
//!
//! 合成走 `VoiceRuntime::tts_to_file`:与聊天念话**同一条链、同一份缓存、同一个音色设置**
//! (含用户克隆的嗓子 `voice.speaker = clone:<id>`),零新引擎;产物从 TTS 缓存**拷**出来
//! 落到下载夹 / 指定目录(缓存件不动),同名 dedupe 永不覆盖(§7.2 三规①)。
//! 扩展名跟着引擎走(edge = mp3 / 离线 vits 与克隆 = wav),本原语不转码。

use std::path::{Path, PathBuf};

use anyhow::Context;
use async_trait::async_trait;

use super::{Tool, ToolCtx, ToolRisk, ToolSpec};
use crate::files::{dedupe_path, default_download_dir, human_size, sanitize_filename};

/// 单次文本上限(字符,不是字节):超了如实退回让模型拆段 / 精简,**绝不静默截断**(§6.5)。
/// 几百字读出来已是几分钟,再长克隆音色在 CPU 上要等很久。§4.11 待用户确认。
const SPEAK_MAX_CHARS: usize = 1000;
/// 模型没给 `name` 时的文件名主干。§4.11 待用户确认。
const DEFAULT_NAME: &str = "留言";
/// `name` 里若顺手带了这些音频扩展名就剥掉再拼真实扩展名 —— 真实格式由引擎定,不剥会
/// 落成「xx.mp3.wav」。入参宽容,arg_bool 同族(§4.4)。
const STRIP_AUDIO_EXTS: &[&str] = &["mp3", "wav", "m4a", "aac", "ogg", "opus", "flac"];

pub(super) struct SpeakToFile {
    spec: ToolSpec,
}

impl SpeakToFile {
    pub(super) fn new() -> SpeakToFile {
        SpeakToFile {
            spec: ToolSpec {
                name: "speak_to_file",
                description: "把一段文字用当前设置的音色读成音频文件(用户克隆的嗓子也照用),\
                              落在下载文件夹或 dir 指定目录,返回文件绝对路径 —— 适合录一段话\
                              给家里人听:生成后用 send_file 把文件发到 TA 手机。只管文字→音频\
                              文件,不在这边播放。text 按口语写、别带 Markdown/表情符号(会被\
                              逐字念出来);有字数上限,超了会退回,长内容拆成几段各读一个文件。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "text": {
                            "type": "string",
                            "description": format!("要读出来的话(口语化纯文本,最多 {SPEAK_MAX_CHARS} 字)")
                        },
                        "name": {
                            "type": "string",
                            "description": format!(
                                "文件名主干,不带扩展名(可选;缺省「{DEFAULT_NAME}」)。同名自动加序号,不覆盖"
                            )
                        },
                        "dir": {
                            "type": "string",
                            "description": "落到哪个目录(可选;缺省 = 系统下载文件夹;支持 ~ 开头)"
                        }
                    },
                    "required": ["text"]
                }),
                // 克隆音色几百字在 CPU 上合成要等好一阵(与 read_audio 同档,回合内等不转后台:
                // 路径得当场回到模型手里才接得上 send_file)
                timeout: std::time::Duration::from_secs(300),
                ui_key: "tool.speak_to_file",
            },
        }
    }
}

/// 解析好的入参(纯函数产物,可单测)。
#[derive(Debug)]
struct Req {
    text: String,
    /// 文件名主干(已清洗,不带扩展名)。
    stem: String,
    dir: PathBuf,
}

fn parse_args(args: &serde_json::Value) -> anyhow::Result<Req> {
    let text = args
        .get("text")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("缺少 text 参数(要读出来的话)"))?;
    let n = text.chars().count();
    anyhow::ensure!(
        n <= SPEAK_MAX_CHARS,
        "这段话太长了({n} 字,上限 {SPEAK_MAX_CHARS} 字)——精简一下,或拆成几段各读一个文件"
    );
    let stem = file_stem(args.get("name").and_then(serde_json::Value::as_str));
    let dir = match args.get("dir").and_then(serde_json::Value::as_str).map(str::trim) {
        Some(d) if !d.is_empty() => {
            let p = PathBuf::from(super::expand_home(d)); // 「~/xxx」宽容展开(§4.4)
            anyhow::ensure!(p.is_absolute(), "dir 需要绝对路径,收到: {d}");
            p
        }
        _ => default_download_dir(),
    };
    Ok(Req { text: text.to_string(), stem, dir })
}

/// 文件名主干:缺省 / 空白 → `DEFAULT_NAME`;剥掉模型顺手带的音频扩展名;非法字符经
/// `sanitize_filename` 清洗(`/` `\` 也在内 → 名字传不进子目录,目录只认 `dir`)。
fn file_stem(name: Option<&str>) -> String {
    let raw = strip_audio_ext(name.map(str::trim).unwrap_or(""));
    // 只剩点 / 空格的名字(「..」「.」「.mp3」)清洗后是空的 → 回落默认名,不借 sanitize 的通用兜底名
    if raw.trim_end_matches([' ', '.']).is_empty() {
        return DEFAULT_NAME.to_string();
    }
    sanitize_filename(raw)
}

fn strip_audio_ext(name: &str) -> &str {
    if let Some((stem, ext)) = name.rsplit_once('.') {
        if STRIP_AUDIO_EXTS.contains(&ext.to_ascii_lowercase().as_str()) {
            return stem;
        }
    }
    name
}

/// 产物落点:`dir/<stem>.<ext>`,同名 dedupe 加序号(永不覆盖,资源管理器口径)。
fn target_path(dir: &Path, stem: &str, ext: &str) -> PathBuf {
    dedupe_path(&dir.join(format!("{stem}.{ext}")))
}

#[async_trait]
impl Tool for SpeakToFile {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    /// 往磁盘落新文件(永不覆盖,dedupe 加序号)。
    fn risk(&self) -> ToolRisk {
        ToolRisk::Mutating
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> anyhow::Result<String> {
        let req = parse_args(&args)?;
        // 先看有没有嗓子再谈落盘:壳层没注入语音运行时(core 单测 / eval / headless)= 如实说,
        // 别先弹一张目录授权卡再告诉用户读不了(§3.5)
        let Some(voice) = ctx.voice.clone() else {
            anyhow::bail!("这台机器上语音组件没就绪,读不成音频");
        };
        // 落盘 = 存入(§7.2 授权圈;缺省「下载」夹在出厂基线内,零打扰)。授权过了才建目录、才合成。
        super::guard::ensure(
            ctx,
            super::guard::Access::Create,
            &[req.dir.to_string_lossy().into_owned()],
        )
        .await?;
        std::fs::create_dir_all(&req.dir)
            .with_context(|| format!("建不了目标文件夹 {}", req.dir.display()))?;

        // 合成:与聊天念话同一条链(音色 / 语速取用户设置,克隆音色走 ZipVoice),产物进 TTS 缓存
        let cached = voice.tts_to_file(&req.text).await.context("读不成音频")?;
        let ext = cached
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_owned)
            .context("TTS 缓存文件没有扩展名,认不出格式")?;
        let out = target_path(&req.dir, &req.stem, &ext);
        // 拷出缓存(缓存件留着:下次同一句同音色秒回),缓存目录在数据根、产物在用户目录
        tokio::fs::copy(&cached, &out)
            .await
            .with_context(|| format!("音频写不到 {}", out.display()))?;
        let size = tokio::fs::metadata(&out).await.map(|m| m.len()).unwrap_or(0);

        Ok(format!(
            "读好了,音频已存到:{}({},{ext} 格式,用的是当前设置的音色)。\
             要给家里人听就用 send_file 把这个文件发到 TA 手机。",
            out.display(),
            human_size(size)
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx(tag: &str) -> (ToolCtx, PathBuf) {
        let dir = std::env::temp_dir().join(format!("lw-speak-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = crate::store::Store::open(&dir.join("t.db")).unwrap();
        let me = store.users.ensure_default_user().unwrap();
        let media = crate::media::MediaRuntime::detached(store.clone());
        (
            ToolCtx {
                user_id: me.id,
                conv_id: 1,
                media,
                store,
                web: None,
                voice: None,
                confirm: None,
                grants: Default::default(),
                agent: None,
            },
            dir,
        )
    }

    #[test]
    fn parse_rejects_missing_and_oversize_text() {
        assert!(parse_args(&json!({})).is_err(), "缺 text 要退回");
        assert!(parse_args(&json!({"text": "   "})).is_err(), "空白 text 要退回");
        let err = parse_args(&json!({"text": "字".repeat(SPEAK_MAX_CHARS + 1)})).unwrap_err();
        assert!(err.to_string().contains("太长"), "超限要给明白话、不截断: {err:#}");
        // 恰好上限放行,且按字符数不按字节(中文一字三字节)
        let ok = parse_args(&json!({"text": "字".repeat(SPEAK_MAX_CHARS)})).unwrap();
        assert_eq!(ok.text.chars().count(), SPEAK_MAX_CHARS);
    }

    #[test]
    fn parse_defaults_dir_and_name_and_expands_home() {
        let r = parse_args(&json!({"text": "你好"})).unwrap();
        assert_eq!(r.dir, default_download_dir(), "缺省落系统下载夹");
        assert_eq!(r.stem, DEFAULT_NAME, "缺省文件名主干");
        assert!(
            parse_args(&json!({"text": "你好", "dir": "relative/x"})).is_err(),
            "相对路径退回"
        );
        if dirs::home_dir().is_some() {
            let r = parse_args(&json!({"text": "你好", "dir": "~/Music"})).unwrap();
            assert!(r.dir.is_absolute(), "~ 要展开成绝对路径: {}", r.dir.display());
            assert!(!r.dir.to_string_lossy().starts_with('~'));
        }
    }

    #[test]
    fn file_stem_cleans_defaults_and_strips_audio_ext() {
        assert_eq!(file_stem(None), DEFAULT_NAME);
        assert_eq!(file_stem(Some("  ")), DEFAULT_NAME);
        assert_eq!(file_stem(Some("..")), DEFAULT_NAME, "只剩点的名字回落默认名");
        assert_eq!(file_stem(Some(".mp3")), DEFAULT_NAME, "光秃扩展名 = 没起名");
        assert_eq!(file_stem(Some("给家里的话")), "给家里的话");
        assert_eq!(file_stem(Some("留言.MP3")), "留言", "带音频扩展名要剥,真实格式由引擎定");
        assert_eq!(file_stem(Some("留言.wav")), "留言");
        assert_eq!(file_stem(Some("v1.2")), "v1.2", "不是音频扩展名的点号原样留");
        assert_eq!(file_stem(Some("a/b:c?")), "a_b_c_", "非法字符清洗(/ 也在内,传不进子目录)");
        assert_eq!(file_stem(Some("CON")), "_CON", "Windows 保留名规避");
    }

    #[test]
    fn target_path_dedupes_never_overwrites() {
        let dir = std::env::temp_dir().join(format!("lw-speak-dedupe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let first = target_path(&dir, DEFAULT_NAME, "mp3");
        assert_eq!(first, dir.join(format!("{DEFAULT_NAME}.mp3")));
        std::fs::write(&first, b"x").unwrap();
        assert_eq!(
            target_path(&dir, DEFAULT_NAME, "mp3"),
            dir.join(format!("{DEFAULT_NAME} (2).mp3")),
            "同名 dedupe 成 (2)(资源管理器口径从 2 起)"
        );
        assert_eq!(
            target_path(&dir, DEFAULT_NAME, "wav"),
            dir.join(format!("{DEFAULT_NAME}.wav")),
            "扩展名不同不算撞名"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 没注入语音运行时(core 单测 / eval / headless)= 如实说读不了,不装能读(§3.5);
    /// 且不留半个文件。
    #[tokio::test]
    async fn without_voice_runtime_it_says_so() {
        let (ctx, dir) = ctx("novoice");
        let e = SpeakToFile::new()
            .run(json!({"text": "你好", "dir": dir.to_string_lossy()}), &ctx)
            .await
            .unwrap_err();
        assert!(format!("{e:#}").contains("语音组件没就绪"), "{e:#}");
        assert!(
            !dir.join(format!("{DEFAULT_NAME}.mp3")).exists()
                && !dir.join(format!("{DEFAULT_NAME}.wav")).exists(),
            "退回时不该落任何产物"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 参数校验先于语音组件检查:超限文本的退回话术是「太长」,不是「没就绪」。
    #[tokio::test]
    async fn oversize_text_is_rejected_before_voice_check() {
        let (ctx, dir) = ctx("long");
        let e = SpeakToFile::new()
            .run(
                json!({"text": "字".repeat(SPEAK_MAX_CHARS + 1), "dir": dir.to_string_lossy()}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(format!("{e:#}").contains("太长"), "{e:#}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 真合成探针(手动跑):
    /// `LW_TTS_OFFLINE=1 LW_VOICE_DIR=…/larkwing/voice cargo test -p larkwing-core --lib \
    ///  real_speak_to_file_probe -- --ignored --nocapture`。看产物落哪、多大、什么格式。
    /// `LW_TTS_OFFLINE=1` = 走离线 vits(模型不在 LW_VOICE_DIR 就用时下载);**不设它会走 edge,
    /// 而 edge 的 websocket TLS 要进程级 rustls provider —— 只有壳层 boot(src-tauri lib.rs)装,
    /// 单测进程里没装 → 合成任务 panic**(不是本工具的 bug,是 msedge-tts 依赖进程默认 provider)。
    #[tokio::test]
    #[ignore]
    async fn real_speak_to_file_probe() {
        let (mut ctx, dir) = ctx("real");
        let voice_dir = std::env::var("LW_VOICE_DIR")
            .map(PathBuf::from)
            .unwrap_or(dir.join("voice"));
        if std::env::var("LW_TTS_OFFLINE").is_ok() {
            ctx.store.settings.set(None, "voice.tts_backend", "offline").unwrap();
        }
        ctx.voice = Some(crate::voice::VoiceRuntime::new(
            voice_dir,
            ctx.store.clone(),
            crate::bus::Bus::new(),
            crate::scenes::Scenes::builtin(),
        ));
        let t0 = std::time::Instant::now();
        let out = SpeakToFile::new()
            .run(
                json!({"text": "这是一段测试用的话,看看能不能读成文件。", "name": "探针", "dir": dir.to_string_lossy()}),
                &ctx,
            )
            .await
            .unwrap();
        eprintln!("{out}\n耗时 {:?}", t0.elapsed());
        assert!(out.contains("探针."), "{out}");
    }
}
