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

## 拆巨型文件:三个一起拆,靠「零差异行」当验收(2026-09-08·二批)

> 上一批把它记进「明确没做」(理由 = 零行为收益 + diff 巨大),用户随后拍板「ABC 一起搞吧,也不是什么复杂的事儿」。
> 结果:`engine/mod.rs` 3897→534 · `media/mod.rs` 3975→957 · `SettingsView.vue` 2957→1491,新增 17 个文件。

### Rust 侧能做成纯搬运的原因(也是选这个切法的理由)

**子模块能访问父模块类型的私有字段。** `engine/settings.rs` 里写 `impl Engine { … }` 照样能碰 `self.store` / `self.llm` —— 不需要改任何字段可见性、任何签名、任何调用点。

更关键:**仓库已经在这么干了**。`media/download.rs` / `edit.rs` / `lyrics.rs` / `archive.rs` / `torrent.rs` / `usage.rs` / `introdetect.rs` 七个文件本来就是 `impl MediaRuntime` + `self.inner.…`。所以这不是引入新范式,是把 mod.rs 里剩下的那摊按同一个模子分出去 —— 当初加新功能都懂得开新文件,只有老代码一直在原地堆。

**可见性是单向的,这是唯一要小心的地方**:父 → 子可见,子 → 父、子 → 兄弟都不可见。于是搬出去的私有 helper 若被别的文件调用,就得标 `pub(super)`。engine 11 处 / media 28 处,一个个都是「原签名前面加两个词」。

⚠️ **教训:算跨文件调用时别只扫 mod.rs。** media 这边漏了 `probe_with_ffmpeg` / `refresh_skip` —— 它们被**早就存在**的兄弟模块(`edit.rs` / `introdetect.rs` / `lyrics.rs`)调用,我只分析了 mod.rs 内部的调用图,靠编译器才逮着。

### 验收 = 多重集比对,「丢失的行」必须逐条能解释

搬完把新文件的行拼起来,和 `git show HEAD:<原文件>` 做非空行多重集 diff:

- engine:3651 非空行里「丢失」**12 条** = 11 条加了 `pub(super)` 的签名 + 1 条 `fn engine` 夹具进了 testkit。
- media:3760 非空行里「丢失」**28 条**,全是加了 `pub(super)` 的签名。
- SettingsView:2873 非空行里「丢失」**11 条** = 8 条 import 重写 + 1 条内联 `import('../lib/backend')` 补路径深度 + 1 条 `refreshAutoBackup()` 换了 onMounted 的家 + 1 条 `wxLoginSeq++` 换成函数调用。

新增的行全是脚手架(`//!` 文档 / `use super::*` / `impl X {` / `}` / `mod` 声明 / `pub use` 再导出 / `return {…}`)。**这个「零差异行」性质是整批的验收依据 —— 所以搬运期间看见 bug 也不修**,一旦掺进一行真改动,比对就不再是证明。

### 测试:随主题走,共用夹具进 `testkit`

测试 fn 从 `mod tests` 挪进各主题文件自己的 `mod tests`,**缩进不变**(都是 4 格,原来在 `mod tests {}` 里、现在在新文件的 `mod tests {}` 里)→ 照样字节相同。共用夹具(engine 的 `engine(tag)`;media 的 `runtime` / `touch` / `mk_shuffle_playlist` / `report`)进 mod.rs 的 `#[cfg(test)] mod testkit`,标 `pub(super)` → 父模块里 `pub(super)` 的项对**全部后代**可见,兄弟文件 `use crate::engine::testkit::engine` 即可。测试总数前后都是 825 + 20,一条没少没多。

### Vue 侧:只抽 script,template 与 style 一行不动

`SettingsView.vue` 的 2957 行里,script 占 1663 —— 真正的问题在那。抽成 `composables/settings/use*Settings.ts` 七个,SettingsView 用**同名解构**接回来,于是 **template + style 那 1294 行字节级不变**(脚本核过 `==`)。

**为什么不顺手把 template 拆成 `VoiceTab.vue` 之类**:scoped CSS 只给子组件的**根节点**带 scope id,后代一律不带。把 `<div v-if="tab==='voice'">` 挪进子组件,那 214 行 scoped 样式里每一条打在后代上的规则都会**静默失效** —— 正是 §6.7 那条刚被咬的坑(`.md code` 等 12 条规则白写三个月)。要拆得连样式一起搬 + 逐条量 computed,那是另一个决定。

三处不得不真改的地方:
1. `wxLoginSeq` 是模块内的 `let` 代次(重扫 / 切走 tab 时作废旧轮询),**不能过解构** → 新增 `leaveRemoteTab()` 代劳,view 里的 `watch(tab)` 改调它。这是整批唯一新写的函数。
2. 设置页那个大 `onMounted` 里混着 `refreshAutoBackup()`(属 data 组)→ 挪进 `useDataSettings` 自己的 `onMounted`,挂载时机不变。
3. `famOpts`(渠道对话「指认给谁」下拉)夹在大脑那段代码里,其实属家人组 —— 按语义归位,不按文件里的位置归位。

`useSettings` / `useVoice` / `useWakeCalib` / `useCaptureRoute` 都是模块级单例,各 composable 自己调即可,不用透传。`t` 走 `useI18n()`(composable 由 setup 同步调用 → 合法;§6.6 那条 `i18n.global.t` 的坑只管模块作用域)。

### 明确没做

- **`set_setting` 那个 231 行 match 没拆**:原样进 `settings.rs`。掺进真改动就毁了上面那个验收性质;要拆成按前缀分派的几个 fn,单独排一批 —— 那时 diff 只有几十行,能好好审。
- **`media/local.rs` 1024 行仍是最大的一个**(`play_local` 一个方法 297 行)。它是一条完整的管线搭建流程,再切会把「探测 → 判路 → 搭管线 → 兜底」切散;等真有第二个消费者再说。

## 明确没做的(别当遗漏捡)
- **`parking_lot` 换掉约 130 处 `lock().unwrap()`**:任一任务在持锁段 panic 就毒锁,之后 `MediaRuntime.inner` / `Engine.sessions` 这类进程级状态整体失效。是设计取舍(新依赖 + 130 处触点),记档不做。
- **聊天历史「加载更早」**:`load_conversation` 只回最近 200 行,而 `search_messages` 跨全表 —— 命中 200 行之前的消息,点开后看不到那条。修它要改命令签名 + UI 分页 + 命中定位 = **新功能**,不在「优化」范围内。
- **前端单元测试**:全仓零 vitest;`useLyrics` 解析 / `useScrubHover` 算位 / `useMediaKeys` 键表 / `streamGroups` 分组这类纯函数正是前端出过 bug 的地方,值得补。本批只补了 Rust 侧 14 条。

