//! 密钥落地:系统密钥串(`keyring`:mac Keychain / Win 凭据管理器 / Linux keyutils),
//! **不进 SQLite 明文**(§6.3「Windows 发布前换 keyring」的兑现)。
//!
//! - keyring 不可用(headless / dev / 无后端)→ **回落 `settings` 表并 warn**,绝不让 app 哑掉
//!   (容错铁律:宁可降级也不崩;开发机/CI 无真密钥,降级无害)。
//! - 存的是**用户原文**(literal 或 `${ENV}` 引用);`${ENV}` 解析在 engine 取值时跑(`resolve_env`),
//!   不在这一层 —— 所以引用类「秘密」也照常进 keyring,读出来再解析,语义不变。
//! - `is_secret` 圈定哪些 settings key 是秘密;非秘密(开关 / 公钥 / 白名单)照旧走 settings。

use crate::store::SettingsRepo;

const SERVICE: &str = "larkwing";

/// Windows 凭据管理器单条上限 = `CRED_MAX_CREDENTIAL_BLOB_SIZE` 2560 字节,keyring 按 UTF-16 存
/// → 1280 个 UTF-16 单元(keyring-3 `windows.rs` 的校验)。这是平台事实,不是产品默认。
/// 分片取 1200 留余量。
const CHUNK_UNITS: usize = 1200;
/// 分片数的界(~1.4MB):秘密不该有这么大,超了如实报错,不无限拆。
const MAX_CHUNKS: usize = 1000;
/// 分片头:主条目存它 + 代次 + 片数,真值在 `name#代次.序号`。首字符是控制符,
/// 用户的密钥 / JSON 不会以它开头,与老的单条值天然分得开。
const CHUNK_MAGIC: &str = "\u{1}lw-chunks:";

/// 密钥串后端:按名字读 / 写 / 删一条。Windows = 凭据管理器;测试 = 内存表。
/// 抽成 trait 是为了让分片协议在 Mac 上也测得到(Mac 平时不启用 keyring)。
trait Vault {
    /// Ok(None) = 没这条。
    fn read(&self, name: &str) -> Result<Option<String>, String>;
    fn write(&self, name: &str, value: &str) -> Result<(), String>;
    /// 没这条也算成功。
    fn remove(&self, name: &str) -> Result<(), String>;
}

struct KeyringVault;

impl Vault for KeyringVault {
    fn read(&self, name: &str) -> Result<Option<String>, String> {
        let e = keyring::Entry::new(SERVICE, name).map_err(|e| e.to_string())?;
        match e.get_password() {
            Ok(v) => Ok(Some(v)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(err) => Err(err.to_string()),
        }
    }
    fn write(&self, name: &str, value: &str) -> Result<(), String> {
        let e = keyring::Entry::new(SERVICE, name).map_err(|e| e.to_string())?;
        e.set_password(value).map_err(|e| e.to_string())
    }
    fn remove(&self, name: &str) -> Result<(), String> {
        let e = keyring::Entry::new(SERVICE, name).map_err(|e| e.to_string())?;
        match e.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(err) => Err(err.to_string()),
        }
    }
}

fn chunk_name(name: &str, gen: u8, i: usize) -> String {
    format!("{name}#{gen}.{i}")
}

/// 主条目若是分片头 → (代次, 片数)。
fn parse_header(v: &str) -> Option<(u8, usize)> {
    let rest = v.strip_prefix(CHUNK_MAGIC)?;
    let (g, n) = rest.split_once(':')?;
    Some((g.parse().ok()?, n.parse().ok()?))
}

/// 按 UTF-16 单元数切(绝不劈开代理对:按 char 累计)。
fn split_utf16(s: &str, max_units: usize) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut start, mut units) = (0usize, 0usize);
    for (i, c) in s.char_indices() {
        let w = c.len_utf16();
        if units + w > max_units {
            out.push(&s[start..i]);
            start = i;
            units = 0;
        }
        units += w;
    }
    out.push(&s[start..]);
    out
}

fn vault_get(v: &impl Vault, name: &str) -> Result<Option<String>, String> {
    let Some(main) = v.read(name)? else { return Ok(None) };
    let Some((gen, n)) = parse_header(&main) else { return Ok(Some(main)) };
    let mut out = String::new();
    for i in 1..=n {
        match v.read(&chunk_name(name, gen, i))? {
            Some(part) => out.push_str(&part),
            None => return Err(format!("分片缺了第 {i}/{n} 片")),
        }
    }
    Ok(Some(out))
}

