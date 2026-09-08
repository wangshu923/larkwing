//! 片头 / 片尾标记(PLAN §9 影音 / 片头片尾):一部剧集里「哪一段是片头、片尾从哪开始」。
//!
//! 两种行,同一张表:
//! - `source = manual`:**手标规则**,锚在某一集,「从这集起」生效 —— 火影这类换片头的长番,
//!   第 26 集再标一次,后面的集跟新值、前面的不受影响(用户拍板 2026-09-07);字段 NULL = 那一项没标。
//! - `source = detected`:**指纹检测**的逐集结果(相邻集音频公共段),只对那一集有效。
//!
//! 章节元数据 / B 站 OP·ED 段**不落库**(现探现算,派生可丢)。手标 > 自动(用户纠错永远赢)。
//!
//! **铁律(§6.2)**:`series_key` / `episode_id` 与续播表同一套身份,绝不存绝对路径。

use anyhow::Result;
use serde::Serialize;

use super::db::{m, now_ms, Db, Migration};

pub const MIGRATIONS: &[Migration] = &[m(
    "0033_media_skip_init",
    "CREATE TABLE media_skip (
        series_key   TEXT NOT NULL,
        episode_id   TEXT NOT NULL,
        source       TEXT NOT NULL,
        intro_start  REAL,
        intro_end    REAL,
        outro_start  REAL,
        outro_end    REAL,
        duration     REAL,
        updated_at   INTEGER NOT NULL,
        PRIMARY KEY (series_key, episode_id, source)
    );",
)];

/// 一行标记(手标规则 / 检测结果共用形状;没标 / 没测出的项为 None)。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SkipRow {
    pub series_key: String,
    pub episode_id: String,
    /// `manual` | `detected`
    pub source: String,
    pub intro_start: Option<f64>,
    pub intro_end: Option<f64>,
    pub outro_start: Option<f64>,
    pub outro_end: Option<f64>,
    /// 标记 / 检测时那一集的总长(秒):手标片尾换算成「距结尾多少秒」套到别的集上要靠它。
    pub duration: Option<f64>,
    pub updated_at: i64,
}

#[derive(Clone)]
pub struct MediaSkipRepo {
    db: Db,
}

impl MediaSkipRepo {
    pub(super) fn new(db: Db) -> Self {
        Self { db }
    }

    /// 一部剧的全部标记(手标 + 检测;解析归 `media::skip::resolve`)。
    pub fn list(&self, series_key: &str) -> Result<Vec<SkipRow>> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT series_key, episode_id, source, intro_start, intro_end, outro_start, outro_end,
                        duration, updated_at
                 FROM media_skip WHERE series_key = ?1 ORDER BY updated_at",
            )?;
            let rows = stmt.query_map([series_key], |r| {
                Ok(SkipRow {
                    series_key: r.get(0)?,
                    episode_id: r.get(1)?,
                    source: r.get(2)?,
                    intro_start: r.get(3)?,
                    intro_end: r.get(4)?,
                    outro_start: r.get(5)?,
                    outro_end: r.get(6)?,
                    duration: r.get(7)?,
                    updated_at: r.get(8)?,
                })
            })?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
    }

    /// 写一行(同 (剧, 集, 来源) 覆盖)。
    pub fn upsert(&self, row: &SkipRow) -> Result<()> {
        self.db.with(|c| {
            c.execute(
                "INSERT INTO media_skip
                   (series_key, episode_id, source, intro_start, intro_end, outro_start, outro_end,
                    duration, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(series_key, episode_id, source)
                 DO UPDATE SET intro_start = excluded.intro_start,
                               intro_end = excluded.intro_end,
                               outro_start = excluded.outro_start,
                               outro_end = excluded.outro_end,
                               duration = excluded.duration,
                               updated_at = excluded.updated_at",
                rusqlite::params![
                    row.series_key,
                    row.episode_id,
                    row.source,
                    row.intro_start,
                    row.intro_end,
                    row.outro_start,
                    row.outro_end,
                    row.duration,
                    now_ms()
                ],
            )?;
            Ok(())
        })
    }

    /// 清掉一部剧的全部**手标**(「清除本剧标记」;检测结果留着)。返回删了几行。
    pub fn clear_manual(&self, series_key: &str) -> Result<usize> {
        self.db.with(|c| {
            Ok(c.execute(
                "DELETE FROM media_skip WHERE series_key = ?1 AND source = 'manual'",
                [series_key],
            )?)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::SkipRow;
    use crate::store::Store;

    fn store(tag: &str) -> Store {
        let p = std::env::temp_dir().join(format!("lw-mediaskip-test-{}-{tag}.db", std::process::id()));
        let _ = std::fs::remove_file(&p);
        Store::open(&p).unwrap()
    }

    fn row(ep: &str, source: &str, intro_end: Option<f64>) -> SkipRow {
        SkipRow {
            series_key: "local:abc".into(),
            episode_id: ep.into(),
            source: source.into(),
            intro_start: Some(0.0),
            intro_end,
            outro_start: None,
            outro_end: None,
            duration: Some(1400.0),
            updated_at: 0,
        }
    }

    #[test]
    fn upsert_list_and_clear_manual_keeps_detected() {
        let s = store("rt");
        let r = &s.media_skip;
        r.upsert(&row("e1.mp4", "manual", Some(90.0))).unwrap();
        r.upsert(&row("e1.mp4", "detected", Some(88.5))).unwrap();
        r.upsert(&row("e1.mp4", "manual", Some(95.0))).unwrap(); // 同键覆盖
        let rows = r.list("local:abc").unwrap();
        assert_eq!(rows.len(), 2, "手标 + 检测各一行");
        let manual = rows.iter().find(|x| x.source == "manual").unwrap();
        assert_eq!(manual.intro_end, Some(95.0), "覆盖写");
        assert!(r.list("local:other").unwrap().is_empty(), "别的剧不串");

        assert_eq!(r.clear_manual("local:abc").unwrap(), 1);
        let rows = r.list("local:abc").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].source, "detected", "清手标不动检测结果");
    }
}
