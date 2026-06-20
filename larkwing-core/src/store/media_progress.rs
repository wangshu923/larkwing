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

pub const MIGRATIONS: &[Migration] = &[m(
    "0015_media_progress_init",
    "CREATE TABLE media_progress (
        user_id         INTEGER NOT NULL,
        series_key      TEXT NOT NULL,
        episode_id      TEXT NOT NULL,
        position_seconds REAL NOT NULL DEFAULT 0,
        updated_at      INTEGER NOT NULL,
        PRIMARY KEY (user_id, series_key)
    );",
)];

/// 一部剧集的续播位置:停在哪一集(`episode_id` = 集身份,B 站 bvid/`p3`、本地相对文件名)。
#[derive(Debug, Clone, Serialize)]
pub struct Progress {
    pub episode_id: String,
    /// 集内秒数:集级续播恒 0(预留列);日后秒级续播写它。
    pub position_seconds: f64,
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

    /// 落进度(起播 / 切集时):一剧集一行,覆盖写。
    pub fn set(
        &self,
        user_id: i64,
        series_key: &str,
        episode_id: &str,
        position_seconds: f64,
    ) -> Result<()> {
        self.db.with(|c| {
            c.execute(
                "INSERT INTO media_progress
                   (user_id, series_key, episode_id, position_seconds, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(user_id, series_key)
                 DO UPDATE SET episode_id = excluded.episode_id,
                               position_seconds = excluded.position_seconds,
                               updated_at = excluded.updated_at",
                rusqlite::params![user_id, series_key, episode_id, position_seconds, now_ms()],
            )?;
            Ok(())
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

        s.media_progress.set(1, "bili:season:42", "BV1aa", 0.0).unwrap();
        let p = s.media_progress.get(1, "bili:season:42").unwrap().unwrap();
        assert_eq!(p.episode_id, "BV1aa");

        // 同剧集覆盖写(看到下一集)
        s.media_progress.set(1, "bili:season:42", "BV1bb", 0.0).unwrap();
        assert_eq!(s.media_progress.get(1, "bili:season:42").unwrap().unwrap().episode_id, "BV1bb");

        // 别的用户不串
        assert!(s.media_progress.get(2, "bili:season:42").unwrap().is_none(), "归人隔离");
        // 别的剧集不串
        assert!(s.media_progress.get(1, "local:abc").unwrap().is_none());
    }
}
