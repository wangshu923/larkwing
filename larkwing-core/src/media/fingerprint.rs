//! 音频指纹(Haitsma–Kalker 风格)+ 两段音频的「最长公共段」搜索 —— 给「剧集片头 / 片尾自动检测」
//! 当地基(思路 = Plex / Jellyfin 的 intro 检测:片头曲在相邻两集里是同一段音频,取每集开头几分钟
//! 算指纹、找最长公共段,那段就是片头)。
//!
//! **纯函数、无 IO、无 async。** 输入 16 kHz 单声道 PCM:仓库 ffmpeg 解码器 `decode_file_pcm16k`
//! 出的是归一化 f32(`-f f32le`)→ 走 [`fingerprint_f32`];i16 走 [`fingerprint`]。两条路算出的
//! 指纹可以互相比对(i16 量化只是一点扰动)。
//!
//! ## 指纹怎么算
//! 每 [`HOP_LEN`] 个样本开一个 [`FRAME_LEN`] 长的 Hann 窗 → FFT → 16 个对数分布频带(200–3500 Hz)
//! 的能量。每个哈希位 = **相邻频带能量差的时间差分符号**(15 位)+ 1 位整体能量的时间差分符号,
//! 每帧一个 `u16`。差分两次的好处:整体增益(音量归一 / 0.7 倍)、频响的缓慢倾斜都被消掉,只剩
//! 「频谱形状随时间怎么变」—— 正是同一段音乐在两集里不变的东西。
//!
//! ## 公共段怎么找
//! 对每个相对偏移逐帧算汉明距离(误码率 BER),滑窗平滑后找「平滑 BER ≤ max_ber」的最长连续段,
//! 长度够 `min_secs` 才算命中;多候选取最长、再取 BER 最低。O(Na·Nb):两条 6 分钟音频约 1.1 万帧
//! 各、1.3 亿次 16 位汉明比较,release 下半秒内。命中后再用变点估计(CUSUM)把起止点钉准 ——
//! 阈值法的边界会随实际 BER 高低往里 / 往外偏,变点估计不受这个影响。
//!
//! ## 静音
//! 数字静音(黑场、片头前的空白)在两集里逐字节相同,若照常比对会被当成「公共段」。所以每帧带一个
//! `silent` 标记(窗内 RMS 低于 [`SILENCE_RMS`]),比对时**任一边静音的帧按随机水平计**(16 位错 8 位)
//! —— 静音不提供证据,既不算匹配也不算不匹配。

use rustfft::num_complex::Complex;
use rustfft::FftPlanner;

// ───────────────────────── 常量(单源;改动请连同下面的理由一起改)─────────────────────────

/// 输入采样率。与 `decode_file_pcm16k`(ffmpeg `-ar 16000 -ac 1`)一致,也是语音链全程的采样率。
pub const SAMPLE_RATE: usize = 16_000;

/// 分析帧长(样本数)= 256 ms。频率分辩率 3.9 Hz/bin,最低频带(200–239 Hz)仍有 10 个 bin;
/// 帧越长边界越糊(起止点只能钉到约半帧),256 ms 是「频带能量估得稳」与「边界够准」的折中。
pub const FRAME_LEN: usize = 4096;

/// 帧步进(样本数)= 32 ms,**8:1 重叠**。这里刻意比常见的「步进 = 半帧」细得多,原因是**两集的
/// 片头落在各自帧网格上的相位不同**(相位差在 ±半步内均匀分布),时间差分位对这个错位很敏感:
/// 按 Hann 窗自相关推算,噪声类内容在半步错位下位翻转率 —— 2:1 重叠(步进 128 ms)≈ 0.43、
/// 4:1 ≈ 0.30、8:1 ≈ 0.17、16:1 ≈ 0.06。阈值 0.35 下 2:1 会让相当比例的集对根本匹配不上;
/// 8:1 最坏 0.17,留出余量。代价是帧数 ×4、O(N²) 比对 ×16(6 分钟仍在半秒内)。
/// Haitsma–Kalker 原文用 32:1(370 ms 帧 / 11.6 ms 步)正是同一个理由。
pub const HOP_LEN: usize = 512;

