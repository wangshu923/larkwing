//! 流解析:spawn yt-dlp 拿"真实流地址 + 必需请求头"。robot 里这件事藏在 mpv 的
//! ytdl 钩子里;我们自己接管是为了把播放器长进 UI(宪法 §3.4 界面优先)。
//! 解析是回合内阻塞型(秒级):子进程 kill_on_drop,取消级联天然成立(约束 #5)。

use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::Serialize;

/// 音频优先选 m4a:WebView2(Chromium)和开发机 WKWebView 都原生解码 AAC;
/// opus/webm 在 WKWebView 放不了,别让开发机和发布机行为分叉。
const AUDIO_FORMAT: &str = "ba[ext=m4a]/ba/b";
/// 视频:**强制 H.264(avc)** + 1080p 封顶。`[ext=mp4]` 只约束容器不约束编码,
/// B 站(尤其登录后)常给 HEVC/AV1;WebView2(Windows 发布机)解不了 HEVC(要付费扩展)
/// /AV1(要免费扩展,常缺)→ 视频轨黑屏、只剩 AAC 声音(开发机 WKWebView 有硬解,故漏网)。
/// relay 是 `-c copy` 纯复制,编码选择是唯一低成本杠杆(转码毁掉"CPU 近零")。
/// 逐级放宽:avc≤1080+m4a → avc 任意分辨率 → avc+任意音频 → avc 单文件;末段保留旧串做
/// 兜底地板(HEVC/AV1-only 的极少数视频仍能出声,不硬失败)。音视频分离两路交 ffmpeg 混流。
const VIDEO_FORMAT: &str = "bv*[vcodec^=avc][height<=1080]+ba[ext=m4a]/\
                            bv*[vcodec^=avc]+ba[ext=m4a]/\
                            bv*[vcodec^=avc]+ba/\
                            b[vcodec^=avc]/\
                            bv*[ext=mp4][height<=1080]+ba[ext=m4a]/bv*+ba/b";

#[derive(Debug, Clone, Default, Serialize)]
pub struct UpStream {
    pub url: String,
    /// yt-dlp 给的 http_headers(Referer/UA 防盗链就在这),转发/混流时原样带上。
    pub headers: Vec<(String, String)>,
    // 以下供合成 DASH MPD(B 站两路自适应流走 MSE 播放,见 relay::build_mpd);
    // 非 DASH 路径(直转 / 本地)忽略。来自 yt-dlp 的 per-format 元数据。
    /// 视频编码(RFC6381,如 `avc1.640028`);音频流为 None。
    pub vcodec: Option<String>,
    /// 音频编码(RFC6381,如 `mp4a.40.2`);视频流为 None。
    pub acodec: Option<String>,
    pub width: Option<u64>,
    pub height: Option<u64>,
    /// 码率(bits/s),DASH Representation 必填项。
    pub bandwidth: Option<u64>,
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
    super::no_console(&mut cmd); // Windows 下不弹控制台黑框

    let out = tokio::time::timeout(std::time::Duration::from_secs(45), cmd.output())
        .await
        .map_err(|_| ResolveError::Other(anyhow::anyhow!("解析超时(45s)")))?
        .map_err(|e| ResolveError::Other(anyhow::anyhow!("yt-dlp 启动失败: {e}")))?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        // 观测(宪法 §7):真实失败原因进日志。之前只塞进 bail! 喂模型,控制台看不见 ——
        // 用户实测"控制台看不出问题"正是这条缺位;解析失败是冷路径,全量记录不会刷屏。
        tracing::warn!(url = page_url, "yt-dlp 解析失败:\n{}", stderr.trim());
        return Err(classify_failure(&stderr));
    }

    let json: serde_json::Value = serde_json::from_slice(&out.stdout)
        .context("yt-dlp 输出不是 JSON")
        .map_err(ResolveError::Other)?;
    parse_resolved(&json).map_err(ResolveError::Other)
}

