use anyhow::Result;
use rusqlite::OptionalExtension;
use serde::Serialize;

use super::db::{m, now_ms, Db, Migration};

pub const MIGRATIONS: &[Migration] = &[
    m(
        "0001_users_init",
        "CREATE TABLE users (
        id             INTEGER PRIMARY KEY,
        name           TEXT NOT NULL,
        skin_id        TEXT NOT NULL DEFAULT 'scifi',
        created_at     INTEGER NOT NULL,
        last_active_at INTEGER NOT NULL
    );",
    ),
    // 旧默认是 'warm'(sci-fi-default 决策前的遗留);此前并无换肤入口 → 库里所有 'warm' 都是陈旧默认,
    // 一次性归位到真正的默认 'scifi'。此后用户经设置选的皮肤照常持久化(本迁移按 id 只跑一次)。
    m(
        "0002_default_skin_scifi",
        "UPDATE users SET skin_id = 'scifi' WHERE skin_id = 'warm';",
    ),
];

#[derive(Debug, Clone, Serialize)]
pub struct User {
    pub id: i64,
    pub name: String,
    pub skin_id: String,
    pub created_at: i64,
    pub last_active_at: i64,
}

#[derive(Clone)]
pub struct UserRepo {
    db: Db,
}

impl UserRepo {
    pub(super) fn new(db: Db) -> Self {
        Self { db }
    }

    /// 首启零配置:一个用户都没有时自动建默认用户;否则返回**主人**(= 最早建的那个 = 最小 id)。
    /// ⚠️ 身份绝不按活跃度推断(2026-07-04 真机实锤):曾 `ORDER BY last_active_at DESC`,
    /// 结果加家人 / 家人发条渠道消息都会改选「当前用户」,主人视角乱漂(会话/记忆/名字/性格
    /// 忽有忽无、「(你)」标记乱跳)。主人是**固定身份**——第一次开机建的那个用户(id 最小,
    /// 家人永远晚于它建),锚死在 id,与 last_active_at 彻底解耦。删自己 UI 已拦(§渠道归人)。
    pub fn ensure_default_user(&self) -> Result<User> {
        self.db.with(|c| {
            let existing = c
                .query_row(
                    "SELECT id, name, skin_id, created_at, last_active_at
                     FROM users ORDER BY id ASC LIMIT 1",
                    [],
                    row_to_user,
                )
                .optional()?;
            if let Some(u) = existing {
                return Ok(u);
            }
            let now = now_ms();
            c.execute(
                "INSERT INTO users (name, skin_id, created_at, last_active_at)
                 VALUES ('我', 'scifi', ?1, ?1)",
                [now],
            )?;
            Ok(User {
                id: c.last_insert_rowid(),
                name: "我".into(),
                skin_id: "scifi".into(),
                created_at: now,
                last_active_at: now,
            })
        })
    }

    /// 添加家人(PLAN §11 D 多用户落地):新建一个用户,记忆/声纹各自独立。
    /// ⚠️ `last_active_at` 落 0(**从未活跃**),绝不落 now —— `ensure_default_user` 按最近活跃
    /// 恢复「当前用户」,建号若算活跃,加完家人一重启主人就被切成 TA 的空白视角
    /// (2026-07-03 真机实锤:会话/记忆/名字/性格「全没了」,钉钉来一句 touch 主人才恢复)。
    /// 家人的活跃只来自真实说话归人(声纹/渠道那一侧的刻意 touch,现阶段没有)。
    pub fn create(&self, name: &str) -> Result<User> {
        self.db.with(|c| {
            let now = now_ms();
            c.execute(
                "INSERT INTO users (name, skin_id, created_at, last_active_at)
                 VALUES (?1, 'scifi', ?2, 0)",
                rusqlite::params![name, now],
            )?;
            Ok(User {
                id: c.last_insert_rowid(),
                name: name.into(),
                skin_id: "scifi".into(),
                created_at: now,
                last_active_at: 0,
            })
        })
    }

