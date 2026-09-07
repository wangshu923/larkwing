//! 续播进度(PLAN §9 影音 / 多集续播):记住每部"剧集"/每部电影上次放到**哪一集、第几秒**。
//!
//! **为什么独立成域、不进记忆系统**:播放进度是高频、结构化的运行态,不是"关于这个人的事实";
//! 塞进 §13 的 recall/记忆会污染它(原则"智能≠记得多")。这里只是一张小账:内容身份 → 当前集 + 位置。
//!
//! **铁律(§6.2)**:`series_key` / `episode_id` **绝不存绝对路径** —— B 站用 season id / bvid,
//! 本地用 `hash(文件夹+骨架)` 与**相对文件名**、单部电影用 `hash(路径)`;整棵目录搬走进度仍对得上。
//!
//! **进度按家记、不按人(2026-09-07 用户拍板)**:客厅一台电脑一部剧,原先 PK(user, series)
//! 的后果 = 工具路径记在说话人名下、自动续播记在主人名下,同一部剧两行各说各话(「昨晚看到 4 集
//! 今天回到 2 集」的候选病灶之一)。`last_user_id` 只记「上次谁看的」,不参与查找。
//! **集内秒数**由前端心跳落盘(30s 一次 + 暂停/停止/切集即落),`finished` = 这一集看完了
//! (下次从下一集开头起)。

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
    // 0032:按家记 —— 主键去掉 user_id;同一部剧多人各一行时保留**最新**那行(INSERT OR IGNORE
    // 兜同毫秒的极端并列)。补 series_title(剧名,关怀条用;老行是集名)/ duration / finished。
    m(
        "0032_media_progress_household",
        "CREATE TABLE media_progress_v2 (
        series_key       TEXT PRIMARY KEY,
        episode_id       TEXT NOT NULL,
        title            TEXT NOT NULL DEFAULT '',
        series_title     TEXT NOT NULL DEFAULT '',
        position_seconds REAL NOT NULL DEFAULT 0,
        duration_seconds REAL NOT NULL DEFAULT 0,
        finished         INTEGER NOT NULL DEFAULT 0,
        last_user_id     INTEGER,
        updated_at       INTEGER NOT NULL
    );
    INSERT OR IGNORE INTO media_progress_v2
        (series_key, episode_id, title, position_seconds, last_user_id, updated_at)
    SELECT p.series_key, p.episode_id, p.title, p.position_seconds, p.user_id, p.updated_at
    FROM media_progress p
    WHERE p.updated_at = (SELECT MAX(q.updated_at) FROM media_progress q WHERE q.series_key = p.series_key);
    DROP TABLE media_progress;
    ALTER TABLE media_progress_v2 RENAME TO media_progress;",
    ),
];

/// 一部剧集 / 一部电影的续播位置。
#[derive(Debug, Clone, Serialize)]
pub struct Progress {
    /// 停在哪一集(集身份:B 站 bvid / `p3` / `ep123`、本地相对文件名;单部电影 = 文件名)。
    pub episode_id: String,
    /// 集内秒数(前端心跳落盘;起播/切集写 0 或续播位)。
    pub position_seconds: f64,
    /// 这一集的总长(秒;0 = 还没回报过)。读侧据此判「已到片尾」。
    pub duration_seconds: f64,
    /// 这一集看完了(播到结尾 / 停在片尾区):下次从下一集开头起;末集看完 = 整部看完。
    pub finished: bool,
    pub updated_at: i64,
}

/// 一条"最近在看"的续播摘要(主动关怀「继续看《X》」候选用):只带显示所需 —— 剧名 + 上次时间。
#[derive(Debug, Clone, Serialize)]
pub struct RecentProgress {
    pub series_key: String,
    /// 显示名:剧名(`series_title`),老行没有剧名时退回集名。
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

    /// 取某剧集 / 电影的续播位置(没看过 → None)。全家共用一行。
    pub fn get(&self, series_key: &str) -> Result<Option<Progress>> {
        self.db.with(|c| {
            let r = c
                .query_row(
                    "SELECT episode_id, position_seconds, duration_seconds, finished, updated_at
                     FROM media_progress WHERE series_key = ?1",
                    [series_key],
                    |r| {
                        Ok(Progress {
                            episode_id: r.get(0)?,
                            position_seconds: r.get(1)?,
                            duration_seconds: r.get(2)?,
                            finished: r.get::<_, i64>(3)? != 0,
                            updated_at: r.get(4)?,
                        })
                    },
                )
                .optional()?;
            Ok(r)
        })
    }