/// [`HOP_LEN`] 换算成秒(`512 / 16000`;有测试钉着两者一致,别只改一处)。
pub const HOP_SECS: f64 = 0.032;

/// 第 k 个哈希的时间戳偏移(秒):哈希 k 由窗 k、k+1 差分而来,合起来覆盖样本
/// `[k·HOP, k·HOP + FRAME + HOP)`,取其中点 `(FRAME_LEN + HOP_LEN) / 2 / SAMPLE_RATE`。
/// 用中点而不是起点,边界上升沿的对称中心才落在真实起止时刻上(见 [`Fingerprint::frame_time_secs`])。
pub const FRAME_CENTER_SECS: f64 = 0.144;

/// 频带数。15 个相邻带差分位 + 1 个整体能量位 = 16 位,恰好一个 `u16`。
pub const BANDS: usize = 16;
/// 最低频带下沿(Hz)。200 以下多是隆隆声 / 低频噪声,对区分片头帮助小、还容易被响度处理动到。
pub const BAND_LOW_HZ: f64 = 200.0;
/// 最高频带上沿(Hz)。3500 以下是人声 / 旋律的主体;再往上不同集的编码(有损压缩的高频截断)
/// 差异开始显现,留下来反而是噪声。也天然容得下 8 kHz 采样的来源。
pub const BAND_HIGH_HZ: f64 = 3500.0;

/// 静音判据:窗内 RMS 低于 −60 dBFS(以满幅 = 1.0 计)。广播内容响度约 −24 LUFS,−60 dBFS 已是
/// 「实际上没声」;抖动噪声(−90 dBFS 级)也归静音 —— 它的哈希位本就是随机的,不值得比对。
pub const SILENCE_RMS: f32 = 0.001;

/// 每帧哈希位数。
pub const HASH_BITS: u32 = 16;
/// 「任一边静音」的帧对按这个错位数计 = 16 位的一半 = 随机水平(不提供证据)。
const NEUTRAL_ERR_BITS: u32 = HASH_BITS / 2;

/// 默认最短公共段(秒)。片头曲通常 20–90 s;15 s 以下的巧合段(相同的转场音效、同一段配乐
/// 片段)不值得当片头处理。
pub const DEFAULT_MIN_SECS: f64 = 15.0;
/// 默认平滑误码率上限。随机水平 0.5,同源音频在最坏网格错位下 ≈ 0.17(见 [`HOP_LEN`]),再叠一层
/// 重编码 / 响度处理也难到 0.3;0.35 两头都留了余量。
pub const DEFAULT_MAX_BER: f64 = 0.35;
/// 默认平滑窗长(秒)。1 s ≈ 31 帧:够抹平单帧的随机抖动,又不至于把 15 s 的段边界糊掉太多。
pub const DEFAULT_SMOOTH_SECS: f64 = 1.0;

// ───────────────────────────────────── 指纹 ─────────────────────────────────────

/// 一段音频的指纹。`frames[k]` 与 `silent[k]` 一一对应;第 k 个哈希的时刻见 [`Self::frame_time_secs`]。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Fingerprint {
    /// 帧步进(秒)= [`HOP_SECS`]。带在结构里是为了让消费方不必认识本模块的常量。
    pub hop_secs: f64,
    /// 每帧一个 16 位哈希(见模块文档)。
    pub frames: Vec<u16>,
    /// 与 `frames` 平行:这一帧(它差分用到的两个窗之一)是不是静音。静音帧比对时按随机水平计。
    pub silent: Vec<bool>,
}

impl Fingerprint {
    /// 第 `k` 个哈希代表的时刻(秒)= 它覆盖的样本区间的中点。
    pub fn frame_time_secs(&self, k: usize) -> f64 {
        k as f64 * self.hop_secs + FRAME_CENTER_SECS
    }
}

/// 16 kHz 单声道 i16 PCM → 指纹。太短(凑不出两个完整窗)→ `frames` 为空,不 panic。
pub fn fingerprint(pcm16k: &[i16]) -> Fingerprint {
    // 归一到 [-1, 1):与 ffmpeg f32le 出口同一量纲,静音阈值才对得上。
    analyze(pcm16k.len(), |i| pcm16k[i] as f32 * (1.0 / 32768.0))
}

