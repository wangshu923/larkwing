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
| **docs/notes/*.md** | **每条规则的来历**——决策记录 / 破案叙事 / 落地当时的状态与欠账,按主题一文件,原文逐字归档、只追加不改写 | 落地一批新东西时:叙事进这里,AGENT.md 只留规则句 + 落点 + 指针 |
| **docs/AGENT-LOG.md** | 历次整编纪要(原 AGENT.md 页脚整编链)+ 归档的状态快照 | 每次整编在顶部加一段 |
| **CLAUDE.md** | 仅指向本文件(已轻量化) | 基本不动 |
| Claude 记忆(`~/.claude/.../memory/`) | 跨会话的 point-in-time 观察(用户画像、协作风格、踩坑细节) | 由 Claude 自动维护;是观察不是现行法，引用前核对现状 |

阅读顺序:**本文件(规则)→ docs/notes/(那条规则的来历,按需)→ PLAN.md(对应章节的设计与现状)→ 代码**。本文件只给「规则 + 在哪兑现 + 一句缘由 + 指针」;**grep 规则出处时连同 `docs/notes/` 与 `docs/AGENT-LOG.md` 一起扫**(few-shot 泄漏源漏扫 `assets/` 的同款教训)。

> **入库说明**:仓库里只有本 `AGENT.md`;`PLAN.md` / `CLAUDE.md` / Claude 记忆都是**本地文档**(被 `.gitignore` 排除、不随仓库走)。故本文件力求**自包含**——文中「详见 PLAN §X」指向的是开发机上的本地设计文档,clone 仓库的人没有它也不影响理解规则本身。
> (2026-09-07 分家起)`docs/notes/` 与 `docs/AGENT-LOG.md` **随仓库走**,clone 的人能看到每条规则的来历;PLAN.md 仍是本地文档。

---

## 0. 一分钟认识 Larkwing

- **一句话**:面向普通人 / 家庭(含老人小孩)的**暖萌 / 科幻陪伴型 AI 助手**,**Rust 全新重写**(不是现有 Python `robot`)。= 旺财的桌面版。
- **给谁**:消费级产品,目标用户**不是开发者**。核心洞察 = robot 太自由可配置普通人不会用;Larkwing 反过来**强默认、开箱即用、收口**。
- **技术栈一句话**:Tauri v2 壳 + 单 `larkwing-core`(Rust/tokio)+ Vue 3 + TS(MVVM)+ SQLite + DeepSeek 优先(多供应商 trait 化)。
- **现状**:不在本文件维护(状态会漂,规则不该跟着漂)——看 `CHANGELOG.md` 顶部节 + PLAN.md 各批次节;真机验收欠账以 PLAN watch-items 为准。→ 详见 docs/AGENT-LOG.md「分家前 §0「现状」段(2026-07-12 快照)」

---

## 1. 最必须记住的(TL;DR 铁律)

1. **先想清楚方案 → 跟用户确认范围 → 再写代码**;但本阶段是 **UI 优先、即兴探索式**开发(见 §2)。
2. **用户只面对一个 BT**:绝不向用户暴露 agent / 插件 / prompt / 配置 / 场景 / 工具概念。强默认、收敛、开箱即用。
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
2. **用户只面对一个 BT,绝不暴露 agent / 插件 / prompt / 配置**:用户侧**不存在**场景 / 模式概念,UI 不做场景切换器。场景退为内部偏置预设,由系统 / 模型自决(多模式时走内部 `enter_mode` 类工具,会话级粘性保前缀缓存;**拒绝**每条消息前的意图分类预调用——杀缓存、加延迟)。发现性由**建议气泡**承担(替用户说一句话,不是模式开关)。
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
- 项目英文名 **Larkwing**;面向用户中文名 **旺财**(暖萌皮);默认助手名 **BT**(科幻调性;2026-07-10 用户拍板由「7274」改「BT」= BT-7274 的小名,配合「起什么名字就怎么唤醒」——BT 喊得出口〔逼踢〕。2026-07-11 用户再拍板:BT 就是个英文名、**默认唤醒词只有派生的「逼踢」一个,不为默认名做任何特殊处理**,「七二七四」第二喊法退役,见 §8.2)。
  - **名字 = 用户数据,默认 = 产品决策**:用户在「叫我什么」(`ui.pet_name`)改名后,既进 UI(标题 / 输入框 / 悬浮窗),也进模型——persona 用 `{name}` 占位,`engine/context::build_context` 取 `ui.pet_name`(空 = 默认)填入「你是 {name},…」身份句,所以模型自称、被问名都跟随;**唤醒词也从名字派生(§8.2,2026-07-10 起),改名 = 换喊法,一处改全跟**。开场白则**不带名字**(静态文案,避免与改后的名打架,2026-06-17)。
  - **默认名「BT」三处手工同步**(改默认 = 改这三处):Rust `context::DEFAULT_NAME` · 前端 `pet.name`(`src/locales/*.ts`)· 本条。persona 已是占位符、不再硬编名字,故不在同步清单;默认**唤醒词**另有单源 `voice/wake.rs::DEFAULT_WAKE_WORDS`(=派生的「逼踢」一个,防回潮断言钉着「必须恒等于默认名走普通派生」),改默认名连它一起对齐。
  - **UI / core 文案里出现名字也一律 `{name}` 占位跟随 `petName`(绝不硬编「旺财 / 7274」),名字是用户数据不进 i18n —— 详见 §6.6「用户可见文案绝不硬编助手名字」用户准则。**
- **科幻优先(观感)、性格中性(底座)**:当前默认观感 = 科幻(玻璃 / 辉光 / HUD);**默认性格保持中性 / 极简**(2026-06-17 用户拍板,原暖萌默认调子 → 中性):出厂人设只给功能性底座(语言跟随、自然简洁、记忆、诚实、不自称 AI + 放歌先本地后网络),**不预设任何性格倾向 —— 既不萌也不酷,连「不卖萌」这类否定式规定也不写**(那本身就是一种倾向);目的 = 默认适配最多用户。想要某种性格的人自己在设置「我的性格」(`persona.style`)一句话改;**出厂 `DEFAULT_PERSONA_STYLE` 留空**(= 不注入性格层),输入框用占位示例提示可改。落点:`companion.json` 的 persona / 开场白 / 语音应答词 + `DEFAULT_PERSONA_STYLE`(Rust 与 useSettings 两处镜像、值为空)+ few-shot 示范语气均保持平实中性。暖萌热络 = 可选皮肤,不进默认。
- **已否决的名字**(别再提):`Tideripper`(太凶)、`Sunwing`(撞加航)、`Waterwing`(像泳圈)、`Emberwing`(撞游戏)。

### 4.2 产品定位 🔒
- 消费级、面向普通人 / 家庭,**不是开发者**;架构支持多用户(每人各自记忆)。**多用户第一步「渠道归人」已启用(2026-07-03 用户拍板,进 v0.2.4)**:家人页(加/删/改名)+ 手机渠道对话指认给家人 → TA 说的「提醒我 / 我喜欢」归 TA 自己(§7.7);**没有「切换用户」概念**(§3 零新概念:谁说话就是谁,桌面打字恒 = 会话归属者)。**声纹识别(桌面语音归人)= 第二步,2026-07-04 已落地并随 v0.2.6 发布(§9;声纹 enroll 的真机验以其所属会话记录为准)**:enroll 录 3 段 + 置信「宁可不认绝不错认」(≥0.5 且比第二名高 ≥0.10)+ 主人可在回忆页看/管家人记忆(切视图非切身份);首个场景 = 闲聊陪伴。

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
- 已确立的双源同步点(改其默认即触发本条、逐处对齐):参考音体检阈值(2026-08-21,单源 `voice/denoise.rs`:底噪 −55dB / 削波万分之一)· 默认名 BT(§4.1;2026-07-10 由 7274 改)· persona 留空(§4.1)· 默认唤醒词「逼踢」(单源 `wake::DEFAULT_WAKE_WORDS`,2026-07-11 砍「七二七四」= 与显式起名同一条派生路无特殊处理;唤醒词本身已无独立设置 = 名字派生,§8.2)+ 灵敏度 100(§8.2)· 皮肤 scifi(§6.7)· 代理地址(§4.6)· 文件授权圈三档语义 + 出厂基线「下载/桌面 = 可存入」+「仅这次 = 本回合」+「渠道/语音恒仅这次」(2026-07-30,§7.2,单源 `tools/guard.rs`)· 倍速范围 / 档位表(2026-09-07,core `SPEED_RANGE` ↔ 前端 `RATE_STEPS`,用户拍板十档 0.5–3)· 续播读侧三闸 30s / 90s / 10min + 落盘节拍 30s(单源 `media/mod.rs`;节拍用户拍板,三闸过程默认**待确认**)· 片头片尾检测常量(`introdetect.rs`:开头 360s / 结尾 240s / 邻居一致容差 2s)与前端跳过判定(`VideoOverlay`:拖动判定 2.5s、片尾倒计时 3s〔用户拍板〕、回看口 5s、OSD 0.9s;除倒计时外**待确认**)· 快捷键键位表(`composables/useMediaKeys.ts`,视频 / 音频两面共用一张,用户拍板见 §7.1 ★播放体验五批 + ★音乐播放二批)· 封面常量(2026-09-07·二批用户拍板:relay `COVER_MAX_EDGE` 512 / JPEG q85 / 缓存 16 张;侧车图名单 `COVER_SIDECAR_NAMES` cover / folder / front / album;下载嵌封面 2000px / q90 **待确认**)· 播放模式默认(歌单列表循环 / 单曲与视频放完就停,`PlayMode::default_for`,用户拍板)。
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
- **数据根可「搬家」(`datadir`,用户拍板):别再假设 `app_data_dir` 就是数据根。** 锚点 = OS 默认 `app_data_dir`(永不搬,住 `location.json` 指针);`datadir::resolve(anchor)` 在 boot 最先跑,**之后 store / 日志 / media / voice 全部 `root.join(...)` 派生**;指针指向的目录不在 → 回落锚点 + `data_missing` 弹恢复弹窗,**绝不静默在默认位置重建空数据**。搬家 = 拷可重建子树 → `VACUUM INTO` 快照 → staging 原子改名 → 翻指针(提交点)→ 立即重启。**判据:写任何「数据放哪」的路径都从生效根派生,绝不直接 `app_data_dir()`;DB 里绝不存绝对路径**(只存相对文件名,整棵子树挪走自洽)。→ 详见 docs/notes/data-store.md「数据根搬家 datadir / 一键备份 / 从备份恢复 / 聊天搜索」
  - **一键备份**:`datadir::backup_to` 导出 `larkwing-backup-<时间戳>.zip`(`VACUUM INTO` 快照 + `voice/clones/`;可重建的模型 / 缓存 / 媒体 / 日志不收),纯导出不翻指针不重启;命令 `backup_data`。
  - **从备份恢复**:选 zip → `restore_precheck`(zip 结构 + SQLite 魔数 + **迁移版本前向检查**,来自更新版本的备份拒并指路「先升级」)→ `stage_restore` 解到 `<root>/restore-pending/`(zip-slip 防护、未知条目跳过)→ 重启;**真正落位在下次 boot `Store::open` 之前**(`apply_pending_restore`:现库连 `-wal/-shm` 挪成 `larkwing.db.pre-restore-<时间戳>` 保险副本 → 备份库就位 → 克隆音色合并,任一步失败逆序回滚);成败经 `data_location.restored` 弹 toast 绝不静默。恢复管内容不管位置。Windows 正式版密钥在 keyring 不进备份 → 换机恢复后 LLM key 要重填。
  - **聊天搜索**:`ChatRepo::search_messages` 跨会话 `LIKE` 子串、排除 tool/event 行、按用户隔离;仍是 substring 不上检索核心(§13.9 deferred)。

### 6.3 llm
- **翻译三处各归其位**(同一逻辑只出现一次):`store::Message → ChatMessage` = 策略,在 engine `build_context()`(1 份);`ChatRequest → 厂商 JSON` / `厂商 SSE → ChatEvent` = 方言,在各 provider 私有 `to_wire()`/`parse_chunk()`(每家一份,内容互不相同)。判别「重复」= 一个变化要不要同步改 N 处。
- **中立 `ChatMessage` 终态**:`User{content, parts} / Assistant{content, reasoning, reasoning_state, tool_calls} / ToolResult{call_id, content, parts}`;厂商 JSON 永不出 provider 文件,接触面锁死在 engine(唯一构造点)+ 各 provider `to_wire()`。**工具结果可带图**:`ToolResult.parts` + `Tool::run_output`(默认包 `run` 纯文本,要回图的工具覆写),四供应商出向各自处理;**chat-completions 协议不容 tool 消息带图 → 降级纯文本**,非视觉模型(catalog `vision`)一律降级;图不落库不回放。**丢图必留话**:丢弃 ToolResult 图片时四方言共用单源 `llm::tool_result_text` 在结果文本追加「附带的 N 张图片没能传给当前模型…」,绝不无声丢弃(否则 web_render 文本「已附上截图」= 给模型喂假前提)。消费者:web_render 截图 + `read_image`。→ 详见 docs/notes/llm-providers.md「中立 ChatMessage 终态 + 工具结果多媒体 + 丢图必留话」
- **两阶段错误**:建连前错误(没 key / 401 / 连不上)走 `Err` 立即返回;开流后错误走 `Failed` 事件。
- **取消 = drop Receiver**(provider 内部任务 send 失败即中止断 HTTP),不需额外取消 API。
- **不静默重试**;空闲超时 60s 无增量判 `Failed`。
- **参数无后门**:`ChatOptions` 不留无类型 `extra: Map`——加新旋钮 = 加一个 `Option` 字段(防御性收口)。
- **绑机制不绑模型名**:前瞻模型名(`deepseek-v4-pro` / `gpt-5.5`)无法对官方核实 → 行为绑「文档化机制」,**绝不**硬编模型名分支。
- **图片按 catalog 视觉能力降级**:非视觉模型收到 `image_url` 直接 API 400 → `catalog::ModelInfo.vision` 列(未知模型按 false,宁可少看图不可打挂回合),`openai_compat::to_wire` 出向把图降级成占位文本;**降级必须客户端收口,「服务商会不会自己占位」不可赌**。`ModelOverride.vision` 三态(None=目录)= 大脑卡高级区「图片理解」,`supports_vision` 先查覆盖再查目录。**占位文本必须引导「用工具处理刚发的图」**(认码 / 转格式 / 读文档与视觉无关,永远走工具),只说「看不了」= 模型转而要用户报路径。**目录子串匹配的排序规则:含子串的特异行必须排在宽泛行之前**(`deepseek-v4-flash-vision-exp` 在 `deepseek-v4-flash` 前、`kimi-k2.5` 在 `kimi-k2` 前,否则被宽泛行抢先命中错杀 vision;测试钉着)。DeepSeek 视觉模型只许图在 user 消息 → **能看图的 chat-completions 模型收 ToolResult 图 = 转交**:`to_wire` 把该轮 tool 消息的图攒进 `pending_tool_images`,在这组 tool 消息之后追加一条 `role:user` 中性说明 + image parts(`flush_tool_images`),非视觉 / 无图字节不变;图仍不落库不回放。牌价「宁可高估」(分时对折不建模)。→ 详见 docs/notes/llm-providers.md「图片按 catalog 视觉能力降级 / vision override / DeepSeek 视觉模型 / 工具图转交」
- **模型清单现查服务商、不写死名单**:设置·大脑「模型」框的 ▾ 经 `LlmProvider::list_models`(默认空)向该接入点拉「这把钥匙能用的模型」(OpenAI 系 / DeepSeek / Ollama GET `/models`、Anthropic `/v1/models?limit=1000`、Gemini `/models?pageSize=1000` 只留 generateContent),解析单源 `llm::parse_openai_models`,全走 `net::Client`;**目录只贴标签(档位 / 能看图 / 牌价,覆盖优先)+ 排序(认识的靶前、组内保服务端原序,不认识的沉底不删,不按名字筛能不能聊天)**;牌价取用口 `catalog::prices_of`。**静态名单 = 必陈的硬编码(§4.11 同族),别再加。** 命令 `list_models` 是 async(§8.4);前端是 **combobox 不是 select**(自由文本保留、按供应商缓存、边打边筛、错误按 kind 给人话、空清单明说「手填也行」)。**厂商预设 = 数据**(`registry::presets()`:千问 / 豆包 / Kimi / 智谱 / 混元 + OpenAI / Gemini / Ollama,**刻意没有默认模型字段**),入口 = 「自己接一个大脑」卡接入点框自己的 ▾,草稿配置经 `list_models_draft` 现拉;弹层 `ModelPickList.vue` 两处共用。**思考方言拆三位**(`Quirks`:`thinking_toggle` / `reasoning_roundtrip` / `enable_thinking_bool`)按预设表派生(`quirks_for_base_url` 按接入点主机,`HOST_ALIASES`),显式设过不覆盖;**绑机制不绑模型名**,按模型的差异不加分支;混元接入点 = TokenHub。**前端两坑**:① `:value` 单向绑定 + 每键重渲染会被 Vue 钉回旧值 → 绑「草稿 ?? 已存值」;② 设置页在 window 听 Esc 关整页,下拉内 Esc 要 `stopPropagation`;③ 新函数落地前 grep 同名(模块级重复 `function` = 语法错整页白屏)。「有没有 /models 端点」的定案手法 = 假钥匙打 `/models` vs 乱路径两组对照。→ 详见 docs/notes/llm-providers.md「模型清单现查服务商 / 厂商预设 / 思考方言三位」
- **本地端点不拒空 key**:vLLM / Ollama 类只要求 Authorization 头存在 → `LlmConfig.api_key` 允许显式占位值(如 `"ollama"`),preflight 别一刀切拒空。
- **key 装配纪律**:`DEEPSEEK_API_KEY` env 优先 → `settings`;改 key = **重建 provider 实例**(不热更新)。
- **密钥落 keyring**:`secrets` 模块把 `SECRET_KEYS`(`llm.api_key` / `llm.providers` 整块 / `crypto.ed25519.private_key` / `remote.*` 凭证 / `net.http_creds`;Ed25519 公钥不在内)存系统密钥串,**不进 SQLite 明文**;读写一律走 `secrets::get/set`。**keyring 仅 Windows 启用**;mac / Linux 开发机回落 `settings` 明文(Keychain 对 dev 二进制弹框太烦,用户拍板),gate 在 `secrets::entry()`。keyring 不可用 → 回落 settings 并 warn,绝不让 app 哑掉;存原文,`resolve_env` 取值时跑;boot `secrets::migrate` 幂等迁 legacy 明文。✅ Windows 真机验过。→ 详见 docs/notes/llm-providers.md「密钥落 keyring(仅 Windows 启用)」
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
- **★计划槽 = agent 会话内工作备忘**:多步 / 多批任务先 `plan_set` 列步骤清单(BASE_TOOLS 控制原语,**全量替换语义**,工具本体无状态只校验 + 回显,解析单源 `tools/plan.rs::parse_args`;常量单源同文件:30 条 / 单条 120 字 / 标题 30 字,超限退回不静默截断);回合循环嗅探结果 ok 才写 `SessionSlot.plan`(会话级瞬态、不落库,重启丢槽 = 再说一句重建)+ 发 `AppEvent::Plan` 全量快照(**刻意不走 tasks 进度总线**);「此刻」缝每回合带一行「计划·title(完成 d/t),接下来:未完项」= 跨回合断链的解;**自检句罗盘化**(带用户原话截引 `QUOTE_CHARS=400` + 未完项 + 「没干完就接着干,别停下来问」),不调 plan_set 也独立生效;LAWS「干长活」点名。**明确不做**:持久化 / 跨会话 / 管理页 / 优先级 / 依赖(跨天归 task 型 job);不叫 todo(撞用户面「待办」)。UI = HUD 计划卡 + 悬浮窗「进行中」一行;eval 守卫 `plan-long-task` / `plan-no-overuse`。→ 详见 docs/notes/agent-runtime.md「计划槽 plan_set = agent 会话内工作备忘」
- **★分头办事 = 子回合委派 `delegate`**:把一件自包含的活派给**新鲜上下文的子回合**独立跑,主回合只拿要点汇报(治探查中间结果灌满主回合)。机制:trait `SubAgent` 接缝(`tools/delegate.rs` 定义、engine 实现,`ToolCtx.agent` 注入;usage 流水锚父回合);子回合**复用同一份回合循环**(`assemble_request` 同源 + `delegate_injection` 物化;Turn ephemeral 模式:会话行一概不落、不点 mood、汇报 = 最后一段收尾文本,`max_rounds=40`);工具面 = 场景白名单 − `SUB_EXCLUDED`(四类排除:会话控制 / 持久知识写入 / 跨回合跨人调度 / 现场交互;**fs_undo 也在内**;**golden 枚举保留集 = 新工具五件套,新工具必显式判定进不进子回合**);**两段式**:`IN_TURN_WAIT` 30s 内跑完当场回,等不完不作废转 bgtasks(report job 唤回合接续);取消级联(同步段 `drop_guard`、转后台 `disarm`);并发 `Engine.sub_active` 满 4 如实退回不排队;深度 1 双锁;grants 共享父回合(「仅这次」含子回合);轮内 join_all 真并发。**`delegate_injection` 必带「你手上只有直接干活的工具」免责段**(LAWS 点名的 plan_set/delegate 与话术指路的 task_status/task_cancel 在子回合被排除,不说清模型照法条调不存在的工具)。**HUD 停止钮 = 通用件**:`TaskView.bg` + `TaskHandle.bind_bg` + 命令 `bg_cancel` 直连 bgtasks 协作旗标(与 task_cancel 同一个,按钮不绕 LLM),长活 job 全 bind;取消话术「按要求停下了」不报「失败」。**明确不做**:嵌套 / 便宜档路由 / 子回合事件桥进想了想 / 子回合持久化 / 排队。常量单源 `tools/delegate.rs`(30s / 并发 4 / 简报 10000 / 轮 40 / 汇报截 4000,用户拍板)。记档:web_render 窗上限 2 与扇出 4 互挤(watch)。→ 详见 docs/notes/agent-runtime.md「分头办事 delegate 子回合 + HUD 停止钮通用件」
- **`Tool` trait**(`tools/`,一工具一文件):`spec()`(name/description/JSON-Schema 参数 / 超时档 / 给 UI 的友好动词)+ `async run(args, &ToolCtx)`;`ToolCtx { user_id, conv_id, store }`(手脚清单不是插件总线)。注册表 `Tools::builtin()`;白名单子集 = 场景数据声明。`Tool::risk()` 元数据 slot 已备但**引擎仍不消费**——动作确认闸(§7.8,2026-07-15)是**动作级**的、走 web_render 内部 + `ToolCtx.confirm` 通用件,与工具级 risk 粒度无关(工具级强制确认对 fs 写原语早被 §7.2 否决,别把 risk 当它的前置)。
- **工具六条预记录约束**(PLAN §3):可并行 / 每工具超时 / 异步两型(turn 内阻塞 + 分离 job)/ 状态可视 / 可中断(取消级联进工具,合成「已取消」ToolResult 保历史完形)/ 流式碎片重组规范。
- **工具写入超长一律退回、绝不静默截断**(§3.5,2026-06-18):模型供给且要落库的字段(记忆 `fact`、备忘 `domain`/`content`、提醒 / 盯天气 `content`)超过字数上限就 `anyhow::bail!` 退回报错(经 `run_tools` 当观察喂回模型),让它精简或拆条重写,**绝不 `.chars().take(MAX)` 默默吃半截**(实锤 bug:萌鸡小队第五季 URL 被 300 字上限悄悄截没,模型却以为写全了)。豁免:派生的展示标签(会话标题取前 N 字、不是数据)、外部内容**读取**(`fs` 列目录/读文本、`web` 抓取已各自「如实告知」截断)。
- **few-shot 纪律**:每场景 2–4 段、总预算 **≤800 token**、**至少一段反例**;示例 id `fs_*` 前缀;加载时校验(引用工具 ⊆ 白名单、call/result 配对完整)。**任何进 prompt 的示范内容绝不嵌「具体、可复用的事实」**——few-shot 只教**工具形状 + 说话风格**:有名有姓的虚构人物会被当真人;带属性的具名清单会被抄进 remember;**头号泄漏口 = `role:tool` 结果行**(模型把示范结果当既知事实,曾据此跳过 fs_find 直奔 media_search)→ 示范里的真实片名 / 歌名 / 城市 / 路径一律换**自标示例的编造占位**(「星海漫游」「D:\\示例片库」)或泛指类别,落空 / 命中类结果不绑真实作品;LAWS「出厂示范」条只立原则不举例,并明写「往记忆 / 备忘写内容只能写用户真实说过的信息」。**工具描述里的示例同样被抄**(尤其 briefing/remember/todo 这类带合并语义的写入工具)→ 描述只教参数形状,真实感路径 / 地名 / 片名去掉或换明显占位,宁可无示例。**排查泄漏源必须盖全三处数据源:工具描述 + LAWS/persona + few-shot(在 `larkwing-core/assets/scenes/`,不在 src/)**。回归守卫 `eval/scenarios.rs::media-local-first`。→ 详见 docs/notes/agent-runtime.md「few-shot 纪律 + 示范内容泄漏三处数据源」
- **落库**:messages 加 nullable `payload`(assistant 行存 tool_calls + 该轮 reasoning;`role='tool'` 行存 call_id/name/status),UI 过滤 tool 行。**同轮多段合并渲染**(用户拍板):带工具的一轮落多条 assistant 行(承重结构,**库不动**),`MainLayout::streamGroups` 把相邻 wang 段**分组进一个气泡**,复制 / 朗读 / 读数收组级;「想了想」药丸跟各自的段、**摆在该段正文之前**(轨迹全是这段话说出口之前发生的事);**展开层 = 一条按时间排的队列**(`TurnTrace.items: Vec<TraceItem>`,Thinking / Tool 交错,live 与回放同构),每条一行人话、**再点一下才露细节**;**在飞也要有东西可看**:`TurnEvent::ToolUse` 带 `id / name / args / result / status`,按字符截断(`LIVE_ARGS_MAX=400` / `LIVE_RESULT_MAX=300`,单源 turn.rs),收尾 hydrate 用落库全量覆盖;无细节的条目不给箭头。「N 步」只数工具条目(`useChat::traceTools` 单源);回执小票收组级排气泡末尾;user 恒单条一组,event 行是系统线,跨天分隔断组。→ 详见 docs/notes/agent-runtime.md「落库 payload + 同轮多段合并渲染 + 想了想时间序两级展开」
- **会话回溯 / 分叉**(用户拍板两动作):用户气泡「从这里重新说」(`ChatRepo::truncate_from` 原地硬删,engine `rollback_conversation` 先 `cancel().await` 再删,**与删会话同口径不弹确认**,原话回填输入框;**软删 / 读路径过滤明确不做**)+「从这里另起新会话」(`fork_before` 无损复制前缀,**恒开 ui 渠道 + 空标题**,不取消在飞);BT 气泡「重新回答」明确不做。**切点恒为 user 行**(repo `ensure_user_cut_point` 校验)。**副作用不回滚**(提醒 / 记忆 / 文件 / usage 留着,「世界已经发生」)。前端两时序坑:① 刚发送的 user 气泡持负数本地 id → 动作前 `resolveUserMsgId` 重拉对账,**startsWith 闸对不上宁可不做**;② 回溯前必须 `turnSeq++` 接管在飞,防迟到 `cancelled` 收尾 pop 削掉新列表尾行。截断后前缀字节不变 → 缓存照常命中。→ 详见 docs/notes/agent-runtime.md「会话回溯 / 分叉」
- **场景自决**(>1 预设时):常驻基础工具 `enter_mode(mode_id)`,turn 内立即生效(写 `conversations.scene_id` 会话级粘性 → 本轮重建请求);换 persona/few-shot/白名单/options,**不换**记忆 / 历史 / 循环代码;每 turn 最多切 1 次;不调用 = 维持现状。

### 6.6 文案 / i18n(PLAN §6)
- **铁规:core 不产用户可见文案。** 错误过桥的是 `kind`,文案由前端按 locale 选;`TurnEvent`/`AppError`/commands 形状不为 i18n 改动。豁免:FakeLlm 文案、tracing 日志、种子数据(`users.name` 默认值)。
- **前端 = 文案唯一产地**:vue-i18n 单字典,`src/locales/en.ts` 是 `zh-CN.ts` 的精确镜像。加语言 = locales/ 加文件 + 注册一行 + 选择器;后端零改动。
- **对话语言 = 模型跟随用户**,与 `ui.locale`(只管界面 chrome)**彻底解耦**;persona 语言中立(「用对方所用的语言回应」),不分叉 per-locale 人格。**开场白**是唯一 locale 触达人格数据处(scene 加 `openings` map)。
- ⚠️ **用户可见文案绝不硬编助手名字(2026-07-04 用户准则,以后别再犯)**:助手名 = **用户数据**(`ui.pet_name`;空 = 默认名 `pet.name` / `context::DEFAULT_NAME`,§4.1),用户可随时改;且「旺财」是**暖萌皮名**,写进中立底座即违反 §5。故凡露出助手名字的用户可见文案(i18n 串 / 模板字面 / mono 徽章 / core 侧渠道话术)**一律 `{name}` 占位**,前端渲染点由 `petName = settings.get('ui.pet_name') || t('pet.name')` 注入;**名字是用户数据、跨语言相同 → 不进 i18n 词典**(zh/en 共用同一个 `{name}` 占位,只加占位符不译名字)。core 侧(如 telegram `ONBOARD_HINT`)用 `Engine::pet_name()`(取主人 `ui.pet_name`,空回落单源 `context::DEFAULT_NAME`,§4.11 不另造硬编)。**判据:改一次昵称,所有露出的名字都跟着变;grep 面向用户处的「旺财 / 7274 / Larkwing」应为零(测试夹具 / 代码注释除外)。** 实锤全量清理(2026-07-04):`旺财` 曾硬编进 11 条 zh 文案、en 各写各的(Larkwing / 7274 /「it」混用)、Memory/Reminders/Ops 徽章写死 `7274`(仅 Settings 用了 petName)、telegram onboarding 硬编「旺财」——已全改 `{name}` 注入。
- **同步纪律(易踩)**:`zh-CN.ts` 加 key **必须**同步 `en.ts`,否则英文模式静默回落中文。复核 = 两文件 flatten 后比 key 集 + 占位符集。
- ⚠️ **i18n 特殊字符陷阱**(`{ } @ |` 都中招):vue-i18n 消息串有自己的语法,字面写进文案就被当语法解析、编译失败 → 整组件 render 抛错 → 该 tab/视图渲染失败(Vue 保留旧 DOM,表现为「**tab 点了切不过去**」),且**warn 级**难查(Vue 不把真实 Error 给 console)。已踩:① `{ }` 插值——`${中文}` → Message compilation error,`${ENV}` → 静默渲染成空;② `@` linked-message——`@BotFather` 之类直接崩(2026-06 远程渠道 tab 实锤);③ `|` 复数分隔符。**对策**:i18n 文案不写字面 `{` `}` `@` `|`;要展示这类语法就纯文本描述,或把含特殊字符的字面量留模板硬编码、**不进 `t()`**(如 `<button>@BotFather</button>`)。抓这类 render 错最快路 = 设 Vue `app.config.errorHandler` 拦真实错误(比翻 console warn 强)。
- **非字典硬编码中文也要同步**:协议徽章、星期数组(走 `toLocaleDateString`)、`persona.style` 默认值(需与 Rust `DEFAULT_PERSONA_STYLE` 手工同步)等不在字典里的中文,加语言时易漏。
- **工具动词朴素、不用叠词(2026-08-16 用户看着「想了想」的轨迹提的)**:`tool.*` 那套 i18n 动词(HUD 工具泡与展开层共用同一份)由「看看文件夹 / 找找文件 / 读读这个文件 / 瞄了一眼时钟」改成「看文件夹 / 找文件 / 读文件 / 看时间」—— 叠词萌化本就与 §4.1「默认性格中性、不预设倾向」相左,列成一条条轨迹时尤其别扭。口径 = **动词 + 宾语、2–5 字、保留结尾省略号**(HUD 要进行时;轨迹行由 `itemLabel` 渲染时去掉省略号)。zh/en 各 56 条一次改齐。
- **布局兜底**:英文比中文长 40–100%,定宽控件按 2 字中文做的会撑爆 → 砍装饰大字距 / 给弹性宽度 / 永远留 `flex-wrap` 折行 / ellipsis 截断。
- **失败别静默(§3.5 的 UI 兑现,2026-06-22 落地)**:用户**主动操作**失败 → `useToast().error(t('toast.*'))` 弹一句友好提示(`composables/useToast.ts` 单例 + 顶层 `ToastHost`,**仅主窗挂**;只用语义 token,换肤跟随;文案仍调用方 `t()` 选好再传,core 不产文案)。⚠️ `useChat.renameConversation` 内局部 `t` 是标题字符串、遮蔽了 i18n 的 `t` → 那一处用 `i18n.global.t`。列表**初载失败**别走空态(会被误读成「没有数据」)→ 走「错误态 + 重试」(共享类 `.lp-error`/`.lp-retry` + `common.loadError`/`common.retry`,见 Memory/Reminders/Ops 三页 `error` ref)。纯**被动**后台刷新(boot / 余额 / trace 补拉)失败可继续 console-only,不必弹。

### 6.7 多皮肤 / 语义 token 架构(§3.6 兑现)
- **唯一色源 = `src/style.css`**;组件**只引用语义 token**(`--bg/--surface*/--line/--accent/--text*/--ok/--warn/--attn/--danger`(各带 `-rgb`)/`--bubble-*`/`--veil-*`/悬浮窗 `--f-*`),**绝不**用皮肤专名或内联 `#5fd2ff`;半透一律 `rgba(var(--X-rgb), a)`。
- **皮肤 = 数据**:每皮肤 = 一个 `:root[data-skin="…"]` 块 redefine 同组语义名 + 一个背景组件。**加新皮肤 = 加 token 块 + 背景组件,组件零改动。** 皮肤存 `users.skin_id`(每用户,默认 `scifi`);boot 过桥设 `<html data-skin>`;跨窗实时同步(`lw:skin` 广播)。
- **形象 / 角色是独立设置 `ui.character`,不随皮肤变**(skin 切观感,character 切谁出镜)。
- **clip-path 会裁掉挂在元素外沿的浮层(2026-07-04 真机实锤)**:切角气泡把 `clip-path` 加在 `.bubble` 上,贴外沿的 hover 浮层(复制/时间/读数,`top:100%`/`bottom:-19px`)全被裁没——「切角没有 hover、圆角有」。对策 = 形状画在 `::before` 背景层(背景/描边一并搬入),元素本身不裁;`.bubble` 的 backdrop-filter 已构成 stacking context,`::before` z-index:-1 稳在内容下。
- **关键陷阱**:scoped 规则(`.x[data-v]`)与全局 `[data-skin] .x` 基础特异度相同 → scoped 后加载会赢。要么 token 化(首选),要么给全局覆盖加 `:root` 前缀提权。
- **原生 `<select>` 下拉弹层无法皮肤化(2026-07-04)**:关闭态的 `.s-input` 已 token 化;但展开的 `<option>` 弹层是 OS/Chromium 渲染,只认**不透明**底色/字色(科幻 `--surface-deep` 带 alpha 会被忽略→ 回落系统白),高亮行等 popup chrome 更控不到。要像素级贴皮得换**自定义下拉组件**(div 模拟 + 键盘/点外关闭),全项目 5 处 select 共用一个。**已做 `SkinSelect.vue`(2026-07-04,v0.2.5)**:普通元素自绘列表,弹层同 ContextMenu(`var(--surface)`+blur14),键盘可达 + 点外关闭;设置页 5 处 select(档位/计价/家人指认/麦克风/识别模型)全换。加新下拉一律用它,别再用原生 `<select>`。
- **列表页共用全局类** `.view-*` / `.lp-*`,新列表页照搬别抄卡片 CSS。**所有滚动容器加 `scrollbar-gutter: stable`**(Windows WebView2 经典条占布局宽,Mac overlay 条看不出 → 真实数据撑满会左移跳动)。

