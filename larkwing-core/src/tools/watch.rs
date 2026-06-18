//! 能力轴:条件提醒(设)。把"提醒"从「到点」扩到「满足条件」—— 天气转凉/转热/下雨时
//! 主动开口(robot 没有的主动性)。底座复用 jobs+scheduler(PLAN 天气块);**看/取消复用
//! reminder_list / reminder_cancel**(条件提醒也是一条 pending job,正交原语不另造工具)。
//! content 同 reminder 纪律:自包含(满足时执行看不到现在的对话,指代全展开)。

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;

use crate::scheduler::WatchCondition;
use crate::weather::WeatherClient;

use super::weather::resolve_city;
use super::{Tool, ToolCtx, ToolSpec};

const CONTENT_MAX_CHARS: usize = 2000;
const DEFAULT_EXPIRE_DAYS: i64 = 7;
const MAX_EXPIRE_DAYS: i64 = 30;
const DAY_MS: i64 = 86_400_000;

pub(super) struct WatchSet {
    spec: ToolSpec,
    weather: Arc<WeatherClient>,
}

impl WatchSet {
    pub(super) fn new(weather: Arc<WeatherClient>) -> WatchSet {
        WatchSet {
            spec: ToolSpec {
                name: "watch_set",
                description: "设「条件提醒」——天气满足某条件时主动提醒(不是到点,而是到天气)。\
                              用于「降温了提醒我加衣」「下雨提醒我带伞」「太热了提醒我」这类(到点\
                              用 reminder_set)。content 自包含:满足时看不到现在的对话,把要提醒\
                              的事写全。metric=看哪项(low_temp 最低温 / high_temp 最高温 / \
                              rain 有无雨雪);op=below(≤阈值)/ above(≥阈值)/ is(rain 用);\
                              value=温度阈值(℃,rain 省略)。城市默认本地(自动定位,别反问)。\
                              一次性:提醒过就结束。看/取消用 reminder_list / reminder_cancel。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "content": {
                            "type": "string",
                            "description": "满足条件时要提醒的话,自包含、指代展开"
                        },
                        "metric": {
                            "type": "string",
                            "enum": ["low_temp", "high_temp", "rain"],
                            "description": "看哪项天气:最低温 / 最高温 / 有无雨雪"
                        },
                        "op": {
                            "type": "string",
                            "enum": ["below", "above", "is"],
                            "description": "below=≤阈值 / above=≥阈值 / is=有(rain 用)"
                        },
                        "value": {
                            "type": "number",
                            "description": "温度阈值(℃);metric=rain 时省略"
                        },
                        "city": {
                            "type": "string",
                            "description": "城市名;不填=所在城市(自动定位/已记住的)"
                        },
                        "expire_days": {
                            "type": "integer",
                            "description": "守候天数,默认 7,最多 30;到期还没满足就放弃"
                        }
                    },
                    "required": ["content", "metric", "op"]
                }),
                timeout: Duration::from_secs(30),
                ui_key: "tool.watch_set",
            },
            weather,
        }
    }
}

#[async_trait]
impl Tool for WatchSet {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> anyhow::Result<String> {
        let content = args
            .get("content")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .context("缺少 content 参数")?
            .to_string();
        // 超长不静默截断(§3.5):退回错误,让模型精简后重试。
        let n = content.chars().count();
        if n > CONTENT_MAX_CHARS {
            anyhow::bail!("提醒内容 {n} 字,超过 {CONTENT_MAX_CHARS} 字上限,没有设。请精简后重试。");
        }
        let metric = args.get("metric").and_then(serde_json::Value::as_str).unwrap_or("");
        if !["low_temp", "high_temp", "rain"].contains(&metric) {
            anyhow::bail!("未知 metric: {metric},可用 low_temp/high_temp/rain");
        }
        let op = args.get("op").and_then(serde_json::Value::as_str).unwrap_or("");
        if !["below", "above", "is"].contains(&op) {
            anyhow::bail!("未知 op: {op},可用 below/above/is");
        }
        let value_opt = args.get("value").and_then(serde_json::Value::as_f64);
        if metric != "rain" && value_opt.is_none() {
            anyhow::bail!("metric={metric} 需要 value(温度阈值 ℃)");
        }
        let value = value_opt.unwrap_or(0.0);
        let expire_days = args
            .get("expire_days")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(DEFAULT_EXPIRE_DAYS)
            .clamp(1, MAX_EXPIRE_DAYS);

        let city_arg =
            args.get("city").and_then(serde_json::Value::as_str).map(str::trim).filter(|s| !s.is_empty());
        let city = resolve_city(ctx, &self.weather, city_arg).await?;

