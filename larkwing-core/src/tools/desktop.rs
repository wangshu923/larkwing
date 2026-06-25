//! 能力轴:桌面(开 + 系统音量)。两个正交原语,给「用户说『打开微信 / 打开下载文件夹 /
//! 打开某网站』『电脑声音大点 / 静音 / 调到一半』」用。OS 副作用按平台分叉:
//! - **打开**:`tokio` 直接喊系统「按名/按路径打开」(Mac `open`/`open -a`、Win `cmd /C start`、
//!   Linux `xdg-open`)。不引依赖、留在 core。
//! - **音量**:Mac 走 `osascript`,Windows 走 Core Audio(`IAudioEndpointVolume`,`#[cfg(windows)]`,
//!   仅 Win 编译)。
//!
//! ⚠️ 平台验收(§8.1):Mac 路径(`open`/`osascript`)开发机即可验;**Windows 路径
//! (`start` 解析应用名、Core Audio COM)只能 Windows 真机验**——含 COM 代码在 Mac 上根本
//! 不编译(`#[cfg(windows)]`),故 Win 端的「能编过 + 能用」是真机/CI 验收项,见 PLAN watch-item。

use async_trait::async_trait;

use super::{Tool, ToolCtx, ToolSpec};

// ───────────────────────── open:打开应用 / 文件 / 网址 ─────────────────────────

pub(super) struct Open {
    spec: ToolSpec,
}

impl Open {
    pub(super) fn new() -> Open {
        Open {
            spec: ToolSpec {
                name: "open",
                description: "在这台电脑上打开一个东西:应用程序(「打开微信」「打开浏览器」)、\
                              文件或文件夹(给绝对路径,如「打开 D:\\照片」)、或网址\
                              (「打开 B 站」= https://www.bilibili.com)。target 传应用名、\
                              绝对路径、或 http(s) 网址。这是「打开/启动」本身 —— 想搜并播放\
                              具体的歌或视频用 media_play、想看文件夹里有什么用 fs_list,别混。\
                              打不开(找不到应用/路径)会如实告诉你,不会假装打开了。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "target": {
                            "type": "string",
                            "description": "要打开的东西:应用名(微信 / 计算器 / Chrome)、绝对路径(D:\\照片、/Users/x/a.pdf)、或 http(s) 网址"
                        }
                    },
                    "required": ["target"]
                }),
                timeout: std::time::Duration::from_secs(15),
                ui_key: "tool.open",
            },
        }
    }
}

#[async_trait]
impl Tool for Open {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: serde_json::Value, _ctx: &ToolCtx) -> anyhow::Result<String> {
        let target = args
            .get("target")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!("缺少 target 参数(要打开什么)"))?;
        open_target(target).await?;
        // 结果是喂给模型的观察(不是 UI 文案),模型用当前人格的语言转述
        Ok(format!("已打开 {target}"))
    }
}

/// 像不像本地路径(用于决定是「开应用」还是「开文件/夹」)。
fn looks_like_path(s: &str) -> bool {
    let b = s.as_bytes();
    s.starts_with('/')                                   // unix 绝对路径
        || s.starts_with("\\\\")                         // Windows UNC \\server\share
        || s.contains('/')
        || s.contains('\\')
        || (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':') // 盘符 C:\…
}

async fn open_target(target: &str) -> anyhow::Result<()> {
    let is_url = target.starts_with("http://") || target.starts_with("https://");
    let is_path = !is_url && looks_like_path(target);
    // 本地路径但根本不存在 → 如实退回(§3.5),别让系统弹个「找不到」的框还报成功
    if is_path && !std::path::Path::new(target).exists() {
        anyhow::bail!("找不到这个路径:{target}");
    }

    #[cfg(target_os = "macos")]
    let cmd = {
        let mut c = tokio::process::Command::new("open");
        if !is_url && !is_path {
            c.arg("-a"); // 既不是网址也不是路径 → 当应用名打开(macOS 能按显示名解析)
        }
        c.arg(target);
        c
    };
    #[cfg(target_os = "windows")]
    let cmd = {
        // `start` 经 cmd:头一个空 "" 占位窗口标题(否则带引号的路径会被当成标题)。
        // 文件/网址可靠;应用按名走 App Paths 注册表尽力解析(显示名可能解析不到 → 见 watch-item)。
        let mut c = tokio::process::Command::new("cmd");
        c.args(["/C", "start", "", target]);
        c
    };
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    let cmd = {
        let mut c = tokio::process::Command::new("xdg-open");
        c.arg(target);
        c
    };

    spawn_and_check(cmd, target).await
}

