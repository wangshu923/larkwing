//! 文件能力(PLAN §9):写类原语的底层执行 + 撤销/重做。纯文件 I/O,同步实现
//! (调用方负责 spawn_blocking)。`tools/fs.rs` 的 Tool 调它干活,engine 的撤销/重做
//! 命令也调它(共享同一份反向逻辑)。**不碰 DB** —— 操作记录的结构化条目(`FsOpItem`)
//! 在这里定义,`store::fsops` 只存其 JSON。
//!
//! 规矩(用户准则:功能性、不覆盖、可撤销;**不做安全承诺**):
//! - 移动/复制**永不静默覆盖**:目标已存在 → 自动在扩展名前加 ` (N)`(资源管理器口径)。
//! - 删除走系统回收站(`trash` crate),不 `unlink`。
//! - 文本写/改前把旧内容快照进记录(小文件白菜价;超 `SNAPSHOT_MAX` 不快照 → 该条不可撤,如实说)。
//! - 新建名过 Windows 合法性闸(保留名 / 非法字符 / 结尾点空格);部署目标是 Windows。
//!
//! 跨平台:`trash::delete` 各平台都行;**还原(撤销删除)只有 Windows/Linux 支持**
//! (`os_limited`),macOS(开发机)上 `restore_from_trash` 返回未还原 —— 如实降级,
//! 不影响产品(Windows)。其余原语(移动/复制/新建/文本)全平台可测。

use std::path::{Path, PathBuf};

use anyhow::{anyhow, ensure, Context, Result};
use serde::{Deserialize, Serialize};

/// 文本内容快照上限:超过则不快照(撤销/重做该条不可用)。家用笔记/清单远在此下。
pub const SNAPSHOT_MAX: usize = 1 << 20; // 1 MiB

/// 一条文件操作的结构化记录:足够反向(撤销)与重放(重做)。一批 = `Vec<FsOpItem>`,
/// 序列化进 `store::fsops` 的 `ops` 列。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum FsOpItem {
    Move { src: String, dst: String },
    Copy { src: String, dst: String },
    Mkdir { path: String },
    Trash { path: String },
    /// `was_new`:此次写入前文件不存在(撤销=删);否则 `old`=旧内容(None=超限/二进制,不可撤)。
    Write { path: String, old: Option<String>, new: Option<String>, was_new: bool },
    Append { path: String, text: String, old_len: u64, was_new: bool },
    Edit { path: String, old: Option<String>, new: Option<String> },
}

// ---------------------------------------------------------------------------
// 路径 / 命名校验、冲突去重(永不覆盖)
// ---------------------------------------------------------------------------

const WIN_RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// 新建/改名目标的**文件名分量**合法性(Windows 口径;Mac 上也照此校 → 行为一致,
/// 出 Windows 包不踩坑)。错误即清晰观察,让模型重拟名,而不是把脏名写进盘。
pub fn validate_name(name: &str) -> Result<()> {
    ensure!(!name.is_empty(), "文件名不能为空");
    ensure!(
        !name
            .chars()
            .any(|c| matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
                || (c as u32) < 0x20),
        "名字里有不能用的字符(< > : \" / \\ | ? * 或控制字符):{name}"
    );
    ensure!(
        name.trim_end_matches([' ', '.']) == name,
        "名字结尾不能是空格或点:{name}"
    );
    let stem = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
    ensure!(!WIN_RESERVED.contains(&stem.as_str()), "{name} 是 Windows 保留名,换一个");
    Ok(())
}

/// 目标已存在 → 在扩展名前加 ` (N)`,取最小可用 N(资源管理器口径)。**永不覆盖。**
fn dedupe_path(dst: &Path) -> PathBuf {
    if !dst.exists() {
        return dst.to_path_buf();
    }
    let parent = dst.parent().unwrap_or_else(|| Path::new("."));
    let stem = dst.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = dst.extension().map(|e| e.to_string_lossy().into_owned());
    for n in 2..100_000 {
        let name = match &ext {
            Some(e) => format!("{stem} ({n}).{e}"),
            None => format!("{stem} ({n})"),
        };
        let cand = parent.join(name);
        if !cand.exists() {
            return cand;
        }
    }
    dst.to_path_buf() // 理论兜底(几乎不可能走到)
}

