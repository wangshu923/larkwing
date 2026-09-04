//! 能力轴:开机启动项(Windows)。「开机好慢」的另一半手脚(system_status 管当下、这里管
//! 开机):`startup_list` 列出开机自启的程序 + 启用/禁用状态;`startup_toggle` 禁用/恢复
//! 某一项。**机制 = 与任务管理器同一套**:程序本体(注册表 Run 键的值 / 启动文件夹里的
//! 快捷方式)一个字节不动,启停走 `Explorer\StartupApproved\{Run,StartupFolder}` 标志位——
//! 任务管理器写的正是它,所以互相可见、天然可逆(Larkwing 卸载钩子操作的也是这两个键族)。
//!
//! 边界:一期只**改**当前用户的两类(HKCU Run + 用户启动文件夹);系统级(HKLM Run /
//! 公共启动文件夹)**只列不改**——写 HKLM 要管理员,如实指路任务管理器。计划任务不碰
//! (枚举输出本地化、面太杂,真机发现覆盖不够再议)。纪律在「电脑清理」技能里:禁用哪个
//! 要用户点名,绝不自作主张批量禁。
//!
//! 平台:仅 Windows 有实现;mac/Linux 调用如实退回(§3.5,mac 是开发机 §4.9)。
//! StartupApproved 字节格式的判断/生成抽成平台无关纯函数,mac 上可单测(desktop.rs
//! `win_start_command_line` 同款手法);真机验收项见 PLAN。

use async_trait::async_trait;

use super::{Tool, ToolCtx, ToolSpec};

pub(super) struct StartupList {
    spec: ToolSpec,
}

impl StartupList {
    pub(super) fn new() -> StartupList {
        StartupList {
            spec: ToolSpec {
                name: "startup_list",
                description: "列出这台电脑开机自动启动的程序(名字、现在是启用还是已禁用、\
                              来自注册表还是启动文件夹、对应的命令/路径)。用户说「开机好慢 /\
                              这电脑一开机就卡」时用它摸底,把占开机时间的项列给用户看。\
                              只是看,不改;要禁用/恢复某一项用 startup_toggle,且必须用户\
                              点名同意哪个才动哪个。仅 Windows 电脑可用。",
                parameters: serde_json::json!({ "type": "object", "properties": {} }),
                timeout: std::time::Duration::from_secs(15),
                ui_key: "tool.startup_list",
            },
        }
    }
}

#[async_trait]
impl Tool for StartupList {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, _args: serde_json::Value, _ctx: &ToolCtx) -> anyhow::Result<String> {
        #[cfg(windows)]
        {
            return tokio::task::spawn_blocking(win::list_report).await?;
        }
        #[cfg(not(windows))]
        anyhow::bail!("这台电脑不是 Windows,看不了开机启动项(如实告诉用户)");
    }
}

pub(super) struct StartupToggle {
    spec: ToolSpec,
}

impl StartupToggle {
    pub(super) fn new() -> StartupToggle {
        StartupToggle {
            spec: ToolSpec {
                name: "startup_toggle",
                description: "禁用或恢复一个开机启动项(名字用 startup_list 列出来的那个)。\
                              走系统任务管理器同一套机制:程序本身一个字节不动、随时可恢复,\
                              下次开机生效。**必须用户明确点名要禁哪个才调**——先 startup_list\
                              把清单给用户看、用户说了「把某某关掉」再动手;绝不自作主张批量禁。\
                              系统级的项(注册表·所有用户 / 公共启动文件夹)这里改不了,\
                              如实告诉用户去任务管理器里改。仅 Windows 电脑可用。",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "启动项的名字(startup_list 结果里的那个名字,原样传)"
                        },
                        "enable": {
                            "type": "boolean",
                            "description": "false = 禁用(不再开机自启),true = 恢复启用"
                        }
                    },
                    "required": ["name", "enable"]
                }),
                timeout: std::time::Duration::from_secs(15),
                ui_key: "tool.startup_toggle",
            },
        }
    }
}

