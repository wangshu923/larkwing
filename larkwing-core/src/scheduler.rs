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

const POLL: std::time::Duration = std::time::Duration::from_secs(30);
/// 错过宽限:超过它的不补发 —— once 标 missed;重复型推进到未来最近一次。
const MISS_GRACE_MS: i64 = 2 * 3600 * 1000;

/// 常驻循环(壳层经 tauri::async_runtime::spawn 挂起)。
pub async fn run(engine: Arc<Engine>) {
    tracing::info!("任务调度器在线(每 {POLL:?} 轮询)");
    loop {
        tokio::time::sleep(POLL).await;
        let now = Local::now().timestamp_millis();
        if let Err(e) = tick(&engine, now).await {
            tracing::warn!("调度器单轮失败: {e:#}");
        }
    }
}

/// 单步(可测):处理当下所有到点任务。
pub async fn tick(engine: &Engine, now: i64) -> anyhow::Result<()> {
    let store = engine.store().clone();
    let due = tokio::task::spawn_blocking(move || store.jobs.due(now)).await??;
    let mut weather: Option<WeatherClient> = None; // 仅在遇到条件提醒时懒建(避免无谓建 HTTP 客户端)
    for job in due {
        // 条件提醒:due_at 是「下次检查时刻」,走天气求值分支;time 型照旧
        if job.kind == "cond" {
            let client = weather.get_or_insert_with(WeatherClient::new);
            if let Err(e) = tick_cond(engine, client, &job, now).await {
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
        match engine.wake_turn(&job).await {
            Ok(true) => {
                // 触发即推进:once → done;重复 → 锚定原钟点的下一次
                let store = engine.store().clone();
                let (id, due_at, repeat) = (job.id, job.due_at, job.repeat.clone());
                tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                    match next_due(due_at, &repeat) {
                        Some(n) => store.jobs.advance(id, n)?,
                        None => store.jobs.finish(id, "done")?,
                    }
                    Ok(())
                })
                .await??;
            }
            Ok(false) => {
                // 目标会话正在飞:不打断,状态不动,下个 tick 再试
                tracing::debug!(job = job.id, "会话忙,本轮跳过");
            }
            Err(e) => {
                // 没钥匙/建连失败:留 pending 自然重试,最终被宽限规则收掉,不死循环轰炸
                tracing::warn!(job = job.id, "自启回合失败: {e}");
            }
        }
    }
    Ok(())
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

    let w = weather.report_for(&city, qw, When::Today).await?;
    let (hit, reading) = cond.evaluate(&w);
    if !hit {
        advance(engine, job.id, now + CHECK_INTERVAL_MS).await?;
        return Ok(());
    }

    // 命中:静音时段(夜里)顺延到早上,不半夜喊人
    let hour = Local.timestamp_millis_opt(now).single().map(|t| t.hour()).unwrap_or(12);
    if hour >= QUIET_START_HOUR || hour < QUIET_END_HOUR {
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

        // 等自启回合收尾的广播(完成才发,带超时防挂死)
        let ev = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if let Ok(AppEvent::Conversation(c)) = bus_rx.recv().await {
                    return c;
                }
            }
        })
        .await
        .expect("自启回合必须广播会话动静");
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
