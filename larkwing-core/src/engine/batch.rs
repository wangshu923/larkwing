//! 分批干活(batch)的 engine 半边:`tools::batch::BatchRunner` 的实现(subagent.rs 的镜像)。
//!
//! 一批 = 一张票据 = 一张 HUD 卡 = 一次汇报(§7.1「一票一报」不变)。内层工具用父回合**同一份**
//! ToolCtx 跑(同会话 / 同授权缓存 / 同确认通道),只多一个 `in_batch` 落点:会转后台的工具据此
//! 直接跑到底、不另开票据不另起卡,进度打到组上。两阶段同 delegate / ffmpeg:`IN_TURN_WAIT` 内
//! 全跑完当场回汇总(不进 report job);没跑完转 bgtasks 接着跑,返回「已转后台(编号 N)」。
//!
//! **撞限自适应**(2026-10-08 用户问「parallel 4 会不会撞 nvenc 上限,工具能不能判断」→ 能):内层
//! 工具把「并发 / 配额撞限」用 `bgtasks::CapacityHit` 挂在错误链上,工作池见它**不算失败** ——
//! 并行降到「撞限那一刻其他在跑的件数」(至少 1)、这件重排;学到的上限按工具名记进 `Engine.batch_caps`,
//! 之后含该工具的批按它封顶。不查显卡 / 驱动的表(漂),不事前探测(白耗显卡)。
//! 常量单源在 `tools/batch.rs`。

use std::collections::{HashSet, VecDeque};
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures_util::stream::{FuturesUnordered, StreamExt};

use super::*;
use crate::bgtasks::{Beat, BgTicket, CapacityHit};
use crate::tasks::TaskHandle;
use crate::tools::batch::{
    BatchCall, BatchRunner, BatchSpec, BATCH_ITEM_RESULT_MAX_CHARS, BATCH_REPORT_MAX_CHARS,
};
use crate::tools::{Tool, ToolCtx};

struct EngineBatchRunner {
    engine: std::sync::Weak<Engine>,
}

#[async_trait::async_trait]
impl BatchRunner for EngineBatchRunner {
    async fn run(&self, ctx: &ToolCtx, spec: BatchSpec) -> anyhow::Result<String> {
        let Some(engine) = self.engine.upgrade() else {
            anyhow::bail!("引擎正在关闭,这批活没派出去");
        };
        engine.run_batch(ctx, spec).await
    }
}

/// 一件的结局。`Busy` 只在工作池内部流转(撞限 → 重排),终局里不会出现(降到 1 路仍撞 → `Err`)。
#[derive(Debug, Clone)]
pub(crate) enum ItemOutcome {
    Ok(String),
    Err(String),
    Cancelled,
    Busy,
}

/// 工作池的调度备注(进汇报,模型据此不把撞限学成「这活干不了」)。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PoolNote {
    /// 起手就按学到的上限封了顶(这批没撞,是上一批学的)。
    pub seeded: Option<usize>,
    /// 中途撞限降到几路。
    pub downshift: Option<usize>,
    /// 因撞限重排了几件次。
    pub requeued: usize,
}

/// 降到 1 路仍撞限的终局话术(编码器多半被别的程序占满了)。
const BUSY_FINAL: &str = "撞上并发 / 配额上限,降到 1 路重试仍起不来(多半被别的程序占着),这件没做";

/// 整批的进度落点:内层工具的 beat / 查取消都打到这里。卡从第一秒起就有(停止钮 = 取消令牌);
/// 转后台后再加票据(`bind_bg`),beat 顺带打到票据上(看门狗据此判活)。
struct BatchScope {
    n: usize,
    done: AtomicUsize,
    token: CancellationToken,
    ticket: Mutex<Option<BgTicket>>,
    task: Mutex<Option<TaskHandle>>,
}

impl BatchScope {
    fn progress(&self, current: &str) {
        let done = self.done.load(Ordering::Relaxed);
        let k = (done + 1).min(self.n);
        if let Some(t) = self.ticket.lk().as_ref() {
            t.beat(done, format!("第 {k}/{} 件 {current}", self.n));
        }
        if let Some(task) = self.task.lk().as_ref() {
            task.step_progress(
                "step.batch",
                serde_json::json!({ "k": k, "n": self.n, "cur": current }),
                done as f32 / self.n.max(1) as f32,
            );
        }
    }

