//! 微信渠道适配器:腾讯官方 **iLink bot HTTP API**(`ilinkai.weixin.qq.com/ilink/bot/*`)over
//! `net::Client`(**不装 `@tencent-weixin/openclaw-weixin` npm 插件**——它是 Node.js、插 OpenClaw CLI 的;
//! 照对 Telegram 的姿势裸接协议,插件 MIT 全量 TS 源码当规格书)。形态同构 telegram.rs:
//! 入站 `getupdates` 长轮询(游标 `get_updates_buf` 字符串)、出站 `sendmessage`;鉴权 = 扫码拿
//! `Bearer <bot_token>` + 固定头(`iLink-App-Id: bot` / `AuthorizationType: ilink_bot_token` /
//! `X-WECHAT-UIN` 随机 uint32 base64 / `iLink-App-ClientVersion`)。免公网、免 SDK。
//!
//! 比 TG 多出来的三件:① **扫码登录**(`qr_start`/`qr_poll_and_store`,唯一真·新活);
//! ② **`context_token` 回显**(每条入站消息带的会话令牌,回复/推送原样回传;复用
//! `channel_threads.push_id` 列存最新值——语义正是「出站收件地址」,零迁移);
//! ③ **媒体 AES-128-ECB**(CDN 走 `full_url` 下载 + AES-128-ECB/PKCS7 解密喂 InAttachment;
//! 语音先用服务端 `voice_item.text`,SILK 解码后置 = 真机 watch-item)。
//!
//! 协议常量(base/cdn url、app-id、bot_type)= **协议事实**(同 `telegram::API` 常量),
//! 单源本文件顶部,非 §4.11 产品默认。

use std::sync::Arc;
use std::time::Duration;

use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use aes::Aes128;
use anyhow::{bail, ensure, Context, Result};
use base64::Engine as _;
use md5::{Digest, Md5};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use super::{drive_turn, split_message, ChannelCtx, ATTACH_HINT};
use crate::engine::InAttachment;
use crate::net;

const CHANNEL: &str = "weixin";

// ── 协议常量(协议事实,非产品默认;单源此处)────────────────────────────────
/// iLink bot API 固定入口;扫码登录后服务端可能给一个 IDC 专属 `baseurl` 覆盖(存 settings)。
pub(super) const DEFAULT_BASE_URL: &str = "https://ilinkai.weixin.qq.com";
/// CDN 媒体收发端点(下载/上传 c2c 加密件)。
const CDN_BASE_URL: &str = "https://novac2c.cdn.weixin.qq.com/c2c";
/// iLink-App-Id:插件 package.json 的 `ilink_appid` = "bot"(探针实证:带此头即可拿二维码)。
const APP_ID: &str = "bot";
/// iLink-App-ClientVersion:uint32 = major<<16|minor<<8|patch,取插件当前 2.4.6 编码(观测用,非鉴权)。
const APP_CLIENT_VERSION: u32 = (2 << 16) | (4 << 8) | 6;
/// 扫码 bot_type(插件 DEFAULT_ILINK_BOT_TYPE)。
const BOT_TYPE: &str = "3";

// ── 运行参数 ──────────────────────────────────────────────────────────────
/// getupdates 服务端长轮询挂起窗口约 35s;net client 超时须 > 它(留足余量)。
const POLL_CLIENT_TIMEOUT_S: u64 = 90;
/// 扫码状态长轮询(get_qrcode_status)服务端约 35s;命令侧 client 超时。
const QR_CLIENT_TIMEOUT_S: u64 = 45;
/// 微信单条文本发送切片上限(字符;远小于平台上限,保守稳妥)。
const WX_MAX: usize = 2000;
/// 入站媒体下载上限(与桌面同缝;超大件如实退回)。
const MEDIA_MAX_BYTES: u64 = 50 * 1024 * 1024;
/// 出站文件上限(超限如实退回,绝不静默截断)。
const FILE_MAX_BYTES: u64 = 50 * 1024 * 1024;
/// getupdates 游标持久化的 settings 键(内部运行态,非用户配置;跨重启避免重放积压)。
/// 多绑定后每账号一个游标 `{UPDATES_BUF_KEY}.{user_id}`;迁移来的无身份账号沿用裸键。
const UPDATES_BUF_KEY: &str = "remote.weixin.updates_buf";
/// 多绑定列表(secrets,JSON 数组;含 token 故整块进 keyring,同 llm.providers 姿势)。
const ACCOUNTS_KEY: &str = "remote.weixin.accounts";

// ═══════════════════════════════════════════════════════════════════════════
// 多绑定(2026-07-11 真机实锤定形:iLink bot 是「一人一 bot」——扫码 = 给那个微信号申请
// 专属 bot 实例;曾经的单值 token 让第二个人扫码顶掉第一个、旧 bot 无人轮询显示「无法连接」。
// 家庭多人 = 每人扫码各养一个 bot,全部并存、每路独立收发)
// ═══════════════════════════════════════════════════════════════════════════

/// 一个绑定 = 一个微信号上的 bot 实例。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub(super) struct Account {
    pub token: String,
    /// IDC 重定向后的专属入口;空 = DEFAULT_BASE_URL(各账号可能不同,per-account 存)。
    #[serde(default)]
    pub base_url: String,
    /// 绑定者的 ilink_user_id:收消息放行、出站按线程 ext_id 匹配账号的键。
    /// 旧单 token 迁移来的账号为空(身份未知,放行靠手动白名单 = 与旧行为等价)。
    #[serde(default)]
    pub user_id: String,
}

impl Account {
    pub(super) fn base(&self) -> &str {
        if self.base_url.trim().is_empty() {
            DEFAULT_BASE_URL
        } else {
            self.base_url.trim()
        }
    }
    /// 本账号的 getupdates 游标键(迁移账号沿用裸键,天然接上旧游标)。
    fn buf_key(&self) -> String {
        if self.user_id.is_empty() {
            UPDATES_BUF_KEY.to_string()
        } else {
            format!("{UPDATES_BUF_KEY}.{}", self.user_id)
        }
    }
}

/// 读绑定列表;accounts 键缺失时**惰性迁移**旧单值 token(≤v0.2.15)成一个无身份账号
/// (user_id 空 → 放行靠手动白名单,与旧行为等价;重新扫码即可补齐身份)。
pub(super) fn load_accounts(settings: &crate::store::SettingsRepo) -> Vec<Account> {
    if let Some(json) =
        crate::secrets::get(settings, ACCOUNTS_KEY).filter(|s| !s.trim().is_empty())
    {
        return serde_json::from_str::<Vec<Account>>(&json).unwrap_or_else(|e| {
            tracing::warn!(err = %e, "微信绑定列表解析失败,当空处理(重新扫码可重建)");
            Vec::new()
        });
    }
    let Some(token) =
        crate::secrets::get(settings, "remote.weixin.token").filter(|s| !s.trim().is_empty())
    else {
        return Vec::new();
    };
    let base_url =
        settings.get(None, "remote.weixin.base_url").ok().flatten().unwrap_or_default();
    let migrated = vec![Account { token, base_url, user_id: String::new() }];
    if let Err(e) = save_accounts(settings, &migrated) {
        tracing::warn!(err = %format!("{e:#}"), "旧微信 token 迁移绑定列表失败(下次再试)");
    } else {
        tracing::info!("旧微信单 token 已迁移成绑定列表(身份未知,重新扫码可补齐)");
    }
    migrated
}

