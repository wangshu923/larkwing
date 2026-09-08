# 工程保障(CI / 测试 / 单源归一 / 资源管护)— 决策记录与破案叙事

> 2026-09-08 起。AGENT.md 只留规则句(§10「CI」节、§4.11「四族归一」与资源管护三族);这里记「为什么这么定、当时是什么状况」。规则以 AGENT.md 为准。

## 毒锁:一个 panic 废掉一整块状态(2026-09-08)

> 起因 = 用户带着完整诊断来的:「毒锁是 Rust 标准库 Mutex 的一个机制。持锁的线程如果 panic 了,这把锁会被永久标记成「中毒」,之后每次 lock() 都返回错误。项目里约 130 处写的是 `.lock().unwrap()`,于是第一个 panic 之后,每个访问点都跟着 panic。要命的地方在于 tokio 任务 panic 不会杀掉进程,所以程序还活着,但那块状态彻底废掉:比如媒体运行时那个锁一中毒,播放、队列、封面、进度全部失效,直到用户重启。」问的是「这个东西改动大不大」。

### 先核premise,两处要修正

- **不是 130 处,是 252 处** —— `.expect(…)` 和 `.unwrap()` 一样 panic,而它在这仓库里几乎一半一半:`.lock().unwrap()` **111** + `.lock().expect(…)` 单行 **113** + 换行形 `.lock()` ⏎ `.unwrap()/.expect()` **15** + `RwLock` 的 `.read()/.write().expect(…)` **13**,分布在 **27 个文件**(25 个 core + 2 个 src-tauri)。用户只 grep 了 unwrap 那一半。
- **`panic = "abort"` 确实是刻意不加的** —— 根 `Cargo.toml` 顶部有 ⚠️⚠️ 一整段:`attach.rs` 的 PDF 文字层抽取靠 `catch_unwind` 兜 `pdf-extract` 在畸形 PDF 上的 panic,abort 会让「往聊天里拖一个坏 PDF」= 整个 app 当场消失。**所以 unwind 是有意留白,毒锁是活风险,不是理论风险。**
- **媒体那块的爆炸半径比用户说的小** —— `media/mod.rs:598` 那一带是 **11 个独立小锁**(`pending_play` / `playback` / `playlist` / `mode` / `audio_track` / `audio_track_lang` / `rate` / `current_local` / `progress` / `progress_at` / `skip_ctx`),不是一把大锁。一次中毒废掉的是**那一个字段**(`playlist` 中毒 → 队列 / 下一集 / 选集全死,但音量倍速还活着)。仍然要修,只是没到「播放全废」。

### 选路:扩展 trait 留在 std,不引 parking_lot

两条路都是「宽而浅」的机械改动(每处删一个 token 或换一个方法名,零类型签名变化、零控制流变化、零跨 crate 涟漪 —— `Mutex<T>` 字段声明一个字都没动):

| | parking_lot | 扩展 trait(选这个) |
|---|---|---|
| 改法 | 换 import + 删 `.unwrap()` | sed → `.lk()` |
| 依赖 | **已在 Cargo.lock**(librqbit 拉的 0.12.5)→ 加成直接依赖零编译成本 | 零新依赖 |
| 中毒时 | 压根没有毒锁标志位 | 有 `Err` 可接 → **有地方记一句 `warn!`** |
| 附带 | 略快 / 内存略小 | `Condvar` 那处一个字不用碰 |

定的是 trait,决定性理由是 **§3.5「不静默失败」**:无条件吞掉中毒 = 把「整块状态永久死亡」换成「偷偷带着可能半更新的状态继续跑」,那个 `warn!` 是那次 panic 在日志里唯一的痕迹。parking_lot 连这个机会都没有。次要理由:仓库里**已有**这个惯用法的先例(`tools/pdf.rs:74` 的 `unwrap_or_else(|p| p.into_inner())`),归一比引依赖自然。

### `warn!` 必须有界(否则自己把证据挤掉)

