//! 家人(多用户)/ 渠道对话指认 / 声纹遗忘。
//!
//! 身份锚死「最早建的用户 = 主人」(id ASC),与 last_active_at 彻底解耦(§7.7)。

use super::*;

impl Engine {
    /// 家人列表(设置·家人 tab);附"是否已录声纹"标记。
    pub fn list_users(&self) -> Result<Vec<(User, bool)>, AppError> {
        let users = self.store.users.list()?;
        let enrolled = self.store.voiceprints.enrolled_ids()?;
        Ok(users.into_iter().map(|u| { let on = enrolled.contains(&u.id); (u, on) }).collect())
    }

    /// 添加家人。
    pub fn create_user(&self, name: &str) -> Result<User, AppError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(AppError { kind: ErrorKind::Internal, message: "名字不能为空".into() });
        }
        Ok(self.store.users.create(name)?)
    }

    /// 给某家人改名(家人 tab 列表行内改;rename_user 改的是默认用户,这条按 id)。
    pub fn rename_family(&self, id: i64, name: &str) -> Result<(), AppError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(AppError { kind: ErrorKind::Internal, message: "名字不能为空".into() });
        }
        self.store.users.rename(id, name)?;
        Ok(())
    }

    pub fn rename_user(&self, name: &str) -> Result<User, AppError> {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 24 {
            return Err(AppError { kind: ErrorKind::Internal, message: "名字为空或过长".into() });
        }
        let user = self.store.users.ensure_default_user()?;
        self.store.users.rename(user.id, name)?;
        Ok(User { name: name.into(), ..user })
    }

    /// 删除家人:守住"至少留一人"+ 编排跨域清理(记忆/声纹随人走,隐私;渠道指认清掉
    /// 回落会话归属者)。会话不删(历史可能混着别人,归属悬空无害;boot 取最近活跃用户兜底)。
    pub fn delete_user(&self, id: i64) -> Result<(), AppError> {
        if self.store.users.count()? <= 1 {
            return Err(AppError { kind: ErrorKind::Internal, message: "至少得留一个人".into() });
        }
        // 按 user_id 归属的表全清 —— `users.id` 是 INTEGER PRIMARY KEY(无 AUTOINCREMENT),
        // **删掉后会被下一个新家人复用**;不清干净的表,新家人就继承了旧家人的残留(2026-08-22 审计)。
        // chat/jobs/memory 有外键 ON DELETE CASCADE(db.rs 开了 foreign_keys=ON)随 users.delete 走;
        // 下面这几张是裸 user_id 列、无外键,必须手动清:
        self.store.memory.delete_for_user(id)?; // (memory 有 CASCADE,这里显式清一道也无妨)
        self.store.voiceprints.remove(id)?;
        self.store.channels.unbind_user(id)?;
        self.store.todos.delete_for_user(id)?; // 待办
        self.store.confirms.delete_for_user(id)?; // 确认足迹
        self.store.fsops.delete_for_user(id)?; // 文件操作记录
        // 续播进度按家记(2026-09-07 起不再归人),删家人不动它
        self.store.usage.delete_for_user(id)?; // 用量流水
        self.store.users.delete(id)?;
        Ok(())
    }

    /// 渠道对话列表(家人页「远程对话」区:这条 TG/钉钉对话是谁在用)。
    pub fn list_channel_chats(&self) -> Result<Vec<crate::store::ChannelThread>, AppError> {
        Ok(self.store.channels.list()?)
    }

    /// 指认某条渠道对话归哪位家人(None = 取消指认,回落会话归属者)。
    /// 校验家人真实存在,防绑到悬空 id(送进 speaker_user 前的同款防线)。
    pub fn bind_channel_chat(
        &self,
        thread_id: i64,
        user_id: Option<i64>,
    ) -> Result<(), AppError> {
        if let Some(uid) = user_id {
            if self.store.users.get(uid)?.is_none() {
                return Err(AppError {
                    kind: ErrorKind::NotFound,
                    message: format!("家人 {uid} 不存在"),
                });
            }
        }
        self.store.channels.bind_user(thread_id, user_id)?;
        Ok(())
    }

    /// 目标用户解析:None = 当前主人(ensure_default_user);Some = 校验真实存在的家人
    /// (回忆页「主人查看家人记忆」/删家人记忆前的同款防线,防绑悬空 id;§渠道归人第二步)。
    pub(super) fn resolve_user(&self, user_id: Option<i64>) -> Result<i64, AppError> {
        match user_id {
            Some(uid) => {
                if self.store.users.get(uid)?.is_none() {
                    return Err(AppError {
                        kind: ErrorKind::NotFound,
                        message: format!("家人 {uid} 不存在"),
                    });
                }
                Ok(uid)
            }
            None => Ok(self.store.users.ensure_default_user()?.id),
        }
    }

    /// 忘掉某家人的声纹(家人页「忘掉声音」):只删声纹,不删人 / 记忆。之后 TA 说话回落
    /// 会话用户(identify 少一个候选)。校家人存在(防悬空 id)。
    pub fn forget_voiceprint(&self, id: i64) -> Result<(), AppError> {
        let uid = self.resolve_user(Some(id))?;
        self.store.voiceprints.remove(uid)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::testkit::engine;

    /// **删家人要清干净所有按 user_id 归属的表(2026-08-22 审计)**。
    ///
    /// `users.id` 是 INTEGER PRIMARY KEY(无 AUTOINCREMENT)→ 删掉最大 id 的家人后,下一个新家人
    /// 复用同一个 id。chat/jobs/memory 有外键 CASCADE 会随删清掉,但 todos/confirms/fsops/
    /// media_progress/usage 是裸 user_id 列、无外键 —— 不手动清,新家人就继承旧家人的待办/确认
    /// 足迹/文件操作记录/续播进度/用量。
    #[test]
    fn deleting_a_family_member_leaves_no_data_for_a_reused_id() {
        let eng = engine("delfam");
        let s = eng.store();
        let owner = s.users.ensure_default_user().unwrap().id; // 至少留一人,别把主人删了
        let ghost = eng.create_user("旧家人").unwrap().id;
        assert!(ghost > owner, "新家人 id 更大(复用的就是它)");

        // 在这四张无 CASCADE 的归人表里各塞一条归 ghost 的数据(续播进度 2026-09-07 起按家记,不在此列)
        s.todos.add(ghost, "旧待办").unwrap();
        s.confirms.record(ghost, 1, "web", "example.com", "erase", "delete", "deny", "channel").unwrap();
        s.fsops.record(ghost, "move", "[]", 1).unwrap();
        s.usage
            .add_round(&crate::store::UsageRound {
                user_id: ghost,
                conversation_id: 1,
                user_msg_id: 1,
                provider_id: "p".into(),
                model: "m".into(),
                input_tokens: 10,
                output_tokens: 5,
                cache_hit_tokens: 0,
                cost_usd: None,
                elapsed_ms: 0,
                ttft_ms: None,
            })
            .unwrap();
        // 塞数据后各 repo 都看得见归 ghost 的那条(用各自的读方法,贴近用户真正会看到的)
        let present = |s: &Store| {
            (
                !s.todos.list_open(ghost, 100).unwrap().is_empty(),
                s.confirms.list_recent(100).unwrap().iter().any(|r| r.user_id == ghost),
                !s.fsops.list_for(ghost, 100).unwrap().is_empty(),
                s.usage.count_for_user(ghost).unwrap() > 0,
            )
        };
        assert_eq!(present(s), (true, true, true, true), "塞数据后四张表都该有 ghost 的行");

        eng.delete_user(ghost).unwrap();

        // 删干净:四张表里归 ghost 的一条不剩
        assert_eq!(present(s), (false, false, false, false), "删家人后四张表都该清空");

        // 复用 id:新家人拿到同一个 id,而且是干净的
        let reborn = eng.create_user("新家人").unwrap().id;
        assert_eq!(reborn, ghost, "SQLite 复用了刚删的 id —— 正是这条 bug 的前提");
        assert!(s.todos.list_open(reborn, 100).unwrap().is_empty(), "新家人不该继承旧待办");
    }
}
