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

    async fn run(&self, args: serde_json::Value, ctx: &ToolCtx) -> anyhow::Result<String> {
        let target = args
            .get("target")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(super::expand_home) // 「~/xxx」路径形宽容展开(§4.4;应用名/网址原样)
            .ok_or_else(|| anyhow::anyhow!("缺少 target 参数(要打开什么)"))?;
        // 文件/文件夹臂过授权圈(§7.2,= 读);网址/应用名不涉及
        let is_url = target.starts_with("http://") || target.starts_with("https://");
        if !is_url && looks_like_path(&target) {
            super::guard::ensure(ctx, super::guard::Access::Read, std::slice::from_ref(&target)).await?;
        }
        // **打开可执行文件 = 让 OS 运行它**(2026-08-22 审计 PARK-1,用户拍板 B + 可信任):
        // `open setup.exe` 在 Windows 上就是启动这个程序 —— 而下载夹/桌面出厂免授权,模型能
        // 「web_download 一个 exe → open 它」全程不问用户。所以打开可执行类文件先弹确认;
        // 但**可信任**:选「一直允许」就把这个文件路径记进信任清单,以后 open 它不再问
        // (用户「每次都打开也没必要」)。照片/PDF/文档/网址/打开应用一律照旧不问。
        if !is_url && looks_like_path(&target) && is_executable_file(&target) {
            confirm_open_executable(ctx, &target).await?;
        }
        open_target(&target).await?;
        // 结果是喂给模型的观察(不是 UI 文案),模型用当前人格的语言转述
        Ok(format!("已打开 {target}"))
    }
}

/// 可执行类文件扩展名(打开 = 运行,要过确认闸)。Windows 的 PATHEXT 家族 + 各平台脚本/安装包。
/// 大小写不敏感;判据是**扩展名**(跨平台一致,不看当前 OS —— NAS 上的 .exe 在 mac 端也该谨慎)。
const EXECUTABLE_EXTS: &[&str] = &[
    "exe", "bat", "cmd", "com", "scr", "msi", "msix", "ps1", "vbs", "vbe", "js", "jse", "wsf",
    "wsh", "cpl", "jar", "sh", "bash", "zsh", "command", "app", "run", "bin", "appimage", "pif",
    "reg", "lnk", "hta", "gadget",
];

/// target 是不是可执行类文件(按扩展名)。目录 / 无扩展名 / 普通文档 → false。
fn is_executable_file(target: &str) -> bool {
    std::path::Path::new(target)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .is_some_and(|e| EXECUTABLE_EXTS.contains(&e.as_str()))
}

/// settings key:已信任、可直接 open 的可执行文件绝对路径清单(app 级 JSON 数组)。
/// 同 `fs.scopes` 先例;走 settings 而非 keyring(不是秘密,是「用户点过一直允许」的记录)。
const TRUSTED_EXEC_KEY: &str = "desktop.trusted_exec";

fn load_trusted_exec(ctx: &ToolCtx) -> Vec<String> {
    ctx.store
        .settings
        .get(None, TRUSTED_EXEC_KEY)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
        .unwrap_or_default()
}

