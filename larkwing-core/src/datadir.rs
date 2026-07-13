//! 数据目录「搬家」(用户决策 2026-06-18):把整个数据根重定位到别的盘 / 目录
//! (Windows 多盘场景:不想把模型 / 数据库全堆在 C 盘)。
//!
//! 核心机制 = **锚点 + 指针文件**。OS 默认 `app_data_dir` 是唯一「永远找得到、且不依赖任何
//! 已存配置」的位置 —— 数据库本身就在要搬的目录里,不能拿它存「数据在哪」(鸡生蛋)。所以在
//! 锚点放一个极小的指针文件 `location.json`:
//!   - 不存在 / `data_root` 空 → 没搬过家,用默认根(= 锚点本身);
//!   - `data_root` 指向别处且目录在 → 搬过家,用记的路径;
//!   - `data_root` 指向别处但目录不在(盘没插 / 被删)→ `Resolution.missing`,壳层友好处理,
//!     **绝不静默在默认位置重建空数据**(§3.5 不静默失败)。
//!
//! 指针文件本身**永不参与搬家**,永远留在锚点。
//!
//! 搬家流程(提交点 = 翻指针):拷可重建 / 静态子树 → DB 走 `VACUUM INTO` 出一致快照(放最后)
//! → 校验 → staging 原子改名就位 → 调用方翻指针(写 `old_root` 供清理)。翻指针前老数据始终
//! 权威;崩在翻指针前 = 老数据完好,新位置那坨是垃圾。DB 里只存相对文件名(克隆音色 wav /
//! TTS 缓存按名落盘),整棵子树挪走相对结构不变 → 路径不会断,这是搬家能安全的前提。

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::tasks::TaskHandle;

/// 指针文件名(放在锚点 = OS 默认 app_data_dir)。
pub const POINTER_FILE: &str = "location.json";
/// 搬家时在用户所选目录里创建的数据子目录名(不把文件散落进用户选的目录)。
pub const DATA_DIR_NAME: &str = "Larkwing";
/// 数据库文件名(须与 lib.rs 装配处 `larkwing.db` 一致)。
const DB_FILE: &str = "larkwing.db";

/// 锚点指针:记「真实数据根在哪」+「刚搬完待清理的旧根」。两者都可空。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Pointer {
    /// 当前数据根(绝对路径);None / 空 = 用默认(锚点)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_root: Option<String>,
    /// 刚搬完、待用户确认删除的旧根;清理后置空。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_root: Option<String>,
}

/// 启动解析结果。
pub struct Resolution {
    /// 生效的数据根(壳层据此开库 / 日志 / 媒体 / 语音)。
    pub root: PathBuf,
    /// 刚搬完待清理的旧根(供「清理旧数据」提示);None = 无残留。
    pub old_root: Option<PathBuf>,
    /// 指针指向的根失效(盘没插 / 被删):`root` 此时回落锚点,壳层应先弹友好对话框,
    /// **不可**当「没搬过家」静默继续。None = 正常。
    pub missing: Option<PathBuf>,
}

fn pointer_path(anchor: &Path) -> PathBuf {
    anchor.join(POINTER_FILE)
}

/// 读指针(文件不存在 / 解析失败 → 默认空指针,不报错 —— 坏指针不该让 app 起不来,
/// 当「没搬过家」从锚点起,坏在哪由日志说)。
pub fn read_pointer(anchor: &Path) -> Pointer {
    match std::fs::read_to_string(pointer_path(anchor)) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
            tracing::warn!(err = %e, "数据位置指针解析失败,按未搬家处理");
            Pointer::default()
        }),
        Err(_) => Pointer::default(),
    }
}

/// 原子写指针(写临时文件再 rename)。
pub fn write_pointer(anchor: &Path, p: &Pointer) -> Result<()> {
    std::fs::create_dir_all(anchor).ok();
    let final_path = pointer_path(anchor);
    let tmp = final_path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(p)?)
        .with_context(|| format!("写数据位置指针失败: {}", tmp.display()))?;
    std::fs::rename(&tmp, &final_path)?;
    Ok(())
}

/// 启动解析:`anchor` = OS 默认 app_data_dir。
pub fn resolve(anchor: &Path) -> Resolution {
    let p = read_pointer(anchor);
    // 指针路径先去 verbatim 前缀:老版本搬家写进过 `\\?\E:\…`(norm 未 simplify 时期),
    // 这里统一洗一遍 = 存量用户下次启动即自愈,不用改指针文件。
    let recorded = p
        .data_root
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| simplify(Path::new(s)));
    let old_root = p
        .old_root
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| simplify(Path::new(s)));
    match recorded {
        None => Resolution { root: anchor.to_path_buf(), old_root, missing: None },
        // 搬过家:目录在 = 用它;目录不在(盘没插 / 被删)= missing,回落锚点交壳层处理。
        Some(root) if root.is_dir() => Resolution { root, old_root, missing: None },
        Some(root) => Resolution { root: anchor.to_path_buf(), old_root, missing: Some(root) },
    }
}

// ---- 搬家预检 + 计划 + 执行 ----

/// 搬家预检失败原因(→ 前端 i18n code,见 `code()`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveBlock {
    /// 目标 = 当前根。
    Same,
    /// 目标落在当前数据根内部(会自吞 / 无限拷贝)。
    Overlap,
    /// 目标处已存在同名数据文件夹且非空。
    Exists,
    /// 目标不可写。
    NotWritable,
    /// 目标剩余空间不足。
    NoSpace,
}

impl MoveBlock {
    /// 前端字典 key(`settings.dataLocation.err.<code>`)。
    pub fn code(self) -> &'static str {
        match self {
            MoveBlock::Same => "same",
            MoveBlock::Overlap => "overlap",
            MoveBlock::Exists => "exists",
            MoveBlock::NotWritable => "not_writable",
            MoveBlock::NoSpace => "no_space",
        }
    }
}

