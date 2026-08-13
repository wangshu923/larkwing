//! 磁盘占用引擎(fs_usage 的纯同步侧):walk 一棵目录树,聚合「第一层子目录排行 +
//! 全树最大文件 + 总量/文件数」。不跟符号链接/junction(防环 + 防重复计数);读不了的
//! 目录跳过并**计数如实报**(§3.5 绝不装算全了)。「往深处看」不在这里做——模型对着
//! 最大的子目录再调一次 fs_usage(一次调用 = 钻一层,§5 组合哲学)。
//! 执行节奏(回合内 30s → 转后台)在 media/usage.rs;报告文案 `render_report` 供
//! 回合内与后台收尾共用(compose_batch_summary 同款单源)。

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::files::human_size;

/// 报告列多少个第一层子目录(§4.11,2026-08-13 用户拍板「TOP 15」)。
pub const REPORT_DIRS: usize = 15;
/// 报告列多少个最大文件(§4.11 同批「TOP 10」)。
pub const REPORT_FILES: usize = 10;
/// 递归深度 backstop:防循环挂载/坏文件系统把栈干爆;真实目录树到不了这么深。
/// 超深的子树按「没看进去」计入 denied,不装算全了。
const MAX_DEPTH: usize = 64;

/// 扫描进度(执行侧看守任务读它 beat;cancel 由 task_cancel / drop 递旗)。
#[derive(Default)]
pub struct Progress {
    pub cancel: AtomicBool,
    /// 已清点的文件数(只增;总数未知,分母不存在)。
    pub scanned: AtomicUsize,
}

pub struct DirStat {
    pub name: String,
    pub bytes: u64,
    pub files: u64,
}

pub struct FileStat {
    pub path: PathBuf,
    pub bytes: u64,
}

#[derive(Default)]
pub struct UsageReport {
    pub total_bytes: u64,
    pub files: u64,
    /// 没权限/读不了(含超深 backstop)的目录数——这些没算进总量。
    pub denied: u64,
    /// 跳过的符号链接数(不跟,防环/防重复计数)。
    pub symlinks: u64,
    /// 第一层子目录,按占用降序(全量;渲染时截 REPORT_DIRS)。
    pub children: Vec<DirStat>,
    /// 直接躺在这一层(不在任何子目录里)的文件合计。
    pub root_files_bytes: u64,
    /// 全树最大文件,降序(至多 REPORT_FILES 条)。
    pub top_files: Vec<FileStat>,
    pub cancelled: bool,
}

/// 全树聚合的共享账本(顶文件小顶堆 + 各类计数)。
struct Tally {
    heap: BinaryHeap<Reverse<(u64, PathBuf)>>,
    files: u64,
    denied: u64,
    symlinks: u64,
}

/// 扫一棵树。根目录本身打不开 = 硬错误(整个动作没意义);子目录打不开 = 计数继续。
pub fn scan(root: &Path, prog: &Progress) -> anyhow::Result<UsageReport> {
    let rd = std::fs::read_dir(root)
        .map_err(|e| anyhow::anyhow!("打不开 {}(没权限或不存在):{e}", root.display()))?;
    let mut tally = Tally { heap: BinaryHeap::new(), files: 0, denied: 0, symlinks: 0 };
    let mut children: Vec<DirStat> = Vec::new();
    let mut root_files_bytes = 0u64;
    for ent in rd {
        if prog.cancel.load(Ordering::Relaxed) {
            return Ok(finish(children, root_files_bytes, tally, true));
        }
        let Ok(ent) = ent else {
            tally.denied += 1;
            continue;
        };
        let Ok(ft) = ent.file_type() else {
            tally.denied += 1;
            continue;
        };
        if ft.is_symlink() {
            tally.symlinks += 1;
        } else if ft.is_dir() {
            let (bytes, files) = walk(&ent.path(), 1, prog, &mut tally);
            children.push(DirStat {
                name: ent.file_name().to_string_lossy().into_owned(),
                bytes,
                files,
            });
        } else {
            let size = ent.metadata().map(|m| m.len()).unwrap_or(0);
            root_files_bytes += size;
            note_file(&mut tally, prog, ent.path(), size);
        }
    }
    let cancelled = prog.cancel.load(Ordering::Relaxed);
    Ok(finish(children, root_files_bytes, tally, cancelled))
}