/// 打开可执行文件前的确认(可信任):已在清单 → 直接放行;否则弹确认闸,
/// 「一直允许」入清单、「仅这次」放行本次、拒/超时 = 观察退回(不静默,§3.5)。
async fn confirm_open_executable(ctx: &ToolCtx, target: &str) -> anyhow::Result<()> {
    // 规范化到绝对路径当清单键(与 open 真正交给 OS 的路径同源;canonicalize 失败退回原串)。
    let key = std::fs::canonicalize(target)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| target.to_string());
    if load_trusted_exec(ctx).iter().any(|t| t == &key) {
        return Ok(()); // 之前点过「一直允许」,信任它,不再问
    }
    let Some(confirmer) = ctx.confirm.as_ref() else {
        // 没有确认通道(headless/单测):可执行文件不能默认放行 —— 如实退回,别静默运行。
        anyhow::bail!("要运行可执行文件 {target},但现在没有可用的确认途径,这步没做");
    };
    let origin = ctx
        .store
        .chat
        .get_conversation(ctx.conv_id)
        .ok()
        .flatten()
        .map(|c| c.channel)
        .unwrap_or_else(|| "ui".into());
    let timeout = if matches!(origin.as_str(), "ui" | "system") {
        crate::confirm::DESKTOP_TIMEOUT
    } else {
        crate::confirm::CHANNEL_TIMEOUT
    };
    let decision = confirmer
        .ask(
            crate::confirm::ConfirmAsk {
                user_id: ctx.user_id,
                conv_id: ctx.conv_id,
                origin,
                host: String::new(),
                action: target.to_string(), // 页面/路径数据,非 core 文案(§6.6);前端按 kind 组动词
                kind: "open_exec".into(),
            },
            timeout,
        )
        .await;
    use crate::confirm::ConfirmDecision::*;
    match decision {
        Allowed { always: true, .. } => {
            // 记进信任清单(读改写;去重)——以后 open 这个文件不再问
            let mut list = load_trusted_exec(ctx);
            if !list.iter().any(|t| t == &key) {
                list.push(key);
                if let Ok(json) = serde_json::to_string(&list) {
                    let _ = ctx.store.settings.set(None, TRUSTED_EXEC_KEY, &json);
                }
            }
            Ok(())
        }
        Allowed { .. } => Ok(()), // 仅这次:开,但不记住
        Denied { via } if via == "unreachable" => {
            anyhow::bail!("要运行 {target} 需要确认,但确认没能送到用户那(渠道断线)——这步没做,如实说明")
        }
        Denied { .. } => {
            anyhow::bail!("用户没同意运行 {target},这步没做(不要绕路去运行它,如实告诉用户)")
        }
        TimedOut => anyhow::bail!("等运行 {target} 的确认超时了,这步没做"),
        NoUi => anyhow::bail!("要运行可执行文件 {target},但没有可用的确认途径,这步没做"),
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

/// 拼 `cmd /C` 后面那一整串(Windows 用;抽成平台无关纯函数才好在 Mac 上单测)。
///
/// ⚠️ **必须自己加引号,不能用 `Command::args([...])`(2026-08-22 修)**:Rust 的 Windows
/// 参数转义按 MSVCRT 规则,只在含空格/引号时才补引号 —— 而 `cmd.exe` 的解析器不吃那套,
/// 它先按 `& | ^ < >` 劈命令行。于是**不含空格但带 `&`** 的 target 会被劈成两条命令:
/// ① 安全洞(`notepad&calc` = 任意命令执行);② **日常正确性 bug** —— 带查询参数的普通
/// 网址(`?a=1&b=2`)在 Windows 上一直是从 `&` 处断开的。
/// 修 = 调用方用 `raw_arg` 绕开 Rust 的转义,由这里把 launch 包进双引号(引号内 cmd 对
/// `& | ^ < >` 一律按字面处理);`start` 的头一个 `""` 仍是占位窗口标题。
/// 记档:引号内 `%VAR%` 仍会展开,但**未定义**的变量原样保留 → 百分号编码的网址安全;
/// 名字里真带 `%环境变量%` 的文件是极冷门情形,不为它把 URL 编码弄坏。
#[cfg_attr(not(windows), allow(dead_code))] // 只有 Windows 臂用它;抽出来是为了能在 Mac 上单测
fn win_start_command_line(launch: &str) -> anyhow::Result<String> {
    // Windows 文件名本就不允许 `"` 与控制字符 → 出现即非常规,如实退回不硬拼(§3.5)。
    anyhow::ensure!(
        !launch.contains('"') && !launch.contains(['\r', '\n', '\0']),
        "这个名字/路径里有不该出现的字符(引号或换行),没法安全地交给系统打开:{launch}"
    );
    Ok(format!("start \"\" \"{launch}\""))
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
        let line = win_start_command_line(&launch)?;
        let mut c = tokio::process::Command::new("cmd");
        {
            use std::os::windows::process::CommandExt;
            let std_cmd = c.as_std_mut();
            std_cmd.raw_arg("/C");
            std_cmd.raw_arg(line);
        }
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

    /// **可执行类文件判定(2026-08-22 PARK-1)**:open 打开这些 = 让 OS 运行,要过确认闸。
    /// 判据是扩展名,跨平台一致(NAS 上的 .exe 在 mac 端也该谨慎),大小写不敏感。
    #[test]
    fn executable_files_are_recognized_across_platforms() {
        for p in [
            "/tmp/setup.exe",
            "D:\\dl\\installer.MSI",
            "/home/u/run.sh",
            "C:\\x\\a.bat",
            "/Applications/Foo.app",
            "~/x.command",
            "/tmp/x.AppImage",
            "C:\\a\\shortcut.lnk",
            "/tmp/macro.ps1",
        ] {
            assert!(is_executable_file(p), "{p} 应判为可执行(打开=运行,要确认)");
        }
        // 照片 / 文档 / 无扩展名 / 目录 —— 一律不是可执行,照旧不问
        for p in [
            "/tmp/photo.jpg",
            "D:\\单据\\发票.pdf",
            "~/Music/song.flac",
            "/home/u/notes.txt",
            "D:\\电影\\a.mkv",
            "/Users/x/Downloads", // 目录,无扩展名
            "照片/2024",
        ] {
            assert!(!is_executable_file(p), "{p} 不该被当可执行(会平白弹确认)");
        }
    }

    /// **cmd 命令行拼装(2026-08-22 修)**:target 恒被包进双引号 —— 这既堵住
    /// `notepad&calc` 那类注入,也修好「带 `&` 查询参数的网址在 Windows 被从 & 劈断」。
    #[test]
    fn win_start_line_quotes_target_against_cmd_metachars() {
        // 带 & 的网址(日常最常见的一种):必须整条在引号里,cmd 才不会劈开
        let line = win_start_command_line("https://x.com/a?b=1&c=2").unwrap();
        assert_eq!(line, "start \"\" \"https://x.com/a?b=1&c=2\"");
        // 注入形:同样被包住 → cmd 只会把它当一个要打开的名字,不会执行 calc
        let line = win_start_command_line("notepad&calc").unwrap();
        assert_eq!(line, "start \"\" \"notepad&calc\"");
        // 带空格的中文路径:占位标题 "" 保证它不被 start 当成窗口标题
        let line = win_start_command_line("D:\\我 的 电影\\a.mkv").unwrap();
        assert_eq!(line, "start \"\" \"D:\\我 的 电影\\a.mkv\"");
        // 引号 / 换行:Windows 文件名本就不允许 → 如实退回,不硬拼
        for bad in ["a\"&calc&\"b", "a\nstart calc", "a\r\nb"] {
            assert!(win_start_command_line(bad).is_err(), "{bad} 该被退回");
        }
    }

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
