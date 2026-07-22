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
                              文件或文件夹(给绝对路径,支持 ~ 开头)、或网址\
                              (「打开 B 站」= https://www.bilibili.com)。target 传应用名、\
                              绝对路径、或 http(s) 网址。这是「打开/启动」本身 —— 想搜并播放\
                              具体的歌或视频用 media_play、想看文件夹里有什么用 fs_list,别混。\
                              打不开(找不到应用/路径)会如实告诉你,不会假装打开了。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "target": {
                            "type": "string",
                            "description": "要打开的东西:应用名(微信 / 计算器 / Chrome)、绝对路径(支持 ~ 开头)、或 http(s) 网址"
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
            .map(super::expand_home) // 「~/xxx」路径形宽容展开(§4.4;应用名/网址原样)
            .ok_or_else(|| anyhow::anyhow!("缺少 target 参数(要打开什么)"))?;
        open_target(&target).await?;
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
        // 文件/网址可靠;但应用名 `start 微信` 只认 PATH/App Paths,认不出中文显示名(实锤:
        // 「系统找不到文件 微信」)→ 先在开始菜单按显示名找同名快捷方式(用户说的「微信」正是
        // 那里显示的名字),命中就 start 那个 .lnk;没命中再把原名交给 start 靠 PATH/App Paths
        // 尽力(Chrome 等注册过的仍中)。
        let launch = if !is_url && !is_path {
            resolve_start_menu_app(target).unwrap_or_else(|| target.to_string())
        } else {
            target.to_string()
        };
        let mut c = tokio::process::Command::new("cmd");
        c.args(["/C", "start", "", &launch]);
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
        // Windows 命令行 stderr 是本地代码页(中文系统 = GBK),不是 UTF-8 → lossy 会出 � 乱码;
        // 乱码就别原样抛给模型(否则观察成「ϵͳ�Ҳ����ļ�」),给干净兜底(§3.5 如实且可读)。
        let raw = String::from_utf8_lossy(&out.stderr);
        let err = raw.trim();
        if err.is_empty() || err.contains('\u{FFFD}') {
            anyhow::bail!("打不开 {what}(没找到这个应用或文件)");
        } else {
            anyhow::bail!("打不开 {what}:{err}");
        }
    }
}

/// 从开始菜单收集到的 (显示名, .lnk 绝对路径) 候选里,挑最匹配 `target` 的那个。
/// 平台无关纯函数(真正扫盘的 `resolve_start_menu_app` 是 Windows-only,不便单测,逻辑抽这里)。
/// 规则:① 显示名归一后完全相等优先(用户说「微信」→「微信.lnk」,不会误中「卸载微信」);
/// ② 否则取「显示名包含 target、且非卸载/帮助类」中最短的(「chrome」→「Google Chrome」)。
#[cfg(any(target_os = "windows", test))]
fn best_shortcut_match(target: &str, entries: &[(String, String)]) -> Option<String> {
    let t = target.trim().to_lowercase();
    if t.is_empty() {
        return None;
    }
    if let Some((_, path)) = entries.iter().find(|(name, _)| name.to_lowercase() == t) {
        return Some(path.clone());
    }
    // 卸载器 / 帮助文档不是「打开这个应用」该启动的东西
    const NOISE: [&str; 4] = ["卸载", "uninstall", "帮助", "help"];
    entries
        .iter()
        .filter(|(name, _)| {
            let n = name.to_lowercase();
            n.contains(&t) && !NOISE.iter().any(|w| n.contains(w))
        })
        .min_by_key(|(name, _)| name.chars().count())
        .map(|(_, path)| path.clone())
}

/// Windows:扫用户 + 全局开始菜单的 `Programs`(递归)找应用快捷方式,按显示名匹配 `target`,
/// 命中返回 .lnk 绝对路径(交给 `start` 打开)。找不到返回 None(调用方回落原名给 start)。
#[cfg(target_os = "windows")]
fn resolve_start_menu_app(target: &str) -> Option<String> {
    fn collect(dir: &std::path::Path, out: &mut Vec<(String, String)>, depth: u32) {
        if depth > 5 {
            return; // 开始菜单层级很浅,防软链环兜底
        }
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect(&path, out, depth + 1);
            } else if path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("lnk"))
            {
                if let (Some(stem), Some(full)) =
                    (path.file_stem().and_then(|s| s.to_str()), path.to_str())
                {
                    out.push((stem.to_string(), full.to_string()));
                }
            }
        }
    }

    let mut entries = Vec::new();
    let roots = [
        std::env::var_os("APPDATA")
            .map(|a| std::path::Path::new(&a).join(r"Microsoft\Windows\Start Menu\Programs")),
        std::env::var_os("ProgramData")
            .map(|a| std::path::Path::new(&a).join(r"Microsoft\Windows\Start Menu\Programs")),
    ];
    for root in roots.into_iter().flatten() {
        collect(&root, &mut entries, 0);
    }
    best_shortcut_match(target, &entries)
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