    /// 落「现在放到哪一集」(起播 / 切集时):一部内容一行,覆盖写;位置 = 起播位(续播接上的秒数
    /// 或 0),`finished` 清零,时长等心跳回报。`title` = 集名、`series_title` = 剧名(关怀条用;
    /// 拿不到传空串,两者都空的行不进 list_recent 候选);`last_user_id` = 这次是谁在看(展示用)。
    pub fn set_episode(
        &self,
        series_key: &str,
        episode_id: &str,
        title: &str,
        series_title: &str,
        position_seconds: f64,
        last_user_id: Option<i64>,
    ) -> Result<()> {
        self.db.with(|c| {
            c.execute(
                "INSERT INTO media_progress
                   (series_key, episode_id, title, series_title, position_seconds, duration_seconds,
                    finished, last_user_id, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 0, 0, ?6, ?7)
                 ON CONFLICT(series_key)
                 DO UPDATE SET episode_id = excluded.episode_id,
                               title = excluded.title,
                               series_title = CASE WHEN excluded.series_title <> ''
                                                   THEN excluded.series_title ELSE series_title END,
                               position_seconds = excluded.position_seconds,
                               duration_seconds = 0,
                               finished = 0,
                               last_user_id = excluded.last_user_id,
                               updated_at = excluded.updated_at",
                rusqlite::params![
                    series_key,
                    episode_id,
                    title,
                    series_title,
                    position_seconds,
                    last_user_id,
                    now_ms()
                ],
            )?;
            Ok(())
        })
    }

    /// 落集内位置(前端心跳 / 暂停 / 停止):**只改当前这一集的行**(episode_id 对得上才写,
    /// 切集后迟到的旧心跳写不进新一集)。返回是否真写了。
    pub fn set_position(
        &self,
        series_key: &str,
        episode_id: &str,
        position_seconds: f64,
        duration_seconds: f64,
        finished: bool,
    ) -> Result<bool> {
        self.db.with(|c| {
            let n = c.execute(
                "UPDATE media_progress
                 SET position_seconds = ?3, duration_seconds = ?4, finished = ?5, updated_at = ?6
                 WHERE series_key = ?1 AND episode_id = ?2",
                rusqlite::params![
                    series_key,
                    episode_id,
                    position_seconds,
                    duration_seconds,
                    finished as i64,
                    now_ms()
                ],
            )?;
            Ok(n > 0)
        })
    }

    /// 最近在看的(按 `updated_at` 新→旧,只回**有名字**的),给主动关怀「继续看《X》」候选 /
    /// 家庭日记用。显示名优先剧名。只读、轻量;调用方再按"搁置多久 / 静默时段"筛与限量。
    pub fn list_recent(&self, limit: usize) -> Result<Vec<RecentProgress>> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT series_key,
                        CASE WHEN series_title <> '' THEN series_title ELSE title END,
                        updated_at
                 FROM media_progress
                 WHERE title <> '' OR series_title <> ''
                 ORDER BY updated_at DESC LIMIT ?1",
            )?;
            let rows = stmt.query_map([limit as i64], |r| {
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
    fn set_episode_overwrites_and_position_only_hits_current_episode() {
        let s = store("rt");
        let mp = &s.media_progress;
        assert!(mp.get("bili:season:42").unwrap().is_none(), "没看过 → None");

        mp.set_episode("bili:season:42", "BV1aa", "第1集", "某剧", 0.0, Some(1)).unwrap();
        let p = mp.get("bili:season:42").unwrap().unwrap();
        assert_eq!(p.episode_id, "BV1aa");
        assert!(!p.finished);

        // 心跳落位置:当前集写得进
        assert!(mp.set_position("bili:season:42", "BV1aa", 754.0, 1400.0, false).unwrap());
        let p = mp.get("bili:season:42").unwrap().unwrap();
        assert_eq!(p.position_seconds, 754.0);
        assert_eq!(p.duration_seconds, 1400.0);

        // 切到下一集(另一个人在看):位置/时长/finished 归零,剧名保留
        mp.set_episode("bili:season:42", "BV1bb", "第2集", "", 0.0, Some(2)).unwrap();
        let p = mp.get("bili:season:42").unwrap().unwrap();
        assert_eq!(p.episode_id, "BV1bb");
        assert_eq!(p.position_seconds, 0.0);
        assert_eq!(p.duration_seconds, 0.0);
        // 迟到的旧心跳(还带着上一集的 id)写不进
        assert!(!mp.set_position("bili:season:42", "BV1aa", 900.0, 1400.0, false).unwrap());
        assert_eq!(mp.get("bili:season:42").unwrap().unwrap().position_seconds, 0.0);
        // 看完标记
        assert!(mp.set_position("bili:season:42", "BV1bb", 1390.0, 1400.0, true).unwrap());
        assert!(mp.get("bili:season:42").unwrap().unwrap().finished);

        // 别的剧集不串
        assert!(mp.get("local:abc").unwrap().is_none());
    }

    #[test]
    fn list_recent_prefers_series_title_and_skips_nameless() {
        let s = store("recent");
        let mp = &s.media_progress;
        mp.set_episode("k:notitle", "e0", "", "", 0.0, None).unwrap(); // 没名字 → 不进候选
        mp.set_episode("k:a", "e1", "第3集", "剧甲", 0.0, None).unwrap();
        mp.set_episode("k:b", "e2", "第7集", "", 0.0, None).unwrap(); // 老行形态:只有集名
        let r = mp.list_recent(5).unwrap();
        let titles: Vec<&str> = r.iter().map(|x| x.title.as_str()).collect();
        assert_eq!(r.len(), 2, "只回带名字的两条");
        assert!(titles.contains(&"剧甲"), "有剧名显剧名");
        assert!(titles.contains(&"第7集"), "没剧名退回集名");
    }
}
