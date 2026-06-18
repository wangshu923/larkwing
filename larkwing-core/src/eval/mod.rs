//! eval harness:把「改提示词 / 工具描述 / few-shot 好不好」从「手动真机试」变成
//! **可重复测的回归 + 跨模型选型矩阵**。
//!
//! 立场(贴 AGENT.md):
//! - **真模型、env 门控**:只 `examples/eval.rs` 触发(要 key、花钱、非确定),**绝不进默认
//!   `cargo test` / CI**。判官逻辑本身用 FakeLlm 在 `cargo test` 里自测(免 key,见本文件 tests)。
//! - **场景 = Rust**(组合子 + 闭包逃生口,不是 DSL —— 见 `grader`)。
//! - **engine 零改**:只用公开 API(`send_message` / `conversation_trace` /
//!   `consolidate_conversation` + store 读写)。
//! - **跑 N 次数通过率**(pass^k 风格:一次全过才算这次 pass);每次全新临时库 → run 间零串扰。
//! - **模型矩阵**:同一套场景 loop over `ProviderSpec` —— 顺带就是「三档路由 / cheap-model」选型表。

pub mod grader;
pub mod scenarios;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::engine::{AssistantPayload, Engine, ToolRowPayload, TraceStep, TurnEvent};
use crate::llm::registry::ProviderSpec;
use crate::llm::LlmProvider;
use crate::scenes::Scenes;
use crate::store::Store;

pub use grader::{
    briefing_written, custom, distilled_at_least, distilled_contains, distilled_empty,
    memory_written, no_memory_contains, no_tool_calls, tool_called, tool_not_called, tool_status,
    Check, Observed, Outcome,
};

/// 驱动方式:一串用户消息(各排到回合收尾)/ 预置一段历史后调 consolidate。
enum Drive {
    Turn(Vec<String>),
    Consolidate(Vec<(String, String)>),
}

type SeedFn = Box<dyn Fn(&Store, i64) + Send + Sync>;

/// 一个评估场景:元信息 + 驱动 + 断言。可被多次运行(N 次 × 多 provider)。
pub struct Scenario {
    pub id: String,
    pub note: String,
    pub runs: u32,
    drive: Drive,
    seed: Option<SeedFn>,
    checks: Vec<Check>,
}

impl Scenario {
    /// turn 场景:驱动一串用户消息(`.say(..)` 追加)。
    pub fn turn(id: &str) -> Scenario {
        Scenario {
            id: id.into(),
            note: String::new(),
            runs: 5,
            drive: Drive::Turn(Vec::new()),
            seed: None,
            checks: Vec::new(),
        }
    }

    /// consolidate 场景:预置一段历史(`.line(role, content)`)后调 `consolidate_conversation`。
    pub fn consolidate(id: &str) -> Scenario {
        Scenario {
            id: id.into(),
            note: String::new(),
            runs: 5,
            drive: Drive::Consolidate(Vec::new()),
            seed: None,
            checks: Vec::new(),
        }
    }

    pub fn note(mut self, s: &str) -> Self {
        self.note = s.into();
        self
    }

    pub fn runs(mut self, n: u32) -> Self {
        self.runs = n;
        self
    }

    /// 追加一条用户消息(turn 场景)。
    pub fn say(mut self, text: &str) -> Self {
        if let Drive::Turn(v) = &mut self.drive {
            v.push(text.into());
        }
        self
    }

    /// 追加一条预置历史(consolidate 场景;role = user/assistant)。
    pub fn line(mut self, role: &str, content: &str) -> Self {
        if let Drive::Consolidate(v) = &mut self.drive {
            v.push((role.into(), content.into()));
        }
        self
    }

    /// 预置记忆 / 需知(在驱动之前跑;这些不计入「本次新写入」)。
    pub fn seed(mut self, f: impl Fn(&Store, i64) + Send + Sync + 'static) -> Self {
        self.seed = Some(Box::new(f));
        self
    }

    pub fn check(mut self, c: Check) -> Self {
        self.checks.push(c);
        self
    }
}

/// 运行参数。
#[derive(Default)]
pub struct RunOpts {
    /// 覆盖每个场景自带的 runs(命令行 EVAL_RUNS);None = 用场景默认。
    pub runs_override: Option<u32>,
}

