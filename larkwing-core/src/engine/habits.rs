//! 行为侧提炼「这个家的惯例」(2026-11-06;§7.3「家里的惯例归家庭备忘」的自动半边):
//! 后台把最近 `LOOKBACK_DAYS` 天里**反复出现**的使用痕迹(工具行:放了什么、几点、周末还是
//! 工作日、谁;家里人说话的时段分布)统计成候选,再让便宜档模型把「称得上惯例」的写成一句句
//! 短陈述,落进家庭备忘的 `habits` 主题(home、整体覆盖重写)—— 用户**没说出口**的习惯也学得到。
//! 这是「越用越懂」唯一一条随使用时间自然变好的回路:consolidate 只看对话文字、tool 行不进转录。
//!
//! 纪律同 consolidate / diary:宁缺毋滥 —— **「多次、跨多天」由程序统计定,不交模型数数**
//! (eval 判官不会数数的教训,计数一律下沉机测),模型只负责措辞与归并;水位线 + 攒够 + 空闲;
//! cheap-model;FakeLlm 挡板(调用方);尽力件(失败不推水位线)。**用户删掉的不再写回**:上次写入
//! 的行留一份(settings),下次跑时与现存内容做差,少掉的行进「否决名单」—— 喂给模型、代码层再滤
//! 一遍。开关复用「自动记住」(`memory.auto_consolidate`),零新概念;产出在回忆页「家里的事」看得见
//! 删得掉。形状同 diary:纯函数(`event_key` / `aggregate` / `build_request` / `parse` / `finalize` /
//! `diff_removed`)+ 取料(`gather`)+ 编排(`run`,FakeLlm 可端到端测)。

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use anyhow::Result;
use chrono::{Datelike, Local, NaiveDate, TimeZone, Timelike, Weekday};

use super::{AssistantPayload, ToolRowPayload, UserMeta};
use crate::llm::{ChatMessage, ChatOptions, ChatRequest, LlmProvider, ToolChoice};
use crate::store::memory::KIND_EXPERIENCE;
use crate::store::{Briefing, Store};
use crate::tools::briefing::RESIDENT_BUDGET_CHARS;

// === 常量单源(§4.11;2026-11-06 用户拍板「开做」按建议值落地,PLAN §2 记档)===
/// 回看窗口(天):统计最近这么多天的痕迹(滑动窗,不是水位线区间 —— 惯例要看一段时间)。
pub(crate) const LOOKBACK_DAYS: i64 = 14;
/// 一条痕迹要出现这么多次才算候选(「多次」)。
pub(crate) const REPEAT_MIN: usize = 3;
/// 且分布在这么多个不同的本地日(「跨多天」;同一天狂点不算惯例)。
pub(crate) const DAYS_MIN: usize = 2;
/// 多久跑一次(水位线距今超过它就该跑)。
pub(crate) const PERIOD_MS: i64 = 7 * 86_400_000;
/// 或工具行攒到这么多条就提前跑(热闹的一周)。
pub(crate) const EVENT_THRESHOLD: i64 = 200;
/// `habits` 主题正文总上限(字符;进前缀,§4.8 注得窄)。超了按行丢尾、绝不切半句。
pub(crate) const HABITS_MAX_CHARS: usize = 300;
/// 单句上限:模型被要求 ≤40 字,超过这个硬界的整句丢弃(半句比没有糟)。
const LINE_MAX_CHARS: usize = 60;
/// 喂给模型的候选上限(按次数降序取前 N)。
const CANDIDATES_MAX: usize = 40;
/// 取料行数上限(14 天量级千行;超过只取最早的会话,记档)。
const ROWS_MAX: i64 = 20_000;
/// 否决名单上限(只留最近删的)。
const REJECTED_MAX: usize = 50;
/// 「家里人说话时段」这条统计要有这么多条发言才给(太少的分布没意义)。
const ACTIVITY_MIN_MSGS: usize = 20;
/// 每个 tool 合计行最多举几个例子(让模型从「各不相同的标题」里看出共性,如睡前故事)。
const SAMPLES_MAX: usize = 5;
/// 模型输出条数上限(法条里也写了;代码再兜一道)。
const ITEMS_MAX: usize = 8;

/// 家庭备忘里惯例落的主题名;scope 恒 home。模型自己也可能往这个主题写,下次重写时作为「已记着的」喂回。
pub(crate) const HABITS_DOMAIN: &str = "habits";
/// 水位线:上次跑的时刻(app 级 settings;小状态不开新域 §6.2)。None = 还没跑过(首次 = 有痕迹就学)。
pub(crate) const WATERMARK_KEY: &str = "habits.covered_until_ms";
/// 上次写进 `habits` 的行(JSON 数组):下次跑时与现存内容做差,少掉的 = 用户删了。
const LAST_WRITTEN_KEY: &str = "habits.last_written";
/// 否决名单(JSON 数组):用户删过的句子,绝不再写回。
const REJECTED_KEY: &str = "habits.rejected";
/// 写进个人按需记忆的那些句子(JSON 数组,每项「用户 id + 分隔符 + 句子」):用户在回忆页删掉
/// 其中一句时据此认出「这是惯例提炼写的」→ 进否决名单(`note_memory_deleted`)。
const PERSONAL_WRITTEN_KEY: &str = "habits.personal_written";
/// 上面那张表的界(纯管护:只为不无限长,不是产品阈值)。
const PERSONAL_WRITTEN_MAX: usize = 500;
const PERSONAL_SEP: char = '\u{1f}';
/// 行与行之间的分隔(备忘正文是一段话;拆回行时也认 `;` 与换行)。
const SEP: &str = "；";

/// 时段表(本地时):(标签, 起始时, 结束时),深夜跨零点。只用于把时刻归桶给模型看。
const BUCKETS: &[(&str, u32, u32)] = &[
    ("清晨", 5, 8),
    ("上午", 8, 11),
    ("中午", 11, 14),
    ("下午", 14, 18),
    ("傍晚", 18, 20),
    ("晚上", 20, 23),
    ("深夜", 23, 5),
];

/// 会「留下惯例」的工具(写 / 做事类;读类、控制类、内部类不算 —— 翻目录不是惯例,放什么才是)。
/// `batch` 不在表里但会被展开成内层工具的事件。
const HABIT_TOOLS: &[&str] = &[
    "media_play",
    "media_search",
    "media_download",
    "lyrics_fetch",
    "torrent_download",
    "web_download",
    "ffmpeg_run",
    "fs_move",
    "fs_copy",
    "fs_mkdir",
    "fs_trash",
    "fs_zip",
    "fs_unzip",
    "reminder_set",
    "weather",
    "print_file",
    "send_file",
    "send_text",
    "speak_to_file",
    "web_fetch",
    "web_render",
];

/// 整理器法条(人格中立、不与人对话,§5):立规则,不教学。数字由程序给,模型不数数。
const SYSTEM: &str = "你是家用助手的后台整理器,不与任何人对话,只输出 JSON。\
  「最近的使用痕迹」是程序从这个家最近一段时间的操作记录里统计出来的,每条都已经满足「多次、跨多天」,\
  里面的次数、天数、时段、谁都是真的,你不用再数。你的活是判断其中哪些称得上「这个家的惯例」或\
  「某个人的习惯」,各写成一句简短的第三人称陈述(不超过 40 字,写清时段 / 周末 / 谁 / 做什么,\
  例如「晚上八点后常放睡前故事」「周末下午常看动画片」);相近的几条合成一句;一条痕迹最多写一句。\
  只写痕迹里统计出来的,不推测动机、不编造、不写一次性的事。\
  「已记着的惯例」是此前写下的:仍被痕迹支持的保留原句不改字,证据变了就改写,痕迹里再也不出现的删掉。\
  「用户删掉的」是用户明确不想要的,绝不再写,换种说法也不行。\
  没有值得写的就输出 [] —— 这是常态,不是失职。\
  输出一个 JSON 数组,每项 {\"text\":\"...\",\"who\":\"\"}:只关于某一个人的习惯,who 填痕迹里那个人的称呼;\
  关于这个家或大家的,who 留空。最多 8 条。";