    /// 收卡:全成 → 绿;有停 → 「按要求停下了」;有败 → 「N 件没成(共 M 件)」。
    fn finish_task(&self, outs: &[ItemOutcome]) {
        let Some(task) = self.task.lk().take() else { return };
        let fail = outs.iter().filter(|o| !matches!(o, ItemOutcome::Ok(_))).count();
        let cancelled = outs.iter().filter(|o| matches!(o, ItemOutcome::Cancelled)).count();
        if fail == 0 {
            task.done();
        } else if cancelled > 0 && self.token.is_cancelled() {
            task.fail("task.err.cancelled", serde_json::Value::Null);
        } else {
            task.fail("task.err.batch", serde_json::json!({ "fail": fail, "total": outs.len() }));
        }
    }
}

impl Beat for BatchScope {
    fn beat(&self, _inner_done: usize, current: String) {
        self.progress(&current);
    }

    fn is_cancelled(&self) -> bool {
        self.token.is_cancelled() || self.ticket.lk().as_ref().is_some_and(|t| t.is_cancelled())
    }
}

/// 跑一件(内层工具用父回合同一份 ctx + 批落点):取消令牌 select,丢 future = ffmpeg kill_on_drop /
/// 阻塞线程见旗停。不设逐件超时:内层工具各有自己的网络 / 子进程超时,长活靠 beat 喂看门狗
/// (10 分钟无进展判卡,与自己开票据的路同一口径)。错误链带 `CapacityHit` = 撞限 → `Busy`。
async fn run_item(
    face: &[Arc<dyn Tool>],
    call: &BatchCall,
    ctx: &ToolCtx,
    token: &CancellationToken,
) -> ItemOutcome {
    let Some(tool) = face.iter().find(|t| t.spec().name == call.tool) else {
        return ItemOutcome::Err(format!("没有叫 {} 的工具", call.tool));
    };
    tokio::select! {
        _ = token.cancelled() => ItemOutcome::Cancelled,
        // 纯文本 run:批里不收图(汇总是文本;要看图单独调 read_image)
        r = tool.run(call.args.clone(), ctx) => match r {
            Ok(text) => ItemOutcome::Ok(text),
            Err(e) => {
                if e.chain().any(|c| c.is::<CapacityHit>()) {
                    tracing::info!(tool = %call.tool, "批内一件撞上并发上限,降并行重排");
                    return ItemOutcome::Busy;
                }
                let full = crate::net::scrub(&e);
                tracing::warn!(tool = %call.tool, "批内一件出错: {full}");
                ItemOutcome::Err(full.chars().take(500).collect())
            }
        },
    }
}

/// 工作池调度(与工具无关的纯调度逻辑,单测用假 runner 驱动):`parallel` 路并发从队列取件,逐件
/// 终局经 `tx` 回报。撞限(`Busy`)的件**不算失败**:并行降到「撞限那一刻其他在跑的件数」(至少 1),
/// 这件重排 —— 并行还在降就一直重排,降到 1 路仍撞才当真失败(每件最多重排「降档次数 + 1」回,
/// 有界)。`on_downshift(件号, 新并行)` 在每次降档时回调(engine 据此按工具名记上限)。
pub(crate) async fn run_pool<F, Fut>(
    n: usize,
    parallel: usize,
    run: F,
    tx: mpsc::Sender<(usize, ItemOutcome)>,
    mut on_downshift: impl FnMut(usize, usize),
) -> PoolNote
where
    F: Fn(usize) -> Fut,
    Fut: Future<Output = ItemOutcome>,
{
    let mut note = PoolNote::default();
    let mut queue: VecDeque<usize> = (0..n).collect();
    let mut limit = parallel.clamp(1, n.max(1));
    let mut retried: HashSet<usize> = HashSet::new();
    let mut inflight = FuturesUnordered::new();
    loop {
        while inflight.len() < limit {
            let Some(i) = queue.pop_front() else { break };
            let fut = run(i);
            inflight.push(async move { (i, fut.await) });
        }
        let Some((i, outcome)) = inflight.next().await else { break };
        match outcome {
            ItemOutcome::Busy => {
                // 自己已出列:此刻其他在跑的件数 = 这台机器眼下吃得下的量(至少留 1 路)
                let new_limit = inflight.len().max(1);
                let decreased = new_limit < limit;
                if decreased {
                    limit = new_limit;
                    note.downshift = Some(limit);
                    on_downshift(i, limit);
                }
                if decreased || retried.insert(i) {
                    note.requeued += 1;
                    queue.push_back(i);
                } else {
                    let _ = tx.send((i, ItemOutcome::Err(BUSY_FINAL.into()))).await;
                }
            }
            other => {
                let _ = tx.send((i, other)).await;
            }
        }
    }
    note
}