第一版想写「每次恢复都 `warn!`」。不行 —— `larkwing.log` 到 5MB 就轮转(§4.11),而中毒的**热**锁(播放进度那种,15s 心跳 + 每次调整)每次访问都吼一句,会把真正有价值的那条 panic 消息挤出日志。**为了记住这次事故而销毁事故现场**,反了。故:前 8 次 + 之后每 1024 次,每条带 `#[track_caller]` 的调用点 + 累计次数;`note_poison` 标 `#[cold]` 不污染取锁热路径的代码布局。计数用一个进程级 `AtomicU64`,不需要 per-lock 状态(那要改类型,涟漪就大了)。

### 机械改动的四个坑

1. **zsh 不对未加引号的变量做词分割** —— `FILES=$(grep -rl …)` 然后 `perl $FILES` 把整串当**一个**文件名,`Can't open …`,一次没跑。改 `grep -rl … > 文件 && xargs perl < 文件`。所幸失败得很响,`git status` 确认工作树没被污染。
2. **导入插进了多行 `use {…}` 块里面** —— 自动插入脚本认「头部最后一个 `use` 行」,而多行 use 的续行(`    ChatEvent, …`)不匹配「空行/注释/属性」→ 循环 break,`last_use` 停在那个 use 的**开括号行**上。3 个文件中招(`engine/turn.rs`、`commands.rs`、`webrender.rs`),编译器报得莫名其妙(`unresolved import crate::llm::r#use`)。手工挪出来。
3. **4 个 mod.rs 头部先是 `mod` 声明** —— `media/` `engine/` `channels/` `voice/` 的 use 块在 `pub use` 重导出之后,脚本找不到「头部 use 块」直接报 `!!` 跳过(它**没有**乱插,这点是对的)。手工插进各自的 `crate::` 组。
4. **嵌套 mod / fn 里的 `use` 不继承父作用域** —— `ftp.rs::fake_ftp`、`web_render.rs::tests`、`weixin.rs` 两个 `#[tokio::test]` 函数体内各有自己的 `use std::sync::Mutex;`,得各加一行 trait 导入;`web_render.rs` 与 `weixin.rs` 的**顶层**导入反而是多余的(调用点全在嵌套作用域里),编译器以 `unused import` 点出来。

### 顺手修掉的四处真问题(机械改动路上撞见的)

这是这批最值钱的部分 —— 都不是「毒锁本身」,是那 4 处**刻意**写成「中毒就静默跳过」的判断,一个个看下来全是 bug:

- **`engine/turn.rs` `Drop for InjectGuard`**:原文 `let Ok(mut st) = self.inject.lock() else { return };` —— 中毒就悄悄放弃,既不上收尾闸(`inject` 会继续往死回合里塞消息)又把排队的消息凭空丢了,**正与它下面那句 `tracing::error!` 自称的「inject 已返回过 true,不能凭空丢」自相矛盾**。
- **`src-tauri/src/webrender.rs` `WindowEvent::Destroyed`**:中毒就不从注册表摘除会话 → **永久占着一个会话槽**,而 `SESSION_MAX = 2` → 之后 `web_render` 开不出窗。
- **`channels/mod.rs::set_state`** / **`commands.rs::restart`**:中毒就不更新 / 不清渠道连接状态 → 设置页永远显示过时的连接态。
- **三个 `Drop` 是 abort 路**:`BgTicket`(bgtasks 收尾)/ `TaskHandle`(HUD 进度条,注释写着「没收尾就没影了(panic / future 被取消):如实告诉 HUD,绝不留僵尸转圈条」)/ `ClearGuard`(确认闸),改前都是 `.lock().expect(…)`。**drop 期间 panic 若正逢 unwind → 直接 abort 进程**。也就是说:这三个专为 panic 路径设计的收尾守卫,自己在 panic 路径上会杀掉整个进程。这比用户描述的「子系统死掉等重启」严重一档。

### `db.rs` 那把锁单独论证过

