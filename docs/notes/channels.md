# 远程渠道(TG / 钉钉 / 微信) — 决策记录与破案叙事

> 本文件是 2026-09-07 AGENT.md「分家」时从规则总纲搬出的段落,**逐字照搬、未改写**;管「为什么这么定 / 怎么发现的 / 当时的状态与验收欠账」。
> **规则以 AGENT.md 为准**(每段在 AGENT.md 里留有规则句 + 指向这里的指针)。本文件只追加不改写:新决策照此格式加一节(标题 + 来源 + 原文)。查规则出处时 grep 要连同 `docs/notes/` 一起扫。

## 渠道会话管理:单聊 12h 轮换 + 映射历史行 + 悬空自愈

> 来源:分家前 AGENT.md「7.7 远程渠道:Telegram / 钉钉 bot(2026-06-17 落地;手机上跟旺财对话)」L420。

- **渠道会话管理(2026-07-13 用户拍板,取代原「回访同一 chat 恒续同一会话/映射到固定 conv_id」)**:每个单聊 chat 独立会话;**单聊闲置 12h 轮换**——chat 的会话 = 名下**最近有动静**的那个(判据 = 会话 `updated_at`,任意角色的最后一条消息;提醒刚到点的老会话算「有动静」→ 用户回「收到」接回它、有上文),超 12h 全凉才开新会话(老会话留桌面当历史);**群聊不轮换**(维持永久续接,支持群聊时再议;单聊判定:TG `chat.type=private`、钉钉 `conversationType`、微信 iLink 恒单聊)。**映射 = 历史行(append-only,迁移 0023 去 UNIQUE)**:改绑 = 插新行**继承指认/昵称/推送地址**,老行留档——轮换走的老会话 `thread_by_conv` 反查照样命中(**提醒推回手机 / 会话列表发起人标签不断链**,这是历史行存在的理由);`set_push_id`/`set_label` 按 (channel, ext_id) 全行保鲜(微信 context_token 每条消息在换,提醒落在轮换走的老会话时要拿最新的);指认 `bind_user` 追溯改全行;家人页 `list` 只出每 chat 最新行。**映射悬空自愈**:桌面删渠道会话(`delete_conversation` 不碰映射)→ 名下无活会话 → 下条消息自动重建重绑,修掉原「删会话后那个手机 chat 永久报错、无自愈」的坑。阈值单源 `channels/mod.rs::FRESH_CONV_IDLE_MS`(§4.11);解析收口 `resolve_conv`(时间注入可测)。Mac 全绿,三渠道真机/真网待验(PLAN watch-item)。

## 不按渠道注入 prompt + 出站富格式后处理(绝不 MarkdownV2)

> 来源:分家前 AGENT.md「7.7 远程渠道:Telegram / 钉钉 bot(2026-06-17 落地;手机上跟旺财对话)」L422。

- **不按渠道注入 prompt**(破前缀缓存,§7.5 同理)。**出站富格式 = 输出后处理(2026-07-08 落地,原「以后走输出后处理」解除;真机/真网待验)**:回复像 markdown(`channels/render.rs::looks_markdown` **保守探测**,漏判 = 发纯文本零风险)→ Telegram 转 **HTML parse_mode**(`render::to_telegram_html`,pulldown-cmark 解析、只产受控标签 b/i/s/code/pre/a/blockquote、**原始 HTML 一律转义成字面**、表格退化 `<pre>` 等宽;源文按 3200 切片再逐片转 = 标签永不跨片;转出超 4096 或发送 4xx → **该片降级纯文本重发**,§3.5 不静默)、钉钉发 **msgtype markdown**(渲染归钉钉客户端、语法宽容不拒收,title=`md_title` 首行摘要;batchSend 推送同判定走 sampleMarkdown)。**纯文本回复仍走老路零处理**(钉钉 markdown 会折叠单换行,短句寒暄别过)。**绝不走 MarkdownV2**(robot 全字符转义地狱)。回合回复与提醒推送共用同一出口。

## 手机端补全:提醒推手机 / 语音 / 照片 / 文档 / 看不了必回话