/// 汇总(纯函数,单测钉着):头一行成 / 败 / 停 + 调度备注,逐件一行(✓ / ✗ / ■ + 结果首段按字截),
/// 整份按 `BATCH_REPORT_MAX_CHARS` 封顶如实说明(§7.2 量约束)。回合内当场回与转后台的汇报同一形。
pub(crate) fn compose_summary(
    title: &str,
    calls: &[BatchCall],
    outs: &[ItemOutcome],
    note: &PoolNote,
) -> String {
    let n = calls.len();
    let ok = outs.iter().filter(|o| matches!(o, ItemOutcome::Ok(_))).count();
    let cancelled = outs.iter().filter(|o| matches!(o, ItemOutcome::Cancelled)).count();
    let fail = n.saturating_sub(ok + cancelled);
    let mut s = format!("一批「{title}」{n} 件跑完了:成 {ok} / 败 {fail}");
    if cancelled > 0 {
        s.push_str(&format!(" / 停 {cancelled}"));
    }
    s.push('。');
    if let Some(limit) = note.downshift {
        s.push_str(&format!(
            "中途撞上并发上限(显卡编码器一次吃不下这么多路),已自动降到 {limit} 路、把撞限的 {} 件次重跑;\
             这台机器之后含同类活的批默认按 {limit} 路,不用你改参数。",
            note.requeued
        ));
    } else if let Some(limit) = note.seeded {
        s.push_str(&format!("(按这台机器学到的上限 {limit} 路跑。)"));
    }
    for (i, (c, o)) in calls.iter().zip(outs).enumerate() {
        let (mark, text) = match o {
            ItemOutcome::Ok(t) => ("✓", t.as_str()),
            ItemOutcome::Err(e) => ("✗", e.as_str()),
            ItemOutcome::Cancelled => ("■", "按要求停下了,没做"),
            ItemOutcome::Busy => ("✗", BUSY_FINAL), // 终局里不该出现,防御性兜一行
        };
        let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
        s.push_str(&format!(
            "\n#{} {} {mark} {}",
            i + 1,
            c.tool,
            crate::text::clip(&flat, BATCH_ITEM_RESULT_MAX_CHARS, "…")
        ));
    }
    if fail > 0 {
        s.push_str("\n失败的照上面的原因处理(改参数只重派那几件,或如实告诉用户)。");
    }
    crate::text::clip(&s, BATCH_REPORT_MAX_CHARS, "…(汇报太长截了尾巴)")
}

impl Engine {
    /// batch 适配器工厂(sub_agent 同款):weak 还没回填 → None,ToolCtx.batch = None → 工具如实退回。
    pub(super) fn batch_runner(&self) -> Option<Arc<dyn BatchRunner>> {
        let weak = self.weak_self.get()?.clone();
        Some(Arc::new(EngineBatchRunner { engine: weak }))
    }