/// 16 kHz 单声道归一化 f32 PCM(`decode_file_pcm16k` 的出口)→ 指纹。语义同 [`fingerprint`]。
pub fn fingerprint_f32(pcm16k: &[f32]) -> Fingerprint {
    analyze(pcm16k.len(), |i| pcm16k[i])
}

/// 一个窗的能量剖面。
struct BandEnergy {
    bands: [f64; BANDS],
    total: f64,
    silent: bool,
}

fn analyze(len: usize, sample: impl Fn(usize) -> f32) -> Fingerprint {
    let windows = if len >= FRAME_LEN { (len - FRAME_LEN) / HOP_LEN + 1 } else { 0 };
    if windows < 2 {
        return Fingerprint { hop_secs: HOP_SECS, frames: Vec::new(), silent: Vec::new() };
    }
    let edges = band_edges();
    let hann: Vec<f32> = (0..FRAME_LEN)
        .map(|t| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * t as f32 / FRAME_LEN as f32).cos())
        .collect();
    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(FRAME_LEN);
    let mut buf = vec![Complex::new(0.0f32, 0.0f32); FRAME_LEN];
    let mut scratch = vec![Complex::new(0.0f32, 0.0f32); fft.get_inplace_scratch_len()];

    let mut acc = HashAcc {
        prev: None,
        frames: Vec::with_capacity(windows - 1),
        silent: Vec::with_capacity(windows - 1),
    };
    // 两个实数窗打包进一次复数 FFT(实部 = 窗 m、虚部 = 窗 m+1),按共轭对称拆回:
    // X1[k] = (Z[k] + conj Z[N−k]) / 2,X2[k] = (Z[k] − conj Z[N−k]) / (2i);只要模方,÷4 即可。
    // FFT 次数减半(debug 下的单测、release 下 6 分钟音频都省一半)。窗数为奇时最后一窗虚部填 0。
    for m in (0..windows).step_by(2) {
        let has_second = m + 1 < windows;
        let start1 = m * HOP_LEN;
        let start2 = start1 + HOP_LEN;
        let (mut sq1, mut sq2) = (0.0f64, 0.0f64);
        for (t, slot) in buf.iter_mut().enumerate() {
            let x1 = sample(start1 + t);
            let x2 = if has_second { sample(start2 + t) } else { 0.0 };
            sq1 += f64::from(x1) * f64::from(x1);
            sq2 += f64::from(x2) * f64::from(x2);
            *slot = Complex::new(x1 * hann[t], x2 * hann[t]);
        }
        fft.process_with_scratch(&mut buf, &mut scratch);
        let mut bands1 = [0.0f64; BANDS];
        let mut bands2 = [0.0f64; BANDS];
        for (b, (e1, e2)) in bands1.iter_mut().zip(bands2.iter_mut()).enumerate() {
            for k in edges[b]..edges[b + 1] {
                let z = buf[k];
                let zc = buf[FRAME_LEN - k].conj();
                *e1 += f64::from((z + zc).norm_sqr());
                *e2 += f64::from((z - zc).norm_sqr());
            }
            *e1 *= 0.25;
            *e2 *= 0.25;
        }
        acc.push(BandEnergy::new(bands1, sq1));
        if has_second {
            acc.push(BandEnergy::new(bands2, sq2));
        }
    }
    Fingerprint { hop_secs: HOP_SECS, frames: acc.frames, silent: acc.silent }
}

impl BandEnergy {
    /// `sum_sq` = 窗内(未加窗)样本平方和,拿来判静音。
    fn new(bands: [f64; BANDS], sum_sq: f64) -> Self {
        let rms = (sum_sq / FRAME_LEN as f64).sqrt();
        BandEnergy {
            bands,
            total: bands.iter().sum(),
            // NaN 输入这里判「不静音」、差分位全 0 —— 不 panic,结果只是这帧没信息。
            silent: rms < f64::from(SILENCE_RMS),
        }
    }
}

/// 逐窗喂进来、攒出哈希序列(哈希是相邻两窗的差分,所以留住上一窗)。
struct HashAcc {
    prev: Option<BandEnergy>,
    frames: Vec<u16>,
    silent: Vec<bool>,
}

