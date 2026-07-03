//! `TtsEngine` trait + EdgeTts(msedge-tts,微软免费在线)+ **按 hash(音色|语速|文本) 落盘缓存**
//! (宪法 §7「少重复 TTS」兑现;blob 不进库走文件)。trait 是接缝承诺:离线 VITS 是
//! PLAN §11 D 期目录档,届时加实现不动调用方。非官方 API 风险记档(PLAN §11 watch)。

use std::path::Path;

use anyhow::{anyhow, ensure, Context, Result};
use sha2::Digest;

pub trait TtsEngine: Send + Sync {
    /// 整句合成到音频字节(格式见 ext)。rate_pct = 语速偏移(%,-15/0/+15 三档映射)。
    fn synthesize(&self, text: &str, voice: &str, rate_pct: i32) -> Result<Vec<u8>>;
    /// 产物容器扩展名(relay 按它给 Content-Type,缓存按它命名):mp3 | wav。
    fn ext(&self) -> &'static str;
    /// 同一实例能否并发调用 synthesize。sherpa 的 OfflineTts(melo/克隆)**非可重入**——
    /// 并发 generate 会在原生层崩溃(整进程退出,无 Rust panic)。默认 false(调用方串行化);
    /// 仅无共享原生状态的引擎(EdgeTts 每次新建 websocket)才声明 true。
    fn reentrant(&self) -> bool {
        false
    }
}

/// 微软 Edge 朗读 API(与 robot 的 edge-tts 同一服务,长期稳定)。
/// 每次合成新建 websocket(简单可靠;句级缓存已大幅摊薄连接成本,池化等真瓶颈再说)。
pub struct EdgeTts;

impl TtsEngine for EdgeTts {
    fn synthesize(&self, text: &str, voice: &str, rate_pct: i32) -> Result<Vec<u8>> {
        let mut client =
            msedge_tts::tts::client::connect().context("TTS 服务连接失败(需要网络)")?;
        let cfg = msedge_tts::tts::SpeechConfig {
            voice_name: voice.to_string(),
            audio_format: "audio-24khz-48kbitrate-mono-mp3".to_string(),
            pitch: 0,
            rate: rate_pct,
            volume: 0,
        };
        let t0 = std::time::Instant::now();
        let audio = client.synthesize(text, &cfg).context("TTS 合成失败")?;
        ensure!(!audio.audio_bytes.is_empty(), "TTS 返回了空音频");
        tracing::info!(
            ms = t0.elapsed().as_millis() as u64,
            chars = text.chars().count(),
            bytes = audio.audio_bytes.len(),
            "TTS 合成完成(edge)"
        );
        Ok(audio.audio_bytes)
    }

    fn ext(&self) -> &'static str {
        "mp3"
    }

    fn reentrant(&self) -> bool {
        true // 每次合成新建 websocket,无共享原生状态,可并发
    }
}

/// 本地离线 VITS(PLAN §11 D;melo-tts 中英双语,断网也能说)。出 wav(WebView 原生可播),
/// 免 mp3 编码依赖。模型贵(163M),加载一次进 OnceCell。
pub struct SherpaVits {
    tts: sherpa_onnx::OfflineTts,
}

impl SherpaVits {
    pub fn load(model_dir: &Path) -> Result<SherpaVits> {
        let p = |n: &str| Some(model_dir.join(n).to_string_lossy().into_owned());
        let mut cfg = sherpa_onnx::OfflineTtsConfig::default();
        cfg.model.vits.model = p("model.onnx");
        cfg.model.vits.lexicon = p("lexicon.txt");
        cfg.model.vits.tokens = p("tokens.txt");
        cfg.model.num_threads = 2;
        // 数字/日期/电话读法规则(melo 离线也要把「3点」读对)
        let fsts = ["date.fst", "number.fst", "phone.fst"]
            .iter()
            .map(|f| model_dir.join(f).to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(",");
        cfg.rule_fsts = Some(fsts);
        let t0 = std::time::Instant::now();
        let tts = sherpa_onnx::OfflineTts::create(&cfg).ok_or_else(|| anyhow!("离线 TTS 加载失败"))?;
        tracing::info!(ms = t0.elapsed().as_millis() as u64, "离线 TTS 模型加载完成(melo-vits)");
        Ok(SherpaVits { tts })
    }
}

impl TtsEngine for SherpaVits {
    fn synthesize(&self, text: &str, _voice: &str, rate_pct: i32) -> Result<Vec<u8>> {
        let cfg = sherpa_onnx::GenerationConfig {
            sid: 0,                                  // melo 单说话人
            speed: 1.0 + rate_pct as f32 / 100.0,    // 语速档(舒缓/标准/轻快)
            ..Default::default()
        };
        let t0 = std::time::Instant::now();
        let audio = self
            .tts
            .generate_with_config(text, &cfg, None::<fn(&[f32], f32) -> bool>)
            .ok_or_else(|| anyhow!("离线 TTS 合成失败"))?;
        let samples = audio.samples();
        ensure!(!samples.is_empty(), "离线 TTS 返回了空音频");
        let wav = pcm_f32_to_wav(samples, audio.sample_rate() as u32);
        tracing::info!(
            ms = t0.elapsed().as_millis() as u64,
            chars = text.chars().count(),
            "TTS 合成完成(离线 vits)"
        );
        Ok(wav)
    }