## 拆 relay.rs:目录模块的变体(2026-09-16)

> 来源:2026-09-16 影音体检第三批。第一路审查点名 `media/relay.rs` 2868 行(修完 bug 后 2995 行)该按 §6.1 拆;因为搬运批不能掺真改动,排在两批修改之后单独走。

**和上一次拆的不同处**:engine / media 那两次拆的是 `mod.rs`,新文件是同一目录下的兄弟;这次拆的是**叶子文件**,新文件成了它的**子**模块(`relay/mod.rs` + `relay/*.rs`)。两条推论:① 搬过去的代码里 `super::probe::first_moof_offset`、`super::no_console` 原先指的是 media 的兄弟,现在 `super` 是 relay —— **不改这些行**(改了多重集验收就对不上),在 `relay/mod.rs` 里 `use super::{no_console, probe};` 把 media 的两个名字借进 relay,子文件的 `super::` 就照旧解得出;② 对外可见性不能只靠 `pub(super)`:`pub(super)` 在子文件里只到 relay,给 media 用的项要保留原 `pub` / `pub(crate)` 并在 mod.rs `pub use child::X;`(`gen_video_init` / `gen_video_segment` / `VideoEncoder` / `video_encode_args` / `cover_jpeg` / `AUDIO_LOUDNESS_AF`)。子文件一律 `use super::*;` —— 能拿到 mod.rs 的私有项与私有 `use` 绑定(`Arc` / `Response` / `UpStream` 这些顶部 import 也在内),测试模块再 `use super::*;` 一层层链上去。

**切法**:先 `git mv relay.rs relay/mod.rs`(git 认成改名,历史不断),再跑一个按**原行号段**搬运的脚本:每段都从上一项的收尾空行起、到本项的顶层 `}` / `;` 止,所以文档注释天然跟着项走;测试按主题分配(每个 test fn 的边界 = 4 空格缩进的 `}`),各文件自带 `#[cfg(test)] mod tests { use super::*;` 壳;脚本自带覆盖检查(1..2995 每行恰进一个文件,漏切的只能是 1999 空行与 2000–2002 的老 `mod tests` 头)和 17 处签名替换(跨子文件调用的 `fn` 加 `pub(super)`),编译再补 3 处(`build_mpd`、`RemuxQuery`、`ThumbQuery`:handler 是 `pub(super)` 了,它签名里的 Query 类型不能更私)。**§4.11 常量全留在 mod.rs 顶部**,单源落点不漂(AGENT 里 `relay.rs` 的三处路径改写为 `relay/mod.rs`)。

**验收**:`git show HEAD:relay.rs` 与 `cat relay/*.rs` 各去空行排序比多重集 —— 丢 20 行、每行都是加了 `pub(super)` 的签名;多 108 行全是文件头 / `mod` / `use` / 九份 `mod tests` 壳。lib 839 / engine 20 / clippy 两 crate 零。一条小坑:`pub(crate) use preview::{cover_jpeg, crop_sprite_cell};` 里 `crop_sprite_cell` 没人在 relay 外用,unused import 在 `-D warnings` 下是红 —— **re-export 只给真被外面用的**。

**落点**:`mod.rs`(Entry / Relay / Inner / 注册表 / FifoCache / 常量 / register_* / `lookup` `bad` `bytes_response`)· `encoder.rs` · `upstream.rs`(/s/ /dash/ + fetch_head + build_mpd)· `file.rs` · `hls.rs` · `adaptive.rs` · `ffmpeg.rs`(gen_video_* / build_*_frag_cmd / run_ffmpeg_collect)· `progressive.rs`(/m/ + spawn_stream)· `preview.rs`(thumb / sprites / cover)· `collect.rs`。**没顺手改的**:`preview.rs` 816 行仍是最大的一个(三条预览路本就同一把串行闸与两级缓存,再切会把闸切散);`stream_ffmpeg` 与 `run_ffmpeg_collect` 的两条「子进程纪律」注释各在一处,没合并成一段。

## 体检二轮:第一二档修复(2026-11-11)

**起因**:用户「扫一遍,看看有没有什么可优化的、新做的」。上次全仓体检是 2026-09-08,影音单独体检是 09-16;这次六路只读扫描(引擎与 agent 运行时 / 工具·web·文件·存储 / 语音与渠道 / 前端 / 壳层·CI·依赖 / AGENT 对账),重点看 09-08 之后新进的代码。子代理的报告逐条下代码核过才算数(第一档十三条全部亲手读过那几行;第二档少数几条是子代理读代码确证、主会话没复核,落地时又各自读了一遍)。用户看完清单拍板「第 1、2 档一起做」。前端那一半(`src/`)由一个分身并行改,主会话改 Rust,最后逐段核对分身的 diff。

下面按「坏法 → 修法」记,行号是修之前的。

### 钥匙与记忆

- **Windows 上配第三家大脑,钥匙静默落回明文**。keyring-3 的 Windows 后端在 `set_password` 之前校验 `password.encode_utf16().count() * 2 > CRED_MAX_CREDENTIAL_BLOB_SIZE`(2560),超了报 TooLong。`llm.providers` 是 `to_string_pretty` 的整张 JSON,`ProviderSpec` 与 `Quirks` 没有任何 `skip_serializing_if`,每家约 550 字符;`effective_specs` 恒含 DeepSeek 与 Anthropic 两个模板,所以保存任何第三方供应商就是三条、约 1600 个单元。`secrets::set` 的失败分支先删 keyring 旧条目再把整张 JSON 写进 `settings`,只有一行 warn —— §6.3「不进 SQLite 明文」悄悄失效,一键备份 / 自动备份的 zip 里也带着全部钥匙。09-04「国内五家接通」之后这是常态路径。修 = `secrets.rs` 内分片:抽 `Vault` trait(Windows = keyring,测试 = 带同一条长度闸的内存表),`vault_set` 放得下单条存、放不下按 1200 单元切(按 char 累计,不劈代理对),先写新一代分片、最后把主条目写成 `\u{1}lw-chunks:代次:片数`、再删上一代;读时见头就拼,缺片报错不拼半截。微信多绑定与下载认证同一条路白拿。**只在 Mac 上用内存表测过协议,Windows 凭据管理器真写真读没验。**
- **记忆提炼记错人**。`consolidate::build_request` 把所有 user 行标成「用户」,结果 `add_distilled(user_id, …)` 写给触发提炼那一回合的说话人。`habits.rs` 取料早就按行认说话人(`UserMeta.speaker_user`,没有 = 会话归属者),照搬过来。