impl HashAcc {
    fn push(&mut self, cur: BandEnergy) {
        if let Some(p) = &self.prev {
            self.frames.push(hash_pair(p, &cur));
            self.silent.push(p.silent || cur.silent);
        }
        self.prev = Some(cur);
    }
}

/// 相邻两窗 → 16 位哈希:位 b(0..15)= 相邻频带能量差的时间差分是否为正;位 15 = 整体能量是否上升。
fn hash_pair(prev: &BandEnergy, cur: &BandEnergy) -> u16 {
    let mut h = 0u16;
    for b in 0..BANDS - 1 {
        let d = (cur.bands[b] - cur.bands[b + 1]) - (prev.bands[b] - prev.bands[b + 1]);
        if d > 0.0 {
            h |= 1 << b;
        }
    }
    if cur.total - prev.total > 0.0 {
        h |= 1 << (BANDS - 1);
    }
    h
}

/// 16 个对数分布频带的 FFT bin 边界(17 个,左闭右开)。当前常量下最窄的一带也有 10 个 bin;
/// 这里仍保证严格递增,免得改常量后「每带至少一个 bin」的前提静默失效。
fn band_edges() -> [usize; BANDS + 1] {
    let ratio = BAND_HIGH_HZ / BAND_LOW_HZ;
    let hz_per_bin = SAMPLE_RATE as f64 / FRAME_LEN as f64;
    let mut edges = [0usize; BANDS + 1];
    for (i, e) in edges.iter_mut().enumerate() {
        let hz = BAND_LOW_HZ * ratio.powf(i as f64 / BANDS as f64);
        *e = ((hz / hz_per_bin).round() as usize).min(FRAME_LEN / 2);
    }
    // 直流 bin(k=0)不进频带:它没有共轭镜像(N−0 越界),而且直流本来就不是「声音」。
    edges[0] = edges[0].max(1);
    for i in 1..=BANDS {
        let floor = edges[i - 1] + 1;
        if edges[i] < floor {
            edges[i] = floor.min(FRAME_LEN / 2);
        }
    }
    edges
}

// ───────────────────────────────────── 比对 ─────────────────────────────────────

/// 公共段搜索参数。
#[derive(Debug, Clone, PartialEq)]
pub struct MatchOpts {
    /// 最短公共段(秒),不够这么长不算命中。
    pub min_secs: f64,
    /// 平滑后的误码率上限(0 = 全对,0.5 = 随机)。
    pub max_ber: f64,
    /// 平滑窗总帧数(以 [`Fingerprint::hop_secs`] 计;实际取两侧各 `smooth_frames / 2` 帧的奇数窗,
    /// 0 或 1 = 不平滑)。
    pub smooth_frames: usize,
}

impl Default for MatchOpts {
    fn default() -> Self {
        Self {
            min_secs: DEFAULT_MIN_SECS,
            max_ber: DEFAULT_MAX_BER,
            smooth_frames: (DEFAULT_SMOOTH_SECS / HOP_SECS).round() as usize,
        }
    }
}

/// 两段音频的公共段。起点是各自音频里的绝对时刻(秒),`len_secs` 两边相同。
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CommonSegment {
    pub a_start_secs: f64,
    pub b_start_secs: f64,
    pub len_secs: f64,
    /// 这一段内逐帧的平均误码率(0 = 全对)。
    pub ber: f64,
}

/// 候选段(内部):相对偏移 + 在重叠区里的帧范围 + 错位总数。
struct Candidate {
    offset: isize,
    start: usize,
    len: usize,
    err_sum: u32,
}

impl Candidate {
    /// 取最长;同长取错得少的;再平就留先到的(偏移从小到大扫,结果确定)。
    fn beats(&self, other: &Candidate) -> bool {
        self.len > other.len || (self.len == other.len && self.err_sum < other.err_sum)
    }
}

