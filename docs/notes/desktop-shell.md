# 壳层 / 窗控 / 托盘 / 平台陷阱 / UI 陷阱 — 决策记录与破案叙事

> 本文件是 2026-09-07 AGENT.md「分家」时从规则总纲搬出的段落,**逐字照搬、未改写**;管「为什么这么定 / 怎么发现的 / 当时的状态与验收欠账」。
> **规则以 AGENT.md 为准**(每段在 AGENT.md 里留有规则句 + 指向这里的指针)。本文件只追加不改写:新决策照此格式加一节(标题 + 来源 + 原文)。查规则出处时 grep 要连同 `docs/notes/` 一起扫。

## 窗控按平台分叉:Windows 无边框三键 + Mac 原生标准窗 + 影院全屏卡死修

> 来源:分家前 AGENT.md「7.6 常驻临场:开机自启 / 托盘 / 悬浮窗(PLAN §12)」L389–L393。

  - **Windows/Linux(保无边框科幻窗)**:主窗无边框(`decorations:false`,透明科幻窗)+ 右上**自绘三键**(最小化 / **最大化⇄还原** / 缩托盘)。主题化是刚需——Windows **没有「可透明的原生标题栏」**(调研确认),原生装饰会插一条不可主题化灰 bar,故必自绘(业界 VSCode/Spotify 同做)。**中键 = 最大化而非全屏**(`win.toggleMaximize`:铺满工作区、三键永在、随时还原,**结构上不困人**);沉浸全屏只留给看视频(VideoOverlay 影院模式,浮层 ✕⛶ / Esc 退出)。⚠️ **无边框窗最大化在 Windows 会盖住任务栏 + 任务栏点击失灵(Tauri #7103 / #14025,blocked-by-upstream 至今未修)** → 必须 `WM_GETMINMAXINFO` Win32 hack 把最大化尺寸钉到工作区(VSCode/Spotify 同款标准做法,项目已有 `windows` crate、`fullscreen.rs` 已用 Win32;**只能 Windows 真机验**)。
  - **macOS(用原生标准窗)**:主窗 **`decorations:true` + `transparent:false`**(标准原生窗,**不设 `titleBarStyle`、不是 Overlay、不是透明**)——原生红绿灯,**绿灯 = 原生真全屏(进独立 Space、真沉浸)**,最小化 / 关闭 / 全屏退出口全 OS 管。`WindowControls` 自绘三键在 Mac 不渲染(判据 `isMacOS`,单源 `backend.ts` UA);`body` 默认 `transparent` → Mac 补 `html.is-macos body{background:var(--bg)}` 不透明底色免露白;顶部原生标题栏独立占位、内容在其下,**不需要 rail 避让**(区别于 Overlay 那套)。float 悬浮窗两平台仍 `decorations:false`+透明(小胶囊)。
  - **⚠️⚠️ 别再走的死路(2026-07-11 真机连踩两轮,详见 §8.1)**:想让 Mac「原生红绿灯 + 保留透明科幻背景」两全 → `titleBarStyle:Overlay` + 透明窗,**真机两配置两坏法**:`transparent:true`→进出全屏后红绿灯变三个黑色实心圆;`transparent:false`+Overlay→启动就黑、仅 hover 上色。**结论:Mac 要原生红绿灯就得用不透明标准窗(`decorations:true`+`transparent:false`,非 Overlay),放弃窗口级透明**(科幻观感靠内容 backdrop 铺满、不靠窗口透明,损失小)。**别再试 Overlay / 透明红绿灯组合。**
  - **原始卡死 bug 的根因与拟修法(与红绿灯无关、两平台通用)**:`media.fullscreen` 被两组件用两套语义(VideoOverlay 的 `onResized` 无条件当"窗口真实全屏镜像"校准 vs WindowControls 当"视频影院全屏"作藏三键判据)→ 手动全屏后三键 `v-show` 变 false 消失、无视频时又没浮层退出口 → 卡死。**拟修法**:onResized 只在有视频〔`show`〕时校准 + WindowControls 判据改 `current.kind==='video' && fullscreen`。这才是用户报的卡死本体,**与要不要红绿灯无关**。
  - **⚠️ 实现状态(2026-07-11 代码已写齐、随 v0.2.14 入库;两平台真机验全部待做)**:① Windows `WM_GETMINMAXINFO` hack = `src-tauri/src/winmax.rs`(SetWindowSubclass 拦消息把最大化尺寸钉到 `rcWork`;lib.rs setup 主窗 `win.hwnd()` 后挂——**坑:组件写完曾漏接调用点,发版整理时 grep `fix_maximize` 才发现零调用,写「挂钩子」类组件必须 grep 调用点收尾**);② Mac 原生标准窗 = `tauri.macos.conf.json`(数组整体替换,main 改 `decorations:true`+`transparent:false`、float 原样重列)+ 前端 `isMacOS`(单源 backend.ts,UA 判)→ WindowControls 整个不渲染 + `html.is-macos` body 补不透明底色;③ 卡死修法 = WindowControls 藏三键判据改 `current.kind==='video' && fullscreen` 双条件 + VideoOverlay `onResized` 只在有视频时校准 `media.fullscreen`(没视频恒 false)。Mac 编译 / `vue-tsc` / 全测过,**但按 §10「没真机验过不说 done」仍欠**:Mac `tauri dev` 验红绿灯正常 / 绿灯真全屏进 Space / 无露白;Windows 真机验最大化不盖任务栏 + 三键卡死消失。

## 别的程序全屏 → 悬浮窗让位(仅 Windows)

> 来源:分家前 AGENT.md「7.6 常驻临场:开机自启 / 托盘 / 悬浮窗(PLAN §12)」L396。

- **别的程序全屏 → 悬浮窗让位(2026-06-19,仅 Windows)**:float 是 `always_on_top`,Windows 上会盖在别 app 的全屏画面上(游戏 / 全屏视频打扰);**Mac 原生 space 已天然不覆盖别 app 全屏 = 正确,不动**。修 = 壳层 `src-tauri/src/fullscreen.rs::foreground_fullscreen()`(整块 `#[cfg(windows)]`:Win32 `GetForegroundWindow` + 前台窗口矩形 vs `MonitorFromWindow` 的 `rcMonitor` 比对,铺满整块显示器 = 全屏;排除桌面 Progman/WorkerW + 我们自己进程的窗口,免误判)+ setup 里 1.5s 轮询线程**仅在全屏态真变化**时 `emit("lw:foreground-fullscreen", bool)`。**Rust 只报事实、显隐决策仍归主窗 JS**(`App.vue::applyFloat` = `floatOn() && !ownFs && !foreignFs`,与 `lw:show-float` 同构;自愈式去重读实时开关值),Mac 不发此事件、`foreignFs` 恒 false 维持原行为。`windows` crate 复用 Tauri 已拉的 0.61(`[target.'cfg(windows)'.dependencies]`,不引入新版本)。属 §8.1「Windows 专属」一类(轮询前台 / Win32 行为 Mac 测不出),**✅ 2026-06-30 Windows 真机验过**(全屏让位 + 不误判),见 PLAN §12。

## 原生库报错在 Windows 正式版蒸发(nativelog + /MT 静态 CRT + 子进程探针)

> 来源:分家前 AGENT.md「8.1 WebView2 ≠ WKWebView(头号陷阱)」L520。

  - **原生库报错在 Windows 正式版会整个蒸发(2026-07-03 实锤,克隆音色追因)**:sherpa-onnx / onnxruntime / espeak 把真实报错 `fprintf(stderr)`,而正式版是 GUI 子系统没有控制台 → binding 只回 `None`,我们只能盲报「文件齐全,疑似格式/运行时问题」。修 = 壳层 `nativelog.rs` boot 把 fd 2 重定向到 **`logs/native.log`**(unix `dup2` / Win UCRT `_dup2`,libc 两端带正确 link_name;仅正式版启用,dev 终端保持可见;Mac 冒烟验过 Rust `eprintln` + C `fprintf` 都捕到)。⚠️ **但对 sherpa 无效(2026-07-03 真机实锤:native.log 只有 boot 标记)**——sherpa-onnx-sys 在 Windows 下载的预编译库是 `win-x64-static-MT`(**静态 CRT**),它有自己私有的 fd 表,主进程内 dup2 只修 Rust 侧那张 → sherpa 的 LOGE 写进它自己那个 GUI 下无效的 fd 2 = 蒸发。**接住 /MT stderr 的唯一路 = 子进程**(所有 CRT 出生时都从父进程句柄初始化 fd 2):① 产品内 = 克隆加载失败自动拉 `exe --probe-zipvoice <dir>` 探针(stderr 管道回收进 larkwing.log「zipvoice 探针」行,每会话一次;`tts::probe_zipvoice` 与 load 共用同一份配置);② 人肉 = cmd 里 `larkwing.exe 2> err.txt`(先托盘退出防单实例转发)。native.log 仍值得留:Rust panic / 动态 CRT 依赖照捕。配套:`TTS_ZIPVOICE.ready` 补 espeak 内容探针(phontab/phondata/cmn_dict)——目录存在≠内容齐,残缺树曾被判「就绪」导致自愈不重解(此假设被真机排除:探针全过仍失败)。**探针首战告捷、真凶落网(2026-07-03)**:`'\\?\E:\…\espeak-ng-data/phontab' does not exist` —— 见下一条。

## Rust canonicalize 的 \\?\ verbatim 路径喂 C 库会炸

> 来源:分家前 AGENT.md「8.1 WebView2 ≠ WKWebView(头号陷阱)」L521。

  - **Rust `canonicalize` 的 `\\?\` verbatim 路径喂 C 库会炸(2026-07-03 克隆音色真机破案)**:数据目录**搬家**后指针路径带上 verbatim 前缀(`datadir::norm` 曾直接用 canonicalize 产出,Windows 恒回 `\\?\E:\…` 形)→ 整棵派生路径带毒。Rust 自己的 fs 毫无感觉(所以 DB/媒体/模型下载全正常、极难发现),但 **verbatim 关闭 Win32 路径归一化:`/` 不再当 `\`** —— sherpa/espeak 这类 C 库内部用 `/` 拼子路径(`espeak-ng-data/phontab`)→ 文件明明在却「does not exist」;KWS/ASR 不炸只因它们的模型路径全由 Rust 拼好整条传入。修 = `datadir::simplify()`(`\\?\C:\…`→`C:\…`、`\\?\UNC\s\p`→`\\s\p`,只认盘符/UNC 形防误伤):`norm()` 产出即净化(以后搬家写盘干净)+ `resolve()` 读指针时洗一遍(**存量带毒指针下次启动自愈**)+ `zipvoice_config` 兜底再洗。**规则:交给 C 库 / 子进程的路径,永远别带 `\\?\`;canonicalize 产出先过 simplify。**

## 藏托盘的主窗仍 60fps 空烧 CPU(useRafLoop)

> 来源:分家前 AGENT.md「8.1 WebView2 ≠ WKWebView(头号陷阱)」L522。

  - **藏托盘的主窗仍 60fps 空烧 CPU**(2026-06-17,Windows 实测 ~3.3%):关主窗 = `hide()` 进程不退(§7.6),而主窗 `transparent:true` 让 Chromium 遮挡检测失效(透明窗永不算"被挡")→ 隐藏后 RAF **不被自动节流**;加之动画循环本来没有可见性判断 → 背景 canvas + 遛弯 `roamFrame` 藏起来照样满帧空跑。修 = `usePageVisible`(`visibilitychange` + 壳层 `lw:win-visible` 事件**双触发**,后者只为 main 发否则关悬浮窗误停主窗)+ `useRafLoop`(不可见即 `cancelAnimationFrame`);所有 canvas 背景(Neon/Hologram/Hud/Starfield)与 `MainLayout.roamFrame` 都改走它。**新代码起 RAF 循环一律用 `useRafLoop`,别再裸 `requestAnimationFrame` 自调度。** 浏览器验过暂停逻辑(88→0→88 帧/秒);藏托盘后 CPU≈0 **✅ 2026-06-30 Windows 真机验过**。

## 开机自启冷启后动画冻死(三修:清死 id / settled / 看门狗 / 三路补发信号)

> 来源:分家前 AGENT.md「8.1 WebView2 ≠ WKWebView(头号陷阱)」L523–L524。

  - **开机自启冷启后动画冻死(2026-06-30,上条的反向孪生坑)**:`--autostart` 启动时窗口先 `hide()`,前端可见性初值靠「`document.hidden`(透明窗会**误报可见**)+ 异步 `win.isHidden()` 竞态」判定;一旦 `visible` 在隐藏期被误判 `true`,`useRafLoop` 会在隐藏窗里排下一个 rAF —— **WebView 把隐藏期排的 rAF 丢弃却留下非 0 的 `raf` id**,之后窗口被打开时 `visible` 没发生 `false→true` 翻转(本就误判 true)、且 `start()` 的 `!raf` 守卫被那个死 id 挡住 → 永不重排 → **只剩静态画面、动画冻死、头像点了不切**(头像/桌宠/背景全走 `useRafLoop`)。三管齐下修:① `useRafLoop` 的 `watch(visible)` 改「**先 `stop()` 清死 id 再 `start()`**」(先清后排);② `usePageVisible` 加 `settled` 守卫,异步 `isHidden()` 不再覆盖已到达的权威事件;③ 壳层自启 `hide()` 后**主动 `emit("lw:win-visible", false)`**(与 `show_window` 的 true 成对,给前端权威「此刻隐藏」信号,不赌 `document.hidden`)。**只能 Windows 开机自启真机验**:重启进系统→自启静默缩托盘→点开主窗→动画在动、头像可切换;预览无头环境**不触发 rAF**(实测最小循环 600ms 0 帧)故此修验不了,仅编译/启动无错验过。
    - **三修(2026-07-10·夜;前两轮〔watch 翻转+settled+壳层 emit=2026-06-30、revive 脉冲=v0.2.10 起已发布〕真机仍冻,余洞两个)**:① **JS 侧 show/hide 路径全都不发信号**——✕ 关窗是自绘按钮走 `hideToTray()` 直接 `hide()`(**不触发 `CloseRequested`**,壳层那条只兜 Alt+F4 → v0.1.2「藏托盘 CPU 空跑」坑在 ✕ 主路径上其实一直复活着)、悬浮窗唤主窗 `summonWindow` / 视频唤窗 `bringToFront` 直接 `show()`(不经壳层 `show_window`);透明窗 `visibilitychange` 又不报 → 自启后 isHidden 兜底把 visible 正确置 false,此后**经悬浮窗打开主窗 = 没有任何信号翻回 true → 会话区动画全冻**,只有托盘路或杀进程能救。② **revive 脉冲单发可被吞**——即便走托盘路,`show()` 紧跟 emit(true),重排 rAF 那一瞬 WebView2 合成器可能还没醒 → 新排的 rAF 同样被吞、又留死 id,此后再无信号。修(纯前端三件套):**a. OS 真相直采**(`usePageVisible` 三重信号之③:window `focus`/`pointerdown` —— 收得到输入的窗口必然可见;只纠 false 值,已可见不动免得每次点击触发 revive 重排);**b. rAF 看门狗**(`useRafLoop` 1.5s 低频自检:「攥着 raf id 却 >1.2s 没回调」= 死 id → 清 id 重排,**重试直到真跑起来**;raf=0 正常停着不碰,不破「隐藏即停」省 CPU 语义);**c. JS 侧三路补发 `lw:win-visible`**(`hideToTray`→false、`summonWindow('main')`/`bringToFront`→true,backend.ts)。**浏览器已端到端模拟验过**(monkey-patch 吞 rAF 制造死 id→看门狗 3.5s 内无事件自愈;隐藏停住+不发 visibilitychange 只发 pointerdown→救活;健康动画跨看门狗周期无误杀、零 console 错);**Windows 自启真机重验项**:自启→**分别经悬浮窗与托盘**打开主窗→桌宠/背景在动;✕ 藏托盘后 CPU ≈0(hideToTray 补发信号后这条才对所有路径成立)。

## 同步 tauri 命令没有 tokio 上下文(裸 tokio::spawn 必崩)

> 来源:分家前 AGENT.md「8.4 Tauri 同步命令无 tokio 上下文——裸 `tokio::spawn` 必崩(2026-08-28 真机实锤)」L553。

- **同步 `#[tauri::command]` 在 IPC/UI 线程内联执行**(tauri 2.11 源码核实:`on_message` → `run_invoke_handler` 直呼;**只有 async 命令**才被 `async_runtime::spawn` 派发到 tauri 的 tokio runtime),该线程**没有 tokio 上下文** → core 里「后台 spawn、不阻塞调用方」型方法(`VoiceRuntime::retry_model` / `MediaRuntime::retry_component`,开头就是裸 `tokio::spawn`)被同步命令直调 = panic「must be called from the context of a Tokio runtime」→ 穿 FFI 边界 abort = **100% 崩进程**。实锤:语音模型下载失败点 HUD「重试」必崩(`retry_voice_model`);`retry_download` 同病、只是没被点到过。**这类崩溃 Mac dev 也一样崩,但重试卡只在下载失败时出现 → 平时测不到,潜伏到用户真撞下载失败才爆。**

## 聊天流无消息却一直下滚 = 桌宠挂滚动容器里(病)

> 来源:分家前 AGENT.md「8.6 悬浮件别挂在滚动容器里——transform 定位的盒子算进可滚动范围(2026-09-04 真机实锤)」L563。

- **病**:用户视频「明明没有消息出现,聊天流却一直往下滚」——回合在飞、只有折叠的「推理轨迹」药丸没正文,内容被一路顶到顶部、下面留一大片空白。根因链:桌宠 `.roamer` 曾绝对定位在 `.stream`(滚动容器)里、每帧 `translate(dogX, dogY + scrollTop)` 补偿位置;CSS 规定 transform 后的盒子**参与**祖先滚动容器的 scrollable overflow(Chromium/WebView2 一致,预览浏览器同样复现);而 `.body` 姿态容器被 block 的 img 撑成 px×px、从锚点向右下延伸半个身位(注释写「零尺寸」是错的;img 再 -50% 挪回居中,所以看不见的盒子挂在形象下方)→ 桌宠站在底边 44px 处时该盒探出视口 8~22px → 回合在飞每条思考增量触发一次 `scrollTop = scrollHeight` 贴底 → 下一帧桌宠随 scrollTop 再探出 → **正反馈环**,直到回合结束才停。预览 1:1 复现:img 底边 577 在视口(585)内、`.body` 底边 598 在外,24 条增量 scrollTop 663→844。**肉眼/算 img 高度会误判「不是桌宠」**——是合成一个同构测试件逐像素挪、读 scrollHeight 才看出「离底边还有 5px 就涨 22」。