/// dst 是已存在的文件夹 → 解析成"移/复制**进**它里面,保留原名"(文件管理器直觉);
/// 否则 dst 当作完整目标路径(移动+改名)。
fn resolve_into_dir(src: &Path, dst_req: &Path) -> PathBuf {
    if dst_req.is_dir() {
        if let Some(name) = src.file_name() {
            return dst_req.join(name);
        }
    }
    dst_req.to_path_buf()
}

/// 递归复制(文件或目录);跨卷移动的兜底也用它。
fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    let md = std::fs::symlink_metadata(src)?;
    if md.is_dir() {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            copy_tree(&entry.path(), &dst.join(entry.file_name()))?;
        }
    } else {
        if let Some(p) = dst.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::copy(src, dst)?;
    }
    Ok(())
}

/// 跨卷判定:Unix `EXDEV`=18 / Windows `ERROR_NOT_SAME_DEVICE`=17 → `rename` 失败需 copy+删源。
fn is_cross_device(e: &std::io::Error) -> bool {
    matches!(e.raw_os_error(), Some(18) | Some(17))
}

fn remove_any(path: &Path) -> Result<()> {
    let md = std::fs::symlink_metadata(path)?;
    if md.is_dir() {
        std::fs::remove_dir_all(path)?;
    } else {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

fn into_trash(path: &Path) -> Result<()> {
    trash::delete(path).map_err(|e| anyhow!("移到回收站失败:{}({e})", path.display()))
}

// ---------------------------------------------------------------------------
// 写类原语(每个返回一条记录;调用方汇总落 fsops)
// ---------------------------------------------------------------------------

/// 移动 / 改名。dst 是已存在文件夹 → 移进去保留名;否则当完整目标路径。永不覆盖(冲突加后缀)。
pub fn move_one(src: &Path, dst_req: &Path) -> Result<FsOpItem> {
    ensure!(src.exists(), "源不存在:{}", src.display());
    let dst_req = resolve_into_dir(src, dst_req);
    if let Some(name) = dst_req.file_name().and_then(|n| n.to_str()) {
        validate_name(name)?;
    }
    let dst = dedupe_path(&dst_req);
    if let Some(p) = dst.parent() {
        std::fs::create_dir_all(p)?;
    }
    match std::fs::rename(src, &dst) {
        Ok(()) => {}
        Err(e) if is_cross_device(&e) => {
            // 跨卷:先整体复制成功,再删源(非原子 —— 复制失败就 ? 出去、不删源)
            copy_tree(src, &dst).with_context(|| format!("跨盘移动复制阶段失败:{}", src.display()))?;
            remove_any(src).with_context(|| format!("跨盘移动删源失败:{}", src.display()))?;
        }
        Err(e) => return Err(e).with_context(|| format!("移动失败:{}", src.display())),
    }
    Ok(FsOpItem::Move {
        src: src.to_string_lossy().into_owned(),
        dst: dst.to_string_lossy().into_owned(),
    })
}

/// 复制(文件或目录)。语义同 move 的目标解析;永不覆盖。
pub fn copy_one(src: &Path, dst_req: &Path) -> Result<FsOpItem> {
    ensure!(src.exists(), "源不存在:{}", src.display());
    let dst_req = resolve_into_dir(src, dst_req);
    if let Some(name) = dst_req.file_name().and_then(|n| n.to_str()) {
        validate_name(name)?;
    }
    let dst = dedupe_path(&dst_req);
    copy_tree(src, &dst)?;
    Ok(FsOpItem::Copy {
        src: src.to_string_lossy().into_owned(),
        dst: dst.to_string_lossy().into_owned(),
    })
}

/// 新建文件夹(递归)。已存在 = 幂等成功。
pub fn mkdir_one(path: &Path) -> Result<FsOpItem> {
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        validate_name(name)?;
    }
    std::fs::create_dir_all(path)?;
    Ok(FsOpItem::Mkdir { path: path.to_string_lossy().into_owned() })
}

/// 删除 → 系统回收站(可还原)。不 `unlink`。
pub fn trash_one(path: &Path) -> Result<FsOpItem> {
    ensure!(path.exists(), "要删的不存在:{}", path.display());
    into_trash(path)?;
    Ok(FsOpItem::Trash { path: path.to_string_lossy().into_owned() })
}

fn read_snapshot(path: &Path) -> Option<String> {
    let md = std::fs::metadata(path).ok()?;
    if md.len() as usize > SNAPSHOT_MAX {
        return None; // 太大不快照
    }
    std::fs::read_to_string(path).ok() // 非 UTF-8/二进制 → None(该条不可撤,如实)
}

fn snapshot_of(content: &str) -> Option<String> {
    (content.len() <= SNAPSHOT_MAX).then(|| content.to_string())
}

/// 写 / 覆盖一个文本文件。改前快照旧内容(撤销用)。覆盖**已管理的文件**是目的(同 §:
/// 与 move/copy 的"不覆盖别的文件"区分)。
pub fn write_text(path: &Path, content: &str) -> Result<FsOpItem> {
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        validate_name(name)?;
    }
    let was_new = !path.exists();
    let old = if was_new { None } else { read_snapshot(path) };
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    std::fs::write(path, content)?;
    Ok(FsOpItem::Write {
        path: path.to_string_lossy().into_owned(),
        old,
        new: snapshot_of(content),
        was_new,
    })
}