`Db(Arc<Mutex<Connection>>)` 是唯一一把「继续用可能半更新的状态」需要真论证的锁:中毒意味着有人持着这个 `Connection` panic 了,问题是会不会留下一个没回滚的事务。**不会** —— rusqlite 的 `Transaction` 是 RAII,不 commit 就在 drop 里 rollback;而局部量 drop 顺序是声明的逆序,`Db::tx` 里 `tx` 在 `conn`(那个 guard)**之前** drop → unwind 时必然先回滚、后中毒。下一个调用者拿到的连接是干净的。这段推理逐字写进了 `db.rs::tx` 的 doc,免得后人再推一遍。

### 守卫与验证

- `scripts/check-locks.sh`:五条禁令(unwrap / expect / 换行形 / RwLock / 静默跳过)+ **阳性对照**(解毒调用点 < 100 就判「扫描路径不对,上面那堆 ✓ 全是假绿」),同 `check-i18n.mjs` 立的「每条都报扫了多少」规矩。独立成 CI job(`source-guards`,ubuntu 纯 grep 不编译):秒级出结论,且不与「测试挂了后面 step 就不跑」互相牵连。
- **金丝雀验过**(§8.5 那条「跑完注入一个假错验证靶场真在查」的路子):往 `tasks.rs` 尾部注入五种违规各一条,五条全被抓到、退出码 1;还原后回 0。**别只看守卫绿了就信它**。
- 结果:`.lk()/.rd()/.wr()` 共 **278** 处,裸取锁 **0**;测试 **825 lib + 20 engine** 与基线逐条一致(825 含新增的 3 条 lockext 自测,即改动本身零回归),两个 crate `clippy -D warnings` 零警。
- 一个 clippy 小插曲:`n % 1024 == 0` 被 `manual_is_multiple_of` 拦(rust 1.96 的新 lint),改 `n.is_multiple_of(1024)` —— MSRV 声明已删,新 std API 可以直接用。

### 边界(别误读这批的收益)

**这不修 panic 本身**,只是把「一次 panic → 子系统永久死亡、必须重启」换成「一次 panic → 那一次操作失败、日志有痕、程序继续」。真正的 panic 该修还是要修,而那句有界的 `warn!` 就是让它们变得可发现的东西。

## 全仓体检:一次「还有什么要优化的」扫出来的东西(2026-09-08)

> 起因 = 用户一句「看看还有什么需要优化的东西」。做法 = 四路并行体检(Rust core 健壮性 / 前端 / AGENT.md 符号漂移 / 测试与 CI 覆盖)+ 自己跑机械检查,然后「一起修」。

- **先说体检里"好"的一半**(免得后人以为这仓库一团糟):`cargo test` 807 全绿、`vue-tsc` 零错、**TODO / FIXME / console.log / @ts-ignore 全仓皆为 0**、`as any` 只 5 处(全在 shaka 那块)、i18n 键集 zh 884 = en 884 零漂移、AGENT.md 引用的 **203 个代码符号只有 1 个过时**。所以这批修的不是"烂代码",是**结构性缺口**:没有 CI、流式期间整屏重排、几处常驻资源无界、几族 helper 重复。
- **头号发现:没有任何 CI 在 push/PR 时跑**。`.github/workflows/` 只有 `release.yml`,`on: push tags: v* / workflow_dispatch` —— 807 个测试、clippy、`vue-tsc`、i18n 键集**从未在 CI 跑过**,全靠本地手跑;而近两周有 5 次 worktree 分支合并。本批最该先补的就是它。
- **符号漂移两处**(§10 要求文档跟着代码走,这是对账结果):AGENT §7.1 写的 `VideoOverlay.KEYS` 已在 2026-09-07·二批被抽进 `src/composables/useMediaKeys.ts` —— `VideoOverlay.vue` 里连 `KEYS` 这个标识符都没了,而代码注释还写着「键位表本体在 useMediaKeys」(**文档与代码注释互相打架**);§6.6 写「zh/en 各 56 条」而实际 `tool` 下各 **63** 条(两侧仍完全同步,只是数字陈旧)。**教训:写死数字的文档句必然漂** → 改成「两侧同步改齐(当前 63 条)」+ 交给脚本机器守。
- **一个查过之后确认「不是问题」的**:PLAN 提到的 `lw:pet-typing` 事件在代码里不存在 —— 但功能在,打字状态经 `lw:pet-behavior` 过窗(`petTyping` 是 MainLayout 的局部 ref,决策在主窗、悬浮窗只收广播做映射渲染)。**文档没记 ≠ 没有,推断出的"缺口"也要下代码 grep**(同 docs-drift 那条老教训)。
- **两个 subagent 报出的、经核实是误报的**(记下来免得后人再追):① 「VideoOverlay 卸载漏清倒计时器」—— 第 334 行另有一个 `onUnmounted` 已清 `cancelCountdown()` + `skipOsdTimer`(一个组件里两个 `onUnmounted` 都会跑);② 「测试基线 800」—— 那是 worktree 落后 3 个提交时的数字,`b9a5646` 上是 807(差的 7 条来自音乐播放改版那批)。**agent 报的行号与数字要抽样核**。

