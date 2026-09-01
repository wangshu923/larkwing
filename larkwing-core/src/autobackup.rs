//! 自动备份(水位线):距上次成功备份超过 `INTERVAL_MS` 就把数据备到用户指定目录,
//! 并按份数轮转(保留最近 `BACKUP_KEEP` 份)。**纯机制,不走 jobs/scheduler、不经模型**
//! (§5 三物种:这不是模型的手脚,是产品自身的数据保障;家庭日记同款水位线哲学 ——
//! 桌面程序没有「每天定点活着」的假设,cron 式「每周日 3 点」大概率错过,水位线开机即补)。
//!
//! 开关语义:settings `backup.auto.dir` 非空 = 开(用户选了目标目录),清空 = 关。
//! **默认关**不违背强默认 —— 备份到数据盘自身是假安全,目标必须是另一块盘/目录,
//! 只有用户知道备到哪,没法开箱即开(§4.11,2026-08-31 用户拍板:10 份 / 每周 / 默认关)。
//!
//! 轮转纪律(用户拍板「机器清理、绝不经 LLM」):只认自己命名格式
//! `larkwing-backup-YYYYMMDD-HHMMSS.zip` 的文件,**先备后删**、只删超出份数的最老几个;
//! 用户改过名的(如 `larkwing-backup-换机前.zip`)不匹配格式 → 永不删(改名 = 钉住)。
//! 直接删、不进回收站:fs_trash 的回收站是给「模型动用户文件」的可逆保障,这里是程序
//! 删自己的产物、且前面还有整排还原点(Time Machine / Windows 文件历史轮转同款)。
//!
//! 失败纪律(§3.5 不静默、也不烦人):单次失败只 warn 静候下轮;超期 ≥`NAG_AFTER_MS`
//! 且本次尝试也失败,才经 Bus 发一次提示(前端 toast「目标盘还在吗」),`NAG_EVERY_MS` 频控。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::bus::{AppEvent, BackupNote, Bus};
use crate::store::Store;

/// 保留最近几份(§4.11 用户拍板 2026-08-31)。按份数不按天数:按天数保留在低频备份下
/// 会把旧备份删光(每月备 + 保 30 天 = 只剩 1 份),按份数则不管什么频率永远有整排还原点。
pub const BACKUP_KEEP: usize = 10;
/// 备份周期:距上次成功超过它就该备了(每周,§4.11 用户拍板)。
pub const INTERVAL_MS: i64 = 7 * 24 * 3_600_000;
/// 循环节拍:备份粒度是天级,10 分钟一查足够;首查延迟避开开机高峰。
const CHECK_EVERY: std::time::Duration = std::time::Duration::from_secs(10 * 60);
const FIRST_DELAY: std::time::Duration = std::time::Duration::from_secs(3 * 60);
/// 超期这么久且本次尝试也失败,才向用户提示一次(目标盘拔了/目录没了)。
const NAG_AFTER_MS: i64 = 14 * 24 * 3_600_000;
/// 提示频控:最多每 3 天烦一次。
const NAG_EVERY_MS: i64 = 3 * 24 * 3_600_000;

/// 目标目录(app 级 settings;非空 = 开)。§6.8:前端 DEFAULTS 与 APP_SETTING_KEYS 两边各一行。
pub const DIR_KEY: &str = "backup.auto.dir";
/// 水位线:上次**成功**备份的毫秒时刻(内部状态,core 自己读写)。
const LAST_OK_KEY: &str = "backup.auto.last_ok_ms";
/// 上次失败原因(设置页展示;成功后清空)。
const LAST_ERR_KEY: &str = "backup.auto.last_error";
/// 上次向用户提示的时刻(频控)。
const LAST_NAG_KEY: &str = "backup.auto.last_nag_ms";

/// 自动备份器:壳层 boot 建一份放 AppState(命令「立即备份」与后台循环共享防重入旗标)。
#[derive(Clone)]
pub struct AutoBackup {
    store: Store,
    data_root: PathBuf,
    bus: Bus,
    inflight: Arc<AtomicBool>,
}

/// 设置页状态视图(命令 `auto_backup_status` 过桥)。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AutoBackupStatus {
    pub dir: Option<String>,
    pub last_ok_ms: Option<i64>,
    pub last_error: Option<String>,
}

impl AutoBackup {
    pub fn new(store: Store, data_root: PathBuf, bus: Bus) -> AutoBackup {
        AutoBackup { store, data_root, bus, inflight: Arc::new(AtomicBool::new(false)) }
    }

    /// 常驻循环(壳层 `tauri::async_runtime::spawn` 挂起)。
    pub async fn run(self) {
        tokio::time::sleep(FIRST_DELAY).await;
        tracing::info!("自动备份在线(水位线,每 {CHECK_EVERY:?} 查一次)");
        loop {
            let now = chrono::Local::now().timestamp_millis();
            self.tick(now, false).await;
            tokio::time::sleep(CHECK_EVERY).await;
        }
    }

