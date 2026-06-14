# 在 Windows 上从源码编译 Larkwing

目标:在一台 **Windows 10/11(x64)** 机器上把 Larkwing 编译成安装包(`.exe` / `.msi`)。

> 想"一键三平台出包"而不折腾本机环境?直接用 GitHub Actions:`.github/workflows/release.yml`,在仓库 Actions 页面点 **Run workflow**(或打 `v*` tag),Windows / macOS / Linux 三平台的安装包都会产出。本文是给"我就要在自己的 Windows 上编"的场景。

---

## 1. 要装什么

| 组件 | 作用 | 必需 |
|------|------|------|
| **Visual Studio Build Tools 2022**(含 "使用 C++ 的桌面开发" 工作负载) | 提供 MSVC 编译器/链接器(`cl.exe`/`link.exe`)+ Windows SDK。`sherpa-onnx`、bundled SQLite、bzip2 的原生库都靠它链接 | ✅ |
| **Rust**(rustup,stable,默认 `x86_64-pc-windows-msvc`) | 编译 Rust 核心 | ✅ |
| **Node.js**(LTS，20+) | 跑前端构建(Vite + Vue) | ✅ |
| **pnpm**(10.x) | 前端包管理(仓库用 pnpm,锁文件 v9) | ✅ |
| **WebView2 Runtime** | Tauri 应用的渲染内核。Win11 与较新的 Win10 已自带;老系统需手动装 | ⚠️ 多数已自带 |
| **Git** | 拉代码 | ✅ |

> **不需要**:Python、CMake、手动编 ONNX Runtime。`sherpa-onnx` 在 Windows 上是**下载官方预编译静态库**(`win-x64-static-MT`),不在本机编译 C++。

---

## 2. 一键安装(winget,管理员 PowerShell)

```powershell
# C++ 工具链(关键:--add VCTools 工作负载,否则没有 MSVC 链接器 → sherpa/SQLite 链接失败)
winget install --id Microsoft.VisualStudio.2022.BuildTools -e `
  --override "--quiet --wait --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"

winget install --id Rustlang.Rustup -e
winget install --id OpenJS.NodeJS.LTS -e
winget install --id Microsoft.EdgeWebView2Runtime -e   # 多数系统已自带,装一次无害
winget install --id Git.Git -e
```

装完 **重开一个 PowerShell**(让 PATH 生效),再装 pnpm:

```powershell
npm install -g pnpm@10
# 或者:corepack enable pnpm
```

> 不想用 winget:VS Build Tools 也可去官网下"Build Tools for Visual Studio 2022"安装器,在勾选界面务必勾上 **"使用 C++ 的桌面开发"(Desktop development with C++)**。

验证环境:

```powershell
rustc --version      # 应显示 ...-pc-windows-msvc
node --version       # v20+
pnpm --version       # 10.x
```

---

## 3. 编译

```powershell
git clone git@github.com:wangshu923/larkwing.git
cd larkwing

pnpm install          # 装前端依赖
pnpm tauri build      # 编译 + 打包(首次较久:要下 sherpa 预编译库 + 编 Rust)
```

产物:

```
target\release\bundle\nsis\*.exe     # NSIS 安装器(推荐分发用)
target\release\bundle\msi\*.msi      # MSI 安装包
```

> 仓库里的 [`.cargo/config.toml`](../.cargo/config.toml) 已为 `windows-msvc` 开了 `+crt-static`(静态 CRT），用来匹配 sherpa 的 `static-MT` 预编译库——**不要删**,否则会撞 `LNK2038` CRT 不匹配错误。

---

## 4. 联网说明

- **编译期**:首次 `pnpm tauri build` 会从 **GitHub** 下载 `sherpa-onnx` 的 Windows 预编译库(~ 数十 MB),WiX / NSIS 打包器也从 GitHub 下。国内直连 GitHub 常超时(`Connection Failed ... os error 10060`)。设环境变量让整个构建走本地代理(`sherpa-onnx-sys` 用 ureq、Tauri 用 reqwest,都读这些变量):

  > ⚠️ **必须用 `http://` 协议头**:sherpa 依赖的 ureq 没编 `socks-proxy`,纯 `socks5://` 用不了。Clash/v2ray 的混合端口或 HTTP 端口用 `http://` 即可。

  Git Bash(MINGW):
  ```bash
  export HTTPS_PROXY=http://127.0.0.1:7890
  export HTTP_PROXY=http://127.0.0.1:7890
  export ALL_PROXY=http://127.0.0.1:7890
  pnpm tauri build
  ```
  PowerShell:
  ```powershell
  $env:HTTPS_PROXY="http://127.0.0.1:7890"; $env:HTTP_PROXY="http://127.0.0.1:7890"; $env:ALL_PROXY="http://127.0.0.1:7890"
  pnpm tauri build
  ```
  端口换成你自己的;环境变量只在**当前终端窗口**有效,要和 build 在同一个窗口。
