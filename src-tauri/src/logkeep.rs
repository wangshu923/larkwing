//! 日志管护:tracing-appender 只滚不清,这里补上——非当天的日志压成 .gz,
//! 超过保留期的删除。启动扫一遍,之后每小时一遍(进程常驻,不能只靠启动那次)。
//! "今天"按 UTC 算,与 tracing-appender 的滚动边界一致;当天文件 appender 正持有,绝不碰。

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use flate2::write::GzEncoder;
use flate2::Compression;
use time::{Date, Month, OffsetDateTime};

/// 与 lib.rs 里 rolling::daily 的文件名前缀一致(appender 产出 `larkwing.log.YYYY-MM-DD`)。
const PREFIX: &str = "larkwing.log.";
/// 保留最近 30 天历史(不含当天活跃文件),更早的删除。
const KEEP_DAYS: i64 = 30;

pub fn spawn(logs_dir: PathBuf) {
  std::thread::Builder::new()
    .name("logkeep".into())
    .spawn(move || loop {
      sweep(&logs_dir, OffsetDateTime::now_utc().date());
      std::thread::sleep(Duration::from_secs(60 * 60));
    })
    .ok();
}

/// 扫一遍日志目录:崩溃残留的 .gz.tmp 清掉,过期的删,昨天及更早的未压缩文件压缩。
fn sweep(dir: &Path, today: Date) {
  let cutoff = today - time::Duration::days(KEEP_DAYS);
  let entries = match fs::read_dir(dir) {
    Ok(e) => e,
    Err(e) => {
      tracing::warn!(dir = %dir.display(), error = %e, "日志目录读取失败,跳过本轮管护");
      return;
    }
  };

  for entry in entries.flatten() {
    let path = entry.path();
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };

    if name.ends_with(".gz.tmp") {
      let _ = fs::remove_file(&path);
      continue;
    }
    let Some((date, compressed)) = parse_log_name(name) else { continue };

    if date < cutoff {
      match fs::remove_file(&path) {
        Ok(()) => tracing::info!(file = name, "删除过期日志"),
        Err(e) => tracing::warn!(file = name, error = %e, "删除过期日志失败"),
      }
    } else if !compressed && date < today {
      match compress(&path) {
        Ok(()) => tracing::info!(file = name, "压缩历史日志"),
        Err(e) => tracing::warn!(file = name, error = %e, "压缩历史日志失败"),
      }
    }
  }
}

/// `larkwing.log.2026-06-10` / `larkwing.log.2026-06-10.gz` → (日期, 是否已压缩)。
/// 不认识的文件名返回 None,sweep 不碰它。
fn parse_log_name(name: &str) -> Option<(Date, bool)> {
  let rest = name.strip_prefix(PREFIX)?;
  let (date_part, compressed) = match rest.strip_suffix(".gz") {
    Some(d) => (d, true),
    None => (rest, false),
  };
  let mut it = date_part.splitn(3, '-');
  let year: i32 = it.next()?.parse().ok()?;
  let month: u8 = it.next()?.parse().ok()?;
  let day: u8 = it.next()?.parse().ok()?;
  let date = Date::from_calendar_date(year, Month::try_from(month).ok()?, day).ok()?;
  Some((date, compressed))
}

/// 压缩一个历史日志:写 `.gz.tmp` → rename 成 `.gz` → 删原文件。
/// 崩在任何一步,下一轮 sweep 都能收拾(tmp 清理 / 补删原文件)。
fn compress(plain: &Path) -> io::Result<()> {
  let gz = sibling(plain, ".gz");
  let tmp = sibling(plain, ".gz.tmp");

  // 上次压完没删成原文件:补删即可,别重压
  if gz.exists() {
    return fs::remove_file(plain);
  }

  let mut src = File::open(plain)?;
  let mut enc = GzEncoder::new(File::create(&tmp)?, Compression::default());
  io::copy(&mut src, &mut enc)?;
  enc.finish()?;
  fs::rename(&tmp, &gz)?;
  fs::remove_file(plain)
}

/// 在完整文件名后追加后缀(`Path::with_extension` 会把日期当扩展名换掉,不能用)。
fn sibling(path: &Path, suffix: &str) -> PathBuf {
  let mut s = path.as_os_str().to_owned();
  s.push(suffix);
  PathBuf::from(s)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn date(y: i32, m: u8, d: u8) -> Date {
    Date::from_calendar_date(y, Month::try_from(m).unwrap(), d).unwrap()
  }

  #[test]
  fn parse_plain_and_gz() {
    assert_eq!(
      parse_log_name("larkwing.log.2026-06-10"),
      Some((date(2026, 6, 10), false))
    );
    assert_eq!(
      parse_log_name("larkwing.log.2026-06-10.gz"),
      Some((date(2026, 6, 10), true))
    );
  }

  #[test]
  fn reject_foreign_names() {
    assert_eq!(parse_log_name("larkwing.log"), None);
    assert_eq!(parse_log_name("larkwing.log.notadate"), None);
    assert_eq!(parse_log_name("other.log.2026-06-10"), None);
    assert_eq!(parse_log_name("larkwing.log.2026-13-40"), None);
  }

  #[test]
  fn sweep_compresses_old_keeps_today_deletes_expired() {
    let dir = std::env::temp_dir().join(format!("larkwing-logkeep-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();

    let today = date(2026, 6, 11);
    fs::write(dir.join("larkwing.log.2026-06-11"), b"today").unwrap(); // 活跃,不碰
    fs::write(dir.join("larkwing.log.2026-06-10"), b"yesterday").unwrap(); // → 压缩
    fs::write(dir.join("larkwing.log.2026-05-12"), b"day30").unwrap(); // 恰好 30 天,保留+压缩
    fs::write(dir.join("larkwing.log.2026-05-11"), b"day31").unwrap(); // 过期 → 删
    fs::write(dir.join("larkwing.log.2026-05-01.gz"), b"old gz").unwrap(); // 过期 → 删
    fs::write(dir.join("larkwing.log.2026-06-09.gz.tmp"), b"crash leftover").unwrap(); // → 删
    fs::write(dir.join("unrelated.txt"), b"keep").unwrap(); // 不碰

    sweep(&dir, today);

    let names: Vec<String> = {
      let mut v: Vec<String> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
      v.sort();
      v
    };
    assert_eq!(
      names,
      vec![
        "larkwing.log.2026-05-12.gz",
        "larkwing.log.2026-06-10.gz",
        "larkwing.log.2026-06-11",
        "unrelated.txt",
      ]
    );

    // 压缩内容可还原
    let mut dec = flate2::read::GzDecoder::new(File::open(dir.join("larkwing.log.2026-06-10.gz")).unwrap());
    let mut s = String::new();
    io::Read::read_to_string(&mut dec, &mut s).unwrap();
    assert_eq!(s, "yesterday");

    let _ = fs::remove_dir_all(&dir);
  }
}
