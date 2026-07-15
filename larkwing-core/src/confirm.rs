//! 动作确认闸(§7.8):web_render 在第三方网站上点「确认支付」级按钮前,先请用户点头。
//! 判据 = 动作后果「出圈且收不回来」(圈 = 这台电脑 + 家庭信任圈);现状过筛只有
//! web_render 的部分动作过线,但本模块是**通用件**——将来有新工具过线,拿 `Confirmer` 即接。
//! 触发 = 高危词表 ∪ 模型自报,**单向阀**:词表命中自报压不掉,自报命中词表兜不住也问。
//! 定位 = 安全带不是保险库(§7.2 功能口吻):防模型犯错/被页面内容带偏,不承诺对抗
//! 恶意站点(挂羊头按钮任何文本闸都防不了,由可见任务窗 + 用户在场兜)。
//!
//! 词表是写死的具名判断,内容 2026-07-15 用户拍板(§4.11);匹配纯函数单源在此,
//! 壳层(动作执行点,拿活 DOM 文本)与渠道/语音回话判定都调这里,绝无第二份实现。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tokio::sync::oneshot;

use crate::bus::{AppEvent, Bus};
use crate::store::Store;

// ---------- 词表(§4.11 用户拍板,2026-07-15)----------

/// 高危动作词(中文):归一化后**去空白子串**匹配(按钮「付 款」也命中)。
/// 纪律:只收「后果自明」的词;泛词(确认/提交/发送/保存)明确不进表——进表 = 每步都问,
/// 用户会麻木。「确认付款」由「付款」兜住。偏向 = 宁多问(误问代价是多点一下,漏的代价是钱)。
const RISKY_ZH: &[&str] = &[
    // 花钱
    "付款", "支付", "购买", "下单", "结算", "转账", "汇款", "充值", "打赏", "订购", "续费",
    "开通会员", "立即抢购", "拍下",
    // 发出(内容上公网)
    "发布", "发帖", "投递", "发送邮件",
    // 删数(远端数据没了)
    "删除", "注销", "清空", "解绑",
    // 签署/授权
    "签署", "授权", "同意并",
];

/// 高危动作短语(英文):归一化后**词边界**匹配(裸 `pay` 会命中 PayPal、`buy` 命中
/// buyer,故一律短语化/整词;`sign` 因 sign in/up 误伤太广刻意不收)。
const RISKY_EN: &[&str] = &[
    "pay now", "buy now", "checkout", "place order", "purchase", "subscribe", "donate",
    "transfer", "post", "publish", "send email", "tweet", "delete", "deactivate", "erase",
    "authorize", "i agree",
];

/// 渠道回话确认的肯定词(精确匹配,代码判定不交模型——页面注入玩不到这里)。
/// 其他任何回复 = 拒,且那句话照常进回合(用户说「先别,改成 X」自然接上)。
const CHANNEL_YES: &[&str] = &["确认", "继续", "好", "是", "ok", "yes"];

/// 语音口头确认词(整句转写归一化后**精确**匹配;「宁可不认绝不错认」——
/// 肯定词必须干净命中,含糊/听不清一律不算数,回落等卡片/超时)。
const VOICE_YES: &[&str] = &["确认", "可以", "好", "好的", "行", "继续", "同意"];
/// 语音明确否定(命中 = 立即拒,不用等超时)。子串匹配(「先不要」「别点了」都算)。
const VOICE_NO: &[&str] = &["不要", "不用", "别", "算了", "取消", "先不"];

// ---------- 匹配纯函数 ----------

/// 归一化:小写 + 全角字母数字转半角 + 空白折叠为单空格 + 去首尾。
fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_space = true; // 开头视作刚放过空格 → 折叠首部空白
    for c in s.chars() {
        let c = match c {
            // 全角字母/数字/常用标点 → 半角
            '\u{FF01}'..='\u{FF5E}' => char::from_u32(c as u32 - 0xFEE0).unwrap_or(c),
            _ => c,
        };
        if c.is_whitespace() {
            if !last_space {
                out.push(' ');
                last_space = true;
            }
        } else {
            for lc in c.to_lowercase() {
                out.push(lc);
            }
            last_space = false;
        }
    }
    out.trim().to_string()
}

