//! 渠道出站文件(send_file 工具的机器件):把本机文件发到某个人的手机渠道。
//! 与提醒推送(mod.rs outbound_loop)同族「出站」,但目标按**人**解析——渠道归人的
//! 映射反着用:指认给 TA 的线程算 TA 的;TA 是主人时,未指认(NULL = 会话归属者)的
//! 线程也算 TA 的。多条命中取最新绑定(id 大)。
//! TG = sendDocument(multipart,≤50MB);钉钉 = 旧接口 media/upload 换 mediaId(只认
//! gettoken 的旧 token)+ 新 token batchSend sampleFile/sampleImageMsg —— 新旧混用是
//! 官方文档姿势,真钉待验(PLAN watch-item)。凭证走 secrets(§6.3),不读明文。

use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;

use crate::net;
use crate::store::Store;

/// 每渠道体积上限(超限如实退回,绝不静默截断/压缩)。
const TG_FILE_MAX: u64 = 50 * 1024 * 1024;
const DT_FILE_MAX: u64 = 20 * 1024 * 1024;
const WX_FILE_MAX: u64 = 50 * 1024 * 1024;

/// 解析好的发送目标(凭证 + 收件地址),一次解析、逐文件复用。
pub(crate) enum Target {
    Telegram { token: String, chat_id: String },
    Dingtalk { app_key: String, app_secret: String, staff_id: String },
    /// 微信 iLink bot:发媒体要回显上次 context_token(存 push_id),到 to_user_id(= ext_id)。
    Weixin { token: String, base_url: String, to_user_id: String, context_token: String },
}

/// Debug 只露渠道名 —— 变体里揣着 token/secret,绝不随 unwrap/日志外泄。
impl std::fmt::Debug for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Target::{}", self.channel_name())
    }
}

impl Target {
    /// 给回话用的渠道名(经模型转述,不是 UI 静态文案)。
    pub(crate) fn channel_name(&self) -> &'static str {
        match self {
            Target::Telegram { .. } => "Telegram",
            Target::Dingtalk { .. } => "钉钉",
            Target::Weixin { .. } => "微信",
        }
    }

    fn max_bytes(&self) -> u64 {
        match self {
            Target::Telegram { .. } => TG_FILE_MAX,
            Target::Dingtalk { .. } => DT_FILE_MAX,
            Target::Weixin { .. } => WX_FILE_MAX,
        }
    }
}

/// 按名字找家人(跨人投递的收件人解析:send_file 的 to / reminder_set 的 for 共用)。
/// 名字 = 家人页里的称呼(用户数据,非 i18n);找不到 / 重名都给明白话(§3.5),
/// 错误里带现有名单,让模型自己纠正或如实转告「先去设置·家人里加」。
pub(crate) fn find_member(store: &Store, name: &str) -> Result<crate::store::User> {
    let name = name.trim();
    anyhow::ensure!(!name.is_empty(), "家人名字是空的");
    let users = store.users.list().unwrap_or_default();
    // 精确优先;没有再放宽到「家人名包含所填」——家人页的名字常带注释(「蛋蛋(就是妈妈)」),
    // 模型照口头称呼填「妈妈/蛋蛋」也该路由得到(2026-07-11 真机实锤:精确匹配把三次重试全拒)。
    // 唯一命中才取;多命中如实报名单,绝不猜。
    let mut hits: Vec<&crate::store::User> = users.iter().filter(|u| u.name == name).collect();
    if hits.is_empty() {
        hits = users.iter().filter(|u| u.name.contains(name)).collect();
    }
    if hits.len() > 1 {
        let names = hits.iter().map(|u| u.name.as_str()).collect::<Vec<_>>().join("、");
        bail!("「{name}」对得上 {} 个家人({names})——换个更具体的叫法", hits.len());
    }
    match hits.pop() {
        Some(u) => Ok(u.clone()),
        None => {
            let known = users.iter().map(|u| u.name.as_str()).collect::<Vec<_>>().join("、");
            bail!("家里没有叫「{name}」的人(现在有:{known})——名字要跟设置·家人里的一致")
        }
    }
}

/// 「这个人的手机」在哪:绑定线程 + 已配凭证 → 发送目标。找不到给明白话(模型如实转告,
/// §3.5 不含糊)。钉钉群聊线程(无 push_id)发不了,跳过继续找别的。
pub(crate) fn resolve_target(store: &Store, user_id: i64, channel: Option<&str>) -> Result<Target> {
    resolve_phone(store, user_id, channel).map(|(_, t)| t)
}

