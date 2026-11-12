//! 检查更新:逐个地址试,任何一种失败都换下一个;下载地址也能换入口重试。
//!
//! **为什么不用插件自带的 `check`**:它在一个循环里试 `endpoints`,但只在「连不上 / 非 2xx / JSON 形状
//! 不对」时才换下一个;回了 200 而正文不是 JSON(第三方镜像的拦截页 / 验证页)那一处是
//! `res.json().await?` —— 直接整体失败,排在后面的地址根本不试(tauri-plugin-updater 2.10.1
//! `updater.rs` 的端点循环)。而镜像排在最前面(国内直连 GitHub 不稳,用户拍板 2026-06-30)。
//! 这里把每个地址单独交给插件查一次、自己做循环:版本比较 / 平台匹配 / 签名校验仍是插件那一套。
//!
//! **下载换入口**:清单里每个平台只有一个下载地址(CI 写的是镜像)。那个镜像不通时,按 `endpoints`
//! 的其余入口(另一个镜像 / 官方)各拼一个候选地址。换入口不降低安全性 —— 下载的包照旧要过
//! minisign 签名校验,地址只决定字节从哪来。
//!
//! 与插件的衔接:查到的 `Update` 放进**这个 webview 的资源表**,返回的元数据与插件 `check` 命令
//! 同形,前端用它 `new Update(meta)`,之后的 download / install 仍走插件命令。

use std::time::Duration;

use serde::Serialize;
use tauri::{Manager, ResourceId, Url};
use tauri_plugin_updater::{Update, UpdaterExt};

/// 官方原址的固定开头。镜像地址的形状是 `<镜像前缀>https://github.com/…`,所以「入口前缀」=
/// 它前面的那一截(官方地址的前缀是空串)。
const UPSTREAM: &str = "https://github.com/";

/// 与插件 `check` 命令返回的元数据同形(前端 `new Update(meta)` 要的字段),另加可换的下载入口数。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateMeta {
  rid: ResourceId,
  current_version: String,
  version: String,
  /// 前端没用到发布日期;为它引 `time` 的格式化不值,恒空。
  date: Option<String>,
  body: Option<String>,
  raw_json: serde_json::Value,
  /// 除清单里那个下载地址之外,还能换几个入口(`update_retarget` 的 `alt` 取 0..这个数)。
  download_alts: usize,
}

/// `tauri.conf` 里 `plugins.updater.endpoints`,原样字符串,顺序即优先级。
fn configured_endpoints(app: &tauri::AppHandle) -> Vec<String> {
  app
    .config()
    .plugins
    .0
    .get("updater")
    .and_then(|u| u.get("endpoints"))
    .and_then(|e| e.as_array())
    .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
    .unwrap_or_default()
}

/// 一个地址的入口前缀;不含官方原址的(形状不认识)= None,不参与换入口。
fn entry_prefix(endpoint: &str) -> Option<&str> {
  endpoint.find(UPSTREAM).map(|i| &endpoint[..i])
}

/// 当前下载地址能换成哪些:剥掉现有前缀拿到官方原址,再套上各入口前缀;去掉与现址相同的、去重,
/// 顺序照 `endpoints`。现址形状不认识 → 空(不猜)。
fn download_alternatives(current: &str, endpoints: &[String]) -> Vec<String> {
  let Some(i) = current.find(UPSTREAM) else {
    return Vec::new();
  };
  let raw = &current[i..];
  let mut out: Vec<String> = Vec::new();
  for ep in endpoints {
    let Some(prefix) = entry_prefix(ep) else { continue };
    let cand = format!("{prefix}{raw}");
    if cand != current && !out.contains(&cand) {
      out.push(cand);
    }
  }
  out
}

fn meta(webview: &tauri::Webview, update: Update, endpoints: &[String]) -> UpdateMeta {
  UpdateMeta {
    current_version: update.current_version.clone(),
    version: update.version.clone(),
    date: None,
    body: update.body.clone(),
    raw_json: update.raw_json.clone(),
    download_alts: download_alternatives(update.download_url.as_str(), endpoints).len(),
    rid: webview.resources_table().add(update),
  }
}