### 6.8 接线纪律(前后端两边各加一行)
- 新增 settings 键:**`useSettings` DEFAULTS ↔ Rust 白名单两边各加一行**。`ui.*` 类自动放行;跨 scope 的同前缀键(如 `voice.*` user/app 各半)**逐键放行,不开通配**。
  - ⚠️ **app 级键其实是「三处」各一行(2026-08-31 实锤补)**:useSettings DEFAULTS + `APP_SETTING_KEYS`(读过桥)+ **`set_setting` 的 match 臂**(写入校验;它是逐键 match、兜底一律拒「不在设置白名单内」)——只加前两处 = 前端读得到、**写不进**(backup.auto.dir 差点漏第三处)。
- OS 真相源的状态(开机自启)**不进 DB**,走独立命令对(`set_autostart`/`autostart_enabled`),薄封装插件防漂移。
- **IPC 事件向前兼容**:过桥的 `TurnEvent` / `app_event` 用 serde **tagged 编码**——加变体对前端是增量、**未知变体可忽略**(给工具进度等未来事件留路);core 类型带 serde 直过 IPC,壳层零转换。
- **Tauri 插件「权限 ≠ 作用域」**(scope 类插件 opener/fs/http… 通用):启用命令的权限(如 `opener:allow-open-url`)**自带零作用域**,**必须**再单独给作用域允许(如 `opener:allow-default-urls`),否则一切被拒;capability 是**编译期**烤进二进制,改 `capabilities/*.json` **必重编 Rust**(Vite HMR 不生效)。详见 §8.3。

### 6.9 组件「用时下载」(包里不带)
- yt-dlp / ffmpeg / 语音模型 / pdfium 等大组件走 `components` 模块**用时下载**到数据目录,**安装包里不带**(性质同浏览器下载文件——所以「不打包 Python」红线不碰)。
- 镜像数据化(`media.gh_mirrors` 等,可热调)+ 校验(有 `SHA256SUMS` 则校验,ffmpeg 不发 SUMS 只靠 TLS,记档)+ PATH 兜底给开发机。**模型 = 数据**,按「语言 → 最强组件」目录化(ModelScope/hf-mirror 优先 + gh 镜像兜底)。

---

## 7. 各功能域约定速查(规则 + 指针)

> 设计与现状以 PLAN 对应 § 为准;这里只记跨会话易错的约定与边界。
> 每条约定的来历(为什么这么定、怎么发现的、当时状态)在 `docs/notes/` 对应主题文件;本节只留规则句 + 落点 + 指针。

