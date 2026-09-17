//! 后台任务进度报告:core 里任何长活(组件下载、解析慢 URL、将来的 job)都经
//! `Tasks::start()` 拿一个句柄,边干边 `step()/progress()`,完事 `done()/fail()`。
//! 句柄被 drop 而没收尾 = 自动判 fail(防 panic 后 HUD 留一根永远转圈的僵尸条)。
//! 文案纪律:这里只有 key+params,句子在前端字典(宪法 §5 人格中立底座)。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;

use crate::bus::{AppEvent, Bus, TaskRetry, TaskState, TaskView, Text};
use crate::lockext::LockExt;

/// 任务卡 → 取消令牌(`bind_cancel` 登记、收尾即摘;HUD 停止钮经 `Tasks::cancel` 掐)。
type Cancels = Arc<Mutex<HashMap<u64, CancellationToken>>>;

#[derive(Clone)]
pub struct Tasks {
    bus: Bus,
    next_id: Arc<AtomicU64>,
    cancels: Cancels,
}

impl Tasks {
    pub fn new(bus: Bus) -> Tasks {
        Tasks { bus, next_id: Arc::new(AtomicU64::new(1)), cancels: Default::default() }
    }

    /// 开一个任务:立即广播 running 态(不定进度),返回句柄。
    pub fn start(&self, kind: &str, label: Text) -> TaskHandle {
        let view = TaskView {
            task_id: self.next_id.fetch_add(1, Ordering::Relaxed),
            kind: kind.into(),
            label,
            state: TaskState::Running,
            progress: None,
            step: None,
            error: None,
            retry: None,
            bg: None,
            cancellable: false,
            meta: None,
        };
        self.bus.publish(AppEvent::Task(view.clone()));
        TaskHandle {
            bus: self.bus.clone(),
            view: Mutex::new(view),
            finished: AtomicBool::new(false),
            cancels: self.cancels.clone(),
        }
    }

    /// HUD 任务卡「停止」(2026-09-17):掐这张卡挂着的取消令牌。true = 掐了;false = 卡已收尾
    /// 或没挂令牌(前端只对 `cancellable` 的卡显钮,正常到不了 false)。与 bgtasks 的协作旗标
    /// 是两把钥匙:后台 job 走 `bg_cancel`,回合内长活(解析 / 分头办事同步段)走这里。
    pub fn cancel(&self, task_id: u64) -> bool {
        match self.cancels.lk().get(&task_id) {
            Some(tok) => {
                tok.cancel();
                true
            }
            None => false,
        }
    }
}

pub struct TaskHandle {
    bus: Bus,
    /// 全量快照:每次更新都广播完整状态(前端 upsert,错过事件也能追平)。
    view: Mutex<TaskView>,
    finished: AtomicBool,
    cancels: Cancels,
}

impl TaskHandle {
    fn publish(&self, f: impl FnOnce(&mut TaskView)) {
        let mut view = self.view.lk();
        f(&mut view);
        self.bus.publish(AppEvent::Task(view.clone()));
    }

    /// 收尾即摘登记(令牌表不随任务数长;收尾后再点停止 = false,前端按终态快照已收钮)。
    fn unregister(&self) {
        let id = self.view.lk().task_id;
        self.cancels.lk().remove(&id);
    }

    /// 到哪一步了(可只更新步骤不动进度)。
    pub fn step(&self, key: &str, params: serde_json::Value) {
        self.publish(|v| v.step = Some(Text::with(key, params)));
    }

    /// 0..=1;调用方自行节流(下载循环里别每个 chunk 都喊)。
    pub fn progress(&self, p: f32) {
        self.publish(|v| v.progress = Some(p.clamp(0.0, 1.0)));
    }

    /// 挂上 bgtasks 登记处编号:HUD 据此显「停止」钮(点击直连 bg_cancel,不绕 LLM §7.1)。
    /// 有 BgTicket 的长活(批量下载/配词/BT/ffmpeg/解压/扫盘/delegate 转后台)注册完就 bind。
    pub fn bind_bg(&self, id: u64) {
        self.publish(|v| v.bg = Some(id));
    }

    /// 挂上取消令牌(2026-09-17):回合内的长活(影音解析 / 分头办事同步段…)把自己的
    /// `CancellationToken` 交进来,HUD 据此显「停止」钮,点击直连 `Tasks::cancel` 掐令牌 ——
    /// 调用方自己 select 令牌收尾(解析 = 丢 future 杀子进程;子回合 = Cancelled 收尾)。
    pub fn bind_cancel(&self, token: CancellationToken) {
        let id = self.view.lk().task_id;
        self.cancels.lk().insert(id, token);
        self.publish(|v| v.cancellable = true);
    }

    /// 附注行(统计 / 备注;key + params,句子在前端字典)。
    pub fn meta(&self, key: &str, params: serde_json::Value) {
        self.publish(|v| v.meta = Some(Text::with(key, params)));
    }