/// Token / 成本账(取自引擎自己的 usage 记账,`store.usage`)。
/// ⚠️ 只含**对话轮**(send_message 的 turn loop 记的);`consolidate` 走 `provider.chat`
/// 不经 turn loop、引擎不记账 → 提炼场景的 token **不计入**(engine 零改的代价,如实标注)。
#[derive(Default, Clone)]
pub struct TokenTally {
    pub input: i64,
    pub output: i64,
    /// 命中前缀缓存的输入 token(⊆ input;DeepSeek 自动缓存,计费约 1/10)。
    pub cache_hit: i64,
    pub cost_usd: f64,
    /// 无牌价(目录里价格存疑)的轮次数;>0 时 cost_usd 不是全貌(§观测:价格存疑只报 token)。
    pub unpriced_rounds: i64,
}

impl TokenTally {
    fn add_totals(&mut self, t: &crate::store::UsageTotals) {
        self.input += t.input_tokens;
        self.output += t.output_tokens;
        self.cache_hit += t.cache_hit_tokens;
        self.cost_usd += t.cost_usd;
        self.unpriced_rounds += t.unpriced_rounds;
    }
    fn merge(&mut self, o: &TokenTally) {
        self.input += o.input;
        self.output += o.output;
        self.cache_hit += o.cache_hit;
        self.cost_usd += o.cost_usd;
        self.unpriced_rounds += o.unpriced_rounds;
    }
}

/// 单场景结果。
pub struct ScenarioResult {
    pub id: String,
    pub note: String,
    pub passed: u32,
    pub total: u32,
    /// 失败断言 → 出现次数(诊断:哪条规则在掉链子)。
    pub failed_checks: Vec<(String, u32)>,
    /// 非正常收尾(报错 / 取消)次数。
    pub bad_outcomes: u32,
    /// 本场景 N 次运行的 token / 成本合计(见 `TokenTally` 注意事项)。
    pub tokens: TokenTally,
}

impl ScenarioResult {
    pub fn rate(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.passed as f64 / self.total as f64
        }
    }
}

/// 一个 provider 的整套结果。
pub struct SuiteReport {
    pub provider_id: String,
    pub provider_model: String,
    pub results: Vec<ScenarioResult>,
}

fn temp_db(tag: &str, run: u32) -> std::path::PathBuf {
    let p =
        std::env::temp_dir().join(format!("lw-eval-{}-{}-{}.db", std::process::id(), tag, run));
    let _ = std::fs::remove_file(&p);
    let _ = std::fs::remove_file(p.with_extension("db-wal"));
    let _ = std::fs::remove_file(p.with_extension("db-shm"));
    p
}

/// 从落库消息直接重建工具轨迹 —— **不走 `engine.conversation_trace`**:后者只在该回合有
/// 可见回复(非空 content)时才结算轨迹、且 loop 末不补 flush,于是「调了工具但收尾没出可见
/// 文字」的回合会被静默丢掉(实测 DeepSeek 会这样)。eval 要的是「工具到底跑没跑」,所以直接
/// 读 assistant 行的 tool_calls + tool 行的 status(eval 在 crate 内,pub(crate) payload 可见)。
fn collect_trace(store: &Store, conv_id: i64) -> Vec<TraceStep> {
    let msgs = store.chat.recent_messages(conv_id, 300).unwrap_or_default();
    let mut steps: Vec<TraceStep> = Vec::new();
    let mut idx: HashMap<String, usize> = HashMap::new();
    for m in &msgs {
        match m.role.as_str() {
            "assistant" => {
                if let Some(p) =
                    m.payload.as_deref().and_then(|s| serde_json::from_str::<AssistantPayload>(s).ok())
                {
                    for c in &p.tool_calls {
                        idx.insert(c.id.clone(), steps.len());
                        steps.push(TraceStep {
                            name: c.name.clone(),
                            ui_key: String::new(),
                            args: c.args.to_string(),
                            result: String::new(),
                            status: String::new(),
                        });
                    }
                }
            }
            "tool" => {
                if let Some(tp) =
                    m.payload.as_deref().and_then(|s| serde_json::from_str::<ToolRowPayload>(s).ok())
                {
                    if let Some(step) = idx.get(&tp.call_id).and_then(|&i| steps.get_mut(i)) {
                        step.result = m.content.clone();
                        step.status = tp.status;
                    }
                }
            }
            _ => {}
        }
    }
    steps
}