### 7.1 影音(PLAN §9)
- **yt-dlp = 解析器组件**(用时下载),**mpv 不搬**——播放器长在自己 UI(WebView 解码 + core localhost relay 转发)。
- **播放分两条路:自适应流走 MSE(shaka)、单流走原生**。判据:`NowPlaying.manifest_url` 有值 → shaka;否则 `stream_url` 挂原生 `<video>/<audio>`。**B 站 DASH 不再 ffmpeg 混流**(重启式 seek 音画错位是固有缺陷修不了):relay 合成 DASH MPD(`Entry::Dash`,`probe::probe_sidx` + `build_mpd`),段经 `/s/` 带防盗链头 + Range 透传;**`/dash/` 端点必须带 CORS**(shaka 用 fetch 拉段);探不到 sidx / 无时长 → 回落老混流。**本地不兼容文件走 HLS 按需切片**(`Entry::FileHls`,`/hls/{token}/index.m3u8` 由 probe 时长合成完整 VOD 列表 + `/hls/{token}/s{N}.m4s` 现切现回),**段一律 fMP4 不发 mpegts**(mux.js transmux 在 WebView2 失败 3015/3016 黑屏),`probe::patch_segment_tfdt` 把段 tfdt 改回累计起点 + `moof_segment_end` 剔 mfra;**段一律转码视频 + 下混立体声 AAC**(`build_frag_cmd`:`-c:v copy` 切不准关键帧;视频转码 + 音频 copy 时长写 2×;多声道 AAC 被 MSE 拒 append 整个 init)。前端 `manifest_url` 对 DASH / HLS 通吃。✅ Windows 真机验过;Mac 可用预览浏览器(Chromium)裸 MSE 复现 WebView2 的 append 失败。→ 详见 docs/notes/media-playback.md「播放分两条路:自适应流走 MSE(shaka)、单流走原生;B 站 DASH + 本地 fMP4-HLS」
- **进度总线 `tasks`**(影音引入的通用件):进度句柄 **drop 未收尾 = 自动 fail**(防僵尸进度条);label / step = key + params 走前端字典(core 不产文案)。
- **登录一期 = app 内扫码**取 cookie(原生 CookieManager;SESSDATA 是 HttpOnly),**绝不依赖外部浏览器保持登录**。匿名也能跑,首次成功后 `LoginHint` 提示一次。
- **需登录的播放 ≠ 失败 + 登录后自动重放(2026-06-18,用户反馈「扫码时还哐哐往下跑、最后失败」)**:`play()` 命中「需登录」时**不再 `bail` 当失败**——记下待重放(`pending_play`,按源,10min 过期作废)+ 发 `AuthRequired`(弹扫码气泡)+ 解析任务**正常收尾**(不标红 HUD、不喂模型「放失败了」),返回 `PlayOutcome::AwaitingLogin{detail}`(工具层据此引导用户扫码、说「登录后会自动接着放」)。扫码成功 `set_cookies` 那一刻**带新 cookie 自动重放**(不绕模型、同嘴控哲学;无 tokio 运行时的同步调用方保留待重放不丢)。未知来源无登录通道才回落「如实退回」。`play()` 返回 `Result<PlayOutcome>`(原 `NowPlaying`),`media_retry` 只看 `Err` 不受影响。
- 多源立场与 LLM 同构:解析层天然多源,按源分化的只有搜索 + 登录态 + **剧集发现**(`MediaSource` trait),MVP 单源 bilibili。
- **多集自动续播**:队列机器**来源无关**(`MediaRuntime` 持 `Playlist{series_key, entries, index, audio_only}`,app 级瞬态派生可丢;`advance(±1)` 只挪 index、`play_entry` 现取现播);剧集发现分路:B 站 `MediaSource::episodes`(合集 ugc_season 优先、其次分P),本地 `local_episodes`(同文件夹 + 同类扩展名桶 + **数字骨架分组**防平铺电影库误触 + 自然排序;**确定式扫描,用户否决了 LLM 排序**)。`NowPlaying.playlist:{index,total,resumed}`;前端 `ended` → 非末集 `media_advance(+1)` 不绕 LLM;嘴控 `next/prev/episode(第 N 集绝对定位)` 共用 `switch_episode`,**队列是 core 单一真相,模型只说集数不碰链接**。**续播规则(2026-09-07 重做)**:路径 / 链接**只用来认剧、不从「传的是第几集」推断意图**;`episode=Some(N)` → 第 N 集从头;`restart` → 第一集从头;否则有进度就接上(停在第 i 集 → 第 i 集 + 集内秒数;看完 → 第 i+1 集;末集看完 = 整部从头);没进度才用传的文件当起点;决策落 info 日志「续播决策」。**续播记忆 = 专用域 `store/media_progress`**(不进记忆系统),**按家记不按人**(迁移 0032,用户拍板),只存集身份 + 相对名,`series_key` 本地用 `local:FNV(目录+骨架)` 单向哈希**绝不落绝对路径**;集内秒数由前端心跳落盘(core 每 30s 真写一次,暂停 / 停止 / 切集立刻;按标题核对防旧心跳串集;短于 10 分钟不记),读侧三闸单源 `media/mod.rs`(<30s 当没看、离结尾 <90s 当看完;§4.11 待确认);单部电影按文件身份续(`local:file:FNV` / `web:FNV`,放歌不记)。→ 详见 docs/notes/media-playback.md「多集自动续播:队列机器 / 剧集发现 / 续播规则 / 续播记忆」
- **番剧多集发现 = 第三路**:番剧是 B 站 **PGC** 体系,与 UGC(BV 号)两个平行命名空间——**番剧集自带的 bvid 拿去问 UGC `view` 端点是 -404,队列 entries 必须存 `bangumi/play/ep{id}` 形**。`extract_pgc` 认 `ep`/`ss` 两形(必须落在 `/bangumi/play/` 路径下防误认)→ `PGC_SEASON_URL`(免 WBI,任何一步不顺 `Ok(None)` 退化单集)→ `parse_season`(载荷在 `result` 不在 `data`;只收 `episodes` 正片,`section` 花絮**不并进队列**);`series_key` 用 `bili:pgc:{season_id}` 前缀,**与 ugc_season 的 `bili:season:` 分开防续播串味**。**番剧搜索刻意不接、追更不做**(均用户拍板)。已知边界:`build_queue` 按 url 精确匹配,分享链带跟踪参数时匹配不上 → 落回自然起点。→ 详见 docs/notes/media-playback.md「番剧多集发现 = 第三路(PGC 与 UGC 两个命名空间)」
- **本地放歌连播**:音频**不套**视频的数字骨架闸——**同文件夹全部音频 = 一个队列**(natural sort,<2 首不成队);`media_play` 并收音频文件夹路径(整夹当列表放、强制只出声);音频 series_key = `local:FNV(目录+audio)`。循环 / 随机的嘴控五动作(loop_one/loop_all/loop_off/shuffle_on/shuffle_off)保留当词汇,**状态归 core、`NowPlaying` 全量镜像、前端 `ended` 改问 core `auto_next`**;单曲循环走前端 `el.loop` 原生无缝;`shuffle_on` 无队列如实退回。(2026-09-07·二批起收成 `PlayMode` 三档,见下 ★音乐播放二批。)。→ 详见 docs/notes/media-playback.md「本地放歌连播 + 循环 / 随机(PlayMode 前身)」
- **音轨选择**:probe 读出每条音轨(BMFF trak stsd fourcc + mdhd 语言 + udta name;mkv 等靠 ffmpeg stderr 语言 + Metadata title),`NowPlaying.audio_tracks/audio_track` 过桥,〔此刻〕带清单让模型把「换英文原声」对到轨号;切换 = 嘴控 `media_control audio_track`(1 起)与播放条 / 视频浮层按钮汇同一口(`media_mode`),**一律「重建管线 + 原位续播」**(relay 带轨号 → ffmpeg `-map 0:a:{n}`,`NowPlaying.resume_at`)。**⚠️ WKWebView 播放中改 `audioTracks.enabled` 不重新路由音频**——就地启停只用来做起播收敛,切换绝不走它。逐轨兼容判定只看选中那条;选轨跨集粘住、新 play() 复位;语言码 → 友好名在前端字典(`media.lang.*`),都没标 → 「音轨 N」兜底。→ 详见 docs/notes/media-playback.md「音轨选择:切换一律重建管线 + 原位续播」
- **★播放体验五批(2026-09-07)**:① **看片快捷键 = 数据表**(`VideoOverlay.KEYS`,分发 / 帮助浮层 / tooltip 同源,加键 = 加一行),全部动作汇到与按钮 / 嘴控同一执行口;键位用户拍板(空格|K 播放暂停、←→ 15s、Shift+←→ 60s、↑↓ 音量、M 静音〔`muted` 旗标不动基准音量〕、Ctrl+←→ 倍速一档、PageUp/Down 上下集、L 选集、S 跳过片头 / 标记、A 音轨、C 字幕、F 全屏、Esc、**H 帮助**);中央 OSD;实体媒体键走 Media Session API;只在视频浮层在前且焦点不在输入框时接管。② **剧集列表**:`playlist_view` 按需取整份标题 + 壳层 `media_playlist` / `media_jump`(与嘴控同一 core 入口),`EpisodeList.vue`(捕获阶段截键)。③ **片头片尾**:`store/media_skip`(手标行 = 锚在某集**从那集起生效**;检测行逐集)+ `media/skip.rs::resolve` 纯函数汇四路(**手标 > B 站番剧 clip_info_list > mkv 章节 OP/ED > 指纹检测**)→ `NowPlaying.skip`;**三入口同一 core 口 `control()`**(嘴控 `intro_start / intro_end / outro_start / skip_intro / skip_clear`、进度条右键、剪刀钮 / S 键);前端自然播进片头即跳(跨 >2.5s 判拖动不跳)+「回看」5s + 越片尾线且有下一集 → **3 秒倒计时**走 `autoNext`;单部电影不跳。B 站 `clip_info_list` 免 WBI、整季为空是常态、**随机 `-10403` 重试一次即过**;**UGC 无 OP/ED 语义,view_points 不接**;章节只认标题说得清 OP / ED 的。指纹检测 `introdetect.rs`(相邻集开头 360s / 结尾 240s,两侧邻居一致 ±2s 才写,**ffmpeg 不在手绝不为它下载**,一次一个、静默)。**明确不做**:从拖动行为学习片头、网上众包跳过库、UGC 章节启发式自动跳。常量 §4.11 待确认。→ 详见 docs/notes/media-playback.md「播放体验五批:快捷键表 / 剧集列表 / 片头片尾(2026-09-07)」
- 工具七原语:`media_search`(读)/ `media_play`(写,job 型秒回)/ `media_control`(嘴控,按钮直连前端 VM 不绕 LLM)/ `media_download`(存·网页音轨,2026-07-27,见下条)/ `lyrics_fetch`(配词,2026-07-27·二批,见下下条)/ `torrent_download`(存·BT,2026-07-29,见下条)/ `ffmpeg_run`(加工·ffmpeg 直通,2026-08-03,见下条);**校验收口 core**,音量跨播放粘住;**倍速 / 播放模式 / 音轨 / 字幕 = 队列级粘住(2026-09-07 用户实锤「1.5 倍看剧下一集掉回 1.0」;循环 / 随机两个状态 2026-09-07·二批收成一个播放模式,见 ★音乐播放二批)**:新点播(`play()`)复位、切集 / 自动续播沿用 —— 「倍速每次复位」的 mpv 教训管的是新点播不是新一集;倍速进 core 状态(`Inner.rate`)随 NowPlaying 全量镜像,UI 调倍速经壳层 `media_mode('speed')` 落 core(与循环 / 随机同一条路),范围单源 `SPEED_RANGE` 0.5–3(下限 0.5 = Chromium 音频渲染器变速保调的下界,更慢直接静音,原 0.25 档在 Windows 上是无声的;前端十档表 `useMedia.RATE_STEPS` 双源同步);音轨切集按**语言**对号(`pick_audio_track`,两集轨序不同不串)、字幕按语言 → 序号 → 关(`carrySubtitle`)。「倍速声音优化」= 变速不变调,浏览器默认已开(Chromium WSOLA),只显式写了一次 `preservesPitch=true`;更高级拉伸算法 / 倍速人声增强评估后不做。
- **★音乐播放二批(2026-09-07·二批)**:① **播放模式一个钮三档**:core `PlayMode { Once, LoopAll, LoopOne, Shuffle }` 取代「循环三态 × 随机开关」,**默认按内容定**(`default_for`:歌单 = 列表循环、单曲 / 视频剧集 = 放完就停,用户拍板);歌单钮轮转 列表循环 → 单曲循环 → 随机,单曲钮只有 放完就停 ↔ 单曲循环,视频无模式钮(嘴控仍可开);随机恒循环;手动上下曲在 Once 之外一律回卷;嘴控五动作名保留、落枚举时归一,结果态经 `MediaEvent::Mode` 发前端(不让前端从动作名反推),`NowPlaying.play_mode` 全量镜像。② **音频快捷键面**:键位表抽成 `composables/useMediaKeys.ts`(视频浮层 + 播放条**共用一张**,按面裁剪;**音频换曲用 N/P,不占翻聊天记录的 PageUp/Down**;R 模式轮转;C 视频 = 字幕 / 音频 = 歌词);接管条件两面同一套;播放条可获焦 + `:focus` 描边,Esc 先收浮层再还焦点给输入框;帮助卡 `KeyHelpCard.vue` 两面共用;**不做** OS 全局字母热键。③ **封面**:`NowPlaying.cover_url` 单一信号(None = ♪ 占位,不假装有图),relay `/cover/{token}` ≤512px JPEG(缓存键 = 来源身份);优先级 **文件内嵌图 > 同目录侧车图 > 网络源封面**(内嵌用 `-c copy` 抽 `(attached pic)` 流,**小写 `-map 0:v:0`,与缩略图排除封面轨的大写 V 正相反**;纯 Rust 标签库因 MSRV 否决;侧车名单 `COVER_SIDECAR_NAMES`;网络源 = yt-dlp `thumbnail` 经 relay 代取,视频也带 → `<video poster>`);起播顺手拿歌手 / 专辑进 `author / album`,**只用已到手的 ffmpeg 不为封面下载**;显示 = 播放条方图(点开「正在播放」大卡:大图 + 三行歌词)· 悬浮窗小图 · Media Session artwork。④ **下载嵌封面**:`media_download` 整理步把源封面转 JPEG(2000px / q90,**原图不裁**)嵌进 m4a / flac(ogg / opus 不嵌);带封面失败自动退回不带封面再整理,**绝不因封面丢下载**。常量单源:relay.rs 顶部 / download.rs / media/mod.rs。→ 详见 docs/notes/media-playback.md「音乐播放二批:PlayMode 三档 / 音频快捷键面 / 封面三源 / 下载嵌封面(2026-09-07·二批)」
- **音乐下载 `media_download`**:把网络页面的音轨存成本地文件(默认系统「下载」夹,需知有音乐目录时模型传 `dir`;用户拍板)。**解析与播放同一条链**,只换下载专用格式串 `resolver::DOWNLOAD_AUDIO_FORMAT = "ba/b"`(有无损就拿存 `.flac`,否则最高码率 AAC 存 `.m4a`;播放用的 AUDIO_FORMAT 刻意不动);落盘 = web_download 同款(`net::Client` 流式 + `.part` + `dedupe_path` 永不覆盖 + 防盗链头原样带上),单文件闸 500MB(单源 `download::AUDIO_MAX_BYTES`);末步 ffmpeg `-c copy` 整理标准容器 + 写标签,**全程不转码(mp3 档明确不做)**,ffmpeg 缺席 / 失败不阻断 = 原样保存如实说。**需要登录 ≠ 失败**(弹扫码)但无自动重下。**批量 = 用户确认后才 `all=true`**(合集 / 分P 没说清先问;eval 守卫 `download-batch-confirm` / `download-batch-explicit`),成系列分离 job 后台逐首下(tasks 进度 `step.audio_batch`),封顶 `BATCH_MAX=100`;**范围参数 `from`/`to`**(1 起含两端,`resolve_range` 越界报「一共 X 首」,超容量退回带现算分段建议)——**话术承诺的能力必须有参数兑现,别留空头承诺**。版权口径:只下用户自己账号能播的内容存本地自用,不碰加密流破解、不接野 API 源。→ 详见 docs/notes/media-tools.md「音乐下载 media_download」
- **命名 / 标签 + 歌词 .lrc + 存量补词 `lyrics_fetch`**:① **干净歌名 / 歌手 = 模型抽取传参**(`title`/`artist`;B 站 UGC 无结构化歌手字段,正则 / 音频指纹两路已否):文件名 `歌手 - 歌名.ext`(没给退视频标题原样,代码不猜),**UP 主绝不进 artist**(挪 `comment`)。② **歌词两级来源落同名 .lrc**(`media/lyrics.rs`;内嵌歌词标签不做):平台**人工** CC 字幕(`ai-*` 滤掉)→ **LRCLIB**(时长容差 3s,**「宁可不配,绝不配错」**;纯音乐不配);找不到如实说不留空文件;**歌词失败绝不影响下载成败**;已有同名 .lrc 跳过不覆盖;野歌词 API 不接。③ **存量补词原语 `lyrics_fetch`**:**绝不改动音频原件**只旁挂 .lrc;歌名优先读文件标签,缺标签点名让模型带参重试;≤20 个回合内跑完、更多转后台 job,封顶 200;**批量收尾回报唤回合**:job 收尾插一条 due=now 一次性 `jobs` 任务(kind **`report`**,与提醒分家:标签「✓ 忙完了」、桌面不念、`list_*` 视图滤掉;**新增 activity kind 必须同步 `channels::outbound_loop` 放行表**),汇总单源 `compose_batch_summary`,点名封顶按**字数** 2000 不按条数;**不给它造「写名单文件」机制**,处置归模型。④ LRCLIB 带可识别 UA;外语歌 zh 字幕常是翻译,话术标注。⑤ **检索简繁双轨**(港台老歌在 LRCLIB 普遍繁体收录):查询含简繁双向变体 + 只按歌名兜底(歌名归一相等闸)+ 挑时长最近;**台/臺歧义特判**;**lyrics_fetch 描述明写「代写 .lrc 绝不编造 [mm:ss] 时间轴」**;Apple 音乐.app 不读 .lrc 旁挂(预期行为)。⑥ **正文繁 → 简**:`write_lrc_beside` 落盘前过 `to_simplified_lyrics`(所有写 .lrc 的路都过这一收口);日文歌跳过;「一律转简」是面向大陆家庭的产品默认不给设置项;模型经 fs_write_text 写的不过此收口 → 技能补「搜回来的词先核三样」。→ 详见 docs/notes/media-tools.md「命名 / 标签 + 歌词 .lrc + 存量补词 lyrics_fetch + 批量收尾回报 + 简繁双轨」
- **播放条滚歌词**:放本地音频时旁挂同名 .lrc 整份原文随 `NowPlaying.lyrics` 过桥(歌词是数据不是文案;**仅 audio_only 本地路带**;非 UTF-8 按 GB18030 回退、>200KB 不带,单源 `media/lyrics.rs::sidecar_lyrics`);前端 `useLyrics.ts` 纯函数解析(同行多标签 = 重复句;`[offset:±ms]` 正值 = 提前;+0.2s 提前量宁早勿晚),PlayerBar 上方滚**当前句**,「词」钮 `ui.lyrics` 默认 '1'。**无时间轴的纯文本 .lrc 不显示**(一行放不下,编时间轴假滚比不显示糟)。悬浮窗 / 视频浮层不接。→ 详见 docs/notes/media-playback.md「播放条滚歌词」
- **进度条 hover 读数 + 缩略图**:① 时间气泡 = 共用件 `useScrubHover.ts`(视频浮层 + 音频播放条共用;`x→百分比` 减半个拇指宽;`duration<=0` 恒不出;气泡夹在面板里不夹在进度条里;不换掉 `<input type=range>`);音频条也改「拖动只动视觉、松手才 seek」。② **本地缩略图**:`NowPlaying.thumb_url: Option`(有值 = 能出图,前端只认这一个信号)+ relay `Entry::Thumb` 与 `/thumb/{token}?t=秒`,**与四条播放臂分开注册**;ffmpeg 走 `Components::ready`(**同步、绝不下载**);抽帧 `-ss` 在 `-i` 前 + **`-map 0:V:0?` 大写 V**(小写 v 会挑中封面图轨,整条进度条全是同一张海报);四道闸单源 `relay.rs`(`THUMB_GRID=10s` / 全局 FIFO `THUMB_CACHE_MAX=48` 不按 token 分 / `Semaphore(1)` / `THUMB_TIMEOUT=8s`);抽不出一律 404,前端降级(失败格不重试,**攒够 3 个不同格才整片放弃**)。③ **网络流 = B 站 `player/videoshot` 雪碧图**(`MediaSource::sprites()`,UGC 单 P / `?p=N` 经 pagelist 取 cid / 番剧 ep 经 season 端点;免 WBI;**`index` 首项是哨兵,`parse_videoshot` 剥掉否则整体错后一帧**);relay `Entry::Sprites` 复用 `/thumb/` 端点(整图有界缓存 → 裁格 → JPEG,两级缓存 `FifoCache<K>`),抓取与 yt-dlp 解析**并行**,`SPRITE_FETCH_TIMEOUT=3s` 绝不拖起播;起播预生成不做。常量(3s / 16 张 / 8MB / q80 / 8192)§4.11 待确认。④ **三个通用坑**:`input[type=range]` 有 UA 默认 `margin:2px`(容器内 `.slider{margin:0}`)· class 名撞车(`.pbtn.track` 已存在 → `.scrub-track`)· 量浮层尺寸用 **`nextTick` 不用 rAF**(rAF 在隐藏窗口不触发)。→ 详见 docs/notes/media-playback.md「进度条 hover 读数 + 本地缩略图 + B 站雪碧图」
- **后台差事登记处 `bgtasks`**(`bgtasks.rs` 通用件,挂 `MediaRuntime.inner.bg`):长活(批量下载 / 配词 / BT / ffmpeg / 解压 / 扫盘 / delegate)的**模型可见性**底座——① 快照两层:每回合〔此刻〕背景一行(≤3 个逐列、再多报个数)+ `task_status` 细看;② `task_cancel` 协作式旗标(正在做的那一项做完就停,停完照常收尾汇报);③ **收尾必有动静**:`BgTicket::finish` / 取消 / 半路断(Ticket Drop 兜 panic)一律插 due=now 一次性 jobs 任务唤回合;④ 卡死看门狗 60s 巡逻、`STALL_MS=10min` 无步进判卡 + abort + 照常汇报。并发上限 20 backstop 满了如实退回不排队;**定期心跳明确不做**(= `reminder_set`+`task_status` 组合);**绝不做**通用任务提交器 / 队列 / 优先级 / DAG;瞬态 app 级,重启即丢 = 汇报不来不是出错;`cap_names` 单源在此。显示端 total=0 不显分母(`progress_frag` 单源)。→ 详见 docs/notes/agent-runtime.md「后台差事登记处 bgtasks」
- **BT / 磁力下载 `torrent_download`**:把**用户给出的**磁力链 / `.torrent` 下成本地文件,下完落文件夹 → 本地播放链全部白拿(`play()` 本就按 `is_local_path` 分派)。**边界 = 不写死任何站的选择器、不内置片源搜索、不维护片源目录**(链接用户给,定位同 web_download;「不去站扒链接」只是引导拦不住模型,用户知情)。**做种策略 A(用户拍板)**:下载期正常上传(tit-for-tat 不传就下不动,这部分关不掉),**下完立即 `Session::pause` 停止做种**。**能力天花板 = 种子健康度,与迅雷(P2SP 服务器缓存池)不可比**:热门种子跑满带宽、冷门救不了,别再重复评估。机制:`librqbit 8.1`(`rust-tls` 对齐全局 rustls,关 axum/webui/sqlx)、`TorrentEngine` **懒建**(不用 BT 的用户不平白发 DHT 包)、`persistence: None`(重启不偷偷续种)、**恒走后台 job**(bgtasks 全套白拿),`bg.running_count_of` 做并发闸。常量单源 `media/torrent.rs`(§4.11 拍板):`TORRENT_MAX_BYTES=50GB` / `MAX_CONCURRENT=3` 满了如实退回 / `METADATA_TIMEOUT=60s` / `NO_PROGRESS_TIMEOUT=5min` / 缺省只下视频扩展名(`only="."` 全下)/ 落盘系统「下载」夹;体积闸与文件清单按 `only_files` 过滤后算。**`.torrent` 比磁力链可靠得多(DHT 走 UDP 正被运营商限速),工具描述明写**;失败话术逐类给因绝不含糊。永不覆盖(`overwrite:false`)+ 放弃种子不删已下字节。**边下边播 = 二期刻意不做**。**新工具四件套 = 注册 builtin() + 场景白名单 + golden 断言 + i18n 键,少一样都算没接完;golden 与数据同源同漏是盲区,收尾必须 grep companion.json**(torrent 曾漏白名单四天)。→ 详见 docs/notes/media-tools.md「BT / 磁力下载 torrent_download」
- **ffmpeg 直通 `ffmpeg_run`**:把 ffmpeg 整个开给模型(剪 / 转 / 抽 / 拼 / 调音量 / 变速 / 截帧 / 烧字幕 = 模型自己的 ffmpeg 知识),**程序不做操作矩阵**;是 §6.3「参数无后门」的有闸例外,闸全在边界:① **args = 字符串数组不是单 String**(零转义直喂 `Command::args`);② **输出永不进 args**:`output` + `dir`(缺省第一个输入旁)独立参数由程序落盘(授权圈 create + dedupe 永不覆盖 + 临时件改名),输入原件一字节不碰;③ 边界闸单源 `tools/ffmpeg_run.rs::scan_args`(纯函数):`-i` 后 token 过授权圈 read,滤镜内嵌路径(subtitles=/ass=/movie=/amovie=)抽出同过闸,拒 `://` / `pipe:` / `-y` / `-n` / `-f concat` / `-f lavfi` / 旁路写文件 flag 族 / **孤零零的裸值**;④ **执行 = 先跑再说不做成本预测**:`media/edit.rs` 回合内等 `IN_TURN_WAIT=30s`,没跑完转 bgtasks(`-progress pipe:1` 解析,**只在真推进时 beat**),`TempGuard` 兜半路死,失败带 stderr 尾巴(2000 字符)回喂模型自纠;⑤ `-c:v h264` 占位 = **确定性替换**成本机最快编码器(`video_encode_args` 单源,结果如实说用了哪个);⑥ **探测形**:不给 `output` = 只探不产,原样返回 stderr 信息横幅(头部 `PROBE_OUT_MAX` 4000 字),「去掉结尾 N 秒」= 先探时长模型自算(media_probe 独立工具被取代)。**长输出的量约束三形态**:汇总 + 点名 / 截断 + offset 续读 / 截断 + 如实说明;「超长一律落临时文件」通用包装层评估过不做(只救得了「装不下」一种病因,重跑便宜且必须新鲜的活加 offset 更省)。→ 详见 docs/notes/media-tools.md「ffmpeg 直通 ffmpeg_run(裸参数 + 边界闸 + 探测形)」

  - **嘴控绝对音量 + 「此刻」富化**:`media_control` 有 `volume`(0–100,core 校验,前端走 `setVolume` 基准路);**相对量(「再大一点」「快进 5 分钟」)不做增量动作**,模型从〔此刻〕背景(进度 mm:ss / 总长 / 音量 % / 倍速)自己算绝对值;前端 `report_media_state` 传 `PlaybackReport`,回报点 = 生命周期切换 + 调整 + 播放中 15s 心跳,core 按倍速外推「此刻」位置;音量粘住语义贯穿。→ 详见 docs/notes/media-playback.md「嘴控绝对音量 + 「此刻」背景富化」