/// 英文短语的词边界命中:短语两侧不能贴着字母/数字(`checkout` 不命中 "check out our
/// blog" 因为那是两个词;`post` 不命中 "postal")。短语内空格已随 normalize 折叠成单空格。
fn en_phrase_hit(norm: &str, phrase: &str) -> bool {
    let bytes = norm.as_bytes();
    let mut from = 0;
    while let Some(pos) = norm[from..].find(phrase) {
        let start = from + pos;
        let end = start + phrase.len();
        let left_ok = start == 0 || !bytes[start - 1].is_ascii_alphanumeric();
        let right_ok = end >= bytes.len() || !bytes[end].is_ascii_alphanumeric();
        if left_ok && right_ok {
            return true;
        }
        from = start + 1;
    }
    false
}

/// 动作目标文本撞没撞高危词表。返回命中的词(确认卡/日志用);None = 干净。
/// 中文去空白子串、英文词边界,单源在此 —— 壳层动作执行点(活 DOM 文本)调它。
pub fn risky_hit(text: &str) -> Option<&'static str> {
    if text.trim().is_empty() {
        return None; // 无文本核不了(图标按钮/press_key)= 软路,靠模型自报兜,记档 §7.8
    }
    let norm = normalize(text);
    let squashed: String = norm.chars().filter(|c| !c.is_whitespace()).collect();
    for w in RISKY_ZH {
        if squashed.contains(w) {
            return Some(w);
        }
    }
    RISKY_EN.iter().find(|p| en_phrase_hit(&norm, p)).copied()
}

/// 渠道回话是不是肯定确认(精确匹配,容忍尾部标点)。
pub fn channel_reply_allows(text: &str) -> bool {
    let norm = normalize(text);
    let norm = norm.trim_end_matches(['!', '。', '.', '~', '\u{FF01}']);
    CHANNEL_YES.contains(&norm)
}

/// 语音转写的三态判定:Some(true)=肯定、Some(false)=明确否定、None=听不清/含糊(不算数)。
/// 肯定要求整句(去语气词后)精确等于肯定词——「宁可不认」;否定子串即可(误拒代价小)。
pub fn voice_reply(text: &str) -> Option<bool> {
    let norm = normalize(text);
    let squashed: String = norm.chars().filter(|c| !c.is_whitespace()).collect();
    if squashed.is_empty() {
        return None;
    }
    if VOICE_NO.iter().any(|w| squashed.contains(w)) {
        return Some(false);
    }
    // 剥掉常见语气尾(「好的呀」「可以啊」);剥完精确匹配肯定词。
    let stripped: String = {
        let mut s = squashed.clone();
        for tail in ["呀", "啊", "呢", "哦", "喔", "嘞", "啦", "吧"] {
            if let Some(rest) = s.strip_suffix(tail) {
                s = rest.to_string();
            }
        }
        s.trim_end_matches(['!', '。', '.', ',', '\u{FF01}', '\u{FF0C}']).to_string()
    };
    if VOICE_YES.contains(&stripped.as_str()) {
        return Some(true);
    }
    None
}

// ---------- 确认请求 / 结局 ----------

/// 一次确认请求(工具 → Confirmer)。
#[derive(Debug, Clone)]
pub struct ConfirmAsk {
    pub user_id: i64,
    pub conv_id: i64,
    /// 回合来源:`ui` / `system` = 桌面(卡片 + 可能的语音问答);渠道名 = 推回那个 chat。
    pub origin: String,
    /// 站点 host(壳层从渲染窗现取;自报路尽力从 url 解析)。
    pub host: String,
    /// 动作原文(按钮文本/提交按钮文本)——页面数据,非 core 文案(§6.6 合规)。
    pub action: String,
    /// click | submit | self_report(模型自报、词表没中)。
    pub kind: String,
}

/// 确认结局。`via`:desktop | float | voice | channel(谁点的头,审计用)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfirmDecision {
    Allowed { via: String },
    Denied { via: String },
    /// 等满超时没人应(桌面 60s / 渠道 120s)。
    TimedOut,
    /// 没有任何确认通道可达(无 UI 订阅者的 headless/单测)——立即拒,不白等。
    NoUi,
}

impl ConfirmDecision {
    pub fn allowed(&self) -> bool {
        matches!(self, ConfirmDecision::Allowed { .. })
    }
}

