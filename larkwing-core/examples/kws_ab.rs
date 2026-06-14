//! A/B 归因:量化 robot(fp32 / score 2.5 / thr 0.2)对 Larkwing(int8 / 1.5 / 0.45)
//! 三轴各自的召回权重。借 robot 已下好的真实 KWS 模型 + 官方 test_wavs(7 条真人音)。
//!   cargo run -p larkwing-core --example kws_ab -- <model_dir>
//! 两组实验:
//!   1) 归因矩阵:基线 → 单轴翻 → 三轴全开,看命中数与逐 wav 翻转
//!   2) 衰减扫描:把音频整体压低(模拟远场/小声),看两端配置谁在弱信号下扛得住

use std::path::Path;

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

/// 喂整段(100ms 块 + 尾部 0.5s 静音 flush),命中即记;gain 整体缩放模拟远场/小声。
fn hit(sp: &sherpa_onnx::KeywordSpotter, samples: &[f32], rate: i32, gain: f32) -> bool {
    let st = sp.create_stream();
    let chunk = (rate as usize / 10).max(1);
    let scaled: Vec<f32> = samples.iter().map(|s| (s * gain).clamp(-1.0, 1.0)).collect();
    let tail = vec![0f32; rate as usize / 2];
    let mut got = false;
    for data in [scaled.as_slice(), tail.as_slice()] {
        for c in data.chunks(chunk) {
            st.accept_waveform(rate, c);
            while sp.is_ready(&st) {
                sp.decode(&st);
            }
            if let Some(r) = sp.get_result(&st) {
                if !r.keyword.is_empty() {
                    got = true;
                    sp.reset(&st);
                }
            }
        }
    }
    got
}

fn main() {
    let dir = std::env::args().nth(1).expect("用法: kws_ab <model_dir>");
    let tk = std::fs::read_to_string(Path::new(&dir).join("test_wavs/test_keywords.txt"))
        .expect("读 test_keywords.txt");
    let kw_buf = tk.trim_end().to_string(); // 无尾换行 = Larkwing 生产方式

    // 一次性读入所有真人 wav
    let wavs: Vec<(usize, Vec<f32>, i32)> = (0..10)
        .filter_map(|i| {
            let pth = Path::new(&dir).join(format!("test_wavs/{i}.wav"));
            sherpa_onnx::Wave::read(&pth.to_string_lossy())
                .map(|w| (i, w.samples().to_vec(), w.sample_rate()))
        })
        .collect();
    let total = wavs.len();

    // ---- 实验 1:归因矩阵(int8?, score, thr) ----
    // 基线 A → 单轴翻(B 阈值 / C score / D fp32)→ 三轴全开 E = robot 等效
    let cells: [(&str, bool, f32, f32); 5] = [
        ("A 基线   int8 / s1.5 / t0.45  ← Larkwing 默认", true, 1.5, 0.45),
        ("B 仅阈值 int8 / s1.5 / t0.20  ← 滑块拉满极限", true, 1.5, 0.20),
        ("C 仅 score int8 / s2.5 / t0.45 ← robot score", true, 2.5, 0.45),
        ("D 仅 fp32 fp32 / s1.5 / t0.45 ← 全精度模型", false, 1.5, 0.45),
        ("E 全开   fp32 / s2.5 / t0.20  ← robot 等效", false, 2.5, 0.20),
    ];
    println!("\n================ 实验 1:归因矩阵(clean test_wavs,gain=1.0) ================");
    println!("(逐 wav: ✓ 命中 / · 漏)  总计 {total} 条真人音\n");
    for (label, int8, score, thr) in cells {
        let sp = make_spotter(&dir, &kw_buf, thr, score, int8);
        let mut n = 0;
        let marks: String = wavs
            .iter()
            .map(|(_, s, r)| {
                if hit(&sp, s, *r, 1.0) {
                    n += 1;
                    '✓'
                } else {
                    '·'
                }
            })
            .collect();
        println!("  {label}   →  {n}/{total}   [{marks}]");
    }

    // ---- 实验 2:衰减扫描(两端配置,看弱信号鲁棒性) ----
    println!("\n================ 实验 2:衰减扫描(模拟远场/小声) ================");
    println!("gain: 1.0=原始  0.5=-6dB  0.25=-12dB  0.125=-18dB\n");
    let anchors: [(&str, bool, f32, f32); 2] = [
        ("Larkwing 默认 int8/s1.5/t0.45", true, 1.5, 0.45),
        ("robot 等效    fp32/s2.5/t0.20", false, 2.5, 0.20),
    ];
    print!("  {:<34}", "gain →");
    for g in [1.0f32, 0.5, 0.25, 0.125] {
        print!("{g:>8}");
    }
    println!();
    for (label, int8, score, thr) in anchors {
        let sp = make_spotter(&dir, &kw_buf, thr, score, int8);
        print!("  {label:<34}");
        for g in [1.0f32, 0.5, 0.25, 0.125] {
            let n = wavs.iter().filter(|(_, s, r)| hit(&sp, s, *r, g)).count();
            print!("{:>8}", format!("{n}/{total}"));
        }
        println!();
    }
    println!();
}