/// 往文本文件追加(缺则建)。只发新增内容 → 无截断风险,适合"清单加一行/记一笔"。
pub fn append_text(path: &Path, text: &str) -> Result<FsOpItem> {
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        validate_name(name)?;
    }
    let was_new = !path.exists();
    let old_len = if was_new { 0 } else { std::fs::metadata(path)?.len() };
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(text.as_bytes())?;
    Ok(FsOpItem::Append { path: path.to_string_lossy().into_owned(), text: text.to_string(), old_len, was_new })
}

/// 轻量替换:把文件里**唯一一处** `find` 换成 `replace`。找不到/多处 → 清晰错误(让模型重拟),
/// 不背 robot 的 read-gate/stale。改前快照旧内容(撤销用)。
pub fn edit_text(path: &Path, find: &str, replace: &str) -> Result<FsOpItem> {
    ensure!(path.is_file(), "要改的文件不存在:{}", path.display());
    ensure!(!find.is_empty(), "要替换的内容不能为空");
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("读不了(可能不是文本文件):{}", path.display()))?;
    let count = content.matches(find).count();
    ensure!(count != 0, "没找到要替换的内容「{find}」");
    ensure!(count == 1, "「{find}」在文件里出现了 {count} 次,改不准 —— 给一段更独特的原文再试");
    let old = snapshot_of(&content);
    let updated = content.replacen(find, replace, 1);
    std::fs::write(path, &updated)?;
    Ok(FsOpItem::Edit { path: path.to_string_lossy().into_owned(), old, new: snapshot_of(&updated) })
}

// ---------------------------------------------------------------------------
// 撤销 / 重做(一批)
// ---------------------------------------------------------------------------

/// 一批撤销/重做的结果:成功几条、跳过几条(不可逆/环境已变/出错)。
#[derive(Debug, Clone, Copy, Default)]
pub struct OpReport {
    pub done: usize,
    pub skipped: usize,
}

/// 撤销一批(逆序逐条;单条失败/不可逆 = 跳过,不连累其余)。
pub fn undo_batch(items: &[FsOpItem]) -> OpReport {
    let mut r = OpReport::default();
    for item in items.iter().rev() {
        match undo_one(item) {
            Ok(true) => r.done += 1,
            Ok(false) => r.skipped += 1,
            Err(e) => {
                tracing::warn!("撤销一条失败:{e:#}");
                r.skipped += 1;
            }
        }
    }
    r
}

/// 重做一批(正序逐条)。
pub fn redo_batch(items: &[FsOpItem]) -> OpReport {
    let mut r = OpReport::default();
    for item in items.iter() {
        match redo_one(item) {
            Ok(true) => r.done += 1,
            Ok(false) => r.skipped += 1,
            Err(e) => {
                tracing::warn!("重做一条失败:{e:#}");
                r.skipped += 1;
            }
        }
    }
    r
}