/// 写入(2026-11-11 修):放得下就单条存;放不下就分片 —— **先写新一代的全部分片、最后翻主条目
/// (提交点)、再删上一代**,中途断电读到的要么是旧值要么是新值,不会拼出半新半旧。
/// 此前整张 `llm.providers` JSON 带缩进每家约 550 字符,恒含两个模板,配第三家就超 1280 →
/// keyring 写失败 → 回落 settings 明文(§6.3「不进 SQLite 明文」悄悄失效,备份包里带着全部钥匙)。
fn vault_set(v: &impl Vault, name: &str, value: &str) -> Result<(), String> {
    let old = v.read(name).ok().flatten().as_deref().and_then(parse_header);
    let parts = split_utf16(value, CHUNK_UNITS);
    if parts.len() == 1 {
        v.write(name, value)?;
    } else {
        if parts.len() > MAX_CHUNKS {
            return Err(format!("值太大({} 个 UTF-16 单元),拆成分片也存不下", value.encode_utf16().count()));
        }
        let gen = match old {
            Some((g, _)) => 1 - g.min(1),
            None => 0,
        };
        for (i, p) in parts.iter().enumerate() {
            v.write(&chunk_name(name, gen, i + 1), p)?;
        }
        v.write(name, &format!("{CHUNK_MAGIC}{gen}:{}", parts.len()))?;
    }
    // 上一代分片收掉(新一代恒换代次,不会删到刚写的;删不掉的只是孤儿条目,读路径看主条目头,
    // 不会读到它们)
    if let Some((g, n)) = old {
        for i in 1..=n {
            if let Err(e) = v.remove(&chunk_name(name, g, i)) {
                tracing::warn!(name, err = %e, "keyring 旧分片删不掉(留个孤儿,不影响读)");
            }
        }
    }
    Ok(())
}

fn vault_delete(v: &impl Vault, name: &str) -> Result<(), String> {
    if let Some((g, n)) = v.read(name).ok().flatten().as_deref().and_then(parse_header) {
        for i in 1..=n {
            v.remove(&chunk_name(name, g, i))?;
        }
    }
    v.remove(name)
}

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
    // WebDAV / 需要认证的直链的账号密码(JSON 数组,整块进 keyring)。**刻意不做成工具参数**
    // ——密码走参数就会进 LLM 上下文 + 落 messages.payload + 每轮回放给供应商(§7.7 凭证不过桥)。
    "net.http_creds",
];

pub fn is_secret(key: &str) -> bool {
    SECRET_KEYS.contains(&key)
}

/// keyring 实体 —— **仅 Windows 启用**(目标平台,凭据管理器随登录会话解锁、不弹每次访问框)。
/// **mac / Linux 开发机回落 `settings`**(返回 None):mac Keychain 对未签名 / 每次重编的 dev 二进制
/// 会弹「允许访问钥匙串」,太烦(2026-06-17 用户拍板去掉);Mac 是开发机,settings 明文可接受(§4.9)。
fn vault() -> Option<KeyringVault> {
    if cfg!(target_os = "windows") {
        Some(KeyringVault)
    } else {
        None // mac/Linux:不碰系统密钥串,一律回落 settings
    }
}

/// 读秘密:keyring 优先;无条目 / 不可用 → 回落 settings(迁移前的 legacy 明文,或降级态)。
pub fn get(settings: &SettingsRepo, name: &str) -> Option<String> {
    if let Some(v) = vault() {
        match vault_get(&v, name) {
            Ok(Some(s)) => return Some(s),
            Ok(None) => {} // 回落 settings(可能是迁移前 legacy)
            Err(err) => tracing::warn!(name, err = %err, "keyring 读失败,回落 settings"),
        }
    }
    settings.get(None, name).ok().flatten()
}

/// 写秘密:写 keyring 成功 → 清掉 settings 里的明文残留;失败 → 回落写 settings(降级,至少不丢)。
/// 超过单条上限的值自动分片(`vault_set`),不再因为长而落明文。
pub fn set(settings: &SettingsRepo, name: &str, value: &str) -> anyhow::Result<()> {
    if let Some(v) = vault() {
        match vault_set(&v, name, value) {
            Ok(()) => {
                let _ = settings.delete(None, name); // 清明文残留(迁移/覆盖)
                return Ok(());
            }
            Err(err) => {
                tracing::warn!(name, err = %err, "keyring 写失败,回落 settings");
                // ⚠️ **回落前必须把 keyring 里的旧条目清掉(2026-08-22 修)**:`get()` 是
                // keyring 优先 —— 留着旧条目 = 新值写进了 settings、读出来的却还是旧值。
                // 用户改完密钥看到「保存好了」,实际用的还是老密钥(**静默回滚**),而新密钥
                // 的明文已经落进 DB 了,两头都坏。
                match vault_delete(&v, name) {
                    Ok(()) => {}
                    Err(derr) => {
                        // 旧条目既写不动也删不掉:写进 settings 也会被它盖住(等于没保存),
                        // 还白留一份明文 → 如实报错,别假装成功(§3.5)。
                        anyhow::bail!(
                            "没能保存到系统密钥串({err}),旧的那条也删不掉({derr})——\
                             硬写进数据库会被旧值盖掉、还会留下明文,所以这次没有保存"
                        );
                    }
                }
            }
        }
    }
    settings.set(None, name, value)
}

