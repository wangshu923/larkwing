//! 定时任务调度器(robot cron 的消费级重生):一张表 + 一个轮询循环,没有 cron 框架。
//! 真相在库(jobs 域),循环本身无状态 —— 重启即恢复;错过太久的不补发(防开机轰炸)。
//! 推进语义:触发即推进(at-most-once per 次),回合半路失败按"已提醒"算,不重发。

use std::sync::Arc;

use anyhow::Context;
use chrono::{Datelike, Duration, Local, TimeZone, Timelike, Weekday};
use serde::{Deserialize, Serialize};

use crate::engine::Engine;
use crate::store::Job;
use crate::weather::{qweather_cfg, Weather, WeatherClient, When};

/// 兜底节拍:没有任何任务快到期时也每隔这么久醒一次(日记 / 惯例提炼搭这一拍;因会话忙、
/// 建连失败留下的过期任务也按它重试)。
const POLL: std::time::Duration = std::time::Duration::from_secs(30);
/// 错过宽限:超过它的不补发 —— once 标 missed;重复型推进到未来最近一次。
const MISS_GRACE_MS: i64 = 2 * 3600 * 1000;

/// 新任务入库时踢调度器一脚(`kick`):别等它睡满这一觉。permit 语义 —— 它正忙着跑一轮时
/// 踢的那一脚不丢,下一觉立刻醒。
static KICK: tokio::sync::Notify = tokio::sync::Notify::const_new();

/// 有新任务落库了(设提醒 / 盯天气 / 后台的活忙完要汇报):叫醒调度器重新看一眼。
/// 同步、廉价,哪个线程都能调;没有调度器在跑(测试)= 白调无害。
pub fn kick() {
    KICK.notify_one();
}

/// 这一觉睡多久(纯函数,可测):有任务在 `POLL` 之内到期就睡到它到点,否则睡满 `POLL`。
/// 2026-11-11 改:原先固定每 30 秒看一次 —— 「3 分钟后叫我」最坏晚 30 秒(「1 分钟后」等于晚一半),
/// 后台的活跑完到开口汇报也要白等最多 30 秒。
fn sleep_for(now: i64, next_due: Option<i64>) -> std::time::Duration {
    match next_due {
        Some(due) if due > now => {
            std::time::Duration::from_millis((due - now) as u64).min(POLL)
        }
        _ => POLL,
    }
}

/// 常驻循环(壳层经 tauri::async_runtime::spawn 挂起)。
pub async fn run(engine: Arc<Engine>) {
    tracing::info!("任务调度器在线(睡到下一个到期点,最长 {POLL:?})");
    loop {
        let now = Local::now().timestamp_millis();
        let store = engine.store().clone();
        let next = tokio::task::spawn_blocking(move || store.jobs.next_due_after(now))
            .await
            .ok()
            .and_then(|r| r.ok())
            .flatten();
        tokio::select! {
            _ = tokio::time::sleep(sleep_for(now, next)) => {}
            _ = KICK.notified() => {} // 有新任务入库:立刻看一眼(它可能就是「现在到期」)
        }
        let now = Local::now().timestamp_millis();
        if let Err(e) = tick(&engine, now).await {
            tracing::warn!("调度器单轮失败: {e:#}");
        }
    }
}