        let now = chrono::Local::now().timestamp_millis();
        let cond = WatchCondition {
            city: Some(city.clone()),
            metric: metric.into(),
            op: op.into(),
            value,
            expire_at: now + expire_days * DAY_MS,
        };
        let cond_json = serde_json::to_string(&cond).context("序列化条件失败")?;

        let store = ctx.store.clone();
        let (user_id, conv_id) = (ctx.user_id, ctx.conv_id);
        let content2 = content.clone();
        // first_check_at = now:下个 tick(~30s)就先查一次,已满足则立即提醒
        let job = tokio::task::spawn_blocking(move || {
            store.jobs.add_watch(user_id, conv_id, &content2, &cond_json, now)
        })
        .await
        .context("条件提醒落库任务挂了")??;

        Ok(format!(
            "已盯上了 #{}:{}(守 {} 天),满足就提醒你",
            job.id,
            describe(metric, op, value, &city),
            expire_days
        ))
    }
}

/// 给模型回执的人话条件描述。
fn describe(metric: &str, op: &str, value: f64, city: &str) -> String {
    let v = value as i32;
    match metric {
        "rain" => format!("{city}有雨雪"),
        "low_temp" if op == "below" => format!("{city}最低温降到 {v}℃ 以下"),
        "low_temp" => format!("{city}最低温升到 {v}℃ 以上"),
        "high_temp" if op == "above" => format!("{city}最高温升到 {v}℃ 以上"),
        "high_temp" => format!("{city}最高温降到 {v}℃ 以下"),
        _ => format!("{city}天气条件"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::MediaRuntime;
    use crate::store::Store;

    fn ctx(tag: &str) -> ToolCtx {
        let dir = std::env::temp_dir().join(format!("lw-watchtool-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _ = std::fs::remove_file(dir.join("t.db"));
        let store = Store::open(&dir.join("t.db")).unwrap();
        store.users.ensure_default_user().unwrap();
        let conv = store.chat.create_conversation(1, "companion").unwrap();
        ToolCtx { user_id: 1, conv_id: conv.id, media: MediaRuntime::detached(store.clone()), store }
    }

    #[tokio::test]
    async fn watch_set_bakes_condition_and_persists() {
        let ctx = ctx("set");
        let tool = WatchSet::new(Arc::new(WeatherClient::new()));
        // 显式给 city → resolve_city 直接用,不触网
        let out = tool
            .run(
                serde_json::json!({
                    "content": "天凉了,提醒加件外套",
                    "metric": "low_temp",
                    "op": "below",
                    "value": 10,
                    "city": "杭州"
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.contains("#1") && out.contains("杭州") && out.contains("守 7 天"));

        let jobs = ctx.store.jobs.list_pending(1).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].kind, "cond");
        let cond: WatchCondition =
            serde_json::from_str(jobs[0].condition.as_ref().unwrap()).unwrap();
        assert_eq!(cond.city.as_deref(), Some("杭州"));
        assert_eq!(cond.metric, "low_temp");
        assert_eq!(cond.value, 10.0);
        assert!(cond.expire_at > chrono::Local::now().timestamp_millis());
    }

    #[tokio::test]
    async fn watch_set_rejects_bad_metric_and_missing_value() {
        let ctx = ctx("bad");
        let tool = WatchSet::new(Arc::new(WeatherClient::new()));
        assert!(tool
            .run(
                serde_json::json!({"content": "x", "metric": "humidity", "op": "below", "value": 1, "city": "x"}),
                &ctx
            )
            .await
            .is_err());
        assert!(
            tool.run(
                serde_json::json!({"content": "x", "metric": "low_temp", "op": "below", "city": "x"}),
                &ctx
            )
            .await
            .is_err(),
            "温度型缺 value 应报错"
        );
    }

    #[tokio::test]
    async fn watch_set_rain_needs_no_value() {
        let ctx = ctx("rain");
        let tool = WatchSet::new(Arc::new(WeatherClient::new()));
        let out = tool
            .run(
                serde_json::json!({
                    "content": "下雨了提醒带伞",
                    "metric": "rain",
                    "op": "is",
                    "city": "北京"
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.contains("有雨雪"));
    }

    #[tokio::test]
    async fn watch_set_rejects_overlong_content() {
        let ctx = ctx("overlong");
        let tool = WatchSet::new(Arc::new(WeatherClient::new()));
        let too_long = "九".repeat(CONTENT_MAX_CHARS + 50);
        // 内容超长 → 退回错误,不静默截断(§3.5)
        assert!(tool
            .run(
                serde_json::json!({"content": too_long, "metric": "rain", "op": "is", "city": "x"}),
                &ctx
            )
            .await
            .is_err());
    }
}