### 引擎

- **自启回合挤掉用户回合**。`wake_turn` 的忙检在装配和 `open_stream` 之前,登记在之后;`register_inflight` 只给旁听让路。改成三态 `Claim`,自启回合撞上在飞自己让路:`launch` 返回 `Ok(None)`,`wake_turn` 撤 event 行返回 false,调度器下个 tick 重试(与开头忙检同一语义,只是晚一步才看见)。
- **空转判定只看同名同参**。`web_render` 的 `click_text:"下一页"` / `scroll:"down"` 每次参数相同、结果不同,从第二次起算没进展,第六次被 `tool_choice=none` 强制收尾。把结果并进指纹会放过「反复查进度」那种轮询(`task_status` 的结果也在变),而那正是这道闸原本拦住的东西(`MAX_TOOL_ROUNDS` 是 200)。评估过四条路:按工具名单分(具名分支,§5 / §4.11 不写)、用 `Tool::risk()`(§9 点名的休眠能力,不该为这个启用)、限次数(翻 20 页是正经活)、结果相似度阈值(又一个魔法数)。落地的是**指纹里的结果去掉 ASCII 数字再哈希**:轮询的结果只有数字在动,翻页的结果是不同的文字 —— 不立名单、不加阈值,一句注释说得清。
- **到点提醒遇会话忙无声推迟**没动:它碰 §7.4「绝不打断」,列在清单的「需要拍板」里。

### 渠道

- Telegram 的「启动丢弃积压」写在 `serve()` 里,而 `run()` 每次出错都重进 `serve()` → 每次重连都丢。微信的游标本来就是持久化、重启后照收积压,所以「重连照收」与它一致。
- 相册:Telegram 把一次选的多张图拆成多条 update、共用 `media_group_id`、只有第一条带说明。全仓原先零引用这个字段。
- `drive_turn` 的 inject-or-send:三渠道都是每条消息 spawn,前一条的回合还没登记时后一条的 inject 落空也去 `send_message`,「新的赢」把前一条取消。闸只罩到拿到 rx 为止。
- 提醒推手机原先取「会话里最新一句非空 assistant」,没有「本次 event 行之后」的界:自启回合没出文字时推的是上一轮的旧回复。
- 微信 `run_account` 的错误分支写了状态行,而清错误的分支在 `serve` 里、条件是 `fails > 0`,重进后 `fails` 从 0 起永远进不去。
- 钉钉长连接只在服务器来帧时才回应;半开连接下 `ws.next()` 永远挂起。
- `send_file` / `send_text` 的缺省目标按映射行 id 降序取第一条,而 12 小时轮换会给别的渠道插新行。

### 语音

- `wake.rs` 的 `Stop` 分支直接 `return`;`Resume` 分支有补发 `ListenEnded`,它没有。前端只认 `ListenEnded` 恢复媒体音量。
- `msedge-tts` 0.4.1 的阻塞客户端是裸 `std::net::TcpStream::connect`,没有连接或读超时;`synth_cached` 的 `spawn_blocking` 也没包超时。crate 有 tokio 异步客户端,但要开新 feature、换依赖树,这次没动 —— 改成「等待有界 + 名额由阻塞线程自己攥着」,黑洞时卡死的线程数封顶 4。
- `wake_start` 在起循环之前 `PromptBank::prepare(...).await`,应答音缓存未命中又遇网络不通时开机唤醒起不来。

### 文件与下载

- `fs_copy` / 跨盘 `fs_move` 是整理文件里唯一没接两阶段的长活。样板已经有四份(加工 / 解压 / 打包 / 扫盘,约 730 行),这次是第五份,写在 `tools/fs.rs::run_transfer`、照 `media/usage.rs` 的形。**没有顺手把五份抽成一个 helper**:那四件都在 PLAN 的 Windows 真机清单上,重构后要重验,不该混在修 bug 的批里。
- `copy_tree` 用 `symlink_metadata`,目录链接的 `is_dir()` 为假、落进 `fs::copy` 必然报错。指向文件的链接照旧按内容复制(行为不变),只跳过指向文件夹的。
- 批里的 `web_download` 用的是带 280 秒总超时的那把客户端;reqwest 的 `timeout` 是连 body 在内的总期限。

### 备份

- `backup_to` 先 `VACUUM INTO` 到系统临时目录、再建 zip:目标写不进时白写一份整库,而且写在 C 盘。自动备份失败不推水位,下一拍(10 分钟后)照跑。
- `apply_pending_restore` 成功后紧跟 `Store::open(...)?`:预检只查魔数与迁移记账,开库或迁移仍可能失败,而负载已清,之后每次启动都起不来。

### 前端(分身改,主会话核对)

- 10-11 把回合状态改成按会话记之后留下两处回归:在屏判定加了 `anchorId == null`,而 `anchorId` 只在两处复位;屏外终态分支删掉了旧码里的停表与复位。
- 「大字」只改了 `.layout` 的内联字号,而 `MainLayout.vue` 的 41 处字号全是 px、没有元素继承根字号。预览里量过 `.bubble` / `.field` / `.xp-text` 的 computed 值:标准档 14px、大字档 16.5px。
- 分身用假的 `__TAURI_INTERNALS__`(假库 + 手动推回合事件)在浏览器预览里走过定位页发消息、屏外收尾、重载窗口期 done、回溯成败、排队区按会话这几条;Tauri 真链没跑。

### 没做的(都在给用户的清单里,不是遗漏)

「可优化」与「可新做」两栏整栏没动:时钟行对前缀缓存的影响(要先量 `usage_rounds`)、调度器睡到下一个到期点、稳定前缀瘦身、CI 补壳层单测、更新端点回落、两阶段样板归一、`voice/mod.rs` 分家,以及自启回合登记、备份带图片、足迹页明细等新功能候选。few-shot「2–4 段、≤800 token」与实际 15 段 62 条不符,是规则该改还是数据该砍,得用户定。

**验证**:`cargo test -p larkwing-core --lib` 893 过(基线 878)、`--test engine` 23 过、`cargo test -p larkwing` 12 过、两个 crate 的 `clippy --all-targets -- -D warnings` 零警告、`check-locks.sh` 全过、`vue-tsc --noEmit` 无报错、`check-i18n.mjs` 5/5(918 = 918)。

## 体检二轮·续:小四件(2026-11-11)

第一二档提交之后,用户:「现在做你建议的下一批(都不大),做完再看接下来干什么」。四件:

**1. 量时钟行对缓存的影响 —— 没量出来。** 推断是:〔此刻〕挂在末条 user 上且不落库,下一回合回放的是不带它的原文,前缀从那里分叉,上一回合 user 之后的 assistant / tool 行按未命中计价;11-06 起时钟行恒在,所以每回合都这样。开发机(Mac)的库里用量流水只到 2026-09-10,一条 11-06 之后的都没有(那之前 29 个可比回合,deepseek,命中 53.8%),比不出前后。查询留在 `scripts/cache-hit.sql`:取每个回合的第一轮、排除会话的首回合,按 11-06 前后分组看命中占比与平均未命中 token。要在日常用的那台机器的库(或它的备份包里的 `larkwing.db`)上跑才有结论;没结论之前不改 —— 修法是把 ambient 存进 user 行 payload 原样回放,会让老消息里留下过期的〔此刻〕,是提示词语义的变化,不该凭推断动。

**2. CI 与三个守卫。**
- 壳层单测进 mac job、clippy 改全工作区。动手前验了一件事:AGENT §10 写着「不先出 dist,`cargo check -p larkwing` 直接失败」,而上一轮体检的子代理读 tauri 源码说 dev 档不需要。本机把 `dist/` 挪走、touch `lib.rs` 与 `main.rs` 后 `cargo test -p larkwing`:重编通过、12 条全过。所以 mac job 直接加,不用先 `pnpm build`。Windows job 那四步没删 —— Windows 上没试过。
- `check-i18n.mjs` 加第 ⑥ 条:扫 `larkwing-core/src` 与 `src-tauri/src` 里 `ui_key: "…"`、`Text::new/with("…")`、`.step/.step_progress/.fail/.fail_retryable("…")` 的字面量(`tool.` / `task.` / `step.` 开头),逐个对词典。现状 174 个 .rs、130 个不同的 key,全有定义;扫到少于 50 个就报「路径或写法变了」(阳性对照)。
- `check-locks.sh`:原先「中毒静默跳过」只认 `let Ok(..) = ..lock()`,漏了 `.read()` / `.write()` 与链式(`.lock().map(..).unwrap_or_default()`、`.read().ok()`)。实际漏网三处:`commands.rs` 的状态快照(上一批已改)、`media/capability.rs` 的 `set_codecs` 与 `codecs`(这次改成 `.wr()` / `.rd()`)。
- 两个守卫都用金丝雀验过:往 `larkwing-core/src` 丢一个不参与编译的 `_canary.rs`(里面一个不存在的 `ui_key`、一个 `.lock().ok()`),两个脚本都红、点名到行;撤掉后恢复全绿。
- `bump-version.sh`:版本号必须是纯数字 `X.Y.Z`(传成 `v0.2.43` 会被原样写进三个清单,之后 sed 只认数字旧值、永远匹配不上);`sed -i ''` 换成两种 sed 都认的 `-i.bak`;改完逐个文件核对真改到了;冒烟必须「真跑到 1 条」才算过(按名字过滤,测试改名就是 0 条、退出码 0)。冒烟那段只读过逻辑,没有真跑一次发版。

**3. 调度器。** 固定 30 秒轮询改成「睡到下一个到期点,最长 30 秒」:`jobs.next_due_after(now)` 取未来最早的待触发任务,`sleep_for` 是纯函数(可测);已过期还没触发的(会话忙 / 建连失败留着重试的)不算进来,仍按 30 秒重试,免得空转。入库时踢一脚:`scheduler::kick()` 是个全局 `Notify`(permit 语义,调度器正忙时踢的不丢),设提醒、盯天气、后台差事插汇报三处调用。放成全局而不是挂在 Engine 上,是因为 bgtasks 在 MediaRuntime 里、够不着 Engine;store 不调它(域层不反向依赖上层)。

**4. 自启回合登记。** 候选方案里否掉两条:给 `ConversationActivity.outcome` 加 `started`(现有消费者 —— 渠道推送循环、前端打标 —— 都把这个事件当「收尾」,加一个值要逐个排雷);让前端轮询全部会话的在飞态(没有事件就没有即时性)。落地的是新变体 `AppEvent::TurnStarted`(tagged 编码,老前端忽略未知 type)+ 只在有登记时才转的对账轮询。两处容易漏的时序:① 上一轮的字还在放时自启回合起跑 —— 那一轮收尾会 `releaseInFlight`、把状态灯放回空闲,所以有登记时不摘在飞标记、灯接回「在想」;② 自启回合在飞时用语音直接发(不排队)—— core 把自启回合取消,它迟到的收尾事件不能把用户回合的在飞标记解除,所以 `send()` 一占住在飞就摘掉登记。预览里用假后端走过四步(起跑 → 灯转「在想」;打字 → 进排队区、没有直接发;收尾 → 重载出系统线与回复、排队的话发出;另一个会话的收尾事件不发 → 3 秒对账收回、打标)。时序 ① 没在预览里造出来,是读代码推的。

**验证**:core lib 895 / engine 23 / 壳层 12 全过,全工作区 clippy `-D warnings` 零警告,check-locks 六条全过,i18n 6/6,vue-tsc 无报错。

## 体检二轮·三:核实后修掉(2026-11-11)

小四件之后用户问「还有什么」,清单里第 2 类是「体检里没进前两档的三十多条小问题,没逐条复核过」;用户:「核实后修掉」。做法 = 每条先读代码确认属实,属实才改;不属实 / 有意设计 / 要拍板的不动,下面逐条有结论。

### 渠道(全部属实,已修)

- **重连没有退避**:Telegram / 钉钉失败后固定 5 秒重连、没有上限。令牌被吊销或长时间断网时每 5 秒一行 warn,一天约 1.7 万行,两天冲掉一轮 5MB 日志。改成 `channels::retry_wait`:头 3 次 5 秒,之后 30 秒(微信那条路早就是这组数,直接沿用)。
- **没填凭证也在重试**:没填 token / AppKey 时照样进重连循环。改成写一条状态行后 `ct.cancelled().await` 停等 —— 填了之后 `reload_channels` 会重起。
- **「已连接」写早了**:一进 `serve` 就写已连接,此时一个请求都还没成功。TG 改成每次(重)连第一轮 `timeout=0` 探活成功后才写、并清失败计数;钉钉在 WS 握手成功后写。
- **白名单外的来信一行日志都没有**:白名单抄错一位就是「手机发了没反应,日志也查不到」。加 `note_stranger`:每个 (渠道, id) 记一次 info,上限 256 个,防陌生人刷屏。
- **攒批附件只在「下次有人发附件」时才清**:之后没人再发,一份几十 MB 文件的 base64 留在内存里直到渠道重启。`take()` 时也清,另在 `outbound_loop` 上每 5 分钟(`ATTACH_TTL / 6`)扫一遍。
- **语音消息撞上 ffmpeg 没下**:原先解码时内联去下几十 MB,手机那头对着空气等。TG / 钉钉都改成:ASR 或 ffmpeg 任一没就绪 → 回「准备中」并后台预取两者(`MediaRuntime::ffmpeg_ready`)。
- **钉钉空回复也发**:回合没说话时发出一条空消息。改成不发。
- **`send_file` 给微信白读一遍文件**:微信那条路自己边读边加密上传,公共层先整读了一遍。改成按需读。