/// 一条行为事件:哪个工具、对什么(归一后的键)、几时、谁。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Event {
    pub tool: String,
    pub key: String,
    pub at_ms: i64,
    pub who: String,
}

/// 取料结果:工具事件 + 家里人发言(时刻, 谁)。
#[derive(Debug, Default)]
pub(crate) struct Gathered {
    pub events: Vec<Event>,
    pub user_msgs: Vec<(i64, String)>,
}

fn local_dt(ms: i64) -> Option<chrono::DateTime<Local>> {
    Local.timestamp_millis_opt(ms).single()
}

fn bucket_of(hour: u32) -> &'static str {
    for (label, lo, hi) in BUCKETS {
        let hit = if lo < hi { hour >= *lo && hour < *hi } else { hour >= *lo || hour < *hi };
        if hit {
            return label;
        }
    }
    "深夜"
}

/// 结果文本里第一对《》中的内容。
fn between_brackets(s: &str) -> Option<&str> {
    let a = s.find('《')? + '《'.len_utf8();
    let rel = s[a..].find('》')?;
    Some(&s[a..a + rel])
}

/// 去掉「第N集」式尾巴(剧集每集标题不同,归到一部剧上)。只认「第 + 阿拉伯数字」,保守。
fn strip_episode(title: &str) -> String {
    let chars: Vec<char> = title.chars().collect();
    for i in 0..chars.len() {
        if chars[i] == '第' && chars.get(i + 1).is_some_and(|c| c.is_ascii_digit()) {
            let head: String = chars[..i].iter().collect();
            let head = head.trim_end_matches([' ', '·', '-', '_']).trim();
            if !head.is_empty() {
                return head.to_string();
            }
        }
    }
    title.trim().to_string()
}

/// 把一个入参值压成可归并的短键:URL → 主机名;路径 → 末两级(文件去掉、留文件夹);其余截 30 字。
fn summarize_value(v: &str) -> String {
    let v = v.trim();
    if let Some(rest) = v.strip_prefix("https://").or_else(|| v.strip_prefix("http://")) {
        let host = rest.split(['/', '?', '#']).next().unwrap_or(rest);
        return host.strip_prefix("www.").unwrap_or(host).to_string();
    }
    let is_path = v.contains('/') || v.contains('\\') || v.starts_with('~');
    if is_path {
        let mut parts: Vec<&str> = v.split(['/', '\\']).filter(|p| !p.is_empty()).collect();
        if parts.len() > 1 && parts.last().is_some_and(|l| l.contains('.')) {
            parts.pop(); // 文件 → 它所在的文件夹
        }
        let n = parts.len();
        return parts[n.saturating_sub(2)..].join("/");
    }
    v.chars().take(30).collect()
}

/// 第一个有值的入参(按「最能说明在干什么」的顺序)。
fn pick_arg(args: &serde_json::Value) -> Option<String> {
    for f in ["query", "content", "url", "path", "root", "dir", "output", "to"] {
        if let Some(s) = args.get(f).and_then(serde_json::Value::as_str) {
            if !s.trim().is_empty() {
                return Some(s.to_string());
            }
        }
    }
    if let Some(s) = args.get("paths").and_then(|v| v.get(0)).and_then(serde_json::Value::as_str) {
        return Some(s.to_string());
    }
    for arr in ["moves", "copies"] {
        if let Some(s) =
            args.get(arr).and_then(|v| v.get(0)).and_then(|m| m.get("dst")).and_then(serde_json::Value::as_str)
        {
            return Some(s.to_string());
        }
    }
    None
}

/// 事件键:media_play 优先取结果里的《标题》(剧集归一到一部剧);其余按入参压成短键;
/// reminder_set 带上重复周期。拿不到 = 空键(同工具的事件归一组)。
pub(crate) fn event_key(tool: &str, args: &serde_json::Value, result_head: &str) -> String {
    if tool == "media_play" {
        if let Some(t) = between_brackets(result_head) {
            return format!("《{}》", strip_episode(t));
        }
        if let Some(u) = args.get("url").and_then(serde_json::Value::as_str) {
            let v = u.trim();
            if !(v.starts_with("http://") || v.starts_with("https://")) {
                // 本地文件:取文件名(去扩展名);文件夹:末级名
                let last = v.split(['/', '\\']).rfind(|p| !p.is_empty()).unwrap_or(v);
                let stem = last.rsplit_once('.').map(|(s, _)| s).unwrap_or(last);
                return format!("《{}》", strip_episode(stem));
            }
            return summarize_value(v);
        }
        return String::new();
    }
    let mut key = pick_arg(args).map(|v| summarize_value(&v)).unwrap_or_default();
    if tool == "reminder_set" {
        if let Some(r) = args.get("repeat").and_then(serde_json::Value::as_str) {
            if !matches!(r, "" | "none" | "once") {
                key.push_str(&format!(" 重复:{r}"));
            }
        }
    }
    key
}

/// 一组事件的统计(键组 / 工具合计 / 家里人发言共用)。
#[derive(Debug, Default)]
struct Stats {
    count: usize,
    days: BTreeSet<NaiveDate>,
    buckets: BTreeMap<&'static str, usize>,
    weekend: usize,
    who: BTreeMap<String, usize>,
}

impl Stats {
    fn add(&mut self, at_ms: i64, who: &str) {
        let Some(t) = local_dt(at_ms) else { return };
        self.count += 1;
        self.days.insert(t.date_naive());
        *self.buckets.entry(bucket_of(t.hour())).or_insert(0) += 1;
        if matches!(t.weekday(), Weekday::Sat | Weekday::Sun) {
            self.weekend += 1;
        }
        *self.who.entry(who.to_string()).or_insert(0) += 1;
    }

    fn qualifies(&self) -> bool {
        self.count >= REPEAT_MIN && self.days.len() >= DAYS_MIN
    }

    /// 「N 次 / D 天(MM-DD~MM-DD);时段 晚上 6、下午 1;周末 5 / 工作日 2;谁:小明 6、用户 1」
    fn describe(&self, unit: &str) -> String {
        let (first, last) = (self.days.iter().next(), self.days.iter().next_back());
        let span = match (first, last) {
            (Some(a), Some(b)) if a != b => format!("({}~{})", a.format("%m-%d"), b.format("%m-%d")),
            (Some(a), _) => format!("({})", a.format("%m-%d")),
            _ => String::new(),
        };
        let mut buckets: Vec<(&str, usize)> = self.buckets.iter().map(|(k, v)| (*k, *v)).collect();
        buckets.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        let buckets: Vec<String> = buckets.iter().take(3).map(|(k, v)| format!("{k} {v}")).collect();
        let mut who: Vec<(&String, &usize)> = self.who.iter().collect();
        who.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        let who: Vec<String> = who.iter().take(3).map(|(k, v)| format!("{k} {v}")).collect();
        format!(
            "{} {unit} / {} 天{span};时段 {};周末 {} / 工作日 {};谁:{}",
            self.count,
            self.days.len(),
            buckets.join("、"),
            self.weekend,
            self.count - self.weekend,
            who.join("、"),
        )
    }
}