/// 截断到 n 个字符(verbose 诊断打印用,别刷屏)。
fn trunc(s: &str, n: usize) -> String {
    let t: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        format!("{t}…")
    } else {
        t
    }
}

/// 驱动一条用户消息到回合收尾,返回结局(把 Delta/ToolUse/Usage 都喝掉,只认终态)。
async fn drive_turn(engine: &Engine, conv_id: i64, text: &str) -> Outcome {
    let mut rx = match engine.send_message(conv_id, text.to_string(), None, Vec::new()).await {
        Ok(rx) => rx,
        Err(e) => return Outcome::Error(format!("{:?}: {}", e.kind, e.message)),
    };
    let mut outcome = Outcome::Cancelled;
    while let Some(ev) = rx.recv().await {
        match ev {
            TurnEvent::Done { .. } => outcome = Outcome::Done,
            TurnEvent::Failed { kind, message } => {
                outcome = Outcome::Failed(format!("{kind:?}: {message}"))
            }
            TurnEvent::Cancelled => outcome = Outcome::Cancelled,
            _ => {}
        }
    }
    outcome
}

/// 跑一个场景 `runs` 次(每次全新临时库 → run 间零串扰),返回通过率结果。
/// `make` 每次造一个**新** provider:真用 `|| spec.build()`;自测 `|| Arc::new(FakeLlm::scripted(..))`。
/// 一次 run 通过 = 正常收尾(Done)且**所有**断言都过(pass^k 风格)。
pub async fn run_scenario<F>(make: F, sc: &Scenario, runs: u32) -> ScenarioResult
where
    F: Fn() -> Arc<dyn LlmProvider>,
{
    let mut passed = 0u32;
    let mut bad_outcomes = 0u32;
    let mut failed: HashMap<String, u32> = HashMap::new();
    let mut tokens = TokenTally::default();
    let verbose = std::env::var("EVAL_VERBOSE").is_ok();

    for run in 0..runs {
        let store = match Store::open(&temp_db(&sc.id, run)) {
            Ok(s) => s,
            Err(_) => {
                bad_outcomes += 1;
                continue;
            }
        };
        let engine = Engine::new(store.clone(), Scenes::builtin());
        engine.set_provider(Some(make()));
        let user = store.users.ensure_default_user().expect("默认用户");
        let conv = store.chat.create_conversation(user.id, "companion").expect("建会话");
        // EVAL_THINKING=1|light|medium|heavy:写 llm.thinking 设置 → 回合(覆盖默认 medium)+ 提炼
        //（默认不开,这里设了才开)都按此档思考。不设 = 不动(回合默认 medium、提炼 off,= 真机默认)。
        // EVAL_THINKING 覆盖思考档(off/light/medium/heavy);不设 = 不写设置,引擎与 consolidate
        // 都按各自缺省走(均为 Medium,2026-06-19 起)。EVAL_THINKING=off 可显式 A/B 关思考基线。
        if let Ok(level) = std::env::var("EVAL_THINKING") {
            let level = match level.as_str() {
                "off" | "light" | "medium" | "heavy" => level,
                _ => "heavy".into(), // EVAL_THINKING=1 等非档位值 → 取最强档,给思考最大机会
            };
            let _ = store.settings.set(None, "llm.thinking", &level);
        }
        if let Some(seed) = &sc.seed {
            seed(&store, user.id);
        }

        // 驱动前快照:之后只看「本次新写入」的记忆 / 需知,与 seed 隔离。
        let pre_mem: HashSet<i64> =
            store.memory.list(user.id).unwrap_or_default().iter().map(|m| m.id).collect();
        let pre_brief: HashSet<i64> =
            store.briefings.list_for(user.id).unwrap_or_default().iter().map(|b| b.id).collect();

        let mut distilled = 0usize;
        let outcome = match &sc.drive {
            Drive::Turn(says) => {
                let mut last = Outcome::Done;
                for s in says {
                    last = drive_turn(&engine, conv.id, s).await;
                }
                last
            }
            Drive::Consolidate(transcript) => {
                for (role, content) in transcript {
                    let _ = store.chat.append_message(conv.id, role, content);
                }
                match engine.consolidate_conversation(conv.id).await {
                    Ok(n) => {
                        distilled = n;
                        Outcome::Done
                    }
                    Err(e) => Outcome::Error(format!("{:?}: {}", e.kind, e.message)),
                }
            }
        };

        let trace = collect_trace(&store, conv.id);
        let memories: Vec<_> = store
            .memory
            .list(user.id)
            .unwrap_or_default()
            .into_iter()
            .filter(|m| !pre_mem.contains(&m.id))
            .collect();
        let briefings: Vec<_> = store
            .briefings
            .list_for(user.id)
            .unwrap_or_default()
            .into_iter()
            .filter(|b| !pre_brief.contains(&b.id))
            .collect();

        // 引擎自己的记账(fresh DB → totals_since(0) 即本次运行合计)。
        if let Ok(u) = store.usage.totals_since(0) {
            tokens.add_totals(&u);
        }

        let observed =
            Observed { trace, memories, briefings, distilled, outcome: outcome.clone() };

        let mut all_ok = matches!(outcome, Outcome::Done);
        if !all_ok {
            bad_outcomes += 1;
        }
        let mut run_failed: Vec<&str> = Vec::new();
        for c in &sc.checks {
            if !c.eval(&observed) {
                all_ok = false;
                run_failed.push(&c.name);
                *failed.entry(c.name.clone()).or_insert(0) += 1;
            }
        }
        if all_ok {
            passed += 1;
        } else if verbose {
            // 失败 run 的现场:轨迹 / 新写入记忆 / 提炼数 / 末条回复 / 没过的断言(走 stderr,不污染矩阵)。
            eprintln!("\n[verbose] {} run#{run} ✗ outcome={:?}", sc.id, observed.outcome);
            if observed.trace.is_empty() {
                eprintln!("    工具:(无调用)");
            } else {
                for s in &observed.trace {
                    eprintln!("    工具:{}({}) -> {}", s.name, trunc(&s.args, 80), s.status);
                }
            }
            for m in &observed.memories {
                eprintln!("    新记忆[{}]:{}", m.kind, trunc(&m.content, 100));
            }
            eprintln!("    提炼:{} 条", observed.distilled);
            if let Some(reply) = store
                .chat
                .recent_messages(conv.id, 50)
                .ok()
                .and_then(|ms| ms.iter().rev().find(|m| m.role == "assistant").map(|m| m.content.clone()))
            {
                if !reply.is_empty() {
                    eprintln!("    末条回复:{}", trunc(&reply, 120));
                }
            }
            eprintln!("    未过:{}", run_failed.join(" / "));
        }
    }

    let mut failed_checks: Vec<(String, u32)> = failed.into_iter().collect();
    failed_checks.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ScenarioResult {
        id: sc.id.clone(),
        note: sc.note.clone(),
        passed,
        total: runs,
        failed_checks,
        bad_outcomes,
        tokens,
    }
}

