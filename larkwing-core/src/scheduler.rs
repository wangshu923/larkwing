//! 定时任务调度器(robot cron 的消费级重生):一张表 + 一个轮询循环,没有 cron 框架。
//! 真相在库(jobs 域),循环本身无状态 —— 重启即恢复;错过太久的不补发(防开机轰炸)。
//! 推进语义:触发即推进(at-most-once per 次),回合半路失败按"已提醒"算,不重发。

use std::sync::Arc;

use chrono::{Datelike, Duration, Local, TimeZone, Weekday};

use crate::engine::Engine;

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
    for job in due {
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