- **★听音频原语 `read_audio`**:本机音频 → 「第几秒唱 / 说了什么」(`[mm:ss.xx] 文本` 逐句),**轴来自音频本身**;配歌词正解 = **模型的词 + 音频的轴**,对齐交给模型(编辑距离 / DTW 对齐代码不做)。机器 = ffmpeg 解码(`decode_file_pcm16k`)→ silero VAD 切句 → 逐段 ASR(共用识别器),零新组件;接线 `ToolCtx.voice: Option<VoiceRuntime>` + `Engine::set_voice`。常量单源 `voice/mod.rs`:`SING_HANGOVER_S=0.35` · `SEG_PREROLL_S=0.2`(VAD 起点卡在能量抬起处、声母在它之前)· 工具侧 `MAX_SECS=600` / `PAGE_LINES=200`(一页 + 报总数 + offset)。回合内等不转后台,超时 300s。**配词梯子顺序(用户纠正过,别再排反)**:lyrics_fetch 查库 → **web_search/web_fetch 上网找**(歌词站常直接给带 [mm:ss] 的 LRC 原文,版权上也比默写稳)→ 自己确实会唱的才默写 → read_audio 听着补轴;**听是「拿轴」的手段不是「拿词」的手段**;**时间戳只有三个正当来源:歌词库、搜到的 LRC 原文、read_audio,一个都不许自己编**;对不上号的句子宁可跳过不硬凑;同名 .lrc 已存在跳过。技能「给歌配歌词」配套。→ 详见 docs/notes/media-tools.md「听音频原语 read_audio + 配歌词梯子顺序」
- 本地播放链:需知(目录)→ 文件原语找文件 → `media_play` 放行本地绝对路径 → relay `/f/` 本地文件端点(手写 Range)。NAS 挂载盘符 / UNC 是普通路径。→ 详见 docs/notes/media-playback.md「本地播放链:探测 / 硬件加速 / 0.2.6 分离自适应 / 0.2.27 管线重做 / 多音轨恒进管线」
  - **探测 → 只转处理不了的那部分**(用户拍板「按需」):BMFF 走 `probe::probe_local` 读 `moov`(零子进程、不下 ffmpeg)→ 全兼容原生 `/f/` 直传;mkv/avi/ts 等容器必经 ffmpeg(先 `ensure_component`、`ffmpeg -i` 解析 stderr 拿编码 + 时长)。`play()` 一进来**后台预取 ffmpeg**(fire-and-forget,不卡播放)。转码取不到一律退回直传不阻断。**「这轨要不要转 / 走哪条路」自 0.2.27 起只认 `media/capability.rs`(下条),探测手段照旧、判定位置作废。**
    - **★硬件加速转码**(全自动无开关,用户拍板):要重编 H.264 时能用 GPU 就用 GPU——`relay::detect_video_encoder` 按平台优先级(Win:nvenc → qsv → amf;Mac:videotoolbox)**逐个试编码一帧**才算数,进程级 `OnceCell` 缓存,都不行回落 `libx264`;**参数收口一处 `relay::apply_video_encode`**(所有转码点唯一出口,`Software` 分支与旧代码逐字节一致,单测钉死);编码器每 entry 固定(init / 段 avcC 必须一致);兜底重放 `replay_local_compat` 强制 muxed HLS + Software;copy 路不转码不探测。✅ Win nvenc 真机验过(qsv / amf / 兜底未覆盖)。
    - **整文件 copy-remux(C2)已删**(用户判鸡肋:首播仍重编 + 后台多搬一份,`+faststart` 二次 pass 大片要好几分钟);`remux_or_direct` 是 pre-C2 件保留。
    - **★0.2.6 音视频分离自适应**(`Entry::FileAdaptive` `/la/`,前端 `localAdaptive.ts` 手写 MSE 两条 SourceBuffer):视频按需分段,兼容 H.264 `-c:v copy` 关键帧对齐**变长段**(`probe::video_keyframes` / `plan_copy_segments`),不兼容转 H.264,tfdt 改累计起点;音频**离散段**(WebView2 fetch 收不下流式 body)固定 6s 网格 + `AUDIO_PREROLL` 0.5s 预卷 + 前端 `appendWindow` 裁 priming → gapless 无累计漂移(根治「音频越放越快」),有界喂养(领先 30s 停 / 落后 12s 驱逐),seek 用代次 `gen`。**双层兜底**:前提不满足后端回落 muxed HLS;前端 MSE 运行时失败(含 12s 停滞看门狗)→ `media_replay_compat`。**诊断日志桥 `media_log`**(前端播放态写进 larkwing.log)是定位「Mac 预览能放、WebView2 不行」的抓手。响度 `AUDIO_LOUDNESS_AF` 只管转码臂(5.1 下混提量),`volume` 5dB(8dB 破音)。
    - **★0.2.27 管线重做**:① **判定收口 `media/capability.rs` = 播放路由唯一判定处**(`plan_source` + `plan_route` 纯函数 → `Plan{Direct/Adaptive/MuxedHls/Progressive}` + 每轨 `Copy/Transcode`,11 条 golden;probe 只给事实、relay 只搬字节);② **兼容性问浏览器**:前端 boot `codecProbe.ts` 探编解码矩阵上报 core(`capability::Codecs` + `normalize_codec`),静态白名单(含 mac_native 放宽)降级为没探过时的兜底;③ **能 copy 就 copy**:视频 copy 前提 = 解得动 + 有关键帧表 + 定得出 codec 串;音频 copy 三前提缺一不可 = 解得动 + 单/双声道(**多声道 AAC 被 MSE 拒 append 是硬墙,恒转**)+ AAC;mkv/ts 容器路走同一条判据;④ **响度分家**:均衡 / 夜间模式移前端 Web Audio 链 `useAudioGraph.ts`(直传 / 管线一视同仁,relay 为 crossorigin 开 CORS);⑤ **时间轴契约 `media/timeline.rs`**(产出即校验,`SEAM_TOL=0.12s`,对不上整条降级 muxed;顺带按内容认容器);⑥ 字幕:内嵌轨 + 旁挂 .srt/.ass 转 WebVTT(`/la/{token}/sub{N}.vtt`),播放条 CC + 嘴控 `subtitle`。
    - **多音轨(≥2)恒进管线:维持、不作废**(用户拍板;唯一站着的理由 = 直传选不了轨:mac 全轨混播实锤、Win 无选轨 API);收窄放宽(仅 Win + 默认轨直传)评估过不做,别再当 TODO 捡。规则单源 `capability.rs::plan_route` 三判据(视频解不了 / 选中音轨解不了 / 多音轨)。
- **工具入参的布尔值走 `tools::arg_bool` 宽容解析(2026-06-19)**:模型(尤其流式 JSON)常把 schema 声明为 boolean 的参数发成**字符串** `"true"`/`"false"`,裸 `Value::as_bool` 认不出就静默回落默认(实锤:`audio_only:"true"` → 当 false → 放本地歌弹出全屏视频框)。新加 boolean 入参一律用 `arg_bool`(真 bool / "true"/"false"/1/0/yes/no 都认),别再裸 `as_bool`。属 §4.4「Quirks 数据修正」一类。
- **工具入参的路径走 `tools::expand_home` 宽容展开(2026-07-21,arg_bool 同族)**:用户嘴里的「~/Downloads」会被模型**原样**写进参数与需知(真机 reasoning 实锤「以后用的时候再展开」——它不知道主目录在哪、也不会自己展开),而全链原本没人认 `~`(fs 家族当相对路径按 cwd 解释、`is_local_path` 闸直接拒收)。修 = 工具边界统一展开:裸 `~` 与 `~/`、`~\` 前缀 → `dirs::home_dir()`(mac=$HOME、Windows=C:\Users\〈名〉,中文用户名 OK);`~某用户名` 指向别人主目录的形不猜、URL 里的 `~` 不碰。已接入:fs 家族全部路径入参(`arg_path`/`arg_paths`/`arg_pairs` 收口)+ media_play/qr_decode/read_image/pdf_to_png/send_file/desktop open/web_download dir/web_render 上传与 dir。**新加收路径的工具一律经它,别再裸 `Path::new(原始入参)`。**(Windows 真机待验:中文用户名全链。)

### 7.2 文件能力(PLAN §9「文件能力」)
- **安全立场(2026-07-30 修订):文件授权圈 = 安全带,不是保险库**——防模型犯错 / 被网页内容带偏乱动文件,**不承诺对抗恶意**(§7.8 确认闸同口径);可逆(撤销 / 重做、操作记录页)仍是**功能性**的第二层保险;**UI / 文案 / 工具描述一律功能口吻**,别用「承重墙 / 兜底 / 保护文件」叙事。原「不做任何安全承诺」表述随授权圈落地作废。
- **「不设路径门禁」已推翻(2026-07-30 用户拍板)→ 文件授权圈**;「不强制对话确认」半边**仍成立**(授权是目录级一次性点头,不是每次写操作都确认——当年否决的是后者,没回潮)。
- **★文件授权圈**:只有明确允许的文件夹模型才能读写;程序数据目录 + 系统临时目录免授权。**闸的是模型的手脚不是程序自己**(只拦工具入参路径,17+ 接入点;程序内部读写不经工具层)。**三档 × 四动作**(单源 `tools/guard.rs`,用户拍板):只读 read / 可存入 create(看 + 落新文件不动已有)/ 完全访问 full;动作 = 读 / 新建 / 修改(fs_move 源按修改判)/ 删除,write/append 按目标存在与否分。**下载 + 桌面 = 出厂基线「可存入」**。匹配 = 路径**组件**前缀比(Win/mac 大小写不敏感;规范化 = expand_home → canonicalize 最深存在祖先 → `datadir::simplify` → 词法归一 `..`);入表自动合并被覆盖的低档子孙,**档位更高的子条目保留绝不静默降级**;表 = settings app 级 `fs.scopes`(专用命令 `fs_scopes_list/fs_scope_set/fs_scope_remove`,不进 `APP_SETTING_KEYS`)。**撞圈 = 确认卡三态**(复用 Confirmer):「一直允许」按所需最低档入表 / 「仅这次」= 本回合(含派生的 delegate 子回合)临时放行 `ToolCtx::grants` / 拒或超时 = 错误观察喂模型(话术带「如实说、别换路径绕」),拒过的目录本回合不再弹;同批多目录并一张卡;批量转后台的**授权前置到 spawn 前**;**渠道文字回话 / 语音口头「允许」恒按仅这次**(永久授权只留给有 UI 的地方)。**接线纪律:新收路径的工具一律在拿到路径后、动手前过 `guard::ensure(ctx, Access, paths)`**(fs_undo 先读记录再 ensure)。**工具描述刻意零改**(拒绝话术自解释,前缀缓存不失效);**不做**单文件授权 / 通配符 / 审批等级与总开关 / 需知自动迁移。UI = 设置·系统「能碰的文件夹」卡,新加默认只读;审计走 confirms 域。→ 详见 docs/notes/files.md「文件授权圈(能碰的文件夹)」
- **可逆三规**(让功能性可逆成立):① `fs_move`/`fs_copy` **永不覆盖**(同名自动 ` (N)`,资源管理器口径——这是功能正确性);② 删除走系统**回收站**不 `unlink`;③ 文本写 / 改前把旧内容快照进记录(`store::fsops`,一批一行 JSON,撤销 = 逆序反向)。
- **直交原语**(read_text/move/copy/mkdir/trash/write_text/append/edit/undo + list/find + unzip/zip),**不造 `organize_media` 这类任务工具**(§5);模型 + 需知目录自行组合。`fs_edit` = 轻量 find/replace + 旧内容快照,**不背 robot 的 read-gate/stale**。
- **压缩包原语 `fs_unzip` / `fs_zip`**:解 zip/rar/7z + 打包 zip(只产 zip,RAR 许可只准解)。**按文件内容认格式**(魔数,假后缀照认;分卷 rar 给第一卷)。**解压恒落全新文件夹**(`base/<包名>`,dedupe 加序号)→ 永不覆盖天然成立、**不进 fsops**;条目路径逐组件重组拒 `..` / 绝对形(zip-slip)+ sanitize;非 UTF-8 文件名按 **GB18030 回退**;总量 >50GB 拒(`ARCHIVE_MAX_BYTES` 与 torrent 同口径);**带密码没给密码在盘点期就如实要**(preflight 先盘条数 / 总量 / 加密),password 只填用户给的。执行 = `media/archive.rs` 回合内 30s → 转 bgtasks(`IN_TURN_WAIT` 单源上移 `pub(super)`),取消粒度 = 条目之间,**半成品收拾在阻塞闭包里兜**(失败 / 取消整删新文件夹;`CancelOnDrop` 递旗)。引擎 = 顶层 `archive.rs` 纯同步可测:zip crate(`aes-crypto`)、`sevenz-rust2` 0.7(**被 MSRV 1.77.2 钉版**;bzip2 形没开)、`unrar` 官方源码绑定(vendored C++)。授权圈:包 = read、新文件夹 / 成品 = create。已知边界:rar 无本机夹具 = 真 rar 包归真机验;unrar C++ 在 Windows MSVC 首编是 watch。→ 详见 docs/notes/files.md「压缩包原语 fs_unzip / fs_zip」
- **磁盘占用原语 `fs_usage`**:只读,整树聚合报「第一层子目录 TOP 15 + 全树最大文件 TOP 10 + 总量 / 文件数」(两个 TOP 用户拍板);「C 盘怎么满了」= 模型对着最大子目录反复下钻,**不造 cleanup 任务工具**,清理走 fs_trash。引擎 `usage.rs` 纯同步:不跟 symlink/junction、没权限的目录跳过**计数如实报**、深度 64 backstop;执行 `media/usage.rs` 回合内 30s → 转 bgtasks;**总数未知按 total=0 提交,显示端不显分母**(`progress_frag` 单源,清点型进度靠 current 文案);取消只递旗停扫(`CancelOnDrop` 可解除武装)。授权圈 read。配套技能「电脑清理」。→ 详见 docs/notes/files.md「磁盘占用原语 fs_usage」
- **属性原语 `fs_stat` + fs_list/fs_find 捎改动日期**:`fs_stat` 只读批量看属性(paths ≤300;结果顺序与传入一致、每文件一行只报 file_name):大小 + 修改 / 创建时间;照片带 EXIF 拍摄时间(归一「2024-01-15 13:20」)/ 相机 / 分辨率(读头不解码);mp4/m4v/mov 带时长(`probe_local` 免 ffmpeg;mkv/mp3 刻意不报,不为一行属性拉子进程)。**独立成原语不塞 fs_read_text**(内容读取单文件 offset 续读,属性读取整批,塞一起是双形 schema;用户拍板)。依赖 `kamadak-exif`。fs_list 行「名字 (大小 · 改动日期)」、fs_find 尾捎日期。分工进各自描述:list/find = 扫一眼,fs_stat = 细看属性,fs_read_text = 读内容。→ 详见 docs/notes/files.md「属性原语 fs_stat + fs_list/fs_find 捎改动日期」
- **Windows 功能正确性**(Mac 测不出,真机验):跨卷 `rename` 失败转 copy+删源;Windows 名校验(保留名 / 非法字符 / 结尾点·空格);回收站还原 `trash::os_limited` 仅 Win/Linux(macOS 降级返回未还原)、长路径 `\\?\`、被占用文件锁。
- **量是一等约束**:写原语**批量原生**(数组);工具结果**只汇总 + 只点名失败**(token 不随条数爆);单次调用封顶 300、超额如实告知。
- **读类三原语一律「一页 + 报总数 + 给续读起点」**:`fs_list`(一页 200)/ `fs_find`(一页 50)/ `fs_read_text`(一页 4 万字)都收 `offset`,截断时把**总数**一并给出(模型据此判翻页还是换条件),越界如实退回;**顺序必须稳定**才配分页;fs_find 扫到 `FIND_SCAN_MAX=1000`(§4.11 待拍板)就如实说「可能还有」。**聚合型输出不套这个形**(fs_usage 的 TOP 是聚合不是截断);批量写原语超额仍是报错分批不是分页。→ 详见 docs/notes/files.md「读类三原语一律「一页 + 报总数 + 给续读起点」」
- **fs_find 返回文件夹 + 广度优先浅层先出**:文件夹与文件同一套匹配口径都算命中(以 `/` 结尾,照样往下钻);收集**广度优先**(撞 `FIND_SCAN_MAX` 时浅层保证扫全,每层按名排序 → 截断集合确定);排序键「(深度, 路径)」浅的先出 = 骨架先看到(**光换遍历不行,sort 会抹掉遍历序,真正要换的是排序键**);描述改口「文件夹以 / 结尾、想看里面接 fs_list」。「截断时报命中分布」评估过不做(针对单 case、破坏原语单纯性)。→ 详见 docs/notes/files.md「fs_find 返回文件夹 + 广度优先浅层先出」

### 7.3 任务需知(PLAN §9「任务需知」)
- **任务需知** = 跟着**任务 / 能力域**走的环境知识(电影目录在哪、代码仓库在哪):非人格、非个人记忆、非应用设置。数据形 `{域, 内容, scope:家|个人, 常驻?}`,自有小域;**常驻 vs 按需是每条的数据属性**(§4.8)。
- **法条搬进底座**:运行时法条进 `engine/context::LAWS` 固定段(人格中立),companion persona 修剪成纯性格(第二场景自动继承)。
- 写入 = 对话即配置,few-shot 三路路由(关于人 → 小本本 `remember`;环境 / 资源 → 需知 `briefing_write`;一次性 → 不记)。**用户面零新概念**(看 / 改 / 删 = 回忆页两分组「关于你」/「家里的事」)。
- 提示词总原则:**提示词只立法条,教学归 few-shot,绑定归工具描述,内容归数据节**。

### 7.4 提醒 / jobs / web(PLAN §10)
- **jobs 底座**:`jobs` 域 + `scheduler`(30s 轮询,无 cron 框架;错过宽限 2h——once→missed、**重复任务推进到未来不补发**;触发即推进 = at-most-once)+ `engine.wake_turn` 自启回合(event 行落库;目标会话在飞则跳过本 tick 绝不打断)。**「设提醒 → 到点」四步在聊天流全可见**(用户拍板):用户说 → 回执小票「⏰ 已记下」(`MainLayout::groupChips` 零后端;remember 同款「记住了」跳记忆页;加新回执 = groupChips 加分支;排气泡末尾)→ 到点 = event 行渲染成居中系统线「⏰ 到点了 · 内容」(wake 回合失败也可见 = 到点必有动静)→ 模型转述 = 普通气泡(trigger 标签已摘)。**忙检必须看 `join.is_finished()` 而非 `inflight.is_some()`**(正常收尾不清句柄,否则会话聊过一次后所有提醒被「会话忙」永远跳过;skip 日志 info 级;回归 `wake_turn_fires_after_completed_turn_in_same_conv`)。→ 详见 docs/notes/scheduler.md「jobs 底座 + 「设提醒 → 到点」四步可见 + 忙检 is_finished」
- **job 执行一律新鲜上下文**:稳定前缀与聊天回合字节级相同(共享缓存),不回放历史;任务语境靠创建时**物化进 content**(自包含、指代全展开,2000 字上限)。
- **mode 只活在创建时刻**:remind → 落当前会话;task → 建专属会话(连载帖,闲聊会话零污染)。
- **提醒三件套** reminder_set/list/cancel:用户说人话,模型用 `now` 推 first_at;**cron/job 概念永不暴露**。**桌面提醒页 = 主人的管理面(2026-07-04)**:显示**全家**待触发提醒(家人经渠道归人设的带名字标签)、都可撤(`jobs.list_pending_all`/`cancel_any`);工具侧 reminder_list/cancel 仍按说话人限定——此前按当前用户过滤,家人设的提醒在页面上「消失」,真机困惑实锤。
  - **跨人提醒 / 捎话 = `reminder_set` 加 `for`**(用户拍板三件:跨人可以 / 转述形态 / 收不到人如实说):`for` 填家人名字(`outbound::find_member` 宽匹配,查无 / 重名带名单退回)→ **收件人在创建时刻物化成 conv_id**(落 TA 的手机对话,`outbound::resolve_phone`),到点在 TA 的会话里开口(转述形态)→ 既有 outbound_loop 推 TA 手机,到点链路零改;**捎话 = first_at 填当前时间的跨人提醒,零新工具**;TA 没连手机 → 创建时就如实退回;`for` 只配 remind 不配 task;`jobs.created_by`(迁移 0022)让发起人也看得见撤得掉,悬浮窗「下个提醒」只看自己收件的。→ 详见 docs/notes/scheduler.md「跨人提醒 / 捎话 = reminder_set 加 for」
- **web 三件套 = 搜索即抓取 / 读页 / 存盘**:`web_search` 一次带回 top3 正文证据片段;`web_fetch` 读正文 + 页内链接(锚文本 + 绝对地址,LINKS_MAX=25)+ 可选 `offset` 续读(截断话术直报「继续读带 offset=N」,全文在 10min 缓存 Page JSON 里,不落盘不建新机制;`fs_read_text` 同款;`tools::arg_u64` 收字符串数字)+ **表格转 markdown**(首行当表头;行 30 / 列 8 封顶如实注明;<2 行或单列的布局表照旧拍扁);`web_download` 把直链存本地(默认下载夹、Content-Disposition 文件名、50MB 同步档、`files::dedupe_path` 永不覆盖)。`web::non_page_hint` 把 PDF / 二进制直链如实拦下指路「web_download → fs_read_text」(只认明确二进制 CT / %PDF 魔数,误拦比漏拦贵)。选择器是代码不是数据,坏了改 `web.rs`;预算闸(片段 1200 / 全文 6000 / 页面 10MB)+ 同 URL 10min 缓存。**★搜索请求必须长得像浏览器**(裸 GET 被判机器人;最阴的坏法 = 跳转链上 query 被弄丢、回一个结构完整内容却是别的假结果页):cookie 罐 + 整套浏览器头 + **HTTP/2**(reqwest `http2` feature 必开,只说 h1.1 就是机器人特征);`count=10` 参数本身是机器人特征**别再加回来**;重定向上限 20。**三源按可靠度 × 快排:搜狗 → Bing → DDG**,搜索单请求超时 30s、抓正文 15s;**合理性闸 `looks_relevant`**(查询抽 ASCII 单词 + 相邻双汉字当信号,单个汉字刻意不收;一条不沾就换源;刻意宽松);**限流是常态不是缺陷**(验证码页解析 0 条 → 自动换源);`web::tests::real_source_report`(#[ignore])= 「搜索又不对劲了」的第一件工具。web_render 快照 6000 字截断刻意不动(每步重拍 offset 语义不稳;通读走 `read`)。→ 详见 docs/notes/web-office.md「web 三件套 + 搜索请求像浏览器 / 三源 / 合理性闸」
- **网页正文 = 不可信输入**:以「观察」喂回模型(§9 不做风控,但**外部抓来的文本不当指令**;后续考虑来源标记 / 隔离话术)。

### 7.5 语音(PLAN §11)
- **业务零入**:ASR 出文本 → 前端走既有 send 链;TTS 念 TurnEvent 文本。回合循环 / 工具 / 记忆 / 场景**零改动**;voice 模块不碰 engine。桌面形态**编排者 = 前端 VM**(robot 最痛的双播坑结构性消失)。
- **管线参数出处**:全部源自前身项目 robot 的 Windows 真机实战调优(其语音设计文档的经验教训 / 调参速查 + channel / vad / wake_word 源码);属本地参考资料、不随本仓库。
- **推理统一 sherpa-onnx**(官方 Rust 绑定 `sherpa-onnx`;sherpa-rs 已弃用):ASR/VAD/KWS/声纹/本地 TTS 一个原生依赖。
- **播放分两路**:长内容(回合 TTS)走 WebView relay `/tts/`;延迟敏感短句(唤醒应答 / 追问 / 告退)走 **core 原生 cpal 直出**(PCM 内存常驻,应答一停立即开录 0 间隙)。
- **管线参数 = robot Windows 真机实调终值,锁死进代码不暴露**(采样率 / 帧长 / hangover 800ms / min_seconds 0.5 反幻觉 / max 12 / silero 阈值 0.5 …)。silero 是唯一 VAD,不留 energy 档(强默认收口)。(2026-08-31 订正:原清单里的「pre-roll 200ms」在 `collect_utterance` 里**从未兑现过**——交互链一直只靠 sherpa 内建的 min_speech+64ms 回退;现由下条 1.0s 段前滚取代,0.2s 的 `SEG_PREROLL_S` 只管 read_audio 逐段转写。)
- **★交互听音段前滚 1.0s**(治唤醒后报数字丢头;用户拍板):sherpa silero VAD 要求**连续** `min_speech_duration=0.5s` 才触发,触发前任何一个 32ms 窗掉阈值就清零重计 → 断续开头(「一…二…」)结构性落在段外。修单源 `voice/mod.rs`:① `collect_utterance` 自缓冲 + 出段按 `seg.start()` 回切 `UTTER_PREROLL_S=1.0s`(`pad_utterance` 纯函数,越界回落裸段;**录制类采集〔标定 / 克隆参考音 / 声纹注册〕传 0 忠实原声**;`CaptureOut::Utterance` 带 `speech_from`,反幻觉闸与声纹按裸段算、只有 ASR 吃整段);② cpal 采集通道 64→512 帧(确认层 ASR 阻塞期溢出丢帧);③ 尾部偶发丢字 = 停顿超 hangover 首段定稿,**机制不动**(patience 可调)。**订正:原清单里「pre-roll 200ms」在 collect_utterance 从未兑现过**,`SEG_PREROLL_S=0.2` 只管 read_audio。→ 详见 docs/notes/voice.md「交互听音段前滚 1.0s(唤醒后报数字丢头)」
- **TTS 流式切分 = 跑道(runway)驱动自适应**(急 / 稳 / 懒三档 + 停顿 / 收尾兜底);切分器 = `useSpeech.ts` 纯函数,参数锁死;Markdown/代码/URL 净化在切分前。**ffmpeg 全程不在 TTS 链路**。
- **★参考音必须先洗干净再入库**(零样本克隆把录音条件当音色学走:参考底噪 → 输出底噪一比一跟随;治法在参考音一次性离线做,不在输出侧):① **`peak_normalize` 硬削波是真 bug 已修**——按 99.5 分位推到 −3dBFS 而语音真峰在其 1.8 倍上被 clamp 切平;**录得越轻增益越大削得越狠,「调低输入增益」建议是反的**;修 = 增益取 `min(响度目标/p99.5, 0.99/真峰)`,响度让位于不失真,「只增不减」不变。② **去噪 = GTCRN 神经模型,推理期零旋钮**(`voice/denoise.rs`,sherpa-onnx 同一依赖 + 0.5MB 用时下载 sha256 钉死);**「换个环境要不要重调参数」是选型硬判据**,谱减法(afftdn)要按环境调参且在音节尾音留伪影 → 否决绝不进产品;尽力件:模型不可用不挡录入,体检如实告知;现场录音与导入两条路都洗。③ **录入体检 = 绝对量测量 + 一条可执行建议**(不是闸):削波在归一前量、底噪量去噪后那份,一次只报一条(削波 > 底噪 > 没能去噪);阈值 §4.11 单源 `denoise.rs`:`NOISE_FLOOR_MAX_DB=−55` · `CLIP_MAX_RATIO=1e−4`;core 只过 issue key。④ `delete_clone` 逐用户清指着它的 `voice.speaker`(原悬空 → 每次合成报「音色不存在」)。**诊断方法论:听感问题先量再猜**(astats 地板 / 峰值 / 平顶 + 百分位,参考 vs 输出 vs 云端摆一张表);探针 `examples/denoise_probe.rs`。→ 详见 docs/notes/voice.md「参考音先洗干净再入库:归一不硬削 + GTCRN 去噪 + 录入体检」
- **★克隆音色输出必须自己归一响度**(−16 LUFS,用户拍板):ZipVoice 输出响度照抄参考音(`target_rms` 是条件化旋钮不是音量旋钮),参考录得轻这把嗓子永远轻;参考音侧救不了 → 输出侧 `tts.rs::normalize_loudness`(门限 RMS 定增益 +12/−6 dB 封顶 → 5ms 前视限幅,不是硬 clip;纯 Rust 守「ffmpeg 不进 TTS 链路」),常量单源 `tts.rs`;**只归一克隆音色**(云端音色自带母带);两条播放路都覆盖(归一在 `synthesize` 里)。**`CLONE_RENDER_VERSION` 掺进克隆 cache_key,改任一响度常量 = 版本号 +1**(否则旧缓存照播老响度;只掺克隆免连带作废 edge 缓存)。标定夹具 `#[ignore] loudness_real_calibration`。→ 详见 docs/notes/voice.md「克隆音色输出归一响度 −16 LUFS + CLONE_RENDER_VERSION」
- **AEC = 「采集端浏览器 AEC」路线**(用户拍板;自研 / 引入软件 AEC 仍不做):麦克风采集迁前端 `getUserMedia({echoCancellation:true})`——WebView2 = Chromium AEC3,参考信号 = 它自己在播的全部音频,绕开参考跨 IPC / 双时钟两座山;AudioWorklet 16k 推 core(`voice_push_audio`),VAD/KWS/ASR/声纹下游不动。`voice.capture.source`(app 级)→ `VoiceRuntime::open_capture_auto` 接缝分派七个采集点;browser 源 = `useMicBridge`(**AEC on / NS off / AGC off 锁死**,NS 双开啃双讲人声),「core 需要麦」才开不常驻;失败自愈:起不来 → toast + 自动切回 cpal + 重启唤醒,绝不静默聋;**TTS 自激闸门条件化**:`useSpeech.syncWakeSuspend` 只在 cpal 源挂起,browser 源 = barge-in 解锁;麦选择器双列表分键(`voice.input_device_web` / `voice.input_device`,绝不混写);开关在声音 tab 高级区「回声消除」,NS/AGC 不暴露。录音媒体避让 duck 20%(candidate 即 duck);Windows「通信活动自动压低」是 OS 设置放引导卡。**⚠️ mac 上开着 browser 采集 = 自家播放音质被系统通话处理弄糊**(两层:蓝牙耳机被开麦打落 HFP 16k 电话档〔所有语音助手 + 蓝牙耳机都撞〕;macOS 给持有 getUserMedia(EC) 的 WebView 套 VPIO 连带处理自播音频,A/B 开关实锤、无代码级绕法);Windows AEC3 只处理采集 → 暂判 mac dev 边界,Windows 补「开 AEC 放同一首歌 A/B」验收。**形态指引:耳机听歌关回声消除**(自播进不了麦,AEC 零收益纯付代价)→ 三态自动档(下条)。→ 详见 docs/notes/voice.md「AEC = 采集端浏览器 AEC 路线(含 mac 通话处理污染播放音质)」
- **回声消除三态 = 自动 / 开 / 关,默认「自动」**(用户拍板「识别是耳机就出声就给关了」):`voice.capture.source` 加 `auto`(**新默认**,§4.11 双源镜像 = Rust `capture_source` unwrap_or + 前端 DEFAULTS + `set_setting` 校验白名单;存量显式值一次性迁成 auto,标记 `voice.capture.auto_defaulted` 只迁一次,cpal 自愈写的 cpal 不受损)。解析 = `voice/output_route.rs`:看**默认输出设备是不是耳机**(耳机 → cpal,扬声器 / 认不出 → browser;认不出不赌,AEC 多开无害),纯函数 `resolve`;检测原生:mac CoreAudio 传输类型(**蓝牙一律当耳机**;内置输出 DataSource=='hdpn')/ Windows MMDevice FormFactor(**cfg(windows) 代码 mac 编不了,首编进 watch**)。生效值消费:core `open_capture_auto` / `confirm_listen` 现查现解析;前端经 `voice_route` + `useCaptureRoute` 喂三处闸(MicBridge 开麦闸——**auto 未解析时不开麦**;useSpeech 自激闸门——未解析按 browser;设置页麦列表)。**跟随设备切换 = 前端 5s 轮询**(`ROUTE_POLL_MS`),变了 `voiceWakeSet` 双拍重启换管。麦克风设备不自动动。UI = 三段钮 + auto 档实时提示行。→ 详见 docs/notes/voice.md「回声消除三态 = 自动 / 开 / 关,默认自动」
- **渠道会话不触发桌面 TTS(2026-07-04 真机)**:「提醒到点自动开口」只对**桌面自己的会话**(channel=ui/system)念;远程渠道(telegram/钉钉)设的提醒到点已由 `outbound_loop` 推到那台手机(§7.7 A1),桌面即便正停在该渠道会话也**不再 TTS 念**——手机的事、双份打扰。前端 gate:当前会话 channel 属 {ui,system} 才念。
- **听不清两段式有声兜底**(绝不静默失败):没声音 / 没识别出字 → 第一次播追问立即重听;第二次仍空 → 播告退语回待唤醒。机制在 core,**话术 = 人格数据**(场景 JSON,随语言变体)。
- **免唤醒连续对话 = 跟进窗,早已落地(2026-07-09 对账补记——此前本文件漏记此机制,一次能力盘点把它误判成缺口)**:唤醒发起的回合 TTS 念完 → 前端 `wakeFollowUp()`(useChat;非唤醒回合 no-op)→ core `Phase::FollowUp` 开 **6s 跟进窗**(`wake.rs::FOLLOW_UP_WINDOW`,robot 终值):窗内开口直接转写进下一轮(免再喊唤醒词,答完再开窗、可连续多轮);窗内安静 → `follow_up_idle` **安静回待唤醒**(不追问不告退,robot 纪律「对话自然结束别烦人」)。窗长若真机体感偏短属调参(§13.7「真用才能调」同款),不是缺功能。
  - **媒体在播 → 3s 短窗(2026-07-10 用户拍板「暂定 3s」)**:跟进窗全程把电影/音乐 duck 在 20%,能短就短。`FollowUp` 指令带 `media_playing`(前端**念完那一刻**按 `useMedia.status==='playing'` 传,不在 done 时读),core 择 `FOLLOW_UP_WINDOW_MEDIA=3s` / 常态 6s(两常量同在 wake.rs 单源)。唤醒后的首听 `WAKE_START_TIMEOUT` 仍 6s(刻意喊醒 = 就是要说话,没跟着缩)。
  - **主动结束连续对话**(用户拍板):说「没事了 / 拜拜」立即收跟进窗回待唤醒(叫醒仍必需)。机制 = **模型判断 + 带外旗标**(不走哨兵):BASE_TOOLS `end_conversation`(无参)+ LAWS「聊完了就收尾」(**默认留着听,拿不准就别调**,「宁可多留一会儿」);turn loop 记「本回合调过它」→ `TurnEvent::Done{end_session}` → 前端 `wakeFollowUp(endSession)` 把出口从 `voiceFollowUp` 换成 `voiceWakeResume`,**仍等 TTS 念完再动**(否则「拜拜」漏进 KWS)。为何带外不哨兵:END 是「真回复 + 之后收尾」,尾部哨兵会漏进念 / 显 / 历史库。降级:模型没调 = 退回 6s 静音自动收;打字回合调它 no-op。eval 守卫 `end-conversation-farewell` / `end-conversation-keep-open`。→ 详见 docs/notes/voice.md「主动结束连续对话 end_conversation(带外旗标不哨兵)」
  - **⚠️ 唤醒收尾通知一个都不能漏**:媒体音量恢复链 = 前端收尾通知(`voiceFollowUp` / `voiceWakeResume`)→ core `ListenEnded` → 前端关唤醒区间恢复 duck;漏发 = core 卡 AwaitTurn 到 `AWAIT_TURN_CAP=180s` 兜底(「音量隔好几分钟才恢复」真凶)。useChat 三条早退路(切走会话 / 被新回合接管 / 前置错误)已补齐 settle;core `WakeCmd::Resume` 离开非 Watch 相位补发 `ListenEnded`。**规则:给唤醒回合加任何新的终态 / 早退分支,必须带上 wake settle(wakeFollowUp / wakeResume 幂等、非唤醒 no-op,白调无害)。**。→ 详见 docs/notes/voice.md「唤醒收尾通知一个都不能漏(wake settle 规则)」