/// Ok(true)=动了 Ok(false)=跳过(目标已不在/不可逆/非空)。
fn undo_one(item: &FsOpItem) -> Result<bool> {
    match item {
        FsOpItem::Move { src, dst } => {
            let d = Path::new(dst);
            if !d.exists() {
                return Ok(false); // 被用户又动过了
            }
            move_one(d, Path::new(src))?; // 套同样不覆盖(原位被占则加后缀)
            Ok(true)
        }
        FsOpItem::Copy { dst, .. } => {
            // 撤销复制 = 删掉那个副本(原件还在,不丢数据);走回收站让它也可恢复
            let d = Path::new(dst);
            if !d.exists() {
                return Ok(false);
            }
            into_trash(d)?;
            Ok(true)
        }
        FsOpItem::Mkdir { path } => {
            // 只删空文件夹;非空(用户往里放了东西)/不存在 → 跳过
            Ok(std::fs::remove_dir(path).is_ok())
        }
        FsOpItem::Trash { path } => restore_from_trash(Path::new(path)),
        FsOpItem::Write { path, old, was_new, .. } => {
            let p = Path::new(path);
            if *was_new {
                Ok(std::fs::remove_file(p).is_ok())
            } else if let Some(old) = old {
                std::fs::write(p, old)?;
                Ok(true)
            } else {
                Ok(false) // 旧内容没快照(超限/二进制)
            }
        }
        FsOpItem::Append { path, old_len, was_new, .. } => {
            let p = Path::new(path);
            if *was_new {
                Ok(std::fs::remove_file(p).is_ok())
            } else {
                let f = std::fs::OpenOptions::new().write(true).open(p)?;
                f.set_len(*old_len)?;
                Ok(true)
            }
        }
        FsOpItem::Edit { path, old, .. } => {
            if let Some(old) = old {
                std::fs::write(path, old)?;
                Ok(true)
            } else {
                Ok(false)
            }
        }
    }
}

fn redo_one(item: &FsOpItem) -> Result<bool> {
    match item {
        FsOpItem::Move { src, dst } => {
            let s = Path::new(src);
            if !s.exists() {
                return Ok(false);
            }
            move_one(s, Path::new(dst))?;
            Ok(true)
        }
        FsOpItem::Copy { src, dst } => {
            copy_one(Path::new(src), Path::new(dst))?;
            Ok(true)
        }
        FsOpItem::Mkdir { path } => {
            std::fs::create_dir_all(path)?;
            Ok(true)
        }
        FsOpItem::Trash { path } => {
            let p = Path::new(path);
            if !p.exists() {
                return Ok(false);
            }
            into_trash(p)?;
            Ok(true)
        }
        FsOpItem::Write { path, new, .. } | FsOpItem::Edit { path, new, .. } => {
            if let Some(new) = new {
                std::fs::write(path, new)?;
                Ok(true)
            } else {
                Ok(false)
            }
        }
        FsOpItem::Append { path, text, .. } => {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
            f.write_all(text.as_bytes())?;
            Ok(true)
        }
    }
}

/// 从系统回收站按原路径还原。Windows/Linux 支持;macOS 的 trash crate 无 list/restore →
/// 返回未还原(如实降级,产品在 Windows)。
#[cfg(any(target_os = "windows", all(unix, not(target_os = "macos"))))]
fn restore_from_trash(original: &Path) -> Result<bool> {
    use trash::os_limited::{list, restore_all};
    let want = original.to_path_buf();
    let mut hits: Vec<trash::TrashItem> =
        list().map_err(|e| anyhow!("读回收站失败:{e}"))?
            .into_iter()
            .filter(|it| it.original_path() == want)
            .collect();
    if hits.is_empty() {
        return Ok(false); // 已被清空回收站等 → 还不回来(如实)
    }
    hits.sort_by_key(|it| it.time_deleted); // 同原路径多条 → 还原最近删的
    let newest = hits.pop().unwrap();
    restore_all([newest]).map_err(|e| anyhow!("从回收站还原失败:{e}"))?;
    Ok(true)
}

