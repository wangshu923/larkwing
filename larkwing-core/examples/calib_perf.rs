//! 临时诊断:复现「正在算」卡顿 —— 标定扫描的两种建模方式计时。
//! A) 现状:每个阈值重建一个 KeywordSpotter(9 次 create,9 次加载 onnx)。
//! B) 修法:只建一次 spotter,每阈值用 create_stream_with_keywords + 内联 `#threshold`。
//! 同时验证内联 `#threshold` 不会 panic(格式可用)。
//! cargo run -p larkwing-core --example calib_perf -- <kws_model_dir>

use std::collections::HashSet;
use std::path::Path;
use std::time::Instant;

use pinyin::ToPinyin;

fn load_vocab(p: &Path) -> HashSet<String> {
    std::fs::read_to_string(p)
        .expect("读 tokens.txt")
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .map(str::to_string)
        .collect()
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

fn encode(word: &str, vocab: &HashSet<String>) -> String {
    let mut tokens = Vec::new();
    for ch in word.chars() {
        let py = ch.to_pinyin().expect("中文").with_tone();
        tokens.extend(split_syllable(py, vocab).expect("音节切不进词表"));
    }
    tokens.join(" ")
}

fn make_spotter(dir: &str, buf: &str, thr: f32) -> sherpa_onnx::KeywordSpotter {
    let mut k = sherpa_onnx::KeywordSpotterConfig::default();
    let p = |n: &str| Some(Path::new(dir).join(n).to_string_lossy().into_owned());
    k.model_config.transducer.encoder = p("encoder-epoch-12-avg-2-chunk-16-left-64.int8.onnx");
    k.model_config.transducer.decoder = p("decoder-epoch-12-avg-2-chunk-16-left-64.int8.onnx");
    k.model_config.transducer.joiner = p("joiner-epoch-12-avg-2-chunk-16-left-64.int8.onnx");
    k.model_config.tokens = p("tokens.txt");
    k.keywords_threshold = thr;
    k.keywords_score = 1.5;
    k.keywords_buf = Some(buf.to_string());
    sherpa_onnx::KeywordSpotter::create(&k).expect("KWS create 失败")
}

fn detect(sp: &sherpa_onnx::KeywordSpotter, st: &sherpa_onnx::OnlineStream, samples: &[f32]) -> bool {
    let chunk = 1600usize;
    let tail = vec![0f32; 8000];
    let mut hit = false;
    for data in [samples, tail.as_slice()] {
        for c in data.chunks(chunk) {
            st.accept_waveform(16000, c);
            while sp.is_ready(st) {
                sp.decode(st);
            }
            if let Some(r) = sp.get_result(st) {
                if !r.keyword.is_empty() {
                    hit = true;
                    sp.reset(st);
                }
            }
        }
    }
    hit
}

fn main() {
    let dir = std::env::args().nth(1).expect("用法: calib_perf <kws_model_dir>");
    let vocab = load_vocab(&Path::new(&dir).join("tokens.txt"));
    let tokens = encode("旺财", &vocab);
    println!("旺财 编码 = {tokens:?}");

    let grid = [0.10f32, 0.15, 0.20, 0.25, 0.30, 0.35, 0.40, 0.45, 0.50];
    // 6 段"样本":5 条 ~0.8s 正样本占位 + 1 条 4s 底噪(用低幅噪声,只为计时 create+detect 循环)
    let mk = |secs: f32, amp: f32| -> Vec<f32> {
        (0..(secs * 16000.0) as usize).map(|i| amp * ((i % 91) as f32 / 91.0 - 0.5)).collect()
    };
    let samples: Vec<Vec<f32>> =
        (0..5).map(|_| mk(0.8, 0.05)).chain(std::iter::once(mk(4.0, 0.02))).collect();

    // ---- A) 现状:每阈值重建 spotter ----
    let buf_a = format!("{tokens} @v0");
    let t0 = Instant::now();
    let mut create_total = std::time::Duration::ZERO;
    for &thr in &grid {
        let tc = Instant::now();
        let sp = make_spotter(&dir, &buf_a, thr);
        create_total += tc.elapsed();
        for s in &samples {
            let st = sp.create_stream();
            detect(&sp, &st, s);
        }
    }
    let a_total = t0.elapsed();
    println!(
        "\n[A 现状] 9 次重建 spotter:总 {:.2}s(其中 create 累计 {:.2}s,每次 ~{:.0}ms)",
        a_total.as_secs_f32(),
        create_total.as_secs_f32(),
        create_total.as_secs_f32() * 1000.0 / 9.0
    );

    // ---- B) 修法:只建一次,create_stream_with_keywords + 内联 #threshold ----
    let t1 = Instant::now();
    let tc = Instant::now();
    let sp = make_spotter(&dir, &format!("{tokens} :1.5 #0.1 @v0"), 0.05);
    let one_create = tc.elapsed();
    for &thr in &grid {
        // k2-fsa 格式:tok… :boost #threshold @词(:/# 后紧跟浮点)
        let kw = format!("{tokens} :1.5 #{thr} @v0");
        for s in &samples {
            let st = sp.create_stream_with_keywords(&kw);
            detect(&sp, &st, s);
        }
    }
    let b_total = t1.elapsed();
    println!(
        "[B 修法] 建 1 次(create {:.0}ms)+ 9×流内联阈值:总 {:.2}s(不 panic = 内联 #threshold 格式可用)",
        one_create.as_secs_f32() * 1000.0,
        b_total.as_secs_f32()
    );

    println!(
        "\n>>> 提速 {:.1}×(create 从 9 次降到 1 次)。",
        a_total.as_secs_f32() / b_total.as_secs_f32().max(0.001)
    );
}