### 语音(全部属实,已修)

- **启动途中被关,循环照样起**:`wake_start` 要先下模型 / 备应答音(可能几十秒),这期间用户把唤醒关了,下完循环照起 —— 设置里是关、麦克风开着。加 `WAKE_STOPS` 计数,进门记一次、占槽前比对,变了就不启动。
- **旧一代循环还会接着听**:循环被重启后,旧循环若正卡在一次命中的处理里,会完整跑完一轮(开听、确认),其间与新循环同时在听。`on_wake` 开头与 FollowUp / ConfirmListen 两处开听之前先问 `wake_is_current(gen)`,不是现行的就发 `ListenEnded` 收尾不听。采集通道 `Disconnected` 那条路也补了 `ListenEnded`(又一条「收尾通知不能漏」)。
- **克隆草稿录音没人收**:录入 / 导入先把 wav 落盘、点保存才写库;中途关掉设置页,草稿永远留着。下次录入 / 导入时清掉 1 小时前的无主草稿(`CLONE_DRAFT_GRACE`)。
- **0 字节的缓存件被当命中**:合成写到一半被掐,留下空文件,之后每次念这句都「命中」一个空音频。`tts::cached_ok` = 存在且非空,三处判断都换。
- **多音字补丁改了,旧缓存照播旧读音**:克隆缓存键掺 `POLYPHONE_SUPPLEMENT` 的哈希。
- **`read_audio` 翻页重听**:每次带 `offset` 取下一页都重新解码 + VAD + 逐段 ASR(几分钟的歌要几十秒)。加 `LAST_HEARD` 单槽缓存,键 = 路径 + 起点 + 文件大小 + 改动时间。
- **天气缺值落成 0°C**:两个源里某天气温缺失时按 0 填,等于给模型喂「最高 0°C」。改成那一天整天丢弃。

### 工具与存储(全部属实,已修)

- **授权圈并发各弹一张卡**:同一轮并发的几路调用(批里 4 路)撞上同一个没授权的目录,各自去问用户。`ensure` 改成先免锁判一遍,要问才进 `Grants.asking` 异步锁、进去后重判;探盘放阻塞线程(网络盘 / 休眠的移动硬盘上 `canonicalize` 会卡住执行器)。测试 `concurrent_ensure_asks_only_once`。
- **`fs_find` 超时后线程还在扫**:扫描上限卡的是命中数,在大盘根上找一个稀有名字会把整盘走完;工具超时返回了,阻塞线程照跑。`walk` 加停止旗,future 被丢时由 `StopOnDrop` 立旗。
- **docx / pptx 解压没有界**:包本身有 50MB 闸,压缩比没有界。`OOXML_XML_MAX` 256MiB,pptx 按合计算。
- **`web_search` 会整体超时**:三源梯子最多花 30 秒,之后抓正文遇慢站再花 15 秒 × 3,超过工具超时 → 搜到的结果全丢。抓正文每条只给「工具超时 − 已用 − 3 秒」。
- **网页缓存没有上限**:只在读缓存时清过期。入库时也清,并封顶 32 页(挤最旧)。
- **没声明字符集的 GBK 页乱码**:`decode_html` 按声明(或默认 UTF-8)解出坏字节时,再试一次 GB18030,它解得干净才采用(声明写着 utf-8 实际是 GBK 的站也一并救了)。
- **嵌套块正文重复**:`<li><p>…</p></li>` 外层一遍里层一遍。改成只收每个块「自己的」文字(`own_text`)。
- **操作记录页列表拖明细**:`list_all` 把每批的 `ops` JSON(带文本快照,可达数 MB)全取出来只为数条数。列表查询给空串。

### 引擎(四条属实已修,两条不动)

- **分批提示会丢**:自检轮与分批提示撞在同一轮时让位给自检句,但那次提示已经计了数、之后不再补。`batch_hint_carry` 留到下一轮发。
- **回溯后计划槽不清**:被回溯掉的那几轮列的计划留在槽里,〔此刻〕接着带、HUD 卡接着显。`rollback_conversation` 清槽并发空快照。
- **批里一件 panic 整批丢**:工作池在同一个任务里轮着跑各件,一件 panic 把池带走,已跑完的结果一起丢。`run_item` 用 `catch_unwind` 接住,只算这一件失败;`select!` 加 `biased`,已叫停时排队的件不再起跑。
- **惯例把提醒回合的工具调用算进去**:每晚定时放故事的提醒,会被统计成「晚上常放故事」并记在上一个说话人头上。取料多带 event 行,`habits.rs` 跳过 event 行之后、下一条 user 行之前的工具行。原有取料测试的断言随之更新(event 行现在出,不带 content)。
- **不动 · 长回合裁剪切在提示句处**:提示句是长回合里唯一可用的裁剪边界,换别的切法有取舍,不是 bug。
- **不动 · `plan_set` 的 done 序号对不上**:工具本体无状态是有意设计(§6.5),序号由嗅探侧对着槽解释。

### 壳层(全部属实,已修)

- **工具 panic 带走整个回合**:这条比体检清单写的更重。清单原话是「转发循环在回合任务异常结束时补一条 Failed(防御性)」;顺着查发现 `turn.rs::run_tools` 根本不接 panic —— 工具是在回合任务里就地跑的,任何一个工具 panic,回合任务被 tokio 吞掉:不落库、不发终态,前端只认终态收尾,于是一直停在「在想」,停止钮也按不动(它等的 `cancelled` 永远不来)。两层都修:`run_tools` 用 `catch_unwind` 接住、当错误观察喂回模型(回合照常收尾),壳层转发循环在通道关闭而没见终态时补发 `Failed{Internal}`。测试 `a_panicking_tool_fails_alone`。
- **登录窗轮询认窗名**:关窗后 1.5 秒内又点开,上一轮轮询把新窗认成自己的 —— 两轮同时读 cookie,且上一轮的 5 分钟期限会提前关掉新窗。加 `LOGIN_GEN` 代次。
- **浏览窗 TTL 续命两头不对**:① 只在获得焦点那一刻续一次,用户停在窗里慢慢填表超过 10 分钟,窗在手底下被收;② 任何导航都续,会自动刷新的页(行情 / 直播 / 轮播广告)永不过期,置顶小窗一直挂着。改法:`focused` 旗由 Focused 事件维护,清扫每拍见它为真就续;导航续命看 `nav_renews(focused, 距上次有人用过多久)` —— 停在窗里恒算,否则只在「有人用过之后的一个 TTL 内」算。后一条没有做成「只有聚焦时才算」,是因为扫码登录时用户不碰窗、登录后的跳转应当照旧续命。

