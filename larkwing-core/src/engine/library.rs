//! 记忆 / 需知 / 待办 / 技能 / 提醒 / 文件操作记录 —— UI 那几个列表页的读改删面。
//!
//! 都是薄壳:调各自 Repo,归属靠 `resolve_user`(记忆归人 §4.7)。

use super::*;

/// 提醒页一行(IPC 词汇):Job 全字段平铺 + `owner`(家人的提醒标谁的;自己的 = None 不显)。
#[derive(Debug, Clone, Serialize)]
pub struct ReminderItem {
    #[serde(flatten)]
    pub job: crate::store::Job,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
}

impl Engine {
    /// 小本本(回忆页):看记住了什么。user_id=None → 当前主人;Some → 主人查看某家人
    /// (§渠道归人第二步「主人查看家人记忆」;共享的「家里的事」由 briefings 承载,不在此)。
    pub fn list_memories(&self, user_id: Option<i64>) -> Result<Vec<Memory>, AppError> {
        let uid = self.resolve_user(user_id)?;
        Ok(self.store.memory.list(uid)?)
    }

    /// 最近的记忆维护流水(§13.7 调阈值用:回看「淡出/合并是否过激」)。归当前用户。
    pub fn list_memory_maintenance(
        &self,
        limit: i64,
    ) -> Result<Vec<crate::store::MaintenanceLog>, AppError> {
        let user = self.store.users.ensure_default_user()?;
        Ok(self.store.memory.recent_maintenance(user.id, limit)?)
    }

    /// 记错了点掉(记忆卫生 = 信任感)。user_id=None → 当前主人;Some → 主人删某家人的记忆
    /// (主人管理面,同提醒页;delete 按 (user,id) 限定,删不到别人的行)。
    pub fn delete_memory(&self, user_id: Option<i64>, id: i64) -> Result<(), AppError> {
        let uid = self.resolve_user(user_id)?;
        if !self.store.memory.delete(uid, id)? {
            return Err(AppError {
                kind: ErrorKind::NotFound,
                message: format!("记忆 {id} 不存在"),
            });
        }
        Ok(())
    }

    /// 回忆页「家里的事」分组:某用户视角的家庭备忘 = home(全家共享)+ TA 的个人 scope。
    /// user_id=None → 当前主人;Some → 主人查看某家人视角(共享那份对谁都在,归人的跟着切)。
    pub fn list_briefings(&self, user_id: Option<i64>) -> Result<Vec<Briefing>, AppError> {
        let uid = self.resolve_user(user_id)?;
        Ok(self.store.briefings.list_for(uid)?)
    }

    pub fn delete_briefing(&self, id: i64) -> Result<(), AppError> {
        if !self.store.briefings.remove_by_id(id)? {
            return Err(AppError {
                kind: ErrorKind::NotFound,
                message: format!("备忘 {id} 不存在"),
            });
        }
        Ok(())
    }

    /// 回忆页「没办完的事」分组:开着的待办(切片2 小账,工具 `note_todo` 记的)。
    /// user_id=None → 当前主人;Some → 主人查看某家人(同 list_memories 的主人管理面)。
    /// 上限给宽(远超前缀 `TODO_PREFIX_LIMIT`,那是喂模型的限量,这里是给人看的全量)。
    pub fn list_todos(&self, user_id: Option<i64>) -> Result<Vec<crate::store::Todo>, AppError> {
        let uid = self.resolve_user(user_id)?;
        Ok(self.store.todos.list_open(uid, 200)?)
    }

    /// 回忆页勾掉一件事(办完 / 不用了):按 (user,id) 限定了结,不删行——与工具
    /// `finish_todo` 同语义(done 后不再进前缀、不再露面)。
    pub fn finish_todo(&self, user_id: Option<i64>, id: i64) -> Result<(), AppError> {
        let uid = self.resolve_user(user_id)?;
        if !self.store.todos.close(uid, id)? {
            return Err(AppError {
                kind: ErrorKind::NotFound,
                message: format!("待办 {id} 不存在"),
            });
        }
        Ok(())
    }

    /// 技能页:全部技能(内置 + 用户教的)+ 触发统计三数字(总/近7天/最近)+ 附录节名。
    /// 技能是 agent 的、恒全局 —— 不分用户,无视角参数。
    pub fn list_skills(&self) -> Result<Vec<crate::store::SkillWithStats>, AppError> {
        Ok(self.store.skills.list_with_stats()?)
    }

    /// 技能页开关:停用即从索引消失(下一回合生效),内置技能的「不想用」走这里。
    pub fn set_skill_enabled(&self, id: i64, enabled: bool) -> Result<(), AppError> {
        if !self.store.skills.set_enabled(id, enabled)? {
            return Err(AppError { kind: ErrorKind::NotFound, message: format!("技能 {id} 不存在") });
        }
        Ok(())
    }

