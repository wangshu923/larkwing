//! 克隆音色库(PLAN §11 D-clone):每条 = 一个零样本克隆音色的元数据。
//! 音色 = 数据(同声音目录哲学);参考音 wav 是可重建大 blob → 走文件
//! (`数据目录/voice/clones/<wav_file>`),库里只存元数据(同 TTS 缓存 blob 不进库的约定)。
//! id 不可变:重录 = 新条目(参考音永不就地变 → 既有 TTS 缓存按 id 命名空间永不过期)。

use anyhow::Result;
use rusqlite::OptionalExtension;

use super::db::{m, now_ms, Db, Migration};

pub const MIGRATIONS: &[Migration] = &[m(
    "0011_cloned_voices_init",
    "CREATE TABLE cloned_voices (
        id         TEXT PRIMARY KEY,
        name       TEXT NOT NULL,
        wav_file   TEXT NOT NULL,
        transcript TEXT NOT NULL,
        lang       TEXT NOT NULL DEFAULT 'zh',
        builtin    INTEGER NOT NULL DEFAULT 0,
        created_at INTEGER NOT NULL
    );",
)];

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClonedVoice {
    pub id: String,
    pub name: String,
    /// 参考音文件名(相对 `数据目录/voice/clones/`)。
    pub wav_file: String,
    pub transcript: String,
    pub lang: String,
    /// 内置预置(随包/下载):不可删。
    pub builtin: bool,
    pub created_at: i64,
}

#[derive(Clone)]
pub struct ClonedVoiceRepo {
    db: Db,
}

impl ClonedVoiceRepo {
    pub(super) fn new(db: Db) -> Self {
        Self { db }
    }

    pub fn insert(&self, v: &ClonedVoice) -> Result<()> {
        self.db.with(|c| {
            c.execute(
                "INSERT INTO cloned_voices
                   (id, name, wav_file, transcript, lang, builtin, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    v.id,
                    v.name,
                    v.wav_file,
                    v.transcript,
                    v.lang,
                    v.builtin as i64,
                    now_ms()
                ],
            )?;
            Ok(())
        })
    }

    pub fn get(&self, id: &str) -> Result<Option<ClonedVoice>> {
        self.db.with(|c| {
            let row = c
                .query_row(
                    "SELECT id, name, wav_file, transcript, lang, builtin, created_at
                     FROM cloned_voices WHERE id = ?1",
                    [id],
                    Self::map_row,
                )
                .optional()?;
            Ok(row)
        })
    }

    pub fn list(&self) -> Result<Vec<ClonedVoice>> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT id, name, wav_file, transcript, lang, builtin, created_at
                 FROM cloned_voices ORDER BY created_at",
            )?;
            let rows = stmt
                .query_map([], Self::map_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    pub fn rename(&self, id: &str, name: &str) -> Result<()> {
        self.db.with(|c| {
            c.execute(
                "UPDATE cloned_voices SET name = ?2 WHERE id = ?1",
                rusqlite::params![id, name],
            )?;
            Ok(())
        })
    }

    /// 删除一条,返回被删行的 `wav_file`(供调用方删盘);内置音色不可删,返回 None。
    pub fn delete(&self, id: &str) -> Result<Option<String>> {
        self.db.with(|c| {
            let wav: Option<String> = c
                .query_row(
                    "SELECT wav_file FROM cloned_voices WHERE id = ?1 AND builtin = 0",
                    [id],
                    |r| r.get(0),
                )
                .optional()?;
            if wav.is_some() {
                c.execute("DELETE FROM cloned_voices WHERE id = ?1 AND builtin = 0", [id])?;
            }
            Ok(wav)
        })
    }

    fn map_row(r: &rusqlite::Row) -> rusqlite::Result<ClonedVoice> {
        Ok(ClonedVoice {
            id: r.get(0)?,
            name: r.get(1)?,
            wav_file: r.get(2)?,
            transcript: r.get(3)?,
            lang: r.get(4)?,
            builtin: r.get::<_, i64>(5)? != 0,
            created_at: r.get(6)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> ClonedVoiceRepo {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        db.migrate(MIGRATIONS).unwrap();
        ClonedVoiceRepo::new(db)
    }

    fn sample(id: &str, builtin: bool) -> ClonedVoice {
        ClonedVoice {
            id: id.into(),
            name: "我的声音".into(),
            wav_file: format!("{id}.wav"),
            transcript: "你好,我是测试音色".into(),
            lang: "zh".into(),
            builtin,
            created_at: 0,
        }
    }

    #[test]
    fn insert_get_list_rename_delete() {
        let r = repo();
        r.insert(&sample("abc", false)).unwrap();
        assert_eq!(r.get("abc").unwrap().unwrap().wav_file, "abc.wav");
        assert_eq!(r.list().unwrap().len(), 1);
        r.rename("abc", "新名字").unwrap();
        assert_eq!(r.get("abc").unwrap().unwrap().name, "新名字");
        assert_eq!(r.delete("abc").unwrap().as_deref(), Some("abc.wav"));
        assert!(r.get("abc").unwrap().is_none());
    }

    #[test]
    fn builtin_is_not_deletable() {
        let r = repo();
        r.insert(&sample("bt", true)).unwrap();
        assert_eq!(r.delete("bt").unwrap(), None, "内置音色删不掉");
        assert!(r.get("bt").unwrap().is_some());
    }
}
