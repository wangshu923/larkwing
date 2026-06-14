# Larkwing(旺财)

> 面向普通人 / 家庭的**暖萌陪伴型桌面 AI 助手**,用 Rust 全新重写。

Larkwing 是"旺财"的桌面版:开窗就能看见它、打字就能聊、关了再开它还记得你。
和开发者向、强调自由配置的老项目不同,Larkwing **强默认、开箱即用、收口** —— 用户只面对一个助手,不碰 agent / 插件 / prompt / 配置。

- **项目英文名**:Larkwing　**面向用户中文名**:旺财(暖萌皮)
- **默认助手名**:7274(科幻调性)　**唤醒词**:小七
- **调性**:科幻优先(玻璃 / 辉光 / HUD),性格是亲和陪伴;暖萌为后续可选皮肤

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

## 现状

后端 MVP 与多个能力期已落地,核心 **133 测试全绿**:

- ✅ **对话闭环** —— DeepSeek 流式 + SQLite 记忆 + "闲聊陪伴"人格,关了再开还记得你
- ✅ **Agent / 工具运行时** —— 通用回合循环 + `Tool` trait + 场景白名单 / few-shot + 双方言工具协议
- ✅ **影音一期** —— 任务进度总线 / HUD + media 三工具(搜 / 放 / 控)+ B 站扫码登录 + 音频直转 / 视频混流
- ✅ **任务需知机制** —— briefings 域 + 三工具;法条搬进底座,场景从此纯性格
- ✅ **工具批次** —— jobs 域 + 调度器 + 自启回合(提醒三件套)+ web 二件套(搜索即抓取)
- ✅ **语音** —— A 按住说话 / B 开口回应 / C 免手唤醒「小七」 / D 本地 TTS 离线档 + mic watchdog

**已备未启用**(core 写完测试绿,暂不接 UI):多用户、CAM++ 声纹识别、家人 CRUD;本地文件 fs 原语。

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
- **Tauri v2 系统依赖**:macOS 装 Xcode Command Line Tools;出 Windows 包需 WebView2 + MSVC(见 [Tauri 文档](https://tauri.app/start/prerequisites/))

### 跑起来

```bash
pnpm install          # 装前端依赖

pnpm tauri dev        # 启动桌面 App(Rust + WebView,首次会编译 Rust,稍久)
pnpm dev              # 仅前端(Vite @ :1420,纯视觉迭代用,浏览器里看)

cargo test            # Rust 核心测试(workspace 全量)
```

### 出包

```bash
pnpm tauri build      # 桌面安装包(在 Windows 上构建即出 Windows 包)
```

> **首次运行会按需下载外部组件**(yt-dlp / ffmpeg / 语音模型)到数据目录 —— 安装包里不含它们,性质同浏览器下载文件。

---

## 配置

- **LLM key**:唯一不可免的首次设置。首次在设置里友好地填一次 DeepSeek API key,或读环境变量。
- key / 接入点支持 `${ENV}` 引用(明文或环境变量随用户,取值时解析、存储留原文)。
- "用脑策略"三档(省着用 / 均衡 / 聪明优先)对应内部路由,用户不见模型细节。

---

## 约定

- **先想清楚架构 / 方案 → 跟用户确认范围 → 再写代码**。
- **人格中立底座**:引擎 / 回合循环 / 工具 / 事件 / UI 基建一律人格中立;人格只从**场景数据**与**皮肤层**进入。换一套场景数据 + 皮肤 = 另一个助手,底座零改动。
- **场景 / 人格 / 皮肤 = 数据**,不是代码插件;core 只给文案 key,渲染在前端字典。