    fn ext(&self) -> &'static str {
        "wav"
    }
}

/// 克隆音色引用解析:clone-id(已去 `clone:` 前缀)→ (参考音 wav 文件, 文字稿)。
/// 闭包捕获 store,引擎本体因此不依赖具体存储类型(单一真相源 = cloned_voices 库)。
pub type CloneResolver =
    std::sync::Arc<dyn Fn(&str) -> Result<(std::path::PathBuf, String)> + Send + Sync>;

/// 本地零样本音色克隆(ZipVoice,k2-fsa;PLAN §11 D-clone):参考音 prompt_audio(5-30s)
/// + 文字稿 prompt_text 在生成时传入 → 克隆任意说话人,**免训练**;中英双语 distill int8,
/// 跨语种(英文参考音说中文)亦可。出 wav(同 melo,免 mp3 编码)。模型贵,加载一次进 OnceCell。
/// `voice` 参数 = `clone:<id>`,由 `resolve` 闭包查 (参考音 wav 路径, 文字稿)。
pub struct ZipVoiceTts {
    tts: sherpa_onnx::OfflineTts,
    resolve: CloneResolver,
}

/// sherpa `OfflineTts::create` 只回 None、不给缘由 → 自己核一遍必需文件,把线索塞进加载错误(§3.5)。
/// 判定与 `models::TTS_ZIPVOICE.ready` **同一份清单/下界**(单源;旧版 1MB 松门槛把截断的 124MB
/// decoder 也报成「文件齐全」,2026-07-02 Windows 实锤)。区分「缺/过小(→ 自愈重下能救)」和
/// 「真齐全却加载失败(→ 格式/运行时问题,重下没用)」。
fn zipvoice_dir_hint(dir: &Path) -> String {
    let mut bad = Vec::new();
    for (name, min) in super::models::TTS_ZIPVOICE.ready {
        if super::models::tree_item_ok(dir, name, *min) {
            continue;
        }
        match std::fs::metadata(dir.join(name)) {
            Ok(m) => bad.push(format!("{name}={}B(应≥{min})", m.len())),
            Err(_) => bad.push(format!("{name} 缺失")),
        }
    }
    if bad.is_empty() {
        "(文件齐全,疑似格式/运行时问题,非缺文件)".into()
    } else {
        format!("(不完整:{})", bad.join("、"))
    }
}

/// 组一份 ZipVoice 加载配置(`load` 与子进程探针共用同一份——探针要复现的就是这份)。
/// feat_scale/t_shift/target_rms/guidance_scale 取自 sherpa-onnx 官方 `zipvoice_tts` 例子
/// (Default 全 0 会跑不出声),锁死不暴露(同管线参数纪律)。
fn zipvoice_config(model_dir: &Path) -> Result<sherpa_onnx::OfflineTtsConfig> {
    let p = |n: &str| Some(model_dir.join(n).to_string_lossy().into_owned());
    let mut cfg = sherpa_onnx::OfflineTtsConfig::default();
    cfg.model.zipvoice.tokens = p("tokens.txt");
    cfg.model.zipvoice.encoder = p("encoder.int8.onnx");
    cfg.model.zipvoice.decoder = p("decoder.int8.onnx");
    cfg.model.zipvoice.vocoder = p("vocos_24khz.onnx");
    cfg.model.zipvoice.data_dir = p("espeak-ng-data");
    // 多音字补丁:把内置补丁词表合并进下载的 lexicon,纠正「好战」类贪婪误读。
    cfg.model.zipvoice.lexicon =
        Some(merge_polyphone_lexicon(model_dir)?.to_string_lossy().into_owned());
    cfg.model.zipvoice.feat_scale = 0.1;
    cfg.model.zipvoice.t_shift = 0.5;
    cfg.model.zipvoice.target_rms = 0.1;
    cfg.model.zipvoice.guidance_scale = 1.0;
    // CPU 合成线程:克隆音色是本地 ZipVoice,合成耗时随线程数近线性下降;
    // 2 太保守(实测 77 字 ~19s),提到 6(留核给 ASR/LLM/UI)。配短参考音一起降延迟。
    cfg.model.num_threads = 6;
    Ok(cfg)
}

