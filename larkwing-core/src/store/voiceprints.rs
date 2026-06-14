//! 声纹注册库(PLAN §11 D):每个家人一条 embedding(192 维 f32)。
//! 归属于 user(宪法 §6 记忆归人的同构);识别时现读现算余弦,无内存常驻状态。
//! blob 存库(单条 ~768 字节,极小;不像 TTS 音频那种可重建大 blob 走文件)。

use anyhow::Result;
use rusqlite::OptionalExtension;

use super::db::{m, now_ms, Db, Migration};

pub const MIGRATIONS: &[Migration] = &[m(
    "0010_voiceprints_init",
    "CREATE TABLE voiceprints (
        user_id    INTEGER PRIMARY KEY,
        embedding  BLOB NOT NULL,
        created_at INTEGER NOT NULL
    );",
)];

#[derive(Clone)]
pub struct VoiceprintRepo {
    db: Db,
}

impl VoiceprintRepo {
    pub(super) fn new(db: Db) -> Self {
        Self { db }
    }

    /// 注册/重录(同 user 覆盖):enroll 出的 embedding 落库。
    pub fn upsert(&self, user_id: i64, embedding: &[f32]) -> Result<()> {
        let blob = embed_to_bytes(embedding);
        self.db.with(|c| {
            c.execute(
                "INSERT INTO voiceprints (user_id, embedding, created_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(user_id) DO UPDATE SET embedding = ?2, created_at = ?3",
                rusqlite::params![user_id, blob, now_ms()],
            )?;
            Ok(())
        })
    }

    pub fn remove(&self, user_id: i64) -> Result<()> {
        self.db.with(|c| {
            c.execute("DELETE FROM voiceprints WHERE user_id = ?1", [user_id])?;
            Ok(())
        })
    }

    pub fn has(&self, user_id: i64) -> Result<bool> {
        self.db.with(|c| {
            let n: i64 = c.query_row(
                "SELECT COUNT(*) FROM voiceprints WHERE user_id = ?1",
                [user_id],
                |r| r.get(0),
            )?;
            Ok(n > 0)
        })
    }

    /// identify 用:全部注册声纹(家人就几个,O(n) 逐个余弦无所谓)。
    pub fn list_all(&self) -> Result<Vec<(i64, Vec<f32>)>> {
        self.db.with(|c| {
            let mut stmt = c.prepare("SELECT user_id, embedding FROM voiceprints")?;
            let rows = stmt
                .query_map([], |r| {
                    let id: i64 = r.get(0)?;
                    let blob: Vec<u8> = r.get(1)?;
                    Ok((id, bytes_to_embed(&blob)))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// 设置页用户列表的"已录声纹"标记(单查)。
    pub fn enrolled_ids(&self) -> Result<Vec<i64>> {
        self.db.with(|c| {
            let mut stmt = c.prepare("SELECT user_id FROM voiceprints")?;
            let ids = stmt
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(ids)
        })
    }

    #[allow(dead_code)] // 对称提供;当前 list_all 已覆盖读路径
    pub fn get(&self, user_id: i64) -> Result<Option<Vec<f32>>> {
        self.db.with(|c| {
            let blob: Option<Vec<u8>> = c
                .query_row(
                    "SELECT embedding FROM voiceprints WHERE user_id = ?1",
                    [user_id],
                    |r| r.get(0),
                )
                .optional()?;
            Ok(blob.map(|b| bytes_to_embed(&b)))
        })
    }
}

fn embed_to_bytes(e: &[f32]) -> Vec<u8> {
    e.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn bytes_to_embed(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedding_roundtrips_through_blob() {
        let e = vec![0.1f32, -0.5, 0.9, 0.0, 1.0];
        let back = bytes_to_embed(&embed_to_bytes(&e));
        assert_eq!(e, back);
    }
}