### 前端(全部属实,已修;预览里用假后端实测)

- **在飞工具步骤存的是裸对象**:`wang.trace.items.push(step)` 之后 `liveSteps` 里留的是原件,回填 result / status 改的是原件,Vue 不知道。实际影响比清单写的小 —— 回合在飞时在飞时钟每 200ms 触发一次重渲染,多半会顺带刷出来;但那是碰巧。改成 `reactive()` 建。实测:展开着的那一步,finished 事件后结果当场出现。
- **本回合用量全程只有 state 上一份**:切走期间的轮次不进账(只记屏上这轮),在别的会话发一句又把它清零,切回来接着往别人的数上加,钉到气泡上的读数是错的。改成每回合自己的 `turnUsage`(闭包内,在不在屏上都记),`state.usage.turn` 只是视图。实测:会话 1 在屏记 100 → 切走后台记 200 → 会话 2 发一句记 7 → 切回会话 1,灯带 300、钉到气泡上 300。
- **睡眠定时在暂停中到点**:原注释写「一按播放下一拍就淡出,没差」—— 隔几分钟确实没差,隔一夜就是「早上一按播放又被停掉」;切歌同理(定时跨集保留)。判据没用时长阈值,用的是「谁动的」:过点之后用户**亲手**恢复 / 切歌 = 定时作废;自动接下一首不算(换曲、卡顿的间隙跨过定时点,人多半已经睡了,下一拍照旧停)。前端 `resume()` 一处,core `advance` / `jump_to_episode` 一处,`auto_next` 不调。实测:过点 → 清掉并通知 core;没到点、「放完这一首」两种都不动。
- **会话列表键盘够不着、焦点环是浏览器默认色**:列表项加 roving tabindex(Tab 只停当前会话)、↑↓ / 回车 / 空格;全局 `:focus-visible` 用 accent。实测:从搜索框按 Tab 落在当前会话上,环是 `rgba(95, 200, 255, 0.75)`、画在内侧。

### 核实后没动的(8 条)

- 时钟行对缓存的影响:开发机没有 11-06 之后的数据,量不出(见上一节)。
- 长回合裁剪切在提示句处、`plan_set` done 序号:见「引擎」。
- 文档续读每页重抽:代码注释明写不建抽取缓存,是当时的取舍。
- 钉钉长回复分片:单条上限离线核实不了,不凭猜测加切分。
- 本地 TTS 模型常驻内存:设计取舍(换来不用每句重载),要不要闲置释放得拍板。
- 恢复留下的保险副本(`larkwing.db.pre-restore-*`)不自动清:删用户数据的事,要拍板。
- 浏览窗快照回传受页面 CSP 限制、Windows 上页面自动出声:都只是高度怀疑,Mac 上验不了,修法要动 `cfg(windows)` 代码或换回传通道。记进 PLAN 真机项。

### 验证

core lib 904 / engine 23 / 壳层 13 全过,全工作区 clippy `-D warnings` 零警告,check-locks 六条过,i18n 6/6,vue-tsc 无报错。前端四条在预览里用假后端逐条实测(见上)。Windows 真机一项没验。

## 十件一批(2026-11-12)

用户在上一轮「还有什么值得做的」的清单里圈了十件:小而实的 1–5、提醒 / 汇报回合的回复流式上屏、常驻提示词瘦身,外加三条拍板(恢复保险副本留最近 3 份 / 到点提醒撞上会话忙 / few-shot 规则与数据不符)。下面按件记:原先怎样、改成怎样、怎么验的、哪里只能真机判。

### 1. 启动失败没有任何提示

**原先**:`lib.rs` 的 setup 闭包里有十几处 `?`(建数据目录、注册插件、开库、装 provider…),末尾是 `.run(…).expect("error while running tauri application")`。读 tauri 2.11.2 源码(`app.rs` 的 `RuntimeRunEvent::Ready` 分支):setup 返回 Err 时框架直接 `panic!("Failed to setup app: {e}")` —— 根本走不到那个 `expect`。正式版是 GUI 子系统、没有控制台,panic 的字无处可去,表现就是双击没反应。

**改成**:setup 闭包的正文整段挪进 `fn assemble(app) -> Result<…>`(Python 脚本按行搬、统一去一层缩进;搬之前查过正文里没有跨行字符串、没有缩进不足的行),setup 里只剩 `catch_unwind(|| assemble(app))`:Err 与 panic 都交给 `startup_failed`;`run()` 自己返回的 Err(WebView2 运行时缺失这类)同样接。`startup_failed` = 记日志 → `rfd::MessageDialog` 弹框(原因取错误的整条 source 链、超 1200 字截断并注明;日志位置按数据根解析给)→ `exit(1)`。`rfd` 是 dialog 插件自己的依赖,版本与特性照抄它的声明,依赖树里是同一份。

**文案**:这时前端没起来、库也可能没开(读不到界面语言),只能壳层硬编 —— 中英各一句,不带助手名。

**怎么验的**:三条单测(错误链 / panic 负载 / 正文截断);另用 `HOME=/var/empty` 起开发版造了一次真失败(log 插件初始化 `Operation not permitted`),进程停在对话框里(采样栈里有 `startup_failed`,stderr 有 `CFUserNotificationDisplayAlert … will block waiting for a response`)。Windows 上弹不弹、长什么样归真机。

### 2. 更新通道的单点

**核实**:读 tauri-plugin-updater 2.10.1 `updater.rs` 的端点循环 —— 连不上、非 2xx、JSON 形状不对这三种都会换下一个地址;唯独「回了 2xx 而正文不是 JSON」是 `res.json().await?`,整个 `check()` 当场返回 Err。`tauri.conf.json` 的 endpoints 顺序是两个第三方镜像在前、GitHub 官方在后(国内直连不稳,2026-06-30 用户拍板)。镜像回一个拦截页 / 验证页(200 + HTML)时,排在后面的地址一个都不会试,而自动检查失败只进 console —— 所有人静默收不到更新,而更新是发修复的唯一通道。