/// 单步(可测):处理当下所有到点任务。
pub async fn tick(engine: &Engine, now: i64) -> anyhow::Result<()> {
    // 家庭日记搭同一节拍(engine/diary.rs):跨了没写的日历日才真跑,内部自带限流/防重入/
    // FakeLlm 挡板,fire-and-forget 绝不拖慢下面的提醒。
    engine.spawn_diary(now);
    // 惯例提炼搭同一节拍(engine/habits.rs):到期 / 攒够 / 空闲才真跑,同样 fire-and-forget。
    engine.spawn_habits(now);
    let store = engine.store().clone();
    let due = tokio::task::spawn_blocking(move || store.jobs.due(now)).await??;
    let mut weather: Option<WeatherClient> = None; // 仅在遇到条件提醒时懒建(避免无谓建 HTTP 客户端)
    // 已随同会话的另一条汇报一起交出去(或一起被挡回)的:这一拍不再单独处理
    let mut merged: std::collections::HashSet<i64> = std::collections::HashSet::new();
    for (i, job) in due.iter().enumerate() {
        if merged.contains(&job.id) {
            continue;
        }
        // 条件提醒:due_at 是「下次检查时刻」,走天气求值分支;time 型照旧
        if job.kind == "cond" {
            let client = weather.get_or_insert_with(WeatherClient::new);
            if let Err(e) = tick_cond(engine, client, job, now).await {
                tracing::warn!(job = job.id, "条件提醒检查失败: {e:#}");
            }
            continue;
        }
        if now - job.due_at > MISS_GRACE_MS {
            // 错过太久(关机/没钥匙堆着):once → missed;重复 → 推进,不补发
            let store = engine.store().clone();
            let (id, due_at, repeat) = (job.id, job.due_at, job.repeat.clone());
            tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                match next_future_due(due_at, &repeat, now) {
                    Some(n) => store.jobs.advance(id, n)?,
                    None => store.jobs.finish(id, "missed")?,
                }
                Ok(())
            })
            .await??;
            tracing::info!(job = job.id, repeat = %job.repeat, "任务超出宽限期,跳过本次");
            continue;
        }
        // 同一会话里同时到期的其余汇报并进这一回合(2026-11-12):四路分头办事前后脚跑完,原先是
        // 四个串行回合(第二条起每条都先撞一次「会话忙」等下一拍)、每个都带整段历史。
        let mates = report_mates(&due[i + 1..], job, now);
        merged.extend(mates.iter().map(|j| j.id)); // 成不成都算处理过:挡回来的下一拍一起重试
        let also: Vec<String> = mates.iter().map(|j| j.content.clone()).collect();
        match engine.wake_turn_with(job, &also).await {
            Ok(true) => {
                // 触发即推进:once → done;重复 → 锚定原钟点的下一次
                let store = engine.store().clone();
                let fired: Vec<(i64, i64, String)> = std::iter::once(job)
                    .chain(mates.iter().copied())
                    .map(|j| (j.id, j.due_at, j.repeat.clone()))
                    .collect();
                if fired.len() > 1 {
                    tracing::info!(conv = job.conv_id, n = fired.len(), "同会话同时到期的汇报并成一个回合");
                }
                tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                    for (id, due_at, repeat) in fired {
                        match next_due(due_at, &repeat) {
                            Some(n) => store.jobs.advance(id, n)?,
                            None => store.jobs.finish(id, "done")?,
                        }
                    }
                    Ok(())
                })
                .await??;
            }
            Ok(false) => {
                // 目标会话正在飞:不打断,状态不动,下个 tick 再试。
                // info 级:这条若连续刷 = 会话被误判「一直在忙」,是要人看见的异常信号
                // (2026-07-04 之前是 debug,提醒被无声跳过好几天没人知道)。
                tracing::info!(job = job.id, conv = job.conv_id, "会话忙,提醒本轮跳过(下个 tick 再试)");
            }
            Err(e) => {
                // 没钥匙/建连失败:留 pending 自然重试,最终被宽限规则收掉,不死循环轰炸
                tracing::warn!(job = job.id, "自启回合失败: {e}");
            }
        }
    }
    Ok(())
}

/// `rest`(本拍到期清单里排在 `lead` 之后的那些)里能并进 `lead` 这一回合的:都是汇报、落在同一个
/// 会话、同一个说话人名下,且没超出错过宽限(超了的让它自己走「错过」那条路)。`lead` 不是汇报 → 空。
fn report_mates<'a>(rest: &'a [Job], lead: &Job, now: i64) -> Vec<&'a Job> {
    if lead.kind != "report" {
        return Vec::new();
    }
    rest.iter()
        .filter(|j| {
            j.kind == "report"
                && j.conv_id == lead.conv_id
                && j.user_id == lead.user_id
                && now - j.due_at <= MISS_GRACE_MS
        })
        .collect()
}