#[cfg(not(any(target_os = "windows", all(unix, not(target_os = "macos")))))]
fn restore_from_trash(_original: &Path) -> Result<bool> {
    Ok(false) // macOS 等:trash crate 不支持还原;Windows 产品上走上面的实现
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("lw-files-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn validate_name_rejects_windows_traps() {
        assert!(validate_name("正常.mp4").is_ok());
        assert!(validate_name("a<b.txt").is_err());
        assert!(validate_name("结尾点.").is_err());
        assert!(validate_name("空格 ").is_err());
        assert!(validate_name("CON").is_err());
        assert!(validate_name("nul.txt").is_err());
    }

    #[test]
    fn move_never_overwrites_then_undo_restores() {
        let d = sandbox("move");
        std::fs::write(d.join("a.txt"), b"A").unwrap();
        std::fs::write(d.join("b.txt"), b"B-existing").unwrap();
        // 移动 a → b(已存在)→ 自动变 b (2).txt,不覆盖
        let item = move_one(&d.join("a.txt"), &d.join("b.txt")).unwrap();
        let FsOpItem::Move { dst, .. } = &item else { panic!() };
        assert!(dst.ends_with("b (2).txt"), "冲突加后缀: {dst}");
        assert!(!d.join("a.txt").exists());
        assert_eq!(std::fs::read(d.join("b.txt")).unwrap(), b"B-existing", "原 b 没被覆盖");
        // 撤销 → a 回来,副本没了
        let r = undo_batch(std::slice::from_ref(&item));
        assert_eq!((r.done, r.skipped), (1, 0));
        assert!(d.join("a.txt").exists());
        assert!(!d.join("b (2).txt").exists());
    }

    #[test]
    fn move_into_existing_dir_keeps_name() {
        let d = sandbox("into");
        std::fs::write(d.join("song.mp3"), b"x").unwrap();
        std::fs::create_dir(d.join("Music")).unwrap();
        let item = move_one(&d.join("song.mp3"), &d.join("Music")).unwrap();
        let FsOpItem::Move { dst, .. } = &item else { panic!() };
        assert!(dst.ends_with("Music/song.mp3") || dst.ends_with("Music\\song.mp3"), "{dst}");
        assert!(d.join("Music/song.mp3").exists());
    }

    #[test]
    fn write_then_undo_and_redo() {
        let d = sandbox("write");
        let f = d.join("note.txt");
        let w1 = write_text(&f, "第一版").unwrap();
        assert!(matches!(&w1, FsOpItem::Write { was_new: true, .. }));
        let w2 = write_text(&f, "第二版").unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "第二版");
        // 撤销第二次写 → 回到第一版
        undo_batch(std::slice::from_ref(&w2));
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "第一版");
        // 重做第二次写 → 又是第二版
        redo_batch(std::slice::from_ref(&w2));
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "第二版");
        // 撤销首次写(was_new)→ 文件消失
        undo_batch(std::slice::from_ref(&w1));
        assert!(!f.exists());
    }

    #[test]
    fn append_then_undo_truncates() {
        let d = sandbox("append");
        let f = d.join("list.txt");
        std::fs::write(&f, "牛奶\n").unwrap();
        let a = append_text(&f, "鸡蛋\n").unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "牛奶\n鸡蛋\n");
        undo_batch(std::slice::from_ref(&a));
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "牛奶\n", "截回追加前的长度");
    }

    #[test]
    fn edit_replaces_unique_and_errors_on_ambiguous() {
        let d = sandbox("edit");
        let f = d.join("note.txt");
        std::fs::write(&f, "苹果 香蕉 苹果").unwrap();
        assert!(edit_text(&f, "苹果", "梨").is_err(), "多处匹配应报错");
        let e = edit_text(&f, "香蕉", "西瓜").unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "苹果 西瓜 苹果");
        undo_batch(std::slice::from_ref(&e));
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "苹果 香蕉 苹果");
    }

    #[test]
    fn mkdir_undo_only_removes_empty() {
        let d = sandbox("mkdir");
        let made = mkdir_one(&d.join("新建/深层")).unwrap();
        assert!(d.join("新建/深层").is_dir());
        // 往里放东西后撤销 → 不删(跳过)
        std::fs::write(d.join("新建/深层/x.txt"), b"x").unwrap();
        let r = undo_batch(std::slice::from_ref(&made));
        assert_eq!(r.skipped, 1, "非空文件夹不删");
        assert!(d.join("新建/深层").is_dir());
    }

    #[test]
    fn op_items_json_roundtrip() {
        let item = FsOpItem::Move { src: "/a".into(), dst: "/b".into() };
        let j = serde_json::to_string(&item).unwrap();
        assert!(j.contains("\"op\":\"move\""));
        let back: FsOpItem = serde_json::from_str(&j).unwrap();
        assert_eq!(item, back);
    }
}