**改成**:壳层新模块 `updatecheck.rs`。`update_check` 读 conf 里的 endpoints,逐个 `updater_builder().endpoints(vec![一个])…check()`,怎么失败都记一行 warn 换下一个;第一个给出明确答案(有新版 / 已最新)的算数。查到的 `Update` 放进**这个 webview 的资源表**、返回与插件 `check` 命令同形的元数据,前端 `new Update(meta)`,下载 / 安装仍是插件命令 —— 版本比较、平台匹配、minisign 校验一样没绕。

顺手补了下载那一半:清单里每个平台只有一个下载地址(CI 写死成一个镜像),那个镜像不通原先只有「开了代理就带代理重下」一条退路。`update_retarget(rid, alt)` 把下载地址换个入口(剥掉镜像前缀拿到官方原址,再套上 endpoints 里其余入口的前缀),前端在第一次下载失败后各试一次。换入口不降安全性:包照旧过签名校验。

**没解决的**:镜像若回的是**过期的**清单(缓存),第一个答案就是「已最新」—— 这与原先行为相同,延迟一个缓存周期。

### 3. 备份带上聊天图片与收件区

**原先**:`backup_to` 只收库快照与克隆音色。聊天里的图(`media/attachments/`,内容寻址)和收到的文件(`media/inbox/`)不在包里 —— 换机恢复后老对话的图全是破图、收到的文件也没了。`stage_restore` 早就写了「不认识的条目跳过」,所以加成员对老版本是向前兼容的。

**改成**:备份多收这两个目录(都是平铺的;只存不压,它们多半已是压缩格式;单个文件被别的程序独占读不了 → 记日志跳过,不让一张图拦住整份备份)。恢复时这两类**只补缺、不覆盖**:图片同名即同内容;收件区同名的留现有那份 —— 恢复不替用户删文件。条目只认恰好三段的 `media/<attachments|inbox>/<名>`。

**一个当时没问出口的取舍**:自动备份也带上了(每周一份、留 10 份),图片多的家庭备份盘上会多占「图片体积 × 10」。事后问了用户,答复「备份带图吧」—— 维持现状;真嫌占地方再给 `backup_to` 加范围参数、让自动备份只带库。

### 4. 恢复前的保险副本只留最近 3 份(用户拍板)

每次恢复把现库挪成 `larkwing.db.pre-restore-<时间戳>`,原先永不清,每份一整个库。`prune_restore_safeties` 按名字里的时间戳留最新 3 份,其余连 `-wal` / `-shm` 一起删(直接删、不进回收站,与自动备份轮转同口径)。两处调:恢复落位成功后;没有待恢复负载的普通启动(老用户手里已经攒下的也收掉)。`rollback_restore` 要用的那份恒是最新的,必在保留之列。`restore-failed-*`(恢复后打不开的坏库,留着备查)没动。

### 5. 同会话同时到期的多条汇报并成一个回合

**原先**:调度器逐条 `wake_turn`。四路分头办事前后脚跑完 → 第一条起回合,第二条撞「会话忙」等下一拍(≤30 秒),依次串行;每个回合都带整段历史。

**改成**:`scheduler::report_mates` 在本拍到期清单里挑出与领头那条同会话、同说话人、没过宽限的其余汇报;`wake_turn_with(job, &also)` 给每条各落一条 event 行(payload 都是 `kind=report`),`assemble_request` 把它们各翻成一条「【后台的活忙完了】…」,一个回合看全。回合的锚点(用量流水、渠道「这次 event 行之后说的话」)= 最后一条 event 行;回合没起来时这次落的 event 行全撤。成不成这几条在本拍都不再单独处理(挡回来的下一拍一起重试)。

集成测试 `simultaneous_reports_for_one_conversation_merge_into_one_turn`:同会话三条 + 另一个会话一条 → 行序 `event, event, event, assistant`,另一个会话 `event, assistant`,四条任务都算触发过。

### 6. 到点提醒撞上会话忙(用户拍板)

**原先**:`wake_turn` 的忙检直接返回 false,调度器每 30 秒重试;回合一直在跑就一直推迟,超 2 小时标 missed —— 全程无声。

**方案里最要紧的一处**:不能在到点那一刻直接往库里插 event 行。在飞回合的行是「assistant(tool_calls) → tool 结果」分两步落的,插在两者之间,下一回合回放时配对就断了(严格端点 400)。所以走现成的**插队队列**:`Engine::queue_event` 往在飞回合的 `InjectState` 里推一条 `event: true` 的项,回合在轮间 / 收尾前排空它 —— 落成 event 行、往 request 里追加的是 `event_injection(content)`(与历史回放翻译 event 行逐字节相同,前缀缓存不破)、通告 `TurnEvent::EventLine`。

**「到点必有动静」不靠回合走到轮间**:排进去的同时立刻发 `ConversationActivity { kind: reminder, outcome: Queued, text }`。桌面收到就念(沿用到点自动开口的闸:正看着的桌面会话、没关朗读;正在念别的就不打断 —— 那个回合稍后会转述);渠道的 `outbound_loop` 把原文推回手机(这时 event 行还没落库,所以原文随事件带)。

排不进的两种(旁听仲裁回合不收插队 / 回合正在收尾)照旧返回 false 等下一拍 —— 回合马上就结束,下一拍就是普通的到点回合。汇报不走这条。

集成测试 `reminder_due_while_busy_is_queued_into_the_running_turn`:慢回合在飞时提醒到点 → 立刻收到 Queued 事件(带原文)→ 回合里先把原来那句说完、EventLine、再说一段、Done → 行序 `user, assistant, event, assistant`。前端在预览里验过:系统线插在两段回复之间;Queued 那一声念的是「到点了:…」、不刷列表不打标。

### 7. few-shot 规则与数据不符(用户拍板:规则认现状 + 加守卫)

AGENT §6.5 写「每场景 2–4 段、≤800 token」,`companion.json` 实际 15 段 62 条,`Scene::validate` 只查白名单与配对。量出来(`Scene::few_shot_size`:一段 = 一条 user 起头的一组;字符 = 正文 + 工具调用的名字与参数 + 工具结果)是 **15 段 / 2175 字符**。闸定在 16 段 / 2400 字符(现状加一成),超了 `validate` 不过;另有一条断言防「闸被悄悄放宽到形同虚设」(闸比现状宽出两成以上就红)。

### 8. 自启回合的回复流式上屏