## push/PR CI:三个 job 各自的理由(2026-09-08)

- **mac 跑 test + clippy,不用 ubuntu**:core 依赖 sherpa-onnx **预编译原生库**,ubuntu 大概率编不过;开发机本来就是 mac,与本地口径一致。缓存与装配步骤照抄 `release.yml`,**sherpa cache key 刻意与它逐字一致 → 两个 workflow 共用缓存**;rust-cache 加 `save-if: main` 免 PR 互挤。clippy **刻意放最后**:前面 step 挂了后面就不跑,测试先跑才能在 clippy 红着时仍看到测试结论。
- **`clippy -- -D warnings`**:开工时 core 有 28 条 lib 警告,这批清零了,所以能上 `-D`。两处 `!(x > 0.0)`(`fixed_segments` / `plan_copy_segments`)是**故意的 NaN 防护**(`x <= 0.0` 对 NaN 判 false 会漏过闸门),走 `#[allow(clippy::neg_cmp_op_on_partial_ord)]` + 中文注释「别按 lint 建议改」。
- **`cargo fmt --check` 没加**:实测仓库**不是** rustfmt-clean(`examples/asr_probe.rs` 等处成片差异,多是长中文串该折行没折)。全仓 `cargo fmt` 的 diff 巨大且会撞在飞改动 → 要开就单独做一批 format。yml 里留了注释说明「怎么开、为什么现在不开」。
- **windows-check 必须先 `pnpm build`**:`src-tauri/src/lib.rs` 的 `tauri::generate_context!()` 是编译期宏,要把 `tauri.conf.json` 的 `frontendDist`(`../dist`)嵌进二进制,而 `dist/` 在 `.gitignore` 里 → 不先出 dist,`cargo check -p larkwing` 直接失败。crate 真名是 **`larkwing`**(不是 `larkwing-app`)。cfg(windows) 代码分布:core 10 个文件 38 处、src-tauri 6 个文件 14 处,两个 crate 都值得 check。这个 job = §8.5 那条的**补网**,不取代合入前的本地靶场(本地 30 秒 vs CI 一趟)。

## scripts/check-i18n.mjs:守 §6.6 五条,并且防"假绿"(2026-09-08)