/// 下一次 due(锚定原 due 的钟点,本地时区):daily +1 天;weekdays 跳过周末;
/// weekly +7 天;once/未知词 → None。
pub fn next_due(due_ms: i64, repeat: &str) -> Option<i64> {
    let dt = Local.timestamp_millis_opt(due_ms).single()?;
    let next = match repeat {
        "daily" => dt + Duration::days(1),
        "weekly" => dt + Duration::days(7),
        "weekdays" => {
            let mut n = dt + Duration::days(1);
            while matches!(n.weekday(), Weekday::Sat | Weekday::Sun) {
                n += Duration::days(1);
            }
            n
        }
        _ => return None,
    };
    Some(next.timestamp_millis())
}

/// 推进到 now 之后的最近一次(宽限超期时用);once → None。
fn next_future_due(mut due_ms: i64, repeat: &str, now: i64) -> Option<i64> {
    loop {
        due_ms = next_due(due_ms, repeat)?;
        if due_ms > now {
            return Some(due_ms);
        }
    }
}

// ---------------------------------------------------------------------------
// 条件提醒(PLAN 天气块):scheduler 旁的求值器 —— 天气谓词逻辑落在这里,**不进通用
// 回合循环**(宪法 §5 任务知识零入底座)。watch_set 工具写 condition JSON;此处解析 +
// 取天气 + 比较;命中则 wake_turn 触发(克隆 Job 现填实测值,wake_turn 本身不动)。
// ---------------------------------------------------------------------------

const CHECK_INTERVAL_MS: i64 = 3_600_000; // 不满足时每小时复查
const QUIET_START_HOUR: u32 = 22; // 22:00–08:00 静音:满足也顺延到早上再喊
const QUIET_END_HOUR: u32 = 8;

/// 天气谓词(watch 工具写、scheduler 读)。metric: low_temp|high_temp|rain;
/// op: below(≤)|above(≥)|is(rain 用);value = 温度阈值;expire_at 后停止守候。
/// city 在 watch_set 时已解析成具体城市(定位机器在工具层,scheduler 只管纯求值)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchCondition {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub city: Option<String>,
    pub metric: String,
    pub op: String,
    #[serde(default)]
    pub value: f64,
    pub expire_at: i64,
}

impl WatchCondition {
    /// 给今天的天气,返回 (是否满足, 实测读数文案)。
    pub fn evaluate(&self, w: &Weather) -> (bool, String) {
        match self.metric.as_str() {
            "low_temp" => match w.days.first().map(|d| d.low_c).or(w.temp_c) {
                Some(l) => (cmp(l as f64, &self.op, self.value), format!("最低 {l}℃")),
                None => (false, String::new()),
            },
            "high_temp" => match w.days.first().map(|d| d.high_c).or(w.temp_c) {
                Some(h) => (cmp(h as f64, &self.op, self.value), format!("最高 {h}℃")),
                None => (false, String::new()),
            },
            "rain" => {
                let txt = w.days.first().map(|d| d.text.as_str()).unwrap_or(w.text.as_str());
                let rainy = ["雨", "雪", "雷"].iter().any(|k| txt.contains(k));
                (rainy, txt.to_string())
            }
            _ => (false, String::new()),
        }
    }
}

fn cmp(actual: f64, op: &str, threshold: f64) -> bool {
    match op {
        "below" => actual <= threshold,
        "above" => actual >= threshold,
        _ => false,
    }
}

