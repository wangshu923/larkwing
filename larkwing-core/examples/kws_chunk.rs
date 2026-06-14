//! 复现"活体喂法"是否破坏命中:同一段真实 wav(离线 100ms 块能命中),改成
//! 模拟 live 的喂法——小块(cpal ~10ms 回调)、长流不 reset——看 KWS 还命不命中。
//!   cargo run -p larkwing-core --example kws_chunk -- <model_dir> <wav> [关键词=旺财]

use std::collections::HashSet;
use std::path::Path;

use pinyin::ToPinyin;

fn load_vocab(t: &Path) -> HashSet<String> {
    std::fs::read_to_string(t).expect("tokens").lines().filter_map(|l| l.split_whitespace().next()).map(str::to_string).collect()
}
fn split_syllable(s: &str, v: &HashSet<String>) -> Option<Vec<String>> {
    if v.contains(s) { return Some(vec![s.to_string()]); }
    let c: Vec<char> = s.chars().collect();
    for plen in [2usize, 1] {
        if c.len() > plen {
            let (h, t): (String, String) = (c[..plen].iter().collect(), c[plen..].iter().collect());
            if v.contains(&h) && v.contains(&t) { return Some(vec![h, t]); }
        }
    }
    None
}
fn encode(word: &str, v: &HashSet<String>) -> Option<String> {
    let mut tk = Vec::new();
    for ch in word.chars() { tk.append(&mut split_syllable(ch.to_pinyin()?.with_tone(), v)?); }
    (!tk.is_empty()).then(|| format!("{} @{}", tk.join(" "), word))
}
fn spotter(dir: &str, kw: &str) -> sherpa_onnx::KeywordSpotter {
    let mut c = sherpa_onnx::KeywordSpotterConfig::default();
    let p = |n: &str| Some(Path::new(dir).join(n).to_string_lossy().into_owned());
    c.model_config.transducer.encoder = p("encoder-epoch-12-avg-2-chunk-16-left-64.int8.onnx");
    c.model_config.transducer.decoder = p("decoder-epoch-12-avg-2-chunk-16-left-64.int8.onnx");
    c.model_config.transducer.joiner = p("joiner-epoch-12-avg-2-chunk-16-left-64.int8.onnx");
    c.model_config.tokens = p("tokens.txt");
    c.keywords_threshold = 0.45; // 生产档
    c.keywords_score = 1.5;
    c.keywords_buf = Some(kw.to_string());
    sherpa_onnx::KeywordSpotter::create(&c).expect("KWS")
}

/// 用指定块大小喂整段(每段尾补 0.5s 静音),fresh stream,返回命中次数。
fn hits_at_chunk(sp: &sherpa_onnx::KeywordSpotter, samples: &[f32], rate: i32, chunk: usize) -> usize {
    let st = sp.create_stream();
    let tail = vec![0f32; rate as usize / 2];
    let mut n = 0;
    for data in [samples, tail.as_slice()] {
        for c in data.chunks(chunk.max(1)) {
            st.accept_waveform(rate, c);
            while sp.is_ready(&st) { sp.decode(&st); }
            if let Some(r) = sp.get_result(&st) {
                if !r.keyword.is_empty() { n += 1; sp.reset(&st); }
            }
        }
    }
    n
}

fn main() {
    let dir = std::env::args().nth(1).expect("用法: kws_chunk <model_dir> <wav> [关键词]");
    let wav = std::env::args().nth(2).expect("缺 <wav>");
    let keyword = std::env::args().nth(3).unwrap_or_else(|| "旺财".to_string());
    let vocab = load_vocab(&Path::new(&dir).join("tokens.txt"));
    let kw = encode(&keyword, &vocab).expect("编码失败");
    let w = sherpa_onnx::Wave::read(&wav).expect("读 wav");
    let (s, rate) = (w.samples(), w.sample_rate());
    println!("关键词「{keyword}」{kw:?}  wav={:.1}s @ {rate}Hz\n", s.len() as f32 / rate as f32);

    println!("=== 块大小扫描(同段音频,fresh stream,看小块是否破坏命中)===");
    for ms in [100usize, 50, 30, 20, 10, 5] {
        let ch = rate as usize * ms / 1000;
        let n = hits_at_chunk(&spotter(&dir, &kw), s, rate, ch);
        println!("  块={ms:3}ms ({ch:4} 样本)  →  命中 {n} 次  {}", if n > 0 { "✓" } else { "✗ 不命中!" });
    }

    println!("\n=== 长流不 reset:把整段连喂 3 遍进同一 stream(模拟 live 90s 长流)===");
    let sp = spotter(&dir, &kw);
    let st = sp.create_stream();
    let tail = vec![0f32; rate as usize / 2];
    for round in 1..=3 {
        let mut n = 0;
        for data in [s, tail.as_slice()] {
            for c in data.chunks(rate as usize / 100) { // 10ms 块
                st.accept_waveform(rate, c);
                while sp.is_ready(&st) { sp.decode(&st); }
                if let Some(r) = sp.get_result(&st) {
                    if !r.keyword.is_empty() { n += 1; sp.reset(&st); }
                }
            }
        }
        println!("  第 {round} 遍(10ms 块,不重建 stream)→ 命中 {n} 次  {}", if n > 0 { "✓" } else { "✗" });
    }
}