/// 通过预检后的搬家计划。
#[derive(Debug)]
pub struct MovePlan {
    /// 用户所选目录下的最终数据根(= `picked/DATA_DIR_NAME`)。
    pub new_root: PathBuf,
    /// 同卷暂存目录(拷贝期间用,完工原子 rename 成 `new_root`)。
    staging: PathBuf,
    /// 源数据体积估算(字节)。
    pub need_bytes: u64,
    /// 目标盘剩余字节(0 = 查不到)。
    pub free_bytes: u64,
}

/// 去掉 Windows 长路径前缀(`\\?\C:\…` → `C:\…`;`\\?\UNC\srv\share` → `\\srv\share`)。
/// Rust `canonicalize` 在 Windows 恒返回 verbatim 形;Rust 自己的 fs 无所谓,但 **verbatim
/// 会关闭 Win32 的路径归一化(`/` 不再当 `\`)** —— sherpa/espeak 这类 C 库内部用 `/` 拼
/// 子路径(`espeak-ng-data/phontab`),在带前缀的数据根下全部「文件不存在」
/// (2026-07-03 克隆音色真机破案:搬家到 E 盘后指针带上了 `\\?\`)。
/// 数据根从这里出去 → 一律去前缀;只认盘符形与 UNC 形(canonicalize 的全部产出),
/// 其它 `\\?\` 设备路径原样保留防误伤。
pub fn simplify(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        let b = rest.as_bytes();
        if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
            return PathBuf::from(rest);
        }
    }
    p.to_path_buf()
}

/// 规范化路径(canonicalize 失败 —— 如目标尚不存在 —— 就用原值,尽力而为)。
/// ⚠️ macOS 的 `/var → /private/var` 等符号链接:**只能** canonicalize 存在的路径,
/// 故嵌套判断要先把存在的 `picked`/`current` 规范化,再从规范化的前缀拼出尚不存在的 `new_root`,
/// 否则一头带 `/private` 一头不带,`starts_with` 永远不命中。
/// Windows 侧顺手去 verbatim 前缀(见 `simplify`)—— 由此拼出的 new_root 写进指针即是干净形。
fn norm(p: &Path) -> PathBuf {
    simplify(&std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf()))
}

/// 预检:`current_root` = 当前数据根;`picked` = 用户选的目录。
pub fn precheck(current_root: &Path, picked: &Path) -> std::result::Result<MovePlan, MoveBlock> {
    // 先规范化两个存在的路径,再拼 new_root/staging —— 前缀比对才一致(见 norm 的符号链接注记)。
    let current_c = norm(current_root);
    let picked_c = norm(picked);
    let new_root = picked_c.join(DATA_DIR_NAME);
    let staging = picked_c.join(format!("{DATA_DIR_NAME}.moving"));

    if norm(&new_root) == current_c {
        return Err(MoveBlock::Same);
    }
    // 目标(staging/new_root 都在 picked 下)落在当前根内部 → 自吞,拦掉。
    if norm(&new_root).starts_with(&current_c) {
        return Err(MoveBlock::Overlap);
    }
    // 目标已存在且非空。
    let exists_nonempty = new_root
        .read_dir()
        .map(|mut d| d.next().is_some())
        .unwrap_or(false);
    if exists_nonempty {
        return Err(MoveBlock::Exists);
    }
    // 可写探测:能在 picked 建 staging 即可写;探完先清掉(真搬时再建)。
    if std::fs::create_dir_all(&staging).is_err() {
        return Err(MoveBlock::NotWritable);
    }
    std::fs::remove_dir_all(&staging).ok();

    let need_bytes = dir_size(current_root);
    let free_bytes = fs2::available_space(picked).unwrap_or(0);
    // 留 5% 余量;查不到空间(0)就不卡(交由真实拷贝兜底报错)。
    if free_bytes > 0 && free_bytes < need_bytes + need_bytes / 20 {
        return Err(MoveBlock::NoSpace);
    }
    Ok(MovePlan { new_root, staging, need_bytes, free_bytes })
}

/// 执行搬家(同步阻塞,调用方放 `spawn_blocking`)。成功 = staging 已原子改名为 `new_root`;
/// **不翻指针**(调用方成功后翻 + 重启,见 commands)。
pub fn perform_move(current_root: &Path, plan: &MovePlan, task: &TaskHandle) -> Result<()> {
    // staging 全新开始(预检留过 / 上次崩过都清掉)。
    if plan.staging.exists() {
        std::fs::remove_dir_all(&plan.staging).ok();
    }
    std::fs::create_dir_all(&plan.staging)?;

    // 1) 拷可重建 / 静态子树(媒体 / 语音 / 未来新域),跳过 DB(VACUUM 出)、指针、日志、staging。
    task.step_progress("step.relocate_copy", serde_json::Value::Null, 0.0);
    let total = plan.need_bytes.max(1);
    let mut copied: u64 = 0;
    let mut last_pct: i32 = -1;
    for entry in std::fs::read_dir(current_root)?.flatten() {
        let name = entry.file_name();
        if is_skip(&name) {
            continue;
        }
        copy_recursive(
            &entry.path(),
            &plan.staging.join(&name),
            total,
            &mut copied,
            &mut last_pct,
            task,
        )?;
    }

    // 2) DB 一致快照放最后(VACUUM INTO 要求目标不存在 → staging 全新所以一定不存在)。
    let src_db = current_root.join(DB_FILE);
    if src_db.is_file() {
        task.step("step.relocate_db", serde_json::Value::Null);
        vacuum_into(&src_db, &plan.staging.join(DB_FILE))?;
        if !plan.staging.join(DB_FILE).is_file() {
            bail!("搬家校验失败:目标缺少数据库");
        }
    }

    // 3) 原子就位(同卷 rename)。new_root 应不存在(预检保证非空才拦);稳妥起见空壳先删。
    task.step("step.relocate_commit", serde_json::Value::Null);
    if plan.new_root.exists() {
        std::fs::remove_dir(&plan.new_root).ok(); // 仅删空目录;非空预检已拦
    }
    std::fs::rename(&plan.staging, &plan.new_root).with_context(|| {
        format!("就位失败(rename {} → {})", plan.staging.display(), plan.new_root.display())
    })?;
    Ok(())
}