> 来源:分家前 AGENT.md「7.7 远程渠道:Telegram / 钉钉 bot(2026-06-17 落地;手机上跟旺财对话)」L423–L428。

- **手机端补全(2026-07-03,v0.2.4;原「MVP 纯文本」的入站半边解除)**:
  - **① 提醒推回手机**:`channels::run` 内 `outbound_loop` 订阅全局事件车道(§7.6 悬浮窗同款「又一个消费者」,**engine 零改**),见 `ConversationActivity{kind:"reminder"}`(提醒/盯天气 wake_turn 收尾)→ `thread_by_conv` 反查渠道映射 → **Done 推最后 assistant 回复;Failed 推 event 行提醒原文(截断)保底**(§3.5 到点必须有动静,哪怕回合没跑成)。TG=`sendMessage`(ext_id 即 chat_id);钉钉 sessionWebhook 有时效撑不到「到点」→ 走 `oauth2/accessToken` 现取 + `robot/oToMessages/batchSend` 单聊主动推(robotCode=appKey;收件地址=入站顺存的 `senderStaffId`,迁移 0018 `push_id`;**群聊不推**,群推送另一套 API 后续)。**不加开关**(强默认 §3);推送失败只 warn 不翻渠道状态。
  - **② TG 语音消息能听**:getFile 下载(20MB 上限)→ ffmpeg 组件解 16k 单声道 PCM(`MediaRuntime::decode_audio_pcm16k`)→ 本地 ASR(`VoiceRuntime::transcribe_pcm`,与听写/唤醒共用缓存识别器)→ 文本进 drive_turn(`input=voice_msg`,**speak 恒 false**——渠道回复是文字,〔语音〕说话守则不触发)。>60s(`VOICE_MSG_MAX_SECS`,本地 ASR 超长精度掉)/ ASR 未就绪(回「准备中」+ `prefetch_asr` 后台下,绝不卡收循环)/ 转写为空,**全部如实回话**。
  - **③ TG 照片能看**:取最大档下载 → 桌面同缝 `InAttachment`(图当轮注入不落库,§9);caption 当消息文字。vision 表现与桌面发图完全一致(含主模型无视觉时)。
  - **④ 看不了的不再已读不回**(§3.5):内容型消息(贴纸/视频/文件)回一句提示;服务性消息(入群/置顶)仍静默。**钉钉图/语音已补齐(2026-07-07,原「留后续」解除;真机/真网待验)**:downloadCode 经 `robot/messageFiles/download` 换直链下载(accessToken 复用提醒推送那套;20MB cap + 字节嗅探 MIME);语音**优先用钉钉自带 `recognition` 转写**(有就不下载、不占本地 ASR),没有才走 TG 同款 ffmpeg→本地 ASR 链(超长/未就绪/为空同款如实回话);**richText(文字+图混排)也收**(文字并成 caption + 取第一张图,纯文字当 Text)。**文档也能读(2026-07-08,file/document 半边解除;真机/真网待验)**:TG `document` / 钉钉 `file` 里**能抽文字的文档**(pdf/docx/pptx/xlsx/文本类;`attach::doc_supported` **下载前预检**,别白下 20MB 压缩包)与**按扩展名认出的原图**(「以文件发送」,`attach::image_mime_by_ext`)→ 下载走桌面同缝 `InAttachment`(`process_attachments` 自动分流:文档文字进 history §9、图当轮;扫描件等抽不出的落「读不出」占位、模型如实说);压缩包/视频文件等仍回「看不了」(UNSUPPORTED_HINT 话术已同步能收清单);TG 超 20MB 下载前如实回「太大」。video 消息仍回「看不了」。详见 PLAN §0.2.4 批次二。
  - **架构**:channels(边界适配器)**组合** voice/media 两个 core 运行时——壳层装配传入(`ChannelSup::new(engine, voice, media)`),voice/media 均不反向依赖 channels(§6.1 mod 边界不破);渠道操作性静态话术(§6.6 债)随之添了几条,仍不经模型。

## 入站附件攒批 = 文件与意图分离

> 来源:分家前 AGENT.md「7.7 远程渠道:Telegram / 钉钉 bot(2026-06-17 落地;手机上跟旺财对话)」L429。

