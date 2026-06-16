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
12. **(元规则)规则变了必须更新本文件**;改 🔒 / 用户准则先确认(§10)。

---

## 2. 工作方式(协作规范 · 必须遵守)

- **先架构再写,但本阶段 UI 优先**:消费陪伴产品,视觉 / 交互手感是核心。先做「能看见、能点」的纯前端(Vue3+TS,浏览器热更新预览),后端(Rust/Tauri/LLM/记忆)押后、先用假数据顶;前期不急着抽 Pinia store / 分层,UI 摸顺再重构。锁定的技术栈(§4)仍要守住,前端代码以后要能平滑进 `src/`。
- **过程决策直接做、别逐一问**:实现 / 设计层面的小决策**自己拍板、直接写**,简短说明做了哪些决策即可。**只有**真正不确定、涉及产品方向、不可逆、或撞上 🔒 / 用户准则时,才停下来确认。频繁询问会拖慢用户的探索节奏。
- **改 🔒 锁定项前先确认并更新本文件**(§10)。
- **纯视觉调整**可用浏览器预览(本地静态服务器)快速迭代,不必每次原生 build。
- **执行节奏**:PLAN 里多块设计「确认后一起执行」,不逐块边聊边写(用户要求)。

---

## 3. 产品铁律(§ 收敛 / 强默认 / 不暴露)

1. **收敛、强调、强默认**:设置面客观上不小(LLM / 语音 / 远程渠道,体量趋近 robot),不追求完全隐藏;原则 = 第一层只放高频少数项,其余分层可达。第一屏不是设置页;唯一不可避免的首次设置 = LLM 的 key(友好填一次,或读环境变量)。
2. **用户只面对一个 7274,绝不暴露 agent / 插件 / prompt / 配置**:用户侧**不存在**场景 / 模式概念,UI 不做场景切换器。场景退为内部偏置预设,由系统 / 模型自决(多模式时走内部 `enter_mode` 类工具,会话级粘性保前缀缓存;**拒绝**每条消息前的意图分类预调用——杀缓存、加延迟)。发现性由**建议气泡**承担(替用户说一句话,不是模式开关)。
   - ⚠️ 一处放开:聊天「想了想」**展开层**会露工具名 / 入参 / 结果 + CoT 原文(会展开的就是想看机器的人);**折叠层仍守**铁律。是否正式入规则待定。
3. **引导式上手**:示例、建议气泡;普通人靠「看到能点的」探索,不读文档。
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
- **科幻优先**:当前默认观感 = 科幻(玻璃 / 辉光 / HUD),性格仍是亲和陪伴;暖萌 = 可选皮肤。
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
- 硬规则:`net.proxy` 关(空)⇒ 一律直连,哪怕某 host 之前被 sticky 标记;换值 / 关代理即清 sticky。net 模块**不碰 store/llm**(守 engine 唯一合流点),解析在 `Engine::resolve_proxy`。
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

### 6.3 llm
- **翻译三处各归其位**(同一逻辑只出现一次):`store::Message → ChatMessage` = 策略,在 engine `build_context()`(1 份);`ChatRequest → 厂商 JSON` / `厂商 SSE → ChatEvent` = 方言,在各 provider 私有 `to_wire()`/`parse_chunk()`(每家一份,内容互不相同)。判别「重复」= 一个变化要不要同步改 N 处。
- **中立 `ChatMessage` 终态**(纸面已定,分期抵达):`User{content} / Assistant{content, reasoning, reasoning_state, tool_calls} / ToolResult{call_id, content}`,`content: Vec<ContentPart>`(Text/Json/Image)。厂商 JSON 永不出 provider 文件。接触面锁死在 engine(唯一构造点)+ 各 provider `to_wire()`,store/IPC/前端不依赖它 → 重构成本不随 app 长大。
- **两阶段错误**:建连前错误(没 key / 401 / 连不上)走 `Err` 立即返回;开流后错误走 `Failed` 事件。
- **取消 = drop Receiver**(provider 内部任务 send 失败即中止断 HTTP),不需额外取消 API。
- **不静默重试**;空闲超时 60s 无增量判 `Failed`。
- **参数无后门**:`ChatOptions` 不留无类型 `extra: Map`——加新旋钮 = 加一个 `Option` 字段(防御性收口)。
- **绑机制不绑模型名**:前瞻模型名(`deepseek-v4-pro` / `gpt-5.5`)无法对官方核实 → 行为绑「文档化机制」,**绝不**硬编模型名分支。
- **本地端点不拒空 key**:vLLM / Ollama 类只要求 Authorization 头存在 → `LlmConfig.api_key` 允许显式占位值(如 `"ollama"`),preflight 别一刀切拒空。
- **key 装配纪律**:`DEEPSEEK_API_KEY` env 优先 → `settings`;改 key = **重建 provider 实例**(不热更新);MVP 明文存,**Windows 发布前换 `keyring`**(待办)。
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
- **few-shot 纪律**:每场景 2–4 段、总预算 **≤800 token**、**至少一段反例**(不该调工具的对话);示例 id 用 `fs_*` 前缀与真实 id 隔开;加载时校验(引用工具 ⊆ 白名单、call/result 配对完整)。**绝不嵌「用户的具体事实」**(有名有姓有属性的虚构人物会被模型当真事——「朵朵」bug);remember 类示范用一次性、自我纠正的事实(不吃香菜)。
- **落库**:messages 加 nullable `payload` TEXT(assistant 行存 tool_calls + 该轮 reasoning;`role='tool'` 行存 call_id/name/status);UI 渲染过滤 tool 行。
- **场景自决**(>1 预设时):常驻基础工具 `enter_mode(mode_id)`,turn 内立即生效(写 `conversations.scene_id` 会话级粘性 → 本轮重建请求);换 persona/few-shot/白名单/options,**不换**记忆 / 历史 / 循环代码;每 turn 最多切 1 次;不调用 = 维持现状。