- **i18n** = 一张「语言 →(ASR / TTS / KWS)最强组件」目录表,每语言配该语言最强组件;切语言 = 换模型组件(用时下载)+ 换音色。一期只填中文。
- **ASR 识别模型用户可选,四档**(`voice.asr.model`,app 级,默认 `sense-voice`):SenseVoice(快,默认)/ FireRedASR2-CTC(`firered-ctc`,「更准·听不清 / 孩子选这个」,✅ 真孩子声验过)/ Fun-ASR-Nano(`funasr-nano`,远场高噪 / 方言,SenseVoice 兼容导出零新架构)/ Paraformer-zh(`paraformer`,不同架构 = 真备胎)。**Whisper 三档已砍**(输出繁体 + 效果差;丢多语种可接受);孩子无现成解(真根治 = 儿童语料微调,本期不做)。加档 = `voice/models.rs` 一份规格(`ModelSpec` 或整包 `TarModelSpec`;就绪 / 下载走 `VoiceModels::is_asr_ready` / `ensure_asr` 按档派发)+ `AsrModel` 一支 + `asr.rs` 一个构造分支 + `set_setting` 白名单 + 前端 asrOpts + i18n;`transcribe` / trait 面不动;缓存带档身份,换档由 `restartWakeIfRunning` 生效;旧 `whisper-*` 值回落默认。**下拉露产品名不露模型 ID**(用户拍板)。**新加档先跑 `examples/asr_probe.rs` 真加载真转写再入 spec**。**FireRed 下载源 = gh release 整包**(上游 HF 仓库转 gated 回 401,hf-mirror 308 跳回同死,**HTTP 拒绝不是连通性,开代理也救不了**);SenseVoice 的 HF 源仍健康刻意不动(gh tar 含 fp32 大 4.5×);hf-mirror 已变纯 308 跳转,默认档国内可达性实际靠代理兜底。→ 详见 docs/notes/voice.md「ASR 识别模型四档(SenseVoice / FireRed / Fun-ASR-Nano / Paraformer)+ 下载源」
- **语音会话模式**(robot channel_format 对应物):**不按渠道注入提示词**(破前缀缓存)。落法 = 交互形态**物化成消息数据**(payload `{input, speak}`)+ 装配时 speak=true 加确定性标记 `〔语音〕` + 一段**常驻法条按数据条件生效**(LAWS「说话守则」)。
- **听写快捷键**:app 内绑定、**输入框打开时才生效**、**不做全局热键**;且**固定不暴露**(不进设置,tooltip 提示)——强默认收口的兑现。
- **英文免手唤醒 = 单独工程,记 someday**(用户「需求不强」):KWS 被中文模型锁死,**sherpa 英文 KWS 真人实测召回 ≈0**、双语模型与当前 binding 不兼容;出路不是修 KWS 而是换机制(① VAD→ASR 唤醒复用 SenseVoice 零新依赖,+0.5–1s 延迟;② openWakeWord 引 `ort` 破单一 sherpa 依赖)。ASR/TTS 早支持英文只唤醒词没有,别当 bug。**测 KWS 召回必须真人,TTS 顶不了。**。→ 详见 docs/notes/voice.md「英文免手唤醒 = 单独工程 someday」

