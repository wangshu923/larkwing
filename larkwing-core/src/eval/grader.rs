//! eval 判官:把「一次运行后能观察到的一切」收成 `Observed`(工具轨迹 + 本次新写入的
//! 记忆/需知 + 提炼条数 + 收尾结局),断言用一把返回 `Check` 的组合子表达。
//!
//! 关键:`Check` **不 panic、返回 bool** —— 才好跟「跑 N 次数通过率」组合(裸 `assert!`
//! 一 panic 就没法 tally)。这不是 DSL:场景本身是 Rust,这些只是可读的断言函数;
//! 覆盖不到的怪需求走 `custom(name, 闭包)` 写任意 Rust,不必扩任何「词表」。

use crate::engine::TraceStep;
use crate::store::briefings::Briefing;
use crate::store::memory::Memory;

/// 一次驱动(turn 串 / consolidate)跑完后能观察到的一切。
pub struct Observed {
    /// 本次运行所有回合的工具步(扁平化;`TraceStep` 含 name/args/result/status)。
    pub trace: Vec<TraceStep>,
    /// 本次驱动**新写入**的记忆(已剔除 seed 预置的,只看这一跑产生的)。
    pub memories: Vec<Memory>,
    /// 本次驱动**新写入**的需知。
    pub briefings: Vec<Briefing>,
    /// consolidate 新增条数(turn 类驱动恒 0)。
    pub distilled: usize,
    /// 收尾结局。
    pub outcome: Outcome,
}

/// 回合 / 提炼的收尾结局。run 通过的前提之一 = `Done`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Done,
    Failed(String),
    Cancelled,
    Error(String),
}

impl Observed {
    /// 这次运行里调用过某工具吗?
    pub fn called(&self, name: &str) -> bool {
        self.trace.iter().any(|s| s.name == name)
    }
}

/// 一条断言:不 panic,返回是否通过。`name` 用于失败报告(哪条规则没守住)。
pub struct Check {
    pub name: String,
    run: Box<dyn Fn(&Observed) -> bool + Send + Sync>,
}

impl Check {
    pub fn new(
        name: impl Into<String>,
        run: impl Fn(&Observed) -> bool + Send + Sync + 'static,
    ) -> Check {
        Check { name: name.into(), run: Box::new(run) }
    }

    pub fn eval(&self, o: &Observed) -> bool {
        (self.run)(o)
    }
}

// ───────── 组合子:返回 `Check` 的小函数,覆盖 80% 常见断言 ─────────

/// 调用过某工具(如 `tool_called("recall")`)。
pub fn tool_called(name: &str) -> Check {
    let name = name.to_string();
    Check::new(format!("调用了 {name}"), move |o| o.called(&name))
}

/// 没调用某工具(精确反例:别的工具可以调,就是不该调这个)。
pub fn tool_not_called(name: &str) -> Check {
    let name = name.to_string();
    Check::new(format!("没调用 {name}"), move |o| !o.called(&name))
}

/// 整轮没调任何工具(闲聊反例)。
pub fn no_tool_calls() -> Check {
    Check::new("没有任何工具调用", |o| o.trace.is_empty())
}

/// 某工具以指定结局收尾(如写入超长退回 → `tool_status("remember","error")`)。
pub fn tool_status(name: &str, status: &str) -> Check {
    let (name, status) = (name.to_string(), status.to_string());
    Check::new(format!("{name} 结局为 {status}"), move |o| {
        o.trace.iter().any(|s| s.name == name && s.status == status)
    })
}

/// 本次新写入了一条记忆,内容含子串(可选限定 kind,如 identity/experience/fact)。
pub fn memory_written(kind: Option<&str>, contains: &str) -> Check {
    let kind = kind.map(str::to_string);
    let contains = contains.to_string();
    let label = match &kind {
        Some(k) => format!("写入 {k} 记忆含「{contains}」"),
        None => format!("写入记忆含「{contains}」"),
    };
    Check::new(label, move |o| {
        o.memories.iter().any(|m| {
            m.content.contains(&contains) && kind.as_deref().map_or(true, |k| m.kind == k)
        })
    })
}

/// 本次没有把某子串记进任何记忆(few-shot 泄漏回归守卫:湿地公园 / 朵朵 …)。
pub fn no_memory_contains(s: &str) -> Check {
    let s = s.to_string();
    Check::new(format!("没把「{s}」记进记忆"), move |o| {
        !o.memories.iter().any(|m| m.content.contains(&s))
    })
}

