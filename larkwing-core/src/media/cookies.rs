//! 登录态存取:壳层从登录窗口的原生 CookieManager 取到 cookie(SESSDATA 是 HttpOnly,
//! JS 拿不到,必须走原生 API),这里负责落库(settings,按源自治)+ 导出两种消费形:
//! Netscape 文件喂 yt-dlp `--cookies`,Cookie 请求头喂搜索 API。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::store::Store;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CookieRec {
    pub name: String,
    pub value: String,
    pub domain: String,
    #[serde(default = "default_path")]
    pub path: String,
}

fn default_path() -> String {
    "/".into()
}

fn settings_key(source: &str) -> String {
    format!("media.cookies.{source}")
}

pub fn load(store: &Store, source: &str) -> Option<Vec<CookieRec>> {
    let json = store.settings.get(None, &settings_key(source)).ok()??;
    serde_json::from_str(&json).ok()
}

pub fn save(store: &Store, source: &str, cookies: &[CookieRec]) -> Result<()> {
    let json = serde_json::to_string(cookies)?;
    store.settings.set(None, &settings_key(source), &json)?;
    Ok(())
}

/// `Cookie: k=v; k2=v2` 请求头值(搜索 API 用)。
pub fn header_value(cookies: &[CookieRec]) -> String {
    cookies
        .iter()
        .map(|c| format!("{}={}", c.name, c.value))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Netscape cookies.txt(yt-dlp --cookies 的输入形)。过期时间给远未来:
/// 真实有效期由站点说了算,失效走 AuthRequired 兜底,这里不替它记账。
pub fn netscape(cookies: &[CookieRec]) -> String {
    let mut out = String::from("# Netscape HTTP Cookie File\n");
    for c in cookies {
        let sub = if c.domain.starts_with('.') { "TRUE" } else { "FALSE" };
        out.push_str(&format!(
            "{}\t{}\t{}\tTRUE\t2147483647\t{}\t{}\n",
            c.domain, sub, c.path, c.name, c.value
        ));
    }
    out
}

/// 导出 cookie 文件,返回路径;无 cookie = None(yt-dlp 不带 --cookies 匿名跑)。
pub async fn export_file(dir: &Path, source: &str, cookies: &[CookieRec]) -> Result<PathBuf> {
    tokio::fs::create_dir_all(dir).await?;
    let path = dir.join(format!("cookies-{source}.txt"));
    tokio::fs::write(&path, netscape(cookies)).await.context("写 cookie 文件失败")?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn netscape_format_golden() {
        let cookies = vec![
            CookieRec {
                name: "SESSDATA".into(),
                value: "abc%2C123".into(),
                domain: ".bilibili.com".into(),
                path: "/".into(),
            },
            CookieRec {
                name: "buvid3".into(),
                value: "x".into(),
                domain: "www.bilibili.com".into(),
                path: "/".into(),
            },
        ];
        let txt = netscape(&cookies);
        assert!(txt.starts_with("# Netscape HTTP Cookie File\n"), "yt-dlp 认这行头");
        assert!(txt.contains(".bilibili.com\tTRUE\t/\tTRUE\t2147483647\tSESSDATA\tabc%2C123\n"));
        assert!(txt.contains("www.bilibili.com\tFALSE\t/\tTRUE\t2147483647\tbuvid3\tx\n"));
        assert_eq!(header_value(&cookies), "SESSDATA=abc%2C123; buvid3=x");
    }
}
