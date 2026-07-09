//! eval harness 入口(真模型,env 门控)—— 把「改提示词/工具描述好不好」跑成通过率矩阵。
//!
//! 用法(至少给一个 key;给多个 = 出跨模型矩阵,顺带就是路由选型表):
//!   DEEPSEEK_API_KEY=sk-… cargo run -p larkwing-core --example eval
//!   DEEPSEEK_API_KEY=… GEMINI_API_KEY=… EVAL_RUNS=5 cargo run -p larkwing-core --example eval
//!   EVAL_VERBOSE=1 …  # 失败的 run 打印现场(工具轨迹 / 新记忆 / 提炼数 / 末条回复 / 没过的断言)
//!   EVAL_THINKING=… # 思考档覆盖 off/light/medium/heavy(=1 等非档位值 → heavy);不设 = 默认 medium(回合 + 提炼)
//!   EVAL_FILTER=judge …  # 按场景 id 子串过滤(逗号分隔、任一命中);调 rubric / 排障单独重跑,不必整套烧钱
//!
//! 判官逻辑本身的自测(免 key):cargo test -p larkwing-core eval

use larkwing_core::eval::{render_matrix, run_suite, scenarios, RunOpts};
use larkwing_core::llm::registry::ProviderSpec;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut specs = Vec::new();
    if let Ok(k) = std::env::var("DEEPSEEK_API_KEY") {
        specs.push(ProviderSpec::deepseek(k));
    }
    if let Ok(k) = std::env::var("GEMINI_API_KEY") {
        specs.push(ProviderSpec::gemini(k));
    }
    if let Ok(k) = std::env::var("OPENAI_API_KEY") {
        specs.push(ProviderSpec::openai(k));
    }
    if let Ok(k) = std::env::var("ANTHROPIC_API_KEY") {
        specs.push(ProviderSpec::anthropic(k));
    }
    if let Ok(base) = std::env::var("OLLAMA_BASE_URL") {
        specs.push(ProviderSpec::ollama(base));
    }

    if specs.is_empty() {
        eprintln!("没有可用 provider。设一个 key 再跑,例如:");
        eprintln!("  DEEPSEEK_API_KEY=sk-… cargo run -p larkwing-core --example eval");
        eprintln!("(可同时设 GEMINI_API_KEY / OPENAI_API_KEY / ANTHROPIC_API_KEY 出跨模型矩阵)");
        return Ok(());
    }

    let runs: Option<u32> = std::env::var("EVAL_RUNS").ok().and_then(|s| s.parse().ok());
    let mut scenarios = scenarios::suite();
    // EVAL_FILTER=judge / EVAL_FILTER=care,voice:按场景 id 子串过滤(逗号分隔、任一命中)。
    // 调 rubric / 单场景排障时只重跑命中的几个,不必整套重烧钱。
    if let Ok(f) = std::env::var("EVAL_FILTER") {
        let pats: Vec<String> =
            f.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        if !pats.is_empty() {
            scenarios.retain(|s| pats.iter().any(|p| s.id.contains(p.as_str())));
            eprintln!("场景过滤 EVAL_FILTER={f} → 命中 {} 个", scenarios.len());
            if scenarios.is_empty() {
                eprintln!("没有场景命中过滤条件,退出。");
                return Ok(());
            }
        }
    }
    eprintln!(
        "跑 {} 个场景 × {} 个 provider(每场景 {} 次)…\n",
        scenarios.len(),
        specs.len(),
        runs.map(|r| r.to_string()).unwrap_or_else(|| "默认".into()),
    );
    let thinking_label = match std::env::var("EVAL_THINKING").ok().filter(|s| !s.is_empty()) {
        Some(v) if ["light", "medium", "heavy"].contains(&v.as_str()) => v,
        Some(_) => "heavy".into(),
        None => "默认 medium(回合 + 提炼;2026-06-19 起提炼也默认开思考)".into(),
    };
    eprintln!("思考档:{thinking_label}");
    // LLM-judge(§16.3):判官 = specs 里档位最高的那个,run_suite 里固定一个(列间可比)。
    if let Some(j) = specs
        .iter()
        .max_by_key(|s| larkwing_core::llm::catalog::tier_of(&s.model))
    {
        eprintln!("LLM-judge 判官:{}({})—— judge-* 场景由它评自由文本质量", j.id, j.model);
    }
    if std::env::var("EVAL_VERBOSE").is_err() {
        eprintln!("(想看失败 run 的现场:加 EVAL_VERBOSE=1)");
    }
    eprintln!();

    let reports = run_suite(&scenarios, specs, RunOpts { runs_override: runs }).await;
    println!("{}", render_matrix(&reports));
    Ok(())
}
