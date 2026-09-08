//! 给 UI 的整屏快照:boot(开机一次性拉齐)与悬浮窗闲时面板。

use super::*;

/// §7「开窗秒显」的落点:一个 IPC 来回画出首屏。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BootSnapshot {
    pub user: User,
    pub conversation: Conversation,
    pub messages: Vec<Message>,
    pub has_api_key: bool,
    /// 会话还没消息时的场景开场白(引导式上手)。
    pub opening_line: Option<String>,
    /// 用户级语言设置(与皮肤同款,settings scope=user);文案由前端按它选,core 不产文案。
    pub locale: String,
}

/// 悬浮窗待机轮播的"环境信息"(PLAN §12,只读):时间归 OS、余额/今日花费复用现成命令,
/// 这里只补 OS 给不了的两项 —— 下个提醒、最近一句旺财说的话。字段 snake_case(同 DayUsage)。
#[derive(Debug, Clone, Serialize)]
pub struct FloatIdle {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_reminder: Option<FloatReminder>,
    /// 最近一句旺财说的话(已过滤工具轮空串 / __IGNORE__);None = 还没说过。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_line: Option<String>,
    /// 主动关怀候选(PLAN ★主动关怀里程碑,L0 悬浮窗待机):最近没看完的剧「继续看《X》」+
    /// 拖最久的 open 待办「还没办:X」(切片2 小账的 L0 半边),各最多一条、进待机轮播池轮着显。
    /// care.enabled 关 = 空;静默时段的门在前端(本地时钟,同 audio 夜间);文案前端按 kind 选(§6.6)。
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub cares: Vec<CareCandidate>,
}

/// 一条主动关怀候选(中立数据,core 不产文案 §6.6):`kind` 让前端选文案 key,其余是渲染参数。
#[derive(Debug, Clone, Serialize)]
pub struct CareCandidate {
    /// 类型("resume" = 继续看某剧 / "todo" = 没办完的事);前端按它选 i18n,未知 kind 忽略。
    pub kind: String,
    /// 渲染参数:剧名 / 待办内容(填进前端 care.* 的 {title})。
    pub title: String,
    /// 上次看 / 记下的时间(unix ms):前端可据此呈现"搁了多久"或将来做筛。
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct FloatReminder {
    pub content: String,
    /// unix 毫秒(本地时区);前端按它显示"还有多久 / 几点"。
    pub due_at: i64,
}

// ---------- 瞬态状态分层:session 槽 ----------

impl Engine {
    pub fn boot(&self) -> Result<BootSnapshot, AppError> {
        let user = self.store.users.ensure_default_user()?;
        let conversation = match self.store.chat.latest_conversation(user.id)? {
            Some(c) => c,
            None => self.store.chat.create_conversation(user.id, DEFAULT_SCENE_ID)?,
        };
        let messages = self.store.chat.recent_messages(conversation.id, 50)?;
        let locale = self
            .store
            .settings
            .get(Some(user.id), "ui.locale")?
            .unwrap_or_else(|| "zh-CN".into());
        let opening_line = if messages.is_empty() {
            self.scenes.get(&conversation.scene_id).map(|s| s.opening_for(&locale))
        } else {
            None
        };
        Ok(BootSnapshot {
            user,
            conversation,
            messages,
            has_api_key: self.has_provider(),
            opening_line,
            locale,
        })
    }

    /// 悬浮窗待机轮播(PLAN §12):只给 OS 没有的东西 —— 下个提醒 + 最近一句旺财说的话。
    /// 只读、轻量(一次取列表首条 + 一句话);余额/今日花费由前端复用 llm_balance/usage_today。
    pub fn float_idle(&self) -> Result<FloatIdle, AppError> {
        let user = self.store.users.ensure_default_user()?;
        let next_reminder = self
            .store
            .jobs
            .list_pending(user.id)?
            .into_iter()
            .next()
            .map(|j| FloatReminder { content: j.content, due_at: j.due_at });
        let latest_line = match self.store.chat.latest_conversation(user.id)? {
            Some(c) => self.store.chat.latest_assistant_line(c.id)?,
            None => None,
        };
        // 主动关怀候选:总开关缺省开、只有显式 "0" 关;开着给两路——最近一部有剧名的在看剧
        // (「继续看《X》」)+ 拖最久的 open 待办(「还没办:X」,切片2 小账的 L0 半边)。
        // 静默时段 / 呈现节流的门在前端(useFloatIdle,本地时钟)。
        let mut cares = Vec::new();
        if self.store.settings.get(None, "care.enabled")?.as_deref() != Some("0") {
            // 续播进度按家记(2026-09-07):候选取全家最近在看的那部,标题 = 剧名
            if let Some(p) = self.store.media_progress.list_recent(1)?.into_iter().next() {
                cares.push(CareCandidate { kind: "resume".into(), title: p.title, updated_at: p.updated_at });
            }
            if let Some(td) = self.store.todos.oldest_open(user.id)? {
                cares.push(CareCandidate { kind: "todo".into(), title: td.content, updated_at: td.created_at });
            }
        }
        Ok(FloatIdle { next_reminder, latest_line, cares })
    }
}