- **入站附件攒批 = 文件与意图分离的通用解(A,2026-07-11 用户拍板;真机实锤:钉钉文件同微信,file 消息不带文字、文字随后另发一条隔十几秒)**:手机发文件多半不能同时打字(微信铁律;钉钉 file 实测同样;TG 能带 caption、钉钉图文 richText 能带)。**不按渠道分,按「这条附件消息带没带文字」分**:带文字 → 意图齐,直接处理(现状不变);**空文字的附件 → 先攒、不触发回合**,等用户发来文字连同一起进一个回合(治「每个文件独立触发、模型没意图瞎处理」)。机制 = `ChannelCtx` 持 `AttachBuffer`(per (channel, ext_id),三渠道共用;抽独立结构可单测):`buffer_attachments` 缓冲空→满返 true = 发一次提示(防抖:**连发多个文件只第一个吭声**,后续静默并入)、`take_attachments` 用户发文字时取走全部(超 `ATTACH_TTL`=30min 丢弃 + 攒新批时 retain 清僵尸,防 base64 bytes 长留内存)。三渠道接在**各自唯一出口**(微信 `handle_message`、钉钉 `run_reply`、TG `reply_turn`):`!atts.is_empty() && text 空` → 攒 + 提示 + return;否则 take 缓冲并入本次。提示话术 `ATTACH_HINT`(§6.6 债,硬编中文同 ONBOARD_HINT;用户选「极简功能」版)。**桌面不做**(用户在屏幕前、附件小票看得见、回复即时,拖文件不打字顶多模型追问一句,不值当插系统提示;攒批价值全在手机盲发)。守卫:`attach_buffer_debounces_and_merges` 单测(防抖/攒取/清空/分人分渠道独立)。真机/真网待验(尤其钉钉 file→text 的到达顺序:实测是 file 先、间隔十几秒,与「文件先攒、文字触发」一致;若撞到 text 先则退化成分开处理,再补反向)。

## 渠道归人 = 多用户第一步(主人锚定 id ASC / touch 只 touch 会话归属者)

> 来源:分家前 AGENT.md「7.7 远程渠道:Telegram / 钉钉 bot(2026-06-17 落地;手机上跟旺财对话)」L434。

- **渠道归人 = 多用户第一步(2026-07-03,v0.2.4;确定性归人先行,概率性的声纹后置)**:每条渠道对话(`channel_threads` 映射行)可在设置·家人**指认**给一位家人(`user_id` 列,NULL = 会话归属者、零行为变化);入站 `drive_turn` 据此带 `UserMeta.speaker_user` → **记忆 / 需知 / 工具(`ToolCtx.user_id` = mem_user,提醒随之)归说话人**,人格 / pet_name / 会话归属仍走会话归属者(前缀字节稳定,一家人轮流说不破缓存)。装配时该消息加确定性标记 `〔某某说〕`(与 `〔语音〕` 同款「payload 物化 + 装配标记 + LAWS『一家人』法条」;名字单源 users 表、每回合 id→name 现查,家人已删 = 不标不崩)。平台昵称(TG `from.first_name` / 钉钉 `senderNick`)只作家人页认脸 `label`,不参与逻辑。⚠️ **同族第三坑=根治(2026-07-04 真机):身份别按活跃度推断,锚死「最早建的用户=主人」**——`ensure_default_user`/`users.list` 从 `ORDER BY last_active_at DESC` 改 `id ASC`,当前用户/「(你)」标记与 last_active_at 彻底解耦(加家人、家人发渠道消息都不再让主人视角漂移);`touch` 仍在但对身份无影响(留给未来声纹)。回归测试 `family_creation_never_steals_boot_owner` 已扩「家人 touch 后仍是主人」。此前的「建号 last_active=0」是治标,本条是治本(活跃度不再参与身份)。 ⚠️ **同族第二坑(2026-07-04 真机实锤):`users.create` 建号绝不算活跃(last_active_at=0)**——曾落 now,加完家人一重启 `ensure_default_user` 按最近活跃把主人切成新家人的空白视角(会话/记忆/名字/性格「全没了」,渠道来一句 touch 主人才恢复);回归测试 `family_creation_never_steals_boot_owner` 守着。⚠️ **`users.touch` 只 touch 会话归属者、绝不 touch 说话人**——`ensure_default_user` 按 `last_active_at` 恢复「当前用户」,若 touch 说话人,家人在手机上说一句就成「最近活跃」,主人重启 app 会被切成 TA 的视角(2026-07-03 实改于 send_message,回归测试 `speaker_user_marks_context_and_keeps_boot_owner` 守着)。删家人时 `delete_user` 编排里连带 `channels.unbind_user`(指认回落,映射本身保留)。