**原先**:提醒 / 汇报回合没有前端 Channel,`wake_turn` 自己把事件流消费掉,只在收尾时发一条 `Conversation` 事件,前端整页重载。2026-11-11 加了起跑登记(停止钮 / 排队区成立),回复仍是「啪」地一下出现。

**core**:消费端加 `TurnRelayer`,把每条 `TurnEvent` 按序发到全局车道 `AppEvent::Turn { conv_id, seq, event }`。两处讲究:① 逐 token 发会把有界 256 的车道塞满、别的订阅者跟着丢事件 → 相邻的文字增量并成一条、相邻的思考增量并成一条(两种不混),至多攒 60ms,别的事件到来先把攒着的发掉(顺序不乱);② `event` 用已序列化的 JSON 值装 —— 总线若直接引用 `engine::TurnEvent` 就是底层模块反过来依赖 engine。壳层转发时这类事件只 `emit_to("main")`,悬浮窗不画聊天流。

**前端**:没有另写一套事件处理。`send()` 加了一个**自启形**参数:不出用户气泡、不调 `sendMessage`、恒按后台形起步(在飞气泡先游离着),把原来传给 `sendMessage` 的那个回调具名成 `onTurnEvent` 存进 `selfStreams`;全局车道上来的 `turn` 事件按会话喂给它。于是逐字上屏、工具步骤、用量读数、失败气泡、停止、切走再切回接上,全是普通回合那套代码。起跑那一拍若正看着这个会话,先重载一次把系统线带上屏,第一条事件到时 `reattach` 把在飞气泡接到它下面。

**丢事件怎么办**:车道是有界广播,积压会丢。`seq` 从 0 连续,前端发现断号就记 `lossy`;收尾事件到时,流是完整且已见终态的 → 什么都不用做(只发排队的话);否则作废这条流、走原来的整页重载对账。终态事件也丢了的,原有的 3 秒对账节拍(问 core 这个会话还在不在飞)兜住。

**念**:到点提醒改成边流边念(闸不变:正看着的桌面会话、没关朗读),收尾不再整段念第二遍;汇报不念。后台起的回合不掐正在念的话。

**预览实测**(假后端造事件,见记忆 preview-fake-tauri-backend):流式上屏、工具步骤在飞可见且结果回填、在飞时打字进排队区、收尾后发出;无间隔收尾 = 只在起跑重载一次、气泡钉到落库 id、轨迹留着、只念一遍;跳一个序号 = 收尾多重载一次且内容与库一致;不在该会话 = 只打标不念,切回去重载即见。

### 9. 常驻提示词瘦身

**先量**:`tools::tests::tool_prompt_budget` 打出出厂场景 64 个工具的体量表(描述 + 参数 schema,按字符)—— 合计 30798,描述 14058、参数 16740;前 12 个占 45%。LAWS 只有 3231,few-shot 2175。也就是工具面占稳定前缀的八成。

**读完前 12 个的结论**:它们本来就很密,几乎每句都是约束(哪种情况用哪个参数、失败怎么说、哪些话不许编),不少句子背后是一次真机实锤。真正「白占地方」的只有两类:描述把各参数自己的说明又讲了一遍;结尾把前面逐项给过的触发语再列一遍。按这个口径收了三个:

- `web_render`:描述里逐个复述参数的那一长句收成「点、填、选、按键、提交、上传、翻页、返回(各见参数)」;read / save_pdf / confirm 的细节各回各的参数说明;「上传只传用户点名的文件」挪到 `upload_paths`。
- `media_control`:去掉结尾那串复述的触发语;`value` 参数不再把每个动作的数值含义重讲一遍。
- `watch_set`:metric / op / value 的释义各参数自己有(且有 enum),描述里不再重讲。

合计 30798 → 30172(−626,约 2%)。**再往下删就是拿行为换字数**(把教程挪进按需取的技能 = 赌模型会去取),没动。真要大幅降,杠杆不在措辞而在结构:不常用的工具(打印 / 启动项 / 二维码 / 关机 / 声音明信片…)改成按需加载 —— 那是回合循环的设计变更,且工具清单一变前缀缓存就失效,要单独议。

**真模型基线(改动前的程序,DeepSeek deepseek-v4-pro,11 个场景各 3 次)**:9 个 3/3;`download-batch-confirm` 2/3(一次没先问就调了 media_download)、`batch-many-long` 2/3(一次没用 batch)。也就是说这两条守卫在改动**之前**就不稳。这一轮程序估算约 3.1 美元(输入 228 万 token,96% 命中缓存)(这是程序的估算:`catalog::est_cost_usd` 把缓存命中的 token 也按全价算、宁可高估;这一轮 96% 命中缓存,实际账单应明显更低,以 DeepSeek 控制台为准)。

**因此撤回了一处**:原本还把 `ffmpeg_run` 结尾的分批句缩短了(省 56 字符)。batch 的遵循度本来就在边上,为 56 个字去削那句话不值,改回原样。剩下三处改动(web_render / media_control / watch_set)没有任何评测场景覆盖,再跑一轮(估算又是约 3 美元)换不来信号,**改动后没有再跑真模型**;行为归真机清单。

**守卫**:`TOOL_SURFACE_MAX_CHARS` 31000(现状加约 3%),同一条测试里断言;新加工具把它顶破时,先看表收旧的。

**改写时踩的一个坑**:Rust 字符串的行尾反斜杠续行会吃掉下一行的前导空白 —— 脚本折行时若正好折在空格前,那个空格就没了(「ffmpeg 的」变「ffmpeg的」)。发现后改成「折点后一个字符不是空格才折」,并拿「HEAD 版本展平 → 套同样的替换」与「新源码展平」逐条比对相等。

### 10. 零碎五件

- **克隆音色改名**:命令 `rename_voice_clone` 早就有,一直没界面。音色小卡旁加 ✎,点了变输入框,回车 / 失焦保存、Esc 取消(Esc 要拦住,不然整个设置页跟着关)。
- **播放条到头置灰**:`PlayerBar.vue` 里 `wraps` 早算好了没用上;「放完就停」下第一首的 ⏮ / 最后一首的 ⏭ 置灰(与视频浮层同一判据)。
- **Telegram 白名单一键放行**:见 AGENT §7.7。onboarding 话术同步改成「在电脑上点一下就能放行」。
- **钉钉群聊设提醒如实退回**:见 AGENT §7.4。
- **删 `lottie-web`**:零引用。

顺带发现、没动的一处:**手机对话里用 task 模式设的定时任务**,到点回合跑在新建的系统会话里,那个会话没有渠道映射 → 结果只出现在电脑上,推不回手机。记在 PLAN §3。