fn save_accounts(settings: &crate::store::SettingsRepo, list: &[Account]) -> Result<()> {
    let json = serde_json::to_string(list).context("绑定列表序列化失败")?;
    crate::secrets::set(settings, ACCOUNTS_KEY, &json).context("保存微信绑定列表失败")
}

/// 追加/替换一个绑定:同 user_id = 重登替换;否则顶掉「无身份」的迁移账号(同一台机器上
/// 不知道是谁的旧绑定,首个有身份的扫码即接管);都不是 = 追加(家里再来一个人)。
fn append_account(settings: &crate::store::SettingsRepo, acc: Account) -> Result<()> {
    let mut list = load_accounts(settings);
    if let Some(slot) = list
        .iter_mut()
        .find(|a| (!acc.user_id.is_empty() && a.user_id == acc.user_id) || a.user_id.is_empty())
    {
        *slot = acc;
    } else {
        list.push(acc);
    }
    save_accounts(settings, &list)
}

/// 解绑一个账号(user_id 空 = 解绑迁移来的无身份账号);顺带清它的游标。
pub(super) fn unbind_account(settings: &crate::store::SettingsRepo, user_id: &str) -> Result<()> {
    let list = load_accounts(settings);
    let (gone, keep): (Vec<_>, Vec<_>) = list.into_iter().partition(|a| a.user_id == user_id);
    anyhow::ensure!(!gone.is_empty(), "没有这个绑定(可能已解绑)");
    for a in &gone {
        let _ = settings.delete(None, &a.buf_key());
    }
    save_accounts(settings, &keep)
}

/// 出站选账号:线程 ext_id(= 绑定者 ilink_user_id)精确匹配 → 唯一账号兜底(含迁移的
/// 无身份账号)→ 找不到给明白话(TA 的绑定被解/被旧版顶掉,重新扫码即可)。
pub(super) fn account_for(
    settings: &crate::store::SettingsRepo,
    ext_id: &str,
) -> Result<Account> {
    let list = load_accounts(settings);
    ensure!(!list.is_empty(), "没绑定微信(先在设置·远程扫码登录)");
    if let Some(a) = list.iter().find(|a| !a.user_id.is_empty() && a.user_id == ext_id) {
        return Ok(a.clone());
    }
    if list.len() == 1 {
        return Ok(list[0].clone());
    }
    bail!("这个微信对话对应的绑定不在了(可能被解绑或被旧版本顶掉)——让 TA 重新扫码绑一次")
}

// 消息/项类型(protocol enum;见插件 api/types.ts)
const MSG_TYPE_USER: i64 = 1;
const ITEM_TEXT: i64 = 1;
const ITEM_IMAGE: i64 = 2;
const ITEM_VOICE: i64 = 3;
const ITEM_FILE: i64 = 4;
const ITEM_VIDEO: i64 = 5;
const MEDIA_IMAGE: i64 = 1;
const MEDIA_VIDEO: i64 = 2;
const MEDIA_FILE: i64 = 3;

// ⚠️ core 侧静态话术(§6.6 债,同 telegram ONBOARD_HINT):渠道操作性提示,不经模型。
const ONBOARD_HINT: &str =
    "你好,我是{name}。你的用户 ID 是 {id},把它加到设置·远程渠道(微信)的白名单里,我们就能聊啦。";
const ERR_HINT: &str = "(出了点问题,稍后再试试)";
const UNSUPPORTED_HINT: &str =
    "这个我还看不了~现在能收:文字、图片,和常见文档(PDF/Word/Excel/PPT/文本)。";
const FILE_TOO_BIG_HINT: &str = "这个文件有点大,50MB 以内的我才收得动。";
/// 语音暂无服务端转写文字时的兜底(SILK 解码后置,§3.5 不静默):如实说听不了。
const VOICE_HINT: &str = "这条语音我这还听不了,打字或发文字版给我吧。";

// ═══════════════════════════════════════════════════════════════════════════
// 渠道入口 + 长轮询循环(镜像 telegram::run/serve)
// ═══════════════════════════════════════════════════════════════════════════

/// 渠道入口:建长超时 net client,跑服务循环;出错退避重连(不静默失败 §3.5)。
pub(super) async fn run(ctx: Arc<ChannelCtx>, ct: CancellationToken) {
    // 多绑定:每账号一路长轮询(各自 token/入口/游标),全部并存。绑定列表变化
    // (扫码新增/解绑)由 reload_channels 停旧起新,这里启动时快照一次即可。
    let accounts = load_accounts(&ctx.engine.store().settings);
    if accounts.is_empty() {
        ctx.set_state(CHANNEL, false, Some("没绑定微信(先扫码登录)".into()));
        tracing::info!("微信渠道:无绑定,不起");
        return;
    }
    ctx.set_state(CHANNEL, true, None);
    let mut tasks = Vec::new();
    for acc in accounts {
        let (ctx, ct) = (ctx.clone(), ct.clone());
        tasks.push(tokio::spawn(async move { run_account(ctx, acc, ct).await }));
    }
    for t in tasks {
        let _ = t.await;
    }
    ctx.set_state(CHANNEL, false, None);
    tracing::info!("微信渠道已停");
}

/// 单账号的「出错退避重连」外壳(原 run 的循环体,per-account 化)。
/// 状态行粒度是渠道级:某路出错时 running 保持 true(别的路还活着),错误串进状态行
/// ——真机排「谁掉线了」看日志的 who 字段。
async fn run_account(ctx: Arc<ChannelCtx>, acc: Account, ct: CancellationToken) {
    let net = net::Client::new(|b| b.timeout(Duration::from_secs(POLL_CLIENT_TIMEOUT_S)));
    let who = if acc.user_id.is_empty() { "(旧迁移绑定)" } else { acc.user_id.as_str() };
    while !ct.is_cancelled() {
        match serve(&ctx, &net, &acc, &ct).await {
            Ok(()) => break, // 正常返回 = 被取消
            Err(e) => {
                let msg = format!("{who}: {e:#}");
                tracing::warn!(err = %msg, "微信绑定出错,5s 后重连");
                ctx.set_state(CHANNEL, true, Some(msg));
                tokio::select! {
                    _ = ct.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                }
            }
        }
    }
}

