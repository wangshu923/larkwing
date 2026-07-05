//! 续播进度(PLAN §9 影音 / 多集续播):记住每部"剧集"上次放到哪一集。
//!
//! **为什么独立成域、不进记忆系统**:播放进度是高频、结构化的运行态,不是"关于这个人的事实";
//! 塞进 §13 的 recall/记忆会污染它(原则"智能≠记得多")。这里只是一张小账:剧集身份 → 当前集。
//!
//! **铁律(§6.2)**:`series_key` / `episode_id` **绝不存绝对路径** —— B 站用 season id / bvid,
//! 本地用 `hash(文件夹+骨架)` 与**相对文件名**;整棵目录搬走(数据搬家)进度仍对得上。
//! 进度**归人**(per-user),与记忆同源(§4.7)。`position_seconds` 先留列不写(集级续播只用
//! `episode_id`),为日后"集内秒数续播"免再开迁移。

use anyhow::Result;
use rusqlite::OptionalExtension;
use serde::Serialize;

use super::db::{m, now_ms, Db, Migration};

pub const MIGRATIONS: &[Migration] = &[
    m(
        "0015_media_progress_init",
        "CREATE TABLE media_progress (
        user_id         INTEGER NOT NULL,
        series_key      TEXT NOT NULL,
        episode_id      TEXT NOT NULL,
        position_seconds REAL NOT NULL DEFAULT 0,
        updated_at      INTEGER NOT NULL,
        PRIMARY KEY (user_id, series_key)
    );",
    ),
    // 0019:补显示用剧名(主动关怀「继续看《X》」候选要它)。老行 title 默认 '' →
    // list_recent 用 `title <> ''` 跳过,不会弹出没名字的候选。
    m(
        "0019_media_progress_title",
        "ALTER TABLE media_progress ADD COLUMN title TEXT NOT NULL DEFAULT ''",
    ),
];

/// 一部剧集的续播位置:停在哪一集(`episode_id` = 集身份,B 站 bvid/`p3`、本地相对文件名)。
#[derive(Debug, Clone, Serialize)]
pub struct Progress {
    pub episode_id: String,
    /// 集内秒数:集级续播恒 0(预留列);日后秒级续播写它。
    pub position_seconds: f64,
}

/// 一条"最近在看"的续播摘要(主动关怀「继续看《X》」候选用):只带显示所需 —— 剧名 + 上次时间。
#[derive(Debug, Clone, Serialize)]
pub struct RecentProgress {
    pub series_key: String,
    pub title: String,
    pub updated_at: i64,
}

#[derive(Clone)]
pub struct MediaProgressRepo {
    db: Db,
}

impl MediaProgressRepo {
    pub(super) fn new(db: Db) -> Self {
        Self { db }
    }

    /// 取某用户某剧集的续播位置(没看过 → None)。
    pub fn get(&self, user_id: i64, series_key: &str) -> Result<Option<Progress>> {
        self.db.with(|c| {
            let r = c
                .query_row(
                    "SELECT episode_id, position_seconds
                     FROM media_progress WHERE user_id = ?1 AND series_key = ?2",
                    rusqlite::params![user_id, series_key],
                    |r| {
                        Ok(Progress {
                            episode_id: r.get(0)?,
                            position_seconds: r.get(1)?,
                        })
                    },
                )
                .optional()?;
            Ok(r)
        })
    }

    /// 落进度(起播 / 切集时):一剧集一行,覆盖写。`title` = 显示用剧名(主动关怀候选用);
    /// 调用方传当前那集/那部的标题,拿不到就传空串(空串不会进 list_recent 候选)。
    pub fn set(
        &self,
        user_id: i64,
        series_key: &str,
        episode_id: &str,
        title: &str,
        position_seconds: f64,
    ) -> Result<()> {
        self.db.with(|c| {
            c.execute(
                "INSERT INTO media_progress
                   (user_id, series_key, episode_id, title, position_seconds, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(user_id, series_key)
                 DO UPDATE SET episode_id = excluded.episode_id,
                               title = excluded.title,
                               position_seconds = excluded.position_seconds,
                               updated_at = excluded.updated_at",
                rusqlite::params![user_id, series_key, episode_id, title, position_seconds, now_ms()],
            )?;
            Ok(())
        })
    }

    /// 最近在看的剧集(按 `updated_at` 新→旧,只回**有剧名**的),给主动关怀「继续看《X》」候选用。
    /// 只读、轻量;调用方(engine::float_idle)再按"搁置多久 / 静默时段"筛与限量。
    pub fn list_recent(&self, user_id: i64, limit: usize) -> Result<Vec<RecentProgress>> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT series_key, title, updated_at FROM media_progress
                 WHERE user_id = ?1 AND title <> '' ORDER BY updated_at DESC LIMIT ?2",
            )?;
            let rows = stmt.query_map(rusqlite::params![user_id, limit as i64], |r| {
                Ok(RecentProgress {
                    series_key: r.get(0)?,
                    title: r.get(1)?,
                    updated_at: r.get(2)?,
                })
            })?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::store::Store;

    fn store(tag: &str) -> Store {
        let p = std::env::temp_dir()
            .join(format!("lw-mediaprog-test-{}-{tag}.db", std::process::id()));
        let _ = std::fs::remove_file(&p);
        Store::open(&p).unwrap()
    }

    #[test]
    fn set_get_overwrite_and_user_isolation() {
        let s = store("rt");
        assert!(s.media_progress.get(1, "bili:season:42").unwrap().is_none(), "没看过 → None");

        s.media_progress.set(1, "bili:season:42", "BV1aa", "某剧", 0.0).unwrap();
        let p = s.media_progress.get(1, "bili:season:42").unwrap().unwrap();
        assert_eq!(p.episode_id, "BV1aa");

        // 同剧集覆盖写(看到下一集)
        s.media_progress.set(1, "bili:season:42", "BV1bb", "某剧", 0.0).unwrap();
        assert_eq!(s.media_progress.get(1, "bili:season:42").unwrap().unwrap().episode_id, "BV1bb");

        // 别的用户不串
        assert!(s.media_progress.get(2, "bili:season:42").unwrap().is_none(), "归人隔离");
        // 别的剧集不串
        assert!(s.media_progress.get(1, "local:abc").unwrap().is_none());
    }

    #[test]
    fn list_recent_needs_title_and_isolates_user() {
        let s = store("recent");
        s.media_progress.set(1, "k:notitle", "e0", "", 0.0).unwrap(); // 无剧名 → 不进候选
        s.media_progress.set(1, "k:a", "e1", "剧甲", 0.0).unwrap();
        s.media_progress.set(1, "k:b", "e2", "剧乙", 0.0).unwrap();
        let r = s.media_progress.list_recent(1, 5).unwrap();
        let titles: Vec<&str> = r.iter().map(|x| x.title.as_str()).collect();
        assert_eq!(r.len(), 2, "只回带剧名的两条");
        assert!(titles.contains(&"剧甲") && titles.contains(&"剧乙"));
        // 别的用户不串
        assert!(s.media_progress.list_recent(2, 5).unwrap().is_empty(), "归人隔离");
    }
}
