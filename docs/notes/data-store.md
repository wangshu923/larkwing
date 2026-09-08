# 数据目录 / 备份恢复 / 搜索 — 决策记录与破案叙事

> 本文件是 2026-09-07 AGENT.md「分家」时从规则总纲搬出的段落,**逐字照搬、未改写**;管「为什么这么定 / 怎么发现的 / 当时的状态与验收欠账」。
> **规则以 AGENT.md 为准**(每段在 AGENT.md 里留有规则句 + 指向这里的指针)。本文件只追加不改写:新决策照此格式加一节(标题 + 来源 + 原文)。查规则出处时 grep 要连同 `docs/notes/` 一起扫。

## 数据根搬家 datadir / 一键备份 / 从备份恢复 / 聊天搜索

> 来源:分家前 AGENT.md「6.2 store」L177–L180。

- **数据根可「搬家」(`datadir`,2026-06-18 用户拍板):别再假设 `app_data_dir` 就是数据根。** 用户可把整个数据目录(DB + `media/` + `voice/` + 模型缓存 + 日志)整体搬到别的盘(Windows 多盘:不想全堆 C 盘)。机制 = **锚点 + 指针文件**:锚点 = OS 默认 `app_data_dir`(永远找得到、住指针、永不参与搬家),锚点放 `location.json` 指针说真实根在哪;`larkwing-core/src/datadir.rs` 的 `resolve(anchor)` 在 boot **最先**跑(lib.rs 装配头),返回生效根,**之后 store/日志/media/voice 全部 `root.join(...)` 派生**(原有单一出口不变,这是搬家能干净的前提)。三态:无指针/空 = 用锚点;指向别处且在 = 用它;指向别处但目录不在(盘没插/被删)= 回落锚点 + `data_missing`,前端弹恢复弹窗(**绝不静默在默认位置重建空数据**,§3.5)。搬家 = 拷可重建子树 → DB 走 `VACUUM INTO` 出一致快照(放最后)→ staging 同卷原子改名 → **翻指针 = 提交点**(翻前老数据始终权威)→ 立即 `app.restart()`。**判据:写任何"数据放哪"的路径,都从生效根派生,绝不直接 `app_data_dir()`;DB 里也绝不存绝对路径**(克隆音色 wav / TTS 缓存只存相对文件名 → 整棵子树挪走路径不断,这条已是约定、搬家依赖它)。真机验见 §8.1 + PLAN「数据搬家」watch-items。
  - **一键备份(v0.1.6)= 搬家的轻量姊妹**:`datadir::backup_to(data_root, dest_dir)` 在用户所选目录导出 `larkwing-backup-<时间戳>.zip`(`VACUUM INTO` 一致快照当 `larkwing.db` + `voice/clones/` 克隆音色 wav;可重建的模型 / 缓存 / 媒体 / 日志不收)。**纯导出拷贝、不翻指针不重启**(区别于搬家),命令 `backup_data`,`zip` crate(v2 deflate)已在依赖。延续「备份 = 拷文件」(§4.3)+「DB 只存相对名,整棵子树挪走自洽」。完整流程真机验(Windows zip / 大克隆音色)随搬家一并看。
  - **从备份恢复(2026-07-13)= 备份的另一半**:设置·系统「恢复…」选 zip → `datadir::restore_precheck`(zip 结构 + SQLite 魔数 + **迁移版本前向检查**——备份库里有本版不认识的迁移 id〔`store::migration_ids()`〕= 来自更新版本,拒并指路「先升级」;老备份新程序 = 合法,boot 迁移自动升)→ 内联确认(包概要:库 MB + 克隆音色段数)→ `stage_restore` 解到 `<root>/restore-pending/`(先 `.tmp` 后原子改名 = 落定标记;`enclosed_name` 防 zip-slip;只认 DB 与 `voice/clones/`,未知条目跳过 = 向前兼容)→ 自动重启。**真正落位在下次 boot 开库前**(运行中不能覆盖已打开的 DB):lib.rs 在 `Store::open` 之前调 `apply_pending_restore`——现库**连 `-wal/-shm`** 挪成 `larkwing.db.pre-restore-<时间戳>` 保险副本(wal 可能有已提交未 checkpoint 的数据必须跟着走;后缀配对保副本本身可打开)→ 备份库就位 → 克隆音色**合并**(同名以备份为准 = 恢复的 DB 指向那份;现有多出来的留着,孤儿无害);任一步失败逆序回滚 = 老数据仍权威。成败经 `AppState.restore_outcome` → `data_location.restored` → App.vue boot toast,**绝不静默**(§3.5)。恢复管**内容**不管位置(不碰指针/数据根)。记档:Windows 正式版密钥在 keyring 不进备份(§6.3)→ 换机恢复后 LLM key 要重填(mac dev 密钥在 settings 随包走)。
  - **聊天搜索(v0.1.6)**:`ChatRepo::search_messages(user, query, limit)` 跨会话 `content LIKE` 子串(转义 `% _ \`)、**排除 `tool`/`event` 内部行**、按用户隔离;命中带会话标题 / 渠道 + 截断 snippet。**仍是 substring,不上检索核心**(记忆量小够用,守 §13.9 deferred —— 搜索没构成「第二个 RAG 消费者」,它要语义才触发)。前端最近列表顶部搜索框,输入即查(去抖),点结果跳会话。

