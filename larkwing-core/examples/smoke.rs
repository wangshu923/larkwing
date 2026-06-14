//! 冒烟:直连 DeepSeek 看流式吐字与缓存命中。
//! 用法:DEEPSEEK_API_KEY=sk-... cargo run -p larkwing-core --example smoke

use std::io::Write;

use larkwing_core::llm::{
    openai_compat::OpenAiCompatProvider, ChatEvent, ChatMessage, ChatRequest, LlmConfig,
    LlmProvider,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let key = std::env::var("DEEPSEEK_API_KEY")
        .expect("请先设置 DEEPSEEK_API_KEY 环境变量");
    let provider = OpenAiCompatProvider::new(LlmConfig::deepseek(key));
    let req = ChatRequest {
        system: "你是旺财,一只暖萌的电子小狗。中文回复,简短热情。".into(),
        messages: vec![ChatMessage::user("用一句话跟我打个招呼!")],
        ..Default::default()
    };
    let mut rx = provider.chat_stream(req).await?;
    while let Some(ev) = rx.recv().await {
        match ev {
            ChatEvent::Delta(t) => {
                print!("{t}");
                std::io::stdout().flush().ok();
            }
            ChatEvent::Thinking(t) => {
                eprint!("[思考:{t}]");
            }
            ChatEvent::Done { usage: u, stop_reason, .. } => println!(
                "\n--- 完成 input={} output={} cache_hit={} stop={}",
                u.input_tokens,
                u.output_tokens,
                u.cache_hit_tokens,
                stop_reason.as_deref().unwrap_or("unknown")
            ),
            ChatEvent::Failed(e) => println!("\n--- 失败: {e}"),
        }
    }
    Ok(())
}