/// 子树聚合,返回 (bytes, files)。读不了的目录计 denied 继续;不跟符号链接。
fn walk(dir: &Path, depth: usize, prog: &Progress, tally: &mut Tally) -> (u64, u64) {
    if depth > MAX_DEPTH {
        tally.denied += 1;
        return (0, 0);
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        tally.denied += 1;
        return (0, 0);
    };
    let (mut bytes, mut files) = (0u64, 0u64);
    for ent in rd {
        if prog.cancel.load(Ordering::Relaxed) {
            return (bytes, files);
        }
        let Ok(ent) = ent else {
            tally.denied += 1;
            continue;
        };
        let Ok(ft) = ent.file_type() else {
            tally.denied += 1;
            continue;
        };
        if ft.is_symlink() {
            tally.symlinks += 1;
        } else if ft.is_dir() {
            let (b, f) = walk(&ent.path(), depth + 1, prog, tally);
            bytes += b;
            files += f;
        } else {
            let size = ent.metadata().map(|m| m.len()).unwrap_or(0);
            bytes += size;
            files += 1;
            note_file(tally, prog, ent.path(), size);
        }
    }
    (bytes, files)
}

fn note_file(tally: &mut Tally, prog: &Progress, path: PathBuf, size: u64) {
    tally.files += 1;
    prog.scanned.fetch_add(1, Ordering::Relaxed);
    tally.heap.push(Reverse((size, path)));
    if tally.heap.len() > REPORT_FILES {
        tally.heap.pop(); // 小顶堆挤掉当前最小,常驻内存恒 O(REPORT_FILES)
    }
}

fn finish(mut children: Vec<DirStat>, root_files_bytes: u64, tally: Tally, cancelled: bool) -> UsageReport {
    children.sort_by_key(|c| Reverse(c.bytes));
    let total_bytes = children.iter().map(|c| c.bytes).sum::<u64>() + root_files_bytes;
    // Reverse 序的 into_sorted_vec 升序 = 真实大小降序,正好是报告要的顺序
    let top_files = tally
        .heap
        .into_sorted_vec()
        .into_iter()
        .map(|Reverse((bytes, path))| FileStat { path, bytes })
        .collect();
    UsageReport {
        total_bytes,
        files: tally.files,
        denied: tally.denied,
        symlinks: tally.symlinks,
        children,
        root_files_bytes,
        top_files,
        cancelled,
    }
}

