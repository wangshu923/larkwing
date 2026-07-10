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

/// 解析好的发送目标(凭证 + 收件地址),一次解析、逐文件复用。
pub(crate) enum Target {
    Telegram { token: String, chat_id: String },
    Dingtalk { app_key: String, app_secret: String, staff_id: String },
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
        }
    }

    fn max_bytes(&self) -> u64 {
        match self {
            Target::Telegram { .. } => TG_FILE_MAX,
            Target::Dingtalk { .. } => DT_FILE_MAX,
        }
    }
}

/// 「这个人的手机」在哪:绑定线程 + 已配凭证 → 发送目标。找不到给明白话(模型如实转告,
/// §3.5 不含糊)。钉钉群聊线程(无 push_id)发不了,跳过继续找别的。
pub(crate) fn resolve_target(store: &Store, user_id: i64) -> Result<Target> {
    let owner_id = store.users.ensure_default_user().map(|u| u.id).unwrap_or(1);
    let threads = store.channels.list().unwrap_or_default();
    let secret = |key: &str| {
        crate::secrets::get(&store.settings, key)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let mut saw_mine = false;
    // 新绑定优先(id 大 = 后建 = 更可能是现在在用的那台手机)
    for t in threads.into_iter().rev() {
        let mine = t.user_id == Some(user_id) || (user_id == owner_id && t.user_id.is_none());
        if !mine {
            continue;
        }
        saw_mine = true;
        match t.channel.as_str() {
            "telegram" => {
                if let Some(token) = secret("remote.telegram.token") {
                    return Ok(Target::Telegram { token, chat_id: t.ext_id });
                }
            }
            "dingtalk" => {
                let Some(staff_id) = t.push_id.clone() else { continue }; // 群聊推不了
                if let (Some(app_key), Some(app_secret)) =
                    (secret("remote.dingtalk.app_key"), secret("remote.dingtalk.app_secret"))
                {
                    return Ok(Target::Dingtalk { app_key, app_secret, staff_id });
                }
            }
            _ => continue,
        }
    }
    if saw_mine {
        bail!("手机渠道没法发文件(凭证不全,或钉钉那头是群聊)——让用户在设置·远程里检查")
    }
    bail!("这个人还没连上手机(没有绑定的 Telegram/钉钉对话)——先在手机上跟我说句话")
}

/// 发一个文件。体积按渠道上限先检;caption 只 TG 支持(钉钉忽略)。
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
            dt_send_file(net, app_key, app_secret, staff_id, &name, bytes).await
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
        let err = resolve_target(&s, owner.id).unwrap_err();
        assert!(err.to_string().contains("还没连上手机"), "{err:#}");

        // 主人的 TG 线程(未指认 NULL = 会话归属者)+ 凭证在 → 命中 Telegram
        let conv = s.chat.create_conversation_full(owner.id, "companion", "telegram").unwrap();
        s.channels.bind("telegram", "8877", conv.id).unwrap();
        crate::secrets::set(&s.settings, "remote.telegram.token", "tok123").unwrap();
        match resolve_target(&s, owner.id).unwrap() {
            Target::Telegram { token, chat_id } => {
                assert_eq!(token, "tok123");
                assert_eq!(chat_id, "8877");
            }
            _ => panic!("应命中 Telegram"),
        }

        // 家人(指认)只算指认给 TA 的线程;TA 没有 → 明白话
        let kid = s.users.create("小朋友").unwrap();
        let err = resolve_target(&s, kid.id).unwrap_err();
        assert!(err.to_string().contains("还没连上手机"), "{err:#}");
        let t = s.channels.thread_for("telegram", "8877").unwrap().unwrap();
        s.channels.bind_user(t.id, Some(kid.id)).unwrap();
        assert!(matches!(resolve_target(&s, kid.id).unwrap(), Target::Telegram { .. }));

        // 指认走了之后,主人自己反而没有线程了(NULL 的没了)→ 明白话
        let err = resolve_target(&s, owner.id).unwrap_err();
        assert!(err.to_string().contains("还没连上手机"), "{err:#}");
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
        let err = resolve_target(&s, owner.id).unwrap_err();
        assert!(err.to_string().contains("没法发文件"), "{err:#}");
        // 补上单聊 push_id → 命中钉钉
        s.channels.set_push_id("dingtalk", "cidGROUP", "staff01").unwrap();
        match resolve_target(&s, owner.id).unwrap() {
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