    /// 单步(可测):没配置/没到期/在跑 = 静默返回 None;跑了 = Some(成败)。
    /// `force` = 用户刚选完目录立即出第一份(不等水位)。
    pub async fn tick(&self, now: i64, force: bool) -> Option<Result<PathBuf>> {
        let dir = self.setting(DIR_KEY).filter(|s| !s.trim().is_empty())?;
        let last_ok = self.setting_i64(LAST_OK_KEY);
        if !force && now - last_ok < INTERVAL_MS {
            return None; // 还没到期
        }
        if self.inflight.swap(true, Ordering::SeqCst) {
            return None; // 已有一趟在跑(zip 大克隆音色可能要一会儿)
        }
        let result = self.backup_and_prune(&dir).await;
        self.inflight.store(false, Ordering::SeqCst);

        match &result {
            Ok(zip) => {
                let _ = self.store.settings.set(None, LAST_OK_KEY, &now.to_string());
                let _ = self.store.settings.set(None, LAST_ERR_KEY, "");
                tracing::info!(zip = %zip.display(), "自动备份完成");
                // 成功不打扰(设置页自会显示「上次备份」);事件仍发,前端刷新状态用。
                self.bus.publish(AppEvent::Backup(BackupNote { ok: true, stale: false }));
            }
            Err(e) => {
                let _ = self.store.settings.set(None, LAST_ERR_KEY, &format!("{e:#}"));
                tracing::warn!(err = %e, dir = %dir, "自动备份失败(下轮再试)");
                // 超期太久才提示,且频控 —— 单次失败(盘暂时没插)不烦人。
                let stale = now - last_ok >= NAG_AFTER_MS;
                let nagged_recently = now - self.setting_i64(LAST_NAG_KEY) < NAG_EVERY_MS;
                if stale && !nagged_recently {
                    let _ = self.store.settings.set(None, LAST_NAG_KEY, &now.to_string());
                    self.bus.publish(AppEvent::Backup(BackupNote { ok: false, stale: true }));
                }
            }
        }
        Some(result)
    }

    /// 备一份 + 轮转(阻塞活,丢线程池)。先备后删:新份成功落盘才清最老的,
    /// 永不出现「删了旧的、新的又失败 → 一份都没有」。
    async fn backup_and_prune(&self, dir: &str) -> Result<PathBuf> {
        let root = self.data_root.clone();
        let dest = PathBuf::from(dir);
        tokio::task::spawn_blocking(move || -> Result<PathBuf> {
            let zip = crate::datadir::backup_to(&root, &dest)?;
            match prune_backups(&dest, BACKUP_KEEP) {
                Ok(removed) if !removed.is_empty() => {
                    tracing::info!(n = removed.len(), "轮转清理旧备份");
                }
                Ok(_) => {}
                // 轮转失败不翻整趟备份(新份已落盘 = 本职完成);下轮再清。
                Err(e) => tracing::warn!(err = %e, "轮转清理失败(下轮再清)"),
            }
            Ok(zip)
        })
        .await
        .context("备份任务没跑完")?
    }

    /// 设置页状态。
    pub fn status(&self) -> AutoBackupStatus {
        AutoBackupStatus {
            dir: self.setting(DIR_KEY).filter(|s| !s.trim().is_empty()),
            last_ok_ms: match self.setting_i64(LAST_OK_KEY) {
                0 => None,
                v => Some(v),
            },
            last_error: self.setting(LAST_ERR_KEY).filter(|s| !s.trim().is_empty()),
        }
    }

    fn setting(&self, key: &str) -> Option<String> {
        self.store.settings.get(None, key).ok().flatten()
    }

    fn setting_i64(&self, key: &str) -> i64 {
        self.setting(key).and_then(|s| s.trim().parse().ok()).unwrap_or(0)
    }
}

/// 轮转:目录里自家命名格式的备份包按时间戳排序,删到只剩 `keep` 份;返回删掉的。
/// 只按**文件名**时间戳排序(拷贝会污染 mtime,文件名是我们自己写的真相);
/// 解析不出时间戳的(用户改过名)一律跳过 = 钉住。
pub fn prune_backups(dir: &Path, keep: usize) -> Result<Vec<PathBuf>> {
    let mut stamped: Vec<(String, PathBuf)> = Vec::new();
    for e in std::fs::read_dir(dir)
        .with_context(|| format!("读不了备份目录 {}", dir.display()))?
        .flatten()
    {
        let p = e.path();
        if !p.is_file() {
            continue;
        }
        let Some(name) = p.file_name().and_then(|s| s.to_str()) else { continue };
        if let Some(stamp) = parse_backup_stamp(name) {
            stamped.push((stamp.to_string(), p));
        }
    }
    if stamped.len() <= keep {
        return Ok(Vec::new());
    }
    // 固定宽度时间戳,字典序 = 时间序;老的在前。
    stamped.sort_by(|a, b| a.0.cmp(&b.0));
    let n_remove = stamped.len() - keep;
    let mut removed = Vec::new();
    for (_, p) in stamped.into_iter().take(n_remove) {
        std::fs::remove_file(&p).with_context(|| format!("删不掉旧备份 {}", p.display()))?;
        removed.push(p);
    }
    Ok(removed)
}