/// 过桥卡片(bus → 前端 HUD/悬浮窗;渠道 outbound_loop 也消费它推手机)。
/// 全量快照语义同 TaskView:错过任意一条,下一条把状态追平。
#[derive(Debug, Clone, Serialize)]
pub struct ConfirmCard {
    pub id: u64,
    pub user_id: i64,
    pub conv_id: i64,
    pub origin: String,
    pub host: String,
    pub action: String,
    pub kind: String,
    /// pending | allowed | denied | expired
    pub state: String,
    /// 截止时刻(unix ms),前端画倒计时。
    pub deadline_ms: i64,
    /// 终态时:谁点的(desktop/float/voice/channel);pending 无。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
}

/// 桌面确认超时;渠道(人在手机上,反应慢)见 [`CHANNEL_TIMEOUT`]。
pub const DESKTOP_TIMEOUT: Duration = Duration::from_secs(60);
pub const CHANNEL_TIMEOUT: Duration = Duration::from_secs(120);

struct PendingEntry {
    tx: oneshot::Sender<(bool, String)>,
    card: ConfirmCard,
}

/// 确认中枢:pending 注册表 + bus 广播 + 审计落库。engine 构造一份,经 ToolCtx 给工具;
/// 前端 `confirm_action` 命令、渠道回话、语音听音都汇到 [`Confirmer::resolve`],先到先得。
pub struct Confirmer {
    bus: Bus,
    store: Store,
    pending: Mutex<HashMap<u64, PendingEntry>>,
    seq: AtomicU64,
}

impl Confirmer {
    pub fn new(bus: Bus, store: Store) -> Arc<Confirmer> {
        Arc::new(Confirmer { bus, store, pending: Mutex::new(HashMap::new()), seq: AtomicU64::new(1) })
    }

    /// 问一次,等到点头/摇头/超时。drop-safe:回合被取消(future drop)时 guard 摘 pending
    /// 并广播 expired,前端卡片不留尸体。审计流水在此统一落库(§3.5 有问必有档)。
    pub async fn ask(&self, ask: ConfirmAsk, timeout: Duration) -> ConfirmDecision {
        // 没有任何订阅者(headless/单测,壳层未起)= 没有确认通道 → 立即拒,别白等 60s。
        if self.bus.receiver_count() == 0 {
            self.record(&ask, "denied", "no_ui");
            return ConfirmDecision::NoUi;
        }
        let id = self.seq.fetch_add(1, Ordering::Relaxed);
        let deadline_ms = crate::store::now_ms() + timeout.as_millis() as i64;
        let card = ConfirmCard {
            id,
            user_id: ask.user_id,
            conv_id: ask.conv_id,
            origin: ask.origin.clone(),
            host: ask.host.clone(),
            action: ask.action.clone(),
            kind: ask.kind.clone(),
            state: "pending".into(),
            deadline_ms,
            via: None,
        };
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, PendingEntry { tx, card: card.clone() });
        // guard:ask 被 drop(回合取消)也把卡收干净
        let guard = ClearGuard { confirmer: self, id, armed: true };
        self.bus.publish(AppEvent::Confirm(card.clone()));

        let outcome = tokio::time::timeout(timeout, rx).await;
        let mut guard = guard;
        guard.armed = false; // 正常路径自己收尾,guard 退膛
        let decision = match outcome {
            Ok(Ok((true, via))) => ConfirmDecision::Allowed { via },
            Ok(Ok((false, via))) => ConfirmDecision::Denied { via },
            // sender 全 drop(理论不可达:resolve 摘走才 send)按超时收
            Ok(Err(_)) | Err(_) => ConfirmDecision::TimedOut,
        };
        // 终态广播 + 摘表(resolve 路已摘,这里兜超时路)
        self.pending.lock().unwrap().remove(&id);
        let (state, via) = match &decision {
            ConfirmDecision::Allowed { via } => ("allowed", via.clone()),
            ConfirmDecision::Denied { via } => ("denied", via.clone()),
            _ => ("expired", String::new()),
        };
        let mut done = card;
        done.state = state.into();
        done.via = if via.is_empty() { None } else { Some(via.clone()) };
        self.bus.publish(AppEvent::Confirm(done));
        // 审计口径:超时也是「没允许」→ decision=denied、via=timeout(卡片态才叫 expired)
        let log_decision = if state == "expired" { "denied" } else { state };
        self.record(&ask, log_decision, if via.is_empty() { "timeout" } else { &via });
        decision
    }

    /// 应答入口(前端命令/渠道回话/语音听音)。先到先得;id 不在 pending(已过期/已应)
    /// 返回 false,调用方据此告知「已经过期了」。
    pub fn resolve(&self, id: u64, allow: bool, via: &str) -> bool {
        let entry = self.pending.lock().unwrap().remove(&id);
        match entry {
            Some(e) => e.tx.send((allow, via.to_string())).is_ok(),
            None => false,
        }
    }

    /// 这张卡还挂着吗(语音听音开录前查:念问句期间别处已点头就不用听了)。
    pub fn has_pending(&self, id: u64) -> bool {
        self.pending.lock().unwrap().contains_key(&id)
    }

    /// 某会话当前挂着的确认(语音侧「我该不该听」/渠道拦截查 pending 用)。
    pub fn pending_for_conv(&self, conv_id: i64) -> Option<ConfirmCard> {
        self.pending
            .lock()
            .unwrap()
            .values()
            .map(|e| &e.card)
            .find(|c| c.conv_id == conv_id)
            .cloned()
    }

    fn record(&self, ask: &ConfirmAsk, decision: &str, via: &str) {
        if let Err(e) = self.store.confirms.record(
            ask.user_id, ask.conv_id, &ask.origin, &ask.host, &ask.action, &ask.kind, decision, via,
        ) {
            tracing::warn!(error = %e, "确认流水落库失败(不影响决定本身)");
        }
    }
}