- **为什么要**:§6.6 的键集一致、占位符一致、无字面 `{ } @ |`、不硬编助手名,全是**硬规则却全靠人肉**;而 vue-i18n 对缺失 key 只发 warning + 回落中文,`vue-tsc` 也不会红 → 漂移零防护。
- **零新依赖**:locale 是纯 JS 对象字面量(核过,无 TS 语法),直接把文件当 ESM 走 `data:` URL import(注释天然被丢掉);万一以后有人写进 TS 语法,自动回落 `typescript.transpileModule`(devDependencies 已有 typescript)。
- **防假绿是设计要点**:每条检查都打印「扫了多少」(如「扫了 71 个文件里 747 处字面量 key」)—— 一个匹配不到东西的正则会**静默全绿**,数字露出来人一眼能看出不对。落地时在沙箱里造了五类违规做 canary,确认 5 类全报、`exit=1`,且 `pet.name = '旺财'` 正确豁免、`$t('reminders.repeat.')` 这种 `.` 结尾的拼接前缀正确跳过。
- 第 ⑤ 项刻意宽松:大量键是动态消费的(`tool.*` / `err.*` / `settings.*_${v}` / 拼接前缀),只报**完整字面量且确实找不到**的;含 `${` 的模板串、以 `.` 结尾的前缀一律跳过。

## 资源管护:三处「原先无界 / 一把清空」(2026-09-08)

- **TTS 语音缓存从不淘汰**:`<数据根>/voice/tts` 按 `tts::cache_key(voice, rate, text)` 落盘,全模块没有任何清理(开发机已 83 个文件 / 10MB;家庭每天用语音应答,长期几百 MB)。修 = 纯函数 `plan_cache_eviction(entries, max_bytes, min_age_secs)` 决定「该删哪些」(总量不超限 → 一个不删;超了 → 只在够旧的候选里最旧优先删到水位线 80% 即停;候选删完仍超限 → 就这样,新文件一个不碰;同龄按文件名定序,否则 `read_dir` 顺序会让行为飘)+ 执行侧 `sweep_tts_cache` 与限频 `maybe_sweep_tts_cache`。
  - **触发点选在「缓存刚落一份新文件之后」而不是 `VoiceRuntime::new`**:后者在 core 里被构造 7 处(壳层 boot / channels / 两个工具 / 三处测试),每次 spawn 一条常驻线程不合适;而**只有合成才会让目录变大,不合成就完全不用扫** —— 零常驻线程,顺带绕开 §8.4「同步命令没有 tokio 上下文」。
  - **7 天保护期 + 宁可少删**:内容寻址的缓存文件被 relay `/tts/` 按路径现读,删掉正在播的那份会让那一次播放失败;保护期也顺带盖住启动预合成的唤醒应答音银行。
  - **缓存命中时 touch mtime 有意没做**:`File::set_times` 虽够 MSRV,但会在 TTS 命中热路径上多一次 open-for-write,且与 relay 并发读时的 Windows 文件共享行为在 Mac 上验不了。代价 = 退化成「最早产生优先淘汰」,误淘汰一句热门话 = 重合成一次(它本来就只是缓存),且只在超 512MB 时才发生。
- **`native.log` 的前提被纠正了**:它**不是**只增不减 —— 早有 `TRUNCATE_AT = 5MB`,但处理方式是 `std::fs::write(&path, b"")` **一把清空**,于是最后那 5MB(常常正是崩前的原生库报错,§8.1 那套 /MT 静态 CRT 坑的唯一现场)全丢。改成轮转成 `native.log.1`(只留一代),仍在 fd 2 重定向**之前**做 = 本进程还没打开它、零竞态零后台线程;抽纯函数 `plan_boot(Option<u64>) -> Boot{Append,Rotate}`(`None` = 读不到就别乱动)+ 3 个单测(`src-tauri` 8 个文件里此前只有 2 个有测试,nativelog 不在其中)。磁盘上限由 5MB 变约 10MB。
- **relay 注册表**见 [media-playback.md](media-playback.md)「relay 注册表从「只增不减」改成按字节权重有界」。
- **【同日 · 用户拍板:现值确认】** 三族常量按 §4.11 摆给用户后,答「其它常量合理就可以,我暂时没什么特殊要求」→ 现取值即定值(relay 64MB / 500 条 / 保底 8 · TTS 512MB / 80% / 7 天 / 6 小时 · native.log 5MB 轮转一代)。**唯一有实质变化的是最后一条**:磁盘上限由 5MB 变约 10MB,换来「崩前那 5MB 原生库报错不再被清空丢掉」—— 而那常常是 §8.1 那套 /MT 静态 CRT 坑的唯一现场。真机跑一段若嫌某个数不合适,改常量即可(全部单源)。

