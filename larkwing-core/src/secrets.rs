//! 密钥落地:系统密钥串(`keyring`:mac Keychain / Win 凭据管理器 / Linux keyutils),
//! **不进 SQLite 明文**(§6.3「Windows 发布前换 keyring」的兑现)。
//!
//! - keyring 不可用(headless / dev / 无后端)→ **回落 `settings` 表并 warn**,绝不让 app 哑掉
//!   (容错铁律:宁可降级也不崩;开发机/CI 无真密钥,降级无害)。
//! - 存的是**用户原文**(literal 或 `${ENV}` 引用);`${ENV}` 解析在 engine 取值时跑(`resolve_env`),
//!   不在这一层 —— 所以引用类「秘密」也照常进 keyring,读出来再解析,语义不变。
//! - `is_secret` 圈定哪些 settings key 是秘密;非秘密(开关 / 公钥 / 白名单)照旧走 settings。

use crate::store::SettingsRepo;

#[cfg(target_os = "windows")] // 只有 Windows 分支用它建 keyring entry;mac/Linux 回落 settings
const SERVICE: &str = "larkwing";

/// 秘密类 settings key —— 迁 keyring、不落 SQLite 明文。
/// 注:Ed25519 **公钥**不在内(非秘密、要给用户复制);只有**私钥**在内。
pub const SECRET_KEYS: &[&str] = &[
    "llm.api_key",
    "llm.providers", // 整张 JSON 含各 provider 的 key → 整块进 keyring
    "crypto.ed25519.private_key",
    "remote.telegram.token",
    "remote.dingtalk.app_key",
    "remote.dingtalk.app_secret",
    "remote.weixin.token",    // 旧单绑定(≤v0.2.15);留读做迁移,不再写
    "remote.weixin.accounts", // 多绑定列表(JSON 数组,含各账号 token → 整块进 keyring)
];

pub fn is_secret(key: &str) -> bool {
    SECRET_KEYS.contains(&key)
}

/// keyring 实体 —— **仅 Windows 启用**(目标平台,凭据管理器随登录会话解锁、不弹每次访问框)。
/// **mac / Linux 开发机回落 `settings`**(返回 None):mac Keychain 对未签名 / 每次重编的 dev 二进制
/// 会弹「允许访问钥匙串」,太烦(2026-06-17 用户拍板去掉);Mac 是开发机,settings 明文可接受(§4.9)。
fn entry(name: &str) -> Option<keyring::Entry> {
    #[cfg(not(target_os = "windows"))]
    {
        let _ = name; // mac/Linux:不碰系统密钥串,一律回落 settings
        None
    }
    #[cfg(target_os = "windows")]
    {
        match keyring::Entry::new(SERVICE, name) {
            Ok(e) => Some(e),
            Err(e) => {
                tracing::warn!(name, err = %e, "keyring entry 建失败,回落 settings");
                None
            }
        }
    }
}

/// 读秘密:keyring 优先;无条目 / 不可用 → 回落 settings(迁移前的 legacy 明文,或降级态)。
pub fn get(settings: &SettingsRepo, name: &str) -> Option<String> {
    if let Some(e) = entry(name) {
        match e.get_password() {
            Ok(v) => return Some(v),
            Err(keyring::Error::NoEntry) => {} // 回落 settings(可能是迁移前 legacy)
            Err(err) => tracing::warn!(name, err = %err, "keyring 读失败,回落 settings"),
        }
    }
    settings.get(None, name).ok().flatten()
}

/// 写秘密:写 keyring 成功 → 清掉 settings 里的明文残留;失败 → 回落写 settings(降级,至少不丢)。
pub fn set(settings: &SettingsRepo, name: &str, value: &str) -> anyhow::Result<()> {
    if let Some(e) = entry(name) {
        match e.set_password(value) {
            Ok(()) => {
                let _ = settings.delete(None, name); // 清明文残留(迁移/覆盖)
                return Ok(());
            }
            Err(err) => tracing::warn!(name, err = %err, "keyring 写失败,回落 settings"),
        }
    }
    settings.set(None, name, value)
}

/// 删秘密:keyring + settings 两处都清(幂等)。
pub fn delete(settings: &SettingsRepo, name: &str) {
    if let Some(e) = entry(name) {
        match e.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => {}
            Err(err) => tracing::warn!(name, err = %err, "keyring 删失败"),
        }
    }
    let _ = settings.delete(None, name);
}

/// 开机一次性迁移:settings 表里残留的明文秘密 → keyring(写成功即清明文)。幂等可反复调。
/// keyring 不可用时 `set` 回落写回 settings(等于原地不动),不丢密钥。
pub fn migrate(settings: &SettingsRepo) {
    for &k in SECRET_KEYS {
        match settings.get(None, k) {
            Ok(Some(v)) if !v.is_empty() => {
                if let Err(e) = set(settings, k, &v) {
                    tracing::warn!(key = k, err = %format!("{e:#}"), "密钥迁移 keyring 失败(留 settings)");
                }
            }
            _ => {}
        }
    }
}
