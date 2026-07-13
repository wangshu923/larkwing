//! 家庭日记「这些日子」蒸馏(2026-07-09 用户拍板;2026-07-13 触发改版「攒够 + 空闲」):
//! 后台把「上次写到这次之间」这个家发生的事写成按日一两句的日记。**桌面程序没有「每天定点
//! 活着」的假设**(同 jobs「错过不补发」的判断)—— 所以不是定时任务,而是**水位线**:
//! settings `diary.covered_until_ms` 记「写到哪个时刻」。
//!
//! **触发 = 攒够 + 空闲**(2026-07-13 用户拍板,取代「跨日历日才补」—— 今天的事不用等明天):
//! 自水位线起过了 `ACCUM_MS`(且有动静),或家里人的话攒到 `MSG_THRESHOLD` 条(热闹的日子
//! 提前写、当天就能看到),就趁最近 `IDLE_WINDOW_MS` 全局没新消息的空闲窗动笔;区间上界取
//! 「安静边界」(now − 空闲窗),判完空闲哪怕立刻有人开口,新话也落在下一段,不丢不重。
//! 产出仍按日历日出条(一天一条的日记心智);同一天被第二次写到(上午写过、晚上又攒够)=
//! **融合重写**:把已写的喂回模型连同新料重写成这天完整的一条(`DiaryRepo::set`)。
//! 关机一周回来也只花一次便宜档调用;更久的只补最近 `LOOKBACK_MAX_DAYS` 天。
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
/// 水位线:已覆盖到(不含)此毫秒时刻 = 下一段区间的起点(app 级 settings;小状态不开新域 §6.2)。
pub(crate) const WATERMARK_MS_KEY: &str = "diary.covered_until_ms";
/// 旧日期水位线('YYYY-MM-DD',2026-07-13 触发改版前的机制):读到即换算迁移,不再写;
/// 留着不删(惰性垃圾无害,§7.6 autostart `.defaulted` 同款先例)。
const LEGACY_WATERMARK_KEY: &str = "diary.covered_until";
/// 攒够时长:自水位线过了这么久且有动静就写(≈ 一天一条的节奏;2026-07-13 用户拍板 24h)。
const ACCUM_MS: i64 = 24 * 3_600_000;
/// 攒够条数:家里人的话到量就提前写(热闹的日子当天可见,也赶在 `MSGS_PER_DAY` 截断前)。
const MSG_THRESHOLD: i64 = 50;
/// 空闲窗:全局最近这么久没有新消息才动笔(别把聊到一半的话题切成两段);
/// 区间上界也取 now − 它(安静边界)。
pub(crate) const IDLE_WINDOW_MS: i64 = 10 * 60_000;

/// 日记器法条(人格中立、不与人对话,§5):平实记事,宁缺毋滥;同日补写 = 融合重写。
const SYSTEM: &str = "你是家用助手的后台日记器,不与任何人对话,只输出 JSON。\
  以助手第一人称「我」给这个家记日记:每个给出的日子至多两句话,平实自然,只写材料里真出现的事,\
  不评价、不煽情、不编造材料里没有的细节,不写成流水账清单。\
  材料里标了「这天此前已记」的日子,那是这天早些时候已写下的日记:把它和这天的新材料融合,\
  重写成这天完整的一条(仍至多两句),已记的事别丢、同一件事别重复叙述。\
  哪一天没什么值得记,就跳过那一天 —— 跳过是常态,绝不为了凑数硬写。\
  输出一个 JSON 数组,每项形如 {\"date\":\"YYYY-MM-DD\",\"content\":\"…\"};没有可记的就输出 []。";

/// 一天的原料(行已拼好:「谁: 说了啥」「看了《X》」「办完了:X」)。
#[derive(Debug)]
pub(crate) struct DayMaterial {
    pub date: String,
    pub lines: Vec<String>,
    /// 这天早些时候已写下的日记(同日第二段触发时喂给模型融合重写;None = 这天还没写过)。
    pub prior: Option<String>,
}

/// ms → 本地日历日。
fn local_date(ms: i64) -> Option<NaiveDate> {
    Local.timestamp_millis_opt(ms).single().map(|t| t.date_naive())
}

/// 本地日 00:00 → ms(DST 歧义取 earliest 兜底;国内无 DST,纯防御)。
fn day_start_ms(d: NaiveDate) -> Option<i64> {
    Local.from_local_datetime(&d.and_hms_opt(0, 0, 0)?).earliest().map(|t| t.timestamp_millis())
}