## 四族 helper 归一 + 一处 MSRV 违规(2026-09-08)

- **`now_ms` 4 份**:单源在 `store/db.rs`(已 `pub(crate) use` 出来),而 `bgtasks.rs` 逐字复制了一份,`voice/mod.rs` 两处、`voice/wake.rs` 一处各有内联 `SystemTime` 副本。全改调 `crate::store::now_ms`。原来三处产 `u128`、现在 `i64`,格式化出的数字串相同(都是正值毫秒),ID 形状不变。
- **按字符截断 5 份 → 新 `larkwing-core/src/text.rs`**:`llm/openai_compat.rs` 与 `llm/anthropic_compat.rs` 的 `truncate_chars` **逐字相同**;`web.rs::clip`、`engine/turn.rs::clip`、`engine/mod.rs::clip_chars` 同核心、只差尾缀。归一成 `clip_fmt(s, max, 尾缀闭包)` + `clip(s, max, &str)` 简写 —— **收闭包而不是 `&str`,是因为 `turn::clip` 的尾缀带动态剩余字数**(「还有 N 字,跑完看全」)。两份逐字相同的直接删掉,三处有自己话术的留成一行薄壳:**算法单源、话术归各站点**(这些字符串进 prompt 与工具结果,话术变了就是行为变了)。新函数单测覆盖多字节边界(不能切半个字)。
- **`arg_str` 两形**(`tools/skill.rs` 的 Option 形 / `tools/fs.rs` 的 Result 形)归 `tools/mod.rs` —— 与 `arg_bool` / `arg_u64` / `expand_home` 同一个家(§7.1 明写这类宽容解析要收口)。`fs.rs` 的报错话术逐字保留。
- **`download_client` 两份**归 `net::download_client(ua, total_timeout)`:**它们并非逐字相同** —— `tools/web` 带浏览器 UA、`media/download` 刻意不带(防盗链头由 yt-dlp 逐请求给),所以 UA 参数化;仍走 `net::Client` 守 §4.6。
- **MSRV 违规一处**:`eval/scenarios.rs` 用了 `Option::is_none_or`(**1.82 才稳定**),而两个 crate 都声明 `rust-version = "1.77.2"`,且 `eval` 是 `lib.rs` 里无条件 `pub mod` → 真违规(clippy 的 `incompatible_msrv` 一直在报,只是没人把 warning 当回事)。改写成 `map_or(true, …)`。**没抬 MSRV** —— 那是产品决策(1.77.2 钉着 `sevenz-rust2 0.7` / `sysinfo 0.33` 的版本选择),要用户拍板。顺带把 MSRV 从「两个成员各写一份」改成 workspace 继承(重复值必漂)。
- ⚠️ **CI 用的是 `dtolnay/rust-toolchain@stable`,不是 MSRV pin** → `1.77.2` 这个声明目前**无人验证**(本机也只装了 stable)。要真守就得加一个 MSRV job,会拖长 CI;记档待议。
- **【同日后续 · 用户拍板:删】** 我把三条路(抬版 / 加 MSRV job 真守 / 删声明)摆出来后,用户答「MSRV 没用就删」。已删三处声明(根 `[workspace.package]` + 两个成员的 `.workspace = true`)。**理由**:Larkwing 是自己编译出包的桌面应用、不是给人依赖的库,声明 MSRV 只约束得到自己;而它反向钉住依赖的版本选择(cargo 的 MSRV-aware resolver),还制造过「clippy 报 `incompatible_msrv`、代码要绕开 1.82 的 std API」这类纯自伤(本批那处 `is_none_or` 改写就是)。**副作用与处置**:① `Cargo.lock` 一字未动 → 依赖版本一个没变,删声明不等于升级;② 但从此 `cargo update` 不再被 MSRV 挡 → `sevenz-rust2 0.7` / `sysinfo 0.33` 那两处「新版要 1.85+」的钉版改由**依赖行上写死版本 + 注释**来挡(AGENT §7.2 / §7.10 口径同步改了);③ AGENT §7.8 那条「用较新标准库方法前先看 clippy MSRV 检查」**已作废**并原地标注;④ 本批那处 `is_none_or → map_or` 的改写**被 clippy 自己简化回去了**(见下条),等价、照收。**别再把 `rust-version` 加回来。**
- **删声明的意外收获(同日实测)**:MSRV 声明还在**抑制** clippy 那批「建议用新 std API」的 lint。删掉后立刻冒出 **core 13 条 + 壳层 11 条**新警告(`map_or` 可简化成 `is_none_or`/`is_some_and` · `% n == 0` → `is_multiple_of(n)` · `repeat().take(n)` → `repeat_n`),`cargo clippy --fix` 全部自动改掉、逐条核过是纯等价替换(含 `engine/context.rs` 里 `WINDOW_CHUNK` 整块锚定那处 —— 前缀缓存的核心不变量,有 golden 测试钉着,改完仍绿)。**所以「删 MSRV」不是零 diff 的动作**:它会让代码朝 stable 的惯用法收敛一小步。