/// 同 `resolve_target`,但连命中的映射线程一起给(跨人提醒要线程的 conv_id:
/// 到点回合落在 TA 的手机对话里,推送链才接得上)。
/// `channel` = 只考虑这个渠道(用户点名「发我微信」;2026-07-11 真机实锤:一人多渠道时
/// 默认取最新绑定,点名渠道没有入口 → 文件被发去钉钉、微信要不到)。None = 不限渠道。
pub(crate) fn resolve_phone(
    store: &Store,
    user_id: i64,
    channel: Option<&str>,
) -> Result<(crate::store::ChannelThread, Target)> {
    let owner_id = store.users.ensure_default_user().map(|u| u.id).unwrap_or(1);
    let threads = store.channels.list().unwrap_or_default();
    let secret = |key: &str| {
        crate::secrets::get(&store.settings, key)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let setting = |key: &str| {
        store.settings.get(None, key).ok().flatten().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
    };
    let mut saw_mine = false;
    // 新绑定优先(id 大 = 后建 = 更可能是现在在用的那台手机)
    for t in threads.into_iter().rev() {
        let mine = t.user_id == Some(user_id) || (user_id == owner_id && t.user_id.is_none());
        if !mine {
            continue;
        }
        if let Some(ch) = channel {
            if t.channel != ch {
                continue;
            }
        }
        saw_mine = true;
        match t.channel.as_str() {
            "telegram" => {
                if let Some(token) = secret("remote.telegram.token") {
                    let chat_id = t.ext_id.clone();
                    return Ok((t, Target::Telegram { token, chat_id }));
                }
            }
            "dingtalk" => {
                let Some(staff_id) = t.push_id.clone() else { continue }; // 群聊推不了
                if let (Some(app_key), Some(app_secret)) =
                    (secret("remote.dingtalk.app_key"), secret("remote.dingtalk.app_secret"))
                {
                    return Ok((t, Target::Dingtalk { app_key, app_secret, staff_id }));
                }
            }
            "weixin" => {
                // 发媒体要回显 context_token(存 push_id);没有 = 用户登录后还没说过话,推不了
                let Some(context_token) = t.push_id.clone() else { continue };
                if let Some(token) = secret("remote.weixin.token") {
                    let base_url = setting("remote.weixin.base_url")
                        .unwrap_or_else(|| super::weixin::DEFAULT_BASE_URL.to_string());
                    let to_user_id = t.ext_id.clone();
                    return Ok((t, Target::Weixin { token, base_url, to_user_id, context_token }));
                }
            }
            _ => continue,
        }
    }
    if let Some(ch) = channel {
        let ch_name = match ch {
            "telegram" => "Telegram",
            "dingtalk" => "钉钉",
            "weixin" => "微信",
            other => other,
        };
        if saw_mine {
            bail!(
                "{ch_name}那头现在收不了(凭证不全、是群聊,或微信刚绑定还没说过话)\
                 ——先在{ch_name}上给我发一句,或检查设置·远程"
            )
        }
        bail!("这个人没有绑定的{ch_name}对话——先在{ch_name}上跟我说句话")
    }
    if saw_mine {
        bail!(
            "手机渠道没法发文件(凭证不全、钉钉那头是群聊,或微信刚绑定还没说过话)\
             ——让用户在设置·远程里检查"
        )
    }
    bail!("这个人还没连上手机(没有绑定的 Telegram/钉钉/微信对话)——先在手机上跟我说句话")
}

/// 发一个文件。体积按渠道上限先检;caption:TG 随文件带,钉钉文件消息不支持附言 →
/// 文件送达后补推一条文字(跨人捎带说明才有着落);补推失败只 warn 不翻整单
/// (文件确实到了,翻错会让模型误以为要重发文件)。
pub(crate) async fn send_file(
    net: &net::Client,
    target: &Target,
    path: &Path,
    caption: Option<&str>,
) -> Result<()> {
    let meta =
        std::fs::metadata(path).with_context(|| format!("读不到文件 {}", path.display()))?;
    anyhow::ensure!(meta.is_file(), "{} 不是文件", path.display());
    anyhow::ensure!(
        meta.len() <= target.max_bytes(),
        "文件超过{}的 {}MB 上限({}MB)",
        target.channel_name(),
        target.max_bytes() >> 20,
        meta.len() >> 20
    );
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| anyhow!("路径没有文件名: {}", path.display()))?;
    let bytes = tokio::fs::read(path).await.with_context(|| format!("读文件失败 {}", path.display()))?;
    match target {
        Target::Telegram { token, chat_id } => {
            tg_send_document(net, super::telegram::API, token, chat_id, &name, bytes, caption).await
        }
        Target::Dingtalk { app_key, app_secret, staff_id } => {
            dt_send_file(net, app_key, app_secret, staff_id, &name, bytes).await?;
            if let Some(cap) = caption.filter(|s| !s.is_empty()) {
                if let Err(e) = super::dingtalk::push(net, app_key, app_secret, staff_id, cap).await
                {
                    tracing::warn!(err = %format!("{e:#}"), "钉钉文件已送达,附言补推失败");
                }
            }
            Ok(())
        }
        Target::Weixin { token, base_url, to_user_id, context_token } => {
            // 微信上传 CDN(AES 加密)+ 发媒体项;caption 作单独文本项先发(都在 weixin::send_file 内)
            super::weixin::send_file(net, base_url, token, to_user_id, context_token, path, caption).await
        }
    }
}