## 出站文件 send_file(缺省说话人 / to 跨人 / channel 点名 / find_member 宽匹配)

> 来源:分家前 AGENT.md「7.7 远程渠道:Telegram / 钉钉 bot(2026-06-17 落地;手机上跟旺财对话)」L435。

- **出站文件 = `send_file` 工具 → `channels/outbound.rs`(2026-07-10 落地;「电脑 ↔ 手机」补齐出站半边,真网/真钉待验)**:模型能把本机文件/图片发到家里人的手机(**缺省 = 说话人**,`ToolCtx.user_id` = 渠道归人后的说话人:桌面喊「发我手机」= 主人,家人在手机上要文件 = TA 自己;**`to` 填家人名字 = 发给那位家人**——2026-07-11 用户拍板放开跨人,原「目标恒=说话人」作废;家庭信任模型,文件跨人外发不设闸)。目标解析 = 渠道归人映射**反着用**(`resolve_target`:指认给 TA 的线程算 TA 的,TA 是主人时未指认 NULL 的也算;多条取最新绑定;钉钉群聊无 `push_id` 跳过),找不到给明白话(§3.5)。**`channel` 参数点名渠道(2026-07-11 真机实锤补)**:一人多渠道时「最新绑定」让「发我微信」发去了钉钉 → send_file 加 `channel`(telegram/dingtalk/weixin,用户点名才填),`resolve_target/resolve_phone` 按渠道过滤;工具 description 的渠道清单**必须随新渠道同步**(微信接入后 description 仍写「Telegram/钉钉」→ 模型如实说「发不了微信」,发图入口白设同款教训)。**`find_member` 名字宽匹配(同日实锤)**:家人页名字常带注释(「蛋蛋(就是妈妈)」),精确相等把「妈妈/蛋蛋」全拒 → 精确优先、无命中放宽到「家人名包含所填」,唯一命中才取、多命中如实报名单(send_file 的 to / reminder_set 的 for 共用)。TG = `sendDocument` multipart(≤50MB,一律按文档发保真,reqwest 加 `multipart` feature);钉钉 = **旧接口 `media/upload` 换 mediaId(只认 `gettoken` 旧 token)+ 新 token `batchSend`**(sampleFile;**图片走 sampleImageMsg**——sampleFile 只认办公文档扩展),新旧 token 混用是官方文档姿势、≤20MB;**钉钉文件不支持附言 → note 走文件送达后补推一条文字**(补推失败只 warn 不翻整单,文件确实到了)。凭证走 secrets、`Target` 的 Debug 只露渠道名(token 绝不进日志)。工具面 = 「tools 依赖 channels 的单向引用」新先例(channels 不认识 tools,不构成环;outbound 不要求 Engine)。

## 出站文字 send_text

> 来源:分家前 AGENT.md「7.7 远程渠道:Telegram / 钉钉 bot(2026-06-17 落地;手机上跟旺财对话)」L436。

