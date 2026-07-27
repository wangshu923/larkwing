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
    // 兜底去 Windows 长路径前缀(根因修在 datadir,这里防其它来源):verbatim 形关闭 `/`→`\`
    // 归一化,sherpa 内部拼 `espeak-ng-data/phontab` 会「文件不存在」(2026-07-03 真机破案)。
    let model_dir = crate::datadir::simplify(model_dir);
    let model_dir = model_dir.as_path();
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

// ---- 克隆音色输出响度归一(2026-07-27)----
//
// **为什么必须做**:ZipVoice(承 F5-TTS 那套)对参考音的处理是「比 `target_rms` 轻就先放大
// 到 target 喂进模型,合成完再按同一比例缩回去」——最终响度**照抄参考音的响度**,
// `zipvoice_config` 里的 `target_rms` 是条件化旋钮、不是音量旋钮,调它没用。真机实锤
// (2026-07-27):参考音 −21.0 dBFS(3.35s 短参考,当初为降延迟换的)→ 全部合成落 −20 LUFS
// 上下、语音可懂带(300Hz–3kHz)比云端音色还低 2.3 dB;BT 又是低沉男声、能量压在低频,
// 笔记本喇叭砍掉低频后就是「声音特别小」。参考音侧救不了(峰值只剩 3 dB 余量,推高参考还会
// 动音色条件化那半边),只能在输出侧归一。
//
// 目标 **−16 LUFS**(用户拍板;§4.11 常量单源在此)。**只归一克隆音色**——云端音色自带母带
// (用户拍板不动),故 EdgeTts / SherpaVits 两条路字节零变化。链路 = 门限 RMS 定增益 →
// 前视限幅(不是硬 clip,尖峰不出削波失真)。纯 Rust,**ffmpeg 不进 TTS 链路**(§7.5)。
// 参数在真机缓存的 7 段真实合成上标定:落 −16.0~−16.5 LUFS、峰值 ≤ −0.4 dBFS、增益 +0.7~+4.9 dB。
const LOUDNESS_TARGET_RMS: f32 = 0.18; // 门限 RMS 目标(标定值,≈ −16 LUFS)
const LOUDNESS_GATE: f32 = 0.01; // ≈ −40 dBFS:算 RMS 只数出声的样本
const LOUDNESS_MAX_GAIN: f32 = 4.0; // +12 dB 封顶:参考音录得再轻也不把噪底抬穿
const LOUDNESS_MIN_GAIN: f32 = 0.5; // −6 dB 兜底:参考音响过头也拉回来
const LIMITER_CEILING: f32 = 0.95; // ≈ −0.45 dBFS,留头防 i16 量化溢出
const LIMITER_LOOKAHEAD_MS: f32 = 5.0;
const LIMITER_RELEASE_MS: f32 = 60.0;

/// 把一段合成 PCM 归一到 `LOUDNESS_TARGET_RMS`(门限 RMS)再限幅。返回施加的静态增益
/// (进日志,真机可核「到底提了几 dB」)。**原地改**,不额外拷一份。
fn normalize_loudness(samples: &mut [f32], rate: u32) -> f32 {
    if samples.is_empty() || rate == 0 {
        return 1.0;
    }
    // 门限:静音不参与 RMS,否则「话少静音多」的短句(应答音就这形)会被过度放大。
    let mut sq = 0.0f64;
    let mut voiced = 0u64;
    for &s in samples.iter() {
        if s.abs() >= LOUDNESS_GATE {
            sq += (s as f64) * (s as f64);
            voiced += 1;
        }
    }
    if voiced == 0 {
        return 1.0; // 整段近静音:不动(放大噪声毫无意义)
    }
    let rms = (sq / voiced as f64).sqrt() as f32;
    if rms <= 0.0 {
        return 1.0;
    }
    let gain = (LOUDNESS_TARGET_RMS / rms).clamp(LOUDNESS_MIN_GAIN, LOUDNESS_MAX_GAIN);
    for s in samples.iter_mut() {
        *s *= gain;
    }
    // 前视限幅:提前知道即将到来的尖峰 → 瞬时压下(不过冲)、指数回升(不抽气)。
    let look = ((rate as f32 * LIMITER_LOOKAHEAD_MS / 1000.0) as usize).max(1);
    let peaks = lookahead_peaks(samples, look);
    let release = (-1.0 / (rate as f32 * LIMITER_RELEASE_MS / 1000.0)).exp();
    let mut reduction = 1.0f32;
    for (s, peak) in samples.iter_mut().zip(peaks) {
        let need = if peak <= LIMITER_CEILING { 1.0 } else { LIMITER_CEILING / peak };
        reduction = if need < reduction {
            need // attack:前视已看见尖峰,立刻到位
        } else {
            (need + (reduction - need) * release).min(1.0) // release:指数回升
        };
        *s *= reduction;
    }
    gain
}

