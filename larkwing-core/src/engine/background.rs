//! 后台 LLM 杂活:记忆提炼 / 家庭日记 / 会话命名。
//!
//! 共同纪律:都走**最便宜档**(`background_provider`)、都必须挡 FakeLlm(否则偷
//! eval / 测试的剧本队列)、失败只 warn 不打扰用户。

use super::providers::cheapest_candidate;
use super::*;

/// 每隔这么多用户回合,后台自动提炼一次记忆(PLAN §13 Phase 3 自动触发)。
/// 偏稀:蒸馏 = 花钱的 LLM 调用,且 lookback 50 条本就覆盖多轮 → 稀一点、重叠靠去重兜。
const CONSOLIDATE_EVERY_TURNS: u32 = 12;

impl Engine {
    /// 记忆提炼 / 反思(PLAN §13 Phase 3):把一段会话蒸馏成耐久记忆。**保守**——只增不删、
    /// 提炼条目进按需层(不污染前缀)、近重复跳过(详见 `consolidate`)。后台尽力件:没配
    /// provider / 会话不存在 = 返回 0,不报错。返回新增条数。
    /// 这是**手动 / 命令入口**(按 conv_user 提炼);**自动触发**走 `spawn_consolidate`
    /// (`send_message` 每 `CONSOLIDATE_EVERY_TURNS` 个用户回合后台跑、按说话人提炼,2026-06-18 接上)。
    pub async fn consolidate_conversation(&self, conv_id: i64) -> Result<usize, AppError> {
        // cheap-model 路由(§13.6 变体 A):后台提炼走**最便宜档** provider,不烧聊天主选的钱。
        // helper 锁内只取 Arc 快照即放锁,await 在锁外(RwLock guard 不跨 await)。
        let Some(provider) = self.background_provider() else { return Ok(0) };
        let Some(conv) = self.store.chat.get_conversation(conv_id)? else { return Ok(0) };
        let added = consolidate::run(&provider, &self.store, conv.user_id, conv_id, 50).await?;
        Ok(added)
    }

    /// 后台活儿(记忆提炼 / 维护 / 会话定题)选哪个 provider:在已建候选里挑**最便宜的一档**
    /// (catalog tier 最低),与用户聊天「用脑策略」(`llm.strategy`)**解耦** —— 后台杂活不该烧
    /// 旗舰模型的钱(§4.4「钥匙是用户的、路由是产品的」+ §13.6 cheap-model 路由,2026-06-24 变体 A)。
    /// 复用现有档位目录(`catalog::tier_of`),**不新增模型名 / 设置项**(不触发 §4.11、守 §3 收口)。
    /// 单 provider 用户 = 选到它本身 = 与主选一致(**零回归**)。锁内只取 Arc 快照,await 在锁外。
    fn background_provider(&self) -> Option<Arc<dyn LlmProvider>> {
        let candidates = self.llm.rd();
        cheapest_candidate(&candidates).cloned()
    }

    /// 累加该会话「自上次提炼以来的用户回合数」,到阈值则归零并返回 true(该后台提炼了)。
    /// 纯计数(只改 SessionSlot 瞬态、无 IO),便于单测;真正的提炼由 `spawn_consolidate` 起。
    pub(super) fn bump_consolidate_due(&self, conv_id: i64) -> bool {
        let mut sessions = self.sessions.lk();
        let slot = sessions.entry(conv_id).or_default();
        slot.turns_since_consolidate += 1;
        if slot.turns_since_consolidate >= CONSOLIDATE_EVERY_TURNS {
            slot.turns_since_consolidate = 0;
            true
        } else {
            false
        }
    }

    /// 记忆自动提炼总开关(`memory.auto_consolidate`,app 级):缺省 = 开;只有显式 "0" 才关。
    fn auto_consolidate_enabled(&self) -> bool {
        self.store
            .settings
            .get(None, "memory.auto_consolidate")
            .ok()
            .flatten()
            .map(|v| v != "0")
            .unwrap_or(true)
    }

