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
; 前提:bundle.windows.nsis.installMode = currentUser(见 tauri.conf.json),卸载器以当前用户身份运行,
;   HKCU 命中的正是当初 auto-launch 写入的那个用户配置单元。值名大小写不敏感(RegDeleteValue 语义)。
;
; ⚠️ 仅能在 Windows 真机验(Mac/Linux 不出 NSIS;属 AGENT.md §8.1 真机验收单)。
;   若日后改 productName,这里的值名 "larkwing" 要跟着改。

!macro NSIS_HOOK_POSTUNINSTALL
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "larkwing"
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run" "larkwing"
!macroend