/// 统计 → 候选证据行(纯函数,可单测)。三类:① 同工具同键的组(≥REPEAT_MIN 次、≥DAYS_MIN 天),
/// ② 每个工具的合计(键不止一种时才给,附几个例子),③ 家里人发言的时段分布。
/// 阈值在这里执法 —— 模型拿到的每一行都已经是「多次、跨多天」。
pub(crate) fn aggregate(events: &[Event], user_msgs: &[(i64, String)]) -> Vec<String> {
    let mut groups: BTreeMap<(String, String), Stats> = BTreeMap::new();
    let mut per_tool: BTreeMap<String, Stats> = BTreeMap::new();
    let mut keys_of: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
    for e in events {
        groups.entry((e.tool.clone(), e.key.clone())).or_default().add(e.at_ms, &e.who);
        per_tool.entry(e.tool.clone()).or_default().add(e.at_ms, &e.who);
        if !e.key.is_empty() {
            *keys_of.entry(e.tool.clone()).or_default().entry(e.key.clone()).or_insert(0) += 1;
        }
    }
    let mut cands: Vec<(usize, String)> = groups
        .iter()
        .filter(|(_, s)| s.qualifies())
        .map(|((tool, key), s)| {
            let what = if key.is_empty() { tool.clone() } else { format!("{tool} {key}") };
            (s.count, format!("- {what}:{}", s.describe("次")))
        })
        .collect();
    cands.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let mut lines: Vec<String> = cands.into_iter().take(CANDIDATES_MAX).map(|(_, l)| l).collect();

    let mut totals: Vec<(usize, String)> = per_tool
        .iter()
        .filter(|(tool, s)| s.qualifies() && keys_of.get(*tool).is_some_and(|k| k.len() > 1))
        .map(|(tool, s)| {
            let mut samples: Vec<(&String, &usize)> = keys_of[tool].iter().collect();
            samples.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
            let samples: Vec<String> = samples
                .iter()
                .take(SAMPLES_MAX)
                .map(|(k, n)| if **n > 1 { format!("{k}×{n}") } else { (*k).clone() })
                .collect();
            (s.count, format!("- {tool} 合计:{};例如:{}", s.describe("次"), samples.join("、")))
        })
        .collect();
    totals.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    lines.extend(totals.into_iter().map(|(_, l)| l));

    let mut act = Stats::default();
    for (at, who) in user_msgs {
        act.add(*at, who);
    }
    if act.count >= ACTIVITY_MIN_MSGS && act.days.len() >= DAYS_MIN {
        lines.push(format!("- 家里人跟助手说话:{}", act.describe("条")));
    }
    lines
}

/// 家里人 id → 称呼(主人出厂名「我」在第三方视角下改称「用户」;家人用名字)。
fn display_names(store: &Store) -> Result<HashMap<i64, String>> {
    Ok(store
        .users
        .list()?
        .into_iter()
        .map(|u| (u.id, if u.name.trim() == "我" || u.name.trim().is_empty() { "用户".to_string() } else { u.name }))
        .collect())
}