    /// 后台提炼一次该会话(PLAN §13 Phase 3 自动触发):尽力件 —— 开关关 / 没 provider / 上次还在跑
    /// 则跳过,错误只记日志,绝不影响主对话。写到说话人 `user_id`(记忆归人 §6)。
    /// cheap-model 路由已接(§13.6 变体 A,2026-06-24):走 `background_provider`(最便宜档);
    /// 触发频率见 `CONSOLIDATE_EVERY_TURNS`。
    pub(super) fn spawn_consolidate(&self, conv_id: i64, user_id: i64) {
        // 用户在设置关掉了自动提炼 = 直接不跑(手动入口不受影响)
        if !self.auto_consolidate_enabled() {
            return;
        }
        // 上次提炼还没跑完 = 跳过这轮(防并发重复落库;flag 持在会话槽,spawn 任务跑完清)
        let flag = {
            let mut sessions = self.sessions.lk();
            sessions.entry(conv_id).or_default().consolidating.clone()
        };
        if flag.swap(true, Ordering::AcqRel) {
            return;
        }
        // cheap-model 路由(§13.6 变体 A):后台提炼走最便宜档 provider,与聊天主选解耦。
        let Some(provider) = self.background_provider() else {
            flag.store(false, Ordering::Release);
            return;
        };
        let store = self.store.clone();
        tokio::spawn(async move {
            match consolidate::run(&provider, &store, user_id, conv_id, 50).await {
                Ok(n) if n > 0 => {
                    tracing::info!(target: "larkwing::memory", conv = conv_id, added = n, "记忆自动提炼:+{n} 条")
                }
                Ok(_) => {
                    tracing::debug!(target: "larkwing::memory", conv = conv_id, "记忆自动提炼:无新增")
                }
                Err(e) => {
                    tracing::warn!(target: "larkwing::memory", conv = conv_id, "记忆自动提炼失败(尽力件): {e:#}")
                }
            }
            flag.store(false, Ordering::Release);
        });
    }

    /// 家庭日记「这些日子」后台补写(engine/diary.rs,2026-07-09;2026-07-13 触发改版
    /// 「攒够 + 空闲」):scheduler 每个节拍喊一声,这里做便宜闸门 —— care.enabled 显式关 =
    /// 不写;**任一回合在飞就让路**(空闲才动笔;§7.4 忙检教训:看 `join.is_finished()`,
    /// 不看 `inflight.is_some()`;不耗尝试冷却,回合结束后下个节拍就能试);每 5 分钟最多
    /// 真试一次(攒够/安静窗的库侧判断在 diary::run,只是几个轻查询;10 分钟空闲窗按这个
    /// 粒度不漏);防重入;**匿名 provider(model_id 空)不跑**(spawn_title 同款挡板:
    /// FakeLlm 剧本队列是共享弹出,后台杂活去弹会偷走测试/eval 的脚本回合)。尽力件,绝不阻塞调用方。
    pub fn spawn_diary(&self, now_ms: i64) {
        const TRY_INTERVAL_MS: i64 = 300_000;
        if self.store.settings.get(None, "care.enabled").ok().flatten().as_deref() == Some("0") {
            return; // 随主动关怀总开关收口(§3 一个开关,不添新概念)
        }
        {
            let sessions = self.sessions.lk();
            let busy = sessions
                .values()
                .any(|slot| slot.inflight.as_ref().is_some_and(|h| !h.join.is_finished()));
            if busy {
                return;
            }
        }
        let last = self.diary_last_try.load(Ordering::Acquire);
        if now_ms.saturating_sub(last) < TRY_INTERVAL_MS {
            return;
        }
        if self.diary_inflight.swap(true, Ordering::AcqRel) {
            return;
        }
        self.diary_last_try.store(now_ms, Ordering::Release);
        let release = |flag: &Arc<AtomicBool>| flag.store(false, Ordering::Release);
        let Some(provider) = self.background_provider() else {
            release(&self.diary_inflight);
            return;
        };
        if provider.model_id().is_empty() {
            release(&self.diary_inflight);
            return;
        }
        let store = self.store.clone();
        let flag = self.diary_inflight.clone();
        tokio::spawn(async move {
            match diary::run(&provider, &store, now_ms).await {
                Ok(n) if n > 0 => {
                    tracing::info!(target: "larkwing::diary", days = n, "家庭日记:补写 {n} 天")
                }
                Ok(_) => tracing::debug!(target: "larkwing::diary", "家庭日记:无需补写"),
                Err(e) => {
                    tracing::warn!(target: "larkwing::diary", "家庭日记补写失败(尽力件,下个节拍再试): {e:#}")
                }
            }
            flag.store(false, Ordering::Release);
        });
    }

