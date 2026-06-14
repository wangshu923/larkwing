//! 临时诊断:用 robot 已下好的真实 KWS 模型 + 官方 test_wavs,验证
//!   A) Larkwing 的唤醒词编码是否和官方 keywords.txt 完全一致
//!   B) keywords_buf(Larkwing 路径,无尾换行) vs +尾换行 vs keywords_file(robot 路径) 命中差异
//!   C) 用户词「旺财/小七」对合成音频(/tmp/kws_*.wav)的命中
//! cargo run -p larkwing-core --example kws_probe -- <model_dir>

use std::collections::HashSet;
use std::path::Path;

use pinyin::ToPinyin;

// ---- 从 wake.rs 原样复制的三个纯函数(自包含,零副作用) ----
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
    vocab.contains(s).then(|| vec![s.to_string()])
}

fn encode_keywords(words: &[String], vocab: &HashSet<String>) -> (String, Vec<String>) {
    let mut lines = Vec::new();
    let mut dropped = Vec::new();
    'word: for word in words {
        let word = word.trim();
        if word.is_empty() {
            continue;
        }
        let mut tokens: Vec<String> = Vec::new();
        for ch in word.chars() {
            let Some(py) = ch.to_pinyin() else {
                dropped.push(word.to_string());
                continue 'word;
            };
            match split_syllable(py.with_tone(), vocab) {
                Some(mut t) => tokens.append(&mut t),
                None => {
                    dropped.push(word.to_string());
                    continue 'word;
                }
            }
        }
        if tokens.is_empty() {
            dropped.push(word.to_string());
            continue;
        }
        lines.push(format!("{} @{}", tokens.join(" "), word));
    }
    (lines.join("\n"), dropped)
}

fn make_spotter(
    dir: &str,
    buf: Option<String>,
    file: Option<String>,
    thr: f32,
    score: f32,
) -> sherpa_onnx::KeywordSpotter {
    let mut kcfg = sherpa_onnx::KeywordSpotterConfig::default();
    let p = |n: &str| Some(Path::new(dir).join(n).to_string_lossy().into_owned());
    kcfg.model_config.transducer.encoder = p("encoder-epoch-12-avg-2-chunk-16-left-64.int8.onnx");
    kcfg.model_config.transducer.decoder = p("decoder-epoch-12-avg-2-chunk-16-left-64.int8.onnx");
    kcfg.model_config.transducer.joiner = p("joiner-epoch-12-avg-2-chunk-16-left-64.int8.onnx");
    kcfg.model_config.tokens = p("tokens.txt");
    kcfg.keywords_threshold = thr;
    kcfg.keywords_score = score;
    kcfg.keywords_buf = buf;
    kcfg.keywords_file = file;
    sherpa_onnx::KeywordSpotter::create(&kcfg).expect("KWS create 失败")
}

/// 喂整段音频(100ms 分块,尾部补 0.5s 静音 flush),收集所有命中关键词。
fn detect(sp: &sherpa_onnx::KeywordSpotter, samples: &[f32], rate: i32) -> Vec<String> {
    let st = sp.create_stream();
    let mut hits = Vec::new();
    let chunk = (rate as usize / 10).max(1);
    let tail = vec![0f32; rate as usize / 2];
    let feed = |st: &sherpa_onnx::OnlineStream, hits: &mut Vec<String>, data: &[f32]| {
        for c in data.chunks(chunk) {
            st.accept_waveform(rate, c);
            while sp.is_ready(st) {
                sp.decode(st);
            }
            if let Some(r) = sp.get_result(st) {
                if !r.keyword.is_empty() {
                    hits.push(r.keyword.clone());
                    sp.reset(st);
                }
            }
        }
    };
    feed(&st, &mut hits, samples);
    feed(&st, &mut hits, &tail);
    hits
}

fn main() {
    let dir = std::env::args().nth(1).expect("用法: kws_probe <model_dir>");
    let vocab = load_vocab(&Path::new(&dir).join("tokens.txt"));

    let raw_words = [
        "你好军哥", "蛋哥蛋哥", "小爱同学", "你好问问", "小艺小艺", "小米小米", "林美丽",
        "你好西西",
    ];
    let words: Vec<String> = raw_words.iter().map(|s| s.to_string()).collect();
    let (buf, dropped) = encode_keywords(&words, &vocab);

    println!("\n================ 实验 A:编码正确性(Larkwing vs 官方 keywords.txt) ================");
    println!("dropped(编不出的词): {dropped:?}");
    let official = std::fs::read_to_string(Path::new(&dir).join("keywords.txt")).unwrap_or_default();
    let official_trim = official.trim_end();
    println!("--- Larkwing encode_keywords 产物 ---\n{buf}");
    println!("--- 官方 keywords.txt(去尾空白) ---\n{official_trim}");
    println!(
        ">>> 编码是否逐字节一致: {}",
        if buf == official_trim { "✅ 完全一致" } else { "❌ 不一致(见上)" }
    );

    // 用户词
    let (wc, dwc) = encode_keywords(&["旺财".into()], &vocab);
    let (xq, dxq) = encode_keywords(&["小七".into()], &vocab);
    println!("\n旺财 => buf={wc:?}  dropped={dwc:?}");
    println!("小七 => buf={xq:?}  dropped={dxq:?}");

    println!("\n================ 实验 B:真人 test_wavs 在不同阈值的命中(buf 路径=Larkwing 方式) ================");
    let tk_path = Path::new(&dir).join("test_wavs/test_keywords.txt");
    let tk = std::fs::read_to_string(&tk_path).expect("读 test_keywords.txt");
    let tk_buf = tk.trim_end().to_string(); // 无尾换行 = Larkwing 生产方式
    for thr in [0.5f32, 0.45, 0.25] {
        let sp = make_spotter(&dir, Some(tk_buf.clone()), None, thr, 1.5);
        let mut hit = 0;
        let mut total = 0;
        let mut detail = Vec::new();
        for i in 0..10 {
            let wpath = Path::new(&dir).join(format!("test_wavs/{i}.wav"));
            let Some(w) = sherpa_onnx::Wave::read(&wpath.to_string_lossy()) else { continue };
            total += 1;
            let h = detect(&sp, w.samples(), w.sample_rate());
            if !h.is_empty() {
                hit += 1;
            }
            detail.push(format!("wav{i}={h:?}"));
        }
        println!("thr={thr} (score=1.5): 真人命中 {hit}/{total}  {}", detail.join("  "));
    }

    println!("\n================ 实验 D:零声母字编码 bug(韵母开头音节被错拆成两 token) ================");
    for w in ["女儿", "小爱同学", "欧洲", "西安", "恩爱", "二"] {
        let (b, d) = encode_keywords(&[w.to_string()], &vocab);
        println!("{w:8} => {b:?}  dropped={d:?}");
    }

    println!("\n================ 实验 C:用户词「旺财/小七」对合成音频命中 ================");
    for (label, kwbuf, wav) in
        [("旺财", &wc, "/tmp/kws_wc.wav"), ("小七", &xq, "/tmp/kws_xq.wav")]
    {
        let Some(w) = sherpa_onnx::Wave::read(wav) else {
            println!("{label}: 合成音 {wav} 读不了,跳过"); continue;
        };
        for thr in [0.5f32, 0.25, 0.1] {
            let sp = make_spotter(&dir, Some(kwbuf.to_string()), None, thr, 1.5);
            let hits = detect(&sp, w.samples(), w.sample_rate());
            println!("{label} (thr={thr}): 命中={hits:?}");
        }
    }
}
