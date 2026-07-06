//! 把 wav 喂给 app 同款 ASR(SenseVoice),打印识别文本 —— AEC spike 录音的**语义级**验证:
//! 电平数字说明"消掉多少",这里回答"消完之后 ASR/确认层还认不认得出人在说什么"。
//! 任意采样率的 16-bit mono wav 皆可(内置 LinearResampler 到 16k;峰值归一同 voice 管线)。
//!   cargo run -p larkwing-core --example asr_wav -- <sense_voice_model_dir> <wav> [wav…]

use std::path::Path;

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args.next().expect("用法: asr_wav <model_dir> <wav> [wav…]");
    let wavs: Vec<String> = args.collect();
    assert!(!wavs.is_empty(), "缺 <wav>");

    // 与 voice/asr.rs::sense_voice 同款配置(zh + itn)
    let mut cfg = sherpa_onnx::OfflineRecognizerConfig::default();
    let p = |n: &str| Some(Path::new(&dir).join(n).to_string_lossy().into_owned());
    cfg.model_config.sense_voice.model = p("model.int8.onnx");
    cfg.model_config.sense_voice.language = Some("zh".into());
    cfg.model_config.sense_voice.use_itn = true;
    cfg.model_config.tokens = p("tokens.txt");
    let rec = sherpa_onnx::OfflineRecognizer::create(&cfg).expect("ASR 模型加载失败");

    for wav in &wavs {
        let w = sherpa_onnx::Wave::read(wav).unwrap_or_else(|| panic!("读不了 wav: {wav}"));
        let mut s: Vec<f32> = w.samples().to_vec();
        let rate = w.sample_rate();
        if rate != 16_000 {
            let mut rs = sherpa_onnx::LinearResampler::create(rate, 16_000)
                .expect("重采样器创建失败");
            s = rs.resample(&s, true);
        }
        // 峰值归一(voice 管线同款 AGC 简化版:归到 -3dBFS,近静音原样)
        let peak = s.iter().fold(0f32, |m, &x| m.max(x.abs()));
        if peak > 1e-3 {
            let g = (0.7079 / peak).min(10.0); // -3dBFS,封顶 20dB
            for x in &mut s {
                *x *= g;
            }
        }
        let st = rec.create_stream();
        st.accept_waveform(16_000, &s);
        rec.decode(&st);
        let text = st.get_result().map(|r| r.text.trim().to_string()).unwrap_or_default();
        println!("\n=== {wav} ===\n{}", if text.is_empty() { "(没识别出话)".into() } else { text });
    }
}
