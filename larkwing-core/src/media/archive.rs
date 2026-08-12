//! 压缩包差事(fs_unzip / fs_zip 的执行侧):格式引擎在 `crate::archive`(纯同步),
//! 这里管「回合内 30s 窗 → 转后台」的通用节奏(edit.rs 同款):bgtasks 票据(〔此刻〕/
//! 进度 / task_cancel / 收尾唤回合汇报 / 卡死看门狗)+ HUD 任务条。
//!
//! 取消粒度 = 条目之间(引擎每个条目查一次旗标,「正在做的那一项做完就停」= bgtasks
//! 既定语义);**半成品收拾在阻塞闭包里兜**——解压目标恒为全新目录、打包恒为临时件,
//! 取消/失败整个撤掉,谁等它(回合内/后台/看门狗 abort)都不影响收拾。看门狗把守望
//! 任务掀了时,`CancelOnDrop` 把取消旗递给还在跑的阻塞线程,它自己收尾。

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::archive as engine;
use crate::bus::Text;
use crate::files::human_size;

use super::edit::IN_TURN_WAIT;
use super::MediaRuntime;

pub enum ExtractOutcome {
    /// 回合内解完:报告 + 落在哪个文件夹。
    Done(engine::ExtractReport, PathBuf),
    /// 转后台了(bgtasks 可见可停;收尾自动唤回合汇报)。
    Background { title: String },
}

pub enum ZipOutcome {
    /// 回合内打完:成品路径 + 大小 + 条数;note = 跳过说明(符号链接等,空 = 没跳过)。
    Done { path: PathBuf, files: usize, bytes: u64, note: String },
    Background { title: String },
}

/// 看门狗/panic 把守望任务掀了 → 把取消旗递给阻塞线程(它见旗自清半成品)。
struct CancelOnDrop(Arc<engine::Progress>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel.store(true, Ordering::Relaxed);
    }
}

