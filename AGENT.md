# AGENT.md — Larkwing 开发规范总纲

> **这是 Larkwing 全部「规范 / 约定 / 限制」的唯一真相源。** 任何 AI agent、任何新会话、任何协作者，
> 动手前先读本文件。它合并了原 `CLAUDE.md`(项目宪法)、`PLAN.md`(模块设计 + 执行状态)与历次会话沉淀的踩坑经验。
>
> **维护铁律(先看 §10):某些规则一旦变化，必须回来更新本文件。** 改动 🔒 锁定项 / 「用户准则」级规则，**先与用户确认，再改本文件**。

---

## 文档地图(谁是什么的真相源)

| 文件 | 管什么 | 改它的时机 |
|---|---|---|
| **AGENT.md**(本文件) | **规范 / 约定 / 边界**——项目要遵守的一切规则 | 规则变了(见 §10) |
| **PLAN.md** | **模块级设计 + 执行清单 + 真机验收单(watch-items)** | 改设计 / 推进度 / 验收勾项 |
| **CLAUDE.md** | 仅指向本文件(已轻量化) | 基本不动 |
| Claude 记忆(`~/.claude/.../memory/`) | 跨会话的 point-in-time 观察(用户画像、协作风格、踩坑细节) | 由 Claude 自动维护;是观察不是现行法，引用前核对现状 |

阅读顺序:**本文件(规则)→ PLAN.md(对应章节的设计与现状)→ 代码**。本文件给「规则 + 在哪兑现」,深入设计去 PLAN 对应 §。

> **入库说明**:仓库里只有本 `AGENT.md`;`PLAN.md` / `CLAUDE.md` / Claude 记忆都是**本地文档**(被 `.gitignore` 排除、不随仓库走)。故本文件力求**自包含**——文中「详见 PLAN §X」指向的是开发机上的本地设计文档,clone 仓库的人没有它也不影响理解规则本身。

---

## 0. 一分钟认识 Larkwing

- **一句话**:面向普通人 / 家庭(含老人小孩)的**暖萌 / 科幻陪伴型 AI 助手**,**Rust 全新重写**(不是现有 Python `robot`)。= 旺财的桌面版。
- **给谁**:消费级产品,目标用户**不是开发者**。核心洞察 = robot 太自由可配置普通人不会用;Larkwing 反过来**强默认、开箱即用、收口**。
- **技术栈一句话**:Tauri v2 壳 + 单 `larkwing-core`(Rust/tokio)+ Vue 3 + TS(MVVM)+ SQLite + DeepSeek 优先(多供应商 trait 化)。
- **现状(2026-06)**:MVP 后端 + agent/工具运行时 + 影音 + 任务需知 + 文件能力 + 提醒/web + 语音(A+B+C 全量、D 部分)+ 常驻临场(开机自启/托盘/悬浮窗)**均已落地**,核心测试全绿。**权威执行状态以 PLAN.md 各 § 为准。**

---

## 1. 最必须记住的(TL;DR 铁律)

1. **先想清楚方案 → 跟用户确认范围 → 再写代码**;但本阶段是 **UI 优先、即兴探索式**开发(见 §2)。
2. **用户只面对一个 7274**:绝不向用户暴露 agent / 插件 / prompt / 配置 / 场景 / 工具概念。强默认、收敛、开箱即用。
3. **人格中立底座**:引擎 / 回合循环 / 工具 / 事件 / 任务进度 / core 文案一律人格中立;人格只从两个入口进入——**场景数据**与**皮肤层**。
4. **通用回合循环,任务知识零入代码**:无意图分类器、无 per-task workflow、无 per-scene 分支;工具按「能力轴」做**正交原语**;加能力 = 加一个工具文件或一份场景数据,循环永不改。
5. **「X = 数据」哲学**:场景 / 人格 / 皮肤 / 供应商 / 模型档位 / 语音模型 / 任务需知**全部数据化**;加 X = 加一份数据,代码零改或一行注册。
6. **出站 HTTP 全走 `net::Client`**,禁止新建裸 `reqwest::Client`(唯二例外:net 模块自身、`#[cfg(test)]`)。
7. **reasoning 保真铁律**:不透明 reasoning 状态会被兼容层丢弃的供应商 → 下楼写原生;纯文本 reasoning 走兼容端点;**绝不接受静默降质**。
8. **记忆归人**,跨所有场景共享,**绝不**按 agent / 场景隔离。
9. **目标平台 = Windows(WebView2),Mac 开发**;一整类媒体 / 窗口 / 全屏 / 编解码 / 性能 / 语音 / 代理 / JWT 行为**只能 Windows 真机 / 真网 / 真钥匙验**,Mac 跑通 ≠ 通过。
10. **core 不产用户可见文案**(只发 key / 数据);文案在前端字典,i18n 两边同步。
11. **改起来贵的提前保守设计,改起来便宜的按期抵达**;不复刻 robot 的复杂度与防御性堆砌。
12. **不擅自写死「具体默认值 / 具名判断」**:硬编码默认名 / 唤醒词 / 音色 / 皮肤 / 模型名 / 阈值 / 具体值判断分支前**先与用户确认**(用户准则);默认 = 产品决策、应单源 / 数据化,不算可自行拍板的过程决策。详见 §4.11。
13. **(元规则)规则变了必须更新本文件**;改 🔒 / 用户准则先确认(§10)。

---

## 2. 工作方式(协作规范 · 必须遵守)

- **先架构再写,但本阶段 UI 优先**:消费陪伴产品,视觉 / 交互手感是核心。先做「能看见、能点」的纯前端(Vue3+TS,浏览器热更新预览),后端(Rust/Tauri/LLM/记忆)押后、先用假数据顶;前期不急着抽 Pinia store / 分层,UI 摸顺再重构。锁定的技术栈(§4)仍要守住,前端代码以后要能平滑进 `src/`。
- **过程决策直接做、别逐一问**:实现 / 设计层面的小决策**自己拍板、直接写**,简短说明做了哪些决策即可。**只有**真正不确定、涉及产品方向、不可逆、或撞上 🔒 / 用户准则时,才停下来确认。频繁询问会拖慢用户的探索节奏。
  - ⚠️ **明确的例外:写死「具体默认值 / 具名判断」不在「自行拍板」之列 → 先确认**(默认名 / 唤醒词 / 音色 / 皮肤 / 模型名 / 阈值 / 写死的具体值分支)。详见 §4.11。
- **改 🔒 锁定项前先确认并更新本文件**(§10)。
- **纯视觉调整**可用浏览器预览(本地静态服务器)快速迭代,不必每次原生 build。
- **执行节奏**:PLAN 里多块设计「确认后一起执行」,不逐块边聊边写(用户要求)。

---

## 3. 产品铁律(§ 收敛 / 强默认 / 不暴露)

1. **收敛、强调、强默认**:设置面客观上不小(LLM / 语音 / 远程渠道,体量趋近 robot),不追求完全隐藏;原则 = 第一层只放高频少数项,其余分层可达。第一屏不是设置页;唯一不可避免的首次设置 = LLM 的 key(友好填一次,或读环境变量)。
2. **用户只面对一个 7274,绝不暴露 agent / 插件 / prompt / 配置**:用户侧**不存在**场景 / 模式概念,UI 不做场景切换器。场景退为内部偏置预设,由系统 / 模型自决(多模式时走内部 `enter_mode` 类工具,会话级粘性保前缀缓存;**拒绝**每条消息前的意图分类预调用——杀缓存、加延迟)。发现性由**建议气泡**承担(替用户说一句话,不是模式开关)。
   - ⚠️ 一处放开:聊天「想了想」**展开层**会露工具名 / 入参 / 结果 + CoT 原文(会展开的就是想看机器的人);**折叠层仍守**铁律。是否正式入规则待定。
3. **引导式上手**:示例、建议气泡;普通人靠「看到能点的」探索,不读文档。**建议气泡通用版已落地(v0.1.6)**:空会话(还没用户消息)显一组精选「起步建议」chips(放歌 / 提醒 / 整理文件 / 查天气 / 闲聊 / 问能力),点一下 = 替用户把那句发出去(走正常 send,**不是模式开关**);一开口即消失、`hasApiKey` 才显;文案在前端字典 `chat.suggest.*`(core 不产文案 §6.6)。场景触发气泡(如刚放完歌问「单曲循环?」)留后续。
4. **界面优先;语音只是输入方式之一**。**两类交互二分**(贯穿语音设计):
   - **UI 交互** = 打字 / 麦克风按钮 / 听写快捷键 → 正常排版,**默认不念**(气泡可点「再听一遍」)。
   - **语音交互** = 免手唤醒(人可能不在屏幕前)→ 口语短句、不出表格 / 代码 / 链接,**必念**。
5. **容错兜底**:听错 / 连不上 / 出错都要有友好退路;**不静默失败、不静默重试**(重试是 UI 的友好按钮,不是底层隐藏行为)。
6. **视觉主题化(可换肤)**:组件**只用语义 token、绝不写死颜色**;皮肤 = 数据;每个用户记住自己的皮肤;换肤只改观感、不改布局。详见 §6「多皮肤架构」。
- **用户面零新概念**:任何新能力(任务需知、提醒、整理文件…)对用户都是「说一句话 / 无感 / 在已有页面看改删」,不引入新名词。

---

## 4. 🔒 锁定决策与「用户准则」(动前必先确认)

> 以下是宪法级硬规则。改动**任一条**都要先与用户确认,再更新本文件。

### 4.1 命名与调性 🔒
- 项目英文名 **Larkwing**;面向用户中文名 **旺财**(暖萌皮);默认助手名 **7274**(科幻调性)。
  - **名字 = 用户数据,默认 = 产品决策**(同唤醒词模型,§8.2):用户在「叫我什么」(`ui.pet_name`)改名后,既进 UI(标题 / 输入框 / 悬浮窗),也进模型——persona 用 `{name}` 占位,`engine/context::build_context` 取 `ui.pet_name`(空 = 默认)填入「你是 {name},…」身份句,所以模型自称、被问名都跟随。开场白则**不带名字**(静态文案,避免与改后的名打架,2026-06-17)。
  - **默认名「7274」三处手工同步**(改默认 = 改这三处):Rust `context::DEFAULT_NAME` · 前端 `pet.name`(`src/locales/*.ts`)· 本条。persona 已是占位符、不再硬编名字,故不在同步清单。
- **科幻优先(观感)、性格中性(底座)**:当前默认观感 = 科幻(玻璃 / 辉光 / HUD);**默认性格保持中性 / 极简**(2026-06-17 用户拍板,原暖萌默认调子 → 中性):出厂人设只给功能性底座(语言跟随、自然简洁、记忆、诚实、不自称 AI + 放歌先本地后网络),**不预设任何性格倾向 —— 既不萌也不酷,连「不卖萌」这类否定式规定也不写**(那本身就是一种倾向);目的 = 默认适配最多用户。想要某种性格的人自己在设置「我的性格」(`persona.style`)一句话改;**出厂 `DEFAULT_PERSONA_STYLE` 留空**(= 不注入性格层),输入框用占位示例提示可改。落点:`companion.json` 的 persona / 开场白 / 语音应答词 + `DEFAULT_PERSONA_STYLE`(Rust 与 useSettings 两处镜像、值为空)+ few-shot 示范语气均保持平实中性。暖萌热络 = 可选皮肤,不进默认。
- **已否决的名字**(别再提):`Tideripper`(太凶)、`Sunwing`(撞加航)、`Waterwing`(像泳圈)、`Emberwing`(撞游戏)。

### 4.2 产品定位 🔒
- 消费级、面向普通人 / 家庭,**不是开发者**;架构支持多用户(每人各自记忆),但**当前 MVP 不投入多用户 UI**(2026-06-12 用户拍板先搁置,声纹 / 家人 core 已备未启用,见 §9);首个场景 = 闲聊陪伴。

### 4.3 技术栈 🔒
- **Tauri v2**(Rust + WebView)。**不用 Electron、不用 Python。**
- **Rust 核心**(单 `larkwing-core` crate,`tokio` 异步)。引擎整体是 Rust。
- **前端 Vue 3 + TypeScript,MVVM**。**不用 vanilla JS。**(Model=Rust 数据经 commands/events;ViewModel=Pinia/响应式 store;View=Vue 组件;旺财状态机由事件流推导。)
- **存储 SQLite**(记忆 / 历史 / 用户)——一个文件,备份 = 拷文件。
- **LLM:DeepSeek 优先**(OpenAI 兼容,流式 SSE,自动前缀缓存);trait 化(`LlmProvider`)。
- **不重写、不包装现有 Python robot**——独立项目。