/// 找两段指纹的最长公共段。任一边不够 `min_secs` 长、或没有一段平滑 BER 压得住 `max_ber` → `None`。
/// 两个指纹须由本模块算出(`hop_secs` 不同 → `None`,那是拿错了东西比)。
pub fn common_segment(a: &Fingerprint, b: &Fingerprint, opts: &MatchOpts) -> Option<CommonSegment> {
    if a.hop_secs.is_nan() || a.hop_secs <= 0.0 || (a.hop_secs - b.hop_secs).abs() > 1e-9 {
        return None;
    }
    // `frames` 与 `silent` 本该等长;不等就按短的算,别越界。
    let na = a.frames.len().min(a.silent.len());
    let nb = b.frames.len().min(b.silent.len());
    let min_frames = frames_for_secs(opts.min_secs, a.hop_secs);
    if na < min_frames || nb < min_frames {
        return None;
    }
    let half = opts.smooth_frames / 2;
    // 每帧允许的平均错位数(比对时全用整数累加,只在这一步乘出来)。
    let thr_bits = opts.max_ber * f64::from(HASH_BITS);

    let mut err: Vec<u8> = Vec::with_capacity(na.min(nb));
    let mut prefix: Vec<u32> = Vec::with_capacity(na.min(nb) + 1);
    let mut best: Option<Candidate> = None;
    // 相对偏移 d = b 的帧号 − a 的帧号,扫遍所有有重叠的取值。
    for d in -(na as isize - 1)..=(nb as isize - 1) {
        let (i0, j0, l) = overlap(na, nb, d);
        if l < min_frames {
            continue;
        }
        fill_errors(a, b, i0, j0, l, &mut err, &mut prefix);
        scan_runs(&prefix, half, thr_bits, min_frames, |start, len| {
            let cand = Candidate { offset: d, start, len, err_sum: prefix[start + len] - prefix[start] };
            let better = match &best {
                Some(cur) => cand.beats(cur),
                None => true,
            };
            if better {
                best = Some(cand);
            }
        });
    }
    let best = best?;

    // 起止点用变点估计钉准(平滑阈值法的边界会随段内 BER 高低偏移半个窗)。
    let (i0, j0, l) = overlap(na, nb, best.offset);
    fill_errors(a, b, i0, j0, l, &mut err, &mut prefix);
    let radius = opts.smooth_frames.max(1);
    let (s, e) = refine_bounds(&err, &prefix, best.start, best.start + best.len, radius, min_frames);
    let bits = f64::from(prefix[e] - prefix[s]);
    Some(CommonSegment {
        a_start_secs: a.frame_time_secs(i0 + s),
        b_start_secs: b.frame_time_secs(j0 + s),
        len_secs: (e - s) as f64 * a.hop_secs,
        ber: bits / ((e - s) as f64 * f64::from(HASH_BITS)),
    })
}

/// 秒 → 帧数(向上取整,至少 1 帧;非法值〔负 / NaN〕当 1 帧,不 panic)。
fn frames_for_secs(secs: f64, hop: f64) -> usize {
    let n = (secs / hop).ceil();
    if n.is_finite() && n >= 1.0 {
        n as usize
    } else {
        1
    }
}

/// 偏移 d 下两条指纹的重叠区:a 从 i0 起、b 从 j0 起、共 l 帧。
fn overlap(na: usize, nb: usize, d: isize) -> (usize, usize, usize) {
    let i0 = if d < 0 { (-d) as usize } else { 0 };
    let j0 = (i0 as isize + d) as usize;
    let l = (na - i0).min(nb - j0);
    (i0, j0, l)
}

/// 重叠区逐帧错位数(静音对按随机水平计)+ 前缀和(`prefix[k]` = 前 k 帧错位总数)。
fn fill_errors(
    a: &Fingerprint,
    b: &Fingerprint,
    i0: usize,
    j0: usize,
    l: usize,
    err: &mut Vec<u8>,
    prefix: &mut Vec<u32>,
) {
    err.clear();
    prefix.clear();
    prefix.push(0);
    let mut acc = 0u32;
    let frames = a.frames[i0..i0 + l].iter().zip(&b.frames[j0..j0 + l]);
    let silents = a.silent[i0..i0 + l].iter().zip(&b.silent[j0..j0 + l]);
    for ((&fa, &fb), (&sa, &sb)) in frames.zip(silents) {
        let e = if sa || sb { NEUTRAL_ERR_BITS } else { (fa ^ fb).count_ones() };
        err.push(e as u8);
        acc += e;
        prefix.push(acc);
    }
}