/// 跑整套场景 × 每个 provider,产出可打印的矩阵报告。
pub async fn run_suite(
    scenarios: &[Scenario],
    specs: Vec<ProviderSpec>,
    opts: RunOpts,
) -> Vec<SuiteReport> {
    let mut reports = Vec::new();
    for spec in &specs {
        let mut results = Vec::new();
        for sc in scenarios {
            let runs = opts.runs_override.unwrap_or(sc.runs);
            results.push(run_scenario(|| spec.build(), sc, runs).await);
        }
        reports.push(SuiteReport {
            provider_id: spec.id.clone(),
            provider_model: spec.model.clone(),
            results,
        });
    }
    reports
}

fn glyph(rate: f64) -> &'static str {
    if rate >= 0.8 {
        "✓"
    } else if rate >= 0.5 {
        "~"
    } else {
        "✗"
    }
}

/// 渲染 `场景 × provider` 通过率矩阵 + 每 provider 失败明细(给终端看)。
pub fn render_matrix(reports: &[SuiteReport]) -> String {
    use std::fmt::Write;
    if reports.is_empty() {
        return "(没有可用 provider,无结果)".into();
    }
    let mut out = String::new();
    let _ = writeln!(out, "通过率矩阵(passed/runs;✓≥0.8  ~≥0.5  ✗<0.5)\n");

    let id_w = reports[0]
        .results
        .iter()
        .map(|r| r.id.len())
        .max()
        .unwrap_or(8)
        .max(8);

    let _ = write!(out, "{:<width$}", "场景", width = id_w + 2);
    for rep in reports {
        let _ = write!(out, "{:>16}", rep.provider_id);
    }
    out.push('\n');
    for (i, r0) in reports[0].results.iter().enumerate() {
        let _ = write!(out, "{:<width$}", r0.id, width = id_w + 2);
        for rep in reports {
            let r = &rep.results[i];
            let cell = format!("{} {}/{}", glyph(r.rate()), r.passed, r.total);
            let _ = write!(out, "{cell:>16}");
        }
        out.push('\n');
    }

    for rep in reports {
        let _ = writeln!(out, "\n── {} ({}) ──", rep.provider_id, rep.provider_model);
        let mut any = false;
        for r in &rep.results {
            if r.passed == r.total {
                continue;
            }
            any = true;
            let _ = writeln!(out, "  {} {}/{}  {} — {}", glyph(r.rate()), r.passed, r.total, r.id, r.note);
            for (name, cnt) in &r.failed_checks {
                let _ = writeln!(out, "      ✗ {name}({cnt}/{} 次)", r.total);
            }
            if r.bad_outcomes > 0 {
                let _ = writeln!(out, "      ⚠ 非正常收尾 {}/{} 次(报错/取消)", r.bad_outcomes, r.total);
            }
        }
        if !any {
            let _ = writeln!(out, "  全过 ✓");
        }

        // 用量合计(引擎记账;⚠️ consolidate 走 provider.chat 不经 turn loop → 不计入)。
        let mut t = TokenTally::default();
        for r in &rep.results {
            t.merge(&r.tokens);
        }
        let hit_pct = if t.input > 0 {
            100.0 * t.cache_hit as f64 / t.input as f64
        } else {
            0.0
        };
        let _ = writeln!(
            out,
            "  用量(已记录的对话轮;consolidate 自身调用未计入):\n      输入 {} tok(缓存命中 {}, {:.0}%)· 输出 {} tok",
            t.input, t.cache_hit, hit_pct, t.output
        );
        if t.cost_usd > 0.0 {
            let extra = if t.unpriced_rounds > 0 {
                format!(",另有 {} 轮无牌价", t.unpriced_rounds)
            } else {
                String::new()
            };
            let _ = writeln!(out, "      成本 ≈ ${:.4}{}", t.cost_usd, extra);
        } else {
            let _ = writeln!(out, "      成本:模型无牌价,仅报 token(共 {} 轮无牌价)", t.unpriced_rounds);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    //! 判官 + 引擎全链的自测:用 FakeLlm 编排确定性轨迹,验证「做对了就过、漏了就挂」。
    //! 这一层在 `cargo test` 里跑(免 key);真模型质量评估只在 `examples/eval.rs`。
    use super::*;
    use crate::llm::fake::{FakeLlm, FakeTurn};
    use crate::llm::ToolCall;

    fn call(name: &str, args: serde_json::Value) -> ToolCall {
        ToolCall { id: format!("call_{name}"), name: name.into(), args, is_incomplete: false }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn grader_passes_when_engine_does_the_right_thing() {
        let sc = Scenario::turn("t-capture")
            .say("记一下我对花生过敏")
            .check(tool_called("remember"))
            .check(memory_written(Some("identity"), "花生"));
        let make = || -> Arc<dyn LlmProvider> {
            Arc::new(FakeLlm::scripted(vec![
                FakeTurn {
                    tool_calls: vec![call(
                        "remember",
                        serde_json::json!({ "fact": "用户对花生过敏", "kind": "identity" }),
                    )],
                    ..Default::default()
                },
                FakeTurn { text: "记下啦!".into(), ..Default::default() },
            ]))
        };
        let r = run_scenario(make, &sc, 1).await;
        assert_eq!(r.passed, 1, "全链做对了该过;失败项={:?}", r.failed_checks);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn grader_fails_when_engine_skips_the_tool() {
        let sc = Scenario::turn("t-miss").say("记一下我对花生过敏").check(tool_called("remember"));
        let make = || -> Arc<dyn LlmProvider> {
            Arc::new(FakeLlm::scripted(vec![FakeTurn { text: "好的~".into(), ..Default::default() }]))
        };
        let r = run_scenario(make, &sc, 1).await;
        assert_eq!(r.passed, 0, "没调 remember,断言必须判失败(判官不是橡皮图章)");
        assert!(r.failed_checks.iter().any(|(n, _)| n.contains("remember")));
    }

    // consolidate 路径:FakeLlm 回一个 JSON 数组 → distilled 落库,正例断言通过。
    // (consolidate 用 provider.chat 非流式 = trait 默认 drain 流 → FakeTurn.text 即返回。)
    #[tokio::test(flavor = "multi_thread")]
    async fn grader_handles_consolidate_path() {
        let sc = Scenario::consolidate("t-distill")
            .line("user", "整理音乐按歌手分,别按专辑")
            .line("assistant", "好的,按歌手分好了")
            .check(distilled_at_least(1))
            .check(distilled_contains("歌手"));
        let make = || -> Arc<dyn LlmProvider> {
            Arc::new(FakeLlm::scripted(vec![FakeTurn {
                text: r#"[{"kind":"experience","content":"这个家整理音乐按歌手分类,不按专辑"}]"#
                    .into(),
                ..Default::default()
            }]))
        };
        let r = run_scenario(make, &sc, 1).await;
        assert_eq!(r.passed, 1, "提炼落库且含歌手该过;失败项={:?}", r.failed_checks);
    }

    // 回归:模型调完工具后**收尾没出可见文字** → `conversation_trace` 会漏(只在有可见回复时
    // 结算),`collect_trace` 直接读 payload 不漏。真机 capture-allergy 的「有记忆没轨迹」就是这个。
    #[tokio::test(flavor = "multi_thread")]
    async fn tool_detected_even_when_final_reply_is_empty() {
        let sc = Scenario::turn("t-empty-final")
            .say("记一下我对花生过敏")
            .check(tool_called("remember"));
        let make = || -> Arc<dyn LlmProvider> {
            Arc::new(FakeLlm::scripted(vec![
                FakeTurn {
                    tool_calls: vec![call(
                        "remember",
                        serde_json::json!({ "fact": "用户对花生过敏", "kind": "identity" }),
                    )],
                    text: String::new(),
                    ..Default::default()
                },
                // 收尾轮:空文字、无工具(模型调完工具不再多说)。
                FakeTurn { text: String::new(), ..Default::default() },
            ]))
        };
        let r = run_scenario(make, &sc, 1).await;
        assert_eq!(r.passed, 1, "工具跑过即使收尾无可见文字也要被检出(collect_trace 不依赖可见气泡)");
    }

    // token tally:两轮 usage 累加进 ScenarioResult.tokens(取自引擎记账,fresh DB)。
    #[tokio::test(flavor = "multi_thread")]
    async fn tally_sums_recorded_usage() {
        use crate::llm::Usage;
        let sc = Scenario::turn("t-usage").say("几点了").check(tool_called("now"));
        let make = || -> Arc<dyn LlmProvider> {
            Arc::new(FakeLlm::scripted(vec![
                FakeTurn {
                    tool_calls: vec![call("now", serde_json::json!({}))],
                    usage: Usage { input_tokens: 100, output_tokens: 10, cache_hit_tokens: 64 },
                    ..Default::default()
                },
                FakeTurn {
                    text: "三点啦".into(),
                    usage: Usage { input_tokens: 150, output_tokens: 20, cache_hit_tokens: 128 },
                    ..Default::default()
                },
            ]))
        };
        let r = run_scenario(make, &sc, 1).await;
        assert_eq!(r.passed, 1);
        assert_eq!(r.tokens.input, 250, "两轮输入累加");
        assert_eq!(r.tokens.output, 30);
        assert_eq!(r.tokens.cache_hit, 192);
    }
}
