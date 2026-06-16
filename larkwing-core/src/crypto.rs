//! 全应用一对 Ed25519 身份密钥(**全局**,非某服务专属):私钥本地存、永不出门;公钥给用户
//! 复制到各服务控制台(和风 JWT 是首个消费者)。除非某服务要求别的算法,整个程序对外共用这一对。
//!
//! 这里只放**纯函数**(生成密钥对 + 签发 JWT);"确保密钥已存在并落库"的编排在 engine
//! (它持有 store)。我们只**产** token、从不验,所以 JWT 手搓(base64 + 签名),不引 jsonwebtoken。

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine;
use ed25519_dalek::pkcs8::spki::der::pem::LineEnding;
use ed25519_dalek::pkcs8::spki::EncodePublicKey;
use ed25519_dalek::pkcs8::{DecodePrivateKey, EncodePrivateKey};
use ed25519_dalek::{Signer, SigningKey};
use serde::Serialize;

/// settings key(app 级,全局):私钥 PEM(秘密,**永不过桥**)、公钥 PEM(非秘密,给用户复制)。
pub const KEY_ED25519_PRIVATE: &str = "crypto.ed25519.private_key";
pub const KEY_ED25519_PUBLIC: &str = "crypto.ed25519.public_key";

/// 生成一对 Ed25519 → (私钥 PKCS#8 PEM, 公钥 SPKI PEM)。公钥是 `-----BEGIN PUBLIC KEY-----`
/// 形态(同 `openssl pkey -pubout`),正是和风控制台粘贴所需。幂等由调用方"已存在就不重生"保证。
pub fn generate_keypair() -> Result<(String, String)> {
    let mut seed = [0u8; 32];
    getrandom::getrandom(&mut seed).map_err(|e| anyhow::anyhow!("取随机种子失败: {e}"))?;
    let sk = SigningKey::from_bytes(&seed);
    let private_pem = sk.to_pkcs8_pem(LineEnding::LF).context("导出私钥 PEM 失败")?.to_string();
    let public_pem =
        sk.verifying_key().to_public_key_pem(LineEnding::LF).context("导出公钥 PEM 失败")?;
    Ok((private_pem, public_pem))
}

#[derive(Serialize)]
struct JwtHeader<'a> {
    alg: &'a str,
    kid: &'a str,
}

#[derive(Serialize)]
struct JwtClaims<'a> {
    sub: &'a str,
    iat: u64,
    exp: u64,
}

/// 用全局私钥签一个 EdDSA JWT(和风认证格式):
/// `base64url(header{alg:EdDSA,kid}) . base64url(payload{sub,iat,exp}) . base64url(sig)`。
/// `iat` 提前 30s 容时钟漂移;`exp = now + ttl`(和风上限 24h)。
pub fn sign_jwt(private_pem: &str, kid: &str, sub: &str, ttl: Duration) -> Result<String> {
    let sk = SigningKey::from_pkcs8_pem(private_pem).context("私钥 PEM 解析失败")?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).context("系统时间早于纪元")?.as_secs();
    let header = JwtHeader { alg: "EdDSA", kid };
    let claims = JwtClaims { sub, iat: now.saturating_sub(30), exp: now + ttl.as_secs() };
    let signing_input =
        format!("{}.{}", B64.encode(serde_json::to_vec(&header)?), B64.encode(serde_json::to_vec(&claims)?));
    let sig = sk.sign(signing_input.as_bytes());
    Ok(format!("{signing_input}.{}", B64.encode(sig.to_bytes())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::pkcs8::spki::DecodePublicKey;
    use ed25519_dalek::{Verifier, VerifyingKey};
    use serde_json::Value;

    #[test]
    fn generated_keys_are_pem() {
        let (priv_pem, pub_pem) = generate_keypair().unwrap();
        assert!(priv_pem.contains("BEGIN PRIVATE KEY"));
        assert!(pub_pem.contains("BEGIN PUBLIC KEY"), "公钥须是 SPKI PEM 供和风控制台粘贴");
        // 两次生成不同(确有取随机)
        let (_, pub2) = generate_keypair().unwrap();
        assert_ne!(pub_pem, pub2);
    }

    #[test]
    fn jwt_has_qweather_shape_and_verifies() {
        let (priv_pem, pub_pem) = generate_keypair().unwrap();
        let jwt = sign_jwt(&priv_pem, "CRED123", "PROJ456", Duration::from_secs(900)).unwrap();

        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3, "JWT = header.payload.sig 三段");

        let header: Value = serde_json::from_slice(&B64.decode(parts[0]).unwrap()).unwrap();
        assert_eq!(header["alg"], "EdDSA");
        assert_eq!(header["kid"], "CRED123", "kid = 凭据 ID");

        let claims: Value = serde_json::from_slice(&B64.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(claims["sub"], "PROJ456", "sub = 项目 ID");
        let iat = claims["iat"].as_u64().unwrap();
        let exp = claims["exp"].as_u64().unwrap();
        assert!(exp > iat && exp - iat <= 24 * 3600, "exp 在 iat 之后且不超 24h");

        // 用导出的公钥验签 `header.payload`(和风端就是这么验的)
        let vk = VerifyingKey::from_public_key_pem(&pub_pem).unwrap();
        let signing_input = format!("{}.{}", parts[0], parts[1]);
        let sig_bytes: [u8; 64] = B64.decode(parts[2]).unwrap().try_into().unwrap();
        let sig = ed25519_dalek::Signature::from_bytes(&sig_bytes);
        assert!(vk.verify(signing_input.as_bytes(), &sig).is_ok(), "全局公钥须能验证私钥的签名");
    }
}