/// 取料:扫窗口内的行为流水,按会话顺序配对(assistant 行的 tool_calls 给入参、user 行给说话人),
/// 只收 `HABIT_TOOLS` 里 status=ok 的工具行(`batch` 展开成内层工具);家里人发言另攒一份给时段分布。
pub(crate) fn gather(store: &Store, from_ms: i64, to_ms: i64) -> Result<Gathered> {
    let rows = store.chat.behavior_rows_between(from_ms, to_ms, ROWS_MAX)?;
    let names = display_names(store)?;
    let mut owner_of: HashMap<i64, Option<i64>> = HashMap::new();
    let mut out = Gathered::default();
    let mut cur_conv = i64::MIN;
    let mut speaker: Option<i64> = None;
    // 正处在自启回合里(提醒到点 / 后台汇报):它调的工具是程序按计划 / 接着干活调的,不是谁的
    // 使用习惯(2026-11-11)。原先这些也算痕迹、还记在上一个说话人头上 —— 每天定时的播报
    // 天然满足「多次、跨多天」,会被写成一条等于在复述定时任务的「惯例」。
    let mut self_started = false;
    let mut calls: HashMap<String, (String, serde_json::Value)> = HashMap::new();
    for r in rows {
        if r.conversation_id != cur_conv {
            cur_conv = r.conversation_id;
            speaker = None;
            self_started = false;
            calls.clear();
        }
        let owner = *owner_of
            .entry(r.conversation_id)
            .or_insert_with(|| store.chat.get_conversation(r.conversation_id).ok().flatten().map(|c| c.user_id));
        let who = |sp: Option<i64>| -> String {
            sp.or(owner).and_then(|id| names.get(&id).cloned()).unwrap_or_else(|| "家里人".into())
        };
        match r.role.as_str() {
            "event" => self_started = true,
            "user" => {
                self_started = false;
                speaker = r
                    .payload
                    .as_deref()
                    .and_then(|p| serde_json::from_str::<UserMeta>(p).ok())
                    .and_then(|m| m.speaker_user);
                out.user_msgs.push((r.created_at, who(speaker)));
            }
            "assistant" => {
                if let Some(p) = r.payload.as_deref().and_then(|p| serde_json::from_str::<AssistantPayload>(p).ok()) {
                    for tc in p.tool_calls {
                        calls.insert(tc.id, (tc.name, tc.args));
                    }
                }
            }
            "tool" => {
                let Some(p) = r.payload.as_deref().and_then(|p| serde_json::from_str::<ToolRowPayload>(p).ok())
                else {
                    continue;
                };
                if p.status != "ok" || self_started {
                    continue; // 失败的尝试不是惯例;自启回合里的调用也不是(见 self_started)
                }
                let (name, args) = calls.remove(&p.call_id).unwrap_or((p.name.clone(), serde_json::Value::Null));
                let who = who(speaker);
                if name == "batch" {
                    let inner = args.get("calls").and_then(serde_json::Value::as_array).cloned().unwrap_or_default();
                    for c in inner {
                        let Some(tool) = c.get("tool").and_then(serde_json::Value::as_str) else { continue };
                        if !HABIT_TOOLS.contains(&tool) {
                            continue;
                        }
                        let a = c.get("args").cloned().unwrap_or(serde_json::Value::Null);
                        out.events.push(Event {
                            tool: tool.to_string(),
                            key: event_key(tool, &a, ""),
                            at_ms: r.created_at,
                            who: who.clone(),
                        });
                    }
                } else if HABIT_TOOLS.contains(&name.as_str()) {
                    out.events.push(Event {
                        key: event_key(&name, &args, &r.content),
                        tool: name,
                        at_ms: r.created_at,
                        who,
                    });
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

/// 构造请求(纯函数):system=整理器法条,body=已记着的 + 用户删掉的 + 证据行。
pub(crate) fn build_request(
    evidence: &[String],
    prior: Option<&str>,
    kept: &[String],
    rejected: &[String],
) -> ChatRequest {
    let mut body = String::from("【已记着的惯例】\n");
    match prior.map(split_lines).filter(|v| !v.is_empty()) {
        Some(lines) => {
            for l in lines {
                body.push_str("- ");
                body.push_str(&l);
                body.push('\n');
            }
        }
        None => body.push_str("(暂无)\n"),
    }
    // 不是这个整理器写的行(用户在聊天里亲口说、或在回忆页自己改的):程序原样保留,不归模型管
    if !kept.is_empty() {
        body.push_str("\n【家里人自己写下的(程序会原样保留,不用你写,也别重复)】\n");
        for l in kept {
            body.push_str("- ");
            body.push_str(l);
            body.push('\n');
        }
    }
    body.push_str("\n【用户删掉的(别再写)】\n");
    if rejected.is_empty() {
        body.push_str("(无)\n");
    } else {
        for l in rejected {
            body.push_str("- ");
            body.push_str(l);
            body.push('\n');
        }
    }
    body.push_str(&format!("\n【最近 {LOOKBACK_DAYS} 天的使用痕迹】\n"));
    for l in evidence {
        body.push_str(l);
        body.push('\n');
    }
    ChatRequest {
        system: SYSTEM.into(),
        messages: vec![ChatMessage::User { content: body, parts: vec![] }],
        options: ChatOptions::default(),
        tools: vec![],
        tool_choice: ToolChoice::default(),
    }
}

/// 一句的归一形(比较用):去首尾空白与句末句号。
fn norm(s: &str) -> String {
    s.trim().trim_end_matches(['。', '.']).trim().to_string()
}

/// 备忘正文拆回行(写入时用 `SEP` 连;也认 `;` 与换行 —— 模型自己 briefing_write 的也拆得开)。
pub(crate) fn split_lines(content: &str) -> Vec<String> {
    content.split(['；', ';', '\n']).map(norm).filter(|s| !s.is_empty()).collect()
}

/// 解析模型输出:容忍 ```json 围栏 / 前后废话;空 text 丢;封顶条数。解析失败 = 空(尽力件)。
pub(crate) fn parse(text: &str) -> Vec<(String, String)> {
    let slice = match (text.find('['), text.rfind(']')) {
        (Some(a), Some(b)) if b > a => &text[a..=b],
        _ => return Vec::new(),
    };
    let raw: Vec<serde_json::Value> = serde_json::from_str(slice).unwrap_or_default();
    raw.into_iter()
        .filter_map(|v| {
            let text = norm(v.get("text").and_then(serde_json::Value::as_str)?);
            if text.is_empty() {
                return None;
            }
            let who = v.get("who").and_then(serde_json::Value::as_str).map(str::trim).unwrap_or("").to_string();
            Some((text, who))
        })
        .take(ITEMS_MAX)
        .collect()
}

/// 落库前的代码层过滤:超长整句丢、否决名单的丢、去重;`who` 认不出的人并进家级;
/// 家级按 `HABITS_MAX_CHARS` 按行丢尾。返回 (家级行, 个人行 (称呼, 句子))。
pub(crate) fn finalize(
    items: Vec<(String, String)>,
    rejected: &[String],
    known: &HashSet<String>,
) -> (Vec<String>, Vec<(String, String)>) {
    let banned: HashSet<String> = rejected.iter().map(|s| norm(s)).collect();
    let mut seen: HashSet<String> = HashSet::new();
    let mut home: Vec<String> = Vec::new();
    let mut personal: Vec<(String, String)> = Vec::new();
    for (text, who) in items {
        if text.chars().count() > LINE_MAX_CHARS {
            tracing::info!(target: "larkwing::habits", "惯例句过长,整句丢弃:{text}");
            continue;
        }
        if banned.contains(&text) || !seen.insert(text.clone()) {
            continue;
        }
        if !who.is_empty() && known.contains(&who) {
            personal.push((who, text));
        } else {
            home.push(text);
        }
    }
    let mut kept: Vec<String> = Vec::new();
    let mut used = 0usize;
    for l in home {
        let add = l.chars().count() + if kept.is_empty() { 0 } else { SEP.chars().count() };
        if used + add > HABITS_MAX_CHARS {
            tracing::info!(target: "larkwing::habits", "惯例正文超过 {HABITS_MAX_CHARS} 字,丢掉尾行:{l}");
            continue;
        }
        used += add;
        kept.push(l);
    }
    (kept, personal)
}

/// 上次写入的行里、现存内容里**没有了**的 = 用户删掉的(整条没了 = 全部删掉)。
pub(crate) fn diff_removed(last_written: &[String], current: Option<&str>) -> Vec<String> {
    let now: HashSet<String> = current.map(split_lines).unwrap_or_default().into_iter().collect();
    last_written.iter().map(|s| norm(s)).filter(|s| !s.is_empty() && !now.contains(s)).collect()
}

fn read_list(store: &Store, key: &str) -> Result<Vec<String>> {
    Ok(store
        .settings
        .get(None, key)?
        .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
        .unwrap_or_default())
}

fn write_list(store: &Store, key: &str, v: &[String]) -> Result<()> {
    store.settings.set(None, key, &serde_json::to_string(v)?)
}

/// 现存内容里**不是**上次由这里写下的行(用户在聊天里亲口说的惯例、回忆页自己改过的句子)。
/// 这些行不归整理器管:不给模型改、不会因为「痕迹里没有」被删,重写时原样带上(2026-11-11)。
/// 此前整主题覆盖 —— 法条又把「家里的惯例」指向这个主题,用户亲口说的话下一次周跑就没了。
pub(crate) fn foreign_lines(last_written: &[String], current: Option<&str>) -> Vec<String> {
    let ours: HashSet<String> = last_written.iter().map(|s| norm(s)).collect();
    current.map(split_lines).unwrap_or_default().into_iter().filter(|l| !ours.contains(l)).collect()
}

/// 用户在回忆页删掉了一条记忆:它若是惯例提炼写的个人习惯 → 进否决名单,不再写回(2026-11-11)。
/// 原先个人习惯只靠「有没有近似的」去重,删掉之后下周证据还在就又写回来,与家级那条
/// 「用户删掉的不再写回」不一致。只认**用户的删除**(调用方 = 回忆页的删除命令);
/// 记忆维护自己清掉的不走这里,不会被误当成用户不想要。
pub(crate) fn note_memory_deleted(store: &Store, user_id: i64, content: &str) -> Result<()> {
    let entry = format!("{user_id}{PERSONAL_SEP}{}", norm(content));
    let mut written = read_list(store, PERSONAL_WRITTEN_KEY)?;
    let Some(pos) = written.iter().position(|w| *w == entry) else { return Ok(()) };
    written.remove(pos);
    write_list(store, PERSONAL_WRITTEN_KEY, &written)?;
    let line = norm(content);
    let mut rejected = read_list(store, REJECTED_KEY)?;
    if !rejected.iter().any(|x| norm(x) == line) {
        rejected.push(line);
        if rejected.len() > REJECTED_MAX {
            let cut = rejected.len() - REJECTED_MAX;
            rejected.drain(..cut);
        }
        write_list(store, REJECTED_KEY, &rejected)?;
        tracing::info!(target: "larkwing::habits", user = user_id, "用户删掉的个人习惯进否决名单");
    }
    Ok(())
}

/// 取料 + 否决名单更新(阻塞段,整段进线程池)。
struct Prepared {
    evidence: Vec<String>,
    prior: Option<Briefing>,
    /// 上次由这里写下、现在还在的行(给模型管)。
    ours: Vec<String>,
    /// 不是这里写的行(原样保留)。
    foreign: Vec<String>,
    rejected: Vec<String>,
    names: HashMap<i64, String>,
    owner: i64,
}

fn prepare(store: &Store, from_ms: i64, to_ms: i64) -> Result<Prepared> {
    let owner = store.users.ensure_default_user()?.id;
    let g = gather(store, from_ms, to_ms)?;
    let evidence = aggregate(&g.events, &g.user_msgs);
    let prior = store
        .briefings
        .list_for(owner)?
        .into_iter()
        .find(|b| b.scope == crate::store::briefings::scope_home() && b.domain == HABITS_DOMAIN);
    let last = read_list(store, LAST_WRITTEN_KEY)?;
    let mut rejected = read_list(store, REJECTED_KEY)?;
    let removed = diff_removed(&last, prior.as_ref().map(|b| b.content.as_str()));
    if !removed.is_empty() {
        for r in removed {
            if !rejected.iter().any(|x| norm(x) == r) {
                rejected.push(r);
            }
        }
        if rejected.len() > REJECTED_MAX {
            let cut = rejected.len() - REJECTED_MAX;
            rejected.drain(..cut);
        }
        write_list(store, REJECTED_KEY, &rejected)?;
        tracing::info!(target: "larkwing::habits", n = rejected.len(), "用户删过的惯例进否决名单");
    }
    let names = display_names(store)?;
    let current = prior.as_ref().map(|b| b.content.as_str());
    let foreign = foreign_lines(&last, current);
    let ours: Vec<String> =
        current.map(split_lines).unwrap_or_default().into_iter().filter(|l| !foreign.contains(l)).collect();
    Ok(Prepared { evidence, prior, ours, foreign, rejected, names, owner })
}

/// 跑一次惯例提炼(调用方 = `Engine::spawn_habits`,已做限流 / 回合在飞让路 / 防重入 / FakeLlm 挡板):
/// 空闲(最近 `IDLE_WINDOW_MS` 全局无新消息)且到期(水位线距今 ≥ `PERIOD_MS`,或工具行攒到
/// `EVENT_THRESHOLD`,或还没跑过)才动;没有够格的痕迹 = 只推水位线不花钱;失败不推水位线。
/// 返回写入的句数(家级 + 个人)。
pub(crate) async fn run(provider: &Arc<dyn LlmProvider>, store: &Store, now_ms: i64) -> Result<usize> {
    let set_wm = |ms: i64| store.settings.set(None, WATERMARK_KEY, &ms.to_string());
    if let Some(last) = store.chat.last_message_at()? {
        if now_ms.saturating_sub(last) < super::diary::IDLE_WINDOW_MS {
            return Ok(0);
        }
    }
    let wm = store.settings.get(None, WATERMARK_KEY)?.and_then(|s| s.parse::<i64>().ok());
    let due = match wm {
        None => true,
        Some(wm) => {
            now_ms.saturating_sub(wm) >= PERIOD_MS || store.chat.count_tool_rows_between(wm, now_ms)? >= EVENT_THRESHOLD
        }
    };
    if !due {
        return Ok(0);
    }
    let from_ms = now_ms - LOOKBACK_DAYS * 86_400_000;
    let prepared = {
        let store = store.clone();
        tokio::task::spawn_blocking(move || prepare(&store, from_ms, now_ms)).await??
    };
    if prepared.evidence.is_empty() {
        set_wm(now_ms)?; // 没有够格的痕迹:翻篇,不烧钱
        return Ok(0);
    }
    let ours = prepared.ours.join(SEP);
    let mut req = build_request(
        &prepared.evidence,
        (!ours.is_empty()).then_some(ours.as_str()),
        &prepared.foreign,
        &prepared.rejected,
    );
    // 思考档随全局 `llm.thinking`(与 consolidate 同款:缺省 Medium —— 后台不卡延迟,质量优先)。
    let thinking = match store.settings.get(None, "llm.thinking")?.as_deref() {
        Some("off") => crate::llm::Thinking::Off,
        Some("light") => crate::llm::Thinking::Light,
        Some("heavy") => crate::llm::Thinking::Heavy,
        _ => crate::llm::Thinking::Medium,
    };
    if thinking != crate::llm::Thinking::Off {
        req.options.thinking = Some(thinking);
    }
    let text = provider.chat(req).await.map_err(|e| anyhow::anyhow!("惯例提炼 LLM 调用失败: {e:?}"))?;
    let known: HashSet<String> = prepared.names.values().cloned().collect();
    let (home, personal) = finalize(parse(&text), &prepared.rejected, &known);
    // 家里人自己写下的行原样带上、排在前面;模型写的与它们重复的不要
    let home: Vec<String> = home.into_iter().filter(|l| !prepared.foreign.contains(l)).collect();
    let mut written = 0usize;
    if !home.is_empty() {
        let content =
            prepared.foreign.iter().chain(home.iter()).cloned().collect::<Vec<_>>().join(SEP);
        // 预算执法:镜像 briefing_write(写入时定常驻与否,装配无条件全装 → 前缀字节稳定);
        // 覆盖同主题时旧字数让位。按主人视角算(home 行对谁都算数)。
        let old_chars = prepared
            .prior
            .as_ref()
            .filter(|b| b.resident)
            .map(|b| b.content.chars().count() + b.domain.chars().count())
            .unwrap_or(0);
        let new_chars = content.chars().count() + HABITS_DOMAIN.chars().count();
        let total = store.briefings.resident_chars(prepared.owner)?.saturating_sub(old_chars) + new_chars;
        let resident = total <= RESIDENT_BUDGET_CHARS;
        store.briefings.upsert(&crate::store::briefings::scope_home(), HABITS_DOMAIN, &content, resident)?;
        write_list(store, LAST_WRITTEN_KEY, &home)?;
        written += home.len();
        tracing::info!(target: "larkwing::habits", lines = home.len(), resident, "惯例已写进家庭备忘:{content}");
    } else if prepared.prior.is_some() {
        tracing::info!(target: "larkwing::habits", "模型没给出家级惯例,已记着的原条不动");
    }
    let by_name: HashMap<&String, i64> = prepared.names.iter().map(|(id, n)| (n, *id)).collect();
    let mut personal_written = read_list(store, PERSONAL_WRITTEN_KEY)?;
    let before = personal_written.len();
    for (who, text) in personal {
        let Some(&uid) = by_name.get(&who) else { continue };
        if store.memory.has_similar(uid, &text)? {
            continue;
        }
        store.memory.add_distilled(uid, KIND_EXPERIENCE, &text)?;
        personal_written.push(format!("{uid}{PERSONAL_SEP}{}", norm(&text)));
        written += 1;
        tracing::info!(target: "larkwing::habits", user = uid, "个人习惯进按需记忆:{text}");
    }
    if personal_written.len() != before {
        if personal_written.len() > PERSONAL_WRITTEN_MAX {
            let cut = personal_written.len() - PERSONAL_WRITTEN_MAX;
            personal_written.drain(..cut);
        }
        write_list(store, PERSONAL_WRITTEN_KEY, &personal_written)?;
    }
    set_wm(now_ms)?;
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::fake::{FakeLlm, FakeTurn};

    fn store_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("lw-habits-{}-{tag}.db", std::process::id()))
    }

    fn store(tag: &str) -> Store {
        let p = store_path(tag);
        let _ = std::fs::remove_file(&p);
        Store::open(&p).unwrap()
    }

    /// 把一条消息的时间戳改到指定时刻(`append_message` 只会写「此刻」);另开连接直接改库,
    /// 不为测试往生产代码里加口子(diary 测试同款)。
    fn backdate(tag: &str, id: i64, at_ms: i64) {
        let c = rusqlite::Connection::open(store_path(tag)).unwrap();
        c.execute("UPDATE messages SET created_at = ?1 WHERE id = ?2", rusqlite::params![at_ms, id]).unwrap();
    }

    /// 本地某日某时的毫秒时刻(日期相对今天往前数;DST 歧义取 earliest,国内无 DST)。
    fn at(days_ago: i64, hour: u32, minute: u32) -> i64 {
        let d = Local::now().date_naive() - chrono::Duration::days(days_ago);
        Local.from_local_datetime(&d.and_hms_opt(hour, minute, 0).unwrap()).earliest().unwrap().timestamp_millis()
    }

    fn ev(tool: &str, key: &str, at_ms: i64, who: &str) -> Event {
        Event { tool: tool.into(), key: key.into(), at_ms, who: who.into() }
    }

    /// 剧本:若 LLM 真被调,这条会落库 → 「表里没有」即反证没调(FakeLlm 空队列是回声、不报错)。
    fn tattletale() -> Arc<dyn LlmProvider> {
        let json = "[{\"text\":\"要是我被调用了这条就会出现\",\"who\":\"\"}]";
        Arc::new(FakeLlm::scripted(vec![FakeTurn { text: json.into(), ..Default::default() }]))
    }

    fn habits_row(s: &Store) -> Option<Briefing> {
        let me = s.users.ensure_default_user().unwrap();
        s.briefings.list_for(me.id).unwrap().into_iter().find(|b| b.domain == HABITS_DOMAIN)
    }

    /// 种一轮「放了某部剧」:assistant 带 tool_calls 行 + tool 结果行,都改到指定时刻。
    fn seed_play(s: &Store, tag: &str, conv: i64, title: &str, at_ms: i64) {
        let call = format!(
            r#"{{"tool_calls":[{{"id":"c{at_ms}","name":"media_play","args":{{"url":"https://www.bilibili.com/video/BV1"}}}}]}}"#
        );
        let a = s.chat.append_message_full(conv, "assistant", "", Some(&call)).unwrap();
        backdate(tag, a.id, at_ms - 1000);
        let row = format!(r#"{{"call_id":"c{at_ms}","name":"media_play","status":"ok"}}"#);
        let t = s
            .chat
            .append_message_full(conv, "tool", &format!("已开始播放《{title}》(第1集/共26集)"), Some(&row))
            .unwrap();
        backdate(tag, t.id, at_ms);
    }

    #[test]
    fn bucket_table_covers_the_clock() {
        assert_eq!(bucket_of(6), "清晨");
        assert_eq!(bucket_of(20), "晚上");
        assert_eq!(bucket_of(22), "晚上");
        assert_eq!(bucket_of(23), "深夜");
        assert_eq!(bucket_of(2), "深夜");
        assert_eq!(bucket_of(13), "中午");
    }

    #[test]
    fn event_key_prefers_result_title_and_summarizes_args() {
        // media_play:结果里的《标题》优先,且「第N集」尾巴归到一部剧
        let k = event_key("media_play", &serde_json::json!({"url": "https://x"}), "已开始播放《汪汪队立大功 第3集》(第3集/共26集)");
        assert_eq!(k, "《汪汪队立大功》");
        // 结果拿不到(批内 / 截断)→ 本地路径取文件名去扩展名
        let k = event_key("media_play", &serde_json::json!({"url": "D:\\Music\\周杰伦\\晴天.mp3"}), "");
        assert_eq!(k, "《晴天》");
        // 其它工具:URL → 主机;路径 → 末两级文件夹;query → 原文截断;提醒带重复周期
        assert_eq!(event_key("web_download", &serde_json::json!({"url": "https://www.example.com/a/b.zip"}), ""), "example.com");
        assert_eq!(event_key("fs_move", &serde_json::json!({"moves": [{"src": "a", "dst": "D:\\Music\\周杰伦\\x.mp3"}]}), ""), "Music/周杰伦");
        assert_eq!(event_key("fs_mkdir", &serde_json::json!({"path": "D:\\Music\\周杰伦"}), ""), "Music/周杰伦");
        assert_eq!(event_key("media_search", &serde_json::json!({"query": "海贼王"}), ""), "海贼王");
        assert_eq!(event_key("reminder_set", &serde_json::json!({"content": "倒垃圾", "repeat": "weekly"}), ""), "倒垃圾 重复:weekly");
        assert_eq!(event_key("weather", &serde_json::json!({"when": "3d"}), ""), "", "没有可归并的入参 = 空键");
    }

    /// 阈值在代码里执法:≥3 次且 ≥2 天才成候选;同一天狂点不算;合计行只在键不止一种时给;
    /// 发言太少不给时段分布。
    #[test]
    fn aggregate_enforces_repeat_and_days_and_reports_counts() {
        let mut events = Vec::new();
        // 《A》:4 次 / 3 天,都在晚上
        for (d, h) in [(1, 20), (2, 21), (3, 20), (3, 22)] {
            events.push(ev("media_play", "《A》", at(d, h, 30), "小明"));
        }
        // 《B》:只 2 次 → 不够
        events.push(ev("media_play", "《B》", at(1, 10, 0), "用户"));
        events.push(ev("media_play", "《B》", at(2, 10, 0), "用户"));
        // 《C》:3 次但同一天 → 不够
        for h in [9, 10, 11] {
            events.push(ev("media_play", "《C》", at(4, h, 0), "用户"));
        }
        // weather:3 次 / 3 天,单一键 → 有候选行、无合计行
        for d in [1, 2, 3] {
            events.push(ev("weather", "", at(d, 7, 0), "用户"));
        }
        let lines = aggregate(&events, &[]);
        let joined = lines.join("\n");
        let a = lines.iter().find(|l| l.contains("《A》")).expect("《A》够格");
        assert!(a.contains("4 次 / 3 天") && a.contains("晚上 4") && a.contains("小明 4"), "{a}");
        assert!(
            !lines.iter().any(|l| l.starts_with("- media_play 《B》") || l.starts_with("- media_play 《C》")),
            "不够次数 / 不够天数的不成候选行(只许在合计行的「例如」里露面):{joined}"
        );
        let total = lines.iter().find(|l| l.starts_with("- media_play 合计")).expect("键不止一种 → 合计行");
        assert!(total.contains("9 次") && total.contains("例如:《A》×4"), "{total}");
        assert!(lines.iter().any(|l| l.starts_with("- weather:3 次 / 3 天")), "{joined}");
        assert!(!joined.contains("weather 合计"), "单一键的工具不重复给合计");
        assert!(!joined.contains("家里人跟助手说话"), "没发言数据不给时段分布");
        // 发言够多就给分布
        let msgs: Vec<(i64, String)> = (0..ACTIVITY_MIN_MSGS as i64).map(|i| (at(1 + i % 3, 21, 0), "用户".into())).collect();
        let lines = aggregate(&events, &msgs);
        assert!(lines.last().unwrap().contains("家里人跟助手说话:20 条 / 3 天"), "{:?}", lines.last());
    }

    #[test]
    fn build_request_lists_prior_rejected_and_evidence() {
        let req = build_request(
            &["- media_play 《A》:4 次 / 3 天".into()],
            Some("晚上常放《A》；周末看动画"),
            &["周六全家大扫除".into()],
            &["早上听新闻".into()],
        );
        let body = match &req.messages[0] {
            ChatMessage::User { content, .. } => content.clone(),
            other => panic!("{other:?}"),
        };
        assert!(body.contains("【已记着的惯例】\n- 晚上常放《A》\n- 周末看动画\n"), "{body}");
        assert!(body.contains("原样保留") && body.contains("- 周六全家大扫除\n"), "{body}");
        assert!(body.contains("【用户删掉的(别再写)】\n- 早上听新闻\n"), "{body}");
        assert!(body.contains("【最近 14 天的使用痕迹】\n- media_play 《A》"), "{body}");
        assert!(req.system.contains("只输出 JSON"));
        let empty = build_request(&[], None, &[], &[]);
        let body = match &empty.messages[0] {
            ChatMessage::User { content, .. } => content.clone(),
            _ => unreachable!(),
        };
        assert!(body.contains("(暂无)") && body.contains("(无)"));
    }

    #[test]
    fn parse_and_finalize_filter_rejected_long_unknown_and_budget() {
        let txt = "好的:```json\n[{\"text\":\"晚上常放《A》。\",\"who\":\"\"},{\"text\":\"小明周末看动画\",\"who\":\"小明\"},\
                   {\"text\":\"早上听新闻\",\"who\":\"\"},{\"text\":\"\",\"who\":\"\"},{\"text\":\"晚上常放《A》\",\"who\":\"\"},\
                   {\"text\":\"路人甲爱打游戏\",\"who\":\"路人甲\"}]\n```";
        let items = parse(txt);
        assert_eq!(items.len(), 5, "空 text 丢;句号归一:{items:?}");
        let known: HashSet<String> = ["小明".to_string(), "用户".to_string()].into_iter().collect();
        let (home, personal) = finalize(items, &["早上听新闻。".into()], &known);
        assert_eq!(home, vec!["晚上常放《A》".to_string(), "路人甲爱打游戏".to_string()], "否决的滤掉、重复的去重、认不出的人并进家级");
        assert_eq!(personal, vec![("小明".to_string(), "小明周末看动画".to_string())]);
        // 超长整句丢
        let long = "长".repeat(LINE_MAX_CHARS + 1);
        let (home, _) = finalize(vec![(long, String::new()), ("短".into(), String::new())], &[], &known);
        assert_eq!(home, vec!["短".to_string()]);
        // 总预算按行丢尾、不切半句(每行 50 字:5 行 + 4 个分隔符 = 254,第 6 行就破 300)
        let line = "七".repeat(49);
        let items: Vec<(String, String)> = (0..7).map(|i| (format!("{line}{i}"), String::new())).collect();
        let (home, _) = finalize(items, &[], &known);
        assert_eq!(home.len(), 5, "50×5 + 4 ≤ 300、再加一行就超,只留五行");
        assert!(home.iter().all(|l| l.chars().count() == 50), "整行保留、绝不切半句");
        assert!(parse("没啥可记的").is_empty());
    }

    #[test]
    fn diff_marks_user_removed_lines_as_rejected() {
        let last = vec!["晚上常放《A》".to_string(), "周末看动画".to_string()];
        assert_eq!(diff_removed(&last, Some("晚上常放《A》。")), vec!["周末看动画".to_string()], "少掉的那行 = 用户删了");
        assert!(diff_removed(&last, Some("周末看动画；晚上常放《A》")).is_empty(), "顺序无关");
        assert_eq!(diff_removed(&last, None).len(), 2, "整条没了 = 全部删掉");
        assert!(diff_removed(&[], None).is_empty());
    }

    #[tokio::test]
    async fn run_without_qualified_evidence_skips_llm_and_advances_watermark() {
        let tag = "noevidence";
        let s = store(tag);
        let me = s.users.ensure_default_user().unwrap();
        let conv = s.chat.create_conversation(me.id, "companion").unwrap();
        // 只放过一次 → 不够格;且这条要落在空闲窗之外
        seed_play(&s, tag, conv.id, "单次", at(1, 20, 0));
        let now = crate::store::now_ms();
        assert_eq!(run(&tattletale(), &s, now).await.unwrap(), 0);
        assert!(habits_row(&s).is_none(), "LLM 没被调(表里没有告密句)");
        assert_eq!(s.settings.get(None, WATERMARK_KEY).unwrap().unwrap(), now.to_string(), "翻篇");
        // 刚跑过、没到期 → 不再跑(哪怕现在有够格的证据)
        for d in [1, 2, 3] {
            seed_play(&s, tag, conv.id, "够格", at(d, 20, 0));
        }
        assert_eq!(run(&tattletale(), &s, now + 60_000).await.unwrap(), 0);
        assert!(habits_row(&s).is_none(), "未到期不跑");
        // 到期了就跑
        assert!(run(&tattletale(), &s, now + PERIOD_MS).await.unwrap() > 0);
        assert!(habits_row(&s).is_some());
    }

    #[tokio::test]
    async fn run_lets_a_busy_house_finish_talking_first() {
        let tag = "busy";
        let s = store(tag);
        let me = s.users.ensure_default_user().unwrap();
        let conv = s.chat.create_conversation(me.id, "companion").unwrap();
        for d in [1, 2, 3] {
            seed_play(&s, tag, conv.id, "够格", at(d, 20, 0));
        }
        s.chat.append_message(conv.id, "user", "刚说了一句").unwrap(); // 此刻 → 正热闹
        assert_eq!(run(&tattletale(), &s, crate::store::now_ms()).await.unwrap(), 0);
        assert!(habits_row(&s).is_none());
        assert!(s.settings.get(None, WATERMARK_KEY).unwrap().is_none(), "没跑 = 水位线不动");
    }

    #[tokio::test]
    async fn run_learns_home_habits_and_personal_memory_from_tool_rows() {
        let tag = "learn";
        let s = store(tag);
        let me = s.users.ensure_default_user().unwrap();
        let kid = s.users.create("小明").unwrap();
        let conv = s.chat.create_conversation(me.id, "companion").unwrap();
        // 小明(声纹归人)连着三晚看《汪汪队》
        for d in [1, 2, 3] {
            let u = s
                .chat
                .append_message_full(conv.id, "user", "放汪汪队", Some(&format!(r#"{{"speaker_user":{}}}"#, kid.id)))
                .unwrap();
            backdate(tag, u.id, at(d, 20, 0) - 5000);
            seed_play(&s, tag, conv.id, &format!("汪汪队立大功 第{d}集"), at(d, 20, 0));
        }
        let json = "[{\"text\":\"晚上八点后常给小明放《汪汪队立大功》\",\"who\":\"\"},\
                    {\"text\":\"小明晚上爱看动画\",\"who\":\"小明\"},\
                    {\"text\":\"用户早上听新闻\",\"who\":\"用户\"}]";
        let p: Arc<dyn LlmProvider> =
            Arc::new(FakeLlm::scripted(vec![FakeTurn { text: json.into(), ..Default::default() }]));
        let now = crate::store::now_ms();
        let n = run(&p, &s, now).await.unwrap();
        assert_eq!(n, 3, "家级 1 句 + 两个人各 1 句");
        let row = habits_row(&s).expect("惯例落进家庭备忘");
        assert_eq!(row.content, "晚上八点后常给小明放《汪汪队立大功》");
        assert_eq!(row.scope, "home");
        assert!(row.resident, "预算内常驻进前缀");
        let kid_mem = s.memory.list(kid.id).unwrap();
        assert_eq!(kid_mem.len(), 1);
        assert_eq!(kid_mem[0].kind, KIND_EXPERIENCE);
        assert_eq!(kid_mem[0].source, "distilled");
        assert!(!kid_mem[0].resident, "个人习惯进按需层,不污染前缀");
        assert_eq!(s.memory.list(me.id).unwrap()[0].content, "用户早上听新闻", "「用户」= 主人");
        assert_eq!(s.settings.get(None, LAST_WRITTEN_KEY).unwrap().unwrap(), "[\"晚上八点后常给小明放《汪汪队立大功》\"]");
        assert_eq!(s.settings.get(None, WATERMARK_KEY).unwrap().unwrap(), now.to_string());
        // 证据行本身:标题归到一部剧、归到小明名下
        let g = gather(&s, now - LOOKBACK_DAYS * 86_400_000, now).unwrap();
        assert_eq!(g.events.len(), 3);
        assert!(g.events.iter().all(|e| e.key == "《汪汪队立大功》" && e.who == "小明"), "{:?}", g.events);
        assert_eq!(g.user_msgs.len(), 3);
    }

    /// 用户删掉的不再写回:上次写过两句、用户删了一句 → 这句进否决名单,模型再写也被滤掉。
    /// 自启回合(提醒到点 / 后台汇报)里调的工具不算谁的使用痕迹(2026-11-11):每晚定时放故事的
    /// 提醒连着触发三天,取料里一条播放事件都没有;人自己开口点的照算。
    #[test]
    fn gather_skips_tool_rows_of_self_started_turns() {
        let tag = "selfturn";
        let s = store(tag);
        let me = s.users.ensure_default_user().unwrap();
        let conv = s.chat.create_conversation(me.id, "companion").unwrap();
        for d in [1, 2, 3] {
            let ev = s.chat.append_message(conv.id, "event", "到点放睡前故事").unwrap();
            backdate(tag, ev.id, at(d, 20, 0) - 5000);
            seed_play(&s, tag, conv.id, "睡前故事", at(d, 20, 0));
        }
        let now = crate::store::now_ms();
        let g = gather(&s, now - LOOKBACK_DAYS * 86_400_000, now).unwrap();
        assert!(g.events.is_empty(), "定时任务自己放的不算使用痕迹:{:?}", g.events);
        // 人开口点的照算(user 行把「自启回合」这段收住)
        let u = s.chat.append_message(conv.id, "user", "放汪汪队").unwrap();
        backdate(tag, u.id, at(0, 9, 0) - 5000);
        seed_play(&s, tag, conv.id, "汪汪队立大功", at(0, 9, 0));
        let now = crate::store::now_ms();
        let g = gather(&s, now - LOOKBACK_DAYS * 86_400_000, now).unwrap();
        assert_eq!(g.events.len(), 1, "{:?}", g.events);
    }

    /// 家里人自己写进「惯例」的话不归整理器管(2026-11-11):重写时原样带上、排在前面,
    /// 不因为痕迹里没有就被删;整理器只记自己写的那几句为「上次写入」。
    #[tokio::test]
    async fn run_keeps_lines_the_family_wrote_themselves() {
        let tag = "foreign";
        let s = store(tag);
        let me = s.users.ensure_default_user().unwrap();
        let conv = s.chat.create_conversation(me.id, "companion").unwrap();
        for d in [1, 2, 3] {
            seed_play(&s, tag, conv.id, "汪汪队立大功", at(d, 20, 0));
        }
        // 现存内容:一句是上次整理器写的,一句是用户在聊天里亲口说、由聊天模型记进来的
        s.briefings
            .upsert(&crate::store::briefings::scope_home(), HABITS_DOMAIN, "周末看动画；周六上午全家大扫除", true)
            .unwrap();
        write_list(&s, LAST_WRITTEN_KEY, &["周末看动画".into()]).unwrap();
        s.settings.set(None, WATERMARK_KEY, &(crate::store::now_ms() - PERIOD_MS - 1).to_string()).unwrap();
        // 模型这次只写了一句新的(「周末看动画」痕迹里没了,它按法条删掉;大扫除它根本没提)
        let json = "[{\"text\":\"晚上常放《汪汪队立大功》\",\"who\":\"\"}]";
        let p: Arc<dyn LlmProvider> =
            Arc::new(FakeLlm::scripted(vec![FakeTurn { text: json.into(), ..Default::default() }]));
        run(&p, &s, crate::store::now_ms()).await.unwrap();
        assert_eq!(
            habits_row(&s).unwrap().content,
            "周六上午全家大扫除；晚上常放《汪汪队立大功》",
            "用户自己写的那句原样留着"
        );
        assert_eq!(
            read_list(&s, LAST_WRITTEN_KEY).unwrap(),
            vec!["晚上常放《汪汪队立大功》".to_string()],
            "「上次写入」只记整理器自己写的,用户那句永远不算它的"
        );
        assert!(read_list(&s, REJECTED_KEY).unwrap().is_empty(), "没有人删过东西,否决名单不动");
    }

    /// 用户在回忆页删掉惯例提炼写的个人习惯 → 进否决名单;删别的记忆不受影响(2026-11-11)。
    #[test]
    fn deleting_a_habit_memory_bans_it_but_other_memories_do_not() {
        let s = store("pdel");
        let me = s.users.ensure_default_user().unwrap();
        write_list(&s, PERSONAL_WRITTEN_KEY, &[format!("{}{PERSONAL_SEP}周末下午常看动画片", me.id)]).unwrap();
        note_memory_deleted(&s, me.id, "用户不吃香菜").unwrap();
        assert!(read_list(&s, REJECTED_KEY).unwrap().is_empty(), "不是惯例提炼写的,不进否决名单");
        note_memory_deleted(&s, me.id, "周末下午常看动画片。").unwrap();
        assert_eq!(read_list(&s, REJECTED_KEY).unwrap(), vec!["周末下午常看动画片".to_string()]);
        assert!(read_list(&s, PERSONAL_WRITTEN_KEY).unwrap().is_empty(), "认领过的条目摘掉");
    }

    #[tokio::test]
    async fn run_never_rewrites_what_the_user_deleted() {
        let tag = "deleted";
        let s = store(tag);
        let me = s.users.ensure_default_user().unwrap();
        let conv = s.chat.create_conversation(me.id, "companion").unwrap();
        for d in [1, 2, 3] {
            seed_play(&s, tag, conv.id, "汪汪队立大功", at(d, 20, 0));
        }
        // 上次写了两句;用户在回忆页把整条删了(行不存在)
        write_list(&s, LAST_WRITTEN_KEY, &["晚上常放《汪汪队立大功》".into(), "周末看动画".into()]).unwrap();
        s.settings.set(None, WATERMARK_KEY, &(crate::store::now_ms() - PERIOD_MS - 1).to_string()).unwrap();
        let json = "[{\"text\":\"晚上常放《汪汪队立大功》\",\"who\":\"\"},{\"text\":\"周末看动画。\",\"who\":\"\"},\
                    {\"text\":\"工作日早上放新闻\",\"who\":\"\"}]";
        let p: Arc<dyn LlmProvider> =
            Arc::new(FakeLlm::scripted(vec![FakeTurn { text: json.into(), ..Default::default() }]));
        assert_eq!(run(&p, &s, crate::store::now_ms()).await.unwrap(), 1, "只有没被否决的那句落地");
        assert_eq!(habits_row(&s).unwrap().content, "工作日早上放新闻");
        let rejected = read_list(&s, REJECTED_KEY).unwrap();
        assert_eq!(rejected, vec!["晚上常放《汪汪队立大功》".to_string(), "周末看动画".to_string()]);
    }
}
