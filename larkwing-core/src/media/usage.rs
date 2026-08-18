//! 磁盘占用差事(fs_usage 的执行侧):引擎在 `crate::usage`(纯同步),这里管
//! 「回合内 30s 窗 → 转后台」的通用节奏(archive.rs 同款):bgtasks 票据(〔此刻〕/
//! 进度 / task_cancel / 收尾唤回合汇报 / 卡死看门狗)+ HUD 任务条。
//!
//! 只读活、无半成品要收拾;取消/半路死唯一要管的 = 把旗递给阻塞线程,让它别把整个盘
//! 扫完白费 IO(`CancelOnDrop` 可解除武装:正常收尾/交棒后台时不误伤)。总数未知
//! (扫描本身就是在数),bgtasks 按 total=0 提交 = 显示端不出分母,进度全靠 current 文案。

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::bus::Text;
use crate::usage as engine;

use super::edit::IN_TURN_WAIT;
use super::MediaRuntime;

pub enum UsageOutcome {
    /// 回合内扫完:报告直接回(渲染归 engine::render_report 单源)。
    Done(engine::UsageReport),
    /// 转后台了(bgtasks 可见可停;收尾自动唤回合汇报)。
    Background { title: String },
}

/// 谁把这活扔了(回合取消 drop / 看门狗 abort)→ 旗子递给阻塞线程,扫描尽快收手。
struct CancelOnDrop(Option<Arc<engine::Progress>>);

impl CancelOnDrop {
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(p) = &self.0 {
            p.cancel.store(true, Ordering::Relaxed);
        }
    }
}

impl MediaRuntime {
    /// 扫一棵目录树的占用。`origin` = (user_id, conv_id):转后台时收尾汇报靠它唤回合。
    pub async fn disk_usage(&self, root: PathBuf, origin: (i64, i64)) -> Result<UsageOutcome> {
        let prog = Arc::new(engine::Progress::default());
        let (w_root, w_prog) = (root.clone(), prog.clone());
        let mut work = tokio::task::spawn_blocking(move || engine::scan(&w_root, &w_prog));
        // 回合内窗里被取消(future 被 drop)→ 递旗停扫,别让 C:\ 级的 walk 白跑到底
        let mut guard = CancelOnDrop(Some(prog.clone()));

        tokio::select! {
            res = &mut work => {
                guard.disarm();
                let rep = res.context("磁盘扫描任务挂了")??;
                anyhow::ensure!(!rep.cancelled, "磁盘扫描被取消了");
                return Ok(UsageOutcome::Done(rep));
            }
            _ = tokio::time::sleep(IN_TURN_WAIT) => {}
        }

        let name = root
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| root.display().to_string()); // 盘根(C:\)没有末段,显整串
        let title = format!("磁盘占用({name})");
        let ticket = match self.inner.bg.submit(title.clone(), origin, 0) {
            Ok(t) => t,
            // guard 还武装着:带错返回时顺手递旗停扫
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "{e:#};这个文件夹半分钟没扫完、想转后台接着扫但后台满了,这次先停了,\
                     等几件后台事跑完再来"
                ))
            }
        };
        guard.disarm(); // 交棒:此后旗子归看守任务里的 guard 管
        let ticket_id = ticket.id();
        let task = self.inner.tasks.start("usage", Text::new("task.usage"));
        task.bind_bg(ticket_id); // HUD 可直接停(§7 停止钮通用件)
        let (bg_name, bg_root) = (name.clone(), root.clone());
        let watch_prog = prog.clone();
        let join = tokio::spawn(async move {
            let _guard = CancelOnDrop(Some(watch_prog.clone()));
            let mut last = 0usize;
            loop {
                tokio::select! {
                    res = &mut work => {
                        match res {
                            Ok(Ok(rep)) if !rep.cancelled => {
                                task.done();
                                ticket.finish(true, format!(
                                    "磁盘占用扫完了:\n{}\n把占大头的简短告诉用户。",
                                    engine::render_report(&bg_root, &rep)
                                ));
                            }
                            Ok(Ok(_)) => {
                                task.fail("task.err.cancelled", serde_json::Value::Null);
                                ticket.finish(false, format!(
                                    "磁盘占用({bg_name})按要求停下了;数字不全,没法当结论用。"
                                ));
                            }
                            Ok(Err(e)) => {
                                task.fail("task.err.usage", serde_json::Value::Null);
                                ticket.finish(false, format!(
                                    "磁盘占用({bg_name})没扫成:{e:#}。如实告诉用户。"
                                ));
                            }
                            Err(e) => {
                                task.fail("task.err.usage", serde_json::Value::Null);
                                ticket.finish(false, format!(
                                    "磁盘占用({bg_name})的任务挂了:{e}。如实告诉用户。"
                                ));
                            }
                        }
                        break;
                    }
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {
                        if ticket.is_cancelled() {
                            watch_prog.cancel.store(true, Ordering::Relaxed);
                            continue; // 等阻塞线程在条目间见旗收手,走上面的收尾臂
                        }
                        let n = watch_prog.scanned.load(Ordering::Relaxed);
                        if n > last {
                            last = n;
                            ticket.beat(n, format!("清点文件(已数到 {n} 个)"));
                            task.step("step.usage", serde_json::json!({ "t": bg_name, "p": n }));
                        }
                    }
                }
            }
        });
        self.inner.bg.attach_abort(ticket_id, join.abort_handle());
        Ok(UsageOutcome::Background { title })
    }
}
