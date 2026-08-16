//! 后台差事登记处:长活(批量下载/批量配歌词这类分离 job)的**模型可见性**底座 ——
//! HUD 任务条(tasks.rs)只给用户眼睛看,这里补齐另一半:
//!   · 快照可查:`task_status` 工具 + 每回合〔此刻〕背景一行(零工具调用即知进度);
//!   · 可取消:`task_cancel` → 协作式旗标,任务逐项自查,正在做的那一项做完就停;
//!   · 收尾必有动静:完成/取消/半路断,一律把汇总插成 due=now 的一次性 `jobs` 任务 →
//!     调度器 ≤30s 捡起 wake_turn 唤回合,模型向用户转述(§3.5 委托的活绝不无声收场);
//!   · 卡死看门狗:超过 STALL_MS 无步进 = 判卡 + abort + 照常汇报(TaskHandle 的
//!     drop-自动-fail 只兜「崩了」,这里兜「活着但僵住」)。
//! 定位 = 通用件(§5 三物种判据),**不是**模型的手脚:长活工具各自决定走 job 模式后注册
//! 进来;绝不做通用任务提交器 / 队列 / 优先级 / DAG(§9 不复刻)。定期心跳刻意不内建 ——
//! 「每 5 分钟报一次进度」= 模型自己 reminder_set + task_status 组合(§5 正交原语)。
//! 瞬态 app 级(§6.4 三层):重启即丢,丢了只是「汇报不来」,不是出错。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::Result;

use crate::store::Store;