    /// 跑一批(batch 的 engine 半边,§6.5「分批干活」)。工具面 = 父回合会话的场景白名单 ∩ 排除表之外
    /// (排除表工具层已拒,这里管「没这个工具」);全部件名先校验再动手,有错整批退回让模型改。
    async fn run_batch(&self, ctx: &ToolCtx, spec: BatchSpec) -> anyhow::Result<String> {
        let BatchSpec { title, parallel, calls } = spec;
        let n = calls.len();

        let conv_id = ctx.conv_id;
        let store = self.store.clone();
        let conv = tokio::task::spawn_blocking(move || store.chat.get_conversation(conv_id))
            .await
            .map_err(anyhow::Error::from)??;
        let scene_id = conv.map(|c| c.scene_id).unwrap_or_else(|| DEFAULT_SCENE_ID.into());
        let scene = self.scenes.get(&scene_id).unwrap_or_else(|| self.scenes.default_scene()).clone();
        let face: Arc<Vec<Arc<dyn Tool>>> = Arc::new(
            self.tools
                .subset(&scene.tools)
                .into_iter()
                .filter(|t| crate::tools::batch::allowed_in_batch(t.spec().name))
                .collect(),
        );
        let mut unknown: Vec<&str> = calls
            .iter()
            .map(|c| c.tool.as_str())
            .filter(|name| !face.iter().any(|t| t.spec().name == *name))
            .collect();
        if !unknown.is_empty() {
            unknown.sort_unstable();
            unknown.dedup();
            anyhow::bail!(
                "这些工具不存在或当前用不了:{}。改成真实的工具名再派(整批都没动)。",
                unknown.join("、")
            );
        }

        // 学到的上限封顶(撞限自适应):含某工具的批,parallel 不超过这台机器为它学到的路数
        let learned = {
            let caps = self.batch_caps.lk();
            calls.iter().filter_map(|c| caps.get(&c.tool).copied()).min()
        };
        let mut note = PoolNote::default();
        let parallel = match learned {
            Some(cap) if cap < parallel => {
                note.seeded = Some(cap);
                cap
            }
            _ => parallel,
        };

        // HUD 卡从第一秒起就有(停止钮 = 取消令牌;转后台后再 bind_bg)
        let token = CancellationToken::new();
        let task = self
            .media
            .tasks()
            .start("batch", crate::bus::Text::with("task.batch", serde_json::json!({ "t": title })));
        task.bind_cancel(token.clone());
        let scope = Arc::new(BatchScope {
            n,
            done: AtomicUsize::new(0),
            token: token.clone(),
            ticket: Mutex::new(None),
            task: Mutex::new(Some(task)),
        });
        scope.progress("");
        // 父回合取消 / 工具超时 → 本 future 被 drop → 顺手掐整批(转后台时解除武装)
        let cancel_guard = token.clone().drop_guard();

        // 内层 ctx:同会话 / 同授权缓存 / 同确认通道;in_batch = 整批落点;不嵌套(batch / agent 置空)
        let wctx = Arc::new(ToolCtx {
            user_id: ctx.user_id,
            conv_id: ctx.conv_id,
            store: ctx.store.clone(),
            media: ctx.media.clone(),
            web: ctx.web.clone(),
            voice: ctx.voice.clone(),
            confirm: ctx.confirm.clone(),
            grants: ctx.grants.clone(),
            agent: None,
            batch: None,
            in_batch: Some(scope.clone() as Arc<dyn Beat>),
        });

        // 工作池:动态并行(撞限降档重排),逐件终局经 channel 回报(同步段与后台段共用同一个接收端)
        let (tx, mut rx) = mpsc::channel::<(usize, ItemOutcome)>(n.max(1));
        let calls = Arc::new(calls);
        let pool = {
            // 降档回调:按撞限那件的工具名记这台机器的上限(只降不升;重启重学)
            let (caps, names) = (self.batch_caps.clone(), calls.clone());
            let on_downshift = move |i: usize, limit: usize| {
                let mut caps = caps.lk();
                let e = caps.entry(names[i].tool.clone()).or_insert(limit);
                *e = (*e).min(limit);
            };
            let (pcalls, face, wctx, token) = (calls.clone(), face.clone(), wctx.clone(), token.clone());
            let run = move |i: usize| {
                let (calls, face, wctx, token) = (pcalls.clone(), face.clone(), wctx.clone(), token.clone());
                async move { run_item(&face, &calls[i], &wctx, &token).await }
            };
            tokio::spawn(run_pool(n, parallel, run, tx, on_downshift))
        };
        let pool_abort = pool.abort_handle();

        // —— 同步段:IN_TURN_WAIT 内全跑完当场回汇总 ——
        let mut outcomes: Vec<Option<ItemOutcome>> = vec![None; n];
        let mut finished = 0usize;
        let deadline = tokio::time::Instant::now() + crate::media::IN_TURN_WAIT;
        let mut pool_alive = true;
        while finished < n {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Err(_) => break, // 等不完 → 转后台接着跑(不作废)
                Ok(Some((i, o))) => {
                    outcomes[i] = Some(o);
                    finished += 1;
                    scope.done.store(finished, Ordering::Relaxed);
                    scope.progress("");
                }
                Ok(None) => {
                    pool_alive = false;
                    break;
                }
            }
        }
        if finished >= n {
            if let Ok(pool_note) = pool.await {
                note.downshift = pool_note.downshift;
                note.requeued = pool_note.requeued;
            }
            let outs: Vec<ItemOutcome> = outcomes
                .into_iter()
                .map(|o| o.unwrap_or_else(|| ItemOutcome::Err("没拿到结果".into())))
                .collect();
            scope.finish_task(&outs);
            return Ok(compose_summary(&title, &calls, &outs, &note));
        }
        if !pool_alive {
            scope.finish_task(&[ItemOutcome::Err(String::new())]);
            anyhow::bail!("这批活的执行池挂了(内部错误),没拿到结果;可以重派一次");
        }

