// 诊断用一次性探针:绕开 app,直接按 ZipVoiceTts::load 同款配置调 sherpa,
// 把 C++ 层的真实报错暴露到 stderr(OfflineTts::create 只回 None、不给缘由)。
// 用法: cargo run -p larkwing-core --example zipvoice_probe -- <model_dir>
fn main() {
    let dir = std::path::PathBuf::from(
        std::env::args().nth(1).expect("用法: zipvoice_probe <model_dir>"),
    );
    let p = |n: &str| Some(dir.join(n).to_string_lossy().into_owned());
    let mut cfg = sherpa_onnx::OfflineTtsConfig::default();
    cfg.model.zipvoice.tokens = p("tokens.txt");
    cfg.model.zipvoice.encoder = p("encoder.int8.onnx");
    cfg.model.zipvoice.decoder = p("decoder.int8.onnx");
    cfg.model.zipvoice.vocoder = p("vocos_24khz.onnx");
    cfg.model.zipvoice.data_dir = p("espeak-ng-data");
    cfg.model.zipvoice.lexicon = p("lexicon.txt"); // 原始 lexicon(绕开 merge,缩小变量)
    cfg.model.zipvoice.feat_scale = 0.1;
    cfg.model.zipvoice.t_shift = 0.5;
    cfg.model.zipvoice.target_rms = 0.1;
    cfg.model.zipvoice.guidance_scale = 1.0;
    cfg.model.num_threads = 2;
    cfg.model.debug = true; // 让 sherpa 打详细加载日志
    eprintln!("== probing {} ==", dir.display());
    match sherpa_onnx::OfflineTts::create(&cfg) {
        Some(_) => eprintln!("== OK: OfflineTts created =="),
        None => eprintln!("== FAIL: create returned None(真实原因看上方 sherpa stderr)=="),
    }
}