/// 子进程探针本体(壳层 `--probe-zipvoice <dir>` 入口调):用与 `load` 完全相同的配置
/// 重跑一次 create。**存在意义 = 抓 sherpa 的 stderr**:Windows 的 sherpa 预编译库是
/// 静态 CRT(/MT),它的 stderr 与主进程 Rust 侧不是同一张 fd 表,进程内怎么 dup2 都
/// 接不到(native.log 只见 boot 标记的真因,2026-07-03);而**子进程**出生时所有 CRT 都
/// 从父进程给的句柄初始化 fd 2 → 管道全收。返回 create 是否成功。
pub fn probe_zipvoice(model_dir: &Path) -> bool {
    eprintln!("[probe] zipvoice create: {}", model_dir.display());
    let cfg = match zipvoice_config(model_dir) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[probe] 配置构建失败(lexicon 合并):{e:#}");
            return false;
        }
    };
    let ok = sherpa_onnx::OfflineTts::create(&cfg).is_some();
    eprintln!("[probe] create => {}", if ok { "ok" } else { "null(真因见上方 sherpa 输出)" });
    ok
}

impl ZipVoiceTts {
    /// 加载 ZipVoice 模型(encoder/decoder/vocoder/tokens + espeak-ng-data 目录 + lexicon)。
    pub fn load(model_dir: &Path, resolve: CloneResolver) -> Result<ZipVoiceTts> {
        let cfg = zipvoice_config(model_dir)?;
        let t0 = std::time::Instant::now();
        let tts = sherpa_onnx::OfflineTts::create(&cfg).ok_or_else(|| {
            anyhow!(
                "音色克隆模型加载失败{};sherpa 真实报错由自动探针抓取,见 logs/larkwing.log 的「zipvoice 探针」行",
                zipvoice_dir_hint(model_dir)
            )
        })?;
        tracing::info!(ms = t0.elapsed().as_millis() as u64, "音色克隆模型加载完成(zipvoice)");
        Ok(ZipVoiceTts { tts, resolve })
    }
}

/// 多音字补丁词表(随二进制内置)。sherpa 中文前端是贪婪最长匹配、无真正分词,
/// 「好战」会把「做好战斗」的「好」抢成四声。加词 = 改 polyphone_supplement.txt 一行。
const POLYPHONE_SUPPLEMENT: &str = include_str!("polyphone_supplement.txt");

/// 把内置补丁词表合并进下载的 `lexicon.txt`:同名词覆盖、新词追加,保序写出
/// `lexicon.merged.txt` 供合成用(`#`/空行跳过)。每次加载重算,补丁词表是唯一真相源,
/// 模型重新下载也不丢。根治多音字需带分词的 G2P 前端(本前端结构所限,见补丁注释)。
fn merge_polyphone_lexicon(model_dir: &Path) -> Result<std::path::PathBuf> {
    let base_path = model_dir.join("lexicon.txt");
    let base = std::fs::read_to_string(&base_path)
        .with_context(|| format!("读取 lexicon 失败:{}", base_path.display()))?;
    let mut order: Vec<String> = Vec::new();
    let mut map: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for line in base.lines().chain(POLYPHONE_SUPPLEMENT.lines()) {
        let line = line.trim_end();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let word = match line.split_once(' ') {
            Some((w, _)) => w,
            None => continue,
        };
        if !map.contains_key(word) {
            order.push(word.to_string());
        }
        map.insert(word.to_string(), line.to_string());
    }
    let merged = order.iter().map(|w| map[w].as_str()).collect::<Vec<_>>().join("\n");
    let out = model_dir.join("lexicon.merged.txt");
    std::fs::write(&out, merged + "\n")
        .with_context(|| format!("写合并 lexicon 失败:{}", out.display()))?;
    Ok(out)
}