/// ask future 被 drop(回合取消)时的收尾:摘 pending + 广播 expired。
struct ClearGuard<'a> {
    confirmer: &'a Confirmer,
    id: u64,
    armed: bool,
}

impl Drop for ClearGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Some(e) = self.confirmer.pending.lock().unwrap().remove(&self.id) {
            let mut card = e.card;
            card.state = "expired".into();
            self.confirmer.bus.publish(AppEvent::Confirm(card));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn risky_lexicon_hits_and_misses() {
        // 中文子串 + 空白容忍 + 全角归一
        assert_eq!(risky_hit("确认支付 ¥128.00"), Some("支付"));
        assert_eq!(risky_hit("付 款"), Some("付款"));
        assert_eq!(risky_hit("立即购买"), Some("购买"));
        assert_eq!(risky_hit("删除账号"), Some("删除"));
        assert_eq!(risky_hit("同意并继续"), Some("同意并"));
        // 泛词不进表:日常导航绝不误伤
        assert_eq!(risky_hit("下一页"), None);
        assert_eq!(risky_hit("确认"), None);
        assert_eq!(risky_hit("提交"), None);
        assert_eq!(risky_hit("保存草稿"), None);
        assert_eq!(risky_hit("下载电子票"), None);
        assert_eq!(risky_hit("查看订单"), None); // 「下单」不是「订单」的子串
        // 英文词边界
        assert_eq!(risky_hit("Pay Now"), Some("pay now"));
        assert_eq!(risky_hit("Proceed to Checkout"), Some("checkout"));
        assert_eq!(risky_hit("Check out our blog"), None, "两个词不是 checkout");
        assert_eq!(risky_hit("PayPal"), None, "裸 pay 不进表");
        assert_eq!(risky_hit("Postal code"), None, "词边界挡 postal");
        assert_eq!(risky_hit("Post"), Some("post"));
        assert_eq!(risky_hit("Delete my account"), Some("delete"));
        assert_eq!(risky_hit("Sign in"), None, "sign 刻意不收");
        // 空文本 = 软路
        assert_eq!(risky_hit("  "), None);
    }

    #[test]
    fn channel_reply_matching_is_exact() {
        assert!(channel_reply_allows("确认"));
        assert!(channel_reply_allows(" 好 "));
        assert!(channel_reply_allows("OK"));
        assert!(channel_reply_allows("继续!"));
        assert!(!channel_reply_allows("确认一下再说")); // 非精确 = 拒(照常进回合)
        assert!(!channel_reply_allows("先别"));
        assert!(!channel_reply_allows("改成到店自提"));
    }

    #[test]
    fn voice_reply_three_way() {
        assert_eq!(voice_reply("确认"), Some(true));
        assert_eq!(voice_reply("好的呀"), Some(true));
        assert_eq!(voice_reply("可以啊"), Some(true));
        assert_eq!(voice_reply("不要"), Some(false));
        assert_eq!(voice_reply("先不点了"), Some(false));
        assert_eq!(voice_reply("算了算了"), Some(false));
        // 含糊/带尾巴的肯定 = 不算数(宁可不认),回落等卡片
        assert_eq!(voice_reply("好像可以吧再想想"), None);
        assert_eq!(voice_reply("嗯嗯嗯"), None);
        assert_eq!(voice_reply(""), None);
        // 整句里带否定优先(「好 算了」这类口癖)
        assert_eq!(voice_reply("好 算了"), Some(false));
    }