    /// 步骤 + 进度一起更新(下载场景一条事件搞定)。
    pub fn step_progress(&self, key: &str, params: serde_json::Value, p: f32) {
        self.publish(|v| {
            v.step = Some(Text::with(key, params));
            v.progress = Some(p.clamp(0.0, 1.0));
        });
    }

    pub fn done(self) {
        self.finished.store(true, Ordering::Relaxed);
        self.unregister();
        self.publish(|v| {
            v.state = TaskState::Done;
            v.progress = Some(1.0);
            v.step = None;
        });
    }

    pub fn fail(self, key: &str, params: serde_json::Value) {
        self.finished.store(true, Ordering::Relaxed);
        self.unregister();
        self.publish(|v| {
            v.state = TaskState::Failed;
            v.error = Some(Text::with(key, params));
        });
    }

    /// 失败且可重放:同 `fail`,额外带上重放入参 → 前端据此显「重试」按钮(PLAN §10)。
    pub fn fail_retryable(self, key: &str, params: serde_json::Value, retry: TaskRetry) {
        self.finished.store(true, Ordering::Relaxed);
        self.unregister();
        self.publish(|v| {
            v.state = TaskState::Failed;
            v.error = Some(Text::with(key, params));
            v.retry = Some(retry);
        });
    }
}

impl Drop for TaskHandle {
    fn drop(&mut self) {
        if self.finished.load(Ordering::Relaxed) {
            return;
        }
        // 没收尾就没影了(panic / future 被取消):如实告诉 HUD,绝不留僵尸转圈条
        self.unregister();
        let mut view = self.view.lk();
        view.state = TaskState::Failed;
        view.error = Some(Text::new("task.err.dropped"));
        self.bus.publish(AppEvent::Task(view.clone()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(rx: &mut tokio::sync::broadcast::Receiver<AppEvent>) -> Vec<TaskView> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let AppEvent::Task(t) = ev {
                out.push(t);
            }
        }
        out
    }

    #[test]
    fn lifecycle_publishes_upserts_with_same_id() {
        let bus = Bus::new();
        let mut rx = bus.subscribe();
        let tasks = Tasks::new(bus);

        let t = tasks.start("download", Text::new("task.download.ytdlp"));
        t.step_progress("step.download", serde_json::json!({"done": 1, "total": 2}), 0.5);
        t.done();

        let seen = drain(&mut rx);
        assert_eq!(seen.len(), 3);
        assert!(seen.iter().all(|v| v.task_id == seen[0].task_id), "同一任务同一 id");
        assert_eq!(seen[0].state, TaskState::Running);
        assert_eq!(seen[1].progress, Some(0.5));
        assert_eq!(seen[1].step.as_ref().unwrap().key, "step.download");
        assert_eq!(seen[2].state, TaskState::Done);
        assert!(seen[2].step.is_none(), "终态不留步骤行");
    }

    #[test]
    fn dropped_handle_fails_loudly() {
        let bus = Bus::new();
        let mut rx = bus.subscribe();
        let tasks = Tasks::new(bus);
        drop(tasks.start("resolve", Text::new("task.resolve")));
        let seen = drain(&mut rx);
        let last = seen.last().unwrap();
        assert_eq!(last.state, TaskState::Failed);
        assert_eq!(last.error.as_ref().unwrap().key, "task.err.dropped");
    }

    #[test]
    fn ids_are_unique_across_tasks() {
        let tasks = Tasks::new(Bus::new());
        let a = tasks.start("a", Text::new("x"));
        let b = tasks.start("b", Text::new("y"));
        let (ai, bi) = (
            a.view.lk().task_id,
            b.view.lk().task_id,
        );
        assert_ne!(ai, bi);
        a.done();
        b.done();
    }

    /// 取消令牌:bind 后卡标 cancellable,`Tasks::cancel` 掐得动;收尾即摘,再掐 = false。
    #[test]
    fn bind_cancel_marks_card_and_cancel_fires_until_finished() {
        let bus = Bus::new();
        let mut rx = bus.subscribe();
        let tasks = Tasks::new(bus);
        let t = tasks.start("resolve", Text::new("task.resolve"));
        let id = t.view.lk().task_id;
        let tok = CancellationToken::new();
        t.bind_cancel(tok.clone());
        t.meta("step.delegate_stats", serde_json::json!({ "calls": 3, "tokens": 1200 }));
        let seen = drain(&mut rx);
        assert!(!seen[0].cancellable && seen[1].cancellable, "bind 后快照标可取消");
        assert_eq!(seen[2].meta.as_ref().unwrap().key, "step.delegate_stats");
        assert!(!tok.is_cancelled());
        assert!(tasks.cancel(id), "挂着令牌 → 掐得动");
        assert!(tok.is_cancelled());
        t.fail("task.err.cancelled", serde_json::Value::Null);
        assert!(!tasks.cancel(id), "收尾即摘登记");
        assert!(!tasks.cancel(9999), "没挂令牌的卡 = false");
    }
}