/// 收集 `[from_ms, end_ms)` 时刻区间的原料,按本地日分桶;没动静的日子不出现。
/// 桶好后顺带查每个日子已有的日记(→ `prior`,同日第二段触发的融合重写用)。
fn collect_materials(store: &Store, from_ms: i64, end_ms: i64) -> Result<Vec<DayMaterial>> {
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
    for uid in names.keys() {
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

    let mut out = Vec::with_capacity(days.len());
    for (date, lines) in days {
        let prior = store.diary.get_by_date(&date)?;
        out.push(DayMaterial { date, lines, prior });
    }
    out.sort_by(|a, b| a.date.cmp(&b.date));
    Ok(out)
}

/// 构造蒸馏请求(纯函数):system=日记器法条,body=按日分好的原料。
pub(crate) fn build_request(days: &[DayMaterial]) -> ChatRequest {
    let mut body = String::from("【这些日子的材料】\n");
    for d in days {
        body.push_str(&format!("\n== {} ==\n", d.date));
        if let Some(p) = &d.prior {
            body.push_str(&format!("(这天此前已记:{p})\n"));
        }
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

/// 跑一次日记补写(调用方 = `Engine::spawn_diary`,已做限流/回合在飞让路/防重入/FakeLlm 挡板):
/// 攒够(过了 `ACCUM_MS` 或家里人的话 ≥ `MSG_THRESHOLD`)且空闲(最近 `IDLE_WINDOW_MS`
/// 全局无新消息)才动笔,写 `(水位线, now − 空闲窗]` 段;全程无动静则只推水位线(不花钱)。
/// 失败不推水位线(下次再试)。返回写的天数。
pub(crate) async fn run(
    provider: &Arc<dyn LlmProvider>,
    store: &Store,
    now_ms: i64,
) -> Result<usize> {
    let set_wm = |ms: i64| store.settings.set(None, WATERMARK_MS_KEY, &ms.to_string());

    let wm = match store
        .settings
        .get(None, WATERMARK_MS_KEY)?
        .and_then(|s| s.parse::<i64>().ok())
    {
        Some(ms) => ms,
        None => {
            // 旧日期水位线(跨日机制)→ 换算成「该日结束的时刻」接着用;真·首启(或水位线
            // 损坏)锚定 now —— 不回溯补写历史(启用前不欠日记)。
            let legacy = store
                .settings
                .get(None, LEGACY_WATERMARK_KEY)?
                .and_then(|s| NaiveDate::parse_from_str(&s, "%Y-%m-%d").ok())
                .and_then(|d| day_start_ms(d + Duration::days(1)));
            set_wm(legacy.unwrap_or(now_ms))?;
            match legacy {
                Some(ms) => ms, // 迁移完接着往下走(可能已攒够)
                None => return Ok(0),
            }
        }
    };
    // 空闲窗:最近 K 分钟有人说话 / 系统在动(含 tool/event 行)= 正热闹,这轮不动笔。
    if let Some(last) = store.chat.last_message_at()? {
        if now_ms.saturating_sub(last) < IDLE_WINDOW_MS {
            return Ok(0);
        }
    }
    let upper = now_ms - IDLE_WINDOW_MS; // 安静边界:判完空闲立刻有人开口,新话也落下一段
    if upper <= wm {
        return Ok(0);
    }
    // 攒够才写:过了 ACCUM_MS,或家里人的话到量(热闹的日子提前)。
    let user_msgs = store.chat.count_user_messages_between(wm, upper)?;
    if now_ms - wm < ACCUM_MS && user_msgs < MSG_THRESHOLD {
        return Ok(0);
    }
    let from_ms = wm.max(upper - LOOKBACK_MAX_DAYS * 86_400_000);
    let materials = collect_materials(store, from_ms, upper)?;
    if materials.is_empty() {
        set_wm(upper)?; // 没开机/没动静的日子:直接翻篇,不烧钱
        return Ok(0);
    }
    let (Some(from_date), Some(to_date)) = (local_date(from_ms), local_date(upper)) else {
        return Ok(0);
    };
    // 喂过旧文的日子才允许融合重写换新;其余日子维持 IGNORE 安全默认(绝不覆盖没喂过的)。
    let prior_dates: std::collections::HashSet<String> =
        materials.iter().filter(|d| d.prior.is_some()).map(|d| d.date.clone()).collect();
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
    for (date, content) in parse(&text, from_date, to_date) {
        let wrote = if prior_dates.contains(&date) {
            store.diary.set(&date, &content)?
        } else {
            store.diary.upsert(&date, &content)?
        };
        if wrote {
            written += 1;
        }
    }
    set_wm(upper)?;
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
        let now = today_ms();
        let p = tattletale(&local_date(now).unwrap().to_string());
        assert_eq!(run(&p, &s, now).await.unwrap(), 0);
        // 水位线锚到 now、没蒸馏(表空 = LLM 没被调)
        let wm = s.settings.get(None, WATERMARK_MS_KEY).unwrap().unwrap();
        assert_eq!(wm, now.to_string());
        assert!(s.diary.list(10).unwrap().is_empty());
    }

    #[tokio::test]
    async fn legacy_date_watermark_migrates_to_ms() {
        let s = store("legacy");
        s.users.ensure_default_user().unwrap();
        let now = today_ms();
        let today = local_date(now).unwrap();
        // 旧机制留下的日期水位线(已写到前天)→ 换算「昨天 00:00」接着用;
        // 无任何动静:攒够(>24h)但材料空 → 直接翻篇到安静边界,LLM 不被调(表空反证)。
        s.settings
            .set(None, LEGACY_WATERMARK_KEY, &(today - Duration::days(2)).to_string())
            .unwrap();
        let p = tattletale(&today.to_string());
        assert_eq!(run(&p, &s, now).await.unwrap(), 0);
        let wm: i64 =
            s.settings.get(None, WATERMARK_MS_KEY).unwrap().unwrap().parse().unwrap();
        assert_eq!(wm, now - IDLE_WINDOW_MS);
        assert!(s.diary.list(10).unwrap().is_empty());
    }

    #[tokio::test]
    async fn accumulated_time_writes_in_idle_window() {
        let s = store("accum");
        let user = s.users.ensure_default_user().unwrap();
        let conv = s.chat.create_conversation(user.id, "companion").unwrap();
        // 消息落在「真实此刻」;水位线拨到 25h 前(时长攒够),run 拨到 11 分钟后(空闲窗成立)
        s.chat.append_message(conv.id, "user", "给我放一集汪汪队").unwrap();
        s.chat.append_message(conv.id, "assistant", "放上了,第三集。").unwrap();
        let now = today_ms();
        let today = local_date(now).unwrap();
        s.settings.set(None, WATERMARK_MS_KEY, &(now - 25 * 3_600_000).to_string()).unwrap();

        let later = now + IDLE_WINDOW_MS + 60_000;
        let json = format!("[{{\"date\":\"{today}\",\"content\":\"陪着看了会儿汪汪队。\"}}]");
        let p: Arc<dyn LlmProvider> =
            Arc::new(FakeLlm::scripted(vec![FakeTurn { text: json, ..Default::default() }]));
        assert_eq!(run(&p, &s, later).await.unwrap(), 1);

        let all = s.diary.list(10).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].date, today.to_string());
        assert!(all[0].content.contains("汪汪队"));
        // 水位线推进到安静边界(later − 空闲窗)
        let wm: i64 =
            s.settings.get(None, WATERMARK_MS_KEY).unwrap().unwrap().parse().unwrap();
        assert_eq!(wm, later - IDLE_WINDOW_MS);
    }

    #[tokio::test]
    async fn message_threshold_triggers_before_time() {
        let s = store("thresh");
        let user = s.users.ensure_default_user().unwrap();
        let conv = s.chat.create_conversation(user.id, "companion").unwrap();
        // 时长没到(水位线才 2h 前)但家里人的话到量 → 提前写(热闹的日子当天可见)
        for i in 0..MSG_THRESHOLD {
            s.chat.append_message(conv.id, "user", &format!("第{i}句")).unwrap();
        }
        let now = today_ms();
        let today = local_date(now).unwrap();
        s.settings.set(None, WATERMARK_MS_KEY, &(now - 2 * 3_600_000).to_string()).unwrap();
        let json = format!("[{{\"date\":\"{today}\",\"content\":\"家里聊了不少。\"}}]");
        let p: Arc<dyn LlmProvider> =
            Arc::new(FakeLlm::scripted(vec![FakeTurn { text: json, ..Default::default() }]));
        assert_eq!(run(&p, &s, now + IDLE_WINDOW_MS + 60_000).await.unwrap(), 1);
        assert_eq!(s.diary.list(10).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn busy_or_not_accumulated_are_noops() {
        let s = store("noop");
        let user = s.users.ensure_default_user().unwrap();
        let conv = s.chat.create_conversation(user.id, "companion").unwrap();
        s.chat.append_message(conv.id, "user", "随便说一句").unwrap();
        let now = today_ms();
        let p = tattletale(&local_date(now).unwrap().to_string());
        // ① 刚说完话(空闲窗没到)→ 哪怕时长攒够也不动笔、不推水位线
        let old = (now - 25 * 3_600_000).to_string();
        s.settings.set(None, WATERMARK_MS_KEY, &old).unwrap();
        assert_eq!(run(&p, &s, now + 60_000).await.unwrap(), 0);
        assert_eq!(
            s.settings.get(None, WATERMARK_MS_KEY).unwrap().unwrap(),
            old,
            "不空闲:水位线不动"
        );
        // ② 空闲了但没攒够(2h、1 条)→ 同样不动
        s.settings.set(None, WATERMARK_MS_KEY, &(now - 2 * 3_600_000).to_string()).unwrap();
        assert_eq!(run(&p, &s, now + IDLE_WINDOW_MS + 60_000).await.unwrap(), 0);
        assert!(s.diary.list(10).unwrap().is_empty(), "两回都没蒸馏(表空 = LLM 没被调)");
    }

    #[tokio::test]
    async fn quiet_gap_advances_watermark_without_llm() {
        let s = store("quiet");
        s.users.ensure_default_user().unwrap();
        let now = today_ms();
        // 三天没开机、没有任何动静:水位线直接翻篇到安静边界,LLM 不被调(表空反证)
        s.settings.set(None, WATERMARK_MS_KEY, &(now - 3 * 86_400_000).to_string()).unwrap();
        let p = tattletale(&local_date(now).unwrap().to_string());
        assert_eq!(run(&p, &s, now).await.unwrap(), 0);
        assert!(s.diary.list(10).unwrap().is_empty());
        let wm: i64 =
            s.settings.get(None, WATERMARK_MS_KEY).unwrap().unwrap().parse().unwrap();
        assert_eq!(wm, now - IDLE_WINDOW_MS);
    }

    #[tokio::test]
    async fn same_day_second_pass_merges_rewrite() {
        let s = store("merge");
        let user = s.users.ensure_default_user().unwrap();
        let conv = s.chat.create_conversation(user.id, "companion").unwrap();
        let now = today_ms();
        let today = local_date(now).unwrap();
        // 这天早些时候已写过;之后又攒够 → 已写的喂回模型融合,整条换新、仍只一条
        s.diary.upsert(&today.to_string(), "上午陪着看了会儿汪汪队。").unwrap();
        s.chat.append_message(conv.id, "user", "帮我把保单归档一下").unwrap();
        s.settings.set(None, WATERMARK_MS_KEY, &(now - 25 * 3_600_000).to_string()).unwrap();
        let json = format!(
            "[{{\"date\":\"{today}\",\"content\":\"上午陪着看了汪汪队,后来把保单归了档。\"}}]"
        );
        let p: Arc<dyn LlmProvider> =
            Arc::new(FakeLlm::scripted(vec![FakeTurn { text: json, ..Default::default() }]));
        assert_eq!(run(&p, &s, now + IDLE_WINDOW_MS + 60_000).await.unwrap(), 1);
        let all = s.diary.list(10).unwrap();
        assert_eq!(all.len(), 1, "同一天仍只有一条");
        assert!(all[0].content.contains("保单"), "融合重写换上了新内容");
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
    fn materials_bucket_by_day_and_carry_prior() {
        let s = store("mat");
        let user = s.users.ensure_default_user().unwrap();
        let conv = s.chat.create_conversation(user.id, "companion").unwrap();
        s.chat.append_message(conv.id, "user", "放首儿歌").unwrap();
        s.chat.append_message(conv.id, "assistant", "放上了。").unwrap();
        s.chat.append_message(conv.id, "tool", "内部行不进日记").unwrap();
        s.todos.add(user.id, "给车做年检").unwrap();
        let now = today_ms();
        let today = local_date(now).unwrap();
        s.diary.upsert(&today.to_string(), "早上记过一句。").unwrap();
        let days = collect_materials(&s, now - 3_600_000, now + 1).unwrap();
        assert_eq!(days.len(), 1);
        let d = &days[0];
        assert!(d.lines.iter().any(|l| l.starts_with("用户: 放首儿歌")));
        assert!(d.lines.iter().any(|l| l.starts_with("我: ")));
        assert!(d.lines.iter().any(|l| l.contains("记下想办:给车做年检")));
        assert!(!d.lines.iter().any(|l| l.contains("内部行")), "tool 行不进料");
        assert_eq!(d.prior.as_deref(), Some("早上记过一句。"), "这天已写的随材料带上");
        // build_request 把已记的渲染进材料区(SYSTEM 据此融合重写)
        let req = build_request(&days);
        let ChatMessage::User { content, .. } = &req.messages[0] else {
            panic!("materials 该是单条 user 消息");
        };
        assert!(content.contains("这天此前已记:早上记过一句。"));
    }
}