async fn spawn_and_check(mut cmd: tokio::process::Command, what: &str) -> anyhow::Result<()> {
    let out = cmd
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("启动失败:{e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        let err = err.trim();
        if err.is_empty() {
            anyhow::bail!("打不开 {what}");
        } else {
            anyhow::bail!("打不开 {what}:{err}");
        }
    }
}

// ───────────────────────── system_volume:整机系统音量 ─────────────────────────

pub(super) struct SystemVolume {
    spec: ToolSpec,
}

impl SystemVolume {
    pub(super) fn new() -> SystemVolume {
        SystemVolume {
            spec: ToolSpec {
                name: "system_volume",
                description: "调整整台电脑的系统音量(不是当前播放器的音量 —— 正在放东西、\
                              只想调这次播放的声音用 media_control 的 louder/softer)。\
                              action:up 调大 / down 调小 / set 设到具体音量(value=0–100)/ \
                              mute 静音 / unmute 取消静音。用户说「电脑声音太大了 / 调到一半 / \
                              静音 / 大点声(没在放东西时)」时用。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": ["up", "down", "set", "mute", "unmute"]
                        },
                        "value": {
                            "type": "number",
                            "description": "set=目标音量 0–100;up/down=可选步长(不传默认 10);mute/unmute 不传"
                        }
                    },
                    "required": ["action"]
                }),
                timeout: std::time::Duration::from_secs(5),
                ui_key: "tool.system_volume",
            },
        }
    }
}

#[async_trait]
impl Tool for SystemVolume {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: serde_json::Value, _ctx: &ToolCtx) -> anyhow::Result<String> {
        let action = args
            .get("action")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .ok_or_else(|| anyhow::anyhow!("缺少 action 参数"))?;
        let value = args.get("value").and_then(serde_json::Value::as_f64);

        match action {
            "set" => {
                let v = value.ok_or_else(|| anyhow::anyhow!("set 需要 value(0–100 的音量)"))?;
                vol_set_level(v.clamp(0.0, 100.0) as u8).await?;
            }
            "up" | "down" => {
                let step = value.unwrap_or(10.0).abs();
                let (cur, _) = vol_get().await?;
                let target =
                    if action == "up" { cur as f64 + step } else { cur as f64 - step };
                vol_set_level(target.clamp(0.0, 100.0) as u8).await?;
            }
            "mute" => vol_set_mute(true).await?,
            "unmute" => vol_set_mute(false).await?,
            other => anyhow::bail!("未知的 action:{other}(支持 up/down/set/mute/unmute)"),
        }

        // 回读真值当观察喂回模型(模型据此用自己的人格语言回话,core 不产文案)
        let (vol, muted) = vol_get().await?;
        Ok(serde_json::json!({ "volume": vol, "muted": muted }).to_string())
    }
}

/// 读系统音量 →(0–100 百分比, 是否静音)。
async fn vol_get() -> anyhow::Result<(u8, bool)> {
    #[cfg(target_os = "macos")]
    {
        mac_get().await
    }
    #[cfg(windows)]
    {
        tokio::task::spawn_blocking(win_get).await?
    }
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    {
        anyhow::bail!("这个系统暂不支持调音量")
    }
}

async fn vol_set_level(percent: u8) -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        mac_set_level(percent).await
    }
    #[cfg(windows)]
    {
        tokio::task::spawn_blocking(move || win_set_level(percent)).await?
    }
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    {
        let _ = percent;
        anyhow::bail!("这个系统暂不支持调音量")
    }
}