/// 一条条件提醒的单步检查:超期收尾 / 取天气求值 / 命中触发(静音时段顺延)/ 否则推迟复查。
async fn tick_cond(
    engine: &Engine,
    weather: &WeatherClient,
    job: &Job,
    now: i64,
) -> anyhow::Result<()> {
    let cond: WatchCondition =
        serde_json::from_str(job.condition.as_deref().unwrap_or("{}")).context("条件谓词损坏")?;

    // 超期 → 停止守候(set 时默认给 7 天)
    if now >= cond.expire_at {
        let (store, id) = (engine.store().clone(), job.id);
        tokio::task::spawn_blocking(move || store.jobs.finish(id, "missed")).await??;
        tracing::info!(job = job.id, "条件提醒超期,停止守候");
        return Ok(());
    }

    let Some(city) = cond.city.clone().filter(|c| !c.is_empty()) else {
        advance(engine, job.id, now + CHECK_INTERVAL_MS).await?; // 没烘焙到城市,推迟复查不卡死
        return Ok(());
    };

    // 选源(同 weather 工具:和风 JWT 三件套齐备 → 和风,否则免 key Open-Meteo)
    let store = engine.store().clone();
    let qw = tokio::task::spawn_blocking(move || qweather_cfg(&store.settings)).await??;

    // 取天气失败**必须推进 due_at**(2026-08-22 审计):原先这里用 `?` 把错误抛回 tick,
    // due_at 一步没动 → 下个 30s tick 同一条 job 又 due,退化成**每 30 秒打一次第三方天气 API**
    // (断网/限流/城市解析抖动时最长打到 7 天过期为止)。改成:失败就当这次没测成,推迟到
    // 下次常规检查(CHECK_INTERVAL_MS,与「没命中」同一节拍),别把重试压成秒级轮询。
    let w = match weather.report_for(&city, qw, When::Today).await {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!(job = job.id, city = %city, "盯天气取数失败,推迟到下次常规检查: {e:#}");
            advance(engine, job.id, now + CHECK_INTERVAL_MS).await?;
            return Ok(());
        }
    };
    let (hit, reading) = cond.evaluate(&w);
    if !hit {
        advance(engine, job.id, now + CHECK_INTERVAL_MS).await?;
        return Ok(());
    }

    // 命中:静音时段(夜里)顺延到早上,不半夜喊人
    let hour = Local.timestamp_millis_opt(now).single().map(|t| t.hour()).unwrap_or(12);
    if in_quiet_hours(hour) {
        advance(engine, job.id, next_8am(now)).await?;
        tracing::info!(job = job.id, "条件满足但在静音时段,顺延到早上");
        return Ok(());
    }

    // 触发:实测读数现填进 content(克隆 Job,wake_turn 本身不动)
    let mut fire = job.clone();
    fire.content = format!("{}({})", job.content, reading);
    match engine.wake_turn(&fire).await {
        Ok(true) => {
            let (store, id) = (engine.store().clone(), job.id);
            tokio::task::spawn_blocking(move || store.jobs.finish(id, "done")).await??;
        }
        Ok(false) => tracing::debug!(job = job.id, "会话忙,下个 tick 再触发"),
        Err(e) => tracing::warn!(job = job.id, "条件提醒自启回合失败: {e}"),
    }
    Ok(())
}

async fn advance(engine: &Engine, id: i64, next: i64) -> anyhow::Result<()> {
    let store = engine.store().clone();
    tokio::task::spawn_blocking(move || store.jobs.advance(id, next)).await??;
    Ok(())
}

/// 这个钟点在静音时段里吗。静音窗**跨零点**(22:00–08:00),所以写成「不在白天那一段里」——
/// `hour >= 22 || hour < 8` 与 `!(8..22).contains(&hour)` 同义,后者过 clippy(manual_range_contains)。
fn in_quiet_hours(hour: u32) -> bool {
    !(QUIET_END_HOUR..QUIET_START_HOUR).contains(&hour)
}