### 6.6 文案 / i18n(PLAN §6)
- **铁规:core 不产用户可见文案。** 错误过桥的是 `kind`,文案由前端按 locale 选;`TurnEvent`/`AppError`/commands 形状不为 i18n 改动。豁免:FakeLlm 文案、tracing 日志、种子数据(`users.name` 默认值)。
- **前端 = 文案唯一产地**:vue-i18n 单字典,`src/locales/en.ts` 是 `zh-CN.ts` 的精确镜像。加语言 = locales/ 加文件 + 注册一行 + 选择器;后端零改动。
- **对话语言 = 模型跟随用户**,与 `ui.locale`(只管界面 chrome)**彻底解耦**;persona 语言中立(「用对方所用的语言回应」),不分叉 per-locale 人格。**开场白**是唯一 locale 触达人格数据处(scene 加 `openings` map)。
- **同步纪律(易踩)**:`zh-CN.ts` 加 key **必须**同步 `en.ts`,否则英文模式静默回落中文。复核 = 两文件 flatten 后比 key 集 + 占位符集。
- ⚠️ **花括号陷阱**:vue-i18n 把 `{xxx}` 当插值占位符。文案里写字面 `{` / `${...}` 会崩 render(`${中文}` → Message compilation error,整个组件渲染失败,**warn 级**难查——Vue 不把真实 Error 给 console,表现为「tab 点了切不过去」;`${ENV}` → 静默渲染成空)。要展示这类语法**用纯文本描述**(如「留空自动读 HTTPS_PROXY」),或用 vue-i18n 字面语法转义,别写裸花括号。抓这类 render 错最快路 = 设 Vue `app.config.errorHandler` 拦真实错误(比翻 console warn 强)。
- **非字典硬编码中文也要同步**:协议徽章、星期数组(走 `toLocaleDateString`)、`persona.style` 默认值(需与 Rust `DEFAULT_PERSONA_STYLE` 手工同步)等不在字典里的中文,加语言时易漏。
- **布局兜底**:英文比中文长 40–100%,定宽控件按 2 字中文做的会撑爆 → 砍装饰大字距 / 给弹性宽度 / 永远留 `flex-wrap` 折行 / ellipsis 截断。

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
- **yt-dlp = 解析器组件**(用时下载),**mpv 不搬**——播放器长在自己 UI(WebView 解码 + core localhost relay 转发,音视频分离流 ffmpeg `-c copy` 混 fMP4)。
- **进度总线 `tasks`**(影音引入的通用件):进度句柄 **drop 未收尾 = 自动 fail**(防僵尸进度条);label / step = key + params 走前端字典(core 不产文案)。
- **登录一期 = app 内扫码**取 cookie(原生 CookieManager;SESSDATA 是 HttpOnly),**绝不依赖外部浏览器保持登录**。匿名也能跑,首次成功后 `LoginHint` 提示一次。
- 多源立场与 LLM 同构:解析层天然多源,按源分化的只有搜索 + 登录态(`MediaSource` trait),MVP 单源 bilibili。
- 工具三原语:`media_search`(读)/ `media_play`(写,job 型秒回)/ `media_control`(嘴控,按钮直连前端 VM 不绕 LLM);**校验收口 core**,音量跨播放粘住、倍速每次复位(mpv 时代教训)。
- 本地播放链:需知(目录)→ 文件原语找文件 → `media_play` 放行本地绝对路径 → relay `/f/` 本地文件端点(手写 Range)。NAS 挂载盘符 / UNC 是普通路径。

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
- **语音会话模式**(robot channel_format 对应物):**不按渠道注入提示词**(破前缀缓存)。落法 = 交互形态**物化成消息数据**(payload `{input, speak}`)+ 装配时 speak=true 加确定性标记 `〔语音〕` + 一段**常驻法条按数据条件生效**(LAWS「说话守则」)。
- **听写快捷键**:app 内绑定、**输入框打开时才生效**、**不做全局热键**;且**固定不暴露**(不进设置,tooltip 提示)——强默认收口的兑现。
- **英文免手唤醒 = 单独工程**(语音层③后置):KWS 被中文模型锁死(zipformer-zh),ASR / TTS 已支持英文但唤醒词没有;别当 bug。