impl TtsEngine for ZipVoiceTts {
    fn synthesize(&self, text: &str, voice: &str, rate_pct: i32) -> Result<Vec<u8>> {
        // voice = "clone:<id>";查参考音 + 文字稿(零样本克隆的命门)。
        let clone_id = voice.strip_prefix("clone:").unwrap_or(voice);
        let (ref_wav, ref_text) =
            (self.resolve)(clone_id).with_context(|| format!("克隆音色 {clone_id} 解析失败"))?;
        let ref_path = ref_wav.to_string_lossy();
        let wave = sherpa_onnx::Wave::read(ref_path.as_ref())
            .ok_or_else(|| anyhow!("参考音读取失败:{}", ref_wav.display()))?;
        let cfg = sherpa_onnx::GenerationConfig {
            speed: 1.0 + rate_pct as f32 / 100.0,
            reference_audio: Some(wave.samples().to_vec()),
            reference_sample_rate: wave.sample_rate(),
            reference_text: Some(ref_text),
            num_steps: 4, // distill 档:官方例子值(质量/速度权衡)
            ..Default::default()
        };
        let t0 = std::time::Instant::now();
        let audio = self
            .tts
            .generate_with_config(text, &cfg, None::<fn(&[f32], f32) -> bool>)
            .ok_or_else(|| anyhow!("音色克隆合成失败"))?;
        let samples = audio.samples();
        ensure!(!samples.is_empty(), "音色克隆返回了空音频");
        let wav = pcm_f32_to_wav(samples, audio.sample_rate() as u32);
        tracing::info!(
            ms = t0.elapsed().as_millis() as u64,
            chars = text.chars().count(),
            "TTS 合成完成(克隆)"
        );
        Ok(wav)
    }

    fn ext(&self) -> &'static str {
        "wav"
    }
}

/// f32 PCM([-1,1])→ 16-bit WAV 字节(44 字节头 + i16 LE;WebView <audio> 原生可播)。
/// pub(super):enrollment 录入也用它把参考音落成 wav(voice/mod.rs)。
pub(super) fn pcm_f32_to_wav(samples: &[f32], rate: u32) -> Vec<u8> {
    let data_len = samples.len() * 2;
    let mut buf = Vec::with_capacity(44 + data_len);
    let byte_rate = rate * 2; // mono, 16-bit
    buf.extend_from_slice(b"RIFF");
    buf.extend_from_slice(&((36 + data_len) as u32).to_le_bytes());
    buf.extend_from_slice(b"WAVE");
    buf.extend_from_slice(b"fmt ");
    buf.extend_from_slice(&16u32.to_le_bytes()); // PCM fmt chunk size
    buf.extend_from_slice(&1u16.to_le_bytes()); // PCM
    buf.extend_from_slice(&1u16.to_le_bytes()); // mono
    buf.extend_from_slice(&rate.to_le_bytes());
    buf.extend_from_slice(&byte_rate.to_le_bytes());
    buf.extend_from_slice(&2u16.to_le_bytes()); // block align
    buf.extend_from_slice(&16u16.to_le_bytes()); // bits/sample
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&(data_len as u32).to_le_bytes());
    for &s in samples {
        buf.extend_from_slice(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
    }
    buf
}

/// 音色目录(中文行,策展自 robot 实测音色表;目录 = 数据,加语言 = 加一组)。
#[derive(Debug, Clone, serde::Serialize)]
pub struct Speaker {
    pub id: &'static str,
    pub name: &'static str,
}

pub const SPEAKERS_ZH: &[Speaker] = &[
    Speaker { id: "zh-CN-XiaoxiaoNeural", name: "晓晓 · 温柔" },
    Speaker { id: "zh-CN-XiaoyiNeural", name: "晓伊 · 可爱" },
    Speaker { id: "zh-CN-YunxiNeural", name: "云希 · 少年" },
    Speaker { id: "zh-CN-YunjianNeural", name: "云健 · 沉稳" },
];

pub const DEFAULT_SPEAKER: &str = "zh-CN-XiaoxiaoNeural";

/// 语速三档(voice.rate,user 级)→ edge 语速偏移百分比。
pub fn rate_pct(rate: &str) -> i32 {
    match rate {
        "slow" => -15,
        "fast" => 15,
        _ => 0, // standard
    }
}

