//! 家庭日记「这些日子」蒸馏(2026-07-09 用户拍板):后台把「上次写到这次之间」这个家发生的事
//! 写成按日一两句的日记。**桌面程序没有「每天定点活着」的假设**(同 jobs「错过不补发」的判断)——
//! 所以不是定时任务,而是**水位线**:settings `diary.covered_until` 记「写到哪天」,开机后的
//! scheduler 节拍里发现跨过了没写的日历日,就一次蒸馏补齐整个区间、按日出条,水位线推进到昨天
//! (今天还没过完,留给下次)。关机一周回来也只花一次便宜档调用;更久的只补最近 `LOOKBACK_MAX_DAYS` 天。
//!
//! 纪律同 consolidate:宁缺毋滥(没事的日子不出条,空是常态)、人格中立(平实记事,不煽情不编造)、
//! cheap-model(调用方传 `background_provider`)、尽力件(失败不推水位线、下次再试)。
//! 形状也同 consolidate:纯函数(`build_request` / `parse` / 区间推导)+ 编排(`run`,FakeLlm 可测)。

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use chrono::{Duration, Local, NaiveDate, TimeZone};

use crate::llm::{ChatMessage, ChatOptions, ChatRequest, LlmProvider, ToolChoice};
use crate::store::Store;

/// 关机太久只补最近 N 个日历日(更早的日子放过去,别喂爆 prompt、也别翻旧账)。
pub(crate) const LOOKBACK_MAX_DAYS: i64 = 7;
/// 每天最多喂多少条对话 / 每条截多少字(防 prompt 爆炸;日记要的是「那天的样子」不是全文)。
const MSGS_PER_DAY: usize = 60;
const CHARS_PER_MSG: usize = 120;
/// 单日日记上限(同 remember 的防撑爆口径)。
const CONTENT_MAX_CHARS: usize = 200;
/// 水位线:已写到(含)哪个本地日,'YYYY-MM-DD'(app 级 settings;小状态不开新域 §6.2)。
pub(crate) const WATERMARK_KEY: &str = "diary.covered_until";

/// 日记器法条(人格中立、不与人对话,§5):平实记事,宁缺毋滥。
const SYSTEM: &str = "你是家用助手的后台日记器,不与任何人对话,只输出 JSON。\
  以助手第一人称「我」给这个家记日记:每个给出的日子至多两句话,平实自然,只写材料里真出现的事,\
  不评价、不煽情、不编造材料里没有的细节,不写成流水账清单。\
  哪一天没什么值得记,就跳过那一天 —— 跳过是常态,绝不为了凑数硬写。\
  输出一个 JSON 数组,每项形如 {\"date\":\"YYYY-MM-DD\",\"content\":\"…\"};没有可记的就输出 []。";

/// 一天的原料(行已拼好:「谁: 说了啥」「看了《X》」「办完了:X」)。
#[derive(Debug)]
pub(crate) struct DayMaterial {
    pub date: String,
    pub lines: Vec<String>,
}

/// ms → 本地日历日。
fn local_date(ms: i64) -> Option<NaiveDate> {
    Local.timestamp_millis_opt(ms).single().map(|t| t.date_naive())
}

/// 本地日 00:00 → ms(DST 歧义取 earliest 兜底;国内无 DST,纯防御)。
fn day_start_ms(d: NaiveDate) -> Option<i64> {
    Local.from_local_datetime(&d.and_hms_opt(0, 0, 0)?).earliest().map(|t| t.timestamp_millis())
}

