//! 流解析:spawn yt-dlp 拿"真实流地址 + 必需请求头"。robot 里这件事藏在 mpv 的
//! ytdl 钩子里;我们自己接管是为了把播放器长进 UI(宪法 §3.4 界面优先)。
//! 解析是回合内阻塞型(秒级):子进程 kill_on_drop,取消级联天然成立(约束 #5)。

use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::Serialize;

/// 音频优先选 m4a:WebView2(Chromium)和开发机 WKWebView 都原生解码 AAC;
/// opus/webm 在 WKWebView 放不了,别让开发机和发布机行为分叉。
const AUDIO_FORMAT: &str = "ba[ext=m4a]/ba/b";
/// 视频:mp4 容器 + 1080p 封顶(家庭场景够用,流量也省);音视频分离时两路都拿,
/// 交给 relay 的 ffmpeg 混流;实在没有就退单文件。
const VIDEO_FORMAT: &str = "bv*[ext=mp4][height<=1080]+ba[ext=m4a]/bv*+ba/b";

#[derive(Debug, Clone, Serialize)]
pub struct UpStream {
    pub url: String,
    /// yt-dlp 给的 http_headers(Referer/UA 防盗链就在这),转发/混流时原样带上。
    pub headers: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
pub struct Resolved {
    pub title: String,
    pub uploader: Option<String>,
    pub duration_seconds: Option<f64>,
    /// 1 路 = 直转;2 路(视频+音频分离,B 站 DASH 常态)= 走 ffmpeg 混流。
    pub streams: Vec<UpStream>,
}

/// 解析失败的粗分类:登录态问题要单独可见(UI 出扫码入口,模型换话术)。
#[derive(Debug)]
pub enum ResolveError {
    AuthRequired(String),
    Other(anyhow::Error),
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolveError::AuthRequired(s) => write!(f, "需要登录: {s}"),
            ResolveError::Other(e) => write!(f, "{e:#}"),
        }
    }
}

pub async fn resolve(
    ytdlp: &Path,
    page_url: &str,
    cookies_file: Option<&Path>,
    audio_only: bool,
) -> Result<Resolved, ResolveError> {
    let mut cmd = tokio::process::Command::new(ytdlp);
    cmd.arg("-j") // 单条 JSON,不下载
        .arg("--no-warnings")
        .arg("--no-playlist")
        .arg("--socket-timeout")
        .arg("15")
        .arg("-f")
        .arg(if audio_only { AUDIO_FORMAT } else { VIDEO_FORMAT });
    if let Some(f) = cookies_file {
        cmd.arg("--cookies").arg(f);
    }
    cmd.arg(page_url);
    cmd.kill_on_drop(true); // 回合取消 → future 被 drop → 子进程跟着死,不留孤儿
    cmd.stdin(std::process::Stdio::null());

    let out = tokio::time::timeout(std::time::Duration::from_secs(45), cmd.output())
        .await
        .map_err(|_| ResolveError::Other(anyhow::anyhow!("解析超时(45s)")))?
        .map_err(|e| ResolveError::Other(anyhow::anyhow!("yt-dlp 启动失败: {e}")))?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let tail: String = stderr.chars().rev().take(300).collect::<Vec<_>>().into_iter().rev().collect();
        let lower = stderr.to_lowercase();
        if lower.contains("login") || lower.contains("premium") || lower.contains("登录")
            || lower.contains("大会员") || lower.contains("cookies")
        {
            return Err(ResolveError::AuthRequired(tail));
        }
        return Err(ResolveError::Other(anyhow::anyhow!("yt-dlp 解析失败: {tail}")));
    }

    let json: serde_json::Value = serde_json::from_slice(&out.stdout)
        .context("yt-dlp 输出不是 JSON")
        .map_err(ResolveError::Other)?;
    parse_resolved(&json).map_err(ResolveError::Other)
}

/// 纯函数,可测:从 yt-dlp -j 输出抽出流清单。
/// 合并格式 → requested_formats 数组(视频在前音频在后);单格式 → 顶层 url。
pub(super) fn parse_resolved(json: &serde_json::Value) -> Result<Resolved> {
    let title = json["title"].as_str().unwrap_or("未知标题").to_string();
    let uploader = json["uploader"].as_str().map(str::to_string);
    let duration_seconds = json["duration"].as_f64();

    let mut streams = Vec::new();
    if let Some(formats) = json["requested_formats"].as_array() {
        for f in formats {
            streams.push(stream_of(f).context("requested_formats 缺 url")?);
        }
    } else if json["url"].is_string() {
        streams.push(stream_of(json).expect("is_string 已检查"));
    }
    if streams.is_empty() {
        bail!("解析结果里没有可用的流");
    }
    if streams.len() > 2 {
        // 没见过的形状(多音轨?):取前两路并告警,别直接趴下
        tracing::warn!(n = streams.len(), "requested_formats 超过两路,只取前两路");
        streams.truncate(2);
    }
    Ok(Resolved { title, uploader, duration_seconds, streams })
}

fn stream_of(v: &serde_json::Value) -> Option<UpStream> {
    let url = v["url"].as_str()?.to_string();
    let headers = v["http_headers"]
        .as_object()
        .map(|m| {
            m.iter()
                .filter_map(|(k, val)| val.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    Some(UpStream { url, headers })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_url_audio_shape() {
        let json = serde_json::json!({
            "title": "恭喜发财",
            "uploader": "某UP",
            "duration": 225.0,
            "url": "https://cdn.example/audio.m4a",
            "http_headers": { "Referer": "https://www.bilibili.com/", "User-Agent": "UA" }
        });
        let r = parse_resolved(&json).unwrap();
        assert_eq!(r.title, "恭喜发财");
        assert_eq!(r.duration_seconds, Some(225.0));
        assert_eq!(r.streams.len(), 1);
        assert_eq!(r.streams[0].url, "https://cdn.example/audio.m4a");
        assert!(r.streams[0].headers.iter().any(|(k, v)| k == "Referer" && v.contains("bilibili")));
    }

    #[test]
    fn requested_formats_video_pair_shape() {
        let json = serde_json::json!({
            "title": "小猪佩奇 第1集",
            "duration": 300,
            "requested_formats": [
                { "url": "https://cdn.example/video.m4s", "http_headers": { "Referer": "r" } },
                { "url": "https://cdn.example/audio.m4s", "http_headers": { "Referer": "r" } }
            ]
        });
        let r = parse_resolved(&json).unwrap();
        assert_eq!(r.streams.len(), 2, "音视频分离 = 两路,走混流");
        assert_eq!(r.uploader, None);
    }

    #[test]
    fn no_stream_is_an_error() {
        assert!(parse_resolved(&serde_json::json!({ "title": "x" })).is_err());
    }
}