## `[profile.release]`:加了 LTO,禁了 panic=abort(2026-09-08)

- 之前**完全没有** profile 调优。加 `lto = true` / `codegen-units = 1` / `strip = true`(806 个依赖的二进制,通常能缩两到四成、冷启略快)。
- **`panic = "abort"` 永远别加**:`larkwing-core/src/attach.rs` 的 PDF 文字层抽取靠 `catch_unwind` 兜畸形 PDF 的 panic(`pdf-extract` 会 panic),abort 会让用户拖进来一个坏 PDF 就杀进程。profile 上方写了醒目禁忌注释。
- **`strip = true` 的代价**:panic backtrace 的帧名变地址(panic **消息**不受影响,§8.1 的 native.log 照旧能捞);排符号化崩溃栈时临时注掉。
- ⚠️ **本机没验过发版编译**:fat LTO + `codegen-units = 1` + Windows `+crt-static` + sherpa 静态库,发版编译会显著变慢,且大依赖树上 fat LTO 偶发撞 CI 链接器内存上限。**下一个 `v*` tag 若挂在链接步骤,先把 `lto` 换成 `"thin"`**(进了 PLAN watch-items)。

## 明确没做的(别当遗漏捡)

- **拆巨型文件**:`media/mod.rs` 3960 行 / `engine/mod.rs` 3814 行(`impl Engine` 一块约 2435 行 75 个 pub 方法、`set_setting` 一个 230 行 match)/ `SettingsView.vue` 2956 行装 8 个 tab。纯维护性、零行为收益,而 diff 巨大、撞在飞改动的风险高 → 单独一批做,别混在功能批里。
- **`parking_lot` 换掉约 130 处 `lock().unwrap()`**:任一任务在持锁段 panic 就毒锁,之后 `MediaRuntime.inner` / `Engine.sessions` 这类进程级状态整体失效。是设计取舍(新依赖 + 130 处触点),记档不做。
- **聊天历史「加载更早」**:`load_conversation` 只回最近 200 行,而 `search_messages` 跨全表 —— 命中 200 行之前的消息,点开后看不到那条。修它要改命令签名 + UI 分页 + 命中定位 = **新功能**,不在「优化」范围内。
- **前端单元测试**:全仓零 vitest;`useLyrics` 解析 / `useScrubHover` 算位 / `useMediaKeys` 键表 / `streamGroups` 分组这类纯函数正是前端出过 bug 的地方,值得补。本批只补了 Rust 侧 14 条。
