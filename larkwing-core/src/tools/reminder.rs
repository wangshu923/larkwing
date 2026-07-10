//! 能力轴:定时(设/看/取消)。robot cron 的消费级重生:用户说人话,模型翻译时间,
//! 永远不暴露 cron/job 概念。执行一律新鲜上下文 —— 所以 content 必须物化自包含
//! (创建时把指代展开写全),这是描述里教给模型的第一纪律。
//! mode 只活在创建那一刻:remind → 落当前会话;task → 建专属会话(后续都落它)。

use anyhow::Context;
use async_trait::async_trait;
use chrono::TimeZone;

use super::{Tool, ToolCtx, ToolSpec};

const CONTENT_MAX_CHARS: usize = 2000;
const TASK_TITLE_MAX_CHARS: usize = 16;

fn parse_local(s: &str) -> anyhow::Result<i64> {
    let s = s.trim();
    // 先试带秒(短提醒「一分钟后」要秒级才准:now 现在也给秒,模型能算准点),回落到分。
    // 否则「00:52:59 + 1 分钟」被截成「00:53」→ 只差 1 秒就触发,像「立刻」而非「一分钟后」
    // (2026-07-04 用户点出的边界)。
    let naive = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M"))
        .context("时间格式应为 YYYY-MM-DD HH:MM(可带 :SS)")?;
    chrono::Local
        .from_local_datetime(&naive)
        .single()
        .context("时间无法映射到本地时区")
        .map(|dt| dt.timestamp_millis())
}

fn fmt_local(ms: i64) -> String {
    chrono::Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|dt| dt.format("%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "??".into())
}

fn repeat_label(repeat: &str) -> &'static str {
    match repeat {
        "daily" => "每天",
        "weekdays" => "工作日",
        "weekly" => "每周",
        _ => "一次",
    }
}

// ---------------------------------------------------------------------------
// reminder_set
// ---------------------------------------------------------------------------

pub(super) struct ReminderSet {
    spec: ToolSpec,
}

impl ReminderSet {
    pub(super) fn new() -> ReminderSet {
        ReminderSet {
            spec: ToolSpec {
                name: "reminder_set",
                description: "定提醒或定时任务。**先用 now 看当前时间**(now 精确到秒)再把\
                              「明早八点/十分钟后」换算成 first_at;短提醒(如「一分钟后」)务必\
                              基于 now 的秒数算准、first_at 带上秒,别只写到分钟(否则可能差出小半\
                              分钟、显得像立刻就响)。content 必须自包含:到点执行时**看不到现在的\
                              对话**,所以把要做的事、相关细节、对话里的指代全部展开写清\
                              (写「提醒吃降压药(饭后、配水)」,不写「提醒吃那个药」)。\
                              mode 选择:remind=捎话提醒,到点的话出现在当前对话(默认);\
                              task=定期干活的任务(如每天总结新闻),会单开一个任务对话存放\
                              每次的结果。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "content": {
                            "type": "string",
                            "description": "到点要做的事,自包含、指代全部展开"
                        },
                        "first_at": {
                            "type": "string",
                            "description": "首次触发时间,本地时区,格式 YYYY-MM-DD HH:MM(短提醒可带秒 HH:MM:SS 更准)"
                        },
                        "repeat": {
                            "type": "string",
                            "enum": ["once", "daily", "weekdays", "weekly"],
                            "description": "重复:once 一次(默认)/ daily 每天 / weekdays 工作日 / weekly 每周(取 first_at 的星期)"
                        },
                        "mode": {
                            "type": "string",
                            "enum": ["remind", "task"],
                            "description": "remind=捎话进当前对话(默认);task=单开任务对话存放结果"
                        }
                    },
                    "required": ["content", "first_at"]
                }),
                timeout: std::time::Duration::from_secs(5),
                ui_key: "tool.reminder_set",
            },
        }
    }
}