/// 本次新写入了某域需知,内容含子串。
pub fn briefing_written(domain: &str, contains: &str) -> Check {
    let (domain, contains) = (domain.to_string(), contains.to_string());
    Check::new(format!("{domain} 需知含「{contains}」"), move |o| {
        o.briefings.iter().any(|b| b.domain == domain && b.content.contains(&contains))
    })
}

/// 提炼产出为空(宁缺毋滥:没值得记的就不记,空是常态)。
pub fn distilled_empty() -> Check {
    Check::new("提炼为空", |o| o.distilled == 0)
}

/// 提炼至少产出 n 条。
pub fn distilled_at_least(n: usize) -> Check {
    Check::new(format!("提炼 ≥{n} 条"), move |o| o.distilled >= n)
}

/// 提炼出含某子串的记忆(提炼正例:被反复纠正的偏好应被蒸馏出来)。
pub fn distilled_contains(s: &str) -> Check {
    let s = s.to_string();
    Check::new(format!("提炼出含「{s}」的记忆"), move |o| {
        o.distilled > 0 && o.memories.iter().any(|m| m.content.contains(&s))
    })
}

/// 逃生口:任意复杂断言直接写 Rust,不用扩任何「词表」。
pub fn custom(name: &str, f: impl Fn(&Observed) -> bool + Send + Sync + 'static) -> Check {
    Check::new(name, f)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(name: &str, status: &str) -> TraceStep {
        TraceStep {
            name: name.into(),
            ui_key: format!("tool.{name}"),
            args: "{}".into(),
            result: "ok".into(),
            status: status.into(),
        }
    }

    fn mem(kind: &str, content: &str) -> Memory {
        Memory {
            id: 1,
            user_id: 1,
            kind: kind.into(),
            content: content.into(),
            resident: false,
            salience: 1.0,
            source: "explicit".into(),
            last_used_at: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn obs(trace: Vec<TraceStep>, memories: Vec<Memory>, distilled: usize) -> Observed {
        Observed { trace, memories, briefings: vec![], distilled, outcome: Outcome::Done }
    }

    #[test]
    fn tool_predicates() {
        let o = obs(vec![step("remember", "ok"), step("now", "error")], vec![], 0);
        assert!(tool_called("remember").eval(&o));
        assert!(!tool_called("recall").eval(&o));
        assert!(tool_not_called("recall").eval(&o));
        assert!(!tool_not_called("remember").eval(&o));
        assert!(!no_tool_calls().eval(&o));
        assert!(tool_status("now", "error").eval(&o));
        assert!(!tool_status("remember", "error").eval(&o));
        assert!(no_tool_calls().eval(&obs(vec![], vec![], 0)));
    }

    #[test]
    fn memory_predicates_respect_kind_and_substring() {
        let o = obs(vec![], vec![mem("identity", "女儿对花生过敏")], 0);
        assert!(memory_written(None, "花生").eval(&o));
        assert!(memory_written(Some("identity"), "花生").eval(&o));
        assert!(!memory_written(Some("fact"), "花生").eval(&o), "kind 不符不算");
        assert!(!memory_written(None, "海鲜").eval(&o));
        // 泄漏守卫
        assert!(no_memory_contains("湿地公园").eval(&obs(vec![], vec![], 0)));
        assert!(!no_memory_contains("花生").eval(&o));
    }

    #[test]
    fn distill_predicates() {
        let empty = obs(vec![], vec![], 0);
        assert!(distilled_empty().eval(&empty));
        assert!(!distilled_at_least(1).eval(&empty));

        let one = obs(vec![], vec![mem("experience", "整理音乐按歌手分类")], 1);
        assert!(!distilled_empty().eval(&one));
        assert!(distilled_at_least(1).eval(&one));
        assert!(distilled_contains("歌手").eval(&one));
        assert!(!distilled_contains("专辑").eval(&one));
    }

    #[test]
    fn custom_escape_hatch() {
        let o = obs(vec![step("fs_find", "ok"), step("media_play", "ok")], vec![], 0);
        let first_is_local = custom("先本地", |o| {
            matches!(o.trace.first().map(|s| s.name.as_str()), Some("fs_find" | "media_play"))
        });
        assert!(first_is_local.eval(&o));
    }
}