/// 收集 `[from, to]`(含界,本地日)区间的原料,按日分桶;没动静的日子不出现。
fn collect_materials(store: &Store, from: NaiveDate, to: NaiveDate) -> Result<Vec<DayMaterial>> {
    let (Some(from_ms), Some(end_ms)) = (day_start_ms(from), day_start_ms(to + Duration::days(1)))
    else {
        return Ok(Vec::new());
    };
    // 说话人显性化数据复用:user 行 payload 里的 speaker_user → 显示名(家人删了就回落「家里人」)。
    let names: HashMap<i64, String> =
        store.users.list()?.into_iter().map(|u| (u.id, u.name)).collect();
    let mut days: HashMap<String, Vec<String>> = HashMap::new();
    let mut day_counts: HashMap<String, usize> = HashMap::new();

    for m in store.chat.messages_between(from_ms, end_ms, 2_000)? {
        let Some(date) = local_date(m.created_at).map(|d| d.to_string()) else { continue };
        let cnt = day_counts.entry(date.clone()).or_insert(0);
        if *cnt >= MSGS_PER_DAY {
            continue; // 那天话太多:取前一截足够写日记,别喂爆
        }
        let who = match m.role.as_str() {
            "assistant" => "我".to_string(),
            _ => m
                .payload
                .as_deref()
                .and_then(|p| serde_json::from_str::<super::UserMeta>(p).ok())
                .and_then(|meta| meta.speaker_user)
                .map(|id| names.get(&id).cloned().unwrap_or_else(|| "家里人".into()))
                .unwrap_or_else(|| "用户".into()),
        };
        let text: String = m.content.chars().take(CHARS_PER_MSG).collect();
        days.entry(date).or_default().push(format!("{who}: {text}"));
        *cnt += 1;
    }
    // 看/听过的(全家的续播动静都算这个家的日子)
    for (uid, _) in &names {
        for p in store.media_progress.list_recent(*uid, 20)? {
            if p.updated_at < from_ms || p.updated_at >= end_ms {
                continue;
            }
            if let Some(date) = local_date(p.updated_at).map(|d| d.to_string()) {
                let line = format!("(看/听了《{}》)", p.title);
                let bucket = days.entry(date).or_default();
                if !bucket.contains(&line) {
                    bucket.push(line);
                }
            }
        }
    }
    // 待办动静(记下想办 / 办完了)
    for (content, done, created_at, updated_at) in store.todos.changed_between(from_ms, end_ms)? {
        if created_at >= from_ms && created_at < end_ms {
            if let Some(date) = local_date(created_at).map(|d| d.to_string()) {
                days.entry(date).or_default().push(format!("(记下想办:{content})"));
            }
        }
        if done && updated_at >= from_ms && updated_at < end_ms && updated_at != created_at {
            if let Some(date) = local_date(updated_at).map(|d| d.to_string()) {
                days.entry(date).or_default().push(format!("(办完了:{content})"));
            }
        }
    }

    let mut out: Vec<DayMaterial> =
        days.into_iter().map(|(date, lines)| DayMaterial { date, lines }).collect();
    out.sort_by(|a, b| a.date.cmp(&b.date));
    Ok(out)
}

/// 构造蒸馏请求(纯函数):system=日记器法条,body=按日分好的原料。
pub(crate) fn build_request(days: &[DayMaterial]) -> ChatRequest {
    let mut body = String::from("【这些日子的材料】\n");
    for d in days {
        body.push_str(&format!("\n== {} ==\n", d.date));
        for l in &d.lines {
            body.push_str(l);
            body.push('\n');
        }
    }
    ChatRequest {
        system: SYSTEM.into(),
        messages: vec![ChatMessage::User { content: body, parts: vec![] }],
        options: ChatOptions::default(),
        tools: vec![],
        tool_choice: ToolChoice::default(),
    }
}

/// 解析日记输出:容忍 ```json 围栏 / 前后废话;date 必须是合法 'YYYY-MM-DD' 且落在
/// `[from, to]` 区间内(模型幻造的日期一律丢);content 裁长度。解析失败 = 空(尽力件)。
pub(crate) fn parse(text: &str, from: NaiveDate, to: NaiveDate) -> Vec<(String, String)> {
    let slice = match (text.find('['), text.rfind(']')) {
        (Some(a), Some(b)) if b > a => &text[a..=b],
        _ => return Vec::new(),
    };
    let raw: Vec<serde_json::Value> = serde_json::from_str(slice).unwrap_or_default();
    raw.into_iter()
        .filter_map(|v| {
            let date_s = v.get("date").and_then(serde_json::Value::as_str)?.trim().to_string();
            let d = NaiveDate::parse_from_str(&date_s, "%Y-%m-%d").ok()?;
            if d < from || d > to {
                return None; // 幻造/越界日期不收
            }
            let content: String = v
                .get("content")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())?
                .chars()
                .take(CONTENT_MAX_CHARS)
                .collect();
            Some((d.to_string(), content))
        })
        .collect()
}