#[async_trait]
impl Tool for ReminderSet {
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
        let first_at = parse_local(
            args.get("first_at").and_then(serde_json::Value::as_str).context("缺少 first_at")?,
        )?;
        let now = chrono::Local::now().timestamp_millis();
        anyhow::ensure!(
            first_at > now - 60_000,
            "first_at 已经过去了({}),先用 now 工具确认当前时间再换算",
            fmt_local(first_at)
        );
        let repeat: &'static str = match args.get("repeat").and_then(serde_json::Value::as_str) {
            None | Some("once") => "once",
            Some("daily") => "daily",
            Some("weekdays") => "weekdays",
            Some("weekly") => "weekly",
            Some(other) => anyhow::bail!("未知 repeat: {other},可用 once/daily/weekdays/weekly"),
        };
        let task_mode = matches!(args.get("mode").and_then(serde_json::Value::as_str), Some("task"));

        let store = ctx.store.clone();
        let (user_id, here_conv) = (ctx.user_id, ctx.conv_id);
        let content2 = content.clone();
        let job = tokio::task::spawn_blocking(move || -> anyhow::Result<crate::store::Job> {
            // mode 只活在此刻:翻译成 conv_id 落库,唤醒管线不认识 mode
            let conv_id = if task_mode {
                // 任务模式 = 单独的任务对话(自启兑现) → 系统渠道,列表带系统标
                let conv = store.chat.create_conversation_full(
                    user_id,
                    crate::scenes::DEFAULT_SCENE_ID,
                    crate::store::chat::CHANNEL_SYSTEM,
                )?;
                let title: String = content2.chars().take(TASK_TITLE_MAX_CHARS).collect();
                store.chat.set_title(conv.id, &title)?;
                conv.id
            } else {
                here_conv
            };
            store.jobs.add(user_id, conv_id, &content2, first_at, repeat)
        })
        .await
        .context("提醒落库任务挂了")??;

        let place = if task_mode { "单独的任务对话里" } else { "这个对话里" };
        Ok(format!(
            "已设好 #{}:{}({}),到点会出现在{}",
            job.id,
            fmt_local(first_at),
            repeat_label(repeat),
            place
        ))
    }
}

// ---------------------------------------------------------------------------
// reminder_list
// ---------------------------------------------------------------------------

pub(super) struct ReminderList {
    spec: ToolSpec,
}

impl ReminderList {
    pub(super) fn new() -> ReminderList {
        ReminderList {
            spec: ToolSpec {
                name: "reminder_list",
                description: "看当前定着的提醒/定时任务(全局的,不限本对话)。\
                              用户问「我定了什么/有什么提醒」时用;取消前先用它拿编号。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
                timeout: std::time::Duration::from_secs(5),
                ui_key: "tool.reminder_list",
            },
        }
    }
}

#[async_trait]
impl Tool for ReminderList {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, _args: serde_json::Value, ctx: &ToolCtx) -> anyhow::Result<String> {
        let store = ctx.store.clone();
        let user_id = ctx.user_id;
        let jobs = tokio::task::spawn_blocking(move || store.jobs.list_pending(user_id))
            .await
            .context("提醒查询任务挂了")??;
        if jobs.is_empty() {
            return Ok("目前没有定着的提醒".into());
        }
        Ok(jobs
            .iter()
            .map(|j| {
                let content: String = j.content.chars().take(80).collect();
                // 条件提醒(kind=cond)的 due_at 是「下次检查时刻」,显示成时间会误导 → 标条件
                if j.kind == "cond" {
                    format!("#{} [条件触发] {}", j.id, content)
                } else {
                    format!("#{} {}({}){}", j.id, fmt_local(j.due_at), repeat_label(&j.repeat), content)
                }
            })
            .collect::<Vec<_>>()
            .join("\n"))
    }
}

// ---------------------------------------------------------------------------
// reminder_cancel
// ---------------------------------------------------------------------------

pub(super) struct ReminderCancel {
    spec: ToolSpec,
}

impl ReminderCancel {
    pub(super) fn new() -> ReminderCancel {
        ReminderCancel {
            spec: ToolSpec {
                name: "reminder_cancel",
                description: "取消一个提醒/定时任务(编号来自 reminder_list 或设定时的回执)。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "id": { "type": "integer", "description": "提醒编号" }
                    },
                    "required": ["id"]
                }),
                timeout: std::time::Duration::from_secs(5),
                ui_key: "tool.reminder_cancel",
            },
        }
    }
}

#[async_trait]
impl Tool for ReminderCancel {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> anyhow::Result<String> {
        let id = args.get("id").and_then(serde_json::Value::as_i64).context("缺少 id 参数")?;
        let store = ctx.store.clone();
        let user_id = ctx.user_id;
        let cancelled = tokio::task::spawn_blocking(move || store.jobs.cancel(user_id, id))
            .await
            .context("取消任务挂了")??;
        Ok(if cancelled { "ok".into() } else { format!("没有找到 #{id},先用 reminder_list 看看") })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::MediaRuntime;
    use crate::store::Store;