/// `out[i] = 窗口 [i, i+win) 内最大 |x|`(单调队列 O(n))。反着扫 = 标准「尾窗最大值」。
fn lookahead_peaks(x: &[f32], win: usize) -> Vec<f32> {
    let n = x.len();
    let mut out = vec![0.0; n];
    let mut dq: std::collections::VecDeque<usize> = std::collections::VecDeque::new();
    let at = |k: usize| x[n - 1 - k].abs(); // 反向索引 k ↔ 原索引 n-1-k
    for k in 0..n {
        while dq.back().is_some_and(|&j| at(j) <= at(k)) {
            dq.pop_back();
        }
        dq.push_back(k);
        if dq.front().is_some_and(|&j| j + win <= k) {
            dq.pop_front();
        }
        out[n - 1 - k] = at(*dq.front().expect("刚 push 过,必非空"));
    }
    out
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
        ensure!(!audio.samples().is_empty(), "音色克隆返回了空音频");
        // 响度归一(见上方注释):不做的话输出响度 = 参考音录多轻就多轻。
        let mut samples = audio.samples().to_vec();
        let rate = audio.sample_rate() as u32;
        let gain = normalize_loudness(&mut samples, rate);
        let wav = pcm_f32_to_wav(&samples, rate);
        tracing::info!(
            ms = t0.elapsed().as_millis() as u64,
            chars = text.chars().count(),
            gain_db = format!("{:+.1}", 20.0 * gain.log10()),
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
/// 克隆音色的**渲染版本** = 掺进 cache_key 的盐。克隆产物经 `normalize_loudness` 归一,
/// 归一参数变了旧缓存就该失效——否则改完参数,用过的句子(**含开机预合成的应答音银行**)
/// 还照播老响度。**改任一响度常量 = 这个数字 +1**。只掺克隆音色:云端/离线音色不走归一、
/// 字节与本版本无关,没必要连带作废它们的缓存(edge 作废还要重新联网合成)。
const CLONE_RENDER_VERSION: u32 = 1;

pub fn cache_key(voice: &str, rate_pct: i32, text: &str) -> String {
    let mut h = sha2::Sha256::new();
    h.update(voice.as_bytes());
    h.update(b"|");
    h.update(rate_pct.to_le_bytes());
    h.update(b"|");
    if voice.starts_with("clone:") {
        h.update(CLONE_RENDER_VERSION.to_le_bytes());
        h.update(b"|");
    }
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

    /// 克隆产物的字节随响度归一参数变 → 版本盐必须掺进克隆键(否则改完参数旧缓存照播老响度);
    /// 云端/离线音色不走归一 → 键里**不**该有盐(不连带作废、免重新联网合成)。
    #[test]
    fn clone_cache_key_carries_render_version() {
        let unsalted = |voice: &str, text: &str| {
            let mut h = sha2::Sha256::new();
            h.update(voice.as_bytes());
            h.update(b"|");
            h.update(0i32.to_le_bytes());
            h.update(b"|");
            h.update(text.as_bytes());
            h.finalize().iter().map(|b| format!("{b:02x}")).collect::<String>()
        };
        assert_ne!(cache_key("clone:a", 0, "你好"), unsalted("clone:a", "你好"), "克隆要带盐");
        assert_eq!(cache_key("v1", 0, "你好"), unsalted("v1", "你好"), "非克隆不带盐");
    }

    /// 造一段「有静音有话」的测试信号:前后各 0.1s 静音,中间 240Hz 正弦(拟低沉男声)
    /// 到指定 RMS。正弦 RMS = 幅值/√2,故幅值 = rms·√2。(注意:门限会剔掉零穿越附近的小样本,
    /// 所以**门限** RMS 会略高于这里给的标称 rms——断言留了余量。)
    fn speechlike(rate: u32, secs: f32, rms: f32) -> Vec<f32> {
        let amp = rms * std::f32::consts::SQRT_2;
        let pad = (rate as f32 * 0.1) as usize;
        let voiced = (rate as f32 * secs) as usize;
        let mut v = vec![0.0f32; pad];
        for i in 0..voiced {
            let t = i as f32 / rate as f32;
            v.push(amp * (2.0 * std::f32::consts::PI * 240.0 * t).sin());
        }
        v.resize(v.len() + pad, 0.0); // 尾部静音(repeat_n 要 1.82,MSRV 是 1.77.2)
        v
    }

    fn gated_rms_of(x: &[f32]) -> f32 {
        let v: Vec<f32> = x.iter().copied().filter(|s| s.abs() >= LOUDNESS_GATE).collect();
        (v.iter().map(|s| s * s).sum::<f32>() / v.len() as f32).sqrt()
    }

    /// 录得轻的参考音 → 合成轻(真机症状本尊):归一后落到目标响度,且静音段仍是静音。
    #[test]
    fn loudness_lifts_quiet_output() {
        let mut x = speechlike(24_000, 1.0, 0.088); // 真机参考音就是这个量级(−21 dBFS)
        let gain = normalize_loudness(&mut x, 24_000);
        assert!(gain > 1.9 && gain < 2.1, "该提 ~2x,实际 {gain}");
        let got = gated_rms_of(&x);
        assert!(
            (got - LOUDNESS_TARGET_RMS).abs() < 0.02,
            "该落在目标响度附近,实际 {got}"
        );
        assert_eq!(x[0], 0.0, "静音段不该被抬起");
    }

    /// 限幅是软的:尖峰一律压在天花板下,**绝不**靠 clamp 削平(削波在低沉男声上尤其难听)。
    #[test]
    fn loudness_limits_peaks_below_ceiling() {
        let mut x = speechlike(24_000, 0.5, 0.09);
        let spike = x.len() / 2;
        x[spike] = 0.95; // 一记远高于平均的瞬态,乘完增益必超 1.0
        normalize_loudness(&mut x, 24_000);
        let peak = x.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(peak <= LIMITER_CEILING + 1e-4, "峰值该被限住,实际 {peak}");
        // 远离尖峰处仍有信号(限幅是压增益不是挖空)。看一个窗口的峰值——单点会撞上正弦零穿越。
        let before = x[spike - 2200..spike - 2000].iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(before > 0.05, "限幅不该把远处的话压没,实际 {before}");
    }

    /// 响过头的参考音也拉回来(双向一致,换音色不忽大忽小);增益有上下封顶。
    #[test]
    fn loudness_pulls_down_too_loud_and_respects_caps() {
        let mut loud = speechlike(24_000, 0.5, 0.6);
        assert_eq!(normalize_loudness(&mut loud, 24_000), LOUDNESS_MIN_GAIN, "衰减触底封顶");
        // 0.03 已在门限之上(整段低于门限走的是「近静音不动」那条,见下一个测试),
        // 但离目标差 6x → 该被 +12 dB 封顶挡住,不把噪底一起抬穿。
        let mut faint = speechlike(24_000, 0.5, 0.03);
        assert_eq!(normalize_loudness(&mut faint, 24_000), LOUDNESS_MAX_GAIN, "增益触顶封顶");
    }

    /// 整段近静音 / 空音频:不动、不放大噪声、不 NaN。
    #[test]
    fn loudness_leaves_silence_alone() {
        let mut silent = vec![0.0f32; 4800];
        assert_eq!(normalize_loudness(&mut silent, 24_000), 1.0);
        assert!(silent.iter().all(|s| *s == 0.0));
        let mut empty: Vec<f32> = Vec::new();
        assert_eq!(normalize_loudness(&mut empty, 24_000), 1.0);
        let mut no_rate = vec![0.1f32; 100];
        assert_eq!(normalize_loudness(&mut no_rate, 0), 1.0, "采样率 0 不该除零");
    }

    /// 响度归一的**真音频标定夹具**(手动跑;合成信号测不出真语音的动态)。把真实合成的 wav
    /// 过一遍归一,产物用 `ffmpeg -af ebur128` 量 LUFS/峰值 —— 改任何响度常量都该重跑这条,
    /// 确认还落在 −16 LUFS 档。跑法(可给多个文件):
    /// `LW_LOUDNESS_WAVS="a.wav,b.wav" LW_LOUDNESS_OUT=/tmp/norm \
    ///   cargo test -p larkwing-core --lib loudness_real_calibration -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn loudness_real_calibration() {
        let list = std::env::var("LW_LOUDNESS_WAVS").expect("LW_LOUDNESS_WAVS");
        let out_dir = std::env::var("LW_LOUDNESS_OUT").unwrap_or_else(|_| "/tmp/lw-loudness".into());
        std::fs::create_dir_all(&out_dir).expect("建输出目录");
        for path in list.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let wave = sherpa_onnx::Wave::read(path).expect("读 wav");
            let mut samples = wave.samples().to_vec();
            let before = gated_rms_of(&samples);
            let gain = normalize_loudness(&mut samples, wave.sample_rate() as u32);
            let after = gated_rms_of(&samples);
            let peak = samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));
            let name = Path::new(path).file_name().unwrap().to_string_lossy();
            let out = Path::new(&out_dir).join(format!("norm-{name}"));
            std::fs::write(&out, pcm_f32_to_wav(&samples, wave.sample_rate() as u32))
                .expect("写产物");
            println!(
                "{name}: 门限RMS {before:.4} → {after:.4}(目标 {LOUDNESS_TARGET_RMS})\
                 、增益 {:+.1} dB、峰值 {peak:.3} → {}",
                20.0 * gain.log10(),
                out.display()
            );
            assert!(peak <= LIMITER_CEILING + 1e-4, "峰值越过天花板");
        }
    }

    #[test]
    fn lookahead_peaks_is_forward_window_max() {
        let x = [0.1, -0.9, 0.2, 0.3, -0.4];
        assert_eq!(lookahead_peaks(&x, 2), vec![0.9, 0.9, 0.3, 0.4, 0.4], "窗口 [i, i+2)");
        assert_eq!(lookahead_peaks(&x, 1), vec![0.1, 0.9, 0.2, 0.3, 0.4], "窗口 1 = 自己");
        assert_eq!(lookahead_peaks(&x, 99), vec![0.9, 0.9, 0.4, 0.4, 0.4], "窗口超长 = 后缀最大");
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