impl MediaRuntime {
    /// 解压到 `dest`(调用方已 dedupe 的**全新**目录;这里负责创建)。
    /// `origin` = (user_id, conv_id):转后台时收尾汇报靠它唤回合。
    pub async fn extract_archive(
        &self,
        archive: PathBuf,
        dest: PathBuf,
        password: Option<String>,
        origin: (i64, i64),
    ) -> Result<ExtractOutcome> {
        // —— 盘点(快:只读元数据):格式 / 条数 / 总量 / 要不要密码,问题全在动手前退回 ——
        let (p_arch, p_pw) = (archive.clone(), password.clone());
        let (format, ov) = tokio::task::spawn_blocking(move || {
            let format = engine::detect_format(&p_arch)?;
            let ov = engine::preflight(&p_arch, format, p_pw.as_deref())?;
            Ok::<_, anyhow::Error>((format, ov))
        })
        .await
        .context("盘点压缩包的任务挂了")??;
        anyhow::ensure!(
            !ov.needs_password,
            "这个 {} 包带密码——问用户要密码,拿到后带 password 参数重试",
            format.label()
        );
        anyhow::ensure!(ov.entries > 0, "包里没有文件(空包,或只有空目录)");
        anyhow::ensure!(
            ov.total_bytes <= engine::ARCHIVE_MAX_BYTES,
            "解开约有 {},超过 {} 上限——太大了,如实告诉用户",
            human_size(ov.total_bytes),
            human_size(engine::ARCHIVE_MAX_BYTES)
        );
        let total = ov.entries;

        std::fs::create_dir_all(&dest)
            .with_context(|| format!("建不出目录 {}", dest.display()))?;
        let prog = Arc::new(engine::Progress::default());
        let (w_arch, w_dest, w_pw, w_prog) =
            (archive.clone(), dest.clone(), password.clone(), prog.clone());
        let mut work = tokio::task::spawn_blocking(move || {
            let r = engine::extract(&w_arch, format, &w_dest, w_pw.as_deref(), &w_prog);
            // 半成品收拾:失败或取消 = 整个新文件夹撤掉(目标恒为全新目录,整删安全)
            let broken = match &r {
                Ok(rep) => rep.cancelled,
                Err(_) => true,
            };
            if broken {
                let _ = std::fs::remove_dir_all(&w_dest);
            }
            r
        });

        // —— 回合内窗:解完当场回;没解完转后台 ——
        tokio::select! {
            res = &mut work => {
                let rep = res.context("解压任务挂了")??;
                anyhow::ensure!(!rep.cancelled, "解压被取消了");
                return Ok(ExtractOutcome::Done(rep, dest));
            }
            _ = tokio::time::sleep(IN_TURN_WAIT) => {}
        }

        let name = dest
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "解压".into());
        let ticket = match self.inner.bg.submit(format!("解压({name})"), origin, total) {
            Ok(t) => t,
            Err(e) => {
                prog.cancel.store(true, Ordering::Relaxed); // 阻塞线程见旗自清半成品
                return Err(anyhow::anyhow!(
                    "{e:#};解压半分钟没完、想转后台接着跑但后台满了,这次先停了\
                     (解到一半的已清理),等几件后台事跑完再来"
                ));
            }
        };
        let ticket_id = ticket.id();
        let task = self.inner.tasks.start("archive", Text::new("task.archive"));
        let (bg_name, dest_disp) = (name.clone(), dest.display().to_string());
        let watch_prog = prog.clone();
        let join = tokio::spawn(async move {
            let _guard = CancelOnDrop(watch_prog.clone());
            let mut last = 0usize;
            loop {
                tokio::select! {
                    res = &mut work => {
                        match res {
                            Ok(Ok(rep)) if !rep.cancelled => {
                                task.done();
                                ticket.finish(true, format!(
                                    "解压好了:{} 个文件({}),放在 {dest_disp}。{}\
                                     原压缩包没动;把东西放哪了简短告诉用户。",
                                    rep.files,
                                    human_size(rep.bytes),
                                    rep.skipped_note()
                                ));
                            }
                            Ok(Ok(_)) => {
                                task.fail("task.err.cancelled", serde_json::Value::Null);
                                ticket.finish(false, format!(
                                    "解压({bg_name})按要求停下了,解到一半的已清理。"
                                ));
                            }
                            Ok(Err(e)) => {
                                task.fail("task.err.archive", serde_json::Value::Null);
                                ticket.finish(false, format!(
                                    "解压({bg_name})没成:{e:#}。半成品已清理,如实告诉用户。"
                                ));
                            }
                            Err(e) => {
                                task.fail("task.err.archive", serde_json::Value::Null);
                                ticket.finish(false, format!(
                                    "解压({bg_name})的任务挂了:{e}。如实告诉用户。"
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
                        let done = watch_prog.done.load(Ordering::Relaxed);
                        if done > last {
                            last = done;
                            ticket.beat(done.min(total), format!("{done}/{total}"));
                            task.step_progress(
                                "step.archive",
                                serde_json::json!({ "t": bg_name, "p": format!("{done}/{total}") }),
                                (done as f32 / total.max(1) as f32).min(0.99),
                            );
                        }
                    }
                }
            }
        });
        self.inner.bg.attach_abort(ticket_id, join.abort_handle());
        Ok(ExtractOutcome::Background { title: name })
    }

    /// 打包成 zip:`dest` = 意向成品路径(落盘前再 dedupe,永不覆盖);临时件写完改名。
    pub async fn create_zip(
        &self,
        inputs: Vec<PathBuf>,
        dest: PathBuf,
        origin: (i64, i64),
    ) -> Result<ZipOutcome> {
        let plan = tokio::task::spawn_blocking(move || engine::plan_zip(&inputs))
            .await
            .context("盘点要打包的文件挂了")??;
        anyhow::ensure!(!plan.files.is_empty(), "没有可打包的文件(空文件夹?)");
        anyhow::ensure!(
            plan.total_bytes <= engine::ARCHIVE_MAX_BYTES,
            "要打包的共约 {},超过 {} 上限——太大了,如实告诉用户",
            human_size(plan.total_bytes),
            human_size(engine::ARCHIVE_MAX_BYTES)
        );
        let total = plan.files.len();
        let plan_note = plan_skipped_note(&plan);

        let dest_dir = dest
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| anyhow::anyhow!("输出路径没有上级目录"))?;
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let tmp = dest_dir.join(format!(
            ".lw-zip-{}-{}.zip",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let prog = Arc::new(engine::Progress::default());
        let (w_tmp, w_prog) = (tmp.clone(), prog.clone());
        let mut work = tokio::task::spawn_blocking(move || {
            let r = engine::create_zip(&plan, &w_tmp, &w_prog);
            let broken = match &r {
                Ok(rep) => rep.cancelled,
                Err(_) => true,
            };
            if broken {
                let _ = std::fs::remove_file(&w_tmp);
            }
            r
        });

        tokio::select! {
            res = &mut work => {
                let rep = res.context("打包任务挂了")??;
                anyhow::ensure!(!rep.cancelled, "打包被取消了");
                let (path, bytes) = place_zip(&tmp, &dest)?;
                return Ok(ZipOutcome::Done { path, files: rep.files, bytes, note: plan_note });
            }
            _ = tokio::time::sleep(IN_TURN_WAIT) => {}
        }

        let name = dest
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "打包".into());
        let ticket = match self.inner.bg.submit(format!("打包({name})"), origin, total) {
            Ok(t) => t,
            Err(e) => {
                prog.cancel.store(true, Ordering::Relaxed);
                return Err(anyhow::anyhow!(
                    "{e:#};打包半分钟没完、想转后台但后台满了,这次先停了\
                     (半成品已清理),等几件后台事跑完再来"
                ));
            }
        };
        let ticket_id = ticket.id();
        let task = self.inner.tasks.start("pack", Text::new("task.pack"));
        let (bg_name, bg_tmp, bg_dest) = (name.clone(), tmp.clone(), dest.clone());
        let watch_prog = prog.clone();
        let join = tokio::spawn(async move {
            let _guard = CancelOnDrop(watch_prog.clone());
            let mut last = 0usize;
            loop {
                tokio::select! {
                    res = &mut work => {
                        match res {
                            Ok(Ok(rep)) if !rep.cancelled => match place_zip(&bg_tmp, &bg_dest) {
                                Ok((path, bytes)) => {
                                    task.done();
                                    ticket.finish(true, format!(
                                        "打包好了:{}({},{} 个文件)。{plan_note}\
                                         可以接着发到手机或存起来;放哪了简短告诉用户。",
                                        path.display(),
                                        human_size(bytes),
                                        rep.files
                                    ));
                                }
                                Err(e) => {
                                    task.fail("task.err.pack", serde_json::Value::Null);
                                    ticket.finish(false, format!(
                                        "打包({bg_name})最后落盘失败:{e:#}。如实告诉用户。"
                                    ));
                                }
                            },
                            Ok(Ok(_)) => {
                                task.fail("task.err.cancelled", serde_json::Value::Null);
                                ticket.finish(false, format!(
                                    "打包({bg_name})按要求停下了,半成品已清理。"
                                ));
                            }
                            Ok(Err(e)) => {
                                task.fail("task.err.pack", serde_json::Value::Null);
                                ticket.finish(false, format!(
                                    "打包({bg_name})没成:{e:#}。半成品已清理,如实告诉用户。"
                                ));
                            }
                            Err(e) => {
                                task.fail("task.err.pack", serde_json::Value::Null);
                                ticket.finish(false, format!(
                                    "打包({bg_name})的任务挂了:{e}。如实告诉用户。"
                                ));
                            }
                        }
                        break;
                    }
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {
                        if ticket.is_cancelled() {
                            watch_prog.cancel.store(true, Ordering::Relaxed);
                            continue;
                        }
                        let done = watch_prog.done.load(Ordering::Relaxed);
                        if done > last {
                            last = done;
                            ticket.beat(done.min(total), format!("{done}/{total}"));
                            task.step_progress(
                                "step.pack",
                                serde_json::json!({ "t": bg_name, "p": format!("{done}/{total}") }),
                                (done as f32 / total.max(1) as f32).min(0.99),
                            );
                        }
                    }
                }
            }
        });
        self.inner.bg.attach_abort(ticket_id, join.abort_handle());
        Ok(ZipOutcome::Background { title: name })
    }
}

/// 成品落位:dedupe(永不覆盖)→ 临时件改名(edit.rs finalize 同款,这里临时件由
/// 阻塞闭包收拾,故不带守卫)。
fn place_zip(tmp: &Path, dest: &Path) -> Result<(PathBuf, u64)> {
    let out = crate::files::dedupe_path(dest);
    std::fs::rename(tmp, &out)
        .with_context(|| format!("成品改名失败 {} -> {}", tmp.display(), out.display()))?;
    let bytes = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
    Ok((out, bytes))
}

/// 打包时跳过件(符号链接)的说明,空 = 没跳过。
fn plan_skipped_note(plan: &engine::ZipPlan) -> String {
    if plan.skipped_total == 0 {
        return String::new();
    }
    let mut names = plan.skipped.join("、");
    if plan.skipped_total > plan.skipped.len() {
        names.push_str(" 等");
    }
    format!("跳过 {} 个(不收符号链接):{names}。", plan.skipped_total)
}