async fn serve(
    ctx: &ChannelCtx,
    net: &net::Client,
    acc: &Account,
    ct: &CancellationToken,
) -> Result<()> {
    let token = acc.token.clone();
    let base = acc.base().to_string();
    // 放行 = 手动白名单 ∪ 全部绑定者(1:1 形态下每路只有绑定者会说话;绑定即放行,
    // 手动名单归纯手动——扫码不再往里写)
    let mut allowed = allowed_users(ctx);
    for a in load_accounts(&ctx.engine.store().settings) {
        if !a.user_id.is_empty() && !allowed.contains(&a.user_id) {
            allowed.push(a.user_id);
        }
    }
    let buf_key = acc.buf_key();
    let mut buf =
        ctx.engine.store().settings.get(None, &buf_key).ok().flatten().unwrap_or_default();
    tracing::info!(
        who = %if acc.user_id.is_empty() { "(旧迁移绑定)" } else { &acc.user_id },
        allow = allowed.len(),
        has_cursor = !buf.is_empty(),
        "微信绑定在线"
    );

    loop {
        if ct.is_cancelled() {
            return Ok(());
        }
        // 长轮询期间也能被取消(否则要等满服务端窗口)
        let resp = tokio::select! {
            _ = ct.cancelled() => return Ok(()),
            r = get_updates(net, &base, &token, &buf) => r?,
        };
        // API 错误(ret/errcode != 0):退避重试(stale token 由此浮现到状态行,重新扫码即可)
        let ret = resp.get("ret").and_then(Value::as_i64).unwrap_or(0);
        let errcode = resp.get("errcode").and_then(Value::as_i64).unwrap_or(0);
        if ret != 0 || errcode != 0 {
            let errmsg = resp.get("errmsg").and_then(Value::as_str).unwrap_or("");
            tracing::warn!(ret, errcode, errmsg, "微信 getupdates 返回错误,2s 后重试");
            tokio::select! {
                _ = ct.cancelled() => return Ok(()),
                _ = tokio::time::sleep(Duration::from_secs(2)) => {}
            }
            continue;
        }
        // 推进游标并持久化(空串不覆盖;per-account 键)
        if let Some(nb) = resp.get("get_updates_buf").and_then(Value::as_str) {
            if !nb.is_empty() && nb != buf {
                buf = nb.to_string();
                let _ = ctx.engine.store().settings.set(None, &buf_key, &buf);
            }
        }
        let msgs = resp.get("msgs").and_then(Value::as_array).cloned().unwrap_or_default();
        for m in &msgs {
            if let Some(parsed) = parse_message(m) {
                handle_message(ctx, net, &base, &token, &allowed, parsed).await;
            }
        }
    }
}

/// 处理一条入站消息:访问控制 → 下载媒体(如有)→ 复用 turn loop → 回复 → 落 context_token。
async fn handle_message(
    ctx: &ChannelCtx,
    net: &net::Client,
    base: &str,
    token: &str,
    allowed: &[String],
    p: Parsed,
) {
    // 访问控制(非风控 §9):配了白名单只放行名单内;空 = 谁来都先发 onboarding 报 user id
    if !allowed.is_empty() {
        if !allowed.contains(&p.from_user_id) {
            return; // 已设名单的陌生用户:静默忽略
        }
    } else {
        let hint = ONBOARD_HINT.replace("{name}", &ctx.engine.pet_name()).replace("{id}", &p.from_user_id);
        let _ = send_text(net, base, token, &p.from_user_id, &p.context_token, &hint).await;
        return;
    }

    // 媒体:下载 + 解密 → 桌面同缝 InAttachment(图当轮注入 / 文档抽文字进 history,§9)
    let mut attachments = Vec::new();
    let mut text = p.text.clone();
    if let Some(media) = &p.media {
        match media.kind {
            // 语音:优先服务端转写文字;没有则如实说听不了(SILK 解码后置)
            MediaKind::Voice => {
                if text.trim().is_empty() {
                    let _ = send_text(net, base, token, &p.from_user_id, &p.context_token, VOICE_HINT).await;
                    persist_context_token(ctx, &p.from_user_id, &p.context_token);
                    return;
                }
            }
            _ => match download_media(net, media).await {
                Ok(bytes) if bytes.len() as u64 <= MEDIA_MAX_BYTES => {
                    attachments.push(InAttachment {
                        name: media.name.clone(),
                        mime: media.mime.clone(),
                        data: base64::engine::general_purpose::STANDARD.encode(&bytes),
                    });
                }
                Ok(_) => {
                    let _ =
                        send_text(net, base, token, &p.from_user_id, &p.context_token, FILE_TOO_BIG_HINT).await;
                    persist_context_token(ctx, &p.from_user_id, &p.context_token);
                    return;
                }
                Err(e) => {
                    tracing::warn!(err = %format!("{e:#}"), "微信媒体下载/解密失败");
                    let _ = send_text(net, base, token, &p.from_user_id, &p.context_token, ERR_HINT).await;
                    persist_context_token(ctx, &p.from_user_id, &p.context_token);
                    return;
                }
            },
        }
    }
    // 既无文字也无附件:看不了的内容(表情/位置/名片…)如实回一句(§3.5),纯空/服务性则不搭理
    if text.trim().is_empty() && attachments.is_empty() {
        if p.unsupported {
            let _ = send_text(net, base, token, &p.from_user_id, &p.context_token, UNSUPPORTED_HINT).await;
            persist_context_token(ctx, &p.from_user_id, &p.context_token);
        }
        return;
    }
    // 语音转写文字作 payload 记录(不置 speak,渠道回复是文字)
    let input = if p.media.as_ref().map(|m| m.kind == MediaKind::Voice).unwrap_or(false) {
        Some("voice_msg")
    } else {
        None
    };
    // 攒批(A,§7.7):微信发文件/图不能同时打字 → 纯附件消息(有附件、没文字)先攒着、
    // **不触发回合**,等用户发来文字一起处理。防抖靠 buffer_attachments(缓冲空→满才提示,
    // 连发多个只第一个吭声)。提示用当前这条消息的 context_token 即时回,不碰 thread ——
    // 会话映射/持久化留给文字那轮的 drive_turn(那时的 context_token 才是要回显的最新值)。
    if !attachments.is_empty() && text.trim().is_empty() {
        if ctx.buffer_attachments(CHANNEL, &p.from_user_id, attachments) {
            let _ = send_text(net, base, token, &p.from_user_id, &p.context_token, ATTACH_HINT).await;
        }
        return;
    }
    // 有文字(或纯文字):把之前攒着的文件捞出来,连同本次附件一起处理
    let mut attachments = attachments;
    let mut pending = ctx.take_attachments(CHANNEL, &p.from_user_id);
    if !pending.is_empty() {
        pending.extend(attachments);
        attachments = pending;
    }

    let out = drive_turn(&ctx.engine, CHANNEL, &p.from_user_id, text, p.sender.as_deref(), attachments, input)
        .await;
    // context_token 落库(未来提醒/发文件回显要):thread 已由 drive_turn 建好
    persist_context_token(ctx, &p.from_user_id, &p.context_token);
    match out {
        Ok(Some(reply)) => {
            for piece in split_message(&reply, WX_MAX) {
                if let Err(e) = send_text(net, base, token, &p.from_user_id, &p.context_token, &piece).await {
                    tracing::warn!(err = %format!("{e:#}"), "微信发送失败");
                    break;
                }
            }
        }
        Ok(None) => {} // 折进在飞回合(inject),不单独回
        Err(e) => {
            tracing::warn!(err = %format!("{e:#}"), "微信回合失败");
            let _ = send_text(net, base, token, &p.from_user_id, &p.context_token, ERR_HINT).await;
        }
    }
}

/// 落最新 context_token 到映射(复用 push_id 列;set_push_id 空白不写、变了才写)。
fn persist_context_token(ctx: &ChannelCtx, ext_id: &str, context_token: &str) {
    let _ = ctx.engine.store().channels.set_push_id(CHANNEL, ext_id, context_token);
}

// ═══════════════════════════════════════════════════════════════════════════
// 入站消息解析(纯函数,可测)
// ═══════════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, PartialEq)]
enum MediaKind {
    Image,
    Video,
    File,
    Voice,
}