async fn vol_set_mute(muted: bool) -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        mac_set_mute(muted).await
    }
    #[cfg(windows)]
    {
        tokio::task::spawn_blocking(move || win_set_mute(muted)).await?
    }
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    {
        let _ = muted;
        anyhow::bail!("这个系统暂不支持调音量")
    }
}

// ── macOS:osascript(开发机可验) ──
#[cfg(target_os = "macos")]
async fn mac_run(script: &str) -> anyhow::Result<String> {
    let out = tokio::process::Command::new("osascript")
        .arg("-e")
        .arg(script)
        .output()
        .await?;
    if !out.status.success() {
        anyhow::bail!("osascript 失败:{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(target_os = "macos")]
async fn mac_get() -> anyhow::Result<(u8, bool)> {
    let vol = mac_run("output volume of (get volume settings)").await?;
    let muted = mac_run("output muted of (get volume settings)").await?;
    let v = vol.parse::<f32>().unwrap_or(0.0).clamp(0.0, 100.0) as u8;
    Ok((v, muted.eq_ignore_ascii_case("true")))
}

#[cfg(target_os = "macos")]
async fn mac_set_level(percent: u8) -> anyhow::Result<()> {
    mac_run(&format!("set volume output volume {percent}")).await.map(|_| ())
}

#[cfg(target_os = "macos")]
async fn mac_set_mute(muted: bool) -> anyhow::Result<()> {
    mac_run(&format!("set volume output muted {muted}")).await.map(|_| ())
}

// ── Windows:Core Audio / IAudioEndpointVolume(仅 Win 编译,真机验) ──
#[cfg(windows)]
fn with_endpoint<T>(
    f: impl FnOnce(&windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
    use windows::Win32::Media::Audio::{eConsole, eRender, IMMDeviceEnumerator, MMDeviceEnumerator};
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
    };
    unsafe {
        // 在 spawn_blocking 的线程上初始化 COM(多线程套间)。S_OK/S_FALSE 视为成功;
        // RPC_E_CHANGED_MODE(套间已被别的模型占用)则不自行 uninit、直接复用现有套间。
        let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
        let did_init = hr.is_ok();
        let result = (|| -> anyhow::Result<T> {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
            let endpoint: IAudioEndpointVolume = device.Activate(CLSCTX_ALL, None)?;
            f(&endpoint)
        })();
        if did_init {
            CoUninitialize();
        }
        result
    }
}

#[cfg(windows)]
fn win_get() -> anyhow::Result<(u8, bool)> {
    with_endpoint(|ep| unsafe {
        let scalar = ep.GetMasterVolumeLevelScalar()?;
        let muted = ep.GetMute()?.as_bool();
        Ok(((scalar * 100.0).round().clamp(0.0, 100.0) as u8, muted))
    })
}

#[cfg(windows)]
fn win_set_level(percent: u8) -> anyhow::Result<()> {
    with_endpoint(|ep| unsafe {
        ep.SetMasterVolumeLevelScalar(percent as f32 / 100.0, std::ptr::null())?;
        Ok(())
    })
}

#[cfg(windows)]
fn win_set_mute(muted: bool) -> anyhow::Result<()> {
    with_endpoint(|ep| unsafe {
        // IAudioEndpointVolume::SetMute 在 windows 0.61 收的是裸 `bool`(不是 BOOL),
        // 第二参是裸 `*const GUID`(不是 Option) —— 已用 windows 目标交叉 check 核实(见验收单)。
        ep.SetMute(muted, std::ptr::null())?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn looks_like_path_distinguishes_app_names_from_paths() {
        // 路径(各形态)
        assert!(looks_like_path("/Users/x/a.pdf"));
        assert!(looks_like_path("D:\\照片"));
        assert!(looks_like_path("D:\\Movies\\a.mp4"));
        assert!(looks_like_path("\\\\nas\\share\\x"));
        assert!(looks_like_path("照片/2024")); // 含斜杠 = 当路径
        // 应用名(无路径分隔、无盘符)
        assert!(!looks_like_path("微信"));
        assert!(!looks_like_path("Chrome"));
        assert!(!looks_like_path("计算器"));
    }
}
