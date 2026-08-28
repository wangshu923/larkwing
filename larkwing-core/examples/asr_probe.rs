//! 开发探针:验证选定 ASR 档在钉定的 sherpa-onnx 绑定上真能加载、真能转写中文 ——
//! 加新档 / 换下载源前的可行性验证件(2026-08-28 识别模型四档扩容随批)。
//! 配置形状与 voice/asr.rs 各构造分支一致(那边 mod 私有,探针照 denoise_probe 先例
//! 直连 sherpa_onnx 自建配置);要验的风险在「钉定绑定认不认这个模型」,不在参数搬运。
//!
//! 用法: cargo run -p larkwing-core --example asr_probe -- <kind> <model_dir> <wav>
//! kind = voice.asr.model 合法值(sense-voice / funasr-nano / paraformer / firered-ctc);
//! wav 吃 16k 单声道(sherpa release 包自带的 test_wavs 即是)。

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [_, kind, dir, wav] = &args[..] else {
        eprintln!("用法: asr_probe <sense-voice|funasr-nano|paraformer|firered-ctc> <model_dir> <wav>");
        std::process::exit(2);
    };
    let dir = std::path::Path::new(dir);
    let mut cfg = sherpa_onnx::OfflineRecognizerConfig::default();
    cfg.model_config.tokens = Some(dir.join("tokens.txt").to_string_lossy().into_owned());
    let model = dir.join("model.int8.onnx").to_string_lossy().into_owned();
    match kind.as_str() {
        // Fun-ASR-Nano = SenseVoice 兼容导出(model_type=sense_voice_ctc),同一构造
        "sense-voice" | "funasr-nano" => {
            cfg.model_config.sense_voice.model = Some(model);
            cfg.model_config.sense_voice.language = Some("zh".into());
            cfg.model_config.sense_voice.use_itn = true;
        }
        "paraformer" => {
            cfg.model_config.paraformer.model = Some(model);
            cfg.model_config.num_threads = 4;
        }
        "firered-ctc" => {
            cfg.model_config.fire_red_asr_ctc.model = Some(model);
            cfg.model_config.num_threads = 4;
        }
        other => {
            eprintln!("未知档名: {other}");
            std::process::exit(2);
        }
    }
    let t0 = std::time::Instant::now();
    let rec = sherpa_onnx::OfflineRecognizer::create(&cfg).expect("ASR 模型加载失败");
    let load_ms = t0.elapsed().as_millis();
    let wave = sherpa_onnx::Wave::read(wav).expect("读 wav 失败");
    assert_eq!(wave.sample_rate(), 16_000, "探针只吃 16k wav(test_wavs 即是)");
    let t1 = std::time::Instant::now();
    let stream = rec.create_stream();
    stream.accept_waveform(16_000, wave.samples());
    rec.decode(&stream);
    let text = stream.get_result().map(|r| r.text.trim().to_string()).unwrap_or_default();
    println!(
        "加载 {load_ms}ms · 转写 {}ms · 音频 {}ms\n→ {text}",
        t1.elapsed().as_millis(),
        wave.samples().len() as u64 * 1000 / 16_000
    );
    assert!(!text.is_empty(), "转写结果为空 = 模型没真跑起来");
}