### 4.4 多供应商立场 🔒(2026-06 定)
- **协议实现打底**(`openai_compat` / `anthropic_compat`)+ 厂商差异走 **Quirks 数据修正** + 真不兼容的**单独实现**(trait 逃生口)。
- **供应商 = 数据**(`llm.providers` JSON);**模型档位 / 价格 = 目录数据**(粗分三档,未知按均衡档,价格存疑只报 token)。
- **钥匙是用户的,路由是产品的**:用户只见「用脑策略」三档(省着用 / 均衡 / 聪明优先);路由粒度 = 场景 / 会话级(保 prompt cache);建连失败自动切备用。
- 钥匙 / 接入点支持 `${ENV}` 引用(取值时解析,存储留原文)。
- **已落地**:原生 Gemini(`Protocol::Gemini`)、原生 OpenAI Responses、Ollama 走 `/v1` 兼容、DeepSeek 兼容。详见 PLAN §3。

### 4.5 reasoning 保真铁律 🔒(2026-06-16,用户准则)
- **绝不接受「静默降质」**:若走兼容端点会丢失模型必须逐字往返的**不透明** reasoning 状态(Gemini `thought_signature` / OpenAI `encrypted_content` / Anthropic `thinking.signature`),导致不报错、推理却悄悄变笨 → 该供应商**值得下楼写原生实现**。
- **判据** = reasoning 状态**是否不透明且会被兼容层丢弃**:不透明签名 / 加密块 → 原生;**纯文本** reasoning(DeepSeek `reasoning_content`、Ollama `thinking`)兼容端点无损,不在此列。
- 中立类型须为不透明状态留**逐字保真**载体(`ChatEvent::ReasoningState` / `Assistant.reasoning_state`):**不归一、不裁剪、不按 type 过滤**。
- **作用域澄清(重要,易混)**:保真是**轮内 / 工具循环内**的事(模型调完工具继续想时要消费它)。**跨轮丢掉前轮思考是对的、是模型契约,不是降智**(DeepSeek 历史夹 reasoning_content 直接 400 / OpenAI o 系跨轮丢 / Anthropic 跨轮剥离,三家一致)。`openai_compat` 序列化器只在 `!tool_calls.is_empty()` 才写 reasoning_content。收尾轮 reasoning 不落库的唯一原因 = **DB 零膨胀**,与缓存 / 保真无关。「越聊越笨」主因是**上下文稀释 / 指令漂移**,靠记忆 + 历史答案承载跨轮智能,不是回放思考。

### 4.6 出站 HTTP 统一走 `net::Client` 🔒(2026-06-15,用户准则)
- 所有联网(下载 / LLM / web / 天气 / 媒体 …)**必须**经 `larkwing-core/src/net.rs` 的 `net::Client`,**禁止新建裸 `reqwest::Client`**(唯二例外:net 模块自身、`#[cfg(test)]`)。
- 它是「全局代理总开关 + 直连优先 / 连接失败兜底 / per-host sticky」的唯一接缝。
- 加联网代码 = 给调用点一个 `net::Client` 字段,请求走 `.send(url, |c| …)`,下载用 `.direct()` / `.proxy_client()` 两趟。改「该不该走代理」的策略只改 net 一处。
- 硬规则:总开关关 ⇒ 一律直连,哪怕某 host 之前被 sticky 标记;换值 / 关代理即清 sticky。net 模块**不碰 store/llm**(守 engine 唯一合流点),解析在 `Engine::resolve_proxy`。
- **开关与地址分家(2026-06-18,用户要求)**:UI 是一个**总开关** `net.proxy_enabled`(默认 `0` 关)+ 一个**始终保存的地址** `net.proxy`(默认预填 `http://127.0.0.1:7890`,免「空框」烦恼)。关掉只停用、地址不丢;`resolve_proxy` 先看开关——关 ⇒ 直连(**连系统 env 也不读**,比旧的「空=自动读 HTTPS_PROXY」更可预测);开 ⇒ 用地址,地址空才回落 `net::env_proxy`。net 层语义不变(`set_proxy(None)` = 关),开关只是 engine 侧的「用不用」闸门。新增键照 §6.8 两边各加一行(`useSettings` DEFAULTS + Rust `APP_SETTING_KEYS` + `set_setting` 校验 0/1)。
- **未接**:TTS(msedge-tts 同步 connect 不吃代理,国内疑似直连可用);LLM 走代理也可由 `base_url` 覆盖。

### 4.7 记忆归人 🔒
- 记忆**归属于「人(用户)」,跨所有场景共享**;**绝不**按 agent / 场景隔离(明确否决)。
- 分层(边做边细化):画像 / 长期 + 情节 / 历史 + 当下 / 工作;画像层稳定、小、每轮都带 → 进 prompt cache 前缀。

### 4.8 缓存(用户第二优先级:体验第一,成本也省)🔒
- 目标:**体感秒回 + 省 LLM 成本 + 少重复 TTS**;**离线不是目标**。
- 体感秒回(壳):本地缓存最近会话 / 记忆快照 / 场景;进程常驻;流式吐字。
- 省 LLM:稳定大前缀(人格 + 场景 + 共享记忆 + 工具)吃 **provider 侧 prompt cache**(DeepSeek 自动;Anthropic 用 cache_control)。
- **前缀准入判据**(用户准则):一问——「这条信息是不是大多数回合都用不上?」**答题依据 = 这个家的真实使用频率**。常驻与否是**每条信息的数据属性**,不是架构二选一(高频小需知常驻前缀最优;低频 / 大块按需取)。前缀需知区设 token 预算上限,超额降按需。
- 少重复 TTS:按 文本 + 音色 落盘缓存。

### 4.9 部署 🔒
- 目标平台 = **Windows**(WebView2);**Mac 开发迭代,最终出 Windows 包**;不打包 Python。

### 4.10 全局一对 Ed25519 密钥(2026-06-16,用户拍板)
- 整个程序对外**只有一对** Ed25519 身份密钥(`crypto.ed25519.*`),所有 JWT 服务共用;私钥**永不过桥**,公钥在「设置·服务」页展示给用户复制。
- 加新的 Ed25519-JWT 服务**复用这一把**,别 mint 每服务密钥;只有某服务要求**别的算法**才另开。

### 4.11 写死「具体默认值 / 具名判断」必须先确认 🔒(2026-06-20,用户准则)
- **凡要在代码里硬编码一个具体的默认值 / 产品常量 / 具名判断分支**(默认助手名 / 唤醒词 / 音色 / 皮肤 / 模型名 / 阈值,或形如 `?? "某中文"`、`if x == "某具体值"` 的写死分支)——**先与用户确认,再写**。这类「写死」属于**产品决策 + 单源 / 数据化**范畴,**明确排除在 §2「过程实现决策可自行拍板」之外**。
- 缘由:① 默认值 = 产品决策(§4.1 名 / §8.2 唤醒词同款),由用户拍,不是实现细节;② 散落的硬编码副本会**漂移、漏改**——实锤:默认唤醒词「小七」一度在前端 `useFloatIdle` / `useVoice` 各写死一份、与后端单源 `voice/mod.rs::wake_keywords` 脱钩(2026-06-20 清理);③ 呼应 §5「X = 数据」+ §4.8 单源:默认与常量应**单源化 / 数据化**,而非埋进多处代码。
- 正确姿势:能数据化就数据化(进 settings / 场景数据 / 目录);否则归**一个**单一真相源(后端常量),其余处**派生**或留**指向单源的同步注释**;§6.8「两端各加一行」是本条在 settings 上的兑现。真要新增一处写死,**停下来问**。
- 已确立的双源同步点(改其默认即触发本条、逐处对齐):默认名 7274(§4.1)· persona 留空(§4.1)· 唤醒词「小七」+ 灵敏度 100(§8.2)· 皮肤 scifi(§6.7)· 代理地址(§4.6)。
- **单源化范例(本条的正面落地,2026-06-20)**:默认音色 —— 扫描发现前端 `useSettings` 曾写死 `'zh-CN-XiaoxiaoNeural'`、与后端 `tts::DEFAULT_SPEAKER` 成双源;已消除 = 前端默认改空,设置页改从后端 `VoiceStatus.defaultSpeaker` 拉(后端唯一源),故**不**进上面"双源同步点"清单。新加"默认值"照此办:前端别留副本,运行时从后端取。

---

## 5. 架构原则 🔒(怎么搭)

- **core + trait 模块,编译期静态组合,不做动态插件框架。** Python 那套动态插件 / 钩子要甩掉;Rust 动态插件也痛,不走。(`Arc<dyn LlmProvider>` 这类 dyn 调度**不违反**——「静态组合」约束的是不做动态加载,不是禁 dyn。)
- **Trait 接缝**:`LlmProvider` / `MemoryStore` / `InputSource`(文字 now / 语音 later)/ `Tool`。
- **场景 / 人格 = 数据,不是代码插件**:一份场景预设 = 人格提示 + 开场白 + **工具白名单** + **few-shot 示范对话**(中立消息形状,进稳定前缀)。只有需要自定义行为的场景才加一个 trait 实现。
- **人格中立底座**(用户准则):agent 本体(引擎 / 循环 / 工具 / 事件 / 任务进度 / UI 基建 / 全部 core 侧文案管道)**一律人格中立**;代码、core 文案、系统事件、设计文档**不得内嵌具体人格**(反例:「旺财说句萌话」式设计)。人格只从**场景数据**(persona / 开场白 / few-shots)与**皮肤层**(token / 形象 / 前端文案字典)进入。系统事件要「带人格地说」= 把中性事件喂给模型由当前人格组织语言;静态 UI 文案 core 只给 key。**判据:换一套场景数据 + 皮肤 = 另一个助手,底座零改动。**
- **Agent = 通用回合循环,任务知识零入代码**:engine 内唯一一份内循环(调 LLM → 并发执行 tool_calls → 回填再调,至自然收尾);**没有意图分类器、没有 per-task workflow、没有 per-scene 分支**;任务路由 = 模型本身;工具按「能力轴」做正交原语(一原语 ≈ 助理心中「一个动作」),不按任务做。通用性试金石 = 没预设过的组合任务能否完成。
- **Core = 对话编排器**:turn loop(用户 + 场景 + 记忆 → 调 LLM →(工具内循环)→ 流式推 UI → 落库)。
- **「新东西」三物种判据**(防 Runtime cargo-cult):① **能力域**(模型的手脚)= 工具,且仅当有常驻资产(子进程 / 服务 / 注册表)才配 Runtime 进 ToolCtx;② **交互渠道**(消息出入口:语音 / 钉钉 / 微信)= 引擎边界适配器 + InputSource 类接缝,复用 turn loop,**与 ToolCtx 无关**;③ **纯配置 / 数据**(音色 / 渠道地址)= settings / 需知,零代码。一问:模型要不要拿它当手脚用?

---

## 6. 代码约定(怎么写 · 跨模块)

> 深入设计见 PLAN.md 对应 §;这里给可复用的约定与「在哪兑现」。

### 6.1 工作区 / crate
- 单 `larkwing-core` crate(内部分 mod:store / llm / engine / scenes / tools / voice / media / net …),**不拆多 crate**。**mod 边界 = 未来 crate 切割线**:实现只 use 本 mod 的 trait 和 domain,**绝不反向依赖 engine**。
- 该拆的信号:① 出现第二个复用 core 的可执行体;② 全量编译变慢;③ 想单独开源某 provider。
- core 类型全部带 serde,壳层零转换直过 IPC;core 可脱壳测试。`src-tauri` 壳层**只做装配 + 转发,不写业务**。