/// 清理旧数据根:删数据(DB + 媒体 / 语音 / 日志 + 残留 staging),**保留指针文件**
/// (指针永远住锚点;`old_root` 可能就是锚点)。`old_root != anchor`(= 专门数据文件夹)
/// 且清空后连壳删掉;锚点永远留着(住指针)。幂等。
pub fn cleanup_old(old_root: &Path, anchor: &Path) -> Result<()> {
    if !old_root.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(old_root)?.flatten() {
        let n = entry.file_name();
        let n = n.to_string_lossy();
        // 指针文件永不删(它住锚点;old_root 可能 == anchor)。
        if n == POINTER_FILE || n.ends_with(".json.tmp") {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            std::fs::remove_dir_all(&path).ok();
        } else {
            std::fs::remove_file(&path).ok();
        }
    }
    if norm(old_root) != norm(anchor) {
        std::fs::remove_dir(old_root).ok(); // 仅当已空才成功
    }
    Ok(())
}

/// 搬家时跳过的顶层条目:DB 三件套(VACUUM 出)、指针(住锚点)、日志(可弃,新位置重开)、
/// 暂存 / 临时文件。其余(媒体 / 语音 / 未来新域)一律带走 → 加新数据域无需改这里。
fn is_skip(name: &std::ffi::OsStr) -> bool {
    let n = name.to_string_lossy();
    n == DB_FILE
        || n == format!("{DB_FILE}-wal")
        || n == format!("{DB_FILE}-shm")
        || n == POINTER_FILE
        || n == "logs"
        || n.ends_with(".moving")
        || n.ends_with(".json.tmp")
}

fn copy_recursive(
    from: &Path,
    to: &Path,
    total: u64,
    copied: &mut u64,
    last_pct: &mut i32,
    task: &TaskHandle,
) -> Result<()> {
    let ft = std::fs::symlink_metadata(from)?.file_type();
    if ft.is_dir() {
        std::fs::create_dir_all(to)?;
        for e in std::fs::read_dir(from)?.flatten() {
            copy_recursive(&e.path(), &to.join(e.file_name()), total, copied, last_pct, task)?;
        }
    } else if ft.is_file() {
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let n = std::fs::copy(from, to).with_context(|| format!("拷贝失败: {}", from.display()))?;
        *copied += n;
        // 节流:百分比涨了才广播(媒体缓存可能成千小文件,别每个都刷 HUD)。
        let pct = ((*copied as f64 / total as f64) * 100.0) as i32;
        if pct > *last_pct {
            *last_pct = pct;
            task.progress((pct as f32 / 100.0).min(0.99));
        }
    }
    // symlink 等忽略(数据目录里不该有)。
    Ok(())
}

/// 独立连接读当前库(WAL 下读到最新已提交快照),`VACUUM INTO` 出一致、紧凑的单文件。
/// 要求 dest 不存在。单引号转义(文件名理论上可含 `'`)。
fn vacuum_into(src_db: &Path, dest_db: &Path) -> Result<()> {
    let conn = rusqlite::Connection::open(src_db)
        .with_context(|| format!("打开数据库失败: {}", src_db.display()))?;
    let dest = dest_db.to_string_lossy().replace('\'', "''");
    conn.execute(&format!("VACUUM INTO '{dest}'"), [])
        .context("VACUUM INTO 失败")?;
    Ok(())
}

/// 一键备份:在用户所选目录 `dest_dir` 生成 `larkwing-backup-<时间戳>.zip`,内含
/// **DB 一致快照**(`VACUUM INTO`,WAL 下也是完整已提交库)+ **克隆音色 wav**
/// (`voice/clones/`)—— 都是不可重建的用户数据。可重新下载的模型 / 缓存 / 媒体 / 日志
/// 一律不收(免备份包动辄上 G)。区别于「搬家」:不翻指针、不重启,纯导出一份拷贝。
/// 返回生成的压缩包绝对路径(前端提示「已备份到…」)。
pub fn backup_to(data_root: &Path, dest_dir: &Path) -> Result<PathBuf> {
    if !dest_dir.is_dir() {
        bail!("备份目录不存在: {}", dest_dir.display());
    }
    let src_db = data_root.join(DB_FILE);
    if !src_db.is_file() {
        bail!("找不到数据库: {}", src_db.display());
    }
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let zip_path = dest_dir.join(format!("larkwing-backup-{stamp}.zip"));
    // VACUUM 出一致快照到系统临时目录(读进 zip 后即删,不在用户所选目录里散落临时文件)。
    // 名带进程内序号:秒级时间戳在并发调用(测试并行实锤)下会同名互删。
    static SNAP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SNAP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let snap_db = std::env::temp_dir()
        .join(format!("lw-backup-{}-{stamp}-{seq}.db", std::process::id()));
    let _ = std::fs::remove_file(&snap_db);

    let build = || -> Result<()> {
        vacuum_into(&src_db, &snap_db)?;
        let file = std::fs::File::create(&zip_path)
            .with_context(|| format!("创建备份包失败: {}", zip_path.display()))?;
        let mut zip = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default();

        // 1) DB 一致快照。
        zip.start_file(DB_FILE, opts)?;
        std::io::copy(&mut std::fs::File::open(&snap_db)?, &mut zip)?;

        // 2) 克隆音色 wav(若有):data_root/voice/clones/*(DB 只存相对名,整目录带走即自洽)。
        let clones = data_root.join("voice").join("clones");
        if clones.is_dir() {
            for e in std::fs::read_dir(&clones)?.flatten() {
                let p = e.path();
                if p.is_file() {
                    zip.start_file(
                        format!("voice/clones/{}", e.file_name().to_string_lossy()),
                        opts,
                    )?;
                    std::io::copy(&mut std::fs::File::open(&p)?, &mut zip)?;
                }
            }
        }
        zip.finish()?;
        Ok(())
    };

    let result = build();
    let _ = std::fs::remove_file(&snap_db); // 不管成败都清临时快照
    if let Err(e) = result {
        let _ = std::fs::remove_file(&zip_path); // 失败别留半截包
        return Err(e);
    }
    Ok(zip_path)
}