// ───────────────────────── power:电源 / 屏幕(锁屏 / 睡眠 / 息屏 / 关机 / 重启) ─────────────────────────

/// 关机 / 重启倒计时秒数(用户 2026-06-29 拍板 60s)。OS 自带可取消倒计时:用户说「取消」
/// 期间调 cancel 即停 —— 用对话兜底,不建 Tool::risk 人在环中闸门(§0.2.0)。
const SHUTDOWN_DELAY_SECS: u32 = 60;

pub(super) struct Power {
    spec: ToolSpec,
}

impl Power {
    pub(super) fn new() -> Power {
        Power {
            spec: ToolSpec {
                name: "power",
                description: "控制这台电脑的电源 / 屏幕。action:lock 锁屏 / sleep 让电脑睡眠 / \
                              display_off 关掉屏幕(息屏,电脑还醒着)/ shutdown 关机 / restart 重启 / \
                              cancel 取消还没执行的关机或重启。关机和重启**不是立刻执行**——会留 60 秒\
                              倒计时,这期间用户说「取消 / 别关了 / 停」时你就用 action:cancel 停下。\
                              锁屏 / 睡眠 / 息屏是即时且可逆的,直接做。用户说「锁屏 / 睡一下 / 把屏幕关了 / \
                              关机 / 重启电脑 / 取消关机」时用。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": ["lock", "sleep", "display_off", "shutdown", "restart", "cancel"]
                        }
                    },
                    "required": ["action"]
                }),
                timeout: std::time::Duration::from_secs(10),
                ui_key: "tool.power",
            },
        }
    }
}

#[async_trait]
impl Tool for Power {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: serde_json::Value, _ctx: &ToolCtx) -> anyhow::Result<String> {
        let action = args
            .get("action")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .ok_or_else(|| anyhow::anyhow!("缺少 action 参数"))?;
        match action {
            "lock" => {
                power_lock().await?;
                Ok("已锁屏".into())
            }
            "sleep" => {
                power_sleep().await?;
                Ok("已让电脑睡眠".into())
            }
            "display_off" => {
                power_display_off().await?;
                Ok("已关屏(电脑还醒着)".into())
            }
            "shutdown" => {
                power_shutdown(false).await?;
                Ok(format!("已安排 {SHUTDOWN_DELAY_SECS} 秒后关机;用户要是改主意说取消,就用 cancel 停下"))
            }
            "restart" => {
                power_shutdown(true).await?;
                Ok(format!("已安排 {SHUTDOWN_DELAY_SECS} 秒后重启;用户要是改主意说取消,就用 cancel 停下"))
            }
            "cancel" => {
                power_cancel().await?;
                Ok("已取消待执行的关机 / 重启".into())
            }
            other => anyhow::bail!("未知的 action:{other}(支持 lock/sleep/display_off/shutdown/restart/cancel)"),
        }
    }
}

// ── macOS(开发机:CGSession / pmset / osascript)──
#[cfg(target_os = "macos")]
async fn power_lock() -> anyhow::Result<()> {
    let mut c = tokio::process::Command::new(
        "/System/Library/CoreServices/Menu Extras/User.menu/Contents/Resources/CGSession",
    );
    c.arg("-suspend");
    spawn_and_check(c, "锁屏").await
}
#[cfg(target_os = "macos")]
async fn power_sleep() -> anyhow::Result<()> {
    let mut c = tokio::process::Command::new("pmset");
    c.arg("sleepnow");
    spawn_and_check(c, "睡眠").await
}
#[cfg(target_os = "macos")]
async fn power_display_off() -> anyhow::Result<()> {
    let mut c = tokio::process::Command::new("pmset");
    c.arg("displaysleepnow");
    spawn_and_check(c, "息屏").await
}
#[cfg(target_os = "macos")]
async fn power_shutdown(restart: bool) -> anyhow::Result<()> {
    // Mac 开发机:无 sudo 不能定时关机 → osascript 走正常关机流程(会按系统设置弹保存提示);
    // 可取消倒计时是 Windows(目标平台)特性,Mac 即时执行,cancel 在 Mac 无待取消项。
    let verb = if restart { "restart" } else { "shut down" };
    let mut c = tokio::process::Command::new("osascript");
    c.arg("-e").arg(format!("tell application \"System Events\" to {verb}"));
    spawn_and_check(c, if restart { "重启" } else { "关机" }).await
}
#[cfg(target_os = "macos")]
async fn power_cancel() -> anyhow::Result<()> {
    // Mac 这条没有可取消的定时关机(上面是即时);如实告知,不假装。
    anyhow::bail!("Mac 上的关机是即时的,没有待取消的倒计时")
}