/// 从文件名解析自家备份时间戳:`larkwing-backup-YYYYMMDD-HHMMSS.zip` → `YYYYMMDD-HHMMSS`。
/// 格式对不上(改过名/别的文件)→ None,轮转永不碰它。
fn parse_backup_stamp(name: &str) -> Option<&str> {
    let stamp = name.strip_prefix("larkwing-backup-")?.strip_suffix(".zip")?;
    let b = stamp.as_bytes();
    if b.len() != 15 || b[8] != b'-' {
        return None;
    }
    let digits_ok = b[..8].iter().all(u8::is_ascii_digit) && b[9..].iter().all(u8::is_ascii_digit);
    digits_ok.then_some(stamp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamp_parses_only_own_format() {
        assert_eq!(
            parse_backup_stamp("larkwing-backup-20260831-120000.zip"),
            Some("20260831-120000")
        );
        // 改名 = 钉住;别的文件不碰
        assert_eq!(parse_backup_stamp("larkwing-backup-换机前.zip"), None);
        assert_eq!(parse_backup_stamp("larkwing-backup-20260831.zip"), None);
        assert_eq!(parse_backup_stamp("photo.zip"), None);
        assert_eq!(parse_backup_stamp("larkwing-backup-2026083a-120000.zip"), None);
    }

    #[test]
    fn prune_keeps_newest_and_pinned() {
        let dir = std::env::temp_dir().join(format!("lw-abk-prune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // 12 份合法命名(01..12 日)+ 1 份改名 + 1 份无关文件
        for d in 1..=12 {
            std::fs::write(dir.join(format!("larkwing-backup-202608{d:02}-090000.zip")), b"x")
                .unwrap();
        }
        std::fs::write(dir.join("larkwing-backup-搬家前留的.zip"), b"x").unwrap();
        std::fs::write(dir.join("notes.txt"), b"x").unwrap();

        let removed = prune_backups(&dir, 10).unwrap();
        assert_eq!(removed.len(), 2, "12 份留 10,删最老 2 份");
        assert!(!dir.join("larkwing-backup-20260801-090000.zip").exists());
        assert!(!dir.join("larkwing-backup-20260802-090000.zip").exists());
        assert!(dir.join("larkwing-backup-20260803-090000.zip").exists());
        assert!(dir.join("larkwing-backup-20260812-090000.zip").exists(), "最新的在");
        assert!(dir.join("larkwing-backup-搬家前留的.zip").exists(), "改名 = 钉住");
        assert!(dir.join("notes.txt").exists(), "无关文件不碰");
        // 已在份数内 → 不删
        assert!(prune_backups(&dir, 10).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn tick_respects_config_watermark_and_force() {
        let dir = std::env::temp_dir().join(format!("lw-abk-tick-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // backup_to 认数据根下固定名的库(datadir::DB_FILE)——测试根里就得叫这个名
        let store = Store::open(&dir.join("larkwing.db")).unwrap();
        store.users.ensure_default_user().unwrap();
        let dest = dir.join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        let ab = AutoBackup::new(store.clone(), dir.clone(), Bus::new());

        // 没配目录 → 静默不跑
        assert!(ab.tick(INTERVAL_MS * 2, false).await.is_none());

        // 配了目录、水位 0 → 到期,真备一份 + 写水位
        store.settings.set(None, DIR_KEY, dest.to_string_lossy().as_ref()).unwrap();
        let now = INTERVAL_MS * 2;
        let zip = ab.tick(now, false).await.expect("到期该跑").expect("备份成功");
        assert!(zip.is_file());
        assert_eq!(ab.status().last_ok_ms, Some(now));
        assert!(ab.status().last_error.is_none());

        // 刚备完 → 没到期不跑;force = 立即再出一份(用户刚选目录的场景)
        assert!(ab.tick(now + 1000, false).await.is_none());
        assert!(ab.tick(now + 1000, true).await.expect("force 必跑").is_ok());

        // 目标目录没了(拔盘)→ 失败如实记档,水位不动(水位已被 force 那次推到 now+1000)
        std::fs::remove_dir_all(&dest).unwrap();
        let later = now + 1000 + INTERVAL_MS + 1;
        assert!(ab.tick(later, false).await.expect("到期该跑").is_err());
        assert_eq!(ab.status().last_ok_ms, Some(now + 1000), "失败不推水位");
        assert!(ab.status().last_error.is_some(), "失败原因给设置页看(§3.5)");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