- **出站文字 = `send_text` 工具(2026-08-12 落地;真机实锤「转发一句话/链接到我的微信」被答只能发文件——出站原语只有 send_file,缺文字姊妹)**:把一句话/链接**原样**发到说话人或家人的手机;`to`/`channel` 语义与 send_file 完全同源(缺省说话人、`find_member` 宽匹配、点名渠道过滤),机器件 = `outbound::send_text` 汇三渠道**既有** push 出口(TG 富格式+分片 `telegram::push` / 钉钉 `dingtalk::push` batchSend sampleText·sampleMarkdown / 微信 `weixin::push` 带过期令牌降级)——零新协议代码。**与「捎话」分工进工具描述**:要转述/到点提醒走 reminder_set(for),原文照发走这里;send_file 描述补一句「只发文字用 send_text」路由线。**微信窗口关死同享挂起补发**:`PendingSend` 挂起件扩 `text` 条目(serde default **向后兼容**老文件件;同目标同内容去重;TA 开口 flush 时文字直接 push 补发,与文件混挂按序),话术同 send_file「已挂起、开口自动补送、不用重发」。超长(>2000 字,与提醒物化内容同量级)如实退回指路「写成文件走 send_file」。Mac 全绿;三渠道真发一句话/链接 + 模型路由不串(链接→send_text、文件→send_file、捎话→reminder_set)= 真机 watch。

## 微信 = 腾讯官方 iLink bot 渠道(多绑定 / context_token 挂起补发 / 媒体加解密)

> 来源:分家前 AGENT.md「7.7 远程渠道:Telegram / 钉钉 bot(2026-06-17 落地;手机上跟旺财对话)」L437–L441。