### 7.6 常驻临场:开机自启 / 托盘 / 悬浮窗(PLAN §12)
- **零新 core 事件**:悬浮窗只是「全局事件车道」(`app_event`)又一个消费者,复用同一份 Vue app / token / 形象 / composable;窗口管理与托盘进**壳层**。
- 形态 = **C(混合可展开)**:收起 = 一体小挂件,展开 = 信息面板(进行中 / 通知两区);常驻锚点 = 系统托盘;开机自启 = `tauri-plugin-autostart`(OS 真相源)。
- **关主窗 = 隐藏到托盘、不退进程**(✕ 首次点出友好气泡兜底);主窗无边框 → 自绘右上角三键。
- **单实例 / 二次启动唤回**(2026-06-16):全程序只跑一个进程。已在运行时再点快捷方式 / 重复启动**不开新进程**——`tauri-plugin-single-instance`(**放最前注册**),OS 把第二个进程的命令行交给已运行实例的回调、第二个进程退出;回调复用 `show_window` 把主窗(可能藏托盘)唤到前台,沿用 `--autostart` 静默语义(自启触发不唤窗)。无 IPC 命令、不需 capability。**OS 转发 + 窗口前置待 Windows 真机验**(§8.1;若只闪任务栏不前置,加 always-on-top 翻转兜底,见 PLAN §12 watch-item)。
- ⚠️ **悬浮窗 useMedia 只读不发声**(独立 WebView 复用播放 VM 会与主窗双播——多窗变体的双播坑,`play` 分支已堵)。**反向媒体控制已落地**(2026-06-16):悬浮窗迷你播控按钮**转发**给主窗执行(`emitMediaControl`→主窗 `onMediaControl`→`applyControl`,与嘴控汇同一执行口),float 仍不出声;播放态经 `emitNowPlaying(np,status)` 镜像回 float(播/暂停图标翻转)。跨窗联动 Mac/浏览器测不出,**待 Windows 真机验**(§8.1)。
- **托盘「显示悬浮窗」**(2026-06-16):✕ 关掉悬浮窗后,托盘菜单一项重开(`show_float`→壳层 emit `lw:show-float`→主窗置 `ui.float.enabled='1'`+`setFloatVisible(true)`,持久化由主窗收口);比绕设置页顺手。
- **失败任务「重试」已落地**(2026-06-16,轻量版、不等 JobRunner):仅影音解析/组件下载失败带 `TaskRetry::MediaPlay{page_url,audio_only}` 载体,UI 显「重试」直连重放(`media_retry` 命令→`media.play`,按钮不绕 LLM,§7.1 哲学);auth 失败不给盲目重试(走登录)。通用 JobRunner 重试仍后置。
- 排序立场:**不做优先级排序**(robot 配置病)——通知「最新优先 + 自动淡出」、进行中「钉住」。

---

## 8. 平台 / 环境陷阱(踩过的坑,务必记住)

### 8.1 WebView2 ≠ WKWebView(头号陷阱)
- 目标 = Windows(WebView2),开发在 Mac(WKWebView)。**WKWebView 更宽容,会掩盖一整类只在 Windows 暴露的 bug。** 已实锤:
  - B 站视频只有声音(黑屏)——WebView2 解不了 HEVC/AV1;修 = `resolver.rs` 强制 `vcodec^=avc`。
  - 全屏闪烁 / 退出穿帮——HTML5 `requestFullscreen` 与 DWM 打架 + 透明窗放大穿帮;修 = 改走原生窗口全屏 `win.setFullscreen`。
  - 滚动条占布局宽度跳动(§6.7 `scrollbar-gutter`)、唤醒标定**性能**坑(`KeywordSpotter::create` Mac 264ms 不暴露、Windows 卡分钟级)。
- **规则**:改**影音播放 / 窗口全屏 / 编解码 / WebView 渲染 / 媒体流 / 唤醒标定性能**类代码,默认假设 WebView2 更受限;**Mac 跑通 ≠ 验证通过**,这类**必须出 Windows 包真机验**;设计时主动选 WebView2 也支持的路径(avc、原生窗口全屏)。

### 8.2 唤醒「叫不答应」根因与决策(2026-06-16 拍板:保持 KWS)
- 已定案:**默认唤醒阈值 0.45 太严**(口语 / 偏小的「旺财」KWS 分数 ~0.3,robot 用 0.20 → 8/10,0.45 只接咬字清亮的 → 3/10)。已修(降阈 + 修灵敏度滑块落库)。
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
- **媒体附件不进持久前缀**:图 / 文档**当轮注入**(省 token,不为历史旧图反复付 vision 费);后续回合 LLM 看不到上轮的图(要再发)。
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

*最后整编:2026-06-16。本文件合并自 CLAUDE.md(宪法)、PLAN.md(§0–§12 设计与执行状态)及历次会话的踩坑沉淀。*
