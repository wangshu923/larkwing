# Larkwing

> 面向普通人 / 家庭的桌面 AI 助手 —— Rust 核心 + Tauri 壳。

开窗就能看见它、打字就能聊、关了再开它还记得你。
不同于开发者向、强调自由配置的同类,Larkwing **强默认、开箱即用、收口** —— 用户只面对一个助手,不碰 agent / 插件 / prompt / 配置。

底座**人格中立**:引擎、回合循环、工具、记忆、I/O 都不内嵌具体人格;名字、性格、外观是可换的**场景数据 + 皮肤**(英文代号 Larkwing;默认观感科幻、默认称呼 7274,暖萌皮「旺财」等为后续可选)。

---

## 技术栈

| 层 | 选型 |
| --- | --- |
| 壳 | **Tauri v2**(Rust + WebView,小体积;不用 Electron) |
| 核心 | **Rust**(`tokio` 异步,单 crate `larkwing-core`) |
| 前端 | **Vue 3 + TypeScript**,MVVM(不用 vanilla JS) |
| 存储 | **SQLite**(记忆 / 历史 / 用户) |
| LLM | **DeepSeek 优先**(OpenAI 兼容,流式 SSE,自动前缀缓存);trait 化,多供应商 |
| 语音 | **sherpa-onnx**(ASR / VAD / 唤醒 / 声纹)+ msedge-tts / 本地 VITS;cpal 采集 |
| 影音 | **yt-dlp** 解析 + ffmpeg 混流 + localhost 中继(用时下载,不打包) |

> 目标平台 = **Windows**(家里那台机器);**Mac 上开发迭代,最终出 Windows 包**。离线非目标,不打包 Python。

---

## 能力现状

后端 MVP 与多个能力期已落地,核心 **133 测试全绿**:

- **流式对话 + 持久记忆** —— LLM 流式输出 + SQLite 记忆,关了再开仍在
- **Agent / 工具运行时** —— 通用回合循环 + `Tool` trait + 工具白名单 / few-shot + 双方言(OpenAI / Anthropic 风格)工具协议
- **影音** —— 任务进度总线 / HUD + media 三工具(搜 / 放 / 控)+ 扫码登录取 cookie + 音频直转 / 视频混流
- **任务需知** —— briefings 域 + 三工具(环境知识按需注入,与人格解耦)
- **工具批次** —— jobs 域 + 调度器 + 自启回合 + web 二件套(搜索即抓取)
- **语音 I/O** —— 按住说话 / 开口回应 / 免手唤醒 / 本地离线 TTS + mic watchdog

**已备未启用**(core 完成、测试绿,暂未接 UI):多用户、声纹识别、家人 CRUD;本地文件 fs 原语。

---

## 项目结构

```
larkwing/
├─ src/                  前端(Vue 3 + TS;View + composables 作 ViewModel)
│  ├─ views/             ChatView / MemoryView / SettingsView
│  ├─ components/        形象 / HUD / 播放器 / 任务浮层 / 各皮肤背景…
│  ├─ composables/       useChat / useVoice / useMedia / useTasks …(ViewModel)
│  ├─ lib/               backend.ts(Tauri commands / events 封装)、fmt.ts
│  └─ i18n.ts, locales/  前端文案字典(人格 / 皮肤文案只在前端)
│
├─ larkwing-core/src/    Rust 引擎(单 crate,tokio 异步)
│  ├─ engine/            通用回合循环(turn)、上下文装配、用量统计
│  ├─ llm/               LlmProvider:openai_compat / anthropic_compat / 目录 / SSE
│  ├─ store/             SQLite:记忆 / 历史 / 设置 / 用量 / 任务 / 需知 / 用户 / 声纹
│  ├─ tools/             Tool trait:now / remember / reminder / web / media* / fs / briefing
│  ├─ voice/             ASR / VAD / 唤醒 / 声纹 / TTS
│  ├─ media/             影音:bilibili 源 / 解析 / cookie / 中继
│  ├─ scenes.rs          场景 = 数据(人格 + 开场白 + 工具白名单 + few-shot)
│  ├─ scheduler.rs       工具批次调度器        tasks.rs   任务 / job 域
│  ├─ bus.rs             事件总线(进度 / HUD)  components.rs  用时下载的外部组件
│
├─ src-tauri/            Tauri v2 壳(commands.rs 暴露给前端、lib.rs、main.rs)
└─ scripts/              资产生成脚本(如形象帧)
```

---

## 开发

### 前置

- **Rust**(stable,≥ 1.77.2)
- **Node.js** + **pnpm**
- **Tauri v2 系统依赖**:macOS 装 Xcode Command Line Tools;Linux 见 [`.github/workflows/release.yml`](.github/workflows/release.yml) 里的 apt 清单;Windows 本地编译见 [docs/BUILD-WINDOWS.md](docs/BUILD-WINDOWS.md)(VS Build Tools + WebView2 等,附 winget 一键)

### 跑起来

```bash
pnpm install          # 装前端依赖

pnpm tauri dev        # 启动桌面 App(Rust + WebView,首次会编译 Rust,稍久)
pnpm dev              # 仅前端(Vite @ :1420,纯视觉迭代用,浏览器里看)

cargo test            # Rust 核心测试(workspace 全量)
```

### 出包

```bash
pnpm tauri build      # 在当前平台出当前平台的包(产物在 target/release/bundle/)
```

> ⚠️ **不能跨平台出包**:Mac 上编不出 Windows 包(Tauri 不支持跨平台打包,且语音栈 sherpa-onnx 的预编译库按平台分发)。出 Windows 包走以下任一:
> - **(a) GitHub Actions** —— [`.github/workflows/release.yml`](.github/workflows/release.yml),Actions 页面手动 Run 或打 `v*` tag,一次出 Windows/macOS/Linux 三平台。
> - **(b) 一台 Windows 机器** —— 按 [docs/BUILD-WINDOWS.md](docs/BUILD-WINDOWS.md) 装好后 `pnpm tauri build`。

> **首次运行会按需下载外部组件**(yt-dlp / ffmpeg / 语音模型)到数据目录 —— 安装包里不含它们,性质同浏览器下载文件。

---

## 配置

- **LLM key**:唯一不可免的首次设置。首次在设置里友好地填一次 DeepSeek API key,或读环境变量。
- key / 接入点支持 `${ENV}` 引用(明文或环境变量随用户,取值时解析、存储留原文)。
- "用脑策略"三档(省着用 / 均衡 / 聪明优先)对应内部路由,用户不见模型细节。

---

## 设计原则

- **人格中立底座** —— 引擎 / 回合循环 / 工具 / 事件 / UI 基建一律人格中立;人格只从**场景数据**与**皮肤层**进入。换一套场景数据 + 皮肤 = 另一个助手,底座零改动。
- **通用回合循环** —— 没有意图分类器、没有 per-task workflow;任务路由 = 模型本身,工具按能力轴做正交原语。加能力 = 加一个工具文件或一份场景数据,循环不改。
- **场景 / 人格 / 皮肤 = 数据**,不是代码插件;core 只给文案 key,渲染在前端字典。
