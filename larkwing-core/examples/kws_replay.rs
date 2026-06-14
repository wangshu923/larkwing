//! 回放一段真实录音,判 KWS 是否在唤醒词上命中——配合 wake.rs 的 LARKWING_KWS_DUMP_DIR
//! 落盘 wav 用:把 Windows 上喊「旺财」时落盘的 16k wav 喂进来。
//!   ✓ 命中 → 喂给 KWS 的音频没问题,真因在活体循环时序(看那一刻的「状态切换」日志)
//!   ✗ 不命中 → 采集质量(声道下混/重采样)坐实,音频本身就唤不醒
//!   cargo run -p larkwing-core --example kws_replay -- <model_dir> <wav> [关键词=旺财]

use std::collections::HashSet;
use std::path::Path;

use pinyin::ToPinyin;

// ---- 与 wake.rs / kws_probe 同款的纯编码逻辑(自包含) ----
fn load_vocab(tokens_txt: &Path) -> HashSet<String> {
    let text = std::fs::read_to_string(tokens_txt).expect("读 tokens.txt");
    text.lines().filter_map(|l| l.split_whitespace().next()).map(str::to_string).collect()
}

fn split_syllable(s: &str, vocab: &HashSet<String>) -> Option<Vec<String>> {
    if vocab.contains(s) {
        return Some(vec![s.to_string()]);
    }
    let chars: Vec<char> = s.chars().collect();
    for plen in [2usize, 1] {
        if chars.len() > plen {
            let head: String = chars[..plen].iter().collect();
            let tail: String = chars[plen..].iter().collect();
            if vocab.contains(&head) && vocab.contains(&tail) {
                return Some(vec![head, tail]);
            }
        }
    }
    None
}

fn encode_keyword(word: &str, vocab: &HashSet<String>) -> Option<String> {
    let mut tokens = Vec::new();
    for ch in word.chars() {
        let py = ch.to_pinyin()?;
        tokens.append(&mut split_syllable(py.with_tone(), vocab)?);
    }
    (!tokens.is_empty()).then(|| format!("{} @{}", tokens.join(" "), word))
}

fn make_spotter(dir: &str, kw_buf: &str, thr: f32, score: f32, use_int8: bool) -> sherpa_onnx::KeywordSpotter {
    let mut kcfg = sherpa_onnx::KeywordSpotterConfig::default();
    let sfx = if use_int8 { "int8.onnx" } else { "onnx" };
    let p = |n: &str| Some(Path::new(dir).join(n).to_string_lossy().into_owned());
    kcfg.model_config.transducer.encoder = p(&format!("encoder-epoch-12-avg-2-chunk-16-left-64.{sfx}"));
    kcfg.model_config.transducer.decoder = p(&format!("decoder-epoch-12-avg-2-chunk-16-left-64.{sfx}"));
    kcfg.model_config.transducer.joiner = p(&format!("joiner-epoch-12-avg-2-chunk-16-left-64.{sfx}"));
    kcfg.model_config.tokens = p("tokens.txt");
    kcfg.keywords_threshold = thr;
    kcfg.keywords_score = score;
    kcfg.keywords_buf = Some(kw_buf.to_string());
    sherpa_onnx::KeywordSpotter::create(&kcfg).expect("KWS create 失败")
}

/// 命中的大致时刻(秒);100ms 分块 + 尾部 0.5s 静音 flush(同 wake.rs 喂法)。
fn detect(sp: &sherpa_onnx::KeywordSpotter, samples: &[f32], rate: i32) -> Vec<f32> {
    let st = sp.create_stream();
    let chunk = (rate as usize / 10).max(1);
    let tail = vec![0f32; rate as usize / 2];
    let mut hits = Vec::new();
    let mut pos = 0usize;
    for data in [samples, tail.as_slice()] {
        for c in data.chunks(chunk) {
            st.accept_waveform(rate, c);
            while sp.is_ready(&st) {
                sp.decode(&st);
            }
            if let Some(r) = sp.get_result(&st) {
                if !r.keyword.is_empty() {
                    hits.push(pos as f32 / rate as f32);
                    sp.reset(&st);
                }
            }
            pos += c.len();
        }
    }
    hits
}

fn main() {
    let dir = std::env::args().nth(1).expect("用法: kws_replay <model_dir> <wav> [关键词]");
    let wav = std::env::args().nth(2).expect("缺 <wav>");
    let keyword = std::env::args().nth(3).unwrap_or_else(|| "旺财".to_string());

    let vocab = load_vocab(&Path::new(&dir).join("tokens.txt"));
    let kw_buf = encode_keyword(&keyword, &vocab)
        .unwrap_or_else(|| panic!("关键词「{keyword}」编不进模型词表"));
    println!("关键词「{keyword}」→ {kw_buf:?}");

    let w = sherpa_onnx::Wave::read(&wav).unwrap_or_else(|| panic!("读不了 wav: {wav}"));
    let s = w.samples();
    let rate = w.sample_rate();
    let dur = s.len() as f32 / rate.max(1) as f32;
    let peak = s.iter().fold(0f32, |m, x| m.max((*x).abs()));
    let rms = (s.iter().map(|x| (*x as f64) * (*x as f64)).sum::<f64>() / s.len().max(1) as f64).sqrt();
    let dbfs = 20.0 * peak.max(1e-6).log10();
    println!("\nwav: {wav}");
    println!("  采样率={rate}Hz  时长={dur:.2}s  峰值={peak:.3}(peak_dBFS={dbfs:.1})  rms={rms:.4}");
    if rate != 16000 {
        println!("  ⚠ 非 16k:KWS 期望 16k;wake.rs 落盘的是 16k,这条仅在喂别的录音时提醒");
    }
    if peak < 0.05 {
        println!("  ⚠ 峰值极低:信号过弱,强烈指向采集质量(声道下混砍电平/死声道)");
    }

    // 生产档(int8/1.5/0.45)是关键:它若命中=音频没问题。其余档位看天花板。
    let configs: [(&str, bool, f32, f32); 3] = [
        ("int8 生产档   1.5/0.45", true, 1.5, 0.45),
        ("int8 灵敏拉满 1.5/0.20", true, 1.5, 0.20),
        ("fp32 全精度   1.5/0.45", false, 1.5, 0.45),
    ];
    println!("\n命中判定:");
    for (label, int8, score, thr) in configs {
        // Larkwing 下载目录通常只有 int8;缺 fp32 时跳过而非崩
        let enc = Path::new(&dir)
            .join(format!("encoder-epoch-12-avg-2-chunk-16-left-64.{}", if int8 { "int8.onnx" } else { "onnx" }));
        if !enc.exists() {
            println!("  {label}  →  (跳过:模型目录无此精度文件)");
            continue;
        }
        let sp = make_spotter(&dir, &kw_buf, thr, score, int8);
        let hits = detect(&sp, s, rate);
        let mark = if hits.is_empty() { "✗ 未命中" } else { "✓ 命中" };
        let times: Vec<String> = hits.iter().map(|t| format!("{t:.1}s")).collect();
        println!("  {label}  →  {mark}  ({} 次) {}", hits.len(), times.join(" "));
    }
    println!();
}