/// 同时在跑的后台任务上限(2026-07-27 用户拍板 20 = 失控 backstop;满了如实退回不排队)。
pub const BG_MAX_CONCURRENT: usize = 20;
/// 无步进判卡阈值:逐项工作(一首歌级)秒到分钟级,10 分钟没动静基本可断定僵死。
const STALL_MS: i64 = 10 * 60 * 1000;
/// 终态留存条数(task_status 的「刚结束的」视图;再多没有回看价值)。
const FINISHED_KEEP: usize = 10;
/// 〔此刻〕背景最多列几个运行中任务(再多只报个数,让模型用 task_status 细看)。
const AMBIENT_MAX: usize = 3;
/// 点名清单的**字数**预算(§7.2 量约束):批量汇总与 status 视图共用。
/// 按字数而不是按条数截 —— 名字长短差十倍(「两只老虎.mp3」vs 一整条绝对路径),
/// 条数封顶要么把短名单白白砍掉(2026-08-15 真机:92 首没配上,模型只拿到 12 个名字、
/// 用户问「是哪些」它答不出),要么被长路径撑爆。预算内尽量列全,超了列到预算 + 「等 N 个」。
/// 结果给全之后**怎么处置归模型**(要存下来它自己 fs_write_text 写,不给它造专门的机制)。
const NAMES_MAX_CHARS: usize = 2000;
/// 运行中视图(task_status)保留多少个没成的名字:打印仍按字数截,这里只是别无限攒。
const MISS_KEEP: usize = 300;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 点名清单:字数预算内尽量列全,装不下的以「等 N 个」交代总数(token 不随批量大小爆,
/// 但也别为省几百字把模型变成瞎子——它得答得出「到底是哪几个」)。至少列一个。
pub(crate) fn cap_names(names: &[String]) -> String {
    let mut out = String::new();
    let (mut used, mut listed) = (0usize, 0usize);
    for n in names {
        let n_len = n.chars().count();
        let add = if out.is_empty() { n_len } else { n_len + 1 }; // +1 = 顿号
        if !out.is_empty() && used + add > NAMES_MAX_CHARS {
            break;
        }
        if !out.is_empty() {
            out.push('、');
        }
        out.push_str(n);
        used += add;
        listed += 1;
    }
    if listed < names.len() {
        out.push_str(&format!(" 等 {} 个", names.len()));
    }
    out
}

#[derive(Clone)]
pub struct BgTasks {
    inner: Arc<Inner>,
}

struct Inner {
    store: Store,
    next_id: AtomicU64,
    entries: Mutex<Vec<Arc<BgEntry>>>,
    sweeper: OnceLock<()>,
}

struct BgEntry {
    id: u64,
    title: String,
    origin: (i64, i64),
    started_ms: i64,
    total: usize,
    st: Mutex<St>,
}

struct St {
    /// 已完成单元数(正在做第 done_units+1 个)。
    done_units: usize,
    /// 正在处理哪一项(人话,给〔此刻〕/status;模型面文本,不走 i18n —— 同工具结果口径)。
    current: String,
    /// 累计没成的点名(容量封顶;总数另计,防漏报)。
    misses: Vec<String>,
    miss_count: usize,
    last_beat_ms: i64,
    cancelled: bool,
    abort: Option<tokio::task::AbortHandle>,
    finished: Option<Fin>,
}

#[derive(Clone)]
struct Fin {
    ok: bool,
    summary: String,
    at_ms: i64,
}

impl BgEntry {
    fn running(&self) -> bool {
        self.st.lock().expect("bg st lock").finished.is_none()
    }
}

impl BgTasks {
    pub fn new(store: Store) -> BgTasks {
        BgTasks {
            inner: Arc::new(Inner {
                store,
                next_id: AtomicU64::new(1),
                entries: Mutex::new(Vec::new()),
                sweeper: OnceLock::new(),
            }),
        }
    }

    /// 提交一个后台任务(cap 满 = 如实退回,不排队):返回票据,任务循环拿着它
    /// 打点(beat)/查取消/收尾(finish)。spawn 之后记得 `attach_abort`(看门狗要用)。
    pub fn submit(&self, title: String, origin: (i64, i64), total: usize) -> Result<BgTicket> {
        self.ensure_sweeper();
        let now = now_ms();
        let entry = {
            let mut entries = self.inner.entries.lock().expect("bg entries lock");
            let running = entries.iter().filter(|e| e.running()).count();
            anyhow::ensure!(
                running < BG_MAX_CONCURRENT,
                "后台已有 {running} 个任务在跑(上限 {BG_MAX_CONCURRENT}),等几个跑完再提交"
            );
            let entry = Arc::new(BgEntry {
                id: self.inner.next_id.fetch_add(1, Ordering::Relaxed),
                title,
                origin,
                started_ms: now,
                total,
                st: Mutex::new(St {
                    done_units: 0,
                    current: "准备中".into(),
                    misses: Vec::new(),
                    miss_count: 0,
                    last_beat_ms: now,
                    cancelled: false,
                    abort: None,
                    finished: None,
                }),
            });
            entries.push(entry.clone());
            // 终态留存修剪(只裁已结束的,运行中永不裁)
            let finished: Vec<usize> = entries
                .iter()
                .enumerate()
                .filter(|(_, e)| !e.running())
                .map(|(i, _)| i)
                .collect();
            if finished.len() > FINISHED_KEEP {
                for i in finished[..finished.len() - FINISHED_KEEP].iter().rev() {
                    entries.remove(*i);
                }
            }
            entry
        };
        Ok(BgTicket { reg: self.clone(), entry })
    }

    /// 运行中、且标题以 `prefix` 打头的任务数。给「某一类活自己还有并发闸」用
    /// (如 BT 下载:全局 cap 20 之外,自己再限 3 个 —— 再多只是分摊带宽)。
    pub fn running_count_of(&self, prefix: &str) -> usize {
        self.inner
            .entries
            .lock()
            .expect("bg entries lock")
            .iter()
            .filter(|e| e.running() && e.title.starts_with(prefix))
            .count()
    }

    /// spawn 之后把 abort 句柄挂上(看门狗判卡后据此掐掉僵死任务)。
    pub fn attach_abort(&self, id: u64, abort: tokio::task::AbortHandle) {
        if let Some(e) = self.find(id) {
            e.st.lock().expect("bg st lock").abort = Some(abort);
        }
    }

    /// 叫停(协作式):置旗标,任务在下一项开始前自查退出。返回任务名;不在跑 = None。
    pub fn cancel(&self, id: u64) -> Option<String> {
        let e = self.find(id)?;
        let mut st = e.st.lock().expect("bg st lock");
        if st.finished.is_some() {
            return None;
        }
        st.cancelled = true;
        Some(e.title.clone())
    }

    fn find(&self, id: u64) -> Option<Arc<BgEntry>> {
        self.inner.entries.lock().expect("bg entries lock").iter().find(|e| e.id == id).cloned()
    }

    /// 〔此刻〕背景一行:运行中任务的极简摘要(带编号,模型可直接 task_cancel);
    /// 没有运行中的 = None(不占背景)。
    pub fn ambient_line(&self) -> Option<String> {
        let entries = self.inner.entries.lock().expect("bg entries lock");
        let running: Vec<&Arc<BgEntry>> = entries.iter().filter(|e| e.running()).collect();
        if running.is_empty() {
            return None;
        }
        let mut parts: Vec<String> = running
            .iter()
            .take(AMBIENT_MAX)
            .map(|e| {
                let st = e.st.lock().expect("bg st lock");
                format!(
                    "「{}」(编号{}) {}正在{}",
                    e.title,
                    e.id,
                    progress_frag(st.done_units, e.total, "", " "),
                    st.current
                )
            })
            .collect();
        if running.len() > AMBIENT_MAX {
            parts.push(format!("另有 {} 个在跑", running.len() - AMBIENT_MAX));
        }
        Some(format!("后台任务:{}", parts.join("、")))
    }

    /// task_status 的全量视图:运行中(编号/进度/当前项/已用时/累计没成点名)+ 刚结束的。
    pub fn status_report(&self) -> String {
        let now = now_ms();
        let entries = self.inner.entries.lock().expect("bg entries lock");
        let mut running = Vec::new();
        let mut finished = Vec::new();
        for e in entries.iter() {
            let st = e.st.lock().expect("bg st lock");
            match &st.finished {
                None => {
                    let mut line = format!(
                        "「{}」编号{}:{}正在{},已跑 {}",
                        e.title,
                        e.id,
                        progress_frag(st.done_units, e.total, "", ","),
                        st.current,
                        human_elapsed(now - e.started_ms)
                    );
                    if st.miss_count > 0 {
                        line.push_str(&format!(
                            ";目前没成 {} 个:{}",
                            st.miss_count,
                            cap_names(&st.misses)
                        ));
                    }
                    if st.cancelled {
                        line.push_str("(已叫停,正在收尾)");
                    }
                    running.push(line);
                }
                Some(f) => finished.push((
                    f.at_ms,
                    format!(
                        "「{}」({} 前{}):{}",
                        e.title,
                        human_elapsed(now - f.at_ms),
                        if f.ok { "" } else { ",没全成" },
                        f.summary
                    ),
                )),
            }
        }
        finished.sort_by_key(|(at, _)| -*at);
        let mut out = String::new();
        if running.is_empty() {
            out.push_str("现在没有后台任务在跑。");
        } else {
            out.push_str(&format!("运行中 {} 个:\n{}", running.len(), running.join("\n")));
        }
        if !finished.is_empty() {
            out.push_str(&format!(
                "\n刚结束的:\n{}",
                finished.iter().take(5).map(|(_, s)| s.as_str()).collect::<Vec<_>>().join("\n")
            ));
        }
        out
    }

    /// 看门狗扫一轮(时间注入可测):运行中且超过 STALL_MS 无步进 → 判卡收尾 + abort +
    /// 照常汇报。abort 让僵死任务的 TaskHandle 走 drop-自动-fail(HUD 如实标红)。
    pub(crate) fn sweep_once(&self, now: i64) {
        let stalled: Vec<Arc<BgEntry>> = {
            let entries = self.inner.entries.lock().expect("bg entries lock");
            entries
                .iter()
                .filter(|e| {
                    let st = e.st.lock().expect("bg st lock");
                    st.finished.is_none() && now - st.last_beat_ms > STALL_MS
                })
                .cloned()
                .collect()
        };
        for e in stalled {
            let (summary, abort) = {
                let mut st = e.st.lock().expect("bg st lock");
                if st.finished.is_some() {
                    continue; // 竞态:刚好收尾了
                }
                let summary = format!(
                    "「{}」卡住没动静(超过 {} 分钟无进展),已停掉:{}最后在处理{}。\
                     把情况告诉用户;要不要重跑剩下的,听用户的。",
                    e.title,
                    STALL_MS / 60_000,
                    progress_frag(st.done_units, e.total, "跑到 ", ","),
                    st.current
                );
                st.finished = Some(Fin { ok: false, summary: summary.clone(), at_ms: now });
                (summary, st.abort.take())
            };
            tracing::warn!(id = e.id, title = %e.title, "后台任务判卡,已停掉");
            self.report(e.origin, summary);
            if let Some(h) = abort {
                h.abort();
            }
        }
    }

    fn ensure_sweeper(&self) {
        let this = self.clone();
        self.inner.sweeper.get_or_init(move || {
            // 没有 tokio 运行时(纯同步测试)就不起巡逻;届时 sweep_once 可手动调
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    loop {
                        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                        this.sweep_once(now_ms());
                    }
                });
            }
        });
    }

    /// 收尾汇报:插 due=now 的一次性 jobs 任务(调度器 ≤30s 捡起 wake_turn 唤回合)。
    /// 走 `add_report`(kind=report)而不是普通提醒 —— 下游据此把「后台忙完了」与「到点提醒」
    /// 分开:汇报不自动念、系统线也不贴「到点了」(2026-08-15 真机:打字支使它干活,汇报却被念出来)。
    /// 插不进去只 warn(汇报丢了不至于砸任务;任务条终态仍在)。
    fn report(&self, origin: (i64, i64), text: String) {
        let store = self.inner.store.clone();
        let (user_id, conv_id) = origin;
        let due = now_ms();
        let insert = move || {
            if let Err(e) = store.jobs.add_report(user_id, conv_id, &text, due) {
                tracing::warn!("后台任务收尾汇报没插进任务: {e:#}");
            }
        };
        // 常态在 tokio 里(收尾/看门狗)→ 丢线程池;没有运行时(Drop 兜底路/同步测试)→ 直接插
        match tokio::runtime::Handle::try_current() {
            Ok(h) => {
                h.spawn(async move {
                    let _ = tokio::task::spawn_blocking(insert).await;
                });
            }
            Err(_) => insert(),
        }
    }
}

