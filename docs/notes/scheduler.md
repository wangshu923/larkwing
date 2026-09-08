# 提醒 / jobs — 决策记录与破案叙事

> 本文件是 2026-09-07 AGENT.md「分家」时从规则总纲搬出的段落,**逐字照搬、未改写**;管「为什么这么定 / 怎么发现的 / 当时的状态与验收欠账」。
> **规则以 AGENT.md 为准**(每段在 AGENT.md 里留有规则句 + 指向这里的指针)。本文件只追加不改写:新决策照此格式加一节(标题 + 来源 + 原文)。查规则出处时 grep 要连同 `docs/notes/` 一起扫。

## jobs 底座 + 「设提醒 → 到点」四步可见 + 忙检 is_finished

> 来源:分家前 AGENT.md「7.4 提醒 / jobs / web(PLAN §10)」L347。

- **jobs 底座**:`jobs` 域 + `scheduler`(30s 轮询,无 cron 框架;错过宽限 2h——once→missed、**重复任务推进到未来不补发**(防开机轰炸);触发即推进 = at-most-once)+ `engine.wake_turn` 自启回合(event 行落库;目标会话在飞则跳过本 tick 绝不打断;经全局事件车道发动静)。**「设提醒 → 到点」四步在聊天流全可见(2026-07-10 用户拍板,原「event 行 UI 不渲染」解除)**:① 用户说 → ② 设好 = **回执小票**「⏰ 已记下 · 时刻 · 重复」(数据来自「想了想」轨迹的 reminder_set,点击跳提醒页,`MainLayout::groupChips` 零后端;**排气泡末尾**〔推理轨迹之后〕;**remember 记了记忆同款**「记住了 · 事实截断」跳记忆页——同一套回执小票机制,加新回执 = groupChips 加一个分支)→ ③ 到点 = event 行渲染成居中「⏰ 到点了 · 内容」系统线(单行截断、全文进 title;交代"定好的安排叫醒了它")→ ④ 模型转述 = 普通气泡(**原「⏰ 提醒」trigger 标签已摘**,来源由 ③ 交代;engine `mark_triggered` 富化保留、前端不再消费)。副带修正:wake 回合失败时聊天流仍见 ③ 的系统线 = 到点必有动静(§3.5);措辞守 §3(「到点了」是人话,job/cron 不露)。浏览器预览 `?demo` 有全链样张。**忙检必须看 `join.is_finished()` 而非 `inflight.is_some()`(2026-07-04 真机 P0)**:正常收尾的回合不清 inflight 句柄(只有 cancel/新 send 才 take),只查 is_some → 会话聊过一次后所有提醒被「会话忙」永远跳过且零日志(skip 原是 debug 级,已升 info);重启侥幸 = sessions 是内存态。回归测试 `wake_turn_fires_after_completed_turn_in_same_conv` 守着。

## 跨人提醒 / 捎话 = reminder_set 加 for

> 来源:分家前 AGENT.md「7.4 提醒 / jobs / web(PLAN §10)」L351。

  - **跨人提醒/捎话 = `reminder_set` 加 `for`(人际路由,2026-07-11 用户拍板三件:跨人可以/转述形态/收不到人如实说)**:`for` 填家人名字(`outbound::find_member` 按家人页名字解析,查无/重名带名单退回)→ **收件人也在创建时刻物化成 conv_id**(与 mode 同哲学):落 TA 的手机对话(`outbound::resolve_phone` = resolve_target 带回线程),到点 wake_turn 在 TA 的会话里开口(带 TA 的记忆/称呼组织语言 = 转述形态)→ 既有 outbound_loop 推 TA 手机,**到点链路零改**。**捎话 = first_at 填当前时间的跨人提醒,零新工具**(scheduler 30s 节拍内送达)。TA 没连手机 → 创建时就如实退回(绝不设「到点没人收」的提醒);`for` 只配 remind 不配 task。`jobs` 加 `created_by`(迁移 0022):收件人 = `user_id`(TA 可见可撤、提醒页标 TA 名),发起人凭 `created_by` 在 `list_visible`/`cancel` 里**也看得见撤得掉**(「我给爸爸设的我要能反悔」;工具列表标「给某某的/某某设的」);悬浮窗「下个提醒」仍走 `list_pending` 只看自己收件的。真机/真网待验(TG/钉钉端到端)。