/// 删秘密:keyring + settings 两处都清(幂等)。
pub fn delete(settings: &SettingsRepo, name: &str) {
    if let Some(v) = vault() {
        if let Err(err) = vault_delete(&v, name) {
            tracing::warn!(name, err = %err, "keyring 删失败");
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    /// 内存版密钥串:与 Windows 凭据管理器同一条长度闸(1280 个 UTF-16 单元)。
    #[derive(Default)]
    struct MemVault(RefCell<HashMap<String, String>>);

    impl Vault for MemVault {
        fn read(&self, name: &str) -> Result<Option<String>, String> {
            Ok(self.0.borrow().get(name).cloned())
        }
        fn write(&self, name: &str, value: &str) -> Result<(), String> {
            if value.encode_utf16().count() > 1280 {
                return Err("TooLong".into());
            }
            self.0.borrow_mut().insert(name.into(), value.into());
            Ok(())
        }
        fn remove(&self, name: &str) -> Result<(), String> {
            self.0.borrow_mut().remove(name);
            Ok(())
        }
    }

    #[test]
    fn long_values_are_chunked_and_read_back_whole() {
        let v = MemVault::default();
        // 三家供应商量级(~1700 个 UTF-16 单元)+ 中文 + 代理对(emoji 占 2 个 UTF-16 单元)
        let big: String = (0..1500).map(|i| if i % 7 == 0 { '😀' } else { '钥' }).collect();
        assert!(big.encode_utf16().count() > 1280, "夹具要真的超过单条上限");
        vault_set(&v, "llm.providers", &big).unwrap();
        assert_eq!(vault_get(&v, "llm.providers").unwrap().as_deref(), Some(big.as_str()));
        // 每一片都守得住上限,且没把代理对劈开(劈开的话 String 里根本存不进去)
        for (k, s) in v.0.borrow().iter() {
            assert!(s.encode_utf16().count() <= 1280, "{k} 超限");
        }
    }

    #[test]
    fn rewrite_switches_generation_and_cleans_old_chunks() {
        let v = MemVault::default();
        let a: String = "a".repeat(3000);
        let b: String = "b".repeat(1500);
        vault_set(&v, "k", &a).unwrap(); // 3 片,代次 0
        vault_set(&v, "k", &b).unwrap(); // 2 片,代次 1
        assert_eq!(vault_get(&v, "k").unwrap().as_deref(), Some(b.as_str()));
        let names: Vec<String> = v.0.borrow().keys().cloned().collect();
        assert!(!names.iter().any(|n| n.starts_with("k#0.")), "上一代分片要收掉:{names:?}");
        // 缩回放得下的短值:分片全清,只剩主条目
        vault_set(&v, "k", "short").unwrap();
        assert_eq!(vault_get(&v, "k").unwrap().as_deref(), Some("short"));
        assert_eq!(v.0.borrow().len(), 1, "只剩主条目");
        // 删除连分片一起删
        vault_set(&v, "k", &a).unwrap();
        vault_delete(&v, "k").unwrap();
        assert!(v.0.borrow().is_empty());
    }

    #[test]
    fn legacy_plain_value_and_missing_chunk() {
        let v = MemVault::default();
        v.write("old", "sk-legacy").unwrap(); // 分片之前写进去的老值照读
        assert_eq!(vault_get(&v, "old").unwrap().as_deref(), Some("sk-legacy"));
        vault_set(&v, "k", &"x".repeat(2500)).unwrap();
        v.remove("k#0.2").unwrap();
        assert!(vault_get(&v, "k").is_err(), "缺片如实报错,不拼半截值");
    }

    #[test]
    fn split_never_breaks_surrogate_pairs() {
        let s = "😀".repeat(5); // 每个 2 单元
        let parts = split_utf16(&s, 3);
        assert!(parts.iter().all(|p| p.encode_utf16().count() <= 3));
        assert_eq!(parts.concat(), s);
    }
}