### 7.6 常驻临场:开机自启 / 托盘 / 悬浮窗(PLAN §12)
- **零新 core 事件**:悬浮窗只是「全局事件车道」(`app_event`)又一个消费者,复用同一份 Vue app / token / 形象 / composable;窗口管理与托盘进**壳层**。
- 形态 = **C(混合可展开)**:收起 = 一体小挂件,展开 = 信息面板(进行中 / 通知两区);常驻锚点 = 系统托盘;开机自启 = `tauri-plugin-autostart`(OS 真相源)。
- **关主窗 = 隐藏到托盘、不退进程**(✕ 首次点出友好气泡兜底;`CloseRequested` → `prevent_close`+`hide`,Mac 原生红灯关窗也走这条,见 `lib.rs`)。
- **★窗控按平台分叉(2026-07-11 用户拍板;起因 = 真机撞「全屏后右上三键消失、无法退出」卡死,借此重议整套窗控在不同平台怎么展现)**——定调 = **目标平台 Windows 保科幻卖点(无边框可主题化)、开发机 Mac 图省心用原生**,各取所长、不强行统一(用户「要不然我们分开处理」的直觉):
  - **Windows/Linux = 无边框科幻窗**(`decorations:false` + 右上自绘三键:最小化 / **最大化⇄还原** / 缩托盘;Windows 没有「可透明的原生标题栏」,主题化必自绘);**中键 = 最大化而非全屏**(`win.toggleMaximize`,结构上不困人),沉浸全屏只留给看视频。**无边框窗最大化会盖住任务栏(Tauri #7103 上游未修)→ 必须 `WM_GETMINMAXINFO` Win32 hack 钉到工作区**(`src-tauri/src/winmax.rs`,lib.rs setup 挂;**写「挂钩子」类组件必须 grep 调用点收尾**——曾漏接零调用)。→ 详见 docs/notes/desktop-shell.md「窗控按平台分叉:Windows 无边框三键 + Mac 原生标准窗 + 影院全屏卡死修」
  - **macOS = 原生标准窗**(`decorations:true` + `transparent:false`,`tauri.macos.conf.json`;不设 `titleBarStyle`、不是 Overlay、不透明):原生红绿灯,绿灯 = 真全屏;`WindowControls` 在 Mac 不渲染(判据 `isMacOS`,单源 `backend.ts` UA);`html.is-macos body` 补不透明底色。**⚠️⚠️ 别再试 Overlay / 透明红绿灯组合**(真机两配置两坏法:透明 → 进出全屏红绿灯变黑圆;不透明 + Overlay → 启动就黑)。
  - **藏三键判据 = `current.kind==='video' && fullscreen`**,`VideoOverlay.onResized` 只在有视频时校准 `media.fullscreen`(原两组件两套语义 → 手动全屏后三键消失卡死)。两平台真机验全欠(§10 没真机验过不说 done)。
- **单实例 / 二次启动唤回**(2026-06-16):全程序只跑一个进程。已在运行时再点快捷方式 / 重复启动**不开新进程**——`tauri-plugin-single-instance`(**放最前注册**),OS 把第二个进程的命令行交给已运行实例的回调、第二个进程退出;回调复用 `show_window` 把主窗(可能藏托盘)唤到前台,沿用 `--autostart` 静默语义(自启触发不唤窗)。无 IPC 命令、不需 capability。**OS 转发 + 窗口前置 ✅ 2026-06-30 Windows 真机验过**(若只闪任务栏不前置的 always-on-top 翻转兜底备用,见 PLAN §12)。
- ⚠️ **悬浮窗 useMedia 只读不发声**(独立 WebView 复用播放 VM 会与主窗双播——多窗变体的双播坑,`play` 分支已堵)。**反向媒体控制已落地**(2026-06-16):悬浮窗迷你播控按钮**转发**给主窗执行(`emitMediaControl`→主窗 `onMediaControl`→`applyControl`,与嘴控汇同一执行口),float 仍不出声;播放态经 `emitNowPlaying(np,status)` 镜像回 float(播/暂停图标翻转)。跨窗联动 **✅ 2026-06-30 Windows 真机验过**(§8.1)。
- **别的程序全屏 → 悬浮窗让位(仅 Windows)**:float 是 `always_on_top` 会盖别 app 全屏;Mac 原生 space 天然不覆盖 = 正确不动。壳层 `fullscreen.rs::foreground_fullscreen()`(`#[cfg(windows)]`,前台窗口矩形 vs 显示器矩形;排除桌面 Progman/WorkerW + 自己进程)+ 1.5s 轮询**仅全屏态真变化时** `emit("lw:foreground-fullscreen")`;**Rust 只报事实、显隐决策归主窗 JS**(`App.vue::applyFloat` = `floatOn() && !ownFs && !foreignFs`);`windows` crate 复用 Tauri 已拉的版本。✅ Win 真机验过。→ 详见 docs/notes/desktop-shell.md「别的程序全屏 → 悬浮窗让位(仅 Windows)」
- **托盘「显示悬浮窗」**(2026-06-16):✕ 关掉悬浮窗后,托盘菜单一项重开(`show_float`→壳层 emit `lw:show-float`→主窗置 `ui.float.enabled='1'`+`setFloatVisible(true)`,持久化由主窗收口);比绕设置页顺手。
- **失败任务「重试」已落地**(2026-06-16,轻量版、不等 JobRunner):影音解析(`TaskRetry::MediaPlay`)、组件下载(`Download`,0.2.0 推广)、**语音模型下载(`VoiceModel{id}`,2026-07-03 v0.2.4 补齐三型 spec,id=落盘目录名,前端直连 `retry_voice_model`)**失败都带重试载体,UI 显「重试」直连重放(按钮不绕 LLM,§7.1 哲学);auth **不再算失败**——改为记下待重放、扫码登录成功后**自动续播**(2026-06-18,§7.1),不出重试钮。通用 JobRunner 重试仍后置。
- 排序立场:**不做优先级排序**(robot 配置病)——通知「最新优先 + 自动淡出」、进行中「钉住」。
- **打包 / 卸载 / 默认自启**(用户拍板):**Windows 只发 NSIS、不发 MSI**(`bundle.targets` 显式清单 = all 去 msi;CI 零改)。**卸载清自启残留**:`src-tauri/installer-hooks.nsh`(`NSIS_HOOK_POSTUNINSTALL`)删 auto-launch 写的两条 HKCU 键(`…\Run` + `…\Explorer\StartupApproved\Run`,值名 = 产品名,**改 `productName` 要同步改钩子**;`installMode=currentUser`);**升级不删、真卸载才删**(`${If} $UpdateMode <> 1` 守卫,否则升级冲掉用户自启状态;旧版本编进去的卸载器没守卫,那一跳靠 `.v2` 标记补)。**正式版默认开机自启**:首启 `lib.rs` setup 按产品默认 `enable()` **一次**(标记 `system.autostart.defaulted.v2`,app 级直接走 `store.settings` 不进 `APP_SETTING_KEYS`),之后全交设置页;用 auto-launch 自己的 `enable()` 保证与 `is_enabled()`/`disable()` 零漂移;**仅正式版生效**(`!cfg!(debug_assertions)`);**标记带版本号,升版 = 一次性重开默认**(用户拍板接受「手动关过的老用户被重开一次」;未走「自启状态进 DB」方案守 §6.8)。只能 Windows 正式版真机验(装 → 自启 → 关 → 卸载 `reg query` 两键皆无;升级链保留不重复重开)。→ 详见 docs/notes/release.md「打包 / 卸载 / 默认自启(NSIS 钩子 + .v2 标记)」
- **发布流程 / 版本说明**:**版本号一条命令改全** `scripts/bump-version.sh X.Y.Z`(**别再手动逐处改**)。**「发布介绍」= CHANGELOG.md 驱动、CI 自动填**(每版一节 `## x.y.z — 日期`,release.yml 抽本版节喂 `releaseBody`;口径 = 面向用的人、一句导语 + 每条一行,**「待真机确认」块不进 CHANGELOG**)。**触发 = push tag `v*`** → CI 建 **draft** Release = 真机验收闸,验过再手动 Publish。**CI 范式:单 `create-release` job 先建唯一草稿 → build matrix 用 `releaseId` 上传同一个 release(自动合并 latest.json)→ `finalize-manifest` 把资产 url 前缀成镜像(`MIRROR=ghfast.top`,换镜像改一处)+ 从 CHANGELOG 注入 `notes`;⚠️ 绝不让 matrix 各 job 自带 `tagName`+`releaseDraft` 建 Release(竞态出多个同 tag 草稿)**;手动触发只出 artifact。CI 平台 = mac + windows。**一键更新 `tauri-plugin-updater` 全链已落地**(每日 / 手动查 + HUD 下载 + 装重启;updater 每平台只认一个 url 不支持故障转移,前端「开了代理就带代理重下一次」兜底);只能发真 Release 验。**trunk-based**(代码 / tag 直接进 `main`,本机无 `gh`)。**Windows 代码签名 = 已知债、用户拍板搁置,别当 TODO 捡**(SAC 机器拦死未签名包无「仍要运行」可绕,云信誉按哈希 → 自动更新的新鲜哈希每次都可能失败;根治 = Authenticode,首选 SignPath Foundation,CI `bundle.windows.signCommand`,**Authenticode 必须在 updater minisign 之前**;与 `TAURI_SIGNING_PRIVATE_KEY` 无关;Mac Gatekeeper 同类债不还;重启信号 = 用户面扩到陌生家庭 / 用户重提)。→ 详见 docs/notes/release.md「发布流程 / 版本说明 / CI 范式 / 一键更新镜像 / 代码签名债」

### 7.7 远程渠道:Telegram / 钉钉 bot(2026-06-17 落地;手机上跟旺财对话)
- **物种 = 交互渠道(§5 species ②)**,不是工具:引擎边界适配器 + **复用 turn loop**,不碰 ToolCtx、不内嵌人格。core 新模块 `channels/`(`telegram.rs` / `dingtalk.rs` 一渠道一文件 + 监督器),**单 crate 不拆**;壳层只 `ChannelSup` 监督(boot 起 / `reload_channels` 停旧起新,顶层 spawn 用 tauri runtime——core 不依赖 tauri,§6.1)。
- **复用现成,engine 零改**:入站文本 → `channels::drive_turn`(会话映射查 `store/channels` 域 → `inject`(在飞)或 `send_message`(空闲)→ 消费 `Receiver<TurnEvent>` 攒 Delta 到 Done)。inject-or-send 与桌面前端同语义。
- **渠道会话管理**(用户拍板,取代「回访同一 chat 恒续同一会话」):每个单聊 chat 独立会话;**单聊闲置 12h 轮换**(判据 = 会话 `updated_at` 任意角色;chat 的会话 = 名下最近有动静的那个,提醒刚到点的老会话算有动静),超 12h 全凉才开新会话;**群聊不轮换**(TG `chat.type=private` / 钉钉 `conversationType` / 微信恒单聊)。**映射 = 历史行**(append-only,迁移 0023 去 UNIQUE):改绑插新行继承指认 / 昵称 / 推送地址,老行留档(`thread_by_conv` 反查不断链);`set_push_id`/`set_label` 按 (channel, ext_id) 全行保鲜;`bind_user` 追溯改全行;家人页 `list` 只出最新行。**映射悬空自愈**:桌面删渠道会话 → 名下无活会话 → 下条消息自动重建重绑。阈值单源 `channels/mod.rs::FRESH_CONV_IDLE_MS`;解析收口 `resolve_conv`。→ 详见 docs/notes/channels.md「渠道会话管理:单聊 12h 轮换 + 映射历史行 + 悬空自愈」
- **不流式、攒到 Done 一次发**(两家都不支持流式);长消息 `split_message`(Telegram 4096)。
- **不按渠道注入 prompt**(破前缀缓存,§7.5 同理)。**出站富格式 = 输出后处理**:`channels/render.rs::looks_markdown` **保守探测**(漏判 = 发纯文本零风险)→ Telegram 转 HTML parse_mode(`to_telegram_html`,只产受控标签、原始 HTML 一律转义、表格退化 `<pre>`;源文按 3200 切片再逐片转;转出超 4096 或 4xx → **该片降级纯文本重发**)、钉钉 msgtype markdown(title=`md_title`);纯文本回复走老路零处理;**绝不走 MarkdownV2**;回合回复与提醒推送共用同一出口。→ 详见 docs/notes/channels.md「不按渠道注入 prompt + 出站富格式后处理(绝不 MarkdownV2)」
- **手机端补全**(原「MVP 纯文本」入站半边解除):① **提醒推回手机**:`outbound_loop` 订阅全局事件车道(engine 零改),见 `ConversationActivity{kind:"reminder"}` → `thread_by_conv` 反查 → Done 推最后回复 / **Failed 推 event 行提醒原文保底**(到点必须有动静);TG `sendMessage`;钉钉 sessionWebhook 有时效 → `oauth2/accessToken` 现取 + `batchSend` 单聊主动推(收件地址 = 入站顺存的 `senderStaffId`,迁移 0018 `push_id`;群聊不推);不加开关,推送失败只 warn。② **语音消息能听**:TG getFile(20MB)→ ffmpeg 解 16k PCM(`decode_audio_pcm16k`)→ 本地 ASR(`transcribe_pcm`),`input=voice_msg`、**speak 恒 false**;>60s(`VOICE_MSG_MAX_SECS`)/ ASR 未就绪(回「准备中」+ `prefetch_asr`)/ 转写为空全部如实回话;钉钉**优先用自带 `recognition` 转写**。③ **照片能看**:取最大档 → 桌面同缝 `InAttachment`(图当轮不落库),caption 当消息文字;钉钉 richText 也收。④ **看不了的不再已读不回**:内容型消息回一句提示,服务性消息静默;**文档能读**(`attach::doc_supported` **下载前预检**,按扩展名认出的原图也收;压缩包 / 视频仍回「看不了」,`UNSUPPORTED_HINT` 话术随能收清单同步;TG >20MB 下载前回「太大」)。架构:channels **组合** voice/media 两个 core 运行时(壳层装配传入,不反向依赖)。→ 详见 docs/notes/channels.md「手机端补全:提醒推手机 / 语音 / 照片 / 文档 / 看不了必回话」
- **入站附件攒批 = 文件与意图分离**(用户拍板 A;手机发文件多半不能同时打字):**不按渠道分,按「这条附件消息带没带文字」分**——带文字直接处理;**空文字的附件先攒、不触发回合**,等用户发来文字连同一起进一个回合。`ChannelCtx.AttachBuffer`(per (channel, ext_id),三渠道共用):`buffer_attachments` 缓冲空→满返 true = 发一次提示(**连发多个文件只第一个吭声**),`take_attachments` 用户发文字时取走全部(`ATTACH_TTL`=30min 丢弃);接在三渠道各自唯一出口(微信 `handle_message` / 钉钉 `run_reply` / TG `reply_turn`);提示话术 `ATTACH_HINT`(极简版)。**桌面不做**(用户在屏幕前)。单测 `attach_buffer_debounces_and_merges`。→ 详见 docs/notes/channels.md「入站附件攒批 = 文件与意图分离」
- **出站全走 `net::Client`**(§4.6):Telegram 全程 HTTP 长轮询(`getUpdates`/`sendMessage`,免公网免 SDK;国内需代理由 net 直连失败自动兜底);钉钉「开连接」+ 回复 `sessionWebhook` 走 net,**只有 WS 收消息用 `tokio-tungstenite`**(钉钉国内直连,WS 不经代理;TLS 走 rustls 复用进程级 aws-lc provider)。**别用 teloxide**(自带 reqwest 绕过 net)。
- **钉钉 = 官方 Stream 模式**(WebSocket,免公网,robot 同款):WS 只收 + 回 ACK/pong;回复走 sessionWebhook(HTTP)→ 回合可异步 spawn、不阻塞收循环、不丢 ping。单聊按 conversationId 续接、群聊按 (conv, 发言人) 隔离 + strip @mention(robot 坑 #2/#7)。
- **访问控制(非风控 §9)**:Telegram `allowed_chats` 白名单,空 = 不放行 + 回 onboarding 报 chat id(不静默吞,§3.5);钉钉靠应用可见范围。
- **凭证不过桥**:token/app_secret 走 `set_setting` 的 `remote.*` 写入臂(**不进 `APP_SETTING_KEYS`** → 写得进读不回);设置页状态读 `remote_status`(只报 `configured` bool + 连接态);**凭证已与 LLM key 一并走 keyring**(§6.3,`SECRET_KEYS` 含 `remote.*` 三把)。
- **渠道归人 = 多用户第一步**(确定性归人先行):每条渠道对话(`channel_threads` 映射行)可在设置·家人**指认**给一位家人(`user_id` 列,NULL = 会话归属者);入站 `drive_turn` 带 `UserMeta.speaker_user` → **记忆 / 需知 / 工具(`ToolCtx.user_id` = mem_user,提醒随之)归说话人**,人格 / pet_name / 会话归属仍走会话归属者(前缀字节稳定);装配加确定性标记 `〔某某说〕`(名字单源 users 表,每回合现查)。平台昵称只作家人页 `label`。**⚠️ 身份别按活跃度推断,锚死「最早建的用户 = 主人」**(`ensure_default_user` / `users.list` 按 `id ASC`,与 `last_active_at` 彻底解耦;回归 `family_creation_never_steals_boot_owner`);**`users.create` 建号绝不算活跃(last_active_at=0)**;**`users.touch` 只 touch 会话归属者、绝不 touch 说话人**(回归 `speaker_user_marks_context_and_keeps_boot_owner`)。删家人时 `delete_user` 连带 `channels.unbind_user`(映射保留)。→ 详见 docs/notes/channels.md「渠道归人 = 多用户第一步(主人锚定 id ASC / touch 只 touch 会话归属者)」
- **出站文件 = `send_file` 工具 → `channels/outbound.rs`**:把本机文件发到家里人的手机;**缺省 = 说话人**(`ToolCtx.user_id`),**`to` 填家人名字 = 发给那位家人**(用户拍板放开跨人,家庭信任不设闸);目标解析 = 归人映射反着用(`resolve_target`:指认给 TA 的线程算 TA 的,TA 是主人时未指认的也算;多条取最新;钉钉群聊无 `push_id` 跳过),找不到给明白话;**`channel` 参数点名渠道**(一人多渠道时「发我微信」曾发去钉钉;**工具 description 的渠道清单必须随新渠道同步**);**`find_member` 名字宽匹配**(精确优先,无命中放宽到「家人名包含所填」,唯一命中才取、多命中报名单;send_file.to / reminder_set.for 共用)。TG = `sendDocument` multipart(≤50MB 一律按文档发);钉钉 = 旧接口 `media/upload`(`gettoken` 旧 token)+ 新 token `batchSend`(sampleFile;**图片走 sampleImageMsg**),≤20MB;钉钉不支持附言 → note 补推一条文字。凭证走 secrets,`Target` 的 Debug 只露渠道名。tools → channels 单向引用先例。→ 详见 docs/notes/channels.md「出站文件 send_file(缺省说话人 / to 跨人 / channel 点名 / find_member 宽匹配)」
- **出站文字 = `send_text` 工具**(send_file 的文字姊妹):把一句话 / 链接**原样**发到说话人或家人的手机;`to`/`channel` 语义与 send_file 完全同源;机器件 = `outbound::send_text` 汇三渠道**既有** push 出口(零新协议代码);与「捎话」分工进工具描述(转述 / 到点提醒走 reminder_set(for),原文照发走这里;send_file 描述补「只发文字用 send_text」);微信窗口关死同享挂起补发(`PendingSend` 扩 `text` 条目,serde default 向后兼容);>2000 字如实退回指路 send_file。→ 详见 docs/notes/channels.md「出站文字 send_text」
- **微信 = 腾讯官方 iLink bot 渠道**(用户拍板「全做」,推翻「微信暂不做」:前提已变——iLink bot HTTP API 出站长轮询 + 免公网,形态同 Telegram)。**不装 npm 插件、不引 SDK,用 `net::Client` 裸接协议**;协议同构 telegram.rs(长轮询 `getupdates` + `sendmessage`,`Bearer <bot_token>` + 固定头),协议常量是协议事实单源 `weixin.rs` 顶部(非 §4.11 产品默认)。**比 TG 多三件**:① 扫码登录(`get_bot_qrcode` → 轮询状态,`qrcode` crate 渲 SVG);**★多绑定 = 「一人一 bot」**(`remote.weixin.accounts` secrets 数组,每账号一路长轮询,出站按线程 ext_id 选账号;放行 = 手动白名单 ∪ 全部绑定者,**扫码不写白名单**)。② **`context_token` 回显**(复用 `channel_threads.push_id`)——**令牌会过期,主动发撞 `ret=-2`;去令牌重发降级已真机证否(会话窗口 = 平台反骚扰设计,协议无刷新 API)→ 治法 = 挂起补发**:错误带 `weixin::StaleContext` 类型标记,send_file/send_text 把件挂 `remote.weixin.pending_sends`(48h,同目标同内容去重),结果 **Ok 如实说**「已挂起、TA 一开口自动补送」,收循环拿到新令牌先 `flush_pending_sends`;**提醒推送刻意不挂**;响应判定 **ret / errcode 双字段**。③ 媒体 AES-128-ECB 解密喂 `InAttachment`,语音先用 `voice_item.text`(SILK 后置);出站文件 = `getUploadUrl` → AES 加密 PUT CDN。**账号风险如实记档**(个人微信绑 bot 残余风险非零、借 OpenClaw 身份壳)。接线全镜像 TG/钉钉(`SECRET_KEYS` 加 `remote.weixin.token`)。→ 详见 docs/notes/channels.md「微信 = 腾讯官方 iLink bot 渠道(多绑定 / context_token 挂起补发 / 媒体加解密)」
- **✅ 2026-06-30 真机/真网验过**:真 bot token / 真钉钉应用、手机收发、续历史、断网重连、钉钉 WS 在 Windows 连通——见 PLAN 远程渠道 watch-items。

### 7.8 轻办公原语:二维码 / 下载 / PDF 转图 / 发手机(2026-07-10 落地;发票链是试金石不是任务工具)
- **纪律重申(§5)**:这批全是**正交原语**,绝不造 `process_invoices` 类任务工具——用户的发票流程(拍二维码 → 开网页挑下载链接 → 落盘 PDF → 转 PNG → 发回手机)= `qr_decode` → `web_fetch`(页内链接)→ `web_download` → `pdf_to_png` → `send_file` 由模型自由组合,每个原语单独都有通用价值(WiFi 码 / 下载任何文件 / 任何 PDF 转图 / 发任何文件)。这正是 §5「没预设过的组合任务」试金石的实弹。
- **场景词汇不进代码(2026-07-10 用户抓包立规)**:动机场景(「发票」)只活在 AGENT/PLAN(决策记录 + 真机验收单);**工具 description 是提示词,零容忍**(往里写场景词 = 任务偏置渗进底座,当场抓到 qr_decode 一处已清);代码注释/测试夹具也保持中性词(单据/附件/样张)——判据 = 在 `larkwing-core/src` + `src` 里 grep 场景词应为零。建新原语照此自检。
- **`qr_decode`(rxing + image,纯 Rust 全本地)**:缺省吃「刚发的那批图」(`ChatRepo::recent_image_attachments` = 最近**一批**:从新往旧走 user 行,进带图区块后遇不带图的 user 行即止),也收绝对路径;单次 ≤10 张超额如实退回。**解码器选型必须真拍照验,生成的干净码测不出差距**(rqrr 对热敏打印晕染照全灭 → rxing + **预处理阶梯**:≤2000px 原样 → Otsu 二值化 → 缩 800px → 原尺寸兜底;矩阵实验 `tests::probe_matrix` 吃 `LW_QR_PROBE` 真图);rxing 钉 image=0.25.8。**PDF 里的码 = 先 pdf_to_png 再认**(组合,不给 qr 加 PDF 入口)。→ 详见 docs/notes/web-office.md「qr_decode(rxing + Otsu 阶梯;解码器必须真拍照验)」
- **`pdf_to_png` = pdfium 组件**(components 第三成员;bblanchon 预编译 **tgz**,`Archive::TarGz`;无 SUMS 只靠 TLS 同 ffmpeg 记档)。**pdfium-render 绑定每进程只许一次**(二次 bind 报 AlreadyInitialized)→ 进程级 `OnceLock` + 全局互斥闸,取用口收口 `pdf::with_pdfium`。渲染 1600px 宽、单页产物不带 `-pN`、≤20 页超额让模型带 `pages` 分批、产物 `dedupe_path` 永不覆盖。也是扫描件 OCR 的「栅格化」半边机器件。✅ Mac 真网 e2e;Windows DLL / 中文字体渲染待真机。→ 详见 docs/notes/web-office.md「pdf_to_png = pdfium 组件(每进程只许 bind 一次)」
- **`read_image` = 拉取式看图原语**(用户拍板「给『读文件』配一个『读图』」;pdf_to_png 自动附图否决,组合替代:pdf_to_png 出路径 → read_image 看):把本机图片附进工具结果里亲眼看;缺省「刚发的那批图」(超封顶只看最新 5 张并如实说明),也收绝对路径;附前等比缩到最长边 1600 + 统一重编码(透明 PNG / 无透明 JPEG q85),单次 ≤5 张(视觉按张计费,封顶低于 qr)。**只有视觉主脑真能看到**(非视觉收到丢图留话,描述引导「看不了就别调、如实告诉用户」)。视觉子调用(给非视觉主脑代看)没做——默认 DeepSeek 用户看不懂画面 = 有意现状。→ 详见 docs/notes/web-office.md「read_image = 拉取式看图原语」
- **`qr_encode` = 出码半边**:文字 / 链接 → 二维码 PNG(`qrcode` crate 本就在依赖树),落下载夹或 dir(guard create;`qrcode.png` + dedupe 永不覆盖);内容上限 2000 字节(M 纠错档保守值),超了退回指路 send_file;WiFi 格式串归模型知识,工具只管「字符串进码」;组合出路 = show_image / send_file / desktop open;回环测试 = 出码后自家 decode 认回。→ 详见 docs/notes/web-office.md「qr_encode = 出码半边」
- **★`show_image` = 亮图原语**(read_image 的镜像姊妹:read_image = 模型自己看,show_image = 亮给**用户**看;用户拍板通用定位不绑二维码):paths ≤6 → expand_home + guard read → web 安全格式(≤10MB)**原字节**进用户发图同一内容寻址仓(`files::save_image_blob` / `image_ext` 单源,gif 不重编),超大 / 怪格式重编码展示副本(≤2000px);**双路呈现**:live = `TurnEvent::Shown{attachments}`(tagged 增量)当场插进在飞气泡组;落库 = tool 行 payload `attachments`(serde default),重开会话由 `toUiList` 挂邻近 wang 段(**持有点恒 = wang 消息的 attachments,不双渲染**)。**图一个字节都不喂模型**(`ToolOutput.shown` UI 专道,零视觉费、不落 DB、不回放)。**渠道回合如实退回指路 send_file**(`conv.channel ∈ {ui,system}` 才亮)。图卡 `.atts-wang` 340px 上限(码要隔几十厘米扫得动)。→ 详见 docs/notes/web-office.md「show_image = 亮图原语(图一个字节都不喂模型)」
- **`web_download` / `send_file`** 见 §7.4 / §7.7 对应条。
- **`web_render` = web_fetch 的「真浏览器」档**:**app 自己就是浏览器**(壳层开 WebView 窗真渲染;headless Chrome / WebDriver / 云渲染 / 纯 Rust JS 引擎全否)。core `webrender.rs` trait 接缝(壳层 boot `Engine::set_web_renderer` 注入、经 `ToolCtx.web` 进工具,没注入 = 如实说没有渲染组件)→ 壳层注入抽取脚本 → 快照 POST 回 relay `/collect/{token}`(一次性信箱、loopback、512KB;**不给远程页任何 IPC 桥**)。**会话式浏览**:窗跨调用存活(TTL 180s / 最多 2 窗),每步回**编号快照**(`data-lw-ref`,编号跨页即失效如实报)+ `back` + 跳新页。**动作空间**:click_ref / click_text / type_ref / fill / select_ref / press_key / submit / scroll / wait_text / upload_ref(base64 分片暂存 → File 赋 `input.files`,隐藏文件框豁免可见性过滤,总量 50MB 单源 `UPLOAD_MAX_BYTES`);JS 合成事件走 native setter + input 事件 / `requestSubmit()`;填后快照回读 = 天然校验;`click_note` 把动作折扣如实说。**密码类字段标出但拒代填**,凭证 / 验证码 / 登录墙 = 人机接力(用户在可见小窗自己操作后带 session 继续);**CDP 可信输入 defer**。**观察形态(与动作互斥、单独一步)**:`read=true` 通读(当下 DOM 正文定格进会话,12 万字封顶,offset 切片 6000)/ `save_pdf=true`(mac createPDF / Win PrintToPdf,文件名取窗题 + dedupe)/ `screenshot`(原生 FFI 抓当前视口,`with_webview` 主线程发起 + oneshot 桥回**绝不在闭包内同步等**,8s 超时;没窗不给)。**路线:主干 = DOM/文本编号快照,截图是可选增强,纯视觉 / CDP 否;动作空间永不因有无视觉分叉**。**可见任务窗**(右下缩略直播、不抢焦点、关窗 = 会话收摊)+ HUD 任务卡。**通用件三修(不为单 case 做流程)**:弹窗驯服(`window.open` / `_blank` → 本窗)· 点击目标排序(精确 > 包含、最内层优先,回传 `clicked_desc`)· 点击后跳转回报(`post_click_url` 交模型接 web_download)。下载由 tauri `on_download` 接管(Requested 时定落点;**mac 的 `Finished.path` 恒空**);Mac 点 PDF 链接内联打开 → 工具层保住 `post_click_url`(需窗 cookie 的 PDF 可能失败 = mac dev 边界)。确认闸见下条。→ 详见 docs/notes/web-office.md「web_render = 真浏览器档(会话式浏览 / 完全操作 / 上传 / 截图 / 通读 / 存 PDF / 可见任务窗)」
- **`web_download` 松绑:认证 + 大文件转后台 + 下载器专用链拆封**:① `tools::normalize_link` 拆 `thunder://` / `flashget://` / `qqdl://`(只是 base64 包装;拆不开原样返回,拆出磁力指路 torrent_download、ed2k 如实说下不了);② **同步档 `DOWNLOAD_SYNC_MAX_BYTES=50MB` 必须保持不变**(下完立刻接 pdf_to_png 靠回合内即时性),超它 → 转 bgtasks job(`DOWNLOAD_JOB_MAX_BYTES=50GB` 与 torrent 同口径,后台档不设总超时);**服务器不报 Content-Length 就转不了档**,话术点明;③ **认证按 host 现查、凭证绝不过桥**:秘密键 `net.http_creds`(keyring),**刻意不做成工具参数**(密码走参数会进 LLM 上下文 + 落 payload + 每轮回放),增删一律后端读改写,设置·系统「下载认证」卡;④ WebDAV = 带认证的 GET → 坚果云 / 群晖 / Nextcloud / 自架 OpenList 白拿,**一行网盘私有 API 都不写**;⑤ **明确不做**:网盘私有 API(限速是服务端按账号打的;PanDownload 作者已判刑;军备竞赛)· ed2k · aria2 换 librqbit;⑥ NAS/SMB 是普通路径。**自审记档**:磁力哈希按字符取(按字节切多字节 panic);体积闸 / 清单 / 进度按 `only_files` 过滤后算;成败由 `Result` 决定不嗅探字符串;`HttpCred` 手写 Debug 遮密码;跨主机重定向不会泄漏 Basic 凭证(reqwest 自己剥),别再当隐患;**MSRV 1.77.2:`is_none_or` 是 1.82 才稳定的**,用较新标准库方法前先看 clippy MSRV 检查。→ 详见 docs/notes/web-office.md「web_download 松绑:认证 + 大文件转后台 + 专用链拆封(+ 自审补丁)」

- **FTP 下载 = `web_download` 扩一个分支,不新开工具**(用户实锤「荐片大部分是 ftp 资源」,推翻「FTP 不做」;「下载一个文件」是一个原语,模型不该按协议挑工具;顺带闭环 normalize_link 拆出的 ftp://)。**「只有迅雷下得动的 ftp 链」= 那台服务器已经死了,不是协议有门禁**(FTP 无 UA 可识别客户端;迅雷从自己 P2SP 缓存给字节根本没碰 FTP);判据:连不上 = 服务器死了,连上 550 = 文件名对不上。机制 `larkwing-core/src/ftp.rs`(`suppaftp 10` 仅 tokio 不带 TLS):`parse_ftp_url` 解内嵌凭证 / 非标准端口 / 百分号编码中文名(没带凭证按匿名)→ 被动模式 → `probe_size`(取不到不当失败)→ 流式写盘打点查取消;**档位与 http 一致**(50MB 回合内 / 超了转 bgtasks);凭证两路(URL 内嵌优先 → 按 host 查 `net.http_creds`),`FtpTarget` 手写 Debug 遮密码。**⚠️ 中文文件名 GBK**:suppaftp 只收 `&str`,发 GBK 字节要 `from_utf8_unchecked` = UB **明确不进产品**,只发 UTF-8(`OPTS UTF8 ON`),撞老服务器 550 话术点名 GBK。**断点续传 `REST` 已接**:中断 → 重连 → `resume_transfer(offset)`(offset 每轮现读 `.part` 真实长度),`RESUME_MAX_ATTEMPTS=5` 退避 2→4→8→15s(§4.11 待确认),每次续传 info 日志有痕,REST 被拒立即退回「不支持断点续传」不空转,收到字节 > SIZE 作废;`STALL_TIMEOUT=90s` 触发续传;测试 = 假 tokio FTP 服务器四条端到端。记账:**不走 `net::Client`(FTP 不是 HTTP)→ 代理缺失**;suppaftp 单人维护;跨进程重启不续传。超时单源 ftp.rs(建连 20s 死链快失败 / 命令 15s / 停滞 90s)。→ 详见 docs/notes/web-office.md「FTP 下载(web_download 分支;死链判据 / GBK 边界 / REST 断点续传)」

- **★动作确认闸**(「先能力后闸」的「后」):**准入判据(新工具自检项)= 动作后果「出圈且收不回来」才闸**(圈 = 这台电脑 + 家庭信任圈;fs 写 / power / send_file / 跨人提醒 / web_download 全不闸,只有 web_render 在第三方网站的部分动作过线;将来「发邮件」类工具拿 `ToolCtx.confirm` 即接)。**触发 = 高危词表 ∪ 模型自报(`confirm: true`),单向阀**;词表用户拍板、单源 `confirm.rs`(付款 / 支付 / 下单 / 转账 / 发布 / 删除 / 注销 / 签署 / 授权… + 英文短语词边界;裸 `sign` 刻意不收;**泛词「确认 / 提交 / 发送 / 保存」不进表**;偏向宁多问,误伤按数据删词)。**机制 = 壳层动作执行点判定 + 两阶段重发**:先选目标回活 DOM 文本 → core `risky_hit` 单源判 → 命中且未 confirmed 就不执行回 `needs_confirm` → 工具 run 内阻塞问用户(engine 零改)→ 允许 = 带 `confirmed + expect_text` 重发(文本变了按 stale 收);**`confirmed` 是内部字段不进工具 schema**;拒 / 超时 = 观察不是错(禁「换路硬做」);`spec.timeout` 210s。软路(press_key / 无文字图标 / 挂羊头按钮)由自报 + 可见任务窗 + 密码框拒代填兜;安全带不是保险库。**确认路由到回合来源,先到先得**(桌面 60s / 渠道 120s):`Confirmer` + `AppEvent::Confirm` 全量快照卡 + 审计落库,drop-safe;桌面 = HUD 确认卡 + 悬浮窗;渠道 = 推回发起 chat,回话过**代码层严格词表**(确认 / 继续 / 好 / 是 / ok / yes 精确匹配 = 允许,**其他任何回复 = 拒 + 照常 inject 进在飞回合**,不交模型仲裁),推不出去立即按 `unreachable` 拒;**TG / 微信收循环必须 spawn 回合(内联 await 会卡死收循环)**;语音 = 念问句走 `pushDelta`(**绝不 speakText**)+ `Phase::ConfirmListen` 听一段(cpal 源不开听),肯定精确匹配 / 否定子串即拒 / 听不清不算数。**审计 = `confirms` 域流水 + 足迹页「确认过的操作」**;**审批等级 / 总开关 / per-host 免问 / 「这个会话别再问」都不做**(能关的闸出事就是产品事故);谁有权点头 = 认会话不认人;卡片只过桥 kind + 目标原文,动词前端字典。→ 详见 docs/notes/web-office.md「动作确认闸(准入判据 / 词表∪自报 / 两阶段重发 / 四通道路由 / 审计)」
- **`speak_to_file` = 声音明信片**(`read_audio` 的镜像:听 ↔ 说):把一段话用当前音色(含克隆 / 离线 vits)读成音频文件落本机,接 send_file 就是语音留言。`text` ≤ `SPEAK_MAX_CHARS=1000`(超限退回让模型拆段,**绝不静默截断**)/ `name`(默认 `留言`,sanitize + 剥音频扩展名)/ `dir`(缺省下载夹,授权圈 create);`ctx.voice` None 退回「语音组件没就绪」→ `VoiceRuntime::tts_to_file`(edge=mp3 / 克隆·离线=wav)→ **拷贝**到 `dedupe_path` 永不覆盖、缓存件不动。四件套 + 五件套齐(delegate 保留集 +1,产物型无现场交互)。常量 §4.11 待确认。记档:wav 在手机多半跳系统播放器、TG 不显示成语音条,体验差再议转 mp3 / `sendAudio`。→ 详见 docs/notes/web-office.md「speak_to_file = 声音明信片」
- HUD/文案:五工具 `ui_key`(`tool.*`)+ `task.download.pdfium` 已进 zh/en 字典(§6.6)。

### 7.9 技能(工作手册)= agent 的、恒全局(2026-08-04 落地;源起 = 业内 Agent Skills 调研)
- **定位(用户拍板三分账)**:技能 = **agent 的**工作手册/准则(指导完成某类事的做法),**与用户无关、恒全局**(无 user_id 无 scope);记忆 = 与用户的「约定」,归人;家庭备忘 = 环境事实。LAWS「怎么记事」两本账 → **三本账**:人的事(含一句话偏好)→ remember;环境 → briefing_write;**多步骤完整做法 → skill_write——只在用户明说「以后就这么办/记住这个流程」时记,记完复述确认(最保守档,自动沉淀明确不做)**。
- **三层渐进披露全在库里**(业内 skills 的机制内核,**刻意不走文件形态**):L1 索引(name + when_to_use)**恒常驻前缀、无折叠机制**(用户拍板「路径不产生折叠」;量由写入 backstop 管——满 64 条如实退回,绝不静默折叠索引)→ L2 正文 = `skill_lookup` 按需取 → L3 附录节(`skill_sections`,带 section 再取一层;「另有附录:…」导航行**机器生成**、作者只管分节)。不走文件的理由:①触发统计定义会碎(fs_read_text 绕过 lookup);②与文件授权圈不纠缠;③进 SQLite = 备份/恢复/搬家白拿(§4.3/§6.2)。
- **触发的唯一定义 = skill_lookup 命中一次**(`skill_hits` 流水只进不改,同 usage_rounds 形态;统计三数字〔总次数/近7天/最近〕由它现算)。**undertrigger 是业内自认第一坑**:一期靠 LAWS「技能」节 pushy 措辞(「对得上就先取手册照着做」)+ 技能页统计可观测(「教了 30 天零触发」用户看得见);若真机实锤教了不用,二期再上装配层确定性注入,先不做。
- **内置技能 = 出厂数据**(`assets/skills.json` include_str!,`skills_builtin.rs` 解析,boot `sync_builtins` 按 slug 刷内容、**enabled 保留用户状态**、孤儿 slug 清理),现 14 条:放歌放视频(从 companion persona 抽出,persona 回归纯性格)/ 网页上办事 / BT下载 / 整理文件 / 定时播报(重复提醒内容必须物化成自包含「到点做什么」)/ 音视频加工(ffmpeg 配方 + 第 6 条「成品封装」:**时长 / 可拖提示、海报不提示**——`frag_keyframe+empty_moov` 出的 mp4 mvhd 时长为 0,咱们 BMFF probe 当无时长;成品出普通 .mp4 + faststart,产完不给 output 再探一眼;音乐封面另议)/ 看片守约定(**没约定绝不自己发明规矩**,孩子转述拿不准先问家长)/ 存歌进曲库 / 报听写(TTS 无逐词停顿控制如实记档,真机不行就撤)/ 睡前故事 / 盯网页变化(天级三档如实说、内容自包含带基准、变了才开口、续盯撤旧设新)/ 电脑清理与提速(点头哪个才动哪个、恒 fs_trash、系统目录绝不碰、启动项点名才动)/ 给歌配歌词 / 规划旅行(关键事实现查、plan_set + delegate 分头、行程落 markdown + briefing 记路径、支付恒交用户;高德 API / 行程日历页 / travel_plan 工具明确不做)。**内置可关不可删不可改**(觉得不合适 → 关掉自己教);用户教的撞内置名拒收。内容纪律同 few-shot:路径一律占位、不嵌具体可复用事实;工具名可出现。→ 详见 docs/notes/skills.md「内置技能 = 出厂数据(14 条清单与各条要点)」
- **技能 tab = 一等导航**(rail 第六位,记忆与足迹之间;= §3「用户面零新概念」的一次**有意突破**,用户 2026-08-04 拍板。理由:流程知识与记忆的生命周期不同——坏技能让模型持续做错事,天生需要「看得到列表/看得到触发/能停用」三样,业内 Manus/Devin 均独立管理面):列表 = 名称 + 内置|你教的 chip + 时机 + 统计三数字 + 启停;点卡展开正文(附录名折叠列出);删除仅用户教的(两步确认);「技能」进「说人话」可说名词白名单(同「小本本」)。UI 编辑正文一期不做(对话改)。
- **明确不做(均有调研实锤,别再提)**:接第三方 skill hub / 导入外部 skill 文件——Snyk 2026 扫描社区市场 36% 有安全缺陷、13.4% critical,2026-02 已发生真实供应链攻击;且内容语义对不上(社区 skill 写给有 bash 的 coding agent,家庭场景 skill 外面不存在)。skill 内可执行脚本(我们的工具原语 = 更强的 L3:类型化+确认闸+授权圈)。技能分用户/分场景。「hub 需求」的正确形态 = 出厂技能库随版本更新。
- 单源常量:`tools/skill.rs`(SKILLS_MAX=64 / 名称 24 / 时机 80 / 正文 4000 字)、`store/skills.rs`(统计窗口 7 天)——数字按用户授权「宽松、不折叠」原则定,改档回来问(§4.11)。接线纪律:技能三件是 BASE_TOOLS(法条点名),加场景不用声明。
- eval 守卫:`skill-teach`(明说才记)/ `skill-no-teach`(不乱记)/ `skill-trigger`(索引对上先 lookup)。**Mac 全绿 593+16 / clippy 触点零警 / vue-tsc / i18n 镜像 752 双侧相等 / 预览 ?demo 中英验过**(tab/列表/统计/展开/启停);真机 watch 见 PLAN(真模型教技能全链 / 触发遵循度 / 内置升级刷新保 enabled / 渠道与语音教技能)。

### 7.10 PC 管家三件:打印 / 系统快照与启动项 / 自动备份(2026-08-31 落地;用户拍板「123」三件一批)
- **定性**:**不做「电脑管家」品类**(工具箱思维违 §3),吸收真痛点(慢了 / 满了 / 该打印 / 该备份 = 一句话说给 BT)。**明确不做(别再提)**:杀毒 / 弹窗拦截 · 驱动管理 · 注册表清理 / 内存加速球 · 孩子上网管控。**二线缓做**:软件装卸(winget)· 网络诊断。→ 详见 docs/notes/pc-butler.md「PC 管家三件:定性 / 打印 / 系统快照与启动项 / 自动备份 / 复审」
- **打印 `print_file`**(paths ≤10 / copies ≤20 / 单 PDF 可 pages / 总页次 ≤100 超了如实退回):一期只收 **PDF + 图片**,**按内容认格式**(图片必须从裸 reader 起步 `ImageReader::new(BufReader)` + `with_guessed_format`,`ImageReader::open` 带扩展名提示会把垃圾内容判成图);Office 文档如实退回指路「先转 PDF」。**Windows = 自绘 GDI 路**(pdfium 300dpi 栅格化 → `StartDoc/StretchDIBits` 等比居中、宽图横放、**透明合白底**;**绝不走 ShellExecute "print" 动词**;列打印机走注册表 `Print\Printers` + `HKCU\Printers\Connections`;`AbortDoc` 兜半截);Mac dev = `lp`。**pdfium 取用口收口 `pdf::with_pdfium`,别处再开第二个 OnceLock = 二次 bind 必炸**。不过确认闸;送进队列即返回。
- **系统快照 `system_status` + 启动项 `startup_list` / `startup_toggle`**:`system_status` 只读聚合(CPU 两次采样 + TOP10 进程**按名字聚合**、÷核数对齐任务管理器;内存;各卷容量 / 剩余;开机时长;`sysinfo 0.33` 被 MSRV 钉、只开 system+disk);启停**与任务管理器同一套机制** `Explorer\StartupApproved\{Run,StartupFolder}` 标志位(程序本体不动、互相可见、天然可逆;补 `WOW6432Node` + `Run32` 视图),列四源,**toggle 只改当前用户两类**(HKLM 如实指路任务管理器;同名多处只一处能改才动);**计划任务一期不碰**;「用户点名才动、绝不批量禁」进技能;非 Windows 如实退回。记档:用 startup_toggle 禁 Larkwing 自己会与设置页开关漂移,刻意不写具名特判。
- **自动备份**(用户拍板四数:**每周 / 保留 10 份 / 默认关 / 机器清理绝不经 LLM**):`autobackup.rs` **水位线**(不走 jobs / scheduler、不经模型;boot 延迟 3min 首查、之后 10min 一查);开关语义 = `backup.auto.dir` 非空即开(选完目录**立即出第一份** `auto_backup_now`);**轮转 = 机器清理** `prune_backups` 只认自家命名、按**文件名**时间戳排序、**先备后删**留 10、**改名 = 钉住永不删**、直接删不进回收站;**按份数不按天数**;失败纪律:单次 warn 静候,超期 ≥14 天且本次也失败才 toast 一次(3 天频控,`AppEvent::Backup`);状态键写失败必 `warn!`;`AutoBackupStatus` 过桥 `keep`/`interval_days`(设置页不写死数字)。常量单源 `autobackup.rs`。`last_error` 仍是 core 中文错误链直透(§6.6 口径不纯,待议)。
- 公共接线:四件套 ×4 + delegate 五件套判定(startup_toggle / print_file 归现场交互排除);eval 守卫 `print-recent-file`(**夹具内容故意不是图片,eval 机连着真打印机也绝不真出纸**)/ `slow-pc-diagnose` / `startup-no-mass-disable`。**Windows-only 代码改完必过 §8.5 靶场**。

---

## 8. 平台 / 环境陷阱(踩过的坑,务必记住)

### 8.1 WebView2 ≠ WKWebView(头号陷阱)
- 目标 = Windows(WebView2),开发在 Mac(WKWebView)。**WKWebView 更宽容,会掩盖一整类只在 Windows 暴露的 bug。** 已实锤:
  - B 站视频只有声音(黑屏)——WebView2 解不了 HEVC/AV1;修 = `resolver.rs` 强制 `vcodec^=avc`。
  - **本地电影有画面没声音**(上条的镜像):BD 双语压制片音轨常是 AC3/E-AC3/DTS/TrueHD,WebView2 解不了(Mac 走系统解码器有声故漏网);根因 = 网络路径早强制 avc+m4a 兜底,本地 `/f/` 直传漏了这道 → 「探测 → 只转处理不了的那部分」(§7.1 本地播放链)。✅ Win 真机验过(AC3/DTS 有声、普通 mp4 秒开、HEVC/mkv 可放)。→ 详见 docs/notes/media-playback.md「本地电影有画面没声音(AC3/DTS 音轨,WebView2 解不了)」
  - 全屏闪烁 / 退出穿帮——HTML5 `requestFullscreen` 与 DWM 打架 + 透明窗放大穿帮;修 = 改走原生窗口全屏 `win.setFullscreen`。
  - 滚动条占布局宽度跳动(§6.7 `scrollbar-gutter`)、唤醒标定**性能**坑(`KeywordSpotter::create` Mac 264ms 不暴露、Windows 卡分钟级)。
  - **原生库报错在 Windows 正式版会整个蒸发**:sherpa / onnxruntime / espeak `fprintf(stderr)`,GUI 子系统没控制台 → 壳层 `nativelog.rs` boot 把 fd 2 重定向到 `logs/native.log`(仅正式版)。**⚠️ 对 sherpa 无效**:其预编译库是 `win-x64-static-MT`(静态 CRT 私有 fd 表),主进程 dup2 修不到 → **接住 /MT stderr 的唯一路 = 子进程**:克隆加载失败自动拉 `exe --probe-zipvoice <dir>` 探针(stderr 管道回收进 larkwing.log,每会话一次);人肉 = `larkwing.exe 2> err.txt`。native.log 仍捕 Rust panic / 动态 CRT。`TTS_ZIPVOICE.ready` 补 espeak 内容探针(目录存在 ≠ 内容齐)。→ 详见 docs/notes/desktop-shell.md「原生库报错在 Windows 正式版蒸发(nativelog + /MT 静态 CRT + 子进程探针)」
  - **Rust `canonicalize` 的 `\\?\` verbatim 路径喂 C 库会炸**:verbatim 关闭 Win32 路径归一化,`/` 不再当 `\`,C 库内部用 `/` 拼子路径即「does not exist」(数据目录搬家后指针路径带毒,Rust 自己的 fs 毫无感觉极难发现)。修 = `datadir::simplify()`(`\\?\C:\…`→`C:\…`、UNC 同理),`norm()` 产出即净化 + `resolve()` 读指针洗一遍 + `zipvoice_config` 兜底。**规则:交给 C 库 / 子进程的路径永远别带 `\\?\`;canonicalize 产出先过 simplify。**。→ 详见 docs/notes/desktop-shell.md「Rust canonicalize 的 \\?\ verbatim 路径喂 C 库会炸」
  - **藏托盘的主窗仍 60fps 空烧 CPU**:透明窗让 Chromium 遮挡检测失效,隐藏后 RAF 不被节流 → `usePageVisible`(`visibilitychange` + 壳层 `lw:win-visible` 双触发)+ `useRafLoop`(不可见即 cancel);**新代码起 RAF 循环一律用 `useRafLoop`,别再裸 `requestAnimationFrame` 自调度。** ✅ Win 真机验过藏托盘 CPU≈0。→ 详见 docs/notes/desktop-shell.md「藏托盘的主窗仍 60fps 空烧 CPU(useRafLoop)」
  - **开机自启冷启后动画冻死**(上条的反向孪生坑):`--autostart` 先 `hide()`,透明窗 `document.hidden` 误报可见 → 隐藏期排的 rAF 被丢弃却留下死 id,`start()` 的 `!raf` 守卫挡住永不重排。三轮修:① `useRafLoop` 的 `watch(visible)` 先 `stop()` 清死 id 再 `start()`;② `usePageVisible` 加 `settled` 守卫 + 壳层自启 `hide()` 后主动 `emit("lw:win-visible", false)`;③ **OS 真相直采**(window `focus`/`pointerdown` 只纠 false 值)+ **rAF 看门狗**(1.5s 自检「攥着 id 却 >1.2s 没回调」= 死 id 清掉重排,重试直到真跑起来;raf=0 不碰)+ JS 侧 show/hide 三路补发 `lw:win-visible`(`hideToTray` / `summonWindow` / `bringToFront`,**它们不经壳层 `show_window`/`CloseRequested`**)。预览无头环境不触发 rAF 验不了;Windows 自启真机分别经悬浮窗与托盘打开主窗验。→ 详见 docs/notes/desktop-shell.md「开机自启冷启后动画冻死(三修:清死 id / settled / 看门狗 / 三路补发信号)」
  - **MSE/shaka 自适应播放**:B 站 DASH ✅ Win 真机验通(yt-dlp 解析必须 `--proxy ""` 强制直连);本地 fMP4-HLS 三层坑(mpegts 段 transmux 黑屏 3015/3016 → fMP4;段 tfdt 恒 0 → 累计起点;**真黑屏主因 = 多声道 AAC 被 MSE 拒 append 整个 init**)→ 段一律转码视频 + 下混立体声。**复现手法 = 预览浏览器(Chromium,与 WebView2 同 MSE 内核)跑裸 MSE**,把 relay 产出的 init/段喂 `SourceBuffer.appendBuffer`,Mac 上重现 Windows-only append 失败;shaka 错误日志打全 code/category/data(生产版也带)。→ 详见 docs/notes/media-playback.md「MSE/shaka 自适应播放三层坑 + 预览浏览器裸 MSE 复现手法」
  - **数据「搬家」(datadir)只能 Windows 真机验**(2026-06-18):跨盘 C:→D: 搬(同卷 rename 退化成拷+删的边界)、重启绑新根、`VACUUM INTO` 出的库可开且数据全、原生目录选择器、`explorer` 打开数据文件夹、剩余空间预检(fs2 `available_space`)、拔盘后启动弹恢复弹窗、卸载重装后指针仍在 → 数据找得回。Mac 开发能跑通拷贝/VACUUM/重启逻辑,但盘符/可移动盘/资源管理器行为测不出。详见 PLAN「数据搬家」watch-items。
  - **本地「音视频分离自适应」在 Mac 壳绿屏卡死(五案,Mac-only)**:症状 = 段全 append 成功、无错误回调、播放头停 0、满屏绿,`play()` 被拒 `paused` 弹回 true → 12s 看门狗(带 `!paused`)永不触发。**定案**:① 首视频样本 PTS≈0.1s + WebKit「播放头必须落在缓冲区间内才推进 readyState」的严格度 → localAdaptive 加最小 **gap-jump**;② 切轨重建后 `applyResume` 与初始爬灌竞态 + **WKWebView 不接入播放开始后才补进的音频** → `playAdaptive` 收 `startAt` 段指针直指续播位绝不从 0 爬灌、两 pump 记 entryGen 变了重跑;③ **起播闸**:两轨在起播位都有货才 `el.play()`(4s 超时兜底);④ 启动 seek 挪到挂 onSeeking 之前;⑤ **真凶 = 自家 `applyAudioTrackToEl`**(为直传混播写的起播收敛在 MSE 路按文件轨号 disable 了唯一音轨,WebKit 对禁用轨丢弃全部样本)→ 加直传守卫(有 manifest_url 或 /m/ 一律 return);「WKWebView MSE 不可用」结论**彻底撤销**。**教训**:疑难杂症先埋诊断(生命周期日志 + 5s 缓冲快照)再猜;手写 MSE 消费者要自带 gap-jump 与起播位直灌;WebKit 三条严格度记牢(播放头须在缓冲内 / 播放中不重路由 audioTracks / 不接入迟到的音频轨);段内时间戳契约要服务端强制(`probe::zero_tfdt`);**平台 API 的「修复」必须写明适用路并加守卫,疑「引擎异常」前先 grep 自己对该元素 / 该 API 的全部触点**。同批:mac **音频**白名单回滚(直传换来多轨混播 / 切轨只能重建 / 响度不受控),白名单只剩 HEVC 视频;看门狗补 `readyState<2` 盲区;mkv 在 mac 的 MSE 链记档为已知边界。→ 详见 docs/notes/media-playback.md「Mac 壳自适应 MSE 绿屏卡死五案(真凶 = 自家 applyAudioTrackToEl)」
  - **B 站扫码登录 Mac 上连坏两层**(Mac-only,`#[cfg(target_os = "macos")]`):① 「浏览器版本过低」拒开 = WKWebView 默认 UA 缺 `Version/x Safari/x`,登录页前端 JS 判旧 → `media_login` 开窗补真 Safari 冻结形 UA;② 登录成功读不到 SESSDATA = wry mac `cookies_for_url` 按域**精确相等**过滤,`.bilibili.com` 对 `www.bilibili.com` 永不相等 → mac 改 `win.cookies()` 全量 + `cookie_domain_matches` 自做域后缀匹配;Windows 维持 `cookies_for_url`(原生 API 正确)。排查抓手:日志见「登录态已入库」才算数。→ 详见 docs/notes/media-playback.md「B 站扫码登录 Mac 上连坏两层(UA 版本段 + wry cookie 域精确匹配)」
- **规则**:改**影音播放 / 窗口全屏 / 编解码 / WebView 渲染 / 媒体流 / 唤醒标定性能 / 动画循环 / 数据目录搬家**类代码,默认假设 WebView2 / Windows 文件系统更受限;**Mac 跑通 ≠ 验证通过**,这类**必须出 Windows 包真机验**;设计时主动选 WebView2 也支持的路径(avc、原生窗口全屏)。

### 8.2 唤醒「叫不答应」根因与决策(2026-06-16 拍板:保持 KWS)
- 已定案:**默认唤醒阈值 0.45 太严**(口语 / 偏小的「旺财」KWS 分数 ~0.3,robot 用 0.20 → 8/10,0.45 只接咬字清亮的 → 3/10)。已修(降阈 + 修灵敏度滑块落库)。
- **默认灵敏度拉满(2026-06-18,用户拍板)**:默认 `voice.wake.sensitivity` 从 50(→阈值 0.2)改成 **100(最灵敏 →阈值 0.1,映射 clamp 下限)**。理由 = KWS 召回本就偏弱,「**保障能唤醒再说**」——默认偏召回、先保证叫得应,误触嫌吵的人自己往左调 / 录标定,胜过喊半天不答应。**改默认 = 改三处**(同 §6.8 接线纪律):Rust `voice/mod.rs::wake_threshold` 的 `unwrap_or(100.0)` · 前端 `useSettings` DEFAULTS · `SettingsView` 滑块 fallback。标定流程不受影响(用户主动录制时仍按 calib「宁松勿严」择档,会覆盖此默认)。
- **再定案**:阈值这条路到头,病灶 = **通用 3.3M KWS 模型对真实「旺财」召回太弱**(阈拉到 0.12 仍 0 命中,再压只招误触);同采集链上 ASR(听写)又准又能小声 → 采集 / 麦 / 阈值都没问题,就是 KWS 弱。两段式(KWS→ASR 复核)作废。
- **已拍板(2026-06-16,用户准则):保持 KWS,不做 VAD→ASR。** 召回靠**降阈 + 唤醒标定(录几遍定灵敏度 + 拼写覆盖)**兜,接受 KWS 召回上限,换**秒应零延迟**;放弃的 `VAD→ASR 当探测器`(VAD→SenseVoice 转写→命中即唤醒)代价 = +0.5–1s 延迟 + 每段说话跑一次 ASR,用户不取。**记档:此方案已设计完整,留作后备**——若 Windows 真机召回仍不可接受再启,届时走「KWS 秒应快车道 + VAD→ASR 兜底」混合,不动手前别当 TODO。
- `sherpa KWS 不给分数`(`KeywordResult` 无 score)→ 标定只能**二值阈值扫描**,这是阈值类工作的硬前提。
- **标定宁松勿严**(真机教训):噪声 / 分不开时**绝不取最严档**(旧逻辑取最严 = 灵敏度 0 ≈ 叫不应,正好打脸「叫不应」诉求)→ 偏召回取折中档 + 警告自调;标定**全局、非按人**(刻意绕开多用户 / 声纹线)。
- **★唤醒词 = 名字派生,无独立设置**(用户拍板「起什么名字就怎么唤醒」):单源 = 「叫我什么」`ui.pet_name`,`voice/mod.rs::wake_keywords` 每次现派生(`wake::derive_wake_word`:中文原样、数字转读法、**字母缩写按中国人的字母读音**〔BT→逼踢,`wake::LETTER_READINGS`〕);设置页「唤醒词」输入框已删,改名 = 换喊法。**默认名 BT = 派生的「逼踢」一个词,无任何特殊处理**(单源 `wake::DEFAULT_WAKE_WORDS`,防回潮断言钉着;「小七」「七二七四」退役)。英文单词式名字派生不出 → 回落默认词 + `VoiceStatus.wake_fallback` 如实提示;单音节名能用但预期飘。**确认层转写归一与派生共用同一张读音表**(`to_syllables`;否则 ASR 吐「BT」原文时真呼叫被当幻听)。→ 详见 docs/notes/voice.md「唤醒词 = 名字派生,无独立设置(BT → 逼踢)」
- **★唤醒确认层 = 命中三段式**(精度方向,治高频名「天天」误唤醒;与作废的「召回方向 KWS→ASR 复核」不是一回事):KWS 拉满召回降级为**候选探测器**,命中后**立刻出声应答、录音不断**,后台续录到断句(`wake.rs` 常量:预滚环 2s / 无续句窗 0.6s / 断句 0.8s / 上限 12s)→ ASR 整句 → **拼音三段式**(`triage_transcript` 纯函数,无调拼音对齐,取末次出现):。→ 详见 docs/notes/voice.md「唤醒确认层 = 命中三段式 + 旁听回合 + 命中即出声应答(自听回声剥离)」
  - **孤立呼名**(尾随 ≤1 音节,语气词豁免)→ 经典唤醒;
  - **呼名+续句** → 整句交模型仲裁 = **〔旁听〕临时回合**(`send_overheard`,复用 `__IGNORE__` 哨兵):user 行悬置不落库,模型回 `__IGNORE__`/空 = **整轮蒸发**(不进 UI / 历史 / 记忆 / event 行),开口或调真工具 = 连悬置行一起转正;LAWS「旁听」:不是叫我只回 __IGNORE__,**拿不准轻声应一声**;
  - **转写无关键词** → KWS 幻听,静默拒(转写进日志)。
  - 铁则:**确认层 fail-open**(ASR 挂了当经典唤醒);**规则层绝不无声吞掉可能的真呼叫**(只有「转写里根本没有关键词」才静默拒)。旁听真输入优先(会话在飞放弃仲裁;打字 cancel 挤掉未转正回合)。eval 守卫 `overheard-dismiss` / `overheard-execute`。
  - **★命中即出声应答 + 录音不断**(用户拍板接受代价):命中那一刻点灯 + duck + `prompts.play_async(Ack)`(场景数据 wake_acks),全程不 drain 麦;**自听回声修**:多音节应答词被麦收进转写 → 「逼踢我在」超尾随豁免被推去旁听 → 蒸发 = 「答了一声却像没唤醒」;修 = `triage_transcript` 加 `played_ack`(`PromptPlay` 带回文本),**只剥本次这条、只剥紧跟呼名的前缀**(词表整体剥会误伤真实续句同词开头;半截泄漏不剥落仲裁兜);旁听上抛文本 `strip_ack_once` best-effort 抠除。**「browser AEC 消 cpal 直出的应答」分平台不可赌**(Win AEC3 只消 WebView 自播)。「叮」`play_chirp_async` 降级为兜底(应答音银行没就绪才响,喊了名字绝不能没动静)。
  - **配套修:唤醒在听时「停 / 定稿」键**:唤醒录音跑自己的循环线程不占听写会话槽 → `Inner.wake_ctl`(`arm_wake_ctl` 武装、`collect_utterance` 读;`listen_stop` 同写会话槽与 wake_ctl),✕ = 安静回待唤醒、定稿 = 发已听到的,前端零改。

### 8.3 Tauri 插件「权限 ≠ 作用域」陷阱(opener,踩过两轮)
- 开外链走 `backend.openExternal`(`plugin:opener`)——`window.open` 在 WebView2 是 no-op,别用。
- **权限和作用域是两件事**:`opener:allow-open-url` 只启用命令、**自带零作用域**,**必须再加 `opener:allow-default-urls`**,否则 `is_url_allowed` 拒一切 → `ForbiddenUrl`。scope 类插件(opener / fs / http …)权限 + 作用域**两样都要给**。
- **capability 编译期烤进二进制**:改 `capabilities/*.json` 后**必重编 Rust**(Vite HMR 不生效);别让 `openExternal` 静默吞错。

### 8.4 Tauri 同步命令无 tokio 上下文——裸 `tokio::spawn` 必崩(2026-08-28 真机实锤)
- **同步 `#[tauri::command]` 在 IPC/UI 线程内联执行、没有 tokio 上下文**(tauri 2.11 源码核实:只有 async 命令才被 `async_runtime::spawn` 派发)→ core 里开头裸 `tokio::spawn` 的方法(`retry_model` / `retry_component`)被同步命令直调 = panic 穿 FFI abort = 100% 崩进程(下载失败点「重试」必崩;Mac dev 同样崩但平时测不到)。→ 详见 docs/notes/desktop-shell.md「同步 tauri 命令没有 tokio 上下文(裸 tokio::spawn 必崩)」
- **规则:壳层命令要调 core 里会裸 `tokio::spawn` 的方法,命令必须 `async fn`(或显式 `tauri::async_runtime::spawn` 承接,`media_retry` 先例)**;core 侧这类方法 doc 注明「须在 tokio 上下文内调用」。已修(2026-08-28):`retry_download` / `retry_voice_model` 改 async。新写同步命令前过一遍:它调的 core 方法有没有内部 spawn。

### 8.5 `cfg(windows)` 代码别拿 CI 当编译器——mac 上交叉 typecheck 靶场(2026-09-01 实锤立规)
- **病**:`#[cfg(windows)]` 块在 mac 上只 parse 不 typecheck → Windows-only 代码的类型错(API 签名/生命周期)全潜伏到 CI 的 Windows job 才爆,一来一回 20 分钟,且编译器在前几个错就停 = 一次只见冰山一角。实锤:v0.2.34 首发 CI 挂在 print.rs 三个错(pdfium `as_image()` 返回 Result 要解包、windows 0.61 `GetDeviceCaps` 参数 Option 化),本地靶场随后又抓到 CI 还没跑到的**第四个**(PdfBitmap 借 page 的链式 `?` 临时值 E0597)。
- **整 crate 交叉 check 是死路**:`cargo check --target x86_64-pc-windows-msvc` 会被依赖树里的原生 C(ring/sherpa)build script 挡死(mac 的 cc 没有 Windows 头文件)。
- **解 = scratch 靶场**:把新写的 win 模块抽进一个零 C 依赖的临时 crate(只带 windows/winreg/image 等**纯 Rust 绑定**依赖,crate 内引用 stub 成同签名),`rustup target add x86_64-pc-windows-msvc`(纯 std rlib,不需要 MSVC)后 `cargo check --target x86_64-pc-windows-msvc` = 完整 typecheck;**跑完注入一个假类型错验证靶场真在查**(金丝雀),别被缓存假绿骗。配套:API 签名拿不准就 `cargo fetch --target x86_64-pc-windows-msvc` 把 windows crate 源码拉下来直接读(`~/.cargo/registry/src/…/windows-0.61.3/src/Windows/Win32/...`),0.61 世代大量 handle 参数 Option 化、常量是 newtype(`BI_RGB.0`),别凭记忆写。
- **规则:新写成块的 `cfg(windows)` 代码(工具/模块级),合入前过一遍靶场 check;真机/CI 只留给行为验证,别当第一道编译器。**

### 8.6 悬浮件别挂在滚动容器里——transform 定位的盒子算进可滚动范围(2026-09-04 真机实锤)
- **病**:桌宠 `.roamer` 曾绝对定位在滚动容器 `.stream` 里、每帧 `+scrollTop` 补偿;**CSS 规定 transform 后的盒子参与祖先滚动容器的 scrollable overflow**,且 `.body` 姿态容器并非零尺寸 → 桌宠站底边时探出视口 → 回合在飞每条增量 `scrollTop = scrollHeight` 贴底 → 桌宠再探出 = 正反馈环(预览 1:1 复现;肉眼 / 算 img 高度会误判「不是桌宠」)。→ 详见 docs/notes/desktop-shell.md「聊天流无消息却一直下滚 = 桌宠挂滚动容器里(病)」
- **修**:MainLayout 给 `.stream` 外包一层同框 `.stream-wrap`(flex 列、`min-height:0`、`overflow:hidden` 沿用原先被 .stream 裁切的观感),桌宠挂在 wrap 上按视口坐标直写、不再加 scrollTop;`bounds` 仍传 `.stream`(只量尺寸/算指针坐标,两者同框同原点,预览核对四边偏移全 0)。修后同一实验 scrollTop/scrollHeight 纹丝不动。
- **规则**:任何「悬在滚动区上方」的浮层(桌宠 / 将来的角标、气泡、拖拽幽灵)一律放在滚动容器**外**的同框层,绝不用 `+scrollTop` 补偿;判据 = 它的任何盒子(含看不见的容器盒、旋转/缩放后的包围盒)都不该能改变 `scrollHeight`。**「贴底 = scrollTop 追 scrollHeight」与「任何随 scrollTop 移动且能改 scrollHeight 的东西」天然成环**,新写自动滚动逻辑时对照。
- **排查手法(预览面板隐藏态可用)**:面板隐藏时 rAF 冻结、timer 节流,`javascript_tool` 里等毫秒必超时 → 用 MessageChannel 泵一个 16ms 门控的假 rAF 驱动 `useRafLoop`,等待全按帧数;帧循环若攥着死 id 不会自己醒,先派 `visibilitychange`(→false)再派 `pointerdown`(→true)逼「先清死 id 再重排」;组件状态经 vite dev 的 `el.__vueParentComponent` 往上找 `setupState.chat`,直接 push `trace.items` 即可模拟思考增量触发 deep watcher;结束 `finally` 还原 rAF。

---

## 9. 边界 / 暂不做 🔒(别擅自做)

- **离线不做**(非目标,不为它做本地 LLM 兜底)。
- **不复刻** robot 复杂度:插件框架 / RAG / MCP / 风控 / 代码智能 / shell / deploy / test;「第三方可加载插件」是平台级目标,陪伴产品不必(真需要走 WASM)。
- **远场麦阵 / 流式 ASR / 系统级压低其他 app 音量 不做**(§7.5)。**AEC 已从本清单移出**(2026-07-06 解锁为「采集端浏览器 AEC」路线,见 §7.5;**自研/引入软件 AEC 仍不做**)。
- **ASR 专名拼音纠错词表已砍**(robot 实测很少纠对)——听错兜底 = ①识别文本可见可改 ②两段式有声追问 ③模型上下文自纠。任何人再提此方案,**先问真实纠对率**。
- **PDF 文字层已做、扫描件 OCR 仍不做**:`attach.rs::extract_pdf`(`pdf-extract`,畸形 PDF `catch_unwind` 兜住)+ txt / md / 源码 / .docx / .pptx / .xlsx(→CSV);**仍不做**:扫描件(无文字层 → None;真要做走「栅格化转图 + 视觉」)· 老二进制 .doc/.ppt/.xls。缘由:DeepSeek 视觉只收图不吃原生 PDF。→ 详见 docs/notes/agent-runtime.md「PDF 文字层已做、扫描件 OCR 仍不做」
- **媒体附件:图当轮、文档文字进 history**(用户拍板):**图片当轮注入、不落库**(vision 对旧图反复计费;bytes 另落 `<media>/attachments/<内容hash>.<ext>` 供 UI 回看,`AttachmentRef.file` 记相对名;LLM 永不重见旧图);历史带图行加装配标记「〔附了N张图〕」(现加不落库,让「把它下载 / 认一下」路由到吃「最近一批图」的工具);**文档「文字」并进落库的 user 消息内容 → 进 history**(多轮追问文档还在,且落可缓存前缀);落点 `engine/mod.rs::send_message` + inject 路径 `turn.rs::apply_injection` 同步(`llm_content` 含文档、UI 事件用 `display` 原文)。**收件区**:文档 / 文件 bytes 落 `<media>/inbox/`(`save_inbox_blob` = sanitize + dedupe 永不覆盖,随数据根搬家),`doc_text` 带运行时绝对路径交模型(扫描件也照落,措辞明说「读不出文字但文件在本地」);**图片也落收件区原名件**,「〔图片:名 已存到本地:绝对路径〕」一行进 history(与〔附了N张图〕并存;代价 = 图双份存储)。⚠️ 绝对路径随内容进 history → 搬家后老对话里的路径失效(只影响回看)。→ 详见 docs/notes/agent-runtime.md「媒体附件:图当轮、文档文字进 history、收件区」
- **上下文处理 = 单一字数预算(model-aware)+ 整块锚定,无条数窗口**:`engine/context.rs::windowed_start`(字数预算裁 + `WINDOW_CHUNK` 绝对整块锚定保前缀缓存 + 边界吸附防拆 tool 配对);预算 `tail_budget_chars(catalog::ctx_window_of(model), billing)`:未知窗口回落 `DEFAULT_TAIL_BUDGET_CHARS=48_000`,大窗口在 [默认, `MAX_TAIL_BUDGET_CHARS=300_000`] 间放大,小窗口缩到默认以下(`= min(MAX, 窗口token/TAIL_RESERVE_DEN)`,CJK 1 token/字上界);**判据 = 预算只缩或在 [默认,MAX] 间放大、绝不无界增长;常态起点 = 0 缓存零损伤**;caller 按 `HISTORY_PAGE_MAX=800` 做 I/O 分页传 `page_base`。**计价感知**:`catalog::BillingMode`(Cached 默认 / Uncached / PerCall),无缓存 → 预算封到 DEFAULT(每轮全价重发尾巴贵);只影响压缩不改记账。**模型「高级设置」override**:按 model id 纠正档位 / 上下文(K)/ 输入价 / 输出价 / 计价方式(空 = 目录猜测,纠错非配置);catalog **进程级 overlay**(`static OVERRIDES`,`reload_providers` 顶 `set_overrides`)→ `tier_of` / `ctx_window_of` / `est_cost_usd` / `billing_of` 先查覆盖再回落目录,消费点零改;明文 settings `llm.model_overrides`(非秘密,不进白名单),命令 `model_meta` / `set_model_override`。阈值单源 `context.rs` 顶部。✅ Win 真量验过。→ 详见 docs/notes/agent-runtime.md「上下文处理 = 单一字数预算 + 整块锚定;计价感知;模型高级设置 override」
- **声纹归人(多用户第二步)已落地**:core 声纹链路本就全 wired(voiceprints 域 / CAM++ `identify` 接听写 + 唤醒 / `speaker_id→speaker_user→` 记忆归人),补的是 UI + 三个产品决策(用户拍板):① 家人卡「让它认识声音」enroll(**录 3 段取平均** `ENROLL_SAMPLES`,进度经 `VoiceEvent::Enroll` 不静默失败)+「忘掉声音」(只删声纹不删人);② **识别置信「宁可不认、绝不错认」= 余弦 ≥0.5 且比第二名高 ≥0.10**(`speaker::MARGIN` 单源,两人咬太近判认不出回落会话用户;与唤醒「宁松勿严」相反);③ **主人查看家人记忆** = 回忆页「看谁的」下拉(**切视图 ≠ 切身份**;home-scope 需知本就全家共享)。首次注册后**唤醒循环要 off→on 重启**才认得出(非 bug)。→ 详见 docs/notes/voice.md「声纹归人(多用户第二步)」
- **「已备未启用」的休眠能力——看到没接 UI 的 core 代码,不要当「未完成的活」去补完**:
  - `Tool::risk()` 元数据 = 仍不消费的预留(动作确认闸〔§7.8,2026-07-15〕已建但走动作级 + `ToolCtx.confirm` 通用件,不吃 risk;工具级强制确认 §7.2 已否决)。足迹页「审批分区预留」已兑现成「确认过的操作」分组。

---

## 10. 维护本文件(元规则)· 规则变了必须更新

> **这是本文件存在的意义:让规则有一个会被维护的家。规则漂移而文件不更新,等于没有规范。**

### 何时**必须**回来改 AGENT.md
1. **改动任一 🔒 锁定项 或「用户准则」级规则**(§4 全部 + §1 TL;DR + §3 铁律 + §5 架构原则 + §9 边界)→ 这是**宪法级动作**:**先与用户确认,再改本文件**,并在对应条目记下日期与缘由。
2. **新增 / 推翻一条跨模块开发约定**(§6 / §7)、**踩到并定位一类新平台陷阱**(§8)、**解除一个「后置 / 暂不做」边界**(§9)→ 同步更新对应小节(过程级约定可直接改,不必每条都问;撞 🔒 才停)。
3. **一个「已备未启用」能力被启用**、或一条「未决方案」拍板落地 → 把它从 §9 / 现状里移出,更新规则。

### 改哪个文件(别改错地方)
- **规则 / 约定 / 边界变了** → 改 **AGENT.md**(本文件)。
- **一条规则的来历 / 破案过程 / 落地当时的状态** → 追加进 **docs/notes/<主题>.md**(逐字归档、只追加不改写);AGENT.md 里那条只留规则句 + 落点 + 一句缘由 + 指针。
- **整编纪要** → **docs/AGENT-LOG.md** 顶部加一段;AGENT.md 页脚只留最近一次的一行。
- **模块设计变了 / 进度推进 / watch-item 验收勾项** → 改 **PLAN.md**(它管设计与执行状态;AGENT.md 只引指针)。
- **跨会话的个人化观察 / 协作风格 / 踩坑细节** → Claude 记忆(自动维护;它是 point-in-time 观察,**引用前核对现状**,过时就更正)。
- **CLAUDE.md** 只指向本文件,基本不动。

### 现状 / 验收纪律
- **真机验收单(watch-items)在 PLAN 各 § 末**:一类东西**只能 Windows 真机 / 真网 / 真钥匙 / 真麦验**(媒体渲染、窗口全屏、代理「直连失败→代理兜底」、和风 JWT 全链、语音功效与唤醒召回、真实视觉应答、并发注入交错气泡…)。**Mac / 浏览器预览跑通的不要当「已验证」声称**,落在 watch-item 里据实标。
- 报告口径:测试失败就说失败贴输出;跳过的说跳过;**只有真验过才说「done」**。

### 体量纪律(2026-09-07 分家立规)
- **AGENT.md 只收规则句**:做什么 / 不做什么 / 在哪兑现(文件、常量名)/ 一句缘由 / 指针。**过程叙事、真机状态、测试数字(「Mac 全绿 N / clippy 零警 / i18n 镜像 N=N」)、验收欠账一律不进本文件**——叙事进 `docs/notes/`,状态与验收单进 PLAN.md,测试数字进 CHANGELOG / git log。
- **判据**:一条新增超过约 300 字,或含「实锤 / 破案 / 当日落地 / 真机 watch」这类叙事,就拆成「规则句留这、原文进 notes」。缘由:三个月从零长到 24 万字(约 13 万 token,每个会话开头全量进上下文),规则书变成了要 grep 的档案库。
- **页脚只留一行**(最近一次整编),整编链在 `docs/AGENT-LOG.md`。

### 文档分工再申明
- 宪法管**边界**,计划管**设计与执行状态**,本文件管**规则总纲并指路**。三者不矛盾时以本文件的规则表述为准;若发现 PLAN / 记忆与本文件冲突,**以本文件为规则真相源**,并回头修正不一致的那处。
- 来历归 `docs/notes/`(逐字归档、只追加),整编纪要归 `docs/AGENT-LOG.md`;两者与本文件冲突时,以本文件为准,并回头修正 notes。

---

*最后整编:2026-09-07·三批(**AGENT.md 分家**:叙事 / 状态 / 整编链搬出,规则句留守;历次整编纪要见 `docs/AGENT-LOG.md`,每条规则的来历见 `docs/notes/`)。*