#[async_trait]
impl Tool for StartupToggle {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn run(&self, args: serde_json::Value, _ctx: &ToolCtx) -> anyhow::Result<String> {
        let name = args
            .get("name")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!("缺少 name 参数(要动哪个启动项)"))?
            .to_string();
        let enable = super::arg_bool(&args, "enable", true);
        let _ = (&name, enable);
        #[cfg(windows)]
        {
            return tokio::task::spawn_blocking(move || win::toggle(&name, enable)).await?;
        }
        #[cfg(not(windows))]
        anyhow::bail!("这台电脑不是 Windows,改不了开机启动项(如实告诉用户)");
    }
}

// ───────────────── StartupApproved 字节格式(平台无关纯函数,mac 可测)─────────────────

/// 任务管理器的启停标志位:REG_BINARY,首 DWORD 偶数(02/06)= 启用、奇数(03/07)= 禁用,
/// 后 8 字节 = 禁用时刻 FILETIME(启用时全 0)。值缺失 = 启用(从没被禁过)。
#[cfg_attr(not(windows), allow(dead_code))]
fn approved_is_enabled(bytes: Option<&[u8]>) -> bool {
    match bytes.and_then(|b| b.first()) {
        None => true,
        Some(b) => b & 1 == 0,
    }
}

/// 生成要写入 StartupApproved 的 12 字节(FILETIME 写 0:任务管理器只认首 DWORD 的奇偶,
/// 时间戳只影响它显示「禁用日期」,写 0 显示为空,无害)。
#[cfg_attr(not(windows), allow(dead_code))]
fn approved_bytes(enable: bool) -> [u8; 12] {
    let mut b = [0u8; 12];
    b[0] = if enable { 0x02 } else { 0x03 };
    b
}

/// 名字匹配(大小写不敏感精确;列表名带 .lnk 时也认去掉扩展名的叫法——用户嘴里
/// 说的是「钉钉」,文件叫「钉钉.lnk」)。
#[cfg_attr(not(windows), allow(dead_code))]
fn name_matches(item_name: &str, asked: &str) -> bool {
    let (a, b) = (item_name.trim().to_lowercase(), asked.trim().to_lowercase());
    if a == b {
        return true;
    }
    // 「钉钉.lnk」 vs 「钉钉」
    std::path::Path::new(&a)
        .file_stem()
        .map(|s| s.to_string_lossy() == b)
        .unwrap_or(false)
}

// ───────────────────────────── Windows 实现 ─────────────────────────────

