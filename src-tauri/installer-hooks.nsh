; Larkwing — NSIS 安装器卸载钩子(开机自启残留清理)
;
; 为什么需要(见 AGENT.md §6.8「防漂移」/ §7.6 常驻临场):
;   开机自启由 tauri-plugin-autostart(底层 auto-launch 0.5.0)在「运行时」写注册表——
;   用户在设置里打开自启时才写,不是安装器装进去的。NSIS / MSI 卸载器只清自己装过的东西,
;   不认这条运行时写入 → 卸载后会残留一条孤儿启动项,指向已被删掉的 exe。
;
; auto-launch 在 Windows 上写「两处」,值名都是 app_name = package_info().name = 产品名 "larkwing"
; (项目用默认的 tauri_plugin_autostart::init(),未自定义名;productName 与 crate name 均为 larkwing):
;   1) ...\CurrentVersion\Run                            ← 实际启动项(值 = "exe路径" --autostart)
;   2) ...\Explorer\StartupApproved\Run                  ← 任务管理器「已启用」覆盖位(12 字节 blob)
; 两条都要删,否则第二条同样成孤儿。DeleteRegValue 找不到值不报错 → 从没开过自启也安全。
;
; ⚠️ 只在「真卸载」时删,「升级」时保留(2026-06-19):用户走「新包提示卸载旧版」= Tauri 升级流程,
;   它会用 /UPDATE 调旧卸载器并置 $UpdateMode=1。若此处无条件删,升级会把用户的自启状态冲掉
;   (而 app 侧「首启已默认」标记又跨升级残留,新版不会补开)→ 升级后自启莫名变关(实测 bug)。
;   故用 ${If} $UpdateMode <> 1 守卫:升级不删(自启延续)、真卸载才删(免孤儿)。
;   ($UpdateMode 由 Tauri NSIS 模板在 un.onInit 解析命令行设好,本宏插入卸载 Section 内可直接读;
;    模板已 !include LogicLib.nsh,${If} 可用。)
;   ⚠️ 旧版本编进去的卸载器没有这条守卫 → 本修复只护「以后」的升级;「当前版→修好版」那一跳仍会
;   被旧卸载器删一次,靠 app 侧首启「一次性重开默认」补回(见 src/lib.rs 的 .v2 标记)。
;
; 前提:bundle.windows.nsis.installMode = currentUser(见 tauri.conf.json),卸载器以当前用户身份运行,
;   HKCU 命中的正是当初 auto-launch 写入的那个用户配置单元。值名大小写不敏感(RegDeleteValue 语义)。
;
; ⚠️ 仅能在 Windows 真机验(Mac/Linux 不出 NSIS;属 AGENT.md §8.1 真机验收单)。
;   若日后改 productName,这里的值名 "larkwing" 要跟着改。

!macro NSIS_HOOK_POSTUNINSTALL
  ; 升级(/UPDATE → $UpdateMode=1)时保留自启项,真卸载才清(见文件头说明)。
  ${If} $UpdateMode <> 1
    DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "larkwing"
    DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run" "larkwing"
  ${EndIf}
!macroend