    fn ctx(tag: &str) -> ToolCtx {
        let dir = std::env::temp_dir().join(format!("lw-remtool-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _ = std::fs::remove_file(dir.join("t.db"));
        let store = Store::open(&dir.join("t.db")).unwrap();
        store.users.ensure_default_user().unwrap();
        let conv = store.chat.create_conversation(1, "companion").unwrap();
        ToolCtx {
            user_id: 1,
            conv_id: conv.id,
            media: MediaRuntime::detached(store.clone()),
            store,
            web: None,
        }
    }

    fn tomorrow_str() -> String {
        (chrono::Local::now() + chrono::Duration::days(1)).format("%Y-%m-%d 08:00").to_string()
    }

    #[tokio::test]
    async fn set_list_cancel_roundtrip_remind_mode() {
        let ctx = ctx("rt");
        let out = ReminderSet::new()
            .run(
                serde_json::json!({"content": "提醒吃降压药(饭后配水)", "first_at": tomorrow_str()}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.contains("#1") && out.contains("一次") && out.contains("这个对话里"));

        let jobs = ctx.store.jobs.list_pending(1).unwrap();
        assert_eq!(jobs[0].conv_id, ctx.conv_id, "remind 模式落当前会话");

        let list = ReminderList::new().run(serde_json::json!({}), &ctx).await.unwrap();
        assert!(list.contains("降压药"));

        assert_eq!(
            ReminderCancel::new().run(serde_json::json!({"id": 1}), &ctx).await.unwrap(),
            "ok"
        );
        assert!(ReminderList::new()
            .run(serde_json::json!({}), &ctx)
            .await
            .unwrap()
            .contains("没有定着"));
    }

    #[tokio::test]
    async fn task_mode_creates_dedicated_conversation() {
        let ctx = ctx("task");
        ReminderSet::new()
            .run(
                serde_json::json!({
                    "content": "搜索今日要闻并总结成五条",
                    "first_at": tomorrow_str(),
                    "repeat": "daily",
                    "mode": "task"
                }),
                &ctx,
            )
            .await
            .unwrap();
        let job = &ctx.store.jobs.list_pending(1).unwrap()[0];
        assert_ne!(job.conv_id, ctx.conv_id, "task 模式建专属会话");
        let conv = ctx.store.chat.get_conversation(job.conv_id).unwrap().unwrap();
        assert!(!conv.title.is_empty(), "专属会话有标题");
        assert_eq!(job.repeat, "daily");
    }

    #[tokio::test]
    async fn rejects_past_time_and_bad_repeat() {
        let ctx = ctx("bad");
        let past = ReminderSet::new()
            .run(
                serde_json::json!({"content": "x", "first_at": "2020-01-01 08:00"}),
                &ctx,
            )
            .await;
        assert!(past.is_err() && format!("{:#}", past.unwrap_err()).contains("已经过去"));

        let bad = ReminderSet::new()
            .run(
                serde_json::json!({"content": "x", "first_at": tomorrow_str(), "repeat": "hourly"}),
                &ctx,
            )
            .await;
        assert!(bad.is_err());
    }

    #[test]
    fn parse_local_accepts_minute_and_second_precision() {
        let a = parse_local("2030-01-02 08:30").unwrap();
        let b = parse_local("2030-01-02 08:30:00").unwrap();
        assert_eq!(a, b, "带秒 :00 与不带秒同刻");
        let c = parse_local("2030-01-02 08:30:45").unwrap();
        assert_eq!(c - a, 45_000, "秒被真正解析进 first_at(短提醒精度)");
        assert!(parse_local("2030-01-02 08:30:99").is_err(), "非法秒被拒");
    }

    #[tokio::test]
    async fn rejects_overlong_content() {
        let ctx = ctx("overlong");
        let too_long = "九".repeat(CONTENT_MAX_CHARS + 50);
        // 内容超长 → 退回错误,不静默截断(§3.5)
        let r = ReminderSet::new()
            .run(serde_json::json!({"content": too_long, "first_at": tomorrow_str()}), &ctx)
            .await;
        assert!(r.is_err());
    }
}