    /// 回忆页「这些日子」:日记流(日期新→旧)。home 共有一本,不随「看谁的」切换。
    pub fn list_diary(&self, limit: usize) -> Result<Vec<DiaryEntry>, AppError> {
        Ok(self.store.diary.list(limit)?)
    }

    /// 删掉一天的日记(回忆页右键)。
    pub fn delete_diary(&self, id: i64) -> Result<bool, AppError> {
        Ok(self.store.diary.delete(id)?)
    }

    /// 后台给新会话起标题(engine/title.rs,方案 A):非流式调**最便宜档** provider(与提炼同一条
    /// cheap-model 路由),`set_title_if` CAS 替换截断占位 —— 用户已重命名绝不覆盖;成功后广播
    /// `AppEvent::ConvTitle`,前端原位改字。尽力件:没 provider / 失败 = 占位保留,只记日志。
    /// 每会话至多一次(只有「标题为空」的那次 send 才有 seed),无需防并发闸。
    /// **匿名 provider(model_id 空)不起**:空 = 目录不认识的未知形(fake/脚本),记账链路同款降级
    /// 语义;这同时兜住 eval / 引擎测试 —— FakeLlm 的剧本队列是共享弹出(fake.rs),后台杂活
    /// 若也去弹会偷走脚本回合、破坏确定性(spawn_consolidate 靠 12 回合阈值天然躲开,这里必须显式挡)。
    pub(super) fn spawn_title(&self, conv_id: i64, seed: String) {
        let Some(provider) = self.background_provider() else { return };
        if provider.model_id().is_empty() {
            return;
        }
        let store = self.store.clone();
        let bus = self.bus.clone();
        tokio::spawn(async move {
            match title::run(&provider, &store, conv_id, &seed).await {
                Ok(Some(t)) => {
                    tracing::debug!(target: "larkwing::chat", conv = conv_id, title = %t, "会话已定题");
                    bus.publish(crate::bus::AppEvent::ConvTitle(crate::bus::ConvTitle {
                        conv_id,
                        title: t,
                    }));
                }
                Ok(None) => {} // 模型没给出可用标题 / 用户已改名 → 占位保留,不吵
                Err(e) => {
                    tracing::debug!(target: "larkwing::chat", conv = conv_id, "会话定题失败(尽力件): {e:#}")
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::testkit::engine;

    /// 自动提炼计数:前 N-1 轮不触发、第 N 轮触发并归零、不同会话各自独立。
    #[test]
    fn consolidate_due_fires_every_n_turns_and_resets() {
        let eng = engine("consol-due");
        let conv = 1i64;
        for i in 1..CONSOLIDATE_EVERY_TURNS {
            assert!(!eng.bump_consolidate_due(conv), "第 {i} 轮不该触发");
        }
        assert!(eng.bump_consolidate_due(conv), "第 N 轮该触发");
        assert!(!eng.bump_consolidate_due(conv), "触发后计数归零、又从头数");
        assert!(!eng.bump_consolidate_due(2), "另一会话独立计数,不受影响");
    }

    /// 没配 provider 时后台提炼直接放弃(不 panic、不卡住 in-flight 标志)。
    #[test]
    fn spawn_consolidate_noops_without_provider() {
        let eng = engine("consol-noprov");
        eng.spawn_consolidate(1, 1); // 无 provider → 早退;flag 复位,可再次进入
        eng.spawn_consolidate(1, 1);
    }

    /// 自动提炼总开关:缺省 = 开;设 0 关、设回 1 开;非 0/1 被拒(顺带验白名单 + 校验臂)。
    #[test]
    fn auto_consolidate_setting_defaults_on_and_respects_off() {
        let eng = engine("consol-toggle");
        assert!(eng.auto_consolidate_enabled(), "缺省即开");
        eng.set_setting("memory.auto_consolidate", "0").unwrap();
        assert!(!eng.auto_consolidate_enabled(), "设 0 = 关");
        eng.set_setting("memory.auto_consolidate", "1").unwrap();
        assert!(eng.auto_consolidate_enabled(), "设回 1 = 开");
        assert!(eng.set_setting("memory.auto_consolidate", "2").is_err(), "非 0/1 被拒");
    }
}