/// 跑一次日记补写(调用方 = `Engine::spawn_diary`,已做限流/防重入/FakeLlm 挡板):
/// 水位线之后、昨天为止的日子里有动静的,蒸馏落库;全程无动静则只推水位线(不花钱)。
/// 失败不推水位线(下次再试)。返回新写的天数。
pub(crate) async fn run(
    provider: &Arc<dyn LlmProvider>,
    store: &Store,
    now_ms: i64,
) -> Result<usize> {
    let Some(today) = local_date(now_ms) else { return Ok(0) };
    let yesterday = today - Duration::days(1);
    let set_wm = |d: NaiveDate| store.settings.set(None, WATERMARK_KEY, &d.to_string());

    let wm = store
        .settings
        .get(None, WATERMARK_KEY)?
        .and_then(|s| NaiveDate::parse_from_str(&s, "%Y-%m-%d").ok());
    let Some(wm) = wm else {
        // 首启(或水位线损坏):锚定到昨天,今天起算 —— 不回溯补写历史(启用前不欠日记)。
        set_wm(yesterday)?;
        return Ok(0);
    };
    if wm >= yesterday {
        return Ok(0); // 没跨日,无事
    }
    let from = std::cmp::max(wm + Duration::days(1), yesterday - Duration::days(LOOKBACK_MAX_DAYS - 1));
    let materials = collect_materials(store, from, yesterday)?;
    if materials.is_empty() {
        set_wm(yesterday)?; // 没开机/没动静的日子:直接翻篇,不烧钱
        return Ok(0);
    }
    let mut req = build_request(&materials);
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
    let text =
        provider.chat(req).await.map_err(|e| anyhow::anyhow!("日记蒸馏 LLM 调用失败: {e:?}"))?;
    let mut written = 0usize;
    for (date, content) in parse(&text, from, yesterday) {
        if store.diary.upsert(&date, &content)? {
            written += 1;
        }
    }
    set_wm(yesterday)?;
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::fake::{FakeLlm, FakeTurn};
    use crate::store::Store;

    fn store(tag: &str) -> Store {
        let p = std::env::temp_dir().join(format!("lw-diary-run-{}-{tag}.db", std::process::id()));
        let _ = std::fs::remove_file(&p);
        Store::open(&p).unwrap()
    }

    /// 剧本:若 LLM 真被调,这条会落库 → 「表空」即反证没调(FakeLlm 空队列是回声、不报错)。
    fn tattletale(date: &str) -> Arc<dyn LlmProvider> {
        let json = format!("[{{\"date\":\"{date}\",\"content\":\"要是我被调用了这条就会出现\"}}]");
        Arc::new(FakeLlm::scripted(vec![FakeTurn { text: json, ..Default::default() }]))
    }

    fn today_ms() -> i64 {
        crate::store::now_ms()
    }

    #[tokio::test]
    async fn first_run_anchors_watermark_without_llm() {
        let s = store("anchor");
        s.users.ensure_default_user().unwrap();
        let today = local_date(today_ms()).unwrap();
        let p = tattletale(&today.to_string());
        assert_eq!(run(&p, &s, today_ms()).await.unwrap(), 0);
        // 水位线锚到昨天、没蒸馏(表空 = LLM 没被调)
        let wm = s.settings.get(None, WATERMARK_KEY).unwrap().unwrap();
        assert_eq!(wm, (today - Duration::days(1)).to_string());
        assert!(s.diary.list(10).unwrap().is_empty());
    }

    #[tokio::test]
    async fn crossing_a_day_distills_and_advances() {
        let s = store("cross");
        let user = s.users.ensure_default_user().unwrap();
        let conv = s.chat.create_conversation(user.id, "companion").unwrap();
        // 消息落在「真实今天」;把 run 的 now 拨到明天 → 今天成了待补的「昨天」
        s.chat.append_message(conv.id, "user", "给我放一集汪汪队").unwrap();
        s.chat.append_message(conv.id, "assistant", "放上了,第三集。").unwrap();
        let today = local_date(today_ms()).unwrap();
        s.settings.set(None, WATERMARK_KEY, &(today - Duration::days(1)).to_string()).unwrap();

        let tomorrow_ms = today_ms() + 86_400_000;
        let json = format!("[{{\"date\":\"{today}\",\"content\":\"陪着看了会儿汪汪队。\"}}]");
        let p: Arc<dyn LlmProvider> =
            Arc::new(FakeLlm::scripted(vec![FakeTurn { text: json, ..Default::default() }]));
        assert_eq!(run(&p, &s, tomorrow_ms).await.unwrap(), 1);

        let all = s.diary.list(10).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].date, today.to_string());
        assert!(all[0].content.contains("汪汪队"));
        // 水位线推进到「run 视角的昨天」= 真实今天
        assert_eq!(s.settings.get(None, WATERMARK_KEY).unwrap().unwrap(), today.to_string());
    }

    #[tokio::test]
    async fn quiet_gap_advances_watermark_without_llm() {
        let s = store("quiet");
        s.users.ensure_default_user().unwrap();
        let today = local_date(today_ms()).unwrap();
        // 三天没开机、没有任何动静:水位线直接翻篇到昨天,LLM 不被调(表空反证)
        s.settings.set(None, WATERMARK_KEY, &(today - Duration::days(3)).to_string()).unwrap();
        let p = tattletale(&(today - Duration::days(1)).to_string());
        assert_eq!(run(&p, &s, today_ms()).await.unwrap(), 0);
        assert!(s.diary.list(10).unwrap().is_empty());
        assert_eq!(
            s.settings.get(None, WATERMARK_KEY).unwrap().unwrap(),
            (today - Duration::days(1)).to_string()
        );
    }

    #[tokio::test]
    async fn same_day_reruns_are_noops() {
        let s = store("noop");
        s.users.ensure_default_user().unwrap();
        let today = local_date(today_ms()).unwrap();
        s.settings.set(None, WATERMARK_KEY, &(today - Duration::days(1)).to_string()).unwrap();
        let p = tattletale(&today.to_string());
        // 水位线已到昨天 → 今天再跑什么都不做(哪怕库里有今天的消息)
        assert_eq!(run(&p, &s, today_ms()).await.unwrap(), 0);
        assert!(s.diary.list(10).unwrap().is_empty());
    }

    #[test]
    fn parse_drops_hallucinated_dates_and_clamps() {
        let from = NaiveDate::from_ymd_opt(2026, 7, 1).unwrap();
        let to = NaiveDate::from_ymd_opt(2026, 7, 7).unwrap();
        let txt = "好的```json\n[\
            {\"date\":\"2026-07-03\",\"content\":\"在区间内。\"},\
            {\"date\":\"2026-07-20\",\"content\":\"幻造的未来\"},\
            {\"date\":\"胡说\",\"content\":\"坏日期\"},\
            {\"date\":\"2026-07-04\",\"content\":\"  \"}]\n```";
        let items = parse(txt, from, to);
        assert_eq!(items.len(), 1, "越界/坏日期/空内容全丢");
        assert_eq!(items[0].0, "2026-07-03");
        assert!(parse("没啥可写的", from, to).is_empty(), "非 JSON → 空不抛错");
    }

    #[test]
    fn materials_bucket_by_day_and_label_speakers() {
        let s = store("mat");
        let user = s.users.ensure_default_user().unwrap();
        let conv = s.chat.create_conversation(user.id, "companion").unwrap();
        s.chat.append_message(conv.id, "user", "放首儿歌").unwrap();
        s.chat.append_message(conv.id, "assistant", "放上了。").unwrap();
        s.chat.append_message(conv.id, "tool", "内部行不进日记").unwrap();
        s.todos.add(user.id, "给车做年检").unwrap();
        let today = local_date(today_ms()).unwrap();
        let days = collect_materials(&s, today, today).unwrap();
        assert_eq!(days.len(), 1);
        let lines = &days[0].lines;
        assert!(lines.iter().any(|l| l.starts_with("用户: 放首儿歌")));
        assert!(lines.iter().any(|l| l.starts_with("我: ")));
        assert!(lines.iter().any(|l| l.contains("记下想办:给车做年检")));
        assert!(!lines.iter().any(|l| l.contains("内部行")), "tool 行不进料");
    }
}