/// now 之后最近的本地 08:00(静音时段顺延用)。
fn next_8am(now: i64) -> i64 {
    let Some(dt) = Local.timestamp_millis_opt(now).single() else {
        return now + CHECK_INTERVAL_MS;
    };
    let Some(naive8) = dt.date_naive().and_hms_opt(QUIET_END_HOUR, 0, 0) else {
        return now + CHECK_INTERVAL_MS;
    };
    let Some(today8) = Local.from_local_datetime(&naive8).single() else {
        return now + CHECK_INTERVAL_MS;
    };
    let target = if dt.hour() < QUIET_END_HOUR { today8 } else { today8 + Duration::days(1) };
    target.timestamp_millis()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::AppEvent;
    use crate::scenes::Scenes;
    use crate::store::Store;

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> i64 {
        Local.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap().timestamp_millis()
    }

    /// 静音窗跨零点:22/23/0…7 点算静音,8…21 点不算(边界两头都钉住)。
    #[test]
    fn quiet_hours_wrap_around_midnight() {
        for h in [22, 23, 0, 3, 7] {
            assert!(in_quiet_hours(h), "{h} 点该算静音");
        }
        for h in [8, 12, 21] {
            assert!(!in_quiet_hours(h), "{h} 点不该算静音");
        }
    }

    /// 睡到下一个到期点(2026-11-11):快到期的睡到它到点,远的 / 没有的 / 已过期的睡满兜底节拍。
    #[test]
    fn sleeps_until_next_due_but_never_longer_than_poll() {
        use std::time::Duration as D;
        assert_eq!(sleep_for(1_000, Some(4_500)), D::from_millis(3_500), "3.5 秒后到期就睡 3.5 秒");
        assert_eq!(sleep_for(1_000, Some(1_000 + 10 * 60_000)), POLL, "十分钟后才到期:睡满一拍");
        assert_eq!(sleep_for(1_000, None), POLL, "没有待触发的任务");
        assert_eq!(sleep_for(1_000, Some(900)), POLL, "已过期的不算(那是留着重试的,按节拍来)");
    }

    #[test]
    fn next_due_keeps_clock_and_skips_weekends() {
        // 2026-06-12 是周五
        let fri9 = at(2026, 6, 12, 9, 0);
        assert_eq!(next_due(fri9, "daily"), Some(at(2026, 6, 13, 9, 0)));
        assert_eq!(next_due(fri9, "weekly"), Some(at(2026, 6, 19, 9, 0)));
        assert_eq!(next_due(fri9, "weekdays"), Some(at(2026, 6, 15, 9, 0)), "周五的下个工作日是周一");
        assert_eq!(next_due(fri9, "once"), None);
        assert_eq!(next_due(fri9, "每周"), None, "未知词当 once,绝不死循环");
    }

    #[test]
    fn next_future_due_lands_after_now() {
        let base = at(2026, 6, 12, 9, 0);
        let now = at(2026, 6, 15, 13, 30); // 三天半后
        assert_eq!(next_future_due(base, "daily", now), Some(at(2026, 6, 16, 9, 0)));
        assert_eq!(next_future_due(base, "once", now), None);
    }

    #[test]
    fn watch_condition_evaluates_temp_and_rain() {
        let w = Weather {
            city: "x".into(),
            temp_c: Some(20),
            feels_c: None,
            text: "多云".into(),
            humidity: None,
            wind: None,
            tips: vec![],
            days: vec![crate::weather::DayForecast {
                date: String::new(),
                high_c: 30,
                low_c: 5,
                text: "小雨".into(),
            }],
            source: "",
        };
        let cold = WatchCondition {
            city: Some("x".into()),
            metric: "low_temp".into(),
            op: "below".into(),
            value: 10.0,
            expire_at: 0,
        };
        assert!(cold.evaluate(&w).0, "最低 5 ≤ 10 → 满足");
        let hot = WatchCondition {
            city: None,
            metric: "high_temp".into(),
            op: "above".into(),
            value: 35.0,
            expire_at: 0,
        };
        assert!(!hot.evaluate(&w).0, "最高 30 < 35 → 不满足");
        let rain = WatchCondition {
            city: None,
            metric: "rain".into(),
            op: "is".into(),
            value: 0.0,
            expire_at: 0,
        };
        assert!(rain.evaluate(&w).0, "「小雨」含雨 → 满足");
    }

    #[test]
    fn next_8am_lands_on_morning() {
        assert_eq!(next_8am(at(2026, 6, 15, 23, 0)), at(2026, 6, 16, 8, 0), "夜里 → 次日早 8 点");
        assert_eq!(next_8am(at(2026, 6, 15, 3, 0)), at(2026, 6, 15, 8, 0), "凌晨 → 当日早 8 点");
    }

    #[tokio::test]
    async fn tick_fires_due_job_advances_and_announces() {
        let dir = std::env::temp_dir().join(format!("lw-sched-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _ = std::fs::remove_file(dir.join("t.db"));
        let store = Store::open(&dir.join("t.db")).unwrap();
        let engine = Engine::new(store.clone(), Scenes::builtin());
        engine.set_provider(Some(std::sync::Arc::new(crate::llm::fake::FakeLlm::default())));
        let mut bus_rx = engine.bus().subscribe();

        let user = store.users.ensure_default_user().unwrap();
        let conv = store.chat.create_conversation(user.id, "companion").unwrap();
        store.jobs.add(user.id, conv.id, "提醒吃降压药(饭后配水)", 1_000, "daily").unwrap();

        tick(&engine, 5_000).await.unwrap();

        // 等自启回合收尾的广播(完成才发,带超时防挂死)。顺带记下它之前有没有先喊过「起跑了」
        // (2026-11-11:主窗靠这条把会话记成在飞 —— 必须先于收尾到,且会话 / 种类对得上)。
        let (started, ev) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let mut started = None;
            loop {
                match bus_rx.recv().await {
                    Ok(AppEvent::TurnStarted(s)) => started = Some(s),
                    Ok(AppEvent::Conversation(c)) => return (started, c),
                    _ => {}
                }
            }
        })
        .await
        .expect("自启回合必须广播会话动静");
        let started = started.expect("收尾之前必须先广播起跑");
        assert_eq!((started.conv_id, started.kind.as_str()), (conv.id, "reminder"));
        // 收尾广播与回合任务结束几乎同时(事件通道关了 → 任务随即返回):给它一小会儿再查
        let mut busy = engine.conversation_busy(conv.id);
        for _ in 0..40 {
            if !busy {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            busy = engine.conversation_busy(conv.id);
        }
        assert!(!busy, "回合收尾后对账口径要回到「不在飞」(主窗靠它兜漏掉的收尾事件)");
        assert_eq!(ev.conv_id, conv.id);
        assert_eq!(ev.kind, "reminder");

        // event 行落库(UI 不渲染) + 7274 的回应落库
        let msgs = store.chat.recent_messages(conv.id, 10).unwrap();
        assert!(msgs.iter().any(|m| m.role == "event" && m.content.contains("降压药")));
        assert!(msgs.iter().any(|m| m.role == "assistant" && !m.content.is_empty()));

        // daily 推进,锚定原钟点 +1 天
        let pending = store.jobs.list_pending(user.id).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].due_at, next_due(1_000, "daily").unwrap());
    }

    #[tokio::test]
    async fn tick_skips_long_overdue_without_firing() {
        let dir = std::env::temp_dir().join(format!("lw-sched-miss-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _ = std::fs::remove_file(dir.join("t.db"));
        let store = Store::open(&dir.join("t.db")).unwrap();
        let engine = Engine::new(store.clone(), Scenes::builtin());
        engine.set_provider(Some(std::sync::Arc::new(crate::llm::fake::FakeLlm::default())));

        let user = store.users.ensure_default_user().unwrap();
        let conv = store.chat.create_conversation(user.id, "companion").unwrap();
        let once = store.jobs.add(user.id, conv.id, "一次性的", 1_000, "once").unwrap();
        let daily = store.jobs.add(user.id, conv.id, "每天的", 1_000, "daily").unwrap();

        let now = 1_000 + MISS_GRACE_MS + 1;
        tick(&engine, now).await.unwrap();

        let pending = store.jobs.list_pending(user.id).unwrap();
        assert_eq!(pending.len(), 1, "once 标 missed,daily 留着");
        assert_eq!(pending[0].id, daily.id);
        assert!(pending[0].due_at > now, "daily 推进到未来,不补发");
        assert!(
            store.chat.recent_messages(conv.id, 10).unwrap().is_empty(),
            "超宽限不触发任何回合"
        );
        let _ = once;
    }
}