// ── Windows(目标平台:rundll32 / shutdown / SendMessage,真机验)──
#[cfg(windows)]
async fn power_lock() -> anyhow::Result<()> {
    let mut c = tokio::process::Command::new("rundll32.exe");
    c.args(["user32.dll,LockWorkStation"]);
    spawn_and_check(c, "锁屏").await
}
#[cfg(windows)]
async fn power_sleep() -> anyhow::Result<()> {
    // 注:若系统启用了休眠,SetSuspendState 会休眠而非睡眠(Windows 行为,真机调优项)。
    let mut c = tokio::process::Command::new("rundll32.exe");
    c.args(["powrprof.dll,SetSuspendState", "0,1,0"]);
    spawn_and_check(c, "睡眠").await
}
#[cfg(windows)]
async fn power_display_off() -> anyhow::Result<()> {
    // 息屏 = 给所有窗口广播 WM_SYSCOMMAND/SC_MONITORPOWER(关)。无干净 CLI → 走 windows crate。
    tokio::task::spawn_blocking(win_display_off).await?
}
#[cfg(windows)]
fn win_display_off() -> anyhow::Result<()> {
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        SendMessageW, HWND_BROADCAST, SC_MONITORPOWER, WM_SYSCOMMAND,
    };
    // HWND_BROADCAST 给所有顶层窗口;wParam=SC_MONITORPOWER,lParam=2(关屏,1=低功耗、-1=开)。
    unsafe {
        SendMessageW(
            HWND(HWND_BROADCAST.0),
            WM_SYSCOMMAND,
            Some(WPARAM(SC_MONITORPOWER as usize)),
            Some(LPARAM(2)),
        );
    }
    Ok(())
}
#[cfg(windows)]
async fn power_shutdown(restart: bool) -> anyhow::Result<()> {
    // OS 自带可取消倒计时:shutdown /s|/r /t 60;cancel = shutdown /a。
    let flag = if restart { "/r" } else { "/s" };
    let secs = SHUTDOWN_DELAY_SECS.to_string();
    let mut c = tokio::process::Command::new("shutdown");
    c.args([flag, "/t", &secs]);
    spawn_and_check(c, if restart { "重启" } else { "关机" }).await
}
#[cfg(windows)]
async fn power_cancel() -> anyhow::Result<()> {
    let mut c = tokio::process::Command::new("shutdown");
    c.arg("/a");
    spawn_and_check(c, "取消关机").await
}

// ── 其它系统:不支持 ──
#[cfg(all(not(target_os = "macos"), not(windows)))]
async fn power_lock() -> anyhow::Result<()> {
    anyhow::bail!("这个系统暂不支持电源控制")
}
#[cfg(all(not(target_os = "macos"), not(windows)))]
async fn power_sleep() -> anyhow::Result<()> {
    anyhow::bail!("这个系统暂不支持电源控制")
}
#[cfg(all(not(target_os = "macos"), not(windows)))]
async fn power_display_off() -> anyhow::Result<()> {
    anyhow::bail!("这个系统暂不支持电源控制")
}
#[cfg(all(not(target_os = "macos"), not(windows)))]
async fn power_shutdown(_restart: bool) -> anyhow::Result<()> {
    anyhow::bail!("这个系统暂不支持电源控制")
}
#[cfg(all(not(target_os = "macos"), not(windows)))]
async fn power_cancel() -> anyhow::Result<()> {
    anyhow::bail!("这个系统暂不支持电源控制")
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

    #[test]
    fn best_shortcut_match_prefers_exact_then_shortest_contains() {
        let entries = vec![
            ("卸载微信".to_string(), r"C:\a\卸载微信.lnk".to_string()),
            ("微信".to_string(), r"C:\a\微信.lnk".to_string()),
            ("Google Chrome".to_string(), r"C:\b\Google Chrome.lnk".to_string()),
        ];
        // 精确显示名优先,且不误中「卸载微信」
        assert_eq!(
            best_shortcut_match("微信", &entries).as_deref(),
            Some(r"C:\a\微信.lnk")
        );
        // 大小写不敏感 + 包含匹配:「chrome」→「Google Chrome」
        assert_eq!(
            best_shortcut_match("chrome", &entries).as_deref(),
            Some(r"C:\b\Google Chrome.lnk")
        );
        // 只有卸载器时,包含匹配也要跳过它(宁可回落 start 原名)
        let only_uninstall = vec![("卸载微信".to_string(), r"C:\a\卸载微信.lnk".to_string())];
        assert_eq!(best_shortcut_match("微信", &only_uninstall), None);
        // 完全无关 → None
        assert_eq!(best_shortcut_match("钉钉", &entries), None);
        assert_eq!(best_shortcut_match("  ", &entries), None);
    }
}