/// 查一次更新:按 `endpoints` 顺序逐个试,第一个给出明确答案(有新版 / 已最新)的算数;
/// 一个地址怎么失败都换下一个,全失败才报错(带最后一个地址的原因)。
#[tauri::command]
pub async fn update_check(
  webview: tauri::Webview,
  proxy: Option<String>,
  timeout: Option<u64>,
) -> Result<Option<UpdateMeta>, String> {
  let endpoints = configured_endpoints(webview.app_handle());
  if endpoints.is_empty() {
    return Err("no updater endpoints configured".into());
  }
  let proxy = match proxy.as_deref() {
    Some(p) => Some(Url::parse(p).map_err(|e| format!("bad proxy url: {e}"))?),
    None => None,
  };
  let mut last_err = String::new();
  for ep in &endpoints {
    let attempt = async {
      let url = Url::parse(ep).map_err(|e| e.to_string())?;
      let mut b = webview.updater_builder().endpoints(vec![url]).map_err(|e| e.to_string())?;
      if let Some(ms) = timeout {
        b = b.timeout(Duration::from_millis(ms));
      }
      if let Some(p) = &proxy {
        b = b.proxy(p.clone());
      }
      b.build().map_err(|e| e.to_string())?.check().await.map_err(|e| e.to_string())
    };
    match attempt.await {
      Ok(Some(update)) => return Ok(Some(meta(&webview, update, &endpoints))),
      Ok(None) => return Ok(None),
      Err(e) => {
        tracing::warn!(endpoint = %ep, error = %e, "检查更新:这个地址没给出可用的清单,换下一个");
        last_err = e;
      }
    }
  }
  Err(last_err)
}

/// 把一次查到的更新换个下载入口:返回一份新的元数据(新的资源号),前端用它重下。
/// `alt` 恒按**最初那次查到的** `rid` 来数,序号才稳定。
#[tauri::command]
pub fn update_retarget(webview: tauri::Webview, rid: ResourceId, alt: usize) -> Result<UpdateMeta, String> {
  let cur = webview.resources_table().get::<Update>(rid).map_err(|e| e.to_string())?;
  let endpoints = configured_endpoints(webview.app_handle());
  let alts = download_alternatives(cur.download_url.as_str(), &endpoints);
  let target = alts.get(alt).ok_or_else(|| format!("no download alternative #{alt}"))?;
  let mut next = (*cur).clone();
  next.download_url = Url::parse(target).map_err(|e| e.to_string())?;
  tracing::info!(to = %target, "更新下载换入口");
  Ok(meta(&webview, next, &endpoints))
}

#[cfg(test)]
mod tests {
  use super::*;

  fn eps() -> Vec<String> {
    vec![
      "https://mirror-a.example/https://github.com/o/r/releases/latest/download/latest.json".into(),
      "https://mirror-b.example/https://github.com/o/r/releases/latest/download/latest.json".into(),
      "https://github.com/o/r/releases/latest/download/latest.json".into(),
    ]
  }

  #[test]
  fn entry_prefix_splits_mirror_from_upstream() {
    assert_eq!(entry_prefix(&eps()[0]), Some("https://mirror-a.example/"));
    assert_eq!(entry_prefix(&eps()[2]), Some(""), "官方地址的前缀是空串");
    assert_eq!(entry_prefix("https://elsewhere.example/latest.json"), None);
  }

  #[test]
  fn alternatives_swap_the_entry_and_skip_the_current_one() {
    // 清单里写的是镜像 B(CI 改写的那个)→ 候选 = 镜像 A、官方,顺序照 endpoints
    let cur = "https://mirror-b.example/https://github.com/o/r/releases/download/v1/app.exe";
    assert_eq!(
      download_alternatives(cur, &eps()),
      vec![
        "https://mirror-a.example/https://github.com/o/r/releases/download/v1/app.exe".to_string(),
        "https://github.com/o/r/releases/download/v1/app.exe".to_string(),
      ]
    );
    // 清单里是官方原址 → 候选 = 两个镜像
    let raw = "https://github.com/o/r/releases/download/v1/app.exe";
    assert_eq!(download_alternatives(raw, &eps()).len(), 2);
  }

  #[test]
  fn unknown_shapes_get_no_alternatives() {
    assert!(download_alternatives("https://cdn.example/app.exe", &eps()).is_empty(), "现址形状不认识就不猜");
    assert!(download_alternatives("https://github.com/o/r/x.exe", &[]).is_empty());
  }
}
