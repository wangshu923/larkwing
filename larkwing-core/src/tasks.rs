//! 后台任务进度报告:core 里任何长活(组件下载、解析慢 URL、将来的 job)都经
//! `Tasks::start()` 拿一个句柄,边干边 `step()/progress()`,完事 `done()/fail()`。
//! 句柄被 drop 而没收尾 = 自动判 fail(防 panic 后 HUD 留一根永远转圈的僵尸条)。
//! 文案纪律:这里只有 key+params,句子在前端字典(宪法 §5 人格中立底座)。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

use crate::bus::{AppEvent, Bus, TaskState, TaskView, Text};

#[derive(Clone)]
pub struct Tasks {
    bus: Bus,
    next_id: std::sync::Arc<AtomicU64>,
}

impl Tasks {
    pub fn new(bus: Bus) -> Tasks {
        Tasks { bus, next_id: std::sync::Arc::new(AtomicU64::new(1)) }
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
        };
        self.bus.publish(AppEvent::Task(view.clone()));
        TaskHandle { bus: self.bus.clone(), view: Mutex::new(view), finished: AtomicBool::new(false) }
    }
}

pub struct TaskHandle {
    bus: Bus,
    /// 全量快照:每次更新都广播完整状态(前端 upsert,错过事件也能追平)。
    view: Mutex<TaskView>,
    finished: AtomicBool,
}

impl TaskHandle {
    fn publish(&self, f: impl FnOnce(&mut TaskView)) {
        let mut view = self.view.lock().expect("task view lock poisoned");
        f(&mut view);
        self.bus.publish(AppEvent::Task(view.clone()));
    }

    /// 到哪一步了(可只更新步骤不动进度)。
    pub fn step(&self, key: &str, params: serde_json::Value) {
        self.publish(|v| v.step = Some(Text::with(key, params)));
    }

    /// 0..=1;调用方自行节流(下载循环里别每个 chunk 都喊)。
    pub fn progress(&self, p: f32) {
        self.publish(|v| v.progress = Some(p.clamp(0.0, 1.0)));
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
        self.publish(|v| {
            v.state = TaskState::Done;
            v.progress = Some(1.0);
            v.step = None;
        });
    }

    pub fn fail(self, key: &str, params: serde_json::Value) {
        self.finished.store(true, Ordering::Relaxed);
        self.publish(|v| {
            v.state = TaskState::Failed;
            v.error = Some(Text::with(key, params));
        });
    }
}

impl Drop for TaskHandle {
    fn drop(&mut self) {
        if self.finished.load(Ordering::Relaxed) {
            return;
        }
        // 没收尾就没影了(panic / future 被取消):如实告诉 HUD,绝不留僵尸转圈条
        let mut view = self.view.lock().expect("task view lock poisoned");
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
            a.view.lock().unwrap().task_id,
            b.view.lock().unwrap().task_id,
        );
        assert_ne!(ai, bi);
        a.done();
        b.done();
    }
}