/// 任务循环持有的票据:打点 / 查取消 / 收尾。**没收尾就被 drop**(panic / 被 abort)=
/// 自动按「半路断了」收尾并汇报(TaskHandle drop-自动-fail 的模型面镜像)。
pub struct BgTicket {
    reg: BgTasks,
    entry: Arc<BgEntry>,
}

impl BgTicket {
    pub fn id(&self) -> u64 {
        self.entry.id
    }

    /// 打点:已完成 done 个,正在处理 current(顺带喂看门狗的活体信号)。
    pub fn beat(&self, done: usize, current: impl Into<String>) {
        let mut st = self.entry.st.lock().expect("bg st lock");
        st.done_units = done;
        st.current = current.into();
        st.last_beat_ms = now_ms();
    }

    /// 记一个没成的(点名封顶,总数照计)。
    pub fn miss(&self, name: &str) {
        let mut st = self.entry.st.lock().expect("bg st lock");
        st.miss_count += 1;
        if st.misses.len() < MISS_KEEP {
            st.misses.push(name.to_string());
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.entry.st.lock().expect("bg st lock").cancelled
    }

    /// 正常收尾(完成或按要求停下):记终态 + 插收尾汇报唤回合。
    pub fn finish(self, ok: bool, summary: impl Into<String>) {
        let summary = summary.into();
        {
            let mut st = self.entry.st.lock().expect("bg st lock");
            st.finished = Some(Fin { ok, summary: summary.clone(), at_ms: now_ms() });
        }
        self.reg.report(self.entry.origin, summary);
        // finished 已置,Drop 兜底看到终态即跳过
    }
}

impl Drop for BgTicket {
    fn drop(&mut self) {
        let summary = {
            let mut st = self.entry.st.lock().expect("bg st lock");
            if st.finished.is_some() {
                return; // 正常收尾 / 看门狗已处理
            }
            let s = format!(
                "「{}」半路断了(进程内部中断):{}最后在处理{}。把情况告诉用户。",
                self.entry.title,
                progress_frag(st.done_units, self.entry.total, "跑到 ", ","),
                st.current
            );
            st.finished = Some(Fin { ok: false, summary: s.clone(), at_ms: now_ms() });
            s
        };
        tracing::warn!(id = self.entry.id, title = %self.entry.title, "后台任务半路断,已汇报");
        self.reg.report(self.entry.origin, summary);
    }
}

/// 「12/40」进度片段(带前后缀);总数未知的清点型任务(total=0,如磁盘扫描)
/// 不显分母——返回空串,连同前后缀一起省略,进度全靠 current 文案自述。
fn progress_frag(done: usize, total: usize, prefix: &str, suffix: &str) -> String {
    if total == 0 {
        return String::new();
    }
    format!("{prefix}{done}/{total}{suffix}")
}

fn human_elapsed(ms: i64) -> String {
    let s = (ms / 1000).max(0);
    if s >= 3600 {
        format!("{} 小时 {} 分", s / 3600, (s % 3600) / 60)
    } else if s >= 60 {
        format!("{} 分 {} 秒", s / 60, s % 60)
    } else {
        format!("{s} 秒")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store(tag: &str) -> Store {
        let dir = std::env::temp_dir().join(format!("lw-bg-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::open(&dir.join("t.db")).unwrap();
        store.users.ensure_default_user().unwrap(); // jobs.user_id 有外键,汇报要归到人
        store
    }

    #[test]
    fn submit_beats_and_status_reads_back() {
        let bg = BgTasks::new(temp_store("status"));
        let t = bg.submit("批量配歌词(100 个)".into(), (1, 1), 100).unwrap();
        t.beat(37, "《示例曲目》");
        t.miss("没词1.mp3");
        t.miss("没词2.mp3");

        let amb = bg.ambient_line().unwrap();
        assert!(amb.contains("批量配歌词") && amb.contains("37/100") && amb.contains("编号"), "{amb}");
        let st = bg.status_report();
        assert!(st.contains("运行中 1 个") && st.contains("没成 2 个:没词1.mp3、没词2.mp3"), "{st}");
        t.finish(true, "全配好了");
        assert!(bg.ambient_line().is_none(), "收尾后不再占〔此刻〕");
        assert!(bg.status_report().contains("刚结束的"), "{}", bg.status_report());
    }

    #[test]
    fn finish_inserts_wakeup_job_with_summary() {
        let store = temp_store("finish");
        let bg = BgTasks::new(store.clone());
        let t = bg.submit("下载合集(3 首)".into(), (1, 42), 3).unwrap();
        t.finish(true, "下好 3 首,全成。");
        let jobs = store.jobs.due(now_ms() + 1_000).unwrap();
        assert_eq!(jobs.len(), 1, "收尾必插一条唤醒任务");
        assert_eq!(jobs[0].conv_id, 42);
        assert!(jobs[0].content.contains("下好 3 首"));
        assert_eq!(jobs[0].kind, "report", "汇报要与到点提醒分家(桌面据此决定念不念)");
    }

    #[test]
    fn cancel_sets_flag_and_only_once() {
        let bg = BgTasks::new(temp_store("cancel"));
        let t = bg.submit("批量配歌词(50 个)".into(), (1, 1), 50).unwrap();
        let id = t.id();
        assert_eq!(bg.cancel(id).as_deref(), Some("批量配歌词(50 个)"));
        assert!(t.is_cancelled());
        t.finish(false, "按要求停下了");
        assert!(bg.cancel(id).is_none(), "已收尾的不可再叫停");
        assert!(bg.cancel(9999).is_none(), "查无此号");
    }

    #[test]
    fn sweeper_marks_stalled_and_reports() {
        let store = temp_store("stall");
        let bg = BgTasks::new(store.clone());
        let t = bg.submit("批量配歌词(30 个)".into(), (1, 7), 30).unwrap();
        t.beat(5, "《某曲》");
        bg.sweep_once(now_ms() + STALL_MS + 1000);
        let st = bg.status_report();
        assert!(st.contains("刚结束的") && st.contains("卡住没动静"), "{st}");
        let jobs = store.jobs.due(now_ms() + 1_000).unwrap();
        assert_eq!(jobs.len(), 1);
        assert!(jobs[0].content.contains("卡住没动静") && jobs[0].content.contains("5/30"));
        drop(t); // 看门狗已置终态 → drop 兜底不再重复汇报
        assert_eq!(store.jobs.due(now_ms() + 1_000).unwrap().len(), 1);
    }

    #[test]
    fn dropped_ticket_reports_interruption() {
        let store = temp_store("drop");
        let bg = BgTasks::new(store.clone());
        let t = bg.submit("下载合集(9 首)".into(), (1, 3), 9).unwrap();
        t.beat(4, "《某曲》");
        drop(t); // 模拟 panic / abort:没收尾
        let jobs = store.jobs.due(now_ms() + 1_000).unwrap();
        assert_eq!(jobs.len(), 1);
        assert!(jobs[0].content.contains("半路断了") && jobs[0].content.contains("4/9"));
    }

    #[test]
    fn concurrency_cap_bails_honestly() {
        let bg = BgTasks::new(temp_store("cap"));
        let mut keep = Vec::new();
        for i in 0..BG_MAX_CONCURRENT {
            keep.push(bg.submit(format!("任务{i}"), (1, 1), 1).unwrap());
        }
        let err = match bg.submit("再来一个".into(), (1, 1), 1) {
            Err(e) => e,
            Ok(_) => panic!("cap 满还放行"),
        };
        assert!(err.to_string().contains("上限"), "{err:#}");
        keep.pop().unwrap().finish(true, "成");
        assert!(bg.submit("补位".into(), (1, 1), 1).is_ok(), "收尾一个即可再进一个");
    }

    #[test]
    fn cap_names_caps() {
        // 按字数不按条数:几十上百个短名字要**列全**(真机 92 首没配上,模型得答得出是哪些)
        let short: Vec<String> = (0..92).map(|i| format!("儿歌{i:02}.mp3")).collect();
        let out = cap_names(&short);
        assert!(!out.contains("等 "), "短名单要列全,不该截:{out}");
        assert!(out.contains("儿歌00.mp3") && out.contains("儿歌91.mp3"));
        assert_eq!(cap_names(&short[..2]), "儿歌00.mp3、儿歌01.mp3");

        // 长路径撑爆预算 → 列到预算 + 交代总数(且总在预算附近,不随条数爆)
        let long: Vec<String> = (0..300).map(|i| format!("{}{i:03}.flac", "某个很长的目录名/".repeat(5))).collect();
        let out = cap_names(&long);
        assert!(out.contains("等 300 个"), "装不下要交代总数:{}", &out[out.len() - 30..]);
        assert!(out.chars().count() < NAMES_MAX_CHARS + 60, "不该超预算太多:{} 字", out.chars().count());

        // 单个名字就超预算:至少列一个(不给空清单)
        let huge = vec!["x".repeat(NAMES_MAX_CHARS + 500), "y".into()];
        assert!(cap_names(&huge).starts_with("xxx") && cap_names(&huge).contains("等 2 个"));
    }
}