/// 一个 CDN 媒体引用(下载所需:full_url 优先,回落 encrypt_query_param 拼 URL;aes_key 已归一为
/// parse_aes_key 认得的 base64 形)。
#[derive(Debug, Clone, PartialEq)]
struct MediaRef {
    kind: MediaKind,
    full_url: Option<String>,
    encrypt_query_param: Option<String>,
    /// base64(raw16) 或 base64(hex32-ascii);None = 明文下载(无 aes_key)。
    aes_key: Option<String>,
    name: String,
    mime: String,
}

#[derive(Debug, Clone, PartialEq)]
struct Parsed {
    from_user_id: String,
    context_token: String,
    text: String,
    media: Option<MediaRef>,
    /// 平台昵称(暂无稳定来源,占位 None;给家人页认脸,不参与逻辑)。
    sender: Option<String>,
    /// 有内容项但既非文字也非可下载媒体(表情/位置/名片等)→ 该回「看不了」而非静默(§3.5)。
    unsupported: bool,
}

/// 从一条 WeixinMessage(getupdates 元素)解析出 (发送者/令牌/文字/首个媒体)。
/// 只收 message_type=USER(跳过我们自己发的 BOT 回声);无 from_user_id → None。
/// 文字 = 各 TEXT 项 + 语音 `voice_item.text`(服务端 ASR);媒体按 图>视频>文件>语音 取第一个。
fn parse_message(m: &Value) -> Option<Parsed> {
    let mtype = m.get("message_type").and_then(Value::as_i64).unwrap_or(MSG_TYPE_USER);
    if mtype != MSG_TYPE_USER {
        return None; // BOT 回声 / 其它:不处理
    }
    let from_user_id = m.get("from_user_id").and_then(Value::as_str)?.trim().to_string();
    if from_user_id.is_empty() {
        return None;
    }
    let context_token =
        m.get("context_token").and_then(Value::as_str).unwrap_or("").to_string();
    let items = m.get("item_list").and_then(Value::as_array).cloned().unwrap_or_default();

    let mut text = String::new();
    let mut media: Option<MediaRef> = None;
    let mut unsupported = false;
    for item in &items {
        let itype = item.get("type").and_then(Value::as_i64).unwrap_or(0);
        match itype {
            ITEM_TEXT => {
                if let Some(t) = item.pointer("/text_item/text").and_then(Value::as_str) {
                    push_line(&mut text, t.trim());
                }
            }
            ITEM_VOICE => {
                // 服务端转写文字(有就当文字用);媒体标记 Voice(无文字时兜底提示)
                if let Some(t) = item.pointer("/voice_item/text").and_then(Value::as_str) {
                    push_line(&mut text, t.trim());
                }
                if media.is_none() {
                    media = Some(MediaRef {
                        kind: MediaKind::Voice,
                        full_url: None,
                        encrypt_query_param: None,
                        aes_key: None,
                        name: "voice".into(),
                        mime: "audio/silk".into(),
                    });
                }
            }
            ITEM_IMAGE => {
                if !matches!(media, Some(MediaRef { kind: MediaKind::Image, .. })) {
                    media = image_ref(item).or(media);
                }
            }
            ITEM_VIDEO => {
                if media.is_none() {
                    media = cdn_ref(item, "video_item", MediaKind::Video, "video.mp4", "video/mp4");
                }
            }
            ITEM_FILE => {
                if media.is_none() {
                    let name = item
                        .pointer("/file_item/file_name")
                        .and_then(Value::as_str)
                        .filter(|s| !s.trim().is_empty())
                        .unwrap_or("file")
                        .to_string();
                    let mime = crate::attach::image_mime_by_ext(&name)
                        .map(str::to_string)
                        .unwrap_or_else(|| "application/octet-stream".into());
                    media = cdn_ref_named(item, "file_item", MediaKind::File, &name, &mime);
                }
            }
            // 用户发来但我们不认的内容项(表情/位置/名片/小程序卡等;11/12 是 bot 侧工具项不算)
            t if t > 0 && t != 11 && t != 12 => unsupported = true,
            _ => {}
        }
    }
    Some(Parsed { from_user_id, context_token, text, media, sender: None, unsupported })
}

/// 图片项 → MediaRef:aeskey 归一(优先 `image_item.aeskey` hex → base64(raw16),回落 media.aes_key)。
fn image_ref(item: &Value) -> Option<MediaRef> {
    let media = item.pointer("/image_item/media")?;
    let full_url = media.get("full_url").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
    let eqp = media
        .get("encrypt_query_param")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    full_url.as_ref()?; // 至少要有一个下载入口
    let aes_key = item
        .pointer("/image_item/aeskey")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .and_then(|hex_key| hex::decode(hex_key).ok())
        .map(|raw| base64::engine::general_purpose::STANDARD.encode(raw))
        .or_else(|| media.get("aes_key").and_then(Value::as_str).map(str::to_string));
    Some(MediaRef {
        kind: MediaKind::Image,
        full_url,
        encrypt_query_param: eqp,
        aes_key,
        name: "image.jpg".into(),
        mime: "image/jpeg".into(),
    })
}

fn cdn_ref(item: &Value, field: &str, kind: MediaKind, name: &str, mime: &str) -> Option<MediaRef> {
    cdn_ref_named(item, field, kind, name, mime)
}

/// 通用 CDN 项(voice/file/video)→ MediaRef;aes_key 取 media.aes_key(parse_aes_key 认得的形)。
fn cdn_ref_named(item: &Value, field: &str, kind: MediaKind, name: &str, mime: &str) -> Option<MediaRef> {
    let media = item.pointer(&format!("/{field}/media"))?;
    let full_url =
        media.get("full_url").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
    let eqp = media
        .get("encrypt_query_param")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    if full_url.is_none() && eqp.is_none() {
        return None;
    }
    let aes_key = media.get("aes_key").and_then(Value::as_str).map(str::to_string);
    Some(MediaRef {
        kind,
        full_url,
        encrypt_query_param: eqp,
        aes_key,
        name: name.into(),
        mime: mime.into(),
    })
}

fn push_line(buf: &mut String, s: &str) {
    if s.is_empty() {
        return;
    }
    if !buf.is_empty() {
        buf.push('\n');
    }
    buf.push_str(s);
}

// ═══════════════════════════════════════════════════════════════════════════
// API 调用(getupdates / sendmessage / getuploadurl)
// ═══════════════════════════════════════════════════════════════════════════

/// X-WECHAT-UIN:随机 uint32 → 十进制字符串 → base64(镜像插件 randomWechatUin)。
fn random_wechat_uin() -> String {
    let mut b = [0u8; 4];
    let _ = getrandom::getrandom(&mut b);
    let n = u32::from_be_bytes(b);
    base64::engine::general_purpose::STANDARD.encode(n.to_string().as_bytes())
}

fn join(base: &str, endpoint: &str) -> String {
    format!("{}/{}", base.trim_end_matches('/'), endpoint)
}