/// 沿重叠区扫「平滑错位率 ≤ 阈值」的连续段,够长的交给 `on_run(start, len)`。
/// 平滑 = 以 k 为中心、两侧各 `half` 帧的窗(边缘处窗被截短,按实际帧数取均值)。
fn scan_runs(prefix: &[u32], half: usize, thr_bits: f64, min_frames: usize, mut on_run: impl FnMut(usize, usize)) {
    let l = prefix.len() - 1;
    let mut run_start: Option<usize> = None;
    for k in 0..l {
        let lo = k.saturating_sub(half);
        let hi = (k + half + 1).min(l);
        let sum = prefix[hi] - prefix[lo];
        let ok = f64::from(sum) <= thr_bits * (hi - lo) as f64;
        match (ok, run_start) {
            (true, None) => run_start = Some(k),
            (false, Some(s)) => {
                if k - s >= min_frames {
                    on_run(s, k - s);
                }
                run_start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = run_start {
        if l - s >= min_frames {
            on_run(s, l - s);
        }
    }
}

/// 变点估计(CUSUM)钉边界:把逐帧错位数看成「段外 ≈ 随机水平、段内 ≈ 段均值」的阶跃,在核心段
/// 起点 / 终点各自 ±`radius` 帧内找最像阶跃发生处的位置(= 累计偏差的极值点,等价于最小二乘拟合)。
/// 平滑阈值法的边界依赖 BER 与阈值的相对高低(BER 低就往外扩、高就往里缩),这一步不受其影响。
/// 钉完若短于 `min_frames`(极端情形),退回核心段。
fn refine_bounds(
    err: &[u8],
    prefix: &[u32],
    rs: usize,
    re: usize,
    radius: usize,
    min_frames: usize,
) -> (usize, usize) {
    let l = err.len();
    let mean_in = f64::from(prefix[re] - prefix[rs]) / (re - rs) as f64;
    // 段内水平与随机水平的中线:错位数低于它算「像段内」,高于它算「像段外」。
    let mid = (mean_in + f64::from(NEUTRAL_ERR_BITS)) / 2.0;

    // 起点:候选 s ∈ [lo, hi],取「lo..s 的累计 (mid − err) 最小」处 —— 段外帧拉低累计、段内帧抬高。
    let lo = rs.saturating_sub(radius);
    let hi = (rs + radius).min(re - 1);
    let mut best_s = lo;
    let mut best_c = 0.0f64;
    let mut c = 0.0f64;
    for (k, &e) in err.iter().enumerate().take(hi).skip(lo) {
        c += mid - f64::from(e);
        if c < best_c {
            best_c = c;
            best_s = k + 1;
        }
    }

    // 终点:候选 e ∈ [lo_e, hi_e],取「lo_e..e 的累计 (mid − err) 最大」处;并列取靠后的。
    let lo_e = re.saturating_sub(radius).max(best_s + 1);
    let hi_e = (re + radius).min(l);
    let mut best_e = lo_e;
    best_c = 0.0;
    c = 0.0;
    for (k, &e) in err.iter().enumerate().take(hi_e).skip(lo_e) {
        c += mid - f64::from(e);
        if c >= best_c {
            best_c = c;
            best_e = k + 1;
        }
    }

    if best_e - best_s < min_frames {
        (rs, re)
    } else {
        (best_s, best_e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 起点容差(秒):规格按 128 ms 步进说的「≤ 1 帧」,这里步进换成了 32 ms,容差按时间守住不变。
    const START_TOL: f64 = 0.128;
    /// 长度容差(秒):同上,「≤ 2 帧」。
    const LEN_TOL: f64 = 0.256;

    /// 确定性 LCG(Knuth MMIX 参数),不引 rand;跨平台逐位一致。
    struct Lcg(u64);
    impl Lcg {
        fn next_u64(&mut self) -> u64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            self.0
        }
        /// 均匀分布 [-1, 1)。
        fn next_unit(&mut self) -> f32 {
            let v = (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64;
            (v * 2.0 - 1.0) as f32
        }
    }

    /// 白噪声(峰值 0.3、RMS ≈ 0.17),`secs` 秒。
    fn noise(seed: u64, secs: f64) -> Vec<f32> {
        let mut lcg = Lcg(seed);
        let n = (secs * SAMPLE_RATE as f64) as usize;
        (0..n).map(|_| 0.3 * lcg.next_unit()).collect()
    }

    /// 把 `clip` 原样盖进 `track` 的 `at_secs` 处(片头就是这么「嵌」在每集里的)。
    fn embed(track: &mut [f32], at_secs: f64, clip: &[f32]) {
        let at = (at_secs * SAMPLE_RATE as f64) as usize;
        track[at..at + clip.len()].copy_from_slice(clip);
    }

    fn to_i16(pcm: &[f32]) -> Vec<i16> {
        pcm.iter().map(|&x| (x * 32767.0).round() as i16).collect()
    }

    /// 两条不同背景音里嵌同一段 20 s「片头」(a 在 30 s、b 在 95 s):要找到,且起点 / 长度钉准。
    /// a 走 i16 入口、b 走 f32 入口 —— 顺带证两条路算出的指纹能互相比对。
    #[test]
    fn finds_shared_intro_at_different_offsets() {
        let intro = noise(1, 20.0);
        let mut a = noise(2, 60.0);
        embed(&mut a, 30.0, &intro);
        let mut b = noise(3, 120.0);
        embed(&mut b, 95.0, &intro);

        let fa = fingerprint(&to_i16(&a));
        let fb = fingerprint_f32(&b);
        let seg = common_segment(&fa, &fb, &MatchOpts::default()).expect("同一段片头应被找到");
        assert!((seg.a_start_secs - 30.0).abs() <= START_TOL, "a 起点 {}", seg.a_start_secs);
        assert!((seg.b_start_secs - 95.0).abs() <= START_TOL, "b 起点 {}", seg.b_start_secs);
        assert!((seg.len_secs - 20.0).abs() <= LEN_TOL, "长度 {}", seg.len_secs);
        assert!(seg.ber <= DEFAULT_MAX_BER, "BER {}", seg.ber);
    }

    /// 第二份拷贝叠小幅白噪(SNR ≈ 14 dB)+ 增益 0.7(模拟重编码 / 响度归一)仍能匹配。
    #[test]
    fn survives_noise_and_gain_change() {
        let intro = noise(1, 20.0);
        let mut a = noise(2, 60.0);
        embed(&mut a, 30.0, &intro);
        let mut b = noise(3, 120.0);
        embed(&mut b, 95.0, &intro);
        let mut hiss = Lcg(4);
        for x in b.iter_mut() {
            *x = 0.7 * (*x + 0.2 * 0.3 * hiss.next_unit());
        }

        let fa = fingerprint_f32(&a);
        let fb = fingerprint_f32(&b);
        let seg = common_segment(&fa, &fb, &MatchOpts::default()).expect("加噪 + 变增益后仍应找到");
        assert!((seg.a_start_secs - 30.0).abs() <= START_TOL, "a 起点 {}", seg.a_start_secs);
        assert!((seg.b_start_secs - 95.0).abs() <= START_TOL, "b 起点 {}", seg.b_start_secs);
        assert!((seg.len_secs - 20.0).abs() <= LEN_TOL, "长度 {}", seg.len_secs);
        assert!(seg.ber <= DEFAULT_MAX_BER, "BER {}", seg.ber);
    }

    /// 两段毫不相关的信号 → None(哪个偏移都凑不出 15 s 的低 BER 段)。
    #[test]
    fn unrelated_signals_yield_none() {
        let fa = fingerprint_f32(&noise(5, 60.0));
        let fb = fingerprint_f32(&noise(6, 60.0));
        assert_eq!(common_segment(&fa, &fb, &MatchOpts::default()), None);
    }

    /// 极短输入:指纹为空 / 不够最短段 → None,全程不 panic。
    #[test]
    fn too_short_inputs_yield_none() {
        let empty = fingerprint(&[]);
        assert!(empty.frames.is_empty() && empty.silent.is_empty());
        let tiny = fingerprint(&[0i16; FRAME_LEN + HOP_LEN - 1]); // 凑不出第二个窗
        assert!(tiny.frames.is_empty());
        let two_windows = fingerprint(&[0i16; FRAME_LEN + HOP_LEN]);
        assert_eq!(two_windows.frames.len(), 1);

        let long = fingerprint_f32(&noise(7, 60.0));
        let short = fingerprint_f32(&noise(7, 5.0)); // 同种子:内容是 long 的前缀,但不够 15 s
        let opts = MatchOpts::default();
        assert_eq!(common_segment(&empty, &long, &opts), None);
        assert_eq!(common_segment(&long, &empty, &opts), None);
        assert_eq!(common_segment(&tiny, &tiny, &opts), None);
        assert_eq!(common_segment(&short, &long, &opts), None);
        // 把最短段放宽到 3 s,同一前缀就该命中在两边的 0 s。
        let loose = MatchOpts { min_secs: 3.0, ..MatchOpts::default() };
        let seg = common_segment(&short, &long, &loose).expect("前缀相同,放宽后应命中");
        assert!(seg.a_start_secs <= START_TOL + FRAME_CENTER_SECS, "a 起点 {}", seg.a_start_secs);
        assert!(seg.b_start_secs <= START_TOL + FRAME_CENTER_SECS, "b 起点 {}", seg.b_start_secs);
        assert!(seg.len_secs >= 4.0, "长度 {}", seg.len_secs);
    }

    /// 两边都是数字静音 → 逐帧标 silent、绝不算公共段(否则两集片头前的黑场会被当片头)。
    #[test]
    fn digital_silence_never_matches() {
        let quiet = fingerprint(&vec![0i16; 30 * SAMPLE_RATE]);
        assert!(!quiet.frames.is_empty());
        assert!(quiet.silent.iter().all(|&s| s));
        assert!(quiet.frames.iter().all(|&h| h == 0));
        assert_eq!(common_segment(&quiet, &quiet, &MatchOpts::default()), None);
        // 静音夹在两条有声音频中间也不算:各嵌 20 s 静音,其余是不同的噪声。
        let mut a = noise(8, 60.0);
        let mut b = noise(9, 60.0);
        let gap = vec![0.0f32; 20 * SAMPLE_RATE];
        embed(&mut a, 20.0, &gap);
        embed(&mut b, 30.0, &gap);
        assert_eq!(common_segment(&fingerprint_f32(&a), &fingerprint_f32(&b), &MatchOpts::default()), None);
    }

    /// 整体增益不改哈希(差分两次把增益消干净;0.5 倍在浮点里是精确缩放,位应逐个相同)。
    #[test]
    fn hash_is_gain_invariant() {
        let x = noise(10, 8.0);
        let half: Vec<f32> = x.iter().map(|v| v * 0.5).collect();
        let fx = fingerprint_f32(&x);
        let fh = fingerprint_f32(&half);
        assert_eq!(fx.frames, fh.frames);
        assert_eq!(fx.silent, fh.silent);
        assert!(fx.silent.iter().all(|&s| !s));
    }

    /// 常量之间的派生关系钉死(只改一处会漂)。
    #[test]
    fn constants_are_consistent() {
        assert_eq!(HOP_SECS, HOP_LEN as f64 / SAMPLE_RATE as f64);
        assert_eq!(FRAME_CENTER_SECS, (FRAME_LEN + HOP_LEN) as f64 / 2.0 / SAMPLE_RATE as f64);
        assert_eq!(FRAME_LEN % HOP_LEN, 0, "帧长应是步进的整数倍");
        let smooth = MatchOpts::default().smooth_frames;
        assert_eq!(smooth % 2, 1, "默认平滑窗取奇数,中心才对得准");
        assert!((smooth as f64 * HOP_SECS - DEFAULT_SMOOTH_SECS).abs() < HOP_SECS);
        let edges = band_edges();
        assert!(edges.windows(2).all(|w| w[0] < w[1]), "频带边界应严格递增: {edges:?}");
        assert!(edges[BANDS] <= FRAME_LEN / 2);
        assert!(edges[0] >= 1, "0 Hz 不该进最低频带");
    }
}