// ---- 从备份恢复(2026-07-13):backup_to 的另一半 ----
//
// 运行中不能覆盖已打开的 DB(Windows 文件锁 + WAL),所以恢复分两拍:
//   ① 运行中:`restore_precheck` 校验 zip(结构 / SQLite 魔数 / 迁移版本前向检查)→ 用户确认
//      → `stage_restore` 把负载解到 `<root>/restore-pending/` → 调用方重启;
//   ② 下次 boot(开库前):`apply_pending_restore` 把现库(连 -wal/-shm)挪成
//      `larkwing.db.pre-restore-<时间戳>` 保险副本 → 备份库就位 → 克隆音色合并。
// 翻拍点 = staging 目录 rename 成 `restore-pending`(同卷原子);半截 `.tmp` 是垃圾,boot 清。
// 失败回滚保老数据权威(§3.5 不静默,结果由壳层带给前端弹提示)。

/// 待恢复负载目录名(在数据根下;boot 开库前查它)。
pub const RESTORE_PENDING: &str = "restore-pending";

/// 恢复预检失败原因(→ 前端 i18n code,同 MoveBlock 手法)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreBlock {
    /// 不是备份包(打不开 / 不是 zip / 里面没有数据库)。
    NotBackup,
    /// 包里的数据库读不出来(魔数不对 / 打不开 / 没有迁移记账表)。
    BadDb,
    /// 备份来自更新版本(库里有本版不认识的迁移 id)——老程序开新库会坏,拒绝。
    Newer,
}

impl RestoreBlock {
    /// 前端字典 key(`settings.system.restoreErr.<code>`)。
    pub fn code(self) -> &'static str {
        match self {
            RestoreBlock::NotBackup => "not_backup",
            RestoreBlock::BadDb => "bad_db",
            RestoreBlock::Newer => "newer",
        }
    }
}

/// 预检通过后的备份包概要(给前端确认弹窗)。
#[derive(Debug, Clone, Copy)]
pub struct RestoreCheck {
    /// 包内数据库(解压后)字节数。
    pub db_bytes: u64,
    /// 包内克隆音色文件个数。
    pub clones: u32,
}

const SQLITE_MAGIC: &[u8; 16] = b"SQLite format 3\0";

fn stamp() -> String {
    chrono::Local::now().format("%Y%m%d-%H%M%S").to_string()
}

