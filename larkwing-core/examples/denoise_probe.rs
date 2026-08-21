//! 开发探针:GTCRN 神经去噪对克隆参考音的效果 —— 「克隆录入质量」方案的可行性验证件。
//! §7.5「推理统一 sherpa-onnx」:offline_speech_denoiser 走同一原生依赖、零新组件;
//! 神经去噪推理期零旋钮(与 afftdn 这类要手调 nr/nf 的谱减法相对),模型 = 数据。
//!
//! 用法: cargo run -p larkwing-core --example denoise_probe -- <gtcrn.onnx> <in.wav> <out.wav>
//! 输入吃 16k 单声道 wav(克隆参考音原生格式),输出 16-bit PCM wav。

use sherpa_onnx::{
    OfflineSpeechDenoiser, OfflineSpeechDenoiserConfig, OfflineSpeechDenoiserGtcrnModelConfig,
    OfflineSpeechDenoiserModelConfig,
};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [_, model, input, output] = &args[..] else {
        eprintln!("用法: denoise_probe <gtcrn.onnx> <in.wav> <out.wav>");
        std::process::exit(2);
    };
    let wave = sherpa_onnx::Wave::read(input).expect("读输入 wav 失败");
    let cfg = OfflineSpeechDenoiserConfig {
        model: OfflineSpeechDenoiserModelConfig {
            gtcrn: OfflineSpeechDenoiserGtcrnModelConfig { model: Some(model.clone()) },
            num_threads: 2,
            ..Default::default()
        },
    };
    let denoiser = OfflineSpeechDenoiser::create(&cfg).expect("GTCRN 加载失败");
    let t0 = std::time::Instant::now();
    let out = denoiser.run(wave.samples(), wave.sample_rate());
    assert!(!out.samples.is_empty(), "去噪返回了空音频");
    println!(
        "去噪完成: {} 样本@{}Hz → {} 样本@{}Hz,耗时 {}ms",
        wave.samples().len(),
        wave.sample_rate(),
        out.samples.len(),
        out.sample_rate,
        t0.elapsed().as_millis()
    );
    write_wav_16(output, &out.samples, out.sample_rate as u32).expect("写输出 wav 失败");
}

/// f32 PCM → 16-bit 单声道 WAV(与 voice/tts.rs::pcm_f32_to_wav 同构;那边 pub(super)
/// 不外露,探针例子自带一份免动库代码)。
fn write_wav_16(path: &str, samples: &[f32], rate: u32) -> std::io::Result<()> {
    let data_len = samples.len() * 2;
    let mut buf = Vec::with_capacity(44 + data_len);
    buf.extend_from_slice(b"RIFF");
    buf.extend_from_slice(&((36 + data_len) as u32).to_le_bytes());
    buf.extend_from_slice(b"WAVE");
    buf.extend_from_slice(b"fmt ");
    buf.extend_from_slice(&16u32.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes());
    buf.extend_from_slice(&rate.to_le_bytes());
    buf.extend_from_slice(&(rate * 2).to_le_bytes());
    buf.extend_from_slice(&2u16.to_le_bytes());
    buf.extend_from_slice(&16u16.to_le_bytes());
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&(data_len as u32).to_le_bytes());
    for &s in samples {
        buf.extend_from_slice(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
    }
    std::fs::write(path, buf)
}