/// 缓存键:音色|语速|文本 的 SHA-256(同句换音色 = 另一份缓存,语义正确)。
pub fn cache_key(voice: &str, rate_pct: i32, text: &str) -> String {
    let mut h = sha2::Sha256::new();
    h.update(voice.as_bytes());
    h.update(b"|");
    h.update(rate_pct.to_le_bytes());
    h.update(b"|");
    h.update(text.as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_key_varies_by_voice_rate_text() {
        let a = cache_key("v1", 0, "你好");
        assert_eq!(a, cache_key("v1", 0, "你好"), "同参定键");
        assert_ne!(a, cache_key("v2", 0, "你好"));
        assert_ne!(a, cache_key("v1", 15, "你好"));
        assert_ne!(a, cache_key("v1", 0, "你好呀"));
    }

    #[test]
    fn rate_tiers_are_locked() {
        assert_eq!(rate_pct("slow"), -15);
        assert_eq!(rate_pct("standard"), 0);
        assert_eq!(rate_pct("fast"), 15);
        assert_eq!(rate_pct("junk"), 0);
    }

    #[test]
    fn speaker_catalog_has_default() {
        assert!(SPEAKERS_ZH.iter().any(|s| s.id == DEFAULT_SPEAKER));
    }

    #[test]
    fn clone_voices_are_cache_namespaced() {
        // 不同克隆 id → 不同缓存键;克隆与内置在线音色互不串(voice 维度已在 cache_key)。
        assert_ne!(cache_key("clone:a", 0, "你好"), cache_key("clone:b", 0, "你好"));
        assert_ne!(cache_key("clone:a", 0, "你好"), cache_key("zh-CN-XiaoxiaoNeural", 0, "你好"));
    }

    /// 真模型冒烟(手动跑,需真模型 + 16k mono 参考 wav):用真 ZipVoice 端到端合成,
    /// 顺带用 SenseVoice 转写参考音 → 验证 asr.rs + ZipVoiceTts 全链。跑法:
    /// `ZIPVOICE_DIR=.. SENSEVOICE_DIR=.. BT_REF=ref.wav OUT=out.wav SAY="..." \
    ///   cargo test -p larkwing-core --lib zipvoice_real_synth -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn zipvoice_real_synth_smoke() {
        use crate::voice::asr::{Asr, SherpaAsr};
        let zv = std::env::var("ZIPVOICE_DIR").expect("ZIPVOICE_DIR");
        let sv = std::env::var("SENSEVOICE_DIR").expect("SENSEVOICE_DIR");
        let ref_wav = std::env::var("BT_REF").expect("BT_REF");
        let out = std::env::var("OUT").unwrap_or_else(|_| "/tmp/bt/bt_says.wav".into());
        let say =
            std::env::var("SAY").unwrap_or_else(|_| "相信我,飞行员。我会保护你。".into());

        // 1) 用 SherpaAsr 转写参考音(16k mono)→ reference_text
        let wave = sherpa_onnx::Wave::read(&ref_wav).expect("read ref wav");
        let asr = SherpaAsr::sense_voice(Path::new(&sv), "zh").expect("load asr");
        let ref_text = asr.transcribe(wave.samples()).expect("transcribe");
        eprintln!("[REF TEXT] {ref_text}");
        assert!(!ref_text.is_empty(), "参考音没转出文字");

        // 2) 用 ZipVoiceTts(真模型)合成 BT 说新中文
        let rp = ref_wav.clone();
        let rt = ref_text.clone();
        let resolve: CloneResolver =
            std::sync::Arc::new(move |_id: &str| Ok((std::path::PathBuf::from(&rp), rt.clone())));
        let tts = ZipVoiceTts::load(Path::new(&zv), resolve).expect("load zipvoice");
        let wav = tts.synthesize(&say, "clone:bt", 0).expect("synthesize");
        assert!(wav.len() > 1000, "合成音频太小");
        std::fs::write(&out, &wav).expect("write out");
        eprintln!("[OUT] {} bytes -> {out}  (说: {say})", wav.len());
    }

    #[test]
    fn wav_header_is_well_formed() {
        let pcm = vec![0.0f32, 0.5, -0.5, 1.0, -1.0];
        let wav = pcm_f32_to_wav(&pcm, 24_000);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(wav.len(), 44 + pcm.len() * 2, "44 头 + i16 数据");
        // 数据区第一个样本 0.0 → 0i16
        assert_eq!(i16::from_le_bytes([wav[44], wav[45]]), 0);
        // 削顶:1.0 → 32767,-1.0 → -32767
        let last = wav.len() - 2;
        assert_eq!(i16::from_le_bytes([wav[last], wav[last + 1]]), -32767);
    }
}