        // —— 转后台:注册 bgtasks(〔此刻〕/ task_status / 取消 / 看门狗 / 收尾唤回合全白拿) ——
        let ticket = match self.media.bg().submit(format!("成批:{title}"), (ctx.user_id, ctx.conv_id), n) {
            Ok(t) => t,
            Err(e) => {
                // 后台满:掐掉整批(guard drop 即取消),如实退回
                token.cancel();
                scope.finish_task(&[ItemOutcome::Cancelled]);
                anyhow::bail!(
                    "{e:#};这批活半分钟没跑完、想转后台接着跑但后台满了,这次先停了,\
                     等几件后台事跑完再派。"
                );
            }
        };
        let bg_id = ticket.id();
        if let Some(task) = scope.task.lk().as_ref() {
            task.bind_bg(bg_id);
        }
        ticket.beat(finished, format!("第 {}/{n} 件", (finished + 1).min(n)));
        *scope.ticket.lk() = Some(ticket);
        let _ = cancel_guard.disarm(); // 父回合收尾 / 取消不再级联;停这批 = task_cancel / HUD 停止钮

        // 后台看守:收剩下的结果,全到齐就收尾汇报;每 2s 查旗标(HUD 停止钮 / task_cancel 拨的是
        // bgtasks 协作旗标,翻译成整批的取消令牌)。interval 不随事件重置(subagent 同款教训)。
        let (scope_bg, calls_bg, title_bg, token_bg) = (scope.clone(), calls, title, token.clone());
        tokio::spawn(async move {
            let mut outcomes = outcomes;
            let mut finished = finished;
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(2));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            while finished < n {
                tokio::select! {
                    msg = rx.recv() => match msg {
                        Some((i, o)) => {
                            outcomes[i] = Some(o);
                            finished += 1;
                            scope_bg.done.store(finished, Ordering::Relaxed);
                            scope_bg.progress("");
                        }
                        None => break, // 池子没了(看门狗 abort / panic):缺的按半路断计
                    },
                    _ = tick.tick() => {}
                }
                if scope_bg.is_cancelled() {
                    token_bg.cancel();
                }
            }
            let cancelled = token_bg.is_cancelled();
            let mut note = note;
            if let Ok(pool_note) = pool.await {
                note.downshift = pool_note.downshift;
                note.requeued = pool_note.requeued;
            }
            let outs: Vec<ItemOutcome> = outcomes
                .into_iter()
                .map(|o| {
                    o.unwrap_or_else(|| {
                        if cancelled {
                            ItemOutcome::Cancelled
                        } else {
                            ItemOutcome::Err("没拿到结果(半路断了)".into())
                        }
                    })
                })
                .collect();
            let ok_all = outs.iter().all(|o| matches!(o, ItemOutcome::Ok(_)));
            let summary = compose_summary(&title_bg, &calls_bg, &outs, &note);
            scope_bg.finish_task(&outs);
            if let Some(t) = scope_bg.ticket.lk().take() {
                t.finish(ok_all, summary);
            }
        });
        self.media.bg().attach_abort(bg_id, pool_abort);
        Ok(format!(
            "这批活({n} 件)半分钟内没跑完,已转后台接着跑(编号 {bg_id}),跑完会一次性回来汇报 \
             —— 先接着干别的;要中途停下用 task_cancel。"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(tool: &str) -> BatchCall {
        BatchCall { tool: tool.into(), args: serde_json::json!({}) }
    }

    #[test]
    fn summary_counts_marks_and_clips() {
        let calls = vec![call("ffmpeg_run"), call("ffmpeg_run"), call("web_download")];
        let outs = vec![
            ItemOutcome::Ok("加工完成:E:\\out\\第01集.mp4(1.2GB)。\n输入原件没动".into()),
            ItemOutcome::Err("ffmpeg 退出码 1:\n乱七八糟的 stderr".into()),
            ItemOutcome::Cancelled,
        ];
        let s = compose_summary("去广告第一批", &calls, &outs, &PoolNote::default());
        assert!(s.starts_with("一批「去广告第一批」3 件跑完了:成 1 / 败 1 / 停 1。"), "{s}");
        assert!(s.contains("#1 ffmpeg_run ✓ 加工完成:E:\\out\\第01集.mp4(1.2GB)。 输入原件没动"), "换行拍扁:{s}");
        assert!(s.contains("#2 ffmpeg_run ✗ ffmpeg 退出码 1: 乱七八糟的 stderr"), "{s}");
        assert!(s.contains("#3 web_download ■ 按要求停下了"), "{s}");
        assert!(s.contains("失败的照上面的原因处理"), "有败才带处置提示:{s}");
        assert!(!s.contains("并发上限"), "没撞限不提:{s}");

        // 逐件按字截 + 整份封顶如实说明
        let long = "字".repeat(BATCH_ITEM_RESULT_MAX_CHARS + 50);
        let calls: Vec<BatchCall> = (0..30).map(|_| call("fs_read_text")).collect();
        let outs: Vec<ItemOutcome> = (0..30).map(|_| ItemOutcome::Ok(long.clone())).collect();
        let s = compose_summary("读一堆", &calls, &outs, &PoolNote::default());
        assert!(s.chars().count() <= BATCH_REPORT_MAX_CHARS + 20, "整份封顶:{}", s.chars().count());
        assert!(s.ends_with("…(汇报太长截了尾巴)"), "{}", &s[s.len().saturating_sub(60)..]);
        assert!(!s.contains("失败的照上面"), "全成不带处置提示");
    }

    /// 调度备注进汇报:撞限降档说「降到 N 路、重跑 M 件次、之后默认 N 路」;只封顶没撞说「按学到的上限」。
    #[test]
    fn summary_explains_downshift_and_seed() {
        let calls = vec![call("ffmpeg_run"); 2];
        let outs = vec![ItemOutcome::Ok("a".into()), ItemOutcome::Ok("b".into())];
        let s = compose_summary("剪", &calls, &outs, &PoolNote { seeded: None, downshift: Some(3), requeued: 2 });
        assert!(s.contains("已自动降到 3 路") && s.contains("2 件次重跑") && s.contains("默认按 3 路"), "{s}");
        let s = compose_summary("剪", &calls, &outs, &PoolNote { seeded: Some(3), downshift: None, requeued: 0 });
        assert!(s.contains("按这台机器学到的上限 3 路跑"), "{s}");
    }

    /// 假 runner 模拟一台「一次只吃得下 2 路」的机器:第 3 路起手即 Busy。4 路派 6 件 →
    /// 全部最终成功、并行降到 2、撞限的件被重排,on_downshift 收到 2。
    #[tokio::test(flavor = "multi_thread")]
    async fn pool_downshifts_on_capacity_hit_and_requeues() {
        const CAP: usize = 2;
        let active = Arc::new(AtomicUsize::new(0));
        let (tx, mut rx) = mpsc::channel(8);
        let seen = Arc::new(Mutex::new(Vec::<usize>::new()));
        let seen_cb = seen.clone();
        let run = {
            let active = active.clone();
            move |i: usize| {
                let active = active.clone();
                async move {
                    if active.fetch_add(1, Ordering::SeqCst) + 1 > CAP {
                        active.fetch_sub(1, Ordering::SeqCst);
                        return ItemOutcome::Busy;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(15)).await;
                    active.fetch_sub(1, Ordering::SeqCst);
                    ItemOutcome::Ok(format!("done {i}"))
                }
            }
        };
        let note = run_pool(6, 4, run, tx, move |_i, limit| seen_cb.lk().push(limit)).await;
        let mut got = Vec::new();
        while let Ok((i, o)) = rx.try_recv() {
            got.push((i, o));
        }
        assert_eq!(got.len(), 6, "六件都有终局");
        assert!(got.iter().all(|(_, o)| matches!(o, ItemOutcome::Ok(_))), "撞限的件重排后全成:{got:?}");
        assert_eq!(note.downshift, Some(CAP), "降到机器真实吃得下的路数");
        assert!(note.requeued >= 1, "撞限的件至少重排过一次");
        assert_eq!(seen.lk().last().copied(), Some(CAP), "降档回调最后一次 = 真实上限");
    }

    /// 编码器被别的程序占满(永远 Busy):降到 1 路重试仍撞 → 当真失败,有界、不死循环。
    #[tokio::test(flavor = "multi_thread")]
    async fn pool_gives_up_after_retry_at_one_lane() {
        let (tx, mut rx) = mpsc::channel(8);
        let note = run_pool(2, 2, |_i| async { ItemOutcome::Busy }, tx, |_, _| {}).await;
        let mut got = Vec::new();
        while let Ok(x) = rx.try_recv() {
            got.push(x);
        }
        assert_eq!(got.len(), 2);
        assert!(
            got.iter().all(|(_, o)| matches!(o, ItemOutcome::Err(e) if e.contains("降到 1 路重试仍起不来"))),
            "{got:?}"
        );
        assert_eq!(note.downshift, Some(1));
    }
}