### 6.2 store
- 数据两类:**出厂只读**(场景 JSON `include_str!`、皮肤 CSS)不进库;**用户可变数据**全进一个 SQLite 文件。
- 两层:`db.rs`(执行层:连接 / 锁 / 事务 / 迁移机,不认识业务表)+ 每域一个文件(自己的表 + 迁移 + Repo)。**Store 是纯装配袋,无方法。**
- **加新域标准动作**:新建 `store/<域>.rs`(表 + 迁移 + Repo)+ `Store` 加一个字段 + `open()` 注册一行,不碰已有域。迁移 id 全局唯一带序号前缀(`0001_users_init`),重号启动即报错。
- 小状态 / 开关**不开新域**,走 `settings` scoped KV(key 带前缀自治,如 `tool.reminder.*`);blob / 可重建缓存(TTS 音频、向量、日志)**不进库,走文件**。
- 横切:Repo 方法**全同步**(亚毫秒;异步调用方自己 `spawn_blocking`);ID 用 rowid,时间戳 unix 毫秒;流式回复**不逐 token 写库**,流完一次落一行;首启 `ensure_default_user()`。
- **schema 不隔离**(跨域外键照用,engine 跨域读拼 prompt),防滑回 per-agent 数据孤岛(§4.7 记忆归人)。
- **数据根可「搬家」(`datadir`,2026-06-18 用户拍板):别再假设 `app_data_dir` 就是数据根。** 用户可把整个数据目录(DB + `media/` + `voice/` + 模型缓存 + 日志)整体搬到别的盘(Windows 多盘:不想全堆 C 盘)。机制 = **锚点 + 指针文件**:锚点 = OS 默认 `app_data_dir`(永远找得到、住指针、永不参与搬家),锚点放 `location.json` 指针说真实根在哪;`larkwing-core/src/datadir.rs` 的 `resolve(anchor)` 在 boot **最先**跑(lib.rs 装配头),返回生效根,**之后 store/日志/media/voice 全部 `root.join(...)` 派生**(原有单一出口不变,这是搬家能干净的前提)。三态:无指针/空 = 用锚点;指向别处且在 = 用它;指向别处但目录不在(盘没插/被删)= 回落锚点 + `data_missing`,前端弹恢复弹窗(**绝不静默在默认位置重建空数据**,§3.5)。搬家 = 拷可重建子树 → DB 走 `VACUUM INTO` 出一致快照(放最后)→ staging 同卷原子改名 → **翻指针 = 提交点**(翻前老数据始终权威)→ 立即 `app.restart()`。**判据:写任何"数据放哪"的路径,都从生效根派生,绝不直接 `app_data_dir()`;DB 里也绝不存绝对路径**(克隆音色 wav / TTS 缓存只存相对文件名 → 整棵子树挪走路径不断,这条已是约定、搬家依赖它)。真机验见 §8.1 + PLAN「数据搬家」watch-items。
  - **一键备份(v0.1.6)= 搬家的轻量姊妹**:`datadir::backup_to(data_root, dest_dir)` 在用户所选目录导出 `larkwing-backup-<时间戳>.zip`(`VACUUM INTO` 一致快照当 `larkwing.db` + `voice/clones/` 克隆音色 wav;可重建的模型 / 缓存 / 媒体 / 日志不收)。**纯导出拷贝、不翻指针不重启**(区别于搬家),命令 `backup_data`,`zip` crate(v2 deflate)已在依赖。延续「备份 = 拷文件」(§4.3)+「DB 只存相对名,整棵子树挪走自洽」。完整流程真机验(Windows zip / 大克隆音色)随搬家一并看。
  - **聊天搜索(v0.1.6)**:`ChatRepo::search_messages(user, query, limit)` 跨会话 `content LIKE` 子串(转义 `% _ \`)、**排除 `tool`/`event` 内部行**、按用户隔离;命中带会话标题 / 渠道 + 截断 snippet。**仍是 substring,不上检索核心**(记忆量小够用,守 §13.9 deferred —— 搜索没构成「第二个 RAG 消费者」,它要语义才触发)。前端最近列表顶部搜索框,输入即查(去抖),点结果跳会话。

### 6.3 llm
- **翻译三处各归其位**(同一逻辑只出现一次):`store::Message → ChatMessage` = 策略,在 engine `build_context()`(1 份);`ChatRequest → 厂商 JSON` / `厂商 SSE → ChatEvent` = 方言,在各 provider 私有 `to_wire()`/`parse_chunk()`(每家一份,内容互不相同)。判别「重复」= 一个变化要不要同步改 N 处。
- **中立 `ChatMessage` 终态**(纸面已定,分期抵达):`User{content} / Assistant{content, reasoning, reasoning_state, tool_calls} / ToolResult{call_id, content}`,`content: Vec<ContentPart>`(Text/Json/Image)。厂商 JSON 永不出 provider 文件。接触面锁死在 engine(唯一构造点)+ 各 provider `to_wire()`,store/IPC/前端不依赖它 → 重构成本不随 app 长大。
- **两阶段错误**:建连前错误(没 key / 401 / 连不上)走 `Err` 立即返回;开流后错误走 `Failed` 事件。
- **取消 = drop Receiver**(provider 内部任务 send 失败即中止断 HTTP),不需额外取消 API。
- **不静默重试**;空闲超时 60s 无增量判 `Failed`。
- **参数无后门**:`ChatOptions` 不留无类型 `extra: Map`——加新旋钮 = 加一个 `Option` 字段(防御性收口)。
- **绑机制不绑模型名**:前瞻模型名(`deepseek-v4-pro` / `gpt-5.5`)无法对官方核实 → 行为绑「文档化机制」,**绝不**硬编模型名分支。
- **本地端点不拒空 key**:vLLM / Ollama 类只要求 Authorization 头存在 → `LlmConfig.api_key` 允许显式占位值(如 `"ollama"`),preflight 别一刀切拒空。
- **key 装配纪律**:`DEEPSEEK_API_KEY` env 优先 → `settings`;改 key = **重建 provider 实例**(不热更新)。
- **密钥落 keyring 已落地**(2026-06-17,§6.3 原「发布前换 keyring」待办兑现):`secrets` 模块(`secrets.rs`)把秘密类 key 存系统密钥串,**不进 SQLite 明文**。**keyring 仅 Windows 启用**(目标平台,凭据管理器随登录会话解锁、不弹框);**mac / Linux 开发机回落 `settings`**(明文,dev 可接受 §4.9)——mac Keychain 对未签名/每次重编的 dev 二进制会弹「允许访问钥匙串」,太烦(2026-06-17 用户拍板去掉),gate 在 `secrets::entry()`(非 Windows 返回 None)。秘密清单 `SECRET_KEYS` = `llm.api_key` / `llm.providers`(整块,含各 provider key)/ `crypto.ed25519.private_key` / `remote.{telegram.token,dingtalk.app_key,dingtalk.app_secret}`;**Ed25519 公钥不在内**(非秘密、给用户复制)。读写一律走 `secrets::get/set`(engine 的 load_registry/set_api_key/persist_specs/ensure_app_keypair、weather 签 JWT、channels 凭证、set_setting 的 `remote.*` 秘密臂、`remote_status` 的 configured 检查全已路由)。**keyring 不可用(headless/dev/无后端)→ 回落 `settings` 并 warn,绝不让 app 哑掉**;存的是原文(literal 或 `${ENV}`),`resolve_env` 在取值时跑。boot 调 `secrets::migrate` 把 legacy 明文一次性迁入(幂等)。✅ 2026-06-30 Windows 真机验过(凭据管理器存取 + legacy 明文迁移)。
- DeepSeek 坑清单(`thinking` 永远显式发、reasoning_content 随 tool_calls 翻转回传、流式碎片重组 / 截断检测 `is_incomplete` 拒执行半截参数、流中 error 帧、usage 三连、finish_reason 锁存)**见 PLAN §3,照抄别重踩**。

### 6.4 engine
- **ContextBuilder 单一装配权**(`engine/context.rs::build_context`):全系统唯一知道「prompt 长什么样」的地方,**纯函数**(无 IO、无内部状态),可 golden-test 断言**前缀字节级相同**。原料来自各处,**装配只此一处**;provider 只见成品,store 只供原料,UI 全程不知 prompt 形状。
- **核心不变量 = 前缀稳定**:稳定层在前(persona + 画像记忆 + 摘要 + 法条 + 常驻需知 + few-shot),易变尾在后(最近消息);历史窗口**锚定整块裁**(满了一次推进一截,不每轮滑一条,否则缓存永 miss);**锚点对齐 user 边界**(防拆散 tool_call/result 配对 → OpenAI 系 400)。
- **瞬态状态三层**:turn(`Turn::run` 局部)/ session(`SessionSlot` per-conv 懒建)/ app(`Engine` 字段)。**入槽资格 = 派生的、可丢的**:真相永远在库,丢槽 = 重算,**绝不 = 出错**。会话权威状态(历史 / 摘要 / 归属)永远在 DB。
- **词汇分层**:`llm::ChatEvent`(provider↔engine)≠ `TurnEvent`(engine↔UI);不复用——UI 需要 `Cancelled`、`Done{message_id}`、友好 `kind`,`Usage` 只进日志。
- **取消 = 协作式 `CancellationToken`**,不用 `JoinHandle::abort()`(硬杀会跳过 partial 落库)。同会话新 send **自动取消旧回合并 await 收尾**(等 partial 落库完再拼历史)。partial 落普通消息(像人被打断),不加状态列。
- **会话生命周期 = `store::chat` 域 + 边界薄层**,是 core 一等公民,**永不委托**插件层;engine 回合管线对「会话从哪来」零感知(只要 conv_id + ChatRepo 契约)。
- **观测数据进库、时间优先**(用户定调):token / 费用 / 耗时持久化供分析,**时间(每轮耗时 / TTFT)是重点**;新增观测走 `usage_rounds` 同款(流水一轮一行只进不改,聚合用 SQL,UI 只是视图)。UI 呈现**默认隐身、hover 浮现**,别把聊天流变仪表盘。

### 6.5 通用回合循环 / 工具运行时(PLAN §8)
- 循环是全系统唯一一份;一「轮」= 一次「开流 → 并发执行 → 回填」,轮内 tool_calls **并发**跑(`join_all`),轮数限的是**串行依赖深度**。
- **轮数三层控制**(用户拍板,不是单个魔法数):① 每 `SELF_CHECK_EVERY=10` 轮软提示自检(中立一句 append 进 request 尾,不落库不进历史 → 不破前缀缓存);② 连续 `MAX_STALL_ROUNDS=5` 轮「全重复调用 / 全报错」→ 强制收尾;③ 硬上限 `MAX_TOOL_ROUNDS`(失控 backstop)。命中收尾闸即 `tool_choice=none`。
- **`Tool` trait**(`tools/`,一工具一文件):`spec()`(name/description/JSON-Schema 参数 / 超时档 / 给 UI 的友好动词)+ `async run(args, &ToolCtx)`;`ToolCtx { user_id, conv_id, store }`(手脚清单不是插件总线)。注册表 `Tools::builtin()`;白名单子集 = 场景数据声明。`Tool::risk()` 元数据 slot 已备但**引擎当前不消费**(执行前确认闸门 YAGNI,未建)。
- **工具六条预记录约束**(PLAN §3):可并行 / 每工具超时 / 异步两型(turn 内阻塞 + 分离 job)/ 状态可视 / 可中断(取消级联进工具,合成「已取消」ToolResult 保历史完形)/ 流式碎片重组规范。
- **工具写入超长一律退回、绝不静默截断**(§3.5,2026-06-18):模型供给且要落库的字段(记忆 `fact`、备忘 `domain`/`content`、提醒 / 盯天气 `content`)超过字数上限就 `anyhow::bail!` 退回报错(经 `run_tools` 当观察喂回模型),让它精简或拆条重写,**绝不 `.chars().take(MAX)` 默默吃半截**(实锤 bug:萌鸡小队第五季 URL 被 300 字上限悄悄截没,模型却以为写全了)。豁免:派生的展示标签(会话标题取前 N 字、不是数据)、外部内容**读取**(`fs` 列目录/读文本、`web` 抓取已各自「如实告知」截断)。
- **few-shot 纪律**:每场景 2–4 段、总预算 **≤800 token**、**至少一段反例**(不该调工具的对话);示例 id 用 `fs_*` 前缀与真实 id 隔开;加载时校验(引用工具 ⊆ 白名单、call/result 配对完整)。**任何 few-shot 都绝不嵌「具体、可复用的事实」**(不只 remember 示范——闲聊/建议类示范同样会泄漏):有名有姓有属性的虚构人物会被当真人(「朵朵」bug);**带属性的具名清单**(地点/资源)会被模型当用户真事抄进 remember(2026-06-18 实锤:建议示范里「湿地公园(放风筝、喂鸭子)、科技馆、采摘园」被原样记成「用户周末常去的三个地方」)。对策:示范内容要么是一次性/自我纠正的事实(不吃香菜),要么是**泛指、无可复用属性**的方向(「户外走走/室内逛逛」),绝不出现结构化的「具名+属性」清单。**统一判据(2026-06-18 再实锤,扩到结果行)**:few-shot 只教**工具形状 + 说话风格**,绝不携带任何模型会误当「用户真事」或「自己已知事实」的具体内容。**头号泄漏口 = `role:tool` 结果行**——模型分不清示范结果和真实结果、且把结果当真相:实锤是默认场景示范里「放流浪地球2 → fs_find 本地没找到 → 转 B站蓝光修复版」,模型把「流浪地球2 本地没有」当成既知事实、**跳过 fs_find 直奔 media_search**(真用户本地明明有)。所以 ① 示范里的真实片名/歌名/歌手/城市一律换**编造占位**(用户永不会点的名字,如「星海漫游」)或**泛指类别**(「儿歌」),让即便被记住也与真实请求不碰撞;② 真实落空/命中类结果不绑定真实作品;③ LAWS「出厂示范」隔离条**立原则、不举例**(教学归 few-shot,§7.3「法条只立法条」——别把「本地没找到某片就跳过」这种 worked-example 塞进法条,那本身就是「往提示词塞具体内容」的毛病,且 few-shot 已在演示 fs_find 先行):法条只说「示范里工具查到的结果全是编的、不是你自己已知的事实,该查证的仍要现查现问」。回归守卫:`eval/scenarios.rs::media-local-first`(登记电影目录后点播,断言先 `fs_find` 再 `media_search`)。`scenes.rs::validate` 只管结构(配对/前缀/白名单),抓不了内容泄漏 → 这类靠本规则 + eval 守。
- **落库**:messages 加 nullable `payload` TEXT(assistant 行存 tool_calls + 该轮 reasoning;`role='tool'` 行存 call_id/name/status);UI 渲染过滤 tool 行。
- **场景自决**(>1 预设时):常驻基础工具 `enter_mode(mode_id)`,turn 内立即生效(写 `conversations.scene_id` 会话级粘性 → 本轮重建请求);换 persona/few-shot/白名单/options,**不换**记忆 / 历史 / 循环代码;每 turn 最多切 1 次;不调用 = 维持现状。

### 6.6 文案 / i18n(PLAN §6)
- **铁规:core 不产用户可见文案。** 错误过桥的是 `kind`,文案由前端按 locale 选;`TurnEvent`/`AppError`/commands 形状不为 i18n 改动。豁免:FakeLlm 文案、tracing 日志、种子数据(`users.name` 默认值)。
- **前端 = 文案唯一产地**:vue-i18n 单字典,`src/locales/en.ts` 是 `zh-CN.ts` 的精确镜像。加语言 = locales/ 加文件 + 注册一行 + 选择器;后端零改动。
- **对话语言 = 模型跟随用户**,与 `ui.locale`(只管界面 chrome)**彻底解耦**;persona 语言中立(「用对方所用的语言回应」),不分叉 per-locale 人格。**开场白**是唯一 locale 触达人格数据处(scene 加 `openings` map)。
- **同步纪律(易踩)**:`zh-CN.ts` 加 key **必须**同步 `en.ts`,否则英文模式静默回落中文。复核 = 两文件 flatten 后比 key 集 + 占位符集。
- ⚠️ **i18n 特殊字符陷阱**(`{ } @ |` 都中招):vue-i18n 消息串有自己的语法,字面写进文案就被当语法解析、编译失败 → 整组件 render 抛错 → 该 tab/视图渲染失败(Vue 保留旧 DOM,表现为「**tab 点了切不过去**」),且**warn 级**难查(Vue 不把真实 Error 给 console)。已踩:① `{ }` 插值——`${中文}` → Message compilation error,`${ENV}` → 静默渲染成空;② `@` linked-message——`@BotFather` 之类直接崩(2026-06 远程渠道 tab 实锤);③ `|` 复数分隔符。**对策**:i18n 文案不写字面 `{` `}` `@` `|`;要展示这类语法就纯文本描述,或把含特殊字符的字面量留模板硬编码、**不进 `t()`**(如 `<button>@BotFather</button>`)。抓这类 render 错最快路 = 设 Vue `app.config.errorHandler` 拦真实错误(比翻 console warn 强)。
- **非字典硬编码中文也要同步**:协议徽章、星期数组(走 `toLocaleDateString`)、`persona.style` 默认值(需与 Rust `DEFAULT_PERSONA_STYLE` 手工同步)等不在字典里的中文,加语言时易漏。
- **布局兜底**:英文比中文长 40–100%,定宽控件按 2 字中文做的会撑爆 → 砍装饰大字距 / 给弹性宽度 / 永远留 `flex-wrap` 折行 / ellipsis 截断。
- **失败别静默(§3.5 的 UI 兑现,2026-06-22 落地)**:用户**主动操作**失败 → `useToast().error(t('toast.*'))` 弹一句友好提示(`composables/useToast.ts` 单例 + 顶层 `ToastHost`,**仅主窗挂**;只用语义 token,换肤跟随;文案仍调用方 `t()` 选好再传,core 不产文案)。⚠️ `useChat.renameConversation` 内局部 `t` 是标题字符串、遮蔽了 i18n 的 `t` → 那一处用 `i18n.global.t`。列表**初载失败**别走空态(会被误读成「没有数据」)→ 走「错误态 + 重试」(共享类 `.lp-error`/`.lp-retry` + `common.loadError`/`common.retry`,见 Memory/Reminders/Ops 三页 `error` ref)。纯**被动**后台刷新(boot / 余额 / trace 补拉)失败可继续 console-only,不必弹。

### 6.7 多皮肤 / 语义 token 架构(§3.6 兑现)
- **唯一色源 = `src/style.css`**;组件**只引用语义 token**(`--bg/--surface*/--line/--accent/--text*/--ok/--warn/--attn/--danger`(各带 `-rgb`)/`--bubble-*`/`--veil-*`/悬浮窗 `--f-*`),**绝不**用皮肤专名或内联 `#5fd2ff`;半透一律 `rgba(var(--X-rgb), a)`。
- **皮肤 = 数据**:每皮肤 = 一个 `:root[data-skin="…"]` 块 redefine 同组语义名 + 一个背景组件。**加新皮肤 = 加 token 块 + 背景组件,组件零改动。** 皮肤存 `users.skin_id`(每用户,默认 `scifi`);boot 过桥设 `<html data-skin>`;跨窗实时同步(`lw:skin` 广播)。
- **形象 / 角色是独立设置 `ui.character`,不随皮肤变**(skin 切观感,character 切谁出镜)。
- **关键陷阱**:scoped 规则(`.x[data-v]`)与全局 `[data-skin] .x` 基础特异度相同 → scoped 后加载会赢。要么 token 化(首选),要么给全局覆盖加 `:root` 前缀提权。
- **列表页共用全局类** `.view-*` / `.lp-*`,新列表页照搬别抄卡片 CSS。**所有滚动容器加 `scrollbar-gutter: stable`**(Windows WebView2 经典条占布局宽,Mac overlay 条看不出 → 真实数据撑满会左移跳动)。

### 6.8 接线纪律(前后端两边各加一行)
- 新增 settings 键:**`useSettings` DEFAULTS ↔ Rust 白名单两边各加一行**。`ui.*` 类自动放行;跨 scope 的同前缀键(如 `voice.*` user/app 各半)**逐键放行,不开通配**。
- OS 真相源的状态(开机自启)**不进 DB**,走独立命令对(`set_autostart`/`autostart_enabled`),薄封装插件防漂移。
- **IPC 事件向前兼容**:过桥的 `TurnEvent` / `app_event` 用 serde **tagged 编码**——加变体对前端是增量、**未知变体可忽略**(给工具进度等未来事件留路);core 类型带 serde 直过 IPC,壳层零转换。
- **Tauri 插件「权限 ≠ 作用域」**(scope 类插件 opener/fs/http… 通用):启用命令的权限(如 `opener:allow-open-url`)**自带零作用域**,**必须**再单独给作用域允许(如 `opener:allow-default-urls`),否则一切被拒;capability 是**编译期**烤进二进制,改 `capabilities/*.json` **必重编 Rust**(Vite HMR 不生效)。详见 §8.3。

### 6.9 组件「用时下载」(包里不带)
- yt-dlp / ffmpeg / 语音模型 / pdfium 等大组件走 `components` 模块**用时下载**到数据目录,**安装包里不带**(性质同浏览器下载文件——所以「不打包 Python」红线不碰)。
- 镜像数据化(`media.gh_mirrors` 等,可热调)+ 校验(有 `SHA256SUMS` 则校验,ffmpeg 不发 SUMS 只靠 TLS,记档)+ PATH 兜底给开发机。**模型 = 数据**,按「语言 → 最强组件」目录化(ModelScope/hf-mirror 优先 + gh 镜像兜底)。

---

## 7. 各功能域约定速查(规则 + 指针)

> 设计与现状以 PLAN 对应 § 为准;这里只记跨会话易错的约定与边界。

### 7.1 影音(PLAN §9)
- **yt-dlp = 解析器组件**(用时下载),**mpv 不搬**——播放器长在自己 UI(WebView 解码 + core localhost relay 转发)。
- **播放分两条路:自适应流走 MSE(shaka)、单流走原生(2026-06-20 起,Stage 1 落地)**。背景:原来把 B 站 DASH 两条流用 ffmpeg `-c copy` 混成**一条渐进 fMP4** + `?t=` **重启式 seek**,拖进度条后**音画错位几秒**(拷贝视频回退关键帧、另一轨不回退,muxer 又归零 → 音频领先)——这是重启式 seek 的**固有缺陷**(B 站 + 本地转码都中),`make_zero/aresample/noaccurate_seek/copyts` 全验过修不了。**B 站本来就是做好的自适应流**(像网页那样),所以治法 = 别再混流:relay 合成一份 **DASH MPD**(`Entry::Dash`,段经 `/s/` 同款防盗链头+Range 透传,sidx 字节范围由 `probe::probe_sidx` 探 .m4s 头得到、`build_mpd` 合成),前端 **shaka(MSE,懒加载)**经 `NowPlaying.manifest_url` 播 → 播放器自己管时间轴 → **原生 seek + 天生同步**,且 **B 站链路彻底脱 ffmpeg**。判据:`manifest_url` 有值 → shaka;否则 `stream_url` 挂原生 `<video>/<audio>`(直传文件/单流,原生 seek 本就对)。DASH 探不到 sidx / 无时长 → 回落老的 ffmpeg 混流(能放、seek 仍错)。**CORS**:shaka 用 `fetch` 拉段是跨源(app 源 ≠ relay 回环口),`/dash/` 端点必须带 `Access-Control-Allow-Origin` + 暴露 Range 头(`<video src>` 不查 CORS、fetch 查)。**B 站这条 2026-06-20 Windows 真机验通**(拖进度条音画同步 ✓,shaka/MSE 在 WebView2 立住)。**本地不兼容文件(HEVC/AC3/mkv)= Stage 2 也落地了(2026-06-20)**:不再走 /m/ 渐进混流,改 **HLS 按需切片(fMP4 段)**——`Entry::FileHls`,`/hls/{token}/index.m3u8` 由 probe 的 duration 合成**完整 VOD 列表**(全段列出 + 共享 `EXT-X-MAP:init.mp4` → shaka 知道完整时长、可任意 seek)+ `/hls/{token}/s{N}.m4s` 按需 `ffmpeg -ss N*6 -t 6` 出一段**单 moof 分片 mp4**(`empty_moov+default_base_moof` + 大 `frag_duration` 逼单 moof)。**段走 fMP4 而非 mpegts(2026-06-20 二修,头号坑)**:最初发 mpegts(.ts)段,shaka 用 **mux.js 把 mpegts transmux 成 fMP4** 喂 MSE —— 实锤**视频轨 transmux 在 WebView2 失败**(append code **3015/3016**)→ **黑屏**(音频没事就视频炸,正是 transmux 那步;`-output_ts_offset` 让 .ts 跨段 PTS 连续也救不了,病不在 PTS 在 transmux)。改 fMP4 段 = B 站 DASH 已验通的同路、MSE 直吃、**绕开 mux.js**。**关键:ffmpeg 输入 seek 出的分片 `tfdt.baseMediaDecodeTime` 恒被重置为 0**,不修则各段全堆在 0 秒 → 还是错乱 → `probe::patch_segment_tfdt` 从 init 读每轨 timescale、把段内 tfdt 改回**累计起点**(`start×timescale`)+ `moof_segment_end` 剔除 `-f mp4` 尾部写的 `mfra` → 产出**标准累计-tfdt fMP4-HLS**,shaka 按 tfdt 直接拼接、不靠播放器额外算 offset。**段一律转码视频 + 下混立体声 AAC(2026-06-20 三修,Mac Chromium MSE 复现实证,`build_frag_cmd`)**:① **视频必转码**——`-ss`+`-c:v copy` 只能落关键帧、切不准 → 段时长漂(请求 6s 出 8s)、段间重叠/错位;转码则每段从干净 IDR 起、恰好 6s、avcC 跨段一致 → MSE 拼得上;② **音频必转码**——视频转码 + 音频 `copy` 时 fragmented muxer 把样本时长写成 **2×**(段被拉长一倍),两轨都转码即消;③ **下混立体声 `-ac 2`**——多声道 AAC 声道布局不明确(AC3/DTS 转 AAC 常见)→ **MSE 拒 append 整个 init**(报在 video 轨 = 用户「video:2 code=3014 黑屏」的真因),立体声永远能 append。代价 = 已是 H.264 的片子(仅因容器/音轨进 HLS)也重编视频、弱机吃 CPU,但它们当前本就黑屏(mpegts 链路)不是回退;后续可给「视频已兼容」者走整文件 copy-remux→DASH 省 CPU。无临时目录 / 无会话(每段现切现回、用完即弃);seek = shaka 请求目标段 → 现切现回 → 原生 seek + 同步。前端 `manifest_url` 路径对 DASH/HLS 通吃(shaka 自动认),**前端零改**。无时长 → 回落 /m/。**✅ 2026-06-30 Windows 真机验过**:B 站已验通;本地 fMP4-HLS 的 shaka 拼播 + 拖动同步 + HEVC 重编码 CPU 可接受均已验。**Mac 已把能验的全验了**:用**预览浏览器(Chromium,与 WebView2 同 MSE 内核)裸 MSE 复现** —— 复现了 5.1-AAC init append 失败(3014)+ 证实 fMP4 段/tfdt 累计/立体声修法后 init+s0+s1 全 append 成功、时间轴连续;真·ffmpeg 端到端切段→patch_tfdt→可拼;单测全绿(见 §8.1)。
- **进度总线 `tasks`**(影音引入的通用件):进度句柄 **drop 未收尾 = 自动 fail**(防僵尸进度条);label / step = key + params 走前端字典(core 不产文案)。
- **登录一期 = app 内扫码**取 cookie(原生 CookieManager;SESSDATA 是 HttpOnly),**绝不依赖外部浏览器保持登录**。匿名也能跑,首次成功后 `LoginHint` 提示一次。
- **需登录的播放 ≠ 失败 + 登录后自动重放(2026-06-18,用户反馈「扫码时还哐哐往下跑、最后失败」)**:`play()` 命中「需登录」时**不再 `bail` 当失败**——记下待重放(`pending_play`,按源,10min 过期作废)+ 发 `AuthRequired`(弹扫码气泡)+ 解析任务**正常收尾**(不标红 HUD、不喂模型「放失败了」),返回 `PlayOutcome::AwaitingLogin{detail}`(工具层据此引导用户扫码、说「登录后会自动接着放」)。扫码成功 `set_cookies` 那一刻**带新 cookie 自动重放**(不绕模型、同嘴控哲学;无 tokio 运行时的同步调用方保留待重放不丢)。未知来源无登录通道才回落「如实退回」。`play()` 返回 `Result<PlayOutcome>`(原 `NowPlaying`),`media_retry` 只看 `Err` 不受影响。
- 多源立场与 LLM 同构:解析层天然多源,按源分化的只有搜索 + 登录态 + **剧集发现**(`MediaSource` trait),MVP 单源 bilibili。
- **多集自动续播(2026-06-20 落地;B 站合集/分P + 本地剧集 + 集级续播记忆)**:动画片/电视剧放一集自动接着下一集。机器**来源无关**——`MediaRuntime` 持一份队列(`Playlist{series_key, entries:Vec<EpisodeRef{id,url,title}>, index, audio_only}`,app 级瞬态、派生可丢),`advance(±1)` 只挪 index、`play_entry` 现取现播(流地址永不过期);只有「发现剧集清单」分两路:**B 站**走新 `MediaSource::episodes`(view API `x/web-interface/view`,**合集 ugc_season 优先、其次分P pages**;分P 的 P1 用裸 bvid url 对齐 url 匹配),**本地**走 `local_episodes`(同文件夹 + 同类扩展名桶 + **数字骨架分组**防平铺电影库误触 + 自然排序;§确定式扫描,用户否决了 LLM 排序——规模/可复现/续播稳定性)。`NowPlaying.playlist:{index,total,resumed}` 驱动前端「第N/共M集」+ 上/下一集按钮;前端 `ended` → 非末集调 `media_advance(+1)`(不绕 LLM)、保持全屏不退出;嘴控 `media_control` 加 `next/prev`→`advance`。**续播规则**:仅当请求落在「自然起点」(requested_index==0)且非 restart 才用存档跳上次那集;点名某集(index>0)就放那集;`media_play` 的 `restart=true`(用户说「从头/重新看」)忽略存档。**续播记忆 = 专用域 `store/media_progress`**(不进记忆系统——高频结构化运行态非「关于人的事实」),per-user、PK(user,series_key)、**只存集身份 + 相对名,series_key 本地用 `local:FNV(目录+骨架)` 单向哈希,绝不落绝对路径**(§6.2);起播/切集即写,失败不挡播放。**✅ 2026-06-30 Windows 真机/真网验过**(同 §8.1):真 B 站合集/分P 的 view 解析 + 自动切集播放 + 跨会话续播 + 本地文件夹扫描/续播链。
- 工具三原语:`media_search`(读)/ `media_play`(写,job 型秒回)/ `media_control`(嘴控,按钮直连前端 VM 不绕 LLM);**校验收口 core**,音量跨播放粘住、倍速每次复位(mpv 时代教训)。
- 本地播放链:需知(目录)→ 文件原语找文件 → `media_play` 放行本地绝对路径 → relay `/f/` 本地文件端点(手写 Range)。NAS 挂载盘符 / UNC 是普通路径。
  - **本地视频"探测→只转处理不了的那部分"(2026-06-19,§8.1 编解码坑的本地补课;用户拍板「按需」)**:`play_local` 三分路(均**只转 WebView2 解不了的轨**,兼容轨一律 `-c copy`):① **BMFF(mp4/mov/m4v)** 走 `probe::probe_local` 读 `moov`(零子进程、不下 ffmpeg、不拖慢普通文件)→ 全兼容原生 `/f/` 直传秒开,音轨 AC3/DTS/EAC3/TrueHD/ALAC 或视频 HEVC/AV1/杜比视界不兼容才取 ffmpeg;② **mkv/avi/ts/wmv… 容器**(`needs_ffmpeg_container`,WebView2 本就放不了)必经 ffmpeg 转封装 → 先 `ensure_component(Ffmpeg)`、再 `ffmpeg -i` 解析 stderr(`probe::parse_ffmpeg_stderr` 纯函数可测)拿编码+时长、按需 copy/转;③ webm/未知/`audio_only`(放歌)→ 直传。转码走 relay `Entry::FileRemux`(`-c:v libx264 -preset veryfast -crf 23 -pix_fmt yuv420p` / `-c:a aac`,与 B 站 DASH 混流共用 `stream_ffmpeg` + 走 `/m/` + 前端 `?t=` 重启 seek,**前端零改**);ffmpeg 取不到一律退回直传不阻断。配套:`play()` 一进来**后台预取 ffmpeg**(首次播放**任何**媒体含放歌就 fire-and-forget 下好——预取的是工具不是转码,下了不一定用但真用到时零等待;不卡播放,§4.6 同款 net 下载;锁去重+已在磁盘秒返回 → 与用时下载只下一份)。视频转码吃 CPU、弱机可能跟不上,preset/硬解是真机调优项。
- **工具入参的布尔值走 `tools::arg_bool` 宽容解析(2026-06-19)**:模型(尤其流式 JSON)常把 schema 声明为 boolean 的参数发成**字符串** `"true"`/`"false"`,裸 `Value::as_bool` 认不出就静默回落默认(实锤:`audio_only:"true"` → 当 false → 放本地歌弹出全屏视频框)。新加 boolean 入参一律用 `arg_bool`(真 bool / "true"/"false"/1/0/yes/no 都认),别再裸 `as_bool`。属 §4.4「Quirks 数据修正」一类。

### 7.2 文件能力(PLAN §9「文件能力」)
- **安全立场:不做任何安全承诺**(用户拍板)——可逆(撤销 / 重做、操作记录页)是**功能性**的(跟普通文件管理器一样),**不是安全网**;**UI / 文案 / 工具描述一律功能口吻**,别用「承重墙 / 兜底 / 保护文件」叙事。
- **不强制对话确认、不设路径门禁**(都否决了);全交给模型。
- **可逆三规**(让功能性可逆成立):① `fs_move`/`fs_copy` **永不覆盖**(同名自动 ` (N)`,资源管理器口径——这是功能正确性);② 删除走系统**回收站**不 `unlink`;③ 文本写 / 改前把旧内容快照进记录(`store::fsops`,一批一行 JSON,撤销 = 逆序反向)。
- **直交原语**(read_text/move/copy/mkdir/trash/write_text/append/edit/undo + list/find),**不造 `organize_media` 这类任务工具**(§5);模型 + 需知目录自行组合。`fs_edit` = 轻量 find/replace + 旧内容快照,**不背 robot 的 read-gate/stale**。
- **Windows 功能正确性**(Mac 测不出,真机验):跨卷 `rename` 失败转 copy+删源;Windows 名校验(保留名 / 非法字符 / 结尾点·空格);回收站还原 `trash::os_limited` 仅 Win/Linux(macOS 降级返回未还原)、长路径 `\\?\`、被占用文件锁。
- **量是一等约束**:写原语**批量原生**(数组);工具结果**只汇总 + 只点名失败**(token 不随条数爆);单次调用封顶 300、超额如实告知。

### 7.3 任务需知(PLAN §9「任务需知」)
- **任务需知** = 跟着**任务 / 能力域**走的环境知识(电影目录在哪、代码仓库在哪):非人格、非个人记忆、非应用设置。数据形 `{域, 内容, scope:家|个人, 常驻?}`,自有小域;**常驻 vs 按需是每条的数据属性**(§4.8)。
- **法条搬进底座**:运行时法条进 `engine/context::LAWS` 固定段(人格中立),companion persona 修剪成纯性格(第二场景自动继承)。
- 写入 = 对话即配置,few-shot 三路路由(关于人 → 小本本 `remember`;环境 / 资源 → 需知 `briefing_write`;一次性 → 不记)。**用户面零新概念**(看 / 改 / 删 = 回忆页两分组「关于你」/「家里的事」)。
- 提示词总原则:**提示词只立法条,教学归 few-shot,绑定归工具描述,内容归数据节**。

### 7.4 提醒 / jobs / web(PLAN §10)
- **jobs 底座**:`jobs` 域 + `scheduler`(30s 轮询,无 cron 框架;错过宽限 2h——once→missed、**重复任务推进到未来不补发**(防开机轰炸);触发即推进 = at-most-once)+ `engine.wake_turn` 自启回合(event 行落库 UI 不渲染,模型转述才给人看;目标会话在飞则跳过本 tick 绝不打断;经全局事件车道发动静)。
- **job 执行一律新鲜上下文**:稳定前缀与聊天回合字节级相同(共享缓存),不回放历史;任务语境靠创建时**物化进 content**(自包含、指代全展开,2000 字上限)。
- **mode 只活在创建时刻**:remind → 落当前会话;task → 建专属会话(连载帖,闲聊会话零污染)。
- **提醒三件套** reminder_set/list/cancel:用户说人话,模型用 `now` 推 first_at;**cron/job 概念永不暴露**。
- **web 二件套 = 搜索即抓取**:`web_search` 一次带回 top3 正文证据片段(修掉 robot「链接堆 + 串行 fetch」多轮往返);选择器是代码不是数据,坏了改 `web.rs`;预算闸(片段 1200 / 全文 6000 / 页面 10MB)+ 同 URL 10min 缓存。
- **网页正文 = 不可信输入**:以「观察」喂回模型(§9 不做风控,但**外部抓来的文本不当指令**;后续考虑来源标记 / 隔离话术)。

### 7.5 语音(PLAN §11)
- **业务零入**:ASR 出文本 → 前端走既有 send 链;TTS 念 TurnEvent 文本。回合循环 / 工具 / 记忆 / 场景**零改动**;voice 模块不碰 engine。桌面形态**编排者 = 前端 VM**(robot 最痛的双播坑结构性消失)。
- **管线参数出处**:全部源自前身项目 robot 的 Windows 真机实战调优(其语音设计文档的经验教训 / 调参速查 + channel / vad / wake_word 源码);属本地参考资料、不随本仓库。
- **推理统一 sherpa-onnx**(官方 Rust 绑定 `sherpa-onnx`;sherpa-rs 已弃用):ASR/VAD/KWS/声纹/本地 TTS 一个原生依赖。
- **播放分两路**:长内容(回合 TTS)走 WebView relay `/tts/`;延迟敏感短句(唤醒应答 / 追问 / 告退)走 **core 原生 cpal 直出**(PCM 内存常驻,应答一停立即开录 0 间隙)。
- **管线参数 = robot Windows 真机实调终值,锁死进代码不暴露**(采样率 / 帧长 / pre-roll 200ms / hangover 800ms / min_seconds 0.5 反幻觉 / max 12 / silero 阈值 0.5 …)。silero 是唯一 VAD,不留 energy 档(强默认收口)。
- **TTS 流式切分 = 跑道(runway)驱动自适应**(急 / 稳 / 懒三档 + 停顿 / 收尾兜底);切分器 = `useSpeech.ts` 纯函数,参数锁死;Markdown/代码/URL 净化在切分前。**ffmpeg 全程不在 TTS 链路**。
- **AEC 明确不做**(家用麦克芯片带硬件 AEC,软件再叠打架);**录音媒体避让 duck** 到 20%(唤醒 → 回合收尾区间);**Windows「通信活动自动压低」**是 OS 设置(app 改不了,放引导卡)。
- **听不清两段式有声兜底**(绝不静默失败):没声音 / 没识别出字 → 第一次播追问立即重听;第二次仍空 → 播告退语回待唤醒。机制在 core,**话术 = 人格数据**(场景 JSON,随语言变体)。
- **i18n** = 一张「语言 →(ASR / TTS / KWS)最强组件」目录表,每语言配该语言最强组件;切语言 = 换模型组件(用时下载)+ 换音色。一期只填中文。
- **ASR 模型用户可选(2026-06-20 放出选择器;2026-06-30 定为 2 档)**:起因 = 用户实测**小朋友识别率差**。每语言仍有「默认最强组件」(中文 = SenseVoice,快),**额外一个选择器**(设置·声音「识别模型」`voice.asr.model`,app 级,默认 `sense-voice`)给**两档**:`sense-voice`(快,默认)/ `firered-ctc`(小红书 FireRedASR2-CTC int8,~740MB,**大陆原生、简体、普通话 SOTA**,UI = 「更准·听不清/孩子选这个」)。**Whisper 三档(tiny/small/medium)+ fast2s 繁简转换已移除(2026-06-30,用户拍板)**:用户真机发现 Whisper 档**输出繁体 + 效果差**;研究证伪「中文小孩→Whisper」——① 繁体 = 训练/分词偏置非台湾腔(声学照常,本可 fast2s 繁→简贴补丁但治标),② FireRedASR 论文实测比 Whisper-Large-v3 / SenseVoice-L 强 **29–68% CERR**(我们用的 tiny/small 远弱于 large-v3),③ 孩子无现成解(真根治 = 儿童语料微调,本期不做;FireRed 在 19 个口音/方言基准 11.55% CER = 现有最佳代理)。故砍 Whisper(治本去繁体)+ FireRed 当「听不清/孩子」推荐档 + 回 2 档更收敛(§3);代价 = 丢多语种(大陆家庭用不上)。仍守「X=数据」:加档 = `voice/models.rs` 一个 `ModelSpec` + `AsrModel` 一支 + `asr.rs` 一个构造分支(sense_voice / fire_red_asr_ctc 单文件),`transcribe`/trait 面不动;缓存带档身份(换档重建、同档复用),换档+开着唤醒由前端 `restartWakeIfRunning` 生效;旧 `whisper-*` 值回落默认(老用户无感)。UI 友好档名不露模型 ID(§3)。**✅ 2026-06-30 Windows 真机 + 真孩子声验过**:FireRed 对小朋友/口音的识别改善确认明显;HF resolve 直链国内可达(hf-mirror 优先 + gh release .tar.bz2 兜底)。
- **语音会话模式**(robot channel_format 对应物):**不按渠道注入提示词**(破前缀缓存)。落法 = 交互形态**物化成消息数据**(payload `{input, speak}`)+ 装配时 speak=true 加确定性标记 `〔语音〕` + 一段**常驻法条按数据条件生效**(LAWS「说话守则」)。
- **听写快捷键**:app 内绑定、**输入框打开时才生效**、**不做全局热键**;且**固定不暴露**(不进设置,tooltip 提示)——强默认收口的兑现。
- **英文免手唤醒 = 单独工程**(语音层③后置):KWS 被中文模型锁死(zipformer-zh),ASR / TTS 已支持英文但唤醒词没有;别当 bug。

### 7.6 常驻临场:开机自启 / 托盘 / 悬浮窗(PLAN §12)
- **零新 core 事件**:悬浮窗只是「全局事件车道」(`app_event`)又一个消费者,复用同一份 Vue app / token / 形象 / composable;窗口管理与托盘进**壳层**。
- 形态 = **C(混合可展开)**:收起 = 一体小挂件,展开 = 信息面板(进行中 / 通知两区);常驻锚点 = 系统托盘;开机自启 = `tauri-plugin-autostart`(OS 真相源)。
- **关主窗 = 隐藏到托盘、不退进程**(✕ 首次点出友好气泡兜底);主窗无边框(`decorations:false`)→ 自绘右上角三键。**mac 左上原生红绿灯 = 不做(2026-06-16 用户拍板)**:目标平台 Windows 没红绿灯、纯开发机便利;原生 Aqua 红绿灯系统锁死无法主题化(与科幻皮不搭),要它只能 mac 专属 `titleBarStyle:Overlay` 分叉窗形——不值,自绘三键跨平台一致 + 贴主题。
- **单实例 / 二次启动唤回**(2026-06-16):全程序只跑一个进程。已在运行时再点快捷方式 / 重复启动**不开新进程**——`tauri-plugin-single-instance`(**放最前注册**),OS 把第二个进程的命令行交给已运行实例的回调、第二个进程退出;回调复用 `show_window` 把主窗(可能藏托盘)唤到前台,沿用 `--autostart` 静默语义(自启触发不唤窗)。无 IPC 命令、不需 capability。**OS 转发 + 窗口前置 ✅ 2026-06-30 Windows 真机验过**(若只闪任务栏不前置的 always-on-top 翻转兜底备用,见 PLAN §12)。
- ⚠️ **悬浮窗 useMedia 只读不发声**(独立 WebView 复用播放 VM 会与主窗双播——多窗变体的双播坑,`play` 分支已堵)。**反向媒体控制已落地**(2026-06-16):悬浮窗迷你播控按钮**转发**给主窗执行(`emitMediaControl`→主窗 `onMediaControl`→`applyControl`,与嘴控汇同一执行口),float 仍不出声;播放态经 `emitNowPlaying(np,status)` 镜像回 float(播/暂停图标翻转)。跨窗联动 **✅ 2026-06-30 Windows 真机验过**(§8.1)。
- **别的程序全屏 → 悬浮窗让位(2026-06-19,仅 Windows)**:float 是 `always_on_top`,Windows 上会盖在别 app 的全屏画面上(游戏 / 全屏视频打扰);**Mac 原生 space 已天然不覆盖别 app 全屏 = 正确,不动**。修 = 壳层 `src-tauri/src/fullscreen.rs::foreground_fullscreen()`(整块 `#[cfg(windows)]`:Win32 `GetForegroundWindow` + 前台窗口矩形 vs `MonitorFromWindow` 的 `rcMonitor` 比对,铺满整块显示器 = 全屏;排除桌面 Progman/WorkerW + 我们自己进程的窗口,免误判)+ setup 里 1.5s 轮询线程**仅在全屏态真变化**时 `emit("lw:foreground-fullscreen", bool)`。**Rust 只报事实、显隐决策仍归主窗 JS**(`App.vue::applyFloat` = `floatOn() && !ownFs && !foreignFs`,与 `lw:show-float` 同构;自愈式去重读实时开关值),Mac 不发此事件、`foreignFs` 恒 false 维持原行为。`windows` crate 复用 Tauri 已拉的 0.61(`[target.'cfg(windows)'.dependencies]`,不引入新版本)。属 §8.1「Windows 专属」一类(轮询前台 / Win32 行为 Mac 测不出),**✅ 2026-06-30 Windows 真机验过**(全屏让位 + 不误判),见 PLAN §12。
- **托盘「显示悬浮窗」**(2026-06-16):✕ 关掉悬浮窗后,托盘菜单一项重开(`show_float`→壳层 emit `lw:show-float`→主窗置 `ui.float.enabled='1'`+`setFloatVisible(true)`,持久化由主窗收口);比绕设置页顺手。
- **失败任务「重试」已落地**(2026-06-16,轻量版、不等 JobRunner):仅影音解析/组件下载失败带 `TaskRetry::MediaPlay{page_url,audio_only}` 载体,UI 显「重试」直连重放(`media_retry` 命令→`media.play`,按钮不绕 LLM,§7.1 哲学);auth **不再算失败**——改为记下待重放、扫码登录成功后**自动续播**(2026-06-18,§7.1),不出重试钮。通用 JobRunner 重试仍后置。
- 排序立场:**不做优先级排序**(robot 配置病)——通知「最新优先 + 自动淡出」、进行中「钉住」。
- **打包 / 卸载 / 默认自启**(2026-06-17,用户拍板):
  - **Windows 只发 NSIS、不发 MSI**:`bundle.targets` 由 `"all"` 改成显式清单 `["app","dmg","deb","rpm","appimage","nsis"]`(= all 去 msi);Mac 仍 app+dmg、Linux 仍 deb/rpm/appimage,CI(release.yml 三平台)零改。NSIS 钩子灵活、官方推荐;MSI 卸载清理麻烦故弃。
  - **卸载清自启残留**:开机自启的注册表项是 `tauri-plugin-autostart`(auto-launch 0.5.0)**运行时**写的、安装器不认 → 默认卸载会残留孤儿启动项。补 `src-tauri/installer-hooks.nsh`(`NSIS_HOOK_POSTUNINSTALL`)删 auto-launch 在 Windows 写的**两条**键(值名 = `package_info().name` = 产品名 `larkwing`):`…\CurrentVersion\Run`(启动项)+ `…\Explorer\StartupApproved\Run`(任务管理器「已启用」覆盖位)。`nsis.installMode=currentUser` 保证卸载器以当前用户身份跑、HKCU 命中正确。**改 `productName` 要同步改钩子里的值名**。
    - **升级不删、真卸载才删(2026-06-19,修「升级后自启莫名变关」)**:钩子两条删除用 `${If} $UpdateMode <> 1` 守卫。原因——用户走「新包提示卸载旧版」= Tauri NSIS **升级**流程,它用 `/UPDATE` 调旧卸载器并置 `$UpdateMode=1`;若无条件删,升级会把用户原有自启状态冲掉,而 app 侧「首启已默认」标记又跨升级残留(升级不动用户数据)→ 新版不补开 → 升级后自启变关。守卫后:升级保留自启(延续用户状态)、真卸载才清(免孤儿)。`$UpdateMode` 由模板在 `un.onInit` 解析命令行设好、本宏插入卸载 Section 可直接读(模板已 `!include LogicLib.nsh`)。**注意时间差**:旧版本编进去的卸载器没这条守卫 → 本改只护「以后」的升级;「当前版→修好版」那一跳仍被旧卸载器删一次,靠下面的 `.v2` 标记补。
  - **正式版默认开机自启**:首启在 `lib.rs` setup 里按产品默认 `enable()` **一次**(内部标记 `system.autostart.defaulted.v2`,app 级、**不进 `APP_SETTING_KEYS`**、直接走 `store.settings`),之后全交设置页开关(关闭入口已有);用 auto-launch 自己的 `enable()`(而非装机写注册表)保证与 `is_enabled()`/`disable()` **零漂移**(§6.8)。**仅正式版生效**(`!cfg!(debug_assertions)`):dev 自启指向临时调试程序、连不上本地前端(前端开关同样 dev 禁用),日常 Mac 开发不被塞 LaunchAgent。标记落了不再自动开 → 不与用户日后手动关掉打架。
    - **标记带版本号、升版 = 一次性重开默认(2026-06-19,配合上面升级守卫修同一 bug)**:标记键从 `system.autostart.defaulted` 升到 `…defaulted.v2`。已装机器升到「修好版」时缺 `.v2` → 首启重新 `enable()` 一次,补上升级当跳被旧卸载器删掉的那次自启(旧卸载器是被替换的旧版本编进去的,升级守卫管不到这一跳)。**只重置一次**(重开后落 `.v2`,之后照旧不与手动关掉打架);旧键 `.defaulted` 留作惰性垃圾、无害。**代价(用户 2026-06-19 拍板接受)**:曾手动关掉自启的老用户,这一次升级会被重新打开一次(之后再关就记住)。**未走「自启状态进 DB + 每次校正」的更稳方案**——那会软化 §6.8「自启不进 DB」,用户选了改动更小、与现有「首启默认」一致的一次性重置。
  - **只能 Windows 正式版真机验**(§8.1,新增 watch-item):装 → 首开自动进自启 → 重启确认静默缩托盘 → 设置关 → 重启确认没起 → 卸载后 `reg query` 那两条键皆无。**升级链(本次新增)**:已装**旧版本**(无守卫)升到修好版,修好版首启把自启**补回一次**(`.v2` 重置);从带 `.v2`+守卫的版本再**升级**到下一版,自启延续不丢(`$UpdateMode` 守卫生效、不被删)且**不重复**重开(`.v2` 已落)。
- **发布流程 / 版本说明(「发布介绍」)**(2026-06-17 落地):
  - **版本号一条命令改全(2026-06-19 收口,原「5 处手工同步」作废)**:跑 `scripts/bump-version.sh X.Y.Z`。它改 ① **Rust 工作区版本**(根 `Cargo.toml` 的 `[workspace.package].version`;两个 crate 用 `version.workspace = true` 继承 → Rust 侧单源)+ ② **前端/Tauri**(`package.json` + `src-tauri/tauri.conf.json`;app 版本 / 安装包 / `getVersion()` / Release 标题都取 tauri.conf,`getVersion` 见 `backend.ts`)+ `cargo check` 同步 `Cargo.lock`。Rust 与 npm 两个生态本就分家、没法共用同一字面量,脚本把这两处一次写齐 —— **别再手动逐处改**。(`package.json` 的版本 app 其实不读、纯 npm 元数据,脚本顺手保持一致。)
  - **「发布介绍」= CHANGELOG.md 驱动、CI 自动填**:`CHANGELOG.md` 每版一节 `## x.y.z — 日期`;release.yml 的「取本版本更新日志」步骤在打 tag 时抽「`## <tag 去 v>` 到下一个 `## 数字`」之间的内容,喂给 tauri-action 的 `releaseBody`(`shell:bash` 跨 mac/win;抽不到则兜底一句)。**发新版只需在 CHANGELOG 顶部加一节,Release 正文自动带上、不再手填**;tag 早于该机制的旧版本(如首个 0.1.1)其草稿正文为空,手贴 CHANGELOG 对应节即可。
  - **触发 = push tag `v*`**:`git tag vX.Y.Z && git push origin main vX.Y.Z` → tauri-action(`releaseName: Larkwing __VERSION__` 自动取 tauri.conf 版本、`releaseDraft:true`、`prerelease:false`)建 **draft** Release 并挂安装包。**draft = 真机验收闸**:下 Windows 包验自启全链(见上条 watch-item)→ 验过再手动 **Publish**。
  - **CI 平台 = mac + windows**(release.yml matrix,Linux/ubuntu 已砍)→ Release 只出 macOS `.dmg` + Windows `.exe`;`tauri.conf` 的 `bundle.targets` 仍留 `deb/rpm/appimage`,无 Linux runner 时空转无害、加回 Linux 即生效。
  - **trunk-based**:代码 / tag 直接进 `main`(无 PR / 分支流);本机未装 `gh`,Release 走网页操作(SSH 推送,§见记忆 HTTPS 被代理挡)。

### 7.7 远程渠道:Telegram / 钉钉 bot(2026-06-17 落地;手机上跟旺财对话)
- **物种 = 交互渠道(§5 species ②)**,不是工具:引擎边界适配器 + **复用 turn loop**,不碰 ToolCtx、不内嵌人格。core 新模块 `channels/`(`telegram.rs` / `dingtalk.rs` 一渠道一文件 + 监督器),**单 crate 不拆**;壳层只 `ChannelSup` 监督(boot 起 / `reload_channels` 停旧起新,顶层 spawn 用 tauri runtime——core 不依赖 tauri,§6.1)。
- **复用现成,engine 零改**:入站文本 → `channels::drive_turn`(会话映射查 `store/channels` 域 → `inject`(在飞)或 `send_message`(空闲)→ 消费 `Receiver<TurnEvent>` 攒 Delta 到 Done)。inject-or-send 与桌面前端同语义;回访同一 chat 续历史(映射到固定 conv_id)。
- **不流式、攒到 Done 一次发**(两家都不支持流式);长消息 `split_message`(Telegram 4096)。
- **不按渠道注入 prompt**(破前缀缓存,§7.5 同理):MVP **纯文本**(Telegram 不带 parse_mode 绕开 MarkdownV2 转义坑;钉钉 text 类型)。富格式以后走输出后处理,不走 prompt。
- **出站全走 `net::Client`**(§4.6):Telegram 全程 HTTP 长轮询(`getUpdates`/`sendMessage`,免公网免 SDK;国内需代理由 net 直连失败自动兜底);钉钉「开连接」+ 回复 `sessionWebhook` 走 net,**只有 WS 收消息用 `tokio-tungstenite`**(钉钉国内直连,WS 不经代理;TLS 走 rustls 复用进程级 aws-lc provider)。**别用 teloxide**(自带 reqwest 绕过 net)。
- **钉钉 = 官方 Stream 模式**(WebSocket,免公网,robot 同款):WS 只收 + 回 ACK/pong;回复走 sessionWebhook(HTTP)→ 回合可异步 spawn、不阻塞收循环、不丢 ping。单聊按 conversationId 续接、群聊按 (conv, 发言人) 隔离 + strip @mention(robot 坑 #2/#7)。
- **访问控制(非风控 §9)**:Telegram `allowed_chats` 白名单,空 = 不放行 + 回 onboarding 报 chat id(不静默吞,§3.5);钉钉靠应用可见范围。
- **凭证不过桥**:token/app_secret 走 `set_setting` 的 `remote.*` 写入臂(**不进 `APP_SETTING_KEYS`** → 写得进读不回);设置页状态读 `remote_status`(只报 `configured` bool + 连接态);**凭证已与 LLM key 一并走 keyring**(§6.3,`SECRET_KEYS` 含 `remote.*` 三把)。
- **微信暂不做**(2026-06-17):个人微信无官方 API、企业微信需公网回调,都不合「出站连接、免公网」的家用形态——承载方式想清再说。
- **✅ 2026-06-30 真机/真网验过**:真 bot token / 真钉钉应用、手机收发、续历史、断网重连、钉钉 WS 在 Windows 连通——见 PLAN 远程渠道 watch-items。

---

## 8. 平台 / 环境陷阱(踩过的坑,务必记住)

### 8.1 WebView2 ≠ WKWebView(头号陷阱)
- 目标 = Windows(WebView2),开发在 Mac(WKWebView)。**WKWebView 更宽容,会掩盖一整类只在 Windows 暴露的 bug。** 已实锤:
  - B 站视频只有声音(黑屏)——WebView2 解不了 HEVC/AV1;修 = `resolver.rs` 强制 `vcodec^=avc`。
  - **本地电影有画面没声音(2026-06-19,上面那条的镜像)**——BD 国英双语压制片音轨常是 **AC3/E-AC3/DTS/TrueHD**,WebView2(Chromium)解不了 → 视频轨(H.264)正常、音轨静默(Mac WKWebView 走系统解码器有声,故漏网)。根因 = **网络路径早强制 avc+m4a 兜底,本地 `/f/` 直传却原样喂文件、漏了这道兜底**。修 = "探测→只转处理不了的那部分"(§7.1,用户拍板「按需」):**BMFF(mp4/mov)读 `moov` 盒探测**(`media/probe.rs`,**不下 ffmpeg、不跑子进程**,逐盒 seek 跳过 mdat、子串匹配 fourcc)——全兼容则原生 `/f/` 直传秒开,音轨 AC3/DTS 或视频 HEVC 不兼容才取 ffmpeg 走 `/m/`(兼容轨 `-c copy`、不兼容轨才转:音→AAC、视→H.264 `yuv420p`);**mkv/avi 等容器 WebView2 本就放不了 → 必经 ffmpeg 转封装**(先确保 ffmpeg、`ffmpeg -i` 探编码、再按需 copy/转)。视频转码(HEVC→H.264)**吃 CPU、弱机可能跟不上 1x**(preset/硬件加速是真机调优项)。**✅ 2026-06-30 Windows 真机验过**:AC3/DTS 真片有声、普通 mp4 仍秒开不下 ffmpeg、HEVC/mkv 能放且 CPU 可接受、`?t=` 换台、进度条(mvhd / `ffmpeg -i` 时长)、5.1 转 AAC 声道均 OK(AV1 是否真要转的省 CPU 优化留后续)。
  - 全屏闪烁 / 退出穿帮——HTML5 `requestFullscreen` 与 DWM 打架 + 透明窗放大穿帮;修 = 改走原生窗口全屏 `win.setFullscreen`。
  - 滚动条占布局宽度跳动(§6.7 `scrollbar-gutter`)、唤醒标定**性能**坑(`KeywordSpotter::create` Mac 264ms 不暴露、Windows 卡分钟级)。
  - **藏托盘的主窗仍 60fps 空烧 CPU**(2026-06-17,Windows 实测 ~3.3%):关主窗 = `hide()` 进程不退(§7.6),而主窗 `transparent:true` 让 Chromium 遮挡检测失效(透明窗永不算"被挡")→ 隐藏后 RAF **不被自动节流**;加之动画循环本来没有可见性判断 → 背景 canvas + 遛弯 `roamFrame` 藏起来照样满帧空跑。修 = `usePageVisible`(`visibilitychange` + 壳层 `lw:win-visible` 事件**双触发**,后者只为 main 发否则关悬浮窗误停主窗)+ `useRafLoop`(不可见即 `cancelAnimationFrame`);所有 canvas 背景(Neon/Hologram/Hud/Starfield)与 `MainLayout.roamFrame` 都改走它。**新代码起 RAF 循环一律用 `useRafLoop`,别再裸 `requestAnimationFrame` 自调度。** 浏览器验过暂停逻辑(88→0→88 帧/秒);藏托盘后 CPU≈0 **✅ 2026-06-30 Windows 真机验过**。
  - **MSE/shaka 自适应播放(§7.1 播放架构换 MSE)**:① **B 站 DASH = 2026-06-20 Windows 真机验通 ✓**(拖进度条音画同步,shaka/MSE 在 WebView2 立住);配套 yt-dlp 解析必须 `--proxy ""` 强制直连(否则被用户全局代理带歪、TLS 被掐,§7.1)。② **本地 fMP4-HLS(Stage 2)✅ 2026-06-30 Windows 真机验过;此前已用 Chromium 裸 MSE 把黑屏链路全程复现+修复实证(2026-06-20)**:踩了**三层坑**(逐层复现):**(a) mpegts 段黑屏** —— shaka 的 mux.js 把 mpegts 视频 transmux 成 fMP4 在 WebView2 失败 append code 3015/3016;`-output_ts_offset` 修不了(病在 transmux 不在 PTS)→ 改 **fMP4 段**绕开 mux.js。**(b) 段 tfdt 恒为 0** —— 输入 seek 出的分片 baseMediaDecodeTime 归零,不修各段堆 0 秒 → `probe::patch_segment_tfdt` 改累计起点。**(c) 真黑屏主因 = 多声道 AAC 被 MSE 拒 append** —— AC3/DTS 5.1 转 AAC 若声道布局不明确,Chromium/WebView2 **拒绝 append 整个 init**(报在 video 轨 → 正是用户「video:2 code=3014、buffered=0」),且视频 `-c:v copy`+`-ss` 切不准关键帧(段 6s 出 8s)、视频转码+音频 copy 又把时长写 2× → **修法 = HLS 段一律转码视频 + 下混立体声 AAC**(`build_frag_cmd`)。**复现手法(可复用)= 预览浏览器跑裸 MSE**:浏览器 Chromium 与 WebView2 同 MSE/编解码内核(≠ Mac WKWebView),把 relay 会产出的 init/段喂 `SourceBuffer.appendBuffer`,即可在 Mac 上重现 Windows-only 的 append 失败 + 验证修复(5.1 init 失败→立体声成功、s0/s1 拼接时间轴连续)。✅ 2026-06-30 Windows 真机验过 = 真片端到端(shaka 拼 fMP4 段 + 拖动同步)、HEVC/H.264 重编码段 **CPU 可接受**(always-transcode 的代价已验);兼容本地 mp4 仍 `/f/` 原生(回归不破)。退路 / 后续:fMP4-HLS 仍不成 → dash.js / 合成本地 DASH MPD;CPU 太重 → 「视频已兼容」者走整文件 copy-remux→DASH 省 CPU。前端 shaka 错误日志已打全 code/category/data + `video.error`(生产版也带,留作 Windows 定位)。
  - **数据「搬家」(datadir)只能 Windows 真机验**(2026-06-18):跨盘 C:→D: 搬(同卷 rename 退化成拷+删的边界)、重启绑新根、`VACUUM INTO` 出的库可开且数据全、原生目录选择器、`explorer` 打开数据文件夹、剩余空间预检(fs2 `available_space`)、拔盘后启动弹恢复弹窗、卸载重装后指针仍在 → 数据找得回。Mac 开发能跑通拷贝/VACUUM/重启逻辑,但盘符/可移动盘/资源管理器行为测不出。详见 PLAN「数据搬家」watch-items。
- **规则**:改**影音播放 / 窗口全屏 / 编解码 / WebView 渲染 / 媒体流 / 唤醒标定性能 / 动画循环 / 数据目录搬家**类代码,默认假设 WebView2 / Windows 文件系统更受限;**Mac 跑通 ≠ 验证通过**,这类**必须出 Windows 包真机验**;设计时主动选 WebView2 也支持的路径(avc、原生窗口全屏)。

### 8.2 唤醒「叫不答应」根因与决策(2026-06-16 拍板:保持 KWS)
- 已定案:**默认唤醒阈值 0.45 太严**(口语 / 偏小的「旺财」KWS 分数 ~0.3,robot 用 0.20 → 8/10,0.45 只接咬字清亮的 → 3/10)。已修(降阈 + 修灵敏度滑块落库)。
- **默认灵敏度拉满(2026-06-18,用户拍板)**:默认 `voice.wake.sensitivity` 从 50(→阈值 0.2)改成 **100(最灵敏 →阈值 0.1,映射 clamp 下限)**。理由 = KWS 召回本就偏弱,「**保障能唤醒再说**」——默认偏召回、先保证叫得应,误触嫌吵的人自己往左调 / 录标定,胜过喊半天不答应。**改默认 = 改三处**(同 §6.8 接线纪律):Rust `voice/mod.rs::wake_threshold` 的 `unwrap_or(100.0)` · 前端 `useSettings` DEFAULTS · `SettingsView` 滑块 fallback。标定流程不受影响(用户主动录制时仍按 calib「宁松勿严」择档,会覆盖此默认)。
- **再定案**:阈值这条路到头,病灶 = **通用 3.3M KWS 模型对真实「旺财」召回太弱**(阈拉到 0.12 仍 0 命中,再压只招误触);同采集链上 ASR(听写)又准又能小声 → 采集 / 麦 / 阈值都没问题,就是 KWS 弱。两段式(KWS→ASR 复核)作废。
- **已拍板(2026-06-16,用户准则):保持 KWS,不做 VAD→ASR。** 召回靠**降阈 + 唤醒标定(录几遍定灵敏度 + 拼写覆盖)**兜,接受 KWS 召回上限,换**秒应零延迟**;放弃的 `VAD→ASR 当探测器`(VAD→SenseVoice 转写→命中即唤醒)代价 = +0.5–1s 延迟 + 每段说话跑一次 ASR,用户不取。**记档:此方案已设计完整,留作后备**——若 Windows 真机召回仍不可接受再启,届时走「KWS 秒应快车道 + VAD→ASR 兜底」混合,不动手前别当 TODO。
- `sherpa KWS 不给分数`(`KeywordResult` 无 score)→ 标定只能**二值阈值扫描**,这是阈值类工作的硬前提。
- **标定宁松勿严**(真机教训):噪声 / 分不开时**绝不取最严档**(旧逻辑取最严 = 灵敏度 0 ≈ 叫不应,正好打脸「叫不应」诉求)→ 偏召回取折中档 + 警告自调;标定**全局、非按人**(刻意绕开多用户 / 声纹线)。
- **唤醒词归属**:唤醒词 = 用户数据,默认值 = 产品决策。**默认唤醒词 = 「小七」(2026-06-16 用户拍板定为正式默认)**:「7274」四音节拗口、「旺财」是暖萌皮名,「小七」两字好喊好召回。代码默认已是「小七」(`voice/mod.rs::wake_keywords`),用户可在设置改;改默认 = 改这一处 + 本条。

### 8.3 Tauri 插件「权限 ≠ 作用域」陷阱(opener,踩过两轮)
- 开外链走 `backend.openExternal`(`plugin:opener`)——`window.open` 在 WebView2 是 no-op,别用。
- **权限和作用域是两件事**:`opener:allow-open-url` 只启用命令、**自带零作用域**,**必须再加 `opener:allow-default-urls`**,否则 `is_url_allowed` 拒一切 → `ForbiddenUrl`。scope 类插件(opener / fs / http …)权限 + 作用域**两样都要给**。
- **capability 编译期烤进二进制**:改 `capabilities/*.json` 后**必重编 Rust**(Vite HMR 不生效);别让 `openExternal` 静默吞错。

---

## 9. 边界 / 暂不做 🔒(别擅自做)

- **离线不做**(非目标,不为它做本地 LLM 兜底)。
- **不复刻** robot 复杂度:插件框架 / RAG / MCP / 风控 / 代码智能 / shell / deploy / test;「第三方可加载插件」是平台级目标,陪伴产品不必(真需要走 WASM)。
- **AEC 不做 / 远场麦阵 / 流式 ASR / 系统级压低其他 app 音量 不做**(§7.5)。
- **ASR 专名拼音纠错词表已砍**(robot 实测很少纠对)——听错兜底 = ①识别文本可见可改 ②两段式有声追问 ③模型上下文自纠。任何人再提此方案,**先问真实纠对率**。
- **PDF 暂不做**(用户 2026-06-15 拍板,别再当 TODO):DeepSeek 视觉只收图不吃原生 PDF;`extract_doc_text` 对 PDF 返回 None。txt/md/源码/.docx 已支持。真要做优先「文字层(pdf-extract)+ 扫描件栅格化转图」混合。
- **媒体附件:图当轮、文档文字进 history(2026-06-29 用户拍板,收窄原「图+文档都当轮」规则)**:**图片**仍**当轮注入、不落库**(后续回合看不到上轮的图、要再发)——因为 vision 对旧图反复计费。但**文档「文字」并进落库的 user 消息内容 → 进 history**:多轮追问文档还在,且文档落**可缓存前缀**(首轮全价、后续 cache-read,§4.8),治掉「问完文档接着问就丢了」。落点 = `engine/mod.rs::send_message` 把 `process_attachments` 抽的 `doc_text` 拼进 `append_message_full` 的内容(图仍走当轮 `parts`);inject 路径 `turn.rs::apply_injection` 同步(落 `llm_content` 含文档、UI 事件仍用 `display` 原文)。代价 = DB 存这份文字、会话记录变大(用户主动塞文档才付,可接受)。**与本条同批落地的上下文处理重构见下条。**
- **上下文处理 = 单一字数预算(model-aware)+ 整块锚定,无条数窗口(2026-06-29 落地)**:原「`WINDOW_MAX=48` 条数窗口 + 48K 字安全阀」两套机制**合并成一个** `engine/context.rs::windowed_start`(字数预算裁 + `WINDOW_CHUNK` 绝对整块锚定保前缀缓存 + 边界吸附防拆 tool 配对)。预算由 `tail_budget_chars(catalog::ctx_window_of(model))` 算:**未知窗口回落 `DEFAULT_TAIL_BUDGET_CHARS=48_000`(零行为变化);大窗口在 [默认, `MAX_TAIL_BUDGET_CHARS=300_000`] 间放大(装文档);小窗口(本地)缩到默认以下(防溢出)**——`= min(MAX, 窗口token/TAIL_RESERVE_DEN)`,CJK 最坏 1 token/字故 token 数当字数上界=安全。`catalog::ModelInfo` 加 `ctx_window_tokens`(数据,2026-06 采集,全 200K–1M)。**判据 = 预算只缩或在 [默认,MAX] 间放大、绝不无界增长;常态(字数≤预算)起点=0、缓存零损伤**。caller 按 `HISTORY_PAGE_MAX=800` 条做 I/O 分页(非语义窗口)、传 `page_base` 给 windowed_start 做绝对锚定。**✅ 2026-06-30 Windows 真机/真量验过**:大窗口模型真装大文档、多轮缓存命中、小窗口模型不溢出、长会话预算迟滞不 creep。阈值单源 `context.rs` 顶部,§13.7「只能真用才能调」。
  - **计价感知(2026-06-30,推翻原「计价方式 defer」)**:`tail_budget_chars(window, billing)` 再吃一个 `catalog::BillingMode`(`Cached` 默认/`Uncached`/`PerCall`):**无缓存 → 预算封到 `DEFAULT`(每轮全价重发尾巴贵 → 少留勤压)**;有缓存/按次 → 按窗口放大(常态)。**只影响压缩(留多少),不改记账数字**(`est_cost_usd` 仍按 token 估、缓存折扣不建模/高估)。目录无 billing 列(当前 provider 都有前缀缓存)→ 默认 Cached、纯 override 驱动。
  - **模型「高级设置」override(2026-06-30,方案A)**:大脑设置每张 provider 卡折叠「高级设置」= 按 model id 纠正**档位/上下文(K)/输入价/输出价/计价方式**(空=用目录猜测,纠错非配置 §3)。机制 = catalog **进程级 overlay**(`static OVERRIDES`,engine `reload_providers` 顶 `set_overrides`,boot+保存刷新)→ `tier_of`/`ctx_window_of`/`est_cost_usd`/`billing_of` 内部先查覆盖再回落目录 → **消费点零改动**(catalog 仍不依赖 store,§6.1)。明文 settings `llm.model_overrides`(非秘密,不进 keyring/白名单);命令对 `model_meta`/`set_model_override`(空壳删条 → reload 刷 overlay + 档位变了重排候选)。
- **「已备未启用」的休眠能力——看到没接 UI 的 core 代码,不要当「未完成的活」去补完**:
  - **声纹 + 多用户 core**(2026-06-12 用户拍板**先放弃多用户**):voiceprints 域 / CAM++ 识别 / 家人 CRUD / 记忆归人 / enroll / 壳层命令全套**已写完测试绿,故意不接 UI**(家人 tab 维持占位 teaser)。`speaker_user=None` 走会话用户是**向前兼容的正确行为,不是 bug**。真做多用户时作**独立里程碑**,补家人 tab UI + 当前用户切换。
  - `Tool::risk()` 闸门、审批分区 = 同类预留,引擎不消费,别擅自接。

---

## 10. 维护本文件(元规则)· 规则变了必须更新

> **这是本文件存在的意义:让规则有一个会被维护的家。规则漂移而文件不更新,等于没有规范。**

### 何时**必须**回来改 AGENT.md
1. **改动任一 🔒 锁定项 或「用户准则」级规则**(§4 全部 + §1 TL;DR + §3 铁律 + §5 架构原则 + §9 边界)→ 这是**宪法级动作**:**先与用户确认,再改本文件**,并在对应条目记下日期与缘由。
2. **新增 / 推翻一条跨模块开发约定**(§6 / §7)、**踩到并定位一类新平台陷阱**(§8)、**解除一个「后置 / 暂不做」边界**(§9)→ 同步更新对应小节(过程级约定可直接改,不必每条都问;撞 🔒 才停)。
3. **一个「已备未启用」能力被启用**、或一条「未决方案」拍板落地 → 把它从 §9 / 现状里移出,更新规则。

### 改哪个文件(别改错地方)
- **规则 / 约定 / 边界变了** → 改 **AGENT.md**(本文件)。
- **模块设计变了 / 进度推进 / watch-item 验收勾项** → 改 **PLAN.md**(它管设计与执行状态;AGENT.md 只引指针)。
- **跨会话的个人化观察 / 协作风格 / 踩坑细节** → Claude 记忆(自动维护;它是 point-in-time 观察,**引用前核对现状**,过时就更正)。
- **CLAUDE.md** 只指向本文件,基本不动。

### 现状 / 验收纪律
- **真机验收单(watch-items)在 PLAN 各 § 末**:一类东西**只能 Windows 真机 / 真网 / 真钥匙 / 真麦验**(媒体渲染、窗口全屏、代理「直连失败→代理兜底」、和风 JWT 全链、语音功效与唤醒召回、真实视觉应答、并发注入交错气泡…)。**Mac / 浏览器预览跑通的不要当「已验证」声称**,落在 watch-item 里据实标。
- 报告口径:测试失败就说失败贴输出;跳过的说跳过;**只有真验过才说「done」**。

### 文档分工再申明
- 宪法管**边界**,计划管**设计与执行状态**,本文件管**规则总纲并指路**。三者不矛盾时以本文件的规则表述为准;若发现 PLAN / 记忆与本文件冲突,**以本文件为规则真相源**,并回头修正不一致的那处。

---

*最后整编:2026-06-30(模型「高级设置」override 落地=按模型纠正档位/上下文/价/计价方式,catalog 进程级 overlay;计价感知压缩补上=无缓存少留;窗口 UI 以 K 计。前次 2026-06-29:§9 收窄媒体规则=文档文字进 history、图仍当轮;上下文处理合并成单一 model-aware 字数预算 + 整块锚定、删条数窗口 WINDOW_MAX;catalog 加 ctx_window_tokens。2026-06-20 新增 §4.11 用户准则)。原合并自 CLAUDE.md(宪法)、PLAN.md(§0–§12 设计与执行状态)及历次会话的踩坑沉淀。*