/// 失败分类(纯函数,可测):风控/登录类 → AuthRequired(UI 出扫码入口、模型换话术);
/// 其余 → Other。HTTP 412(Precondition Failed)/403 是 B 站对**匿名请求**的拦截,
/// 远端语义就是"得先登录"——与搜索路径把 412/403 当 RiskControl 一致;旧 robot 靠
/// `cookies-from-browser=firefox` 借浏览器登录态绕过,Larkwing 改走 app 内扫码(宪法 §4)。
/// 故意不收"已删除/地区限制":那类登录救不了,留给 Other 如实上报。
fn classify_failure(stderr: &str) -> ResolveError {
    let tail: String =
        stderr.chars().rev().take(300).collect::<Vec<_>>().into_iter().rev().collect();
    let lower = stderr.to_lowercase();
    let needs_login = lower.contains("login")
        || lower.contains("premium")
        || lower.contains("登录")
        || lower.contains("大会员")
        || lower.contains("cookies")
        || lower.contains("precondition failed") // HTTP 412:B 站风控匿名请求
        || lower.contains("http error 403")
        || lower.contains("风控");
    if needs_login {
        ResolveError::AuthRequired(tail)
    } else {
        ResolveError::Other(anyhow::anyhow!("yt-dlp 解析失败: {tail}"))
    }
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
    // 观测(宪法 §7):记下实际选中的编码。B 站只给 HEVC/AV1 时 WebView2 黑屏只剩声音,
    // 这条 info 是 Windows 真机唯一能看出"到底选了什么编码"的地方(强制 avc 是否生效)。
    let fmts: Vec<String> = match json["requested_formats"].as_array() {
        Some(arr) => arr.iter().map(fmt_tag).collect(),
        None => vec![fmt_tag(json)],
    };
    tracing::info!(title = %title, streams = streams.len(), fmts = ?fmts, "媒体解析完成");
    Ok(Resolved { title, uploader, duration_seconds, streams })
}

/// 一个格式的紧凑诊断标签:`format_id/vcodec`(给日志看选中编码用)。
fn fmt_tag(v: &serde_json::Value) -> String {
    format!(
        "{}/{}",
        v["format_id"].as_str().unwrap_or("?"),
        v["vcodec"].as_str().unwrap_or("?")
    )
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
    // 编码/码率/尺寸:供 DASH MPD 合成;"none"/空当缺失。码率优先 tbr,回落 vbr/abr。
    let codec = |key| v[key].as_str().filter(|s| !s.is_empty() && *s != "none").map(str::to_string);
    let kbps = v["tbr"].as_f64().or_else(|| v["vbr"].as_f64()).or_else(|| v["abr"].as_f64());
    Some(UpStream {
        url,
        headers,
        vcodec: codec("vcodec"),
        acodec: codec("acodec"),
        width: v["width"].as_u64(),
        height: v["height"].as_u64(),
        bandwidth: kbps.map(|k| (k * 1000.0) as u64),
    })
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

    #[test]
    fn http_412_and_403_classify_as_auth() {
        // B 站对匿名请求的实拍报错(本机复现 萌鸡小队 即此):412 = 风控 = 需要登录
        let e = classify_failure(
            "ERROR: [BiliBili] 1P87Z6SEfN: Unable to download JSON metadata: \
             HTTP Error 412: Precondition Failed",
        );
        assert!(matches!(e, ResolveError::AuthRequired(_)), "412 风控应判为需要登录");
        assert!(matches!(
            classify_failure("ERROR: HTTP Error 403: Forbidden"),
            ResolveError::AuthRequired(_)
        ));
    }

    #[test]
    fn explicit_login_keywords_classify_as_auth() {
        assert!(matches!(classify_failure("需要大会员才能观看"), ResolveError::AuthRequired(_)));
        assert!(matches!(classify_failure("This video requires login"), ResolveError::AuthRequired(_)));
    }

    #[test]
    fn deleted_or_georestricted_is_not_auth() {
        // 登录救不了的:照实当 Other 报"没解析出来",别误导用户去扫码
        let e = classify_failure(
            "ERROR: [BiliBili] x: This video may be deleted or geo-restricted. \
             You might want to try a VPN or a proxy server",
        );
        assert!(matches!(e, ResolveError::Other(_)), "已删除/地区限制不归 auth");
    }
}