/// 报告文案(喂模型的观察,量约束 = 只汇总 + 点名,不随文件数爆):
/// 回合内路与后台收尾共用这一份,格式永远一致。
pub fn render_report(root: &Path, rep: &UsageReport) -> String {
    if rep.files == 0 && rep.children.is_empty() {
        return format!("{} 是空的(没有文件)。", root.display());
    }
    let mut out = format!(
        "{} 共 {},{} 个文件",
        root.display(),
        human_size(rep.total_bytes),
        rep.files
    );
    if rep.denied > 0 {
        out.push_str(&format!(";另有 {} 个目录没权限看、没算进总量", rep.denied));
    }
    if rep.symlinks > 0 {
        out.push_str(&format!(";跳过 {} 个符号链接", rep.symlinks));
    }
    out.push('\n');
    if !rep.children.is_empty() {
        out.push_str("子目录(按占用):\n");
        for c in rep.children.iter().take(REPORT_DIRS) {
            out.push_str(&format!("- {} — {}({} 个文件)\n", c.name, human_size(c.bytes), c.files));
        }
        if rep.children.len() > REPORT_DIRS {
            let rest: u64 = rep.children.iter().skip(REPORT_DIRS).map(|c| c.bytes).sum();
            out.push_str(&format!(
                "(其余 {} 个子目录共 {})\n",
                rep.children.len() - REPORT_DIRS,
                human_size(rest)
            ));
        }
    }
    if rep.root_files_bytes > 0 {
        out.push_str(&format!("直接放在这一层的文件共 {}\n", human_size(rep.root_files_bytes)));
    }
    if !rep.top_files.is_empty() {
        out.push_str("最大的文件:\n");
        for f in rep.top_files.iter().take(REPORT_FILES) {
            out.push_str(&format!("- {} — {}\n", f.path.display(), human_size(f.bytes)));
        }
    }
    out.push_str("要弄清哪儿占的地方,对着最大的子目录再调一次 fs_usage 往下钻。");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lw-usage-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn scan_aggregates_ranks_and_tops() {
        let root = tmp("agg");
        std::fs::create_dir_all(root.join("big/sub")).unwrap();
        std::fs::create_dir_all(root.join("small")).unwrap();
        std::fs::write(root.join("big/a.bin"), vec![0u8; 3000]).unwrap();
        std::fs::write(root.join("big/sub/b.bin"), vec![0u8; 2000]).unwrap();
        std::fs::write(root.join("small/c.bin"), vec![0u8; 10]).unwrap();
        std::fs::write(root.join("root.txt"), b"hello").unwrap();

        let prog = Progress::default();
        let rep = scan(&root, &prog).unwrap();
        assert_eq!(rep.total_bytes, 3000 + 2000 + 10 + 5);
        assert_eq!(rep.files, 4);
        assert_eq!(rep.root_files_bytes, 5);
        assert_eq!(rep.denied, 0);
        assert!(!rep.cancelled);
        // 子目录按占用降序
        assert_eq!(rep.children[0].name, "big");
        assert_eq!(rep.children[0].bytes, 5000);
        assert_eq!(rep.children[0].files, 2);
        assert_eq!(rep.children[1].name, "small");
        // 最大文件降序
        assert_eq!(rep.top_files[0].bytes, 3000);
        assert_eq!(rep.top_files[1].bytes, 2000);
        assert_eq!(prog.scanned.load(Ordering::Relaxed), 4);

        let text = render_report(&root, &rep);
        assert!(text.contains("big"), "报告要点名大头: {text}");
        assert!(text.contains("再调一次 fs_usage"), "要带下钻指引: {text}");
    }

    #[test]
    fn cancel_flag_stops_scan_early() {
        let root = tmp("cancel");
        std::fs::write(root.join("a.txt"), b"x").unwrap();
        let prog = Progress::default();
        prog.cancel.store(true, Ordering::Relaxed);
        let rep = scan(&root, &prog).unwrap();
        assert!(rep.cancelled);
    }

    #[test]
    fn empty_dir_renders_honest_line() {
        let root = tmp("empty");
        let rep = scan(&root, &Progress::default()).unwrap();
        assert!(render_report(&root, &rep).contains("是空的"));
    }

    #[test]
    fn render_truncates_dirs_beyond_report_cap() {
        let children: Vec<DirStat> = (0..REPORT_DIRS + 5)
            .map(|i| DirStat { name: format!("d{i}"), bytes: 1000 - i as u64, files: 1 })
            .collect();
        let rep = UsageReport {
            total_bytes: children.iter().map(|c| c.bytes).sum(),
            files: children.len() as u64,
            children,
            ..Default::default()
        };
        let text = render_report(Path::new("/x"), &rep);
        assert!(text.contains("其余 5 个子目录"), "超 TOP 要如实汇总: {text}");
    }

    #[cfg(unix)]
    #[test]
    fn denied_dir_is_counted_not_fatal() {
        use std::os::unix::fs::PermissionsExt;
        let root = tmp("denied");
        let locked = root.join("locked");
        std::fs::create_dir_all(&locked).unwrap();
        std::fs::write(locked.join("hidden.bin"), vec![0u8; 100]).unwrap();
        std::fs::write(root.join("seen.bin"), vec![0u8; 7]).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let rep = scan(&root, &Progress::default()).unwrap();
        // 收尾恢复权限,免得 temp 清理不动
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        // root 跑测试时是能读进去的(uid 0 无视权限位)→ 那种环境下没有 denied,别硬断言
        if rep.denied > 0 {
            assert_eq!(rep.total_bytes, 7, "读不进去的目录不该计入总量");
            assert!(render_report(&root, &rep).contains("没权限"), "要如实报没权限");
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_not_followed_or_counted() {
        let root = tmp("syml");
        std::fs::write(root.join("real.bin"), vec![0u8; 50]).unwrap();
        std::os::unix::fs::symlink(root.join("real.bin"), root.join("link.bin")).unwrap();
        std::os::unix::fs::symlink(&root, root.join("loop")).unwrap(); // 指回自己的环
        let rep = scan(&root, &Progress::default()).unwrap();
        assert_eq!(rep.total_bytes, 50, "链接不计大小、不重复计数");
        assert_eq!(rep.files, 1);
        assert_eq!(rep.symlinks, 2);
    }
}