- **微信 = 腾讯官方 iLink bot 渠道,已接(2026-07-11 用户当面拍板「全做」,推翻 2026-06-17「微信暂不做」)**:原拒因「个人微信无官方 API、企业微信需公网回调」——**前提已变**。腾讯 WeixinTeam 发的「微信 ClawBot」(`@tencent-weixin/openclaw-weixin`,MIT)连的是腾讯官方 **iLink bot HTTP API**(`https://ilinkai.weixin.qq.com/ilink/bot/*`):扫码把 bot 绑进个人微信,**出站长轮询 + 免公网**,形态与 Telegram 一模一样,正合家用约束。**我们不装那个 npm 插件**(它是 Node.js、插 OpenClaw CLI 的),照对 Telegram 的姿势——**不引 SDK、用 `net::Client` 裸接协议**(它 MIT 全量 TS 源码当规格书白读;协议不受版权保护)。探针实证(2026-07-11):非 OpenClaw 客户端只带 `iLink-App-Id: bot` 头即可拿到二维码(无 per-app 密钥/无 allowlist),故可自接。
  - **协议同构 telegram.rs**:收 = 长轮询 POST `ilink/bot/getupdates`(游标 `get_updates_buf` 字符串,对应 TG offset);发 = POST `ilink/bot/sendmessage`(`item_list:[{type:1,text_item}]`);鉴权 = 扫码拿 `Bearer <bot_token>` + 固定头(`iLink-App-Id: bot` / `AuthorizationType: ilink_bot_token` / `X-WECHAT-UIN` 随机 uint32 base64 / `iLink-App-ClientVersion`)。协议常量(base/cdn url、app-id、bot_type)是**协议事实**(同 `telegram::API` 常量),单源 `weixin.rs` 顶部、非 §4.11 产品默认。
  - **比 TG 多出来的三件**:① **扫码登录**(唯一真·新活)——POST `get_bot_qrcode?bot_type=3` 拿二维码 URL → 长轮询 GET `get_qrcode_status` 过 wait→scaned→(可能 need_verifycode 手机上输配对码)→confirmed 拿 `bot_token/ilink_bot_id/baseurl/ilink_user_id`;IDC 重定向(`scaned_but_redirect`→`redirect_host`、confirmed→`baseurl` 作后续 base);二维码 core 用 `qrcode` crate 渲染 SVG 给设置 UI。**★多绑定(2026-07-11 真机实锤定形:iLink bot 是「一人一 bot」——扫码 = 给那个微信号申请专属 bot 实例;首版单值 token 让第二个人扫码顶掉第一个、旧 bot 无人轮询显示「无法连接」)**:绑定列表 `remote.weixin.accounts`(secrets JSON 数组 `{token,base_url,user_id}`,同 llm.providers 整块进 keyring;旧单值 `remote.weixin.token` 惰性迁移成无身份账号、留读不写)→ `weixin::run` 每账号一路长轮询(per-account 游标 `updates_buf.{user_id}`)、扫码 = `append_account`(同 user_id 重登替换 / 新身份追加 / 顶替无身份迁移账号)、出站(reply/提醒推送/send_file)按线程 ext_id(= 绑定者)`account_for` 选账号(唯一账号兜底);放行 = 手动白名单 ∪ 全部绑定者(**扫码不再写白名单**——首版扫码追加白名单让用户以为名单被顶换,「额外允许」框回归纯手动);设置 UI = 绑定列表每行可解绑(命令 `weixin_accounts`/`weixin_unbind`)+「再绑一个(扫码)」。② **`context_token` 回显**——每条入站消息带的会话令牌,回复/推送必须原样回传;**复用 `channel_threads.push_id` 列存最新 context_token**(其语义正是「出站收件地址」,零迁移),`to_user_id` = `ext_id`(= from_user_id)。**令牌会过期(2026-07-18 真机实锤)**:拿存量令牌主动发(send_file / 提醒推送)撞上过期 → 服务端拒 `sendmessage ret=-2 errmsg=prepare failed`(不是文档预期的 -14「会话过期」;重试 / 重启渠道都救不回,只有 TA 再来一条消息才刷新;佐证 = hermes-agent PR#62386 / octo-agent PR#1575 同错)→ `send_item` 内建降级 = **去掉令牌原样重发一次**(无令牌发送是官方插件合法路径,openclaw 缺令牌仅 warn 照发),仍失败才如实退回并提示「让 TA 先发一句话」;响应判定 **ret / errcode 双字段**任一非 0 = 失败(只看 ret 会把 errcode-only 的失败当成功,误报「发出去了」)。**降级真机证否(2026-07-21)——iLink 对无令牌发送一样回 -2**:会话窗口 = 平台反骚扰设计(bot 只能在「对方最近说过话」窗口内主动开口),协议里无任何令牌申请/刷新 API(`context_token` 只随入站消息来,翻遍 openclaw 全量类型证实)→ 客户端无绕法,治法 = **挂起补发**:双拒错误带 `weixin::StaleContext` 类型标记(anyhow 链可 downcast),send_file 工具据此把文件挂 `remote.weixin.pending_sends`(settings KV;`PENDING_TTL_HOURS`=48h 单源;同目标同文件去重防模型重试重复挂),工具结果 **Ok 如实说**「已挂起、TA 一开口自动补送、不用重发」并点名是谁(§7.1「需登录 ≠ 失败 + 自动重放」同哲学);收循环 `handle_message` 过白名单拿到新令牌先 `flush_pending_sends`(成功 / 文件已删 = 出列,送失败回炉下条再试,TTL 兜底;`PENDING_MU` 防工具层挂起与收循环补发丢更新)。**提醒推送刻意不挂**(时效敏感,晚到的「该出门了」比不到更误导)。③ **媒体 AES-128-ECB**——CDN 媒体走 `full_url` 下载 + AES-128-ECB(PKCS7)解密喂 `InAttachment`;语音先白嫖 `voice_item.text`(服务端 ASR),SILK 解码后置(ffmpeg 不支持 SILK,记 watch-item)。出站文件 = `getUploadUrl`(md5/密文尺寸/random aeskey)→ AES 加密 PUT CDN → `x-encrypted-param` 回填 item。
  - **账号风险如实记档**:虽是腾讯官方渠道(远比 itchat/wechaty 安全),但个人微信绑自动化 bot 仍有平台政策残余风险;且以 `bot_type=3`/`iLink-App-Id: bot` 的「OpenClaw 身份」连接 = 借它的壳。风险不高、非零。
  - **接线全镜像 TG/钉钉**:`mod.rs` 加 `mod weixin` + 开关 spawn + outbound_loop 推送臂;`secrets.rs` `SECRET_KEYS` 加 `remote.weixin.token`;`remote_status` 加行;设置·远程加卡 + i18n。新增非秘密键 `remote.weixin.allowed_users` / `remote.weixin.base_url`(走 `remote.*` 通用臂,§6.8)。**Mac 编译绿 + 单测 + 预览,真微信扫码/收发/媒体 = Windows/真网/真号 watch-item(PLAN)**。