/// 恢复预检(纯校验,不动数据根):zip 结构 + DB 魔数 + 迁移版本前向检查。
/// `known` = 本版程序认识的全部迁移 id(`store::migration_ids()`,参数传入保持 datadir 不依赖 store)。
pub fn restore_precheck(
    zip_path: &Path,
    known: &[&str],
) -> std::result::Result<RestoreCheck, RestoreBlock> {
    let file = std::fs::File::open(zip_path).map_err(|_| RestoreBlock::NotBackup)?;
    let mut z = zip::ZipArchive::new(file).map_err(|_| RestoreBlock::NotBackup)?;

    let mut db_bytes = 0u64;
    let mut has_db = false;
    let mut clones = 0u32;
    for i in 0..z.len() {
        let f = z.by_index(i).map_err(|_| RestoreBlock::NotBackup)?;
        if f.is_dir() {
            continue;
        }
        match f.enclosed_name() {
            Some(p) if p == Path::new(DB_FILE) => {
                has_db = true;
                db_bytes = f.size();
            }
            Some(p) if p.starts_with("voice/clones") => clones += 1,
            _ => {}
        }
    }
    if !has_db {
        return Err(RestoreBlock::NotBackup);
    }

    // DB 内容校验要开 SQLite → 解到系统临时文件,验完即删。
    // 名带进程内序号:秒级时间戳在并发调用(测试并行)下会同名互删(backup_to 同款)。
    static CHECK_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = CHECK_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = std::env::temp_dir().join(format!(
        "lw-restore-check-{}-{}-{seq}.db",
        std::process::id(),
        stamp()
    ));
    let _ = std::fs::remove_file(&tmp);
    let check = (|| -> std::result::Result<(), RestoreBlock> {
        {
            let mut entry = z.by_name(DB_FILE).map_err(|_| RestoreBlock::NotBackup)?;
            let mut out = std::fs::File::create(&tmp).map_err(|_| RestoreBlock::BadDb)?;
            std::io::copy(&mut entry, &mut out).map_err(|_| RestoreBlock::BadDb)?;
        }
        let head = {
            use std::io::Read;
            let mut buf = [0u8; 16];
            let mut f = std::fs::File::open(&tmp).map_err(|_| RestoreBlock::BadDb)?;
            f.read_exact(&mut buf).map_err(|_| RestoreBlock::BadDb)?;
            buf
        };
        if &head != SQLITE_MAGIC {
            return Err(RestoreBlock::BadDb);
        }
        let conn = rusqlite::Connection::open_with_flags(
            &tmp,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(|_| RestoreBlock::BadDb)?;
        // 迁移记账表是每个真库都有的(首启即建);读不到 = 不是我们的库。
        let ids: Vec<String> = conn
            .prepare("SELECT id FROM schema_migrations")
            .and_then(|mut s| {
                s.query_map([], |r| r.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .map_err(|_| RestoreBlock::BadDb)?;
        if ids.is_empty() {
            return Err(RestoreBlock::BadDb);
        }
        if ids.iter().any(|id| !known.contains(&id.as_str())) {
            return Err(RestoreBlock::Newer);
        }
        Ok(())
    })();
    let _ = std::fs::remove_file(&tmp);
    check.map(|_| RestoreCheck { db_bytes, clones })
}

/// 把备份负载解到 `<root>/restore-pending/`(先 `.tmp` 后原子改名 = 落定标记),供下次 boot 落位。
/// 只认识 DB 与 `voice/clones/`(其余条目跳过 = 老程序对未来备份成员向前兼容);
/// 条目路径走 `enclosed_name` 防 zip-slip。调用方应先跑过 `restore_precheck`。
pub fn stage_restore(data_root: &Path, zip_path: &Path) -> Result<()> {
    let pending = data_root.join(RESTORE_PENDING);
    let tmp = data_root.join(format!("{RESTORE_PENDING}.tmp"));
    for d in [&pending, &tmp] {
        if d.exists() {
            std::fs::remove_dir_all(d)
                .with_context(|| format!("清理旧恢复负载失败: {}", d.display()))?;
        }
    }
    std::fs::create_dir_all(&tmp)?;

    let file = std::fs::File::open(zip_path)
        .with_context(|| format!("打开备份包失败: {}", zip_path.display()))?;
    let mut z = zip::ZipArchive::new(file).context("读备份包失败")?;
    let mut got_db = false;
    for i in 0..z.len() {
        let mut f = z.by_index(i)?;
        if f.is_dir() {
            continue;
        }
        // enclosed_name = None 说明条目名带路径穿越,直接跳过(防 zip-slip)。
        let Some(rel) = f.enclosed_name() else { continue };
        let dest = if rel == Path::new(DB_FILE) {
            got_db = true;
            tmp.join(DB_FILE)
        } else if rel.starts_with("voice/clones") {
            let Some(name) = rel.file_name() else { continue };
            let dir = tmp.join("voice").join("clones");
            std::fs::create_dir_all(&dir)?;
            dir.join(name)
        } else {
            tracing::debug!(entry = %rel.display(), "备份包里不认识的条目,跳过");
            continue;
        };
        let mut out = std::fs::File::create(&dest)
            .with_context(|| format!("写恢复负载失败: {}", dest.display()))?;
        std::io::copy(&mut f, &mut out)?;
    }
    if !got_db {
        bail!("备份包里没有数据库");
    }
    std::fs::rename(&tmp, &pending).context("恢复负载落定失败")?;
    Ok(())
}

/// boot 最先(开库前)调:有待恢复负载就把它换进来。
/// - `None` = 没有待恢复负载,无事;
/// - `Some(Ok(()))` = 恢复成功(原库留保险副本 `larkwing.db.pre-restore-<时间戳>`);
/// - `Some(Err)` = 失败,已尽力回滚,老数据仍权威(负载已清,下次 boot 不再撞)。
pub fn apply_pending_restore(data_root: &Path) -> Option<std::result::Result<(), String>> {
    // rename 落定前崩掉的半截 staging = 垃圾,顺手清。
    let half = data_root.join(format!("{RESTORE_PENDING}.tmp"));
    if half.exists() {
        std::fs::remove_dir_all(&half).ok();
    }
    let pending = data_root.join(RESTORE_PENDING);
    if !pending.is_dir() {
        return None;
    }
    let out = apply_restore_inner(data_root, &pending).map_err(|e| format!("{e:#}"));
    // 成败都清负载:成功 = 已消费;失败 = 别让每次启动反复撞同一坨坏负载。
    std::fs::remove_dir_all(&pending).ok();
    Some(out)
}

fn apply_restore_inner(data_root: &Path, pending: &Path) -> Result<()> {
    let staged_db = pending.join(DB_FILE);
    if !staged_db.is_file() {
        bail!("待恢复负载缺数据库");
    }
    let ts = stamp();

    // 1) 现库连 -wal/-shm 一起挪成保险副本(WAL 里可能有已提交未 checkpoint 的数据,必须跟着走;
    //    后缀保持 `<副本名>-wal` 配对,保险副本本身仍是可直接打开的库)。挪走也顺带保证
    //    新库就位后不会被旧 WAL 污染。记下动过的对,失败逆序滚回去。
    let mut moved: Vec<(PathBuf, PathBuf)> = Vec::new();
    let safety = data_root.join(format!("{DB_FILE}.pre-restore-{ts}"));
    let with_suffix = |base: &Path, suffix: &str| -> PathBuf {
        let mut s = base.as_os_str().to_os_string();
        s.push(suffix);
        PathBuf::from(s)
    };
    for (from, to) in [
        (data_root.join(DB_FILE), safety.clone()),
        (data_root.join(format!("{DB_FILE}-wal")), with_suffix(&safety, "-wal")),
        (data_root.join(format!("{DB_FILE}-shm")), with_suffix(&safety, "-shm")),
    ] {
        if !from.exists() {
            continue; // 全新机器上恢复(还没有库)也是合法路
        }
        if let Err(e) = std::fs::rename(&from, &to) {
            for (f, t) in moved.iter().rev() {
                std::fs::rename(t, f).ok();
            }
            return Err(e).with_context(|| format!("挪保险副本失败: {}", from.display()));
        }
        moved.push((from, to));
    }

    // 2) 备份库就位;失败把保险副本滚回来(老数据仍权威)。
    if let Err(e) = std::fs::rename(&staged_db, data_root.join(DB_FILE)) {
        for (f, t) in moved.iter().rev() {
            std::fs::rename(t, f).ok();
        }
        return Err(e).context("恢复就位失败");
    }

    // 3) 克隆音色合并:同名以备份为准(恢复回来的 DB 指向的就是备份里那份),现有多出来的
    //    留着(孤儿 wav 无害)。库已就位,单个 wav 失败只记日志不翻整单。
    let src = pending.join("voice").join("clones");
    if src.is_dir() {
        let dst_dir = data_root.join("voice").join("clones");
        std::fs::create_dir_all(&dst_dir).ok();
        if let Ok(rd) = std::fs::read_dir(&src) {
            for e in rd.flatten() {
                let p = e.path();
                if !p.is_file() {
                    continue;
                }
                let dest = dst_dir.join(e.file_name());
                if dest.exists() {
                    std::fs::remove_file(&dest).ok();
                }
                if let Err(err) = std::fs::rename(&p, &dest) {
                    tracing::warn!(file = %dest.display(), err = %err, "恢复克隆音色失败(跳过)");
                }
            }
        }
    }
    Ok(())
}

fn dir_size(root: &Path) -> u64 {
    fn walk(p: &Path, acc: &mut u64) {
        if let Ok(rd) = std::fs::read_dir(p) {
            for e in rd.flatten() {
                match e.file_type() {
                    Ok(ft) if ft.is_dir() => walk(&e.path(), acc),
                    Ok(ft) if ft.is_file() => *acc += e.metadata().map(|m| m.len()).unwrap_or(0),
                    _ => {}
                }
            }
        }
    }
    let mut acc = 0;
    walk(root, &mut acc);
    acc
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::Bus;
    use crate::tasks::Tasks;

    /// verbatim 前缀剥离(克隆音色真机破案的修法;字符串级,Mac 也可测)。
    #[test]
    fn simplify_strips_windows_verbatim_prefixes() {
        assert_eq!(simplify(Path::new(r"\\?\E:\a\b")), PathBuf::from(r"E:\a\b"));
        assert_eq!(
            simplify(Path::new(r"\\?\UNC\nas\share\x")),
            PathBuf::from(r"\\nas\share\x"),
            "UNC verbatim 还原成 \\\\server\\share 形"
        );
        assert_eq!(simplify(Path::new(r"C:\plain")), PathBuf::from(r"C:\plain"), "普通盘符不动");
        assert_eq!(simplify(Path::new("/unix/path")), PathBuf::from("/unix/path"), "unix 路径不动");
        // 非盘符的 \\?\ 设备路径(canonicalize 不会产)原样保留,防误伤
        assert_eq!(
            simplify(Path::new(r"\\?\Volume{abc}\y")),
            PathBuf::from(r"\\?\Volume{abc}\y")
        );
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("lw-datadir-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn a_task() -> TaskHandle {
        Tasks::new(Bus::new()).start("relocate", crate::bus::Text::new("x"))
    }

    #[test]
    fn pointer_roundtrip_and_missing_file_is_default() {
        let anchor = tmp("ptr");
        assert!(read_pointer(&anchor).data_root.is_none(), "无文件 = 默认空");
        let p = Pointer { data_root: Some("/d/Larkwing".into()), old_root: Some("/c/old".into()) };
        write_pointer(&anchor, &p).unwrap();
        let back = read_pointer(&anchor);
        assert_eq!(back.data_root.as_deref(), Some("/d/Larkwing"));
        assert_eq!(back.old_root.as_deref(), Some("/c/old"));
    }

    #[test]
    fn backup_produces_zip_with_db_snapshot_and_clones() {
        use std::io::Read;
        let root = tmp("backup-src");
        // 造一个真库(VACUUM INTO 要求合法 sqlite)。
        {
            let c = rusqlite::Connection::open(root.join(DB_FILE)).unwrap();
            c.execute_batch("CREATE TABLE t(x); INSERT INTO t VALUES (1);").unwrap();
        }
        // 造一个克隆音色 wav(应被收进包)。
        let clones = root.join("voice").join("clones");
        std::fs::create_dir_all(&clones).unwrap();
        std::fs::write(clones.join("c1.wav"), b"RIFFfake").unwrap();
        // 可重建的(模型/日志)不该进包。
        std::fs::create_dir_all(root.join("logs")).unwrap();
        std::fs::write(root.join("logs").join("app.log"), b"noise").unwrap();

        let dest = tmp("backup-dest");
        let zip_path = backup_to(&root, &dest).unwrap();
        assert!(zip_path.is_file(), "应生成 zip");
        assert_eq!(zip_path.extension().and_then(|e| e.to_str()), Some("zip"));

        let mut z = zip::ZipArchive::new(std::fs::File::open(&zip_path).unwrap()).unwrap();
        let names: Vec<String> =
            (0..z.len()).map(|i| z.by_index(i).unwrap().name().to_string()).collect();
        assert!(names.iter().any(|n| n == DB_FILE), "含 DB 快照: {names:?}");
        assert!(names.iter().any(|n| n == "voice/clones/c1.wav"), "含克隆音色: {names:?}");
        assert!(!names.iter().any(|n| n.contains("logs")), "日志不该进包: {names:?}");
        // DB 快照是合法 sqlite(头 16 字节魔数)。
        let mut buf = vec![];
        z.by_name(DB_FILE).unwrap().read_to_end(&mut buf).unwrap();
        assert!(buf.len() > 16 && &buf[0..16] == b"SQLite format 3\0", "DB 快照应为合法 sqlite");
    }

    #[test]
    fn resolve_three_states() {
        let anchor = tmp("resolve");
        // 1) 没搬过家 → 用锚点
        let r = resolve(&anchor);
        assert_eq!(r.root, anchor);
        assert!(r.missing.is_none());

        // 2) 搬过家、目录在 → 用记的路径
        let moved = tmp("resolve-moved");
        write_pointer(
            &anchor,
            &Pointer { data_root: Some(moved.to_string_lossy().into()), old_root: None },
        )
        .unwrap();
        let r = resolve(&anchor);
        assert_eq!(norm(&r.root), norm(&moved));
        assert!(r.missing.is_none());

        // 3) 搬过家、目录没了(盘没插)→ missing,root 回落锚点
        let gone = anchor.join("definitely-not-here");
        write_pointer(
            &anchor,
            &Pointer { data_root: Some(gone.to_string_lossy().into()), old_root: None },
        )
        .unwrap();
        let r = resolve(&anchor);
        assert_eq!(r.root, anchor, "失效 → 回落锚点");
        assert_eq!(r.missing.as_deref().map(norm), Some(norm(&gone)));
    }

    #[test]
    fn precheck_rejects_same_overlap_and_nonempty() {
        let base = tmp("precheck");
        let current = base.join("current");
        std::fs::create_dir_all(&current).unwrap();

        // Same:current 自己叫 Larkwing,选它的父 = 目标算回 current
        let lk = base.join(DATA_DIR_NAME);
        std::fs::create_dir_all(&lk).unwrap();
        assert_eq!(precheck(&lk, &base).unwrap_err(), MoveBlock::Same);

        // Overlap:把数据搬进当前根内部
        assert_eq!(precheck(&current, &current).unwrap_err(), MoveBlock::Overlap);

        // Exists:目标 picked/Larkwing 已存在且非空
        let picked = base.join("picked");
        std::fs::create_dir_all(picked.join(DATA_DIR_NAME)).unwrap();
        std::fs::write(picked.join(DATA_DIR_NAME).join("x"), b"1").unwrap();
        assert_eq!(precheck(&current, &picked).unwrap_err(), MoveBlock::Exists);
    }

    #[test]
    fn perform_move_copies_subtrees_vacuums_db_and_skips_pointer_logs() {
        let base = tmp("move");
        let current = base.join("data");
        std::fs::create_dir_all(current.join("voice/models/foo")).unwrap();
        std::fs::write(current.join("voice/models/foo/m.onnx"), vec![7u8; 4096]).unwrap();
        std::fs::create_dir_all(current.join("logs")).unwrap();
        std::fs::write(current.join("logs/larkwing.log"), b"noise").unwrap();
        std::fs::write(current.join(POINTER_FILE), b"{}").unwrap(); // 指针(若恰在根)不该被带走
        // 造个真 sqlite 让 VACUUM INTO 有活干
        {
            let c = rusqlite::Connection::open(current.join(DB_FILE)).unwrap();
            c.execute_batch("CREATE TABLE t(x); INSERT INTO t VALUES (1),(2);").unwrap();
        }

        let picked = base.join("dest");
        std::fs::create_dir_all(&picked).unwrap();
        let plan = precheck(&current, &picked).unwrap();
        let task = a_task();
        perform_move(&current, &plan, &task).unwrap();
        task.done();

        let new_root = picked.join(DATA_DIR_NAME);
        assert!(new_root.join("voice/models/foo/m.onnx").is_file(), "子树带走");
        assert!(new_root.join(DB_FILE).is_file(), "DB 经 VACUUM 出现");
        assert!(!new_root.join("logs").exists(), "日志不搬");
        assert!(!new_root.join(POINTER_FILE).exists(), "指针不搬");
        // 搬来的 DB 能打开且数据在
        let c = rusqlite::Connection::open(new_root.join(DB_FILE)).unwrap();
        let n: i64 = c.query_row("SELECT count(*) FROM t", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 2);
    }

    #[test]
    fn cleanup_old_preserves_pointer_and_removes_dedicated_dir() {
        let anchor = tmp("cleanup-anchor");
        // old_root == anchor:删数据留指针,目录本身留着
        std::fs::write(anchor.join(POINTER_FILE), b"{}").unwrap();
        std::fs::write(anchor.join(DB_FILE), b"db").unwrap();
        std::fs::create_dir_all(anchor.join("voice")).unwrap();
        cleanup_old(&anchor, &anchor).unwrap();
        assert!(anchor.join(POINTER_FILE).is_file(), "指针保留");
        assert!(!anchor.join(DB_FILE).exists(), "DB 删除");
        assert!(!anchor.join("voice").exists(), "子树删除");
        assert!(anchor.is_dir(), "锚点目录留着");

        // old_root != anchor:清空后连壳删掉
        let other = anchor.join("OldLarkwing");
        std::fs::create_dir_all(other.join("media")).unwrap();
        std::fs::write(other.join(DB_FILE), b"db").unwrap();
        cleanup_old(&other, &anchor).unwrap();
        assert!(!other.exists(), "专属数据文件夹整个删掉");
    }

    // ---- 从备份恢复 ----

    /// 造一个带迁移记账表的真库(恢复预检要读 schema_migrations)。
    fn mk_db(path: &Path, marker: i64, migration_ids: &[&str]) {
        let c = rusqlite::Connection::open(path).unwrap();
        c.execute_batch(
            "CREATE TABLE schema_migrations(id TEXT PRIMARY KEY, applied_at INTEGER NOT NULL);
             CREATE TABLE t(x);",
        )
        .unwrap();
        for id in migration_ids {
            c.execute("INSERT INTO schema_migrations VALUES(?1, 1)", [id]).unwrap();
        }
        c.execute("INSERT INTO t VALUES(?1)", [marker]).unwrap();
    }

    fn zip_with(dest: &Path, entries: &[(&str, &[u8])]) {
        use std::io::Write;
        let f = std::fs::File::create(dest).unwrap();
        let mut z = zip::ZipWriter::new(f);
        let opts = zip::write::SimpleFileOptions::default();
        for (name, bytes) in entries {
            z.start_file(*name, opts).unwrap();
            z.write_all(bytes).unwrap();
        }
        z.finish().unwrap();
    }

    #[test]
    fn restore_precheck_gates_garbage_bad_db_and_newer() {
        let base = tmp("restore-precheck");
        let known = ["0001_test"];

        // 非 zip
        let junk = base.join("junk.zip");
        std::fs::write(&junk, b"not a zip at all").unwrap();
        assert_eq!(restore_precheck(&junk, &known).unwrap_err(), RestoreBlock::NotBackup);

        // zip 里没有数据库
        let nodb = base.join("nodb.zip");
        zip_with(&nodb, &[("readme.txt", b"hi")]);
        assert_eq!(restore_precheck(&nodb, &known).unwrap_err(), RestoreBlock::NotBackup);

        // 数据库魔数不对
        let baddb = base.join("baddb.zip");
        zip_with(&baddb, &[(DB_FILE, b"definitely not sqlite")]);
        assert_eq!(restore_precheck(&baddb, &known).unwrap_err(), RestoreBlock::BadDb);

        // 备份来自更新版本(有本版不认识的迁移 id)
        let newer_db = base.join("newer.db");
        mk_db(&newer_db, 1, &["0001_test", "9999_future"]);
        let newer = base.join("newer.zip");
        zip_with(&newer, &[(DB_FILE, &std::fs::read(&newer_db).unwrap())]);
        assert_eq!(restore_precheck(&newer, &known).unwrap_err(), RestoreBlock::Newer);

        // 正常包:概要数出克隆音色
        let ok_db = base.join("ok.db");
        mk_db(&ok_db, 1, &["0001_test"]);
        let okzip = base.join("ok.zip");
        zip_with(
            &okzip,
            &[
                (DB_FILE, &std::fs::read(&ok_db).unwrap()),
                ("voice/clones/c1.wav", b"RIFFxxxx"),
            ],
        );
        let info = restore_precheck(&okzip, &known).unwrap();
        assert!(info.db_bytes > 0);
        assert_eq!(info.clones, 1);
    }

    #[test]
    fn restore_roundtrip_swaps_db_keeps_safety_and_merges_clones() {
        // A 机:真数据 → backup_to 出包
        let root_a = tmp("restore-a");
        mk_db(&root_a.join(DB_FILE), 42, &["0001_test"]);
        std::fs::create_dir_all(root_a.join("voice/clones")).unwrap();
        std::fs::write(root_a.join("voice/clones/c1.wav"), b"AAA").unwrap();
        let dest = tmp("restore-zipdest");
        let zip = backup_to(&root_a, &dest).unwrap();

        // B 机:另一份现数据(同名克隆内容不同 + 多一个自己的 + 残留 -wal)
        let root_b = tmp("restore-b");
        mk_db(&root_b.join(DB_FILE), 7, &["0001_test"]);
        std::fs::write(root_b.join(format!("{DB_FILE}-wal")), b"stale wal").unwrap();
        std::fs::create_dir_all(root_b.join("voice/clones")).unwrap();
        std::fs::write(root_b.join("voice/clones/c1.wav"), b"BBB").unwrap();
        std::fs::write(root_b.join("voice/clones/c2.wav"), b"CCC").unwrap();

        // 预检 → 暂存 → boot 落位
        let info = restore_precheck(&zip, &["0001_test"]).unwrap();
        assert_eq!(info.clones, 1);
        stage_restore(&root_b, &zip).unwrap();
        assert!(root_b.join(RESTORE_PENDING).join(DB_FILE).is_file(), "负载落定");
        let outcome = apply_pending_restore(&root_b).expect("有负载应返回结果");
        outcome.expect("落位应成功");

        // 库换成了备份的内容
        let c = rusqlite::Connection::open(root_b.join(DB_FILE)).unwrap();
        let v: i64 = c.query_row("SELECT x FROM t", [], |r| r.get(0)).unwrap();
        assert_eq!(v, 42, "恢复后应是备份里的数据");
        // 原库(连 -wal)成保险副本;live 旁不再有旧 wal
        let names: Vec<String> = std::fs::read_dir(&root_b)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names.iter().any(|n| n.starts_with(&format!("{DB_FILE}.pre-restore-")) && !n.ends_with("-wal")),
            "应留保险副本: {names:?}"
        );
        assert!(
            names.iter().any(|n| n.starts_with(&format!("{DB_FILE}.pre-restore-")) && n.ends_with("-wal")),
            "旧 wal 跟着保险副本走: {names:?}"
        );
        assert!(!names.iter().any(|n| n == &format!("{DB_FILE}-wal")), "live 旁不残留旧 wal");
        // 克隆音色:同名以备份为准,多出来的留着;负载目录已收摊
        assert_eq!(std::fs::read(root_b.join("voice/clones/c1.wav")).unwrap(), b"AAA");
        assert_eq!(std::fs::read(root_b.join("voice/clones/c2.wav")).unwrap(), b"CCC");
        assert!(!root_b.join(RESTORE_PENDING).exists(), "负载已消费");
        // 保险副本本身是能打开的库(带着自己的 wal 配对名)
        let safety = std::fs::read_dir(&root_b)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .find(|p| {
                let n = p.file_name().unwrap().to_string_lossy().into_owned();
                n.starts_with(&format!("{DB_FILE}.pre-restore-")) && !n.ends_with("-wal") && !n.ends_with("-shm")
            })
            .unwrap();
        let c2 = rusqlite::Connection::open(&safety).unwrap();
        let v2: i64 = c2.query_row("SELECT x FROM t", [], |r| r.get(0)).unwrap();
        assert_eq!(v2, 7, "保险副本 = 恢复前的数据");
    }

    #[test]
    fn apply_pending_restore_noop_without_payload_and_cleans_half_staging() {
        let root = tmp("restore-noop");
        std::fs::create_dir_all(root.join(format!("{RESTORE_PENDING}.tmp"))).unwrap();
        assert!(apply_pending_restore(&root).is_none(), "无负载 = None");
        assert!(!root.join(format!("{RESTORE_PENDING}.tmp")).exists(), "半截 staging 清掉");
    }

    #[test]
    fn stage_restore_skips_zip_slip_entries() {
        let base = tmp("restore-slip");
        let root = base.join("data");
        std::fs::create_dir_all(&root).unwrap();
        let db = base.join("ok.db");
        mk_db(&db, 1, &["0001_test"]);
        let zip = base.join("slip.zip");
        zip_with(
            &zip,
            &[
                (DB_FILE, &std::fs::read(&db).unwrap()),
                ("../evil.wav", b"x"),
                ("voice/clones/../../evil2.wav", b"y"),
            ],
        );
        stage_restore(&root, &zip).unwrap();
        assert!(root.join(RESTORE_PENDING).join(DB_FILE).is_file());
        assert!(!base.join("evil.wav").exists(), "路径穿越条目不落盘");
        assert!(!root.join("evil2.wav").exists(), "路径穿越条目不落盘");
        assert!(!root.join(RESTORE_PENDING).join("evil2.wav").exists());
    }
}