/// TG sendDocument:一律按文档发(保真;图片文档 TG 也内联预览)。
/// multipart Form 不可克隆 → net.send 的每趟闭包里现造(bytes 克隆一份,≤50MB 可接受)。
/// api 参数化只为可测(测试打本地假端点,组包逻辑走真函数)。
async fn tg_send_document(
    net: &net::Client,
    api: &str,
    token: &str,
    chat_id: &str,
    name: &str,
    bytes: Vec<u8>,
    caption: Option<&str>,
) -> Result<()> {
    let url = format!("{api}/bot{token}/sendDocument");
    let resp = net
        .send(&url, |c| {
            let part = reqwest::multipart::Part::bytes(bytes.clone()).file_name(name.to_string());
            let mut form = reqwest::multipart::Form::new()
                .text("chat_id", chat_id.to_string())
                .part("document", part);
            if let Some(cap) = caption.filter(|s| !s.is_empty()) {
                form = form.text("caption", cap.to_string());
            }
            c.post(&url).multipart(form)
        })
        .await
        .context("sendDocument 请求失败")?;
    let status = resp.status();
    if !status.is_success() {
        let detail = resp.text().await.unwrap_or_default();
        bail!("sendDocument HTTP {status}: {detail}");
    }
    Ok(())
}

/// 钉钉发文件:media/upload(旧 token)→ batchSend(新 token)。图片走 sampleImageMsg
/// (钉钉 sampleFile 只认办公文档类扩展,图片按文件发会被拒),其余走 sampleFile。
async fn dt_send_file(
    net: &net::Client,
    app_key: &str,
    app_secret: &str,
    staff_id: &str,
    name: &str,
    bytes: Vec<u8>,
) -> Result<()> {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    let is_image = matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp");

    // 1) 上传媒体:media/upload 只认旧 gettoken 的 access_token(新旧 API 混用是官方姿势)
    let legacy = dt_legacy_token(net, app_key, app_secret).await?;
    let mtype = if is_image { "image" } else { "file" };
    let up_url = format!("https://oapi.dingtalk.com/media/upload?access_token={legacy}&type={mtype}");
    let resp = net
        .send(&up_url, |c| {
            let part = reqwest::multipart::Part::bytes(bytes.clone()).file_name(name.to_string());
            c.post(&up_url).multipart(reqwest::multipart::Form::new().part("media", part))
        })
        .await
        .context("钉钉 media/upload 请求失败")?;
    let v: Value = resp.json().await.context("钉钉 media/upload 响应非 JSON")?;
    let errcode = v.get("errcode").and_then(Value::as_i64).unwrap_or(-1);
    anyhow::ensure!(errcode == 0, "钉钉 media/upload 失败: {v}");
    let media_id = v
        .get("media_id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("钉钉 media/upload 无 media_id: {v}"))?
        .to_string();

    // 2) 单聊主动推送(与提醒推送共用 batchSend 出口)
    let token = super::dingtalk::access_token(net, app_key, app_secret).await?;
    let (msg_key, msg_param) = if is_image {
        ("sampleImageMsg", serde_json::json!({ "photoURL": media_id }).to_string())
    } else {
        (
            "sampleFile",
            serde_json::json!({ "mediaId": media_id, "fileName": name, "fileType": ext })
                .to_string(),
        )
    };
    super::dingtalk::batch_send(net, &token, app_key, staff_id, msg_key, &msg_param).await
}

/// 旧版 access token(oapi gettoken):media/upload 只认它;batchSend 认的新 token 在
/// dingtalk::access_token。两把并存是钉钉新旧 API 过渡期的现实。
async fn dt_legacy_token(net: &net::Client, app_key: &str, app_secret: &str) -> Result<String> {
    let url = format!(
        "https://oapi.dingtalk.com/gettoken?appkey={app_key}&appsecret={app_secret}"
    );
    let resp = net.send(&url, |c| c.get(&url)).await.context("钉钉 gettoken 请求失败")?;
    let v: Value = resp.json().await.context("钉钉 gettoken 响应非 JSON")?;
    let errcode = v.get("errcode").and_then(Value::as_i64).unwrap_or(-1);
    anyhow::ensure!(errcode == 0, "钉钉 gettoken 失败: {v}");
    v.get("access_token")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("钉钉 gettoken 无 access_token"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    fn store(tag: &str) -> Store {
        let dir = std::env::temp_dir().join(format!("lw-outbound-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _ = std::fs::remove_file(dir.join("t.db"));
        Store::open(&dir.join("t.db")).unwrap()
    }

    #[test]
    fn resolve_picks_own_thread_and_reports_missing_honestly() {
        let s = store("resolve");
        let owner = s.users.ensure_default_user().unwrap();

        // 没有任何线程 → 「还没连上手机」
        let err = resolve_target(&s, owner.id, None).unwrap_err();
        assert!(err.to_string().contains("还没连上手机"), "{err:#}");

        // 主人的 TG 线程(未指认 NULL = 会话归属者)+ 凭证在 → 命中 Telegram
        let conv = s.chat.create_conversation_full(owner.id, "companion", "telegram").unwrap();
        s.channels.bind("telegram", "8877", conv.id).unwrap();
        crate::secrets::set(&s.settings, "remote.telegram.token", "tok123").unwrap();
        match resolve_target(&s, owner.id, None).unwrap() {
            Target::Telegram { token, chat_id } => {
                assert_eq!(token, "tok123");
                assert_eq!(chat_id, "8877");
            }
            _ => panic!("应命中 Telegram"),
        }

        // 家人(指认)只算指认给 TA 的线程;TA 没有 → 明白话
        let kid = s.users.create("小朋友").unwrap();
        let err = resolve_target(&s, kid.id, None).unwrap_err();
        assert!(err.to_string().contains("还没连上手机"), "{err:#}");
        let t = s.channels.thread_for("telegram", "8877").unwrap().unwrap();
        s.channels.bind_user(t.id, Some(kid.id)).unwrap();
        assert!(matches!(resolve_target(&s, kid.id, None).unwrap(), Target::Telegram { .. }));

        // 指认走了之后,主人自己反而没有线程了(NULL 的没了)→ 明白话
        let err = resolve_target(&s, owner.id, None).unwrap_err();
        assert!(err.to_string().contains("还没连上手机"), "{err:#}");
    }

    #[test]
    fn find_member_by_name_honest_on_missing_and_dup() {
        let s = store("member");
        s.users.ensure_default_user().unwrap();
        let mom = s.users.create("妈妈").unwrap();

        assert_eq!(find_member(&s, " 妈妈 ").unwrap().id, mom.id, "名字去空白匹配");

        let err = find_member(&s, "二舅").unwrap_err().to_string();
        assert!(err.contains("没有叫") && err.contains("妈妈"), "给现有名单让模型纠正: {err}");

        s.users.create("妈妈").unwrap(); // 重名
        let err = find_member(&s, "妈妈").unwrap_err().to_string();
        assert!(err.contains("对得上"), "{err}");

        // resolve_phone 带回线程(跨人提醒要 conv_id)
        let conv = s.chat.create_conversation_full(mom.id, "companion", "telegram").unwrap();
        s.channels.bind("telegram", "777", conv.id).unwrap();
        let t = s.channels.thread_for("telegram", "777").unwrap().unwrap();
        s.channels.bind_user(t.id, Some(mom.id)).unwrap();
        crate::secrets::set(&s.settings, "remote.telegram.token", "tok").unwrap();
        let (thread, target) = resolve_phone(&s, mom.id, None).unwrap();
        assert_eq!(thread.conv_id, conv.id);
        assert!(matches!(target, Target::Telegram { .. }));
    }

    #[test]
    fn find_member_matches_containment_uniquely() {
        let s = store("member-loose");
        s.users.ensure_default_user().unwrap();
        let mom = s.users.create("蛋蛋(就是妈妈)").unwrap();
        // 口头称呼含于备注名 → 命中(真机实锤:名字带注释时「妈妈/蛋蛋」都该路由得到)
        assert_eq!(find_member(&s, "妈妈").unwrap().id, mom.id);
        assert_eq!(find_member(&s, "蛋蛋").unwrap().id, mom.id);
        // 多人都对得上 → 如实报名单,绝不猜
        s.users.create("蛋挞").unwrap();
        let err = find_member(&s, "蛋").unwrap_err().to_string();
        assert!(err.contains("对得上") && err.contains("蛋挞"), "{err}");
    }

    #[test]
    fn resolve_honors_channel_pick() {
        let s = store("chanpick");
        let owner = s.users.ensure_default_user().unwrap();
        let c1 = s.chat.create_conversation_full(owner.id, "companion", "telegram").unwrap();
        s.channels.bind("telegram", "101", c1.id).unwrap();
        crate::secrets::set(&s.settings, "remote.telegram.token", "tok").unwrap();
        let c2 = s.chat.create_conversation_full(owner.id, "companion", "weixin").unwrap();
        s.channels.bind("weixin", "wxid_1", c2.id).unwrap();
        s.channels.set_push_id("weixin", "wxid_1", "ctx-token-1").unwrap();
        crate::secrets::set(&s.settings, "remote.weixin.token", "wtok").unwrap();
        // 不点名 = 最新绑定(微信);点名 telegram → TG;点名没映射的渠道 → 明白话带渠道名
        assert!(matches!(resolve_target(&s, owner.id, None).unwrap(), Target::Weixin { .. }));
        assert!(matches!(
            resolve_target(&s, owner.id, Some("telegram")).unwrap(),
            Target::Telegram { .. }
        ));
        let err = resolve_target(&s, owner.id, Some("dingtalk")).unwrap_err().to_string();
        assert!(err.contains("钉钉"), "{err}");
    }

    #[test]
    fn resolve_skips_dingtalk_group_without_push_id() {
        let s = store("dtgroup");
        let owner = s.users.ensure_default_user().unwrap();
        let conv = s.chat.create_conversation_full(owner.id, "companion", "dingtalk").unwrap();
        s.channels.bind("dingtalk", "cidGROUP", conv.id).unwrap();
        crate::secrets::set(&s.settings, "remote.dingtalk.app_key", "k").unwrap();
        crate::secrets::set(&s.settings, "remote.dingtalk.app_secret", "s").unwrap();
        // 群聊线程没有 push_id → 发不了,但线程确实是 TA 的 → 「配置不全/群聊」话术
        let err = resolve_target(&s, owner.id, None).unwrap_err();
        assert!(err.to_string().contains("没法发文件"), "{err:#}");
        // 补上单聊 push_id → 命中钉钉
        s.channels.set_push_id("dingtalk", "cidGROUP", "staff01").unwrap();
        match resolve_target(&s, owner.id, None).unwrap() {
            Target::Dingtalk { staff_id, .. } => assert_eq!(staff_id, "staff01"),
            _ => panic!("应命中钉钉"),
        }
    }

    /// TG 发文件走本地假 API:multipart 真组包(裸 body 断言字段/文件名/字节;axum 不开
    /// multipart feature,不为测试加依赖面)。真 bot 归真机验。
    #[tokio::test]
    async fn tg_send_document_posts_multipart() {
        use axum::{routing::post, Router};
        use std::sync::atomic::{AtomicBool, Ordering};
        static SAW: AtomicBool = AtomicBool::new(false);

        async fn sink(body: axum::body::Bytes) -> &'static str {
            let raw = String::from_utf8_lossy(&body);
            if raw.contains("name=\"chat_id\"")
                && raw.contains("42")
                && raw.contains("filename=\"照片.png\"")
                && raw.contains("name=\"caption\"")
                && raw.contains("给你")
                && body.windows(3).any(|w| w == [1u8, 2, 3])
            {
                SAW.store(true, Ordering::Relaxed);
            }
            "{\"ok\":true}"
        }
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, Router::new().route("/{bot}/sendDocument", post(sink)))
                .await
                .ok();
        });

        let net = net::Client::new(|b| b);
        tg_send_document(
            &net,
            &format!("http://127.0.0.1:{port}"),
            "tok",
            "42",
            "照片.png",
            vec![1u8, 2, 3],
            Some("给你"),
        )
        .await
        .unwrap();
        assert!(SAW.load(Ordering::Relaxed), "假 TG 端点收到完整 multipart");
    }
}