#[cfg(windows)]
mod win {
    use super::{approved_bytes, approved_is_enabled, name_matches};
    use anyhow::{Context, Result};
    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, REG_BINARY};
    use winreg::{RegKey, RegValue};

    const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const APPROVED_RUN: &str =
        r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";
    const APPROVED_FOLDER: &str =
        r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\StartupFolder";
    /// 32 位程序在 64 位系统上的 Run 键(注册表重定向视图)与它对应的启停标志键——任务管理器
    /// 「启动应用」把这一栏也列出来,漏掉它就少一截清单(复审实锤)。
    const RUN32: &str = r"Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Run";
    const APPROVED_RUN32: &str =
        r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run32";

    /// 一条启动项:名字 / 来源 / 命令 / 启停。
    pub(super) struct Item {
        pub name: String,
        /// hkcu_run | hklm_run | hklm_run32 | user_folder | common_folder
        pub source: &'static str,
        pub command: String,
        pub enabled: bool,
        /// 这边能不能改(当前用户的两类 = 能;系统级 = 不能,指路任务管理器)。
        pub togglable: bool,
    }

    fn source_label(s: &str) -> &'static str {
        match s {
            "hkcu_run" => "注册表·当前用户",
            "hklm_run" => "注册表·所有用户",
            "hklm_run32" => "注册表·所有用户(32 位)",
            "user_folder" => "启动文件夹",
            _ => "公共启动文件夹",
        }
    }

    /// 读一个 StartupApproved 键里某值的启停(键/值缺失 = 启用)。
    fn approved_state(root: &RegKey, key: &str, name: &str) -> bool {
        let Ok(k) = root.open_subkey_with_flags(key, KEY_READ) else { return true };
        match k.get_raw_value(name) {
            Ok(v) => approved_is_enabled(Some(&v.bytes)),
            Err(_) => true,
        }
    }

    /// 枚举一个 Run 键(值名 = 项名,值 = 命令行);`approved` = 它对应的启停标志键。
    fn run_items(
        root: &RegKey,
        run_key: &str,
        approved: &str,
        source: &'static str,
        togglable: bool,
    ) -> Vec<Item> {
        let Ok(run) = root.open_subkey_with_flags(run_key, KEY_READ) else { return Vec::new() };
        let mut out = Vec::new();
        for entry in run.enum_values().flatten() {
            let (name, _) = entry;
            let command: String = run.get_value(&name).unwrap_or_default();
            let enabled = approved_state(root, approved, &name);
            out.push(Item { name, source, command, enabled, togglable });
        }
        out
    }

    /// 启动文件夹路径(user = %APPDATA%、common = %ProgramData% 下的固定子路径)。
    fn folder(user: bool) -> Option<std::path::PathBuf> {
        let base = std::env::var_os(if user { "APPDATA" } else { "ProgramData" })?;
        Some(
            std::path::Path::new(&base)
                .join(r"Microsoft\Windows\Start Menu\Programs\Startup"),
        )
    }

    /// 枚举一个启动文件夹(项名 = 文件名含扩展;desktop.ini 不算)。
    fn folder_items(user: bool) -> Vec<Item> {
        let Some(dir) = folder(user) else { return Vec::new() };
        let Ok(rd) = std::fs::read_dir(&dir) else { return Vec::new() };
        let hive = RegKey::predef(if user { HKEY_CURRENT_USER } else { HKEY_LOCAL_MACHINE });
        let mut out = Vec::new();
        for e in rd.flatten() {
            let p = e.path();
            if !p.is_file() {
                continue;
            }
            let name = e.file_name().to_string_lossy().into_owned();
            if name.eq_ignore_ascii_case("desktop.ini") {
                continue;
            }
            let enabled = approved_state(&hive, APPROVED_FOLDER, &name);
            out.push(Item {
                name,
                source: if user { "user_folder" } else { "common_folder" },
                command: p.to_string_lossy().into_owned(),
                enabled,
                togglable: user,
            });
        }
        out
    }

    pub(super) fn collect() -> Vec<Item> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
        let mut items = run_items(&hkcu, RUN, APPROVED_RUN, "hkcu_run", true);
        items.extend(run_items(&hklm, RUN, APPROVED_RUN, "hklm_run", false));
        items.extend(run_items(&hklm, RUN32, APPROVED_RUN32, "hklm_run32", false));
        items.extend(folder_items(true));
        items.extend(folder_items(false));
        items
    }

    /// startup_list 的报告文本(喂模型的观察)。
    pub(super) fn list_report() -> Result<String> {
        let items = collect();
        if items.is_empty() {
            return Ok("没有找到开机启动项(注册表 Run 键与启动文件夹都是空的)".into());
        }
        let mut out = format!("开机启动项 {} 个:\n", items.len());
        let mut has_system = false;
        for it in &items {
            has_system |= !it.togglable;
            out.push_str(&format!(
                "- {} —— {}({}){}\n  {}\n",
                it.name,
                if it.enabled { "启用" } else { "已禁用" },
                source_label(it.source),
                if it.togglable { "" } else { "【系统级,这边只能看】" },
                it.command
            ));
        }
        if has_system {
            out.push_str("系统级的项要改得去任务管理器·启动应用;其余的可以用 startup_toggle 禁用/恢复(用户点名才动)。");
        }
        Ok(out.trim_end().to_string())
    }

    /// startup_toggle 的执行:只动当前用户的两类;写 StartupApproved 标志位。
    pub(super) fn toggle(name: &str, enable: bool) -> Result<String> {
        let items = collect();
        let hits: Vec<&Item> = items.iter().filter(|i| name_matches(&i.name, name)).collect();
        let it = match hits.as_slice() {
            [] => {
                let known: Vec<&str> = items.iter().map(|i| i.name.as_str()).collect();
                anyhow::bail!(
                    "没有叫「{name}」的启动项。现有的:{}(用 startup_list 里的原名)",
                    known.join("、")
                );
            }
            [one] => *one,
            many => {
                // 同名多处(HKCU Run 的「钉钉」与启动文件夹的「钉钉.lnk」都认「钉钉」):
                // 能改的只有一处就动它;否则如实报出各处,让用户用带扩展名的原名点名,别猜着关一个
                let mut togglable = many.iter().filter(|i| i.togglable);
                match (togglable.next(), togglable.next()) {
                    (Some(one), None) => *one,
                    _ => anyhow::bail!(
                        "有 {} 个启动项都叫「{name}」:{}——说清是哪一个(用 startup_list 里带扩展名的原名)",
                        many.len(),
                        many.iter()
                            .map(|i| format!("{}〔{}〕", i.name, source_label(i.source)))
                            .collect::<Vec<_>>()
                            .join("、")
                    ),
                }
            }
        };
        if !it.togglable {
            anyhow::bail!(
                "「{}」是系统级启动项({}),这边改不了——请用户在 任务管理器 → 启动应用 里改",
                it.name,
                source_label(it.source)
            );
        }
        if it.enabled == enable {
            return Ok(format!(
                "「{}」本来就是{}状态,没动",
                it.name,
                if enable { "启用" } else { "禁用" }
            ));
        }
        let approved_key = match it.source {
            "hkcu_run" => APPROVED_RUN,
            _ => APPROVED_FOLDER, // user_folder(togglable 的只有这两类)
        };
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let (k, _) = hkcu
            .create_subkey(approved_key)
            .with_context(|| format!("打不开启停标志键 {approved_key}"))?;
        k.set_raw_value(
            &it.name,
            &RegValue { bytes: approved_bytes(enable).to_vec(), vtype: REG_BINARY },
        )
        .with_context(|| format!("写「{}」的启停标志失败", it.name))?;
        Ok(if enable {
            format!("已恢复「{}」的开机自启(下次开机生效)", it.name)
        } else {
            format!("已禁用「{}」的开机自启(下次开机生效;程序本体没动,随时可恢复)", it.name)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approved_bytes_roundtrip_and_task_manager_variants() {
        // 我们写的格式,自己读回来一致
        assert!(approved_is_enabled(Some(&approved_bytes(true))));
        assert!(!approved_is_enabled(Some(&approved_bytes(false))));
        // 任务管理器写过的各种首 DWORD:偶 = 启用、奇 = 禁用(见过 02/03/06/07)
        assert!(approved_is_enabled(Some(&[0x06, 0, 0, 0])));
        assert!(!approved_is_enabled(Some(&[0x07, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8])));
        // 值缺失 = 从没被禁过 = 启用
        assert!(approved_is_enabled(None));
        assert!(approved_is_enabled(Some(&[])));
    }

    #[test]
    fn name_matching_tolerates_lnk_extension_and_case() {
        assert!(name_matches("WeChat", "wechat"));
        assert!(name_matches("钉钉.lnk", "钉钉"), "用户嘴里不带 .lnk");
        assert!(name_matches("钉钉.lnk", "钉钉.LNK"));
        assert!(!name_matches("钉钉.lnk", "钉"), "不做子串猜测——名对不上如实退回带清单");
    }

    #[tokio::test]
    async fn non_windows_refuses_honestly() {
        #[cfg(not(windows))]
        {
            let store = {
                let dir =
                    std::env::temp_dir().join(format!("lw-startup-{}", std::process::id()));
                std::fs::create_dir_all(&dir).unwrap();
                let _ = std::fs::remove_file(dir.join("t.db"));
                crate::store::Store::open(&dir.join("t.db")).unwrap()
            };
            let me = store.users.ensure_default_user().unwrap();
            let ctx = ToolCtx {
                user_id: me.id,
                conv_id: 1,
                media: crate::media::MediaRuntime::detached(store.clone()),
                store,
                web: None,
                voice: None,
                confirm: None,
                grants: Default::default(),
                agent: None,
            };
            let e = StartupList::new().run(serde_json::json!({}), &ctx).await.unwrap_err();
            assert!(e.to_string().contains("Windows"), "{e:#}");
            let e = StartupToggle::new()
                .run(serde_json::json!({"name": "x", "enable": false}), &ctx)
                .await
                .unwrap_err();
            assert!(e.to_string().contains("Windows"), "{e:#}");
        }
    }
}