- **离线 / 代理仍不通**:手动下好 sherpa 包再指给构建,**文件名必须原样**:
  ```bash
  mkdir -p /c/sherpa-cache
  curl -x http://127.0.0.1:7890 -L -o /c/sherpa-cache/sherpa-onnx-v1.13.2-win-x64-static-MT-Release-lib.tar.bz2 \
    https://github.com/k2-fsa/sherpa-onnx/releases/download/v1.13.2/sherpa-onnx-v1.13.2-win-x64-static-MT-Release-lib.tar.bz2
  export SHERPA_ONNX_ARCHIVE_DIR='C:/sherpa-cache'   # 给 Rust 读,用 C:/ 形式,不是 /c/
  pnpm tauri build
  ```
  这只解决 sherpa;WiX/NSIS 仍要联网,所以优先用上面的全局代理。
- **运行期**:首次运行 App 才会按需下载 yt-dlp / ffmpeg / 语音模型到用户数据目录 —— 与编译无关,安装包里不含它们。

---

## 5. 排错

| 症状 | 原因 / 处置 |
|------|------|
| `LNK2038: mismatch detected ... 'MT_StaticRelease' vs 'MD_DynamicRelease'` | `.cargo/config.toml` 的 `+crt-static` 没生效(被删/没在仓库根/用了别的 target)。确认文件在、target 是 `x86_64-pc-windows-msvc` |
| `error: linker 'link.exe' not found` / 找不到 MSVC | VS Build Tools 没装,或没勾 "使用 C++ 的桌面开发" 工作负载。重装时务必勾上 |
| 下载 sherpa 库超时 / 失败 | 设 `HTTPS_PROXY`(见上),或用 `SHERPA_ONNX_ARCHIVE_DIR` 预置离线包 |
| App 启动白屏 / 报缺 WebView2 | 装 `Microsoft.EdgeWebView2Runtime` |

---

## 附:备选方案(仅当 `+crt-static` 下 Tauri 编不过时)

`+crt-static`(全静态)是当前选择,改动最小。万一某个 Tauri 依赖在静态 CRT 下编不过,备选是让 sherpa 走**动态库(shared)**模式,反过来匹配 Tauri 默认的 `/MD`:

1. `larkwing-core/Cargo.toml` 里把 `sherpa-onnx` 改为按平台分化的依赖,Windows 用 `shared`:
   ```toml
   [target.'cfg(not(windows))'.dependencies]
   sherpa-onnx = "1.13.2"                                          # mac/linux 维持 static

   [target.'cfg(windows)'.dependencies]
   sherpa-onnx = { version = "1.13.2", default-features = false, features = ["shared"] }
   ```
2. 删掉 `.cargo/config.toml` 里的 `+crt-static`。
3. `shared` 模式下 `sherpa-onnx-sys` 的构建脚本会把 `onnxruntime.dll` 等运行时 DLL 拷到输出目录,需把它们作为 Tauri 资源一起打包(`src-tauri/tauri.conf.json` 的 `bundle.resources`,或随 exe 同目录分发)。

> 这条路要动核心依赖配置且需要打包 DLL,所以只在 `+crt-static` 确实走不通时再用。