    /// 技能页删除(仅用户教的;内置拒 —— repo 层报错,前端对内置也不出删钮)。
    pub fn delete_skill(&self, id: i64) -> Result<(), AppError> {
        if !self.store.skills.delete_by_id(id)? {
            return Err(AppError { kind: ErrorKind::NotFound, message: format!("技能 {id} 不存在") });
        }
        Ok(())
    }

    /// 提醒页 = 主人的管理面:**全家**待触发提醒(按 due_at 升序;真相在库、回合无状态)。
    /// 家人经渠道归人设的提醒也在列,`owner` 标注是谁的(自己的 = None,前端不显标签)——
    /// 2026-07-03 真机困惑实锤:家人对话里设的提醒在提醒页「消失」,其实是按当前用户过滤掉了。
    /// 工具侧 reminder_list/cancel 仍按说话人限定(TA 只看/只撤自己的)。
    pub fn list_reminders(&self) -> Result<Vec<ReminderItem>, AppError> {
        let me = self.store.users.ensure_default_user()?;
        let names: std::collections::HashMap<i64, String> =
            self.store.users.list()?.into_iter().map(|u| (u.id, u.name)).collect();
        Ok(self
            .store
            .jobs
            .list_pending_all()?
            .into_iter()
            .map(|job| {
                let owner = (job.user_id != me.id)
                    .then(|| names.get(&job.user_id).cloned().unwrap_or_default())
                    .filter(|n| !n.is_empty());
                ReminderItem { job, owner }
            })
            .collect())
    }

    /// 提醒页「取消」:撤掉一条提醒。桌面是主人的管理面 → 全家的都可撤(不按用户限定);
    /// 说话人自助取消走工具 reminder_cancel(仍限本人)。
    pub fn cancel_reminder(&self, id: i64) -> Result<(), AppError> {
        if !self.store.jobs.cancel_any(id)? {
            return Err(AppError {
                kind: ErrorKind::NotFound,
                message: format!("提醒 {id} 不存在"),
            });
        }
        Ok(())
    }

    // ---- 文件操作记录(PLAN §9 文件能力):操作记录页 + 撤销/重做 ----

    /// 操作记录页:当前用户最近的文件操作批次(最近在前)。
    pub fn list_fsops(&self) -> Result<Vec<crate::store::FsOpRow>, AppError> {
        let user = self.store.users.ensure_default_user()?;
        Ok(self.store.fsops.list_for(user.id, 100)?)
    }

    /// 撤销一批(操作记录页「撤销」按钮;模型侧另有 fs_undo 工具)。
    /// 返回**逐条结果**:有 skipped 就说明有文件没能还原(被移动过/已不在/不可逆),
    /// 调用方要如实告诉用户 —— 原先这个报告被丢掉,一项没还原也报成功(§3.5)。
    pub fn fsops_undo(&self, id: i64) -> Result<crate::files::OpReport, AppError> {
        self.apply_fsops(id, "applied", "undone", true)
    }

    /// 重做一批(「重做」按钮)。功能性,非安全承诺。返回同上。
    pub fn fsops_redo(&self, id: i64) -> Result<crate::files::OpReport, AppError> {
        self.apply_fsops(id, "undone", "applied", false)
    }

    /// 撤销/重做共用:校归属 + 校当前状态(已是目标态 = 幂等返回)→ 执行 → 翻状态。
    /// 文件 I/O 直接在此(同 delete_* 等阻塞域方法,Tauri 在工作线程跑同步 command)。
    fn apply_fsops(
        &self,
        id: i64,
        from: &str,
        to: &str,
        undo: bool,
    ) -> Result<crate::files::OpReport, AppError> {
        let user = self.store.users.ensure_default_user()?;
        let row = self.store.fsops.get(id)?.ok_or_else(|| AppError {
            kind: ErrorKind::NotFound,
            message: format!("操作记录 {id} 不存在"),
        })?;
        if row.user_id != user.id {
            return Err(AppError { kind: ErrorKind::NotFound, message: "不是你的操作记录".into() });
        }
        if row.state != from {
            return Ok(crate::files::OpReport::default()); // 已是目标状态 → 幂等(前端刷新即可)
        }
        let items: Vec<crate::files::FsOpItem> =
            serde_json::from_str(&row.ops).map_err(AppError::internal)?;
        let report = if undo {
            crate::files::undo_batch(&items)
        } else {
            crate::files::redo_batch(&items)
        };
        if report.skipped > 0 {
            tracing::warn!(id, done = report.done, skipped = report.skipped,
                "撤销/重做有条目没能处理(不可逆/文件已不在),已如实回报前端");
        }
        // 状态照翻:这一批确实走过一遍,剩下的条目也不会自己好起来;差异由 report 如实说。
        self.store.fsops.set_state(id, to)?;
        Ok(report)
    }
}