/// POST JSON(带 token 的鉴权头);返回解析后的响应 JSON。HTTP 非 2xx 冒泡报错。
async fn post_json(net: &net::Client, base: &str, endpoint: &str, body: &Value, token: Option<&str>) -> Result<Value> {
    let url = join(base, endpoint);
    let resp = net
        .send(&url, |c| {
            let mut rb = c
                .post(&url)
                .json(body)
                .header("AuthorizationType", "ilink_bot_token")
                .header("X-WECHAT-UIN", random_wechat_uin())
                .header("iLink-App-Id", APP_ID)
                .header("iLink-App-ClientVersion", APP_CLIENT_VERSION.to_string());
            if let Some(t) = token {
                rb = rb.header("Authorization", format!("Bearer {t}"));
            }
            rb
        })
        .await
        .with_context(|| format!("{endpoint} 请求失败"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    ensure!(status.is_success(), "{endpoint} HTTP {status}: {text}");
    serde_json::from_str(&text).with_context(|| format!("{endpoint} 响应非 JSON: {text}"))
}

/// GET(仅通用头,无 token;用于扫码状态长轮询)。
async fn get_json(net: &net::Client, base: &str, endpoint: &str) -> Result<Value> {
    let url = join(base, endpoint);
    let resp = net
        .send(&url, |c| {
            c.get(&url)
                .header("iLink-App-Id", APP_ID)
                .header("iLink-App-ClientVersion", APP_CLIENT_VERSION.to_string())
        })
        .await
        .with_context(|| format!("{endpoint} 请求失败"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    ensure!(status.is_success(), "{endpoint} HTTP {status}: {text}");
    serde_json::from_str(&text).with_context(|| format!("{endpoint} 响应非 JSON: {text}"))
}

/// 长轮询取新消息(游标回传;返回原始响应 JSON 交 serve 处理)。
async fn get_updates(net: &net::Client, base: &str, token: &str, buf: &str) -> Result<Value> {
    let body = json!({ "get_updates_buf": buf, "base_info": base_info() });
    post_json(net, base, "ilink/bot/getupdates", &body, Some(token)).await
}

/// 发一条文本消息(to_user_id + 回显 context_token)。
async fn send_text(
    net: &net::Client,
    base: &str,
    token: &str,
    to_user_id: &str,
    context_token: &str,
    text: &str,
) -> Result<()> {
    let item = json!({ "type": ITEM_TEXT, "text_item": { "text": text } });
    send_item(net, base, token, to_user_id, context_token, item).await
}

/// 发一条结构化消息项(文本/图片/文件…共用出口)。
async fn send_item(
    net: &net::Client,
    base: &str,
    token: &str,
    to_user_id: &str,
    context_token: &str,
    item: Value,
) -> Result<()> {
    let mut msg = json!({
        "from_user_id": "",
        "to_user_id": to_user_id,
        "client_id": random_client_id(),
        "message_type": 2, // BOT
        "message_state": 2, // FINISH
        "item_list": [item],
    });
    if !context_token.is_empty() {
        msg["context_token"] = Value::String(context_token.to_string());
    }
    let body = json!({ "msg": msg, "base_info": base_info() });
    let resp = post_json(net, base, "ilink/bot/sendmessage", &body, Some(token)).await?;
    let ret = resp.get("ret").and_then(Value::as_i64).unwrap_or(0);
    if ret != 0 {
        let errmsg = resp.get("errmsg").and_then(Value::as_str).unwrap_or("");
        bail!("sendmessage ret={ret} errmsg={errmsg}");
    }
    Ok(())
}

fn base_info() -> Value {
    json!({ "channel_version": env!("CARGO_PKG_VERSION"), "bot_agent": "Larkwing" })
}

fn random_client_id() -> String {
    let mut b = [0u8; 12];
    let _ = getrandom::getrandom(&mut b);
    format!("larkwing-{}", hex::encode(b))
}

// ═══════════════════════════════════════════════════════════════════════════
// 提醒推送 / 出站文件(mod.rs / outbound.rs 用)
// ═══════════════════════════════════════════════════════════════════════════

/// 生效的 base_url:扫码时可能被服务端换成 IDC 专属地址(存 settings),否则默认入口。
/// 提醒推回手机(mod.rs outbound_loop):主动往某用户发一段文本(带上次 context_token)。
pub(super) async fn push(
    net: &net::Client,
    base: &str,
    token: &str,
    to_user_id: &str,
    context_token: &str,
    text: &str,
) -> Result<()> {
    for piece in split_message(text, WX_MAX) {
        send_text(net, base, token, to_user_id, context_token, &piece).await?;
    }
    Ok(())
}

/// 出站文件(outbound.rs send_file 的微信臂):上传 CDN(AES 加密)→ 发媒体项。
/// caption 作单独文本项先发(微信媒体项不带说明,与插件 sendMediaItems 同)。
pub(super) async fn send_file(
    net: &net::Client,
    base: &str,
    token: &str,
    to_user_id: &str,
    context_token: &str,
    path: &std::path::Path,
    caption: Option<&str>,
) -> Result<()> {
    let meta = std::fs::metadata(path).with_context(|| format!("读不到文件 {}", path.display()))?;
    ensure!(meta.is_file(), "{} 不是文件", path.display());
    ensure!(meta.len() <= FILE_MAX_BYTES, "文件超过微信的 {}MB 上限", FILE_MAX_BYTES >> 20);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .context("路径没有文件名")?;
    let bytes = tokio::fs::read(path).await.with_context(|| format!("读文件失败 {}", path.display()))?;

    if let Some(cap) = caption.filter(|s| !s.is_empty()) {
        send_text(net, base, token, to_user_id, context_token, cap).await?;
    }
    let (media_type, item) = upload_and_build_item(net, base, token, to_user_id, &name, &bytes).await?;
    let _ = media_type;
    send_item(net, base, token, to_user_id, context_token, item).await
}

/// 上传本机文件到 CDN(getUploadUrl → AES-128-ECB 加密 → PUT)→ 构造对应 MessageItem。
async fn upload_and_build_item(
    net: &net::Client,
    base: &str,
    token: &str,
    to_user_id: &str,
    name: &str,
    plaintext: &[u8],
) -> Result<(i64, Value)> {
    let media_type = media_type_by_name(name);
    let rawsize = plaintext.len();
    let rawfilemd5 = {
        let mut h = Md5::new();
        h.update(plaintext);
        hex::encode(h.finalize())
    };
    let filesize = aes_ecb_padded_size(rawsize); // 密文尺寸(PKCS7 到 16 边界)
    let filekey = hex::encode(rand16());
    let aeskey_raw = rand16();
    let aeskey_hex = hex::encode(aeskey_raw);

    let body = json!({
        "filekey": filekey,
        "media_type": media_type,
        "to_user_id": to_user_id,
        "rawsize": rawsize,
        "rawfilemd5": rawfilemd5,
        "filesize": filesize,
        "no_need_thumb": true,
        "aeskey": aeskey_hex,
        "base_info": base_info(),
    });
    let resp = post_json(net, base, "ilink/bot/getuploadurl", &body, Some(token)).await?;
    let upload_full_url =
        resp.get("upload_full_url").and_then(Value::as_str).filter(|s| !s.trim().is_empty());
    let upload_param = resp.get("upload_param").and_then(Value::as_str).filter(|s| !s.is_empty());
    let cdn_url = match (upload_full_url, upload_param) {
        (Some(full), _) => full.trim().to_string(),
        (None, Some(param)) => {
            format!("{CDN_BASE_URL}/upload?encrypted_query_param={}&filekey={}", enc(param), enc(&filekey))
        }
        (None, None) => bail!("getuploadurl 未返回上传地址: {resp}"),
    };

    let ciphertext = aes_ecb_encrypt(plaintext, &aeskey_raw);
    let download_param = cdn_put(net, &cdn_url, ciphertext).await?;

    // CDNMedia.aes_key = base64(hex32-ascii)(与入站 parse_aes_key 的第二种编码一致)
    let aes_key_b64 = base64::engine::general_purpose::STANDARD.encode(aeskey_hex.as_bytes());
    let media = json!({
        "encrypt_query_param": download_param,
        "aes_key": aes_key_b64,
        "encrypt_type": 1,
    });
    let item = match media_type {
        MEDIA_IMAGE => json!({ "type": ITEM_IMAGE, "image_item": { "media": media, "mid_size": filesize } }),
        MEDIA_VIDEO => json!({ "type": ITEM_VIDEO, "video_item": { "media": media, "video_size": filesize } }),
        _ => json!({ "type": ITEM_FILE, "file_item": { "media": media, "file_name": name, "len": rawsize.to_string() } }),
    };
    Ok((media_type, item))
}

/// PUT 密文到 CDN,取响应头 `x-encrypted-param` = 下载加密参数(镜像插件 uploadBufferToCdn)。
async fn cdn_put(net: &net::Client, url: &str, ciphertext: Vec<u8>) -> Result<String> {
    let resp = net
        .send(url, |c| {
            c.post(url).header("Content-Type", "application/octet-stream").body(ciphertext.clone())
        })
        .await
        .context("CDN 上传请求失败")?;
    let status = resp.status();
    let dl = resp
        .headers()
        .get("x-encrypted-param")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    ensure!(status.is_success(), "CDN 上传 HTTP {status}");
    dl.context("CDN 上传响应缺 x-encrypted-param 头")
}

fn media_type_by_name(name: &str) -> i64 {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" => MEDIA_IMAGE,
        "mp4" | "mov" | "m4v" | "mkv" | "avi" | "webm" => MEDIA_VIDEO,
        _ => MEDIA_FILE,
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// 媒体下载 + AES-128-ECB(PKCS7)
// ═══════════════════════════════════════════════════════════════════════════

/// 下载 CDN 媒体并(有 aes_key 时)AES-128-ECB 解密,返回明文字节。
async fn download_media(net: &net::Client, r: &MediaRef) -> Result<Vec<u8>> {
    let url = match (&r.full_url, &r.encrypt_query_param) {
        (Some(full), _) => full.clone(),
        (None, Some(eqp)) => format!("{CDN_BASE_URL}/download?encrypted_query_param={}", enc(eqp)),
        (None, None) => bail!("媒体无下载地址"),
    };
    let resp = net.send(&url, |c| c.get(&url)).await.context("CDN 下载请求失败")?;
    ensure!(resp.status().is_success(), "CDN 下载 HTTP {}", resp.status());
    let bytes = resp.bytes().await.context("读媒体字节失败")?.to_vec();
    match &r.aes_key {
        Some(k) => {
            let key = parse_aes_key(k)?;
            aes_ecb_decrypt(&bytes, &key)
        }
        None => Ok(bytes),
    }
}

/// aes_key JSON 字段 → 16 字节裸密钥。两种在野编码:base64(raw16) 或 base64(hex32-ascii)。
fn parse_aes_key(b64: &str) -> Result<[u8; 16]> {
    let decoded = base64::engine::general_purpose::STANDARD.decode(b64).context("aes_key base64 解码失败")?;
    if decoded.len() == 16 {
        let mut k = [0u8; 16];
        k.copy_from_slice(&decoded);
        return Ok(k);
    }
    if decoded.len() == 32 {
        if let Ok(raw) = hex::decode(&decoded) {
            if raw.len() == 16 {
                let mut k = [0u8; 16];
                k.copy_from_slice(&raw);
                return Ok(k);
            }
        }
    }
    bail!("aes_key 应解码为 16 字节裸密钥或 32 字符 hex,得到 {} 字节", decoded.len())
}

/// 密文尺寸(PKCS7 填充到 16 边界;至少补 1 字节 → 恰好整块时也 +16)。
fn aes_ecb_padded_size(plain: usize) -> usize {
    (plain / 16 + 1) * 16
}

fn aes_ecb_encrypt(plaintext: &[u8], key: &[u8; 16]) -> Vec<u8> {
    let cipher = Aes128::new(GenericArray::from_slice(key));
    // PKCS7 填充
    let pad = 16 - (plaintext.len() % 16);
    let mut buf = Vec::with_capacity(plaintext.len() + pad);
    buf.extend_from_slice(plaintext);
    buf.extend(std::iter::repeat(pad as u8).take(pad));
    for chunk in buf.chunks_mut(16) {
        let block = GenericArray::from_mut_slice(chunk);
        cipher.encrypt_block(block);
    }
    buf
}

fn aes_ecb_decrypt(ciphertext: &[u8], key: &[u8; 16]) -> Result<Vec<u8>> {
    ensure!(!ciphertext.is_empty() && ciphertext.len() % 16 == 0, "密文长度非 16 的倍数");
    let cipher = Aes128::new(GenericArray::from_slice(key));
    let mut buf = ciphertext.to_vec();
    for chunk in buf.chunks_mut(16) {
        let block = GenericArray::from_mut_slice(chunk);
        cipher.decrypt_block(block);
    }
    // 去 PKCS7 填充(容错:填充非法则原样返回,避免因少数畸形件整个失败)
    if let Some(&pad) = buf.last() {
        let pad = pad as usize;
        if (1..=16).contains(&pad) && pad <= buf.len() && buf[buf.len() - pad..].iter().all(|&b| b as usize == pad) {
            buf.truncate(buf.len() - pad);
        }
    }
    Ok(buf)
}

fn rand16() -> [u8; 16] {
    let mut b = [0u8; 16];
    let _ = getrandom::getrandom(&mut b);
    b
}

fn enc(s: &str) -> String {
    // 最小 URL 编码(百分号编码非 unreserved 字符);CDN 参数是 base64ish,含 +/=
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ═══════════════════════════════════════════════════════════════════════════
// 扫码登录(qr_start / qr_poll_and_store)——由 channels::mod 的 pub 包装暴露给命令层
// ═══════════════════════════════════════════════════════════════════════════

/// 扫码登录起手:结果给设置 UI 展示(二维码 SVG + 备用链接 + 轮询用的 qrcode)。
#[derive(Debug, Clone, serde::Serialize)]
pub struct QrStart {
    /// 轮询状态用的 qrcode 标识。
    pub qrcode: String,
    /// 备用链接(二维码扫不了时点开)。
    pub qr_url: String,
    /// 渲染好的二维码 SVG(前端 v-html 直接展示,免前端二维码依赖)。
    pub qr_svg: String,
}

/// 扫码轮询一次的结果(前端据 status 驱动:redirect 更 base、need_verifycode 弹输入、confirmed 收工)。
#[derive(Debug, Clone, serde::Serialize)]
pub struct QrPoll {
    /// wait / scaned / need_verifycode / verify_blocked / expired / redirect / confirmed / already。
    pub status: String,
    /// redirect 时 = 新 base_url(前端下次轮询回传);其余 None。
    pub base_url: Option<String>,
}

/// 起手:POST get_bot_qrcode 拿二维码 URL,渲染成 SVG。
pub(super) async fn qr_start() -> Result<QrStart> {
    let net = net::Client::new(|b| b.timeout(Duration::from_secs(QR_CLIENT_TIMEOUT_S)));
    let endpoint = format!("ilink/bot/get_bot_qrcode?bot_type={BOT_TYPE}");
    let body = json!({ "local_token_list": [] });
    let resp = post_json(&net, DEFAULT_BASE_URL, &endpoint, &body, None).await?;
    let qrcode = resp.get("qrcode").and_then(Value::as_str).context("get_bot_qrcode 无 qrcode")?.to_string();
    let qr_url =
        resp.get("qrcode_img_content").and_then(Value::as_str).context("get_bot_qrcode 无二维码链接")?.to_string();
    let qr_svg = render_qr_svg(&qr_url)?;
    Ok(QrStart { qrcode, qr_url, qr_svg })
}

/// 轮询扫码状态;confirmed 时把 token/base_url/白名单落库(storage 归 core,§6.6)。
/// `base_url` = 前端持有的当前轮询地址(IDC 重定向后回传;空 = 默认入口)。
pub(super) async fn qr_poll_and_store(
    engine: &crate::engine::Engine,
    qrcode: &str,
    base_url: Option<&str>,
    verify_code: Option<&str>,
) -> Result<QrPoll> {
    let net = net::Client::new(|b| b.timeout(Duration::from_secs(QR_CLIENT_TIMEOUT_S)));
    let base = base_url.map(str::trim).filter(|s| !s.is_empty()).unwrap_or(DEFAULT_BASE_URL);
    let mut endpoint = format!("ilink/bot/get_qrcode_status?qrcode={}", enc(qrcode));
    if let Some(code) = verify_code.map(str::trim).filter(|s| !s.is_empty()) {
        endpoint.push_str(&format!("&verify_code={}", enc(code)));
    }
    let resp = match get_json(&net, base, &endpoint).await {
        Ok(v) => v,
        // 网关超时(524)/网络抖动:当 wait 继续轮(镜像插件 pollQRStatus 兜底)
        Err(e) => {
            tracing::debug!(err = %format!("{e:#}"), "扫码状态轮询网络错误,当 wait 重试");
            return Ok(QrPoll { status: "wait".into(), base_url: None });
        }
    };
    let status = resp.get("status").and_then(Value::as_str).unwrap_or("wait");
    match status {
        "scaned_but_redirect" => {
            let host = resp.get("redirect_host").and_then(Value::as_str).filter(|s| !s.is_empty());
            Ok(QrPoll {
                status: "redirect".into(),
                base_url: host.map(|h| format!("https://{h}")),
            })
        }
        "confirmed" => {
            let token = resp.get("bot_token").and_then(Value::as_str).unwrap_or("").to_string();
            let bot_id = resp.get("ilink_bot_id").and_then(Value::as_str).unwrap_or("");
            ensure!(!bot_id.is_empty(), "登录确认但服务端未返回 ilink_bot_id");
            let confirmed_base =
                resp.get("baseurl").and_then(Value::as_str).filter(|s| !s.is_empty()).unwrap_or(base);
            let user_id = resp.get("ilink_user_id").and_then(Value::as_str).unwrap_or("");
            store_login(engine, &token, confirmed_base, user_id)?;
            Ok(QrPoll { status: "confirmed".into(), base_url: None })
        }
        "binded_redirect" => Ok(QrPoll { status: "already".into(), base_url: None }),
        "verify_code_blocked" => Ok(QrPoll { status: "verify_blocked".into(), base_url: None }),
        "need_verifycode" => Ok(QrPoll { status: "need_verifycode".into(), base_url: None }),
        // wait / scaned / expired 原样透传
        other => Ok(QrPoll { status: other.to_string(), base_url: None }),
    }
}

/// 登录成功落库:token(secrets)+ base_url(settings)+ 扫码人进白名单 + 清旧游标(换号从头收)。
fn store_login(engine: &crate::engine::Engine, token: &str, base_url: &str, user_id: &str) -> Result<()> {
    let settings = &engine.store().settings;
    ensure!(!token.is_empty(), "登录确认但服务端未返回 bot_token");
    // 多绑定:扫码 = 追加/替换一个账号(同 user_id 重登替换;新身份追加——家里第二个人
    // 扫码不再顶掉第一个)。白名单不再自动写:绑定者身份随账号列表自带放行(serve 并集),
    // 「允许的用户」框回归纯手动(此前扫码往里追加,让用户以为名单被顶换)。
    let acc = Account {
        token: token.to_string(),
        base_url: if base_url == DEFAULT_BASE_URL { String::new() } else { base_url.to_string() },
        user_id: user_id.to_string(),
    };
    append_account(settings, acc)?;
    // 清该账号的 getupdates 游标(重登换 token,旧游标可能拉空/错乱)
    let key = if user_id.is_empty() {
        UPDATES_BUF_KEY.to_string()
    } else {
        format!("{UPDATES_BUF_KEY}.{user_id}")
    };
    let _ = settings.delete(None, &key);
    // 旧单值键退役:留存量不再写(load_accounts 只在 accounts 缺失时才看它做迁移)
    Ok(())
}

/// 二维码 URL → SVG 字符串(qrcode crate;免前端二维码依赖)。
fn render_qr_svg(url: &str) -> Result<String> {
    use qrcode::render::svg;
    use qrcode::QrCode;
    let code = QrCode::new(url.as_bytes()).context("生成二维码失败")?;
    Ok(code
        .render::<svg::Color>()
        .min_dimensions(240, 240)
        .quiet_zone(true)
        .build())
}

/// 白名单(逗号/空格/分号/换行分隔的用户 id);空 = 未配置。
fn allowed_users(ctx: &ChannelCtx) -> Vec<String> {
    ctx.setting("remote.weixin.allowed_users")
        .map(|s| {
            s.split([',', '，', ' ', ';', '；', '\n'])
                .map(|x| x.trim().to_string())
                .filter(|x| !x.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accounts_migrate_append_unbind_and_route() {
        let dir = std::env::temp_dir().join(format!("lw-wx-acct-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _ = std::fs::remove_file(dir.join("t.db"));
        let s = crate::store::Store::open(&dir.join("t.db")).unwrap();
        let st = &s.settings;

        // 空世界:没绑定,出站明白话
        assert!(load_accounts(st).is_empty());
        assert!(account_for(st, "u1").is_err());

        // 旧单 token(≤v0.2.15)→ 惰性迁移成无身份账号;唯一账号兜底路由
        crate::secrets::set(st, "remote.weixin.token", "old-tok").unwrap();
        st.set(None, "remote.weixin.base_url", "https://idc9.example").unwrap();
        let list = load_accounts(st);
        assert_eq!(list.len(), 1);
        assert!(list[0].user_id.is_empty());
        assert_eq!(list[0].base(), "https://idc9.example");
        assert_eq!(account_for(st, "whoever").unwrap().token, "old-tok");

        // 扫码(有身份)→ 顶替无身份的迁移账号(同一台机器上「不知道是谁」的旧绑定)
        append_account(
            st,
            Account { token: "t1".into(), base_url: String::new(), user_id: "u1".into() },
        )
        .unwrap();
        let list = load_accounts(st);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].user_id, "u1");

        // 家里第二个人扫码 → 追加并存、互不顶替(2026-07-11 真机实锤要治的)
        append_account(
            st,
            Account {
                token: "t2".into(),
                base_url: "https://idc2.example".into(),
                user_id: "u2".into(),
            },
        )
        .unwrap();
        assert_eq!(load_accounts(st).len(), 2);
        // 出站按线程 ext_id 精确路由到各自账号;多账号下不认识的不猜
        assert_eq!(account_for(st, "u1").unwrap().token, "t1");
        assert_eq!(account_for(st, "u2").unwrap().token, "t2");
        assert!(account_for(st, "u3").is_err());

        // 同人重扫 = 替换 token 不加行
        append_account(
            st,
            Account { token: "t1b".into(), base_url: String::new(), user_id: "u1".into() },
        )
        .unwrap();
        assert_eq!(load_accounts(st).len(), 2);
        assert_eq!(account_for(st, "u1").unwrap().token, "t1b");

        // 解绑一个,另一个还在;重复解绑如实报
        unbind_account(st, "u1").unwrap();
        let list = load_accounts(st);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].user_id, "u2");
        assert!(unbind_account(st, "u1").is_err());
    }

    #[test]
    fn parse_message_text_only() {
        let m = json!({
            "message_type": 1,
            "from_user_id": "u_abc",
            "context_token": "ctx1",
            "item_list": [ { "type": 1, "text_item": { "text": "  在吗  " } } ]
        });
        let p = parse_message(&m).unwrap();
        assert_eq!(p.from_user_id, "u_abc");
        assert_eq!(p.context_token, "ctx1");
        assert_eq!(p.text, "在吗");
        assert!(p.media.is_none());
    }

    #[test]
    fn parse_message_skips_bot_echo_and_empty_sender() {
        // BOT 回声(message_type=2)不处理
        let bot = json!({ "message_type": 2, "from_user_id": "u1", "item_list": [] });
        assert_eq!(parse_message(&bot), None);
        // 无 from_user_id → None
        let bare = json!({ "message_type": 1, "item_list": [] });
        assert_eq!(parse_message(&bare), None);
    }

    #[test]
    fn parse_message_voice_uses_server_text() {
        let m = json!({
            "message_type": 1,
            "from_user_id": "u2",
            "item_list": [ { "type": 3, "voice_item": { "text": "帮我记一下", "encode_type": 6 } } ]
        });
        let p = parse_message(&m).unwrap();
        assert_eq!(p.text, "帮我记一下");
        assert_eq!(p.media.as_ref().unwrap().kind, MediaKind::Voice);
    }

    #[test]
    fn parse_message_image_full_url_and_aeskey() {
        // image_item.aeskey(hex)优先转 base64(raw16)
        let raw16 = [0x11u8; 16];
        let hex_key = hex::encode(raw16);
        let m = json!({
            "message_type": 1,
            "from_user_id": "u3",
            "item_list": [ {
                "type": 2,
                "image_item": {
                    "aeskey": hex_key,
                    "media": { "full_url": "https://cdn/x", "aes_key": "ignored" }
                }
            } ]
        });
        let p = parse_message(&m).unwrap();
        let media = p.media.unwrap();
        assert_eq!(media.kind, MediaKind::Image);
        assert_eq!(media.full_url.as_deref(), Some("https://cdn/x"));
        // aes_key 应是 base64(raw16),parse_aes_key 能还原成同一个 raw16
        assert_eq!(parse_aes_key(media.aes_key.as_ref().unwrap()).unwrap(), raw16);
    }

    #[test]
    fn parse_message_flags_unknown_content_unsupported() {
        // 未知内容项(如表情 type=99)→ unsupported=true(该回「看不了」§3.5)
        let m = json!({
            "message_type": 1,
            "from_user_id": "u9",
            "item_list": [ { "type": 99, "sticker_item": {} } ]
        });
        let p = parse_message(&m).unwrap();
        assert!(p.unsupported);
        assert!(p.text.is_empty() && p.media.is_none());
        // 纯文字消息不算 unsupported
        let t = json!({
            "message_type": 1, "from_user_id": "u9",
            "item_list": [ { "type": 1, "text_item": { "text": "hi" } } ]
        });
        assert!(!parse_message(&t).unwrap().unsupported);
    }

    #[test]
    fn parse_message_file_name_and_mime() {
        let m = json!({
            "message_type": 1,
            "from_user_id": "u4",
            "item_list": [ {
                "type": 4,
                "file_item": { "file_name": "报告.pdf", "media": { "full_url": "https://cdn/f" } }
            } ]
        });
        let p = parse_message(&m).unwrap();
        let media = p.media.unwrap();
        assert_eq!(media.kind, MediaKind::File);
        assert_eq!(media.name, "报告.pdf");
    }

    #[test]
    fn aes_ecb_roundtrip_and_pkcs7() {
        let key = [0x42u8; 16];
        // 各种长度(含恰好整块 → PKCS7 必 +16)
        for len in [0usize, 1, 15, 16, 17, 31, 100] {
            let plain: Vec<u8> = (0..len).map(|i| (i * 7) as u8).collect();
            let ct = aes_ecb_encrypt(&plain, &key);
            assert_eq!(ct.len(), aes_ecb_padded_size(len), "密文尺寸 = padded_size(len={len})");
            assert_eq!(ct.len() % 16, 0);
            let pt = aes_ecb_decrypt(&ct, &key).unwrap();
            assert_eq!(pt, plain, "往返一致 len={len}");
        }
    }

    #[test]
    fn parse_aes_key_both_encodings() {
        let raw16 = [0xABu8; 16];
        // base64(raw16)
        let b64_raw = base64::engine::general_purpose::STANDARD.encode(raw16);
        assert_eq!(parse_aes_key(&b64_raw).unwrap(), raw16);
        // base64(hex32-ascii)
        let hex32 = hex::encode(raw16); // 32 ascii chars
        let b64_hex = base64::engine::general_purpose::STANDARD.encode(hex32.as_bytes());
        assert_eq!(parse_aes_key(&b64_hex).unwrap(), raw16);
        // 非法长度
        assert!(parse_aes_key(&base64::engine::general_purpose::STANDARD.encode([0u8; 20])).is_err());
    }

    #[test]
    fn media_type_by_ext() {
        assert_eq!(media_type_by_name("a.PNG"), MEDIA_IMAGE);
        assert_eq!(media_type_by_name("b.mp4"), MEDIA_VIDEO);
        assert_eq!(media_type_by_name("c.pdf"), MEDIA_FILE);
        assert_eq!(media_type_by_name("noext"), MEDIA_FILE);
    }

    #[test]
    fn qr_svg_renders() {
        let svg = render_qr_svg("https://liteapp.weixin.qq.com/q/abc?qrcode=1").unwrap();
        assert!(svg.contains("<svg"), "产出 SVG");
    }

    #[test]
    fn uin_and_client_id_shape() {
        let uin = random_wechat_uin();
        assert!(base64::engine::general_purpose::STANDARD.decode(&uin).is_ok(), "UIN 是 base64");
        assert!(random_client_id().starts_with("larkwing-"));
    }
}