    /// 删除家人(只删 users 行;关联的记忆/声纹由 engine.delete_user 编排清理)。
    pub fn delete(&self, id: i64) -> Result<()> {
        self.db.with(|c| {
            c.execute("DELETE FROM users WHERE id = ?1", [id])?;
            Ok(())
        })
    }

    pub fn count(&self) -> Result<i64> {
        self.db.with(|c| Ok(c.query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))?))
    }

    /// 校验 user 是否真实存在(声纹识别结果回灌前的防注入)。
    pub fn get(&self, id: i64) -> Result<Option<User>> {
        self.db.with(|c| {
            Ok(c.query_row(
                "SELECT id, name, skin_id, created_at, last_active_at FROM users WHERE id = ?1",
                [id],
                row_to_user,
            )
            .optional()?)
        })
    }

    pub fn list(&self) -> Result<Vec<User>> {
        self.db.with(|c| {
            // 主人在前(id 最小),家人按加入顺序 —— 稳定,不随活跃度跳(§家人页「(你)」定位)。
            let mut stmt = c.prepare(
                "SELECT id, name, skin_id, created_at, last_active_at
                 FROM users ORDER BY id ASC",
            )?;
            let users = stmt
                .query_map([], row_to_user)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(users)
        })
    }

    pub fn set_skin(&self, user: i64, skin_id: &str) -> Result<()> {
        self.db.with(|c| {
            c.execute(
                "UPDATE users SET skin_id = ?2 WHERE id = ?1",
                rusqlite::params![user, skin_id],
            )?;
            Ok(())
        })
    }

    pub fn rename(&self, user: i64, name: &str) -> Result<()> {
        self.db.with(|c| {
            c.execute(
                "UPDATE users SET name = ?2 WHERE id = ?1",
                rusqlite::params![user, name],
            )?;
            Ok(())
        })
    }

    pub fn touch(&self, user: i64) -> Result<()> {
        self.db.with(|c| {
            c.execute(
                "UPDATE users SET last_active_at = ?2 WHERE id = ?1",
                rusqlite::params![user, now_ms()],
            )?;
            Ok(())
        })
    }
}

fn row_to_user(r: &rusqlite::Row<'_>) -> rusqlite::Result<User> {
    Ok(User {
        id: r.get(0)?,
        name: r.get(1)?,
        skin_id: r.get(2)?,
        created_at: r.get(3)?,
        last_active_at: r.get(4)?,
    })
}

#[cfg(test)]
mod tests {
    use crate::store::Store;

    /// 加家人 ≠ 活跃(2026-07-03 真机实锤回归):建号若落 last_active_at=now,
    /// 加完家人一重启,ensure_default_user 按最近活跃就把主人切成 TA 的空白视角
    /// (会话/记忆/名字/性格「全没了」)。家人建号必须从未活跃(0)。
    #[test]
    fn family_creation_never_steals_boot_owner() {
        let p = std::env::temp_dir().join(format!("lw-users-boot-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&p);
        let s = Store::open(&p).unwrap();
        let owner = s.users.ensure_default_user().unwrap();
        s.users.touch(owner.id).unwrap();

        let fam = s.users.create("爸爸").unwrap();
        assert_eq!(fam.last_active_at, 0, "建号不算活跃");
        // 家人后来「活跃」了(渠道发消息 touch 等)——身份也绝不能漂到 TA 身上
        s.users.touch(fam.id).unwrap();
        assert_eq!(
            s.users.ensure_default_user().unwrap().id,
            owner.id,
            "身份锚死主人(最小 id),与 last_active_at 无关:家人再活跃也顶不掉主人"
        );
        // 家人列表:主人恒在最前(id 升序),不随活跃度跳
        assert_eq!(s.users.list().unwrap().first().unwrap().id, owner.id, "主人恒在列表首");
    }
}