    fn test_confirmer(tag: &str) -> (Arc<Confirmer>, Bus) {
        let dir = std::env::temp_dir().join(format!("lw-confirm-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _ = std::fs::remove_file(dir.join("t.db"));
        let store = Store::open(&dir.join("t.db")).unwrap();
        store.users.ensure_default_user().unwrap();
        let bus = Bus::new();
        (Confirmer::new(bus.clone(), store), bus)
    }

    fn ask() -> ConfirmAsk {
        ConfirmAsk {
            user_id: 1,
            conv_id: 1,
            origin: "ui".into(),
            host: "x.example.com".into(),
            action: "确认支付 ¥128.00".into(),
            kind: "click".into(),
        }
    }

    #[tokio::test]
    async fn no_subscriber_denies_immediately() {
        let (c, _bus) = test_confirmer("noui");
        let t = std::time::Instant::now();
        let d = c.ask(ask(), Duration::from_secs(60)).await;
        assert_eq!(d, ConfirmDecision::NoUi);
        assert!(t.elapsed() < Duration::from_secs(1), "无订阅者必须立即拒,不白等");
    }

    #[tokio::test]
    async fn resolve_allows_and_broadcasts_final_state() {
        let (c, bus) = test_confirmer("allow");
        let mut rx = bus.subscribe();
        let c2 = c.clone();
        let task = tokio::spawn(async move { c2.ask(ask(), Duration::from_secs(30)).await });
        // 收 pending 卡 → resolve
        let card = loop {
            if let AppEvent::Confirm(card) = rx.recv().await.unwrap() {
                break card;
            }
        };
        assert_eq!(card.state, "pending");
        assert!(c.pending_for_conv(1).is_some());
        assert!(c.resolve(card.id, true, "desktop"));
        let d = task.await.unwrap();
        assert_eq!(d, ConfirmDecision::Allowed { via: "desktop".into() });
        // 终态卡
        let done = loop {
            if let AppEvent::Confirm(card) = rx.recv().await.unwrap() {
                break card;
            }
        };
        assert_eq!((done.state.as_str(), done.via.as_deref()), ("allowed", Some("desktop")));
        assert!(c.pending_for_conv(1).is_none());
        // 二次 resolve = 已收尾
        assert!(!c.resolve(card.id, false, "float"));
        // 审计落了一行
        let log = c.store.confirms.list_recent(10).unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!((log[0].decision.as_str(), log[0].via.as_str()), ("allowed", "desktop"));
    }

    #[tokio::test]
    async fn timeout_expires_card_and_records() {
        let (c, bus) = test_confirmer("timeout");
        let mut rx = bus.subscribe();
        let d = c.ask(ask(), Duration::from_millis(50)).await;
        assert_eq!(d, ConfirmDecision::TimedOut);
        let mut states = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let AppEvent::Confirm(card) = ev {
                states.push(card.state);
            }
        }
        assert_eq!(states, vec!["pending", "expired"]);
        let log = c.store.confirms.list_recent(10).unwrap();
        assert_eq!((log[0].decision.as_str(), log[0].via.as_str()), ("denied", "timeout"));
    }

    #[tokio::test]
    async fn dropped_ask_clears_pending_and_expires_card() {
        let (c, bus) = test_confirmer("drop");
        let mut rx = bus.subscribe();
        let c2 = c.clone();
        let task = tokio::spawn(async move { c2.ask(ask(), Duration::from_secs(30)).await });
        // 等 pending 出现再取消(模拟回合取消级联)
        loop {
            if let AppEvent::Confirm(card) = rx.recv().await.unwrap() {
                assert_eq!(card.state, "pending");
                break;
            }
        }
        task.abort();
        let _ = task.await;
        // guard 收尾:pending 清空 + expired 卡
        assert!(c.pending_for_conv(1).is_none());
        let done = loop {
            if let AppEvent::Confirm(card) = rx.recv().await.unwrap() {
                break card;
            }
        };
        assert_eq!(done.state, "expired");
    }
}
