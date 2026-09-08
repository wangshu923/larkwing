# 数据目录 / 备份恢复 / 搜索 — 决策记录与破案叙事

> 本文件是 2026-09-07 AGENT.md「分家」时从规则总纲搬出的段落,**逐字照搬、未改写**;管「为什么这么定 / 怎么发现的 / 当时的状态与验收欠账」。
> **规则以 AGENT.md 为准**(每段在 AGENT.md 里留有规则句 + 指向这里的指针)。本文件只追加不改写:新决策照此格式加一节(标题 + 来源 + 原文)。查规则出处时 grep 要连同 `docs/notes/` 一起扫。

## 数据根搬家 datadir / 一键备份 / 从备份恢复 / 聊天搜索

> 来源:分家前 AGENT.md「6.2 store」L177–L180。

- **数据根可「搬家」(`datadir`,2026-06-18 用户拍板):别再假设 `app_data_dir` 就是数据根。** 用户可把整个数据目录(DB + `media/` + `voice/` + 模型缓存 + 日志)整体搬到别的盘(Windows 多盘:不想全堆 C 盘)。机制 = **锚点 + 指针文件**:锚点 = OS 默认 `app_data_dir`(永远找得到、住指针、永不参与搬家),锚点放 `location.json` 指针说真实根在哪;`larkwing-core/src/datadir.rs` 的 `resolve(anchor)` 在 boot **最先**跑(lib.rs 装配头),返回生效根,**之后 store/日志/media/voice 全部 `root.join(...)` 派生**(原有单一出口不变,这是搬家能干净的前提)。三态:无指针/空 = 用锚点;指向别处且在 = 用它;指向别处但目录不在(盘没插/被删)= 回落锚点 + `data_missing`,前端弹恢复弹窗(**绝不静默在默认位置重建空数据**,§3.5)。搬家 = 拷可重建子树 → DB 走 `VACUUM INTO` 出一致快照(放最后)→ staging 同卷原子改名 → **翻指针 = 提交点**(翻前老数据始终权威)→ 立即 `app.restart()`。**判据:写任何"数据放哪"的路径,都从生效根派生,绝不直接 `app_data_dir()`;DB 里也绝不存绝对路径**(克隆音色 wav / TTS 缓存只存相对文件名 → 整棵子树挪走路径不断,这条已是约定、搬家依赖它)。真机验见 §8.1 + PLAN「数据搬家」watch-items。
  - **一键备份(v0.1.6)= 搬家的轻量姊妹**:`datadir::backup_to(data_root, dest_dir)` 在用户所选目录导出 `larkwing-backup-<时间戳>.zip`(`VACUUM INTO` 一致快照当 `larkwing.db` + `voice/clones/` 克隆音色 wav;可重建的模型 / 缓存 / 媒体 / 日志不收)。**纯导出拷贝、不翻指针不重启**(区别于搬家),命令 `backup_data`,`zip` crate(v2 deflate)已在依赖。延续「备份 = 拷文件」(§4.3)+「DB 只存相对名,整棵子树挪走自洽」。完整流程真机验(Windows zip / 大克隆音色)随搬家一并看。
  - **从备份恢复(2026-07-13)= 备份的另一半**:设置·系统「恢复…」选 zip → `datadir::restore_precheck`(zip 结构 + SQLite 魔数 + **迁移版本前向检查**——备份库里有本版不认识的迁移 id〔`store::migration_ids()`〕= 来自更新版本,拒并指路「先升级」;老备份新程序 = 合法,boot 迁移自动升)→ 内联确认(包概要:库 MB + 克隆音色段数)→ `stage_restore` 解到 `<root>/restore-pending/`(先 `.tmp` 后原子改名 = 落定标记;`enclosed_name` 防 zip-slip;只认 DB 与 `voice/clones/`,未知条目跳过 = 向前兼容)→ 自动重启。**真正落位在下次 boot 开库前**(运行中不能覆盖已打开的 DB):lib.rs 在 `Store::open` 之前调 `apply_pending_restore`——现库**连 `-wal/-shm`** 挪成 `larkwing.db.pre-restore-<时间戳>` 保险副本(wal 可能有已提交未 checkpoint 的数据必须跟着走;后缀配对保副本本身可打开)→ 备份库就位 → 克隆音色**合并**(同名以备份为准 = 恢复的 DB 指向那份;现有多出来的留着,孤儿无害);任一步失败逆序回滚 = 老数据仍权威。成败经 `AppState.restore_outcome` → `data_location.restored` → App.vue boot toast,**绝不静默**(§3.5)。恢复管**内容**不管位置(不碰指针/数据根)。记档:Windows 正式版密钥在 keyring 不进备份(§6.3)→ 换机恢复后 LLM key 要重填(mac dev 密钥在 settings 随包走)。
  - **聊天搜索(v0.1.6)**:`ChatRepo::search_messages(user, query, limit)` 跨会话 `content LIKE` 子串(转义 `% _ \`)、**排除 `tool`/`event` 内部行**、按用户隔离;命中带会话标题 / 渠道 + 截断 snippet。**仍是 substring,不上检索核心**(记忆量小够用,守 §13.9 deferred —— 搜索没构成「第二个 RAG 消费者」,它要语义才触发)。前端最近列表顶部搜索框,输入即查(去抖),点结果跳会话。


## 聊天历史「加载更早」+ 搜索命中跳得过去(2026-09-08)

> 来源:2026-09-08 优化批的体检结论之一(当时列进「明确没做」,理由是「属新功能不在优化范围」)→ 同日用户看完清单后点名要修。规则句在 AGENT §6.2 / §6.7 / §4.11。

- **病(两个,同一个根)**:`load_conversation` 恒取 `recent_messages(conv_id, 200)`,而 `search_messages` 跨**全表** —— ① 老会话翻不到 200 条之前,历史等于看不见;② **点搜索命中,如果那条在 200 条之外,切过去根本看不到它**(`openHit` 只 `selectConversation`),用户以为搜索坏了。`conversation_stats` / `conversation_trace` 同样写死 200,所以就算把消息翻出来,老气泡也不会有读数与「想了想」。
- **解 = 一个游标,三个读命令共用**:`chat::MsgCursor { before, around }` 三形(都不给 = 最新一页 / `before` = 向上翻 / `around` = 以某条为中心前后各半页,`around` 优先),页大小 `chat::UI_PAGE = 200`(把三处写死的 200 收成一个常量),取料收口 `ChatRepo::page`。**三个读命令都吃同一个游标**,所以消息、读数、轨迹取到的**是同一段** —— 翻上去的老气泡照样能 hover 看读数、展开看轨迹。这是本方案唯一的设计要点:如果只给 `load_conversation` 加分页,翻出来的历史会是「没有想了想的哑气泡」,比翻不出来更糟。
- **搜索命中定位**:`SearchHit` 加 `message_id`(SQL 多取一列 `m.id`),`openHit` 改成 `jumpToMessage(convId, msgId)` → 载 `around` 那页 + `anchorId` 进入定位模式:那条气泡描一圈主题色(只动 `box-shadow`,**刻意不改布局** —— 改高度会搅乱刚算好的滚动位)、`scrollIntoView({block:'center'})` 滚过去、顶部出「回到最新」浮钮(挂在 `.stream-wrap` 而非 `.stream`,守 §8.6「悬浮件绝不挂进滚动容器」)。**没做向下翻页**:定位后想回尾部就点「回到最新」(= 重载最新一页),比再实现一套「向下翻」简单得多且语义清楚。
- **两个必须一起改的前端点**(漏一个功能就白做):
  - **滚动补偿**:上方长出内容后不补 `scrollTop`,视口就跳到别处。写成**赋绝对值** `旧 top + (新 scrollHeight − 旧 scrollHeight)` 而非 `+=` —— Chromium 的 scroll anchoring 有时会先替你补一部分,赋绝对值天然幂等。预览实测:插 40 条(长高 1228px)后 `scrollTop` 保持 714 不动,补偿后 1942 = 714+1228,正好停在原内容。
  - **贴底判据从「条数变了」改成「末条 id 变了」**:头部插一页也让条数变 → 按条数判会立刻 `scrollTop = scrollHeight` 把正在翻历史的用户拽回底部。这与 §8.6 那条(桌宠撑 scrollHeight 造成正反馈环)是同族的坑:**「贴底 = scrollTop 追 scrollHeight」和任何能改 scrollHeight 的东西天然成环**,新写自动滚动逻辑时对照。
- **触发方式选 `IntersectionObserver` 而非 scroll 事件**:不占滚动主线程;而且「插完一页仍没填满视口」时它会自然再触发一次(`loadEarlier` 自带防重入 `loadingEarlier` 与到头闸 `hasEarlier`,不会空转)。**「还有没有更早的」不问后端**:判据 = 上一页是否装满 `MSG_PAGE`(前端常量与 Rust `UI_PAGE` 双源,§4.11),省一次 IPC;代价是恰好整页倍数时会多试一次、拿到空数组才封闸,无害。
- **`prependMessages` 刻意不动 `listGen`**:那个代次闸是防「整表替换让在飞回合的 `wang` 引用变孤儿」;头部插入既没换掉数组本身、也没碰尾部元素,引用仍然有效,回合照写照上屏。按 id 去重(游标是严格 `id <`,正常不重叠;真重叠也宁可丢弃重复的,别出现两条同 id 气泡)。
- **测试**:`chat::tests::page_cursor_walks_history_without_gaps_or_overlap` 钉三形 + 四个边界(升序 / 严格 `id <` 不含游标那条 / 页间首尾相接 / around 前后各 half / 到头给空 / anchor 已不存在也不炸)。**一处自己踩的**:第一版「around 优先」断言写成比长度,而 `page` 的 around 用的是 `UI_PAGE/2` 而非我手传的 half,长度当然不同 → 改成「结果里含 anchor **之后**的消息」才真正区分了两条路。
