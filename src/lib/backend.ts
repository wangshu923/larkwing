// IPC 封装:类型与 commands 一一对应(契约见 PLAN §5)。
// 这是前端唯一 import @tauri-apps/api 的地方。

import { getVersion } from '@tauri-apps/api/app'
import { Channel, invoke } from '@tauri-apps/api/core'
import { emit, listen } from '@tauri-apps/api/event'
import { currentMonitor, getCurrentWindow, LogicalPosition, LogicalSize, Window } from '@tauri-apps/api/window'

// ---- 与 Rust 端 serde 输出一致的类型 ----

export interface User {
  id: number
  name: string
  skin_id: string
  created_at: number
  last_active_at: number
}

export interface Conversation {
  id: number
  user_id: number
  scene_id: string
  title: string
  /** 渠道(会话级):ui(默认/不标) | voice | system | 未来 telegram/dingtalk/slack。列表按此渲染小图标。 */
  channel: string
  /** 钉住:用户右键标记的「没聊完」会话,列表排最前 + 挂 📌。 */
  pinned: boolean
  created_at: number
  updated_at: number
  /** 发起人显示名(engine 富化):渠道指认的家人 / 非主人发起者;主人自己的会话 = 无(是「你」)。 */
  owner_name?: string | null
}

export interface Message {
  id: number
  conversation_id: number
  role: string // 'user' | 'assistant' | 'tool'
  content: string
  created_at: number
  /** 工具轮附加数据(JSON,engine 私有词汇);UI 只用它判断"这行别渲染气泡"。 */
  payload?: string | null
  /** 说话人显示名(engine 富化):user 行说话人非会话归属者(家人 / 声纹 / 渠道)时有;主人自己 = 无。 */
  speaker_name?: string | null
  /** 触发来源(engine 富化):assistant 行由提醒 / 定时任务自动触发时为 'reminder';普通回复 = 无。 */
  trigger?: string | null
}

/** 聊天搜索命中(跨会话):带会话标题/渠道供列表展示;snippet 是截断的展示片段。 */
export interface SearchHit {
  conversation_id: number
  conversation_title: string
  channel: string
  role: string
  snippet: string
  created_at: number
}

/** 小本本一条(回忆页);kind: fact/profile/summary(宪法 §6,细化 TBD)。 */
export interface Memory {
  id: number
  user_id: number
  kind: string
  content: string
  /** 是否进稳定前缀(画像·常驻层);false = 按需层,靠 recall 取。PLAN §13。 */
  resident: boolean
  /** 强化分:取用 +1。 */
  salience: number
  /** 出处:explicit / correction / distilled。 */
  source: string
  /** 上次取用/确认(unix ms);null = 自建后未再取用。 */
  last_used_at: number | null
  created_at: number
  updated_at: number
}

/** 一行记忆维护观测(§13.7 调阈值用;后端 camelCase 序列化)。 */
export interface MaintenanceLog {
  decayed: number
  demoted: number
  promoted: number
  merged: number
  expired: number
  createdAt: number
}

/** 一件没办完的事(切片2 小账,回忆页第三分组):只列 open 态,勾掉即了结。 */
export interface Todo {
  id: number
  content: string
  created_at: number
}

/** 一天的家庭日记(「这些日子」,home 共有一本):date = 'YYYY-MM-DD'。 */
export interface DiaryEntry {
  id: number
  date: string
  content: string
  created_at: number
}

/** 家庭备忘一条(任务需知,PLAN §9):scope 'home' | 'user:<id>'。 */
export interface Briefing {
  id: number
  domain: string
  content: string
  scope: string
  resident: boolean
  created_at: number
  updated_at: number
}

/** 一批文件操作(操作记录页,PLAN §9 文件能力)。功能性历史,非安全承诺。
 *  kind: move|copy|mkdir|trash|write|append|edit;state: 'applied'(已生效)|'undone'(已撤销,可重做);
 *  ops = FsOpItem[] 的 JSON 串(前端不解析,只用 kind/n 展示)。 */
export interface FsOp {
  id: number
  user_id: number
  kind: string
  ops: string
  n: number
  state: string
  created_at: number
  updated_at: number
}

/** 一条提醒(提醒页,jobs 域)。模型把自然语言翻成绝对时刻 + repeat 枚举,用户永不见 cron。
 *  repeat: once|daily|weekdays|weekly;status 待触发恒为 'pending'(列表只取 pending)。 */
export interface Reminder {
  id: number
  user_id: number
  conv_id: number
  content: string
  /** unix 毫秒(本地时区换算后的绝对时刻)。 */
  due_at: number
  repeat: string
  status: string
  /** time(到点)| cond(条件触发);cond 的 due_at 是「下次检查时刻」,不当时间显示。 */
  kind: string
  created_at: number
  updated_at: number
  /** 家人的提醒 = TA 的名字(提醒页是主人的管理面,全家可见可撤);自己的没有此字段。 */
  owner?: string
}

export interface BootSnapshot {
  user: User
  conversation: Conversation
  messages: Message[]
  hasApiKey: boolean
  openingLine: string | null
  locale: string
}

export type ErrorKind =
  | 'no_api_key'
  | 'bad_api_key'
  | 'network'
  | 'api'
  | 'not_found'
  | 'internal'

export interface AppError {
  kind: ErrorKind
  message: string
}

// TurnEvent:tagged 编码 { type, data },未知 type 忽略(增量演化约定)
export interface SettingEntry {
  scope: 'app' | 'user'
  key: string
  value: string
}

/** 供应商卡片(钥匙只来掩码/引用,明文永不过桥)。 */
export interface ProviderView {
  id: string
  name: string
  protocol: 'openai_compat' | 'anthropic_compat'
  baseUrl: string
  model: string
  enabled: boolean
  builtin: boolean
  keyMasked: string
  keySet: boolean
}

/** 保存入参:省略的字段不动;apiKey 空串视同不改。 */
export interface ProviderPatch {
  id: string
  name?: string
  protocol?: string
  baseUrl?: string
  model?: string
  enabled?: boolean
  apiKey?: string
}

export type ModelTier = 'light' | 'balanced' | 'smart'
/** 计价方式:cached=按量+缓存(默认)/ uncached=按量无缓存 / percall=按次。影响压缩(留多少上下文)。 */
export type BillingMode = 'cached' | 'uncached' | 'percall'

/** 「高级」里某模型的用户覆盖;省略字段 = 用目录猜测(纠错语义,非配置)。 */
export interface ModelOverride {
  model: string
  tier?: ModelTier
  inUsdPerM?: number
  outUsdPerM?: number
  ctxWindowTokens?: number
  billing?: BillingMode
  /** 能不能看图;省略 = 用目录猜测(自架视觉模型靠它标上)。 */
  vision?: boolean
}

/** 目录对某模型的猜测(给「高级」占位用;null = 目录也不知道)。 */
export interface ModelGuess {
  tier: ModelTier
  inUsdPerM: number | null
  outUsdPerM: number | null
  ctxWindowTokens: number | null
  billing: BillingMode
  /** 目录猜的「能不能看图」(未知 = false)。 */
  vision: boolean
}

/** 设置页「高级」一格全貌:目录猜测(占位)+ 当前覆盖(值)。 */
export interface ModelMeta {
  guess: ModelGuess
  over: ModelOverride | null
}

/** 一轮 LLM 调用的消耗摘要;cost_usd null = 模型/价格未知,只报 token。 */
export interface UsageDigest {
  input_tokens: number
  output_tokens: number
  cache_hit_tokens: number
  cost_usd: number | null
  /** 本轮耗时(开流到收尾,ms)。 */
  elapsed_ms: number
  /** 首字延迟(ms);整轮没吐字 = null。 */
  ttft_ms: number | null
}

/** 今日累计(自然日);unpriced = 今日有估不出价的轮次,钱不是全貌。 */
export interface DayUsage {
  date: string
  input_tokens: number
  output_tokens: number
  cache_hit_tokens: number
  cost_usd: number
  unpriced: boolean
}

/** 一个聚合窗口(会话等)的合计;unpriced_rounds > 0 时 cost_usd 不是全貌。 */
export interface UsageTotals {
  input_tokens: number
  output_tokens: number
  cache_hit_tokens: number
  cost_usd: number
  unpriced_rounds: number
}

/** 历史/提醒气泡的 hover 读数(PLAN §11 D):一条 assistant 气泡 + 那回合累计用量。 */
export interface MsgStats {
  message_id: number
  ms: number
  input_tokens: number
  output_tokens: number
  cache_hit_tokens: number
  cost_usd: number | null
}

/** 「想了想」轨迹的一步(展开层):一次工具调用的技术细节。 */
export interface TraceStep {
  /** 工具名(原始,技术向)。 */
  name: string
  /** 人格化键(折叠摘要兜底)。 */
  ui_key: string
  /** 入参 JSON(原样)。 */
  args: string
  /** 结果(tool 行内容)。 */
  result: string
  /** ok|error|timeout|cancelled。 */
  status: string
}

/** 一回合的「想了想」轨迹(PLAN §9 思考漏出):展开层给好奇/专业用户看的技术细节
 *  (工具名/入参/结果 + CoT 原文)。load 会话后(及 trace-y 回合落库后)回填到代表气泡。 */
export interface TurnTrace {
  message_id: number
  steps: TraceStep[]
  reasoning?: string | null
}

/** 账户余额(供应商支持才有);amount 是供应商原文字符串,只展示不算术。 */
export interface AccountBalance {
  currency: string
  amount: string
}

/** 悬浮窗待机轮播数据(PLAN §12):只给 OS 没有的 —— 下个提醒 + 最近一句。字段 snake(同 Rust)。 */
export interface FloatReminder {
  content: string
  /** unix 毫秒。 */
  due_at: number
}
export interface CareCandidate {
  /** 关怀类型('resume' 继续看剧 / 'todo' 没办完的事);前端按它选 i18n 文案,未知 kind 忽略。 */
  kind: string
  /** 剧名 / 待办内容 → 填进 care.* 的 {title}。 */
  title: string
  /** 上次看 / 记下的时间(unix 毫秒)。 */
  updated_at: number
}
export interface FloatIdle {
  next_reminder?: FloatReminder
  latest_line?: string
  /** 主动关怀候选(PLAN ★主动关怀里程碑):继续看剧 + 拖最久的待办,各最多一条;care.enabled 关 = 后端不给。 */
  cares?: CareCandidate[]
}

export type TurnEvent =
  | { type: 'delta'; data: string }
  | { type: 'thinking'; data: string }
  // label = i18n 键(如 'tool.remember'),文案在字典;绝不露"工具"概念
  | { type: 'tool_use'; data: { label: string; state: 'started' | 'finished' } }
  // 记账灯带:本轮消耗 + 今日/会话累计快照(工具回合每轮一次;累计来自库,前端只展示)
  | { type: 'usage'; data: { round: UsageDigest; today: DayUsage; conv: UsageTotals } }
  // 插队(PLAN §9 B):回合在飞时注入的 user 消息已落库,之后的回复另起一段
  | { type: 'injected'; data: { message_id: number; text: string; attachments: AttachmentRef[] } }
  // 带文字的工具轮:这段回复在落库里是独立 assistant 行;前端封口当前气泡、另起新泡(结构对齐落库)
  | { type: 'segment'; data: { message_id: number } }
  // end_session = 本轮模型调过 end_conversation(§7.5 会话收尾):唤醒回合据此收窗回待唤醒、
  // 不开跟进窗。旧事件无此字段 → undefined 视作 false(维持开跟进窗的原行为)。
  | { type: 'done'; data: { message_id: number; end_session?: boolean } }
  | { type: 'failed'; data: AppError }
  | { type: 'cancelled' }

// ---- 全局事件车道(bus → "app_event"):回合之外的事 ----

/** 文案引用:key 进字典,params 是命名插值(core 不产文案铁规的线上形态)。 */
export interface TextRef {
  key: string
  params?: Record<string, unknown>
}

/** 失败任务的重放载体(tagged,镜像 Rust TaskRetry);只在可重试的 failed 任务上有。
 *  点重试 → 按入参直连重放,不绕 LLM(§7.1 按钮直连哲学)。 */
export type TaskRetry =
  | { type: 'media_play'; data: { page_url: string; audio_only: boolean } }
  | { type: 'download'; data: { component: string } }
  | { type: 'voice_model'; data: { id: string } }

/** 任务进度快照:按 task_id upsert,每条都是全量,错过即追平。 */
export interface TaskView {
  task_id: number
  kind: string
  label: TextRef
  state: 'running' | 'done' | 'failed'
  progress?: number
  step?: TextRef
  error?: TextRef
  /** 失败且可重放时带上 → UI 显「重试」按钮;无 = 不可重试。 */
  retry?: TaskRetry
}

/** 多集续播位置(有值 = 当前是 ≥2 集的剧集:B 站合集/分P、本地剧集文件夹)。 */
export interface PlaylistPos {
  /** 当前集下标(0 起)。 */
  index: number
  /** 总集数(>1)。 */
  total: number
  /** 本次是否「接着上次」续播跳转而来(前端忽略,仅供工具叙述)。 */
  resumed: boolean
}

/** 本次播放走的链路(core 发 key,前端出短标签)。见 Rust `media::PlaybackRoute`。 */
export type PlaybackRoute = 'direct' | 'dash' | 'hls_copy' | 'hls_transcode' | 'remux'

/** 一条音轨(core 探测;顺序 = 切换时的轨号-1)。 */
export interface AudioTrackInfo {
  /** 编码名("ac-3"/"mp4a"/"ac3"/"aac"…;"?" = 没解析出来)。 */
  codec: string
  /** ISO-639-2 语言码("chi"/"eng"…);缺省 = 未标注。 */
  lang?: string
  /** 元数据标题(mkv 常见「国语 DD5.1」)。 */
  title?: string
}

export interface NowPlaying {
  kind: 'audio' | 'video'
  title: string
  author?: string
  duration_seconds?: number
  stream_url: string
  /** 有值 = 自适应流(DASH/HLS):前端用 shaka(MSE)播,播放器管时间轴 → 原生 seek/同步(B 站走这里)。
   *  否则用 stream_url 挂原生 <video>/<audio>(直传文件/单流)。 */
  manifest_url?: string
  /** 「怎么放的」:直传 / 转码 / copy 切片 / 混流 / 自适应 —— 播放条上出一枚小徽章。
   *  可选:浏览器预览的假数据可能不带(徽章据此不显)。 */
  route?: PlaybackRoute
  page_url: string
  source: string
  /** 有值 = 多集剧集:UI 显「第N/共M集」+ 上/下一集按钮;ended 时若非末集自动续播。 */
  playlist?: PlaylistPos
  /** 循环模式镜像(core 是唯一真相,每次 Play 全量捎带):off / one(单曲,前端 el.loop)/ all(列表)。
   *  可选:浏览器预览假数据可不带(按 off 处理)。 */
  loop_mode?: 'off' | 'one' | 'all'
  /** 随机播放镜像(多集队列才可能 true)。 */
  shuffle?: boolean
  /** 全部音轨(本地探测;≥2 条才出切换钮)。缺省/空 = 单音轨或网络流。 */
  audio_tracks?: AudioTrackInfo[]
  /** 当前音轨(0 起下标)。 */
  audio_track?: number
  /** 有值 = 从这个位置(秒)接着播(切音轨重建管线时 core 带上,加载完 seek 过去)。 */
  resume_at?: number
}

export type MediaEvent =
  | { type: 'play'; data: NowPlaying }
  | { type: 'control'; data: { action: string; value?: number } }
  | { type: 'auth_required'; data: { source: string } }
  | { type: 'login_hint'; data: { source: string } }
  | { type: 'logged_in'; data: { source: string } }

/** 前端回报 core 的播放器快照(镜像 Rust media::PlaybackReport;「此刻」背景的数据源)。 */
export interface PlaybackReport {
  /** playing | paused | idle | loading(其余按 playing)。 */
  status: string
  title: string | null
  /** 基准音量 0–100(用户意图,不含唤醒避让折算)。 */
  volume?: number
  /** 播放位置 / 总长(秒);duration 未知传 null。 */
  position?: number
  duration?: number | null
  /** 倍速(缺省当 1)。 */
  rate?: number
}

// ---- 语音车道(PLAN §11):听写会话的状态与产出 ----

export type VoicePhase = 'idle' | 'preparing' | 'listening' | 'transcribing'

export type VoiceEvent =
  | { type: 'state'; data: { phase: VoicePhase } }
  | { type: 'level'; data: { level: number } }
  | { type: 'speech_started' }
  // 喊名命中(C 期):前端开全区间 duck(到回待唤醒才恢复)
  | { type: 'wake_triggered' }
  // KWS 报了候选、确认层在核(命中→静默续录→ASR 三段式):提前 duck + 轻视觉,不出声
  | { type: 'wake_candidate' }
  // 确认层拒绝(转写无唤醒词 = KWS 幻听):恢复 duck、视觉回 idle,零打扰
  | { type: 'wake_rejected' }
  // 唤醒常驻的权威开关广播(core 起/停时发,boot 自动恢复也发):wakeArmed 与 mic bridge
  // 靠它跟随——没有它,开机自启 core 起得比前端首查慢 → armed 定格 false → browser
  // 采集源永不开麦 =「开着但聋」(2026-07-11 真机实锤)
  | { type: 'wake_running'; data: { running: boolean; keywords: string[] } }
  // 呼名+续句(「天天暂停」/「看天天向上」):整句交模型仲裁(前端调 sendOverheard)
  | { type: 'overheard'; data: { text: string; speaker_id?: number } }
  // via: mic = 听写(屏幕排版) | wake = 语音会话(必念);speaker_id = 声纹认出的家人
  | { type: 'transcribed'; data: { text: string; via: 'mic' | 'wake' | string; speaker_id?: number } }
  | {
      type: 'listen_ended'
      data: {
        reason:
          | 'no_speech'
          | 'cancelled'
          | 'error'
          | 'no_speech_retry' // 唤醒首轮没听清,追问后重听中
          | 'farewell' // 两轮没听到,有声告退(回待唤醒)
          | 'follow_up_idle' // 跟进窗安静结束(回待唤醒)
          | 'wake_done' // 回合周期收尾兜底(回待唤醒)
          | string
      }
    }
  // 唤醒录音标定:正在录第 step/total 段(step 从 1 计;total 含末尾 1 段底噪)
  | { type: 'calib_progress'; data: { step: number; total: number } }
  // 唤醒标定收尾:verdict = good|noisy|hard|cancelled|error(前端字典渲染文案)
  | {
      type: 'calib_result'
      data: {
        ok: boolean
        sensitivity: number
        recall: number
        adopted_spelling: boolean
        verdict: string
      }
    }
  // 声纹注册进展(家人页「让它认识 TA 的声音」):stage=preparing(备模型)|recording(录第
  // done+1/total 段,请说话)|saved(成功)|failed(失败,前端 toast 请重试)。user_id=给谁录。
  | {
      type: 'enroll'
      data: {
        user_id: number
        stage: 'preparing' | 'recording' | 'saved' | 'failed' | string
        done?: number
        total?: number
      }
    }

/** 设置页「语音组件」状态行 + 麦克风设备列表 + 音色目录 + 唤醒状态。 */
export interface VoiceStatus {
  asrReady: boolean
  vadReady: boolean
  kwsReady: boolean
  /** 唤醒循环此刻在跑(事实;settings 里的 enabled 只是意向)。 */
  wakeRunning: boolean
  /** 当前唤醒词(= 名字派生,单源在后端 voice::wake_keywords)。 */
  keywords: string[]
  /** 起了名但名字语音喊不了(英文单词名)→ 回落默认词;UI 据此如实提示(§3.5)。 */
  wakeFallback: boolean
  devices: string[]
  /** 音色列表:内置在线音色 + 克隆(含内置 BT 预置);id 克隆为 "clone:<id>"。 */
  speakers: { id: string; name: string; isClone?: boolean; builtin?: boolean }[]
  /** 出厂默认音色 id(单源 = 后端 tts::DEFAULT_SPEAKER);前端未设音色时用它高亮默认项,不写死副本(§4.11)。 */
  defaultSpeaker: string
}

/** 输入形态(语音会话模式,PLAN §11):发送瞬间物化,真相在库。省略 = 打字默认形。 */
export interface UserMeta {
  input: 'typed' | 'mic' | 'wake'
  speak: boolean
  /** 声纹识别出的家人(D 期):本回合记忆归 TA。省略 = 走会话用户。 */
  speaker_user?: number
  /** 本回合带过的附件小票(媒体输入 PLAN §9):随 user 行 payload 回来,UI 显历史。 */
  attachments?: AttachmentRef[]
}

/** 入站附件(媒体输入 PLAN §9):前端把图/文档读成 base64,随消息送上 core 当轮处理。 */
export interface OutAttachment {
  name: string
  mime: string
  /** 原始字节 base64(无 data: 前缀)。 */
  data: string
}

/** 持久小票(历史里标这条带过图/文档);文档本体不留存,**图片 bytes 落文件**(file 相对名),
 *  重开会话经 attachmentUrl 取回缩略图(不再喂 LLM,§1/§9)。 */
export interface AttachmentRef {
  kind: 'image' | 'doc' | string
  name: string
  mime?: string
  /** 图片落盘相对名;有 = 可经 attachmentUrl 显缩略图。doc / 旧数据无。 */
  file?: string
}

/** 家人(设置·家人 tab,D 期):用户字段 + 是否已录声纹。 */
export interface FamilyMember {
  id: number
  name: string
  skin_id: string
  created_at: number
  last_active_at: number
  enrolled: boolean
}

/** 一条渠道对话映射(家人页「远程对话」区):这条 TG/钉钉对话被指认给谁。 */
export interface ChannelChat {
  id: number
  channel: string // 'telegram' | 'dingtalk'
  ext_id: string
  conv_id: number
  user_id: number | null // 指认的家人;null = 未指认(按会话归属者)
  label: string | null // 平台昵称(认脸用);null = 还没抓到
}

/** 远程渠道一行(设置页,PLAN 远程渠道);**凭证本身永不过桥**,只报 configured(bool)。 */
export interface RemoteChannelView {
  id: string // 'telegram' | 'dingtalk' | 'weixin'
  enabled: boolean
  configured: boolean
  allowed_chats: string
  running: boolean
  last_error: string | null
}

/** 微信扫码登录起手:二维码 SVG(v-html 直接展示)+ 备用链接 + 轮询用 qrcode。 */
export interface WeixinQrStart {
  qrcode: string
  qr_url: string
  qr_svg: string
}

/** 微信扫码轮询一次:status 驱动 UI;redirect 时 base_url = 新地址(下次回传)。 */
export interface WeixinQrPoll {
  status: string // wait | scaned | need_verifycode | verify_blocked | expired | redirect | confirmed | already
  base_url: string | null
}

/** 动作确认卡(§7.8 确认闸):全量快照语义(state 翻终态 = 收卡)。action = 动作原文
 *  (「点『确认支付 ¥128』」,页面数据非产文案);deadline_ms 画倒计时。 */
export interface ConfirmCard {
  id: number
  user_id: number
  conv_id: number
  origin: string // 'ui' | 'system' | 渠道名(渠道来源的卡桌面也显示、也可点——先到先得)
  host: string
  action: string
  kind: string // 'click' | 'submit'
  state: 'pending' | 'allowed' | 'denied' | 'expired'
  deadline_ms: number
  via?: string
}

/** 确认流水一行(操作记录页「确认过的操作」分组)。 */
export interface ConfirmLog {
  id: number
  user_id: number
  conv_id: number
  origin: string
  host: string
  action: string
  kind: string
  decision: 'allowed' | 'denied'
  via: string // desktop | float | voice | channel | timeout | no_ui | unreachable
  created_at: number
}

export type AppEvent =
  | { type: 'task'; data: TaskView }
  | { type: 'media'; data: MediaEvent }
  // 自启回合(提醒/定时)完成:某会话有动静,UI 刷新列表/重拉当前会话。
  // outcome = 回合终态(done/failed):用户不在该会话时,前端据此在列表项打彩色标。
  | { type: 'conversation'; data: { conv_id: number; kind: string; outcome: 'done' | 'failed' } }
  // 后台 LLM 给新会话起好标题(替换截断占位):原位改列表项文字,不打 badge、不重排
  | { type: 'conv_title'; data: { conv_id: number; title: string } }
  | { type: 'voice'; data: VoiceEvent }
  // 回合 mood(PLAN §12 修订):悬浮窗显「正在想/正在说」;主窗用自己的 per-turn mood,忽略这条
  | { type: 'mood'; data: 'idle' | 'thinking' | 'speaking' }
  // 动作确认卡(§7.8):HUD 任务区 + 悬浮窗显卡可点;终态卡 = 收卡信号
  | { type: 'confirm'; data: ConfirmCard }

/** 订阅全局事件车道;未知 type 忽略(与 TurnEvent 同一增量演化约定)。 */
export function onAppEvent(cb: (ev: AppEvent) => void): void {
  if (!isTauri()) return
  void listen<AppEvent>('app_event', (e) => cb(e.payload))
}

/** 纯浏览器预览(vite dev 不在 Tauri 壳里)时为 false,VM 层据此降级假数据。 */
export function isTauri(): boolean {
  return typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window
}

/** 外部链接交系统浏览器。WebView 里 `window.open` 是 no-op(Win 真机尤甚)→ Tauri 下走 opener 插件;
 *  浏览器预览(非 Tauri)兜底 window.open。只放行 http(s),best-effort、失败不打断。 */
export async function openExternal(url: string): Promise<void> {
  if (!/^https?:\/\//i.test(url)) return
  if (!isTauri()) {
    window.open(url, '_blank', 'noopener')
    return
  }
  try {
    await invoke('plugin:opener|open_url', { url })
  } catch (e) {
    console.warn('openExternal failed', e)
  }
}

// ---- 窗口 / 托盘 / 开机启动(PLAN §12):全收口在此,守"前端唯一碰 @tauri-apps/api"纪律 ----

/** App 版本号(x.y.z = 大版本.小版本.BUG修复)。唯一真相源 = tauri.conf.json,
 *  靠 getVersion() 读真身、永不漂移;浏览器预览非 Tauri,给开发兜底串。 */
export function appVersion(): Promise<string> {
  return isTauri() ? getVersion() : Promise.resolve('0.1.0-dev')
}

/** 当前 WebView 窗口标签('main' | 'float');浏览器预览看 ?window= 兜底。 */
export function windowLabel(): string {
  if (!isTauri()) return new URLSearchParams(location.search).get('window') ?? 'main'
  try {
    return getCurrentWindow().label
  } catch {
    return 'main'
  }
}

/** 平台判定:仅用于窗形分叉 —— macOS 用原生标准窗(`decorations:true`,红绿灯 + 绿灯真全屏),
 *  Windows/Linux 走无边框自绘三键(§7.6)。UA 足够:WebView2(Win) 不含 Macintosh、WKWebView(Mac) 含;
 *  非 Tauri 预览按宿主浏览器判(仅影响视觉分叉,无副作用)。 */
export const isMacOS = /Macintosh|Mac OS X/i.test(navigator.userAgent)

/** 主窗自绘三键(Windows/Linux;macOS 用原生红绿灯):无边框窗口操作,作用于当前窗口;非 Tauri 预览下 no-op。 */
export const win = {
  minimize: () => {
    if (isTauri()) void getCurrentWindow().minimize()
  },
  /** 中键(Windows/Linux):最大化⇄还原 —— 铺满工作区但保留三键与任务栏,永不困住用户(区别于全屏)。
   *  真正的沉浸全屏只留给看视频(VideoOverlay 影院模式,有浮层 ✕⛶ / Esc 退出)。 */
  toggleMaximize: () => {
    if (isTauri()) void getCurrentWindow().toggleMaximize()
  },
  /** 当前是否最大化(自绘三键的中键图标切换用:最大化⇄还原)。 */
  isMaximized: (): Promise<boolean> =>
    isTauri() ? getCurrentWindow().isMaximized() : Promise.resolve(false),
  /** 确定性进/退全屏(视频影院浮层专用:自动进 / 关闭强制退,不能用 toggle 读当前态——会竞态)。
   *  视频走原生窗口全屏而非 HTML5 requestFullscreen:后者在 WebView2 上与 DWM 合成器打架
   *  (闪烁/退出穿帮),故一律走原生窗口全屏才稳。窗口本身的"变大"是 toggleMaximize(不是全屏)。 */
  setFullscreen: async (on: boolean) => {
    if (isTauri()) await getCurrentWindow().setFullscreen(on)
  },
  isFullscreen: (): Promise<boolean> =>
    isTauri() ? getCurrentWindow().isFullscreen() : Promise.resolve(false),
  /** 当前窗口是否藏着(托盘 hide)。视频起播前读一次,停时据此决定要不要再藏回去。 */
  isHidden: async (): Promise<boolean> =>
    isTauri() ? !(await getCurrentWindow().isVisible()) : false,
  /** 把当前窗口叫到最前:显示 + 取消最小化 + 聚焦。视频起播用 —— 否则窗口藏在托盘/
   *  别的窗后面时,视频在后台播,用户"只闻其声"(实测)。已在前台则全是 no-op,不闪。 */
  bringToFront: async () => {
    if (!isTauri()) return
    const w = getCurrentWindow()
    await w.show()
    await w.unminimize()
    await w.setFocus()
    // JS 侧 show 不经壳层 show_window → 自己补可见性信号,否则藏托盘期间被视频唤起时
    // visible 卡 false、动画不恢复(透明窗 visibilitychange 不报,见 usePageVisible)。
    void emit('lw:win-visible', true)
  },
  /** 置顶开关:看电影期间开,放完关 —— 别被别的窗口盖住(用户要"置顶")。 */
  setAlwaysOnTop: async (on: boolean) => {
    if (isTauri()) await getCurrentWindow().setAlwaysOnTop(on)
  },
  /** ✕ = 隐藏到托盘,不退进程(真退出走托盘菜单 quit)。 */
  hideToTray: () => {
    if (!isTauri()) return
    void getCurrentWindow().hide()
    // ✕ 是自绘按钮、直接 hide() —— 不触发壳层 CloseRequested(那条只兜 Alt+F4),得自己发
    // 「隐藏」信号:不发的话透明窗 RAF 不被节流,藏托盘后动画满帧空跑(§8.1 v0.1.2 坑本路径复活),
    // 且 visible 卡 true 会在隐藏窗里排下死 rAF id(自启冻死同族)。
    void emit('lw:win-visible', false)
  },
  /** 监听本窗尺寸 / 全屏变化(图标切换);返回取消订阅。 */
  onResized(cb: () => void): () => void {
    if (!isTauri()) return () => {}
    let un = () => {}
    void getCurrentWindow()
      .onResized(() => cb())
      .then((f) => (un = f))
    return () => un()
  },
}

/** 悬浮窗自身控制(在 float 窗口内调用)。前端用 box/setBox 自己算"锚点固定 + 自动方向"展开。 */
export const floatWin = {
  hideSelf: () => {
    if (isTauri()) void getCurrentWindow().hide()
  },
  /** 手动拖动:替代 data-tauri-drag-region(它在 Windows 上会吞掉单击 click,见 FloatWindow 手势)。 */
  startDragging: () => {
    if (isTauri()) void getCurrentWindow().startDragging()
  },
  /** 读当前窗口盒 + 所在屏幕上下边(逻辑像素):前端据此算锚点 / 判断向上还是向下展开。 */
  box: async (): Promise<{
    x: number
    y: number
    w: number
    h: number
    screenTop: number
    screenBottom: number
  } | null> => {
    if (!isTauri()) return null
    const win = getCurrentWindow()
    const sf = await win.scaleFactor()
    const op = await win.outerPosition()
    const os = await win.outerSize()
    const mon = await currentMonitor()
    const top = mon ? mon.position.y / sf : 0
    const bottom = mon ? (mon.position.y + mon.size.height) / sf : 100000
    return { x: op.x / sf, y: op.y / sf, w: os.width / sf, h: os.height / sf, screenTop: top, screenBottom: bottom }
  },
  /** 设窗口盒(逻辑像素):resizable 临时开 → setPosition + setSize → 关回。
   *  为什么 toggle:float 常态保持 resizable:false(否则用户能像普通窗口那样拖边把胶囊/面板缩放,
   *  错了 —— 它只该点击展开)。但 Windows/tao 下 resizable:false 会把 min/max 尺寸钉成初始值、
   *  程序化 setSize 被夹住长不大(tauri#5679;mac 不钳制)。故只在这一次程序化 resize 时短暂打开,
   *  finally 里立刻关回 → 既能展开,用户又拖不动。 */
  setBox: async (x: number, y: number, w: number, h: number) => {
    if (!isTauri()) return
    const win = getCurrentWindow()
    await win.setResizable(true)
    try {
      await win.setPosition(new LogicalPosition(Math.round(x), Math.round(y)))
      await win.setSize(new LogicalSize(Math.round(w), Math.round(h)))
    } finally {
      await win.setResizable(false)
    }
  },
  /** 监听本窗移动(逻辑坐标);返回取消订阅。 */
  onMoved(cb: (x: number, y: number) => void): () => void {
    if (!isTauri()) return () => {}
    let un = () => {}
    const win = getCurrentWindow()
    void win.scaleFactor().then((sf) => {
      void win.onMoved((e) => cb(e.payload.x / sf, e.payload.y / sf)).then((f) => (un = f))
    })
    return () => un()
  },
}

/** 跨标签唤出窗口(托盘 / 悬浮窗点击 → 主窗):show + 取消最小化 + 聚焦。 */
export async function summonWindow(label: string) {
  if (!isTauri()) return
  const w = await Window.getByLabel(label)
  if (!w) return
  await w.show()
  await w.unminimize()
  await w.setFocus()
  // JS 侧 show(悬浮窗唤主窗)不经壳层 show_window → 补可见性信号,否则开机自启后
  // 经悬浮窗打开主窗,visible 卡 false、会话区动画全冻(§8.1 自启冻死第三轮病灶)。
  if (label === 'main') void emit('lw:win-visible', true)
}

/** main 侧按 enabled 显隐悬浮窗(显示不抢焦点 —— 被动呈现)。 */
export async function setFloatVisible(visible: boolean) {
  if (!isTauri()) return
  const w = await Window.getByLabel('float')
  if (!w) return
  if (visible) await w.show()
  else await w.hide()
}

// ---- 跨窗口对齐(PLAN §12 E):两个 WebView 不共享内存,靠全局事件广播 ----

/** 任一窗口改设置 → 广播,各窗口 useSettings 跟随(主窗换形象 / 透明度,悬浮窗实时跟上)。 */
export function emitSettingChanged(key: string, value: string) {
  if (isTauri()) void emit('lw:setting', { key, value })
}
export function onSettingChanged(cb: (key: string, value: string) => void): void {
  if (!isTauri()) return
  void listen<{ key: string; value: string }>('lw:setting', (e) => cb(e.payload.key, e.payload.value))
}

/** 悬浮窗点通知 → 让主窗切到该会话。 */
export function emitOpenConversation(convId: number) {
  if (isTauri()) void emit('lw:open-conv', { conv_id: convId })
}
export function onOpenConversation(cb: (convId: number) => void): void {
  if (!isTauri()) return
  void listen<{ conv_id: number }>('lw:open-conv', (e) => cb(e.payload.conv_id))
}

/** 设置里开/关免手唤醒 → 广播事实态(wakeRunning + 唤醒词):悬浮窗是另一个 WebView,
 *  待机栏据此实时更新「等你喊…」(开机自启那次由 float 自己 voiceStatus() 兜底)。 */
export function emitWakeChanged(running: boolean, keywords: string[]) {
  if (isTauri()) void emit('lw:wake', { running, keywords })
}
export function onWakeChanged(cb: (running: boolean, keywords: string[]) => void): void {
  if (!isTauri()) return
  void listen<{ running: boolean; keywords: string[] }>('lw:wake', (e) =>
    cb(e.payload.running, e.payload.keywords),
  )
}

/** 换肤 → 广播给悬浮窗(另一个 WebView):它据此把 <html data-skin> 同步过去,观感随主窗切。
 *  开机自启那次由 float 自己 api.skin() 兜底拉初值(事件不缓存,先到先丢)。 */
export function emitSkinChanged(skin: string) {
  if (isTauri()) void emit('lw:skin', { skin })
}
export function onSkinChanged(cb: (skin: string) => void): void {
  if (!isTauri()) return
  void listen<{ skin: string }>('lw:skin', (e) => cb(e.payload.skin))
}

/** 主窗是全 app 唯一真出声的播放位;current/status 一变(放 / 暂停 / 停 / 放完)就把全量快照广播给悬浮窗。
 *  悬浮窗(被动镜像、不出声)据此跟随。传整份 NowPlaying|null + 当前播放态 = 绝对态 → 幂等、不怕重放,
 *  也不会像广播相对动作(louder/toggle)那样翻车。只主窗发、只悬浮窗收(见 useMedia.wire)→ 无回声环。
 *  补 core 事件的盲区:UI 点停止 / 暂停 / 自然播完不经 core,只有这条能让悬浮窗追平"正在放/已暂停"。 */
export function emitNowPlaying(np: NowPlaying | null, status: string) {
  if (isTauri()) void emit('lw:nowplaying', { np, status })
}
export function onNowPlaying(cb: (np: NowPlaying | null, status: string) => void): void {
  if (!isTauri()) return
  void listen<{ np: NowPlaying | null; status: string }>('lw:nowplaying', (e) =>
    cb(e.payload.np, e.payload.status),
  )
}

/** 悬浮窗迷你播控 → 主窗(唯一真播放位)执行。只悬浮窗发、只主窗收(见 useMedia.wire)→ 无回声环;
 *  与嘴控(core MediaEvent::Control)汇到同一 applyControl,动作词表一致(pause/resume/stop/…)。 */
export function emitMediaControl(action: string, value?: number) {
  if (isTauri()) void emit('lw:media-control', { action, value })
}
export function onMediaControl(cb: (action: string, value?: number) => void): void {
  if (!isTauri()) return
  void listen<{ action: string; value?: number }>('lw:media-control', (e) =>
    cb(e.payload.action, e.payload.value),
  )
}

/** 悬浮窗关怀候选点击 → 主窗替用户把那句发出去(同建议气泡「替用户说一句」§3.2;
 *  悬浮窗只读不发回合,发送/落库/念答全在主窗。只悬浮窗发、只主窗收 → 无回声环。 */
export function emitFloatSay(text: string) {
  if (isTauri()) void emit('lw:float-say', { text })
}
export function onFloatSay(cb: (text: string) => void): void {
  if (!isTauri()) return
  void listen<{ text: string }>('lw:float-say', (e) => cb(e.payload.text))
}

/** 程序更新状态 → 悬浮窗镜像(待机轮播出「发现新版本」可点条)。主窗是唯一更新执行位
 *  (useUpdater 只在主窗跑),状态一变就广播绝对态(available/downloaded/none),幂等不怕重放;
 *  事件不缓存、先到先丢 —— 错过就等下一轮每日复查再发,浮窗这条是锦上添花不是真相源。 */
export type UpdatePhase = 'none' | 'available' | 'downloaded'
export function emitUpdateState(phase: UpdatePhase, version?: string) {
  if (isTauri()) void emit('lw:update-state', { phase, version })
}
export function onUpdateState(cb: (phase: UpdatePhase, version?: string) => void): void {
  if (!isTauri()) return
  void listen<{ phase: UpdatePhase; version?: string }>('lw:update-state', (e) =>
    cb(e.payload.phase, e.payload.version),
  )
}
/** 悬浮窗点「更新」条 → 主窗执行:没下载 = 开始下载(进任务 HUD);已下载 = 装 + 重启。 */
export function emitFloatUpdate() {
  if (isTauri()) void emit('lw:float-update', {})
}
export function onFloatUpdate(cb: () => void): void {
  if (!isTauri()) return
  void listen('lw:float-update', () => cb())
}

/** 数据目录「搬家」:当前根 / 待清理旧根 / 失效路径(字段随 Rust camelCase)。 */
export interface DataLocation {
  root: string
  oldRoot: string | null
  missing: string | null
  /** 本次启动「从备份恢复」的落位结果('ok'/'failed';null = 无事)→ boot 弹一句结果。 */
  restored: string | null
}

/** 搬家预检结果(给确认弹窗:目标路径 + 体积 + 失败原因 code)。 */
export interface RelocateCheck {
  ok: boolean
  /** 失败原因 code(→ settings.dataLocation.err.<reason>);ok 时 null。 */
  reason: string | null
  newRoot: string | null
  needBytes: number
  freeBytes: number
}

/** 恢复预检结果(给确认弹窗:包概要 + 失败原因 code)。 */
export interface RestoreCheck {
  ok: boolean
  /** 失败原因 code(→ settings.system.restoreErr.<reason>);ok 时 null。 */
  reason: string | null
  dbBytes: number
  clones: number
}

export const api = {
  boot: () => invoke<BootSnapshot>('boot'),

  /** command 立即返回;事件经 Channel 持续推送直到 done/failed/cancelled。 */
  sendMessage(
    convId: number,
    text: string,
    meta: UserMeta | null,
    attachments: OutAttachment[],
    onEvent: (ev: TurnEvent) => void,
  ) {
    const channel = new Channel<TurnEvent>()
    channel.onmessage = onEvent
    return invoke<void>('send_message', { convId, text, meta, attachments, onEvent: channel })
  },

  /** 插队(PLAN §9 B):塞进正在跑的回合,下一轮 LLM 带上。返回 false=没接住,改用 sendMessage。 */
  injectMessage(convId: number, text: string, meta: UserMeta | null, attachments: OutAttachment[]) {
    return invoke<boolean>('inject_message', { convId, text, meta, attachments })
  },

  cancelGeneration: (convId: number) => invoke<void>('cancel_generation', { convId }),
  usageToday: () => invoke<DayUsage>('usage_today'),
  usageConversation: (convId: number) => invoke<UsageTotals>('usage_conversation', { convId }),
  /** 历史/提醒气泡读数(load 会话后回填,让自启回合也能 hover 看时间/token)。 */
  conversationStats: (convId: number) => invoke<MsgStats[]>('conversation_stats', { convId }),
  /** 历史回放的「想了想」轨迹(load 会话后回填到代表气泡)。 */
  conversationTrace: (convId: number) => invoke<TurnTrace[]>('conversation_trace', { convId }),
  llmBalance: () => invoke<AccountBalance | null>('llm_balance'),
  /** 悬浮窗待机轮播:下个提醒 + 最近一句(余额/今日花费复用 llmBalance/usageToday)。 */
  floatIdle: () => invoke<FloatIdle>('float_idle'),
  newConversation: (channel?: string) => invoke<Conversation>('new_conversation', { channel }),
  listConversations: () => invoke<Conversation[]>('list_conversations'),
  loadConversation: (convId: number) => invoke<Message[]>('load_conversation', { convId }),
  /** 跨会话搜索聊天记录(子串,排除工具/系统行);最近命中在前。 */
  searchMessages: (query: string, limit = 50) =>
    invoke<SearchHit[]>('search_messages', { query, limit }),
  deleteConversation: (convId: number) => invoke<void>('delete_conversation', { convId }),
  renameConversation: (convId: number, title: string) =>
    invoke<void>('rename_conversation', { convId, title }),
  setConversationPinned: (convId: number, pinned: boolean) =>
    invoke<void>('set_conversation_pinned', { convId, pinned }),
  setApiKey: (key: string) => invoke<void>('set_api_key', { key }),
  setSkin: (skinId: string) => invoke<void>('set_skin', { skinId }),
  skin: () => invoke<string>('skin'),
  listSettings: () => invoke<SettingEntry[]>('list_settings'),
  setSetting: (key: string, value: string) => invoke<void>('set_setting', { key, value }),
  /** 全局应用公钥(Ed25519,PEM):没有就生成、有就回存量;用户复制到服务控制台(和风 JWT 等)。 */
  ensureAppKeypair: () => invoke<string>('ensure_app_keypair'),
  renameUser: (name: string) => invoke<User>('rename_user', { name }),
  listProviders: () => invoke<ProviderView[]>('list_providers'),
  saveProvider: (patch: ProviderPatch) => invoke<ProviderView[]>('save_provider', { patch }),
  removeProvider: (id: string) => invoke<ProviderView[]>('remove_provider', { id }),
  modelMeta: (model: string) => invoke<ModelMeta>('model_meta', { model }),
  setModelOverride: (over: ModelOverride) => invoke<void>('set_model_override', { over }),
  /** 扫码登录窗口;title 从字典取(原生窗口标题没法事后翻译)。 */
  mediaLogin: (source: string, title: string) => invoke<void>('media_login', { source, title }),
  /** 失败任务重试(目前仅影音):按 retry 载体直连重放,进展/结果照常走事件车道。 */
  mediaRetry: (pageUrl: string, audioOnly: boolean) =>
    invoke<void>('media_retry', { pageUrl, audioOnly }),
  /** 失败下载重试:重下一个组件(yt-dlp/ffmpeg…),直连不绕 LLM。 */
  retryDownload: (component: string) => invoke<void>('retry_download', { component }),
  retryVoiceModel: (id: string) => invoke<void>('retry_voice_model', { id }),
  /** 多集续播切集(+1 下一集 / -1 上一集):ended 自动续播、播放器上/下一集按钮直连这里(不绕 LLM)。
   *  越界(到头/到顶)在 core 内静默(只记日志)。fire-and-forget。 */
  mediaAdvance: (delta: number) => invoke<void>('media_advance', { delta }),
  /** 一集放完问 core「接下来放什么」:true=已接管(循环/随机/顺序切下一首,Play 事件接力);false=正常收尾。 */
  mediaAutoNext: () => invoke<boolean>('media_auto_next'),
  /** 播放条循环/随机/音轨按钮 → core 校验落状态(与嘴控同一执行口);audio_track 带 value(1 起)。 */
  mediaMode: (action: string, value?: number) => invoke<void>('media_mode', { action, value }),
  /** 回报播放器当下状态给 core(只主窗调):状态/标题之外带基准音量(0–100)、进度/时长(秒)、
   *  倍速。core 据此在下个回合喂模型「此刻」背景 —— 修「歌放完了却以为还在播」,并让模型知道
   *  当前音量/进度(才能「调到 50」「快进 5 分钟」)。fire-and-forget。 */
  reportMediaState: (report: PlaybackReport) => invoke<void>('report_media_state', { report }),
  /** 兜底重放:本地自适应(手写 MSE)播放失败 → 后端对同一文件强制走 muxed HLS(能放的老路)。 */
  mediaReplayCompat: (pageUrl: string, audioOnly: boolean) =>
    invoke<void>('media_replay_compat', { pageUrl, audioOnly }),
  /** 前端播放层诊断 → 写进 larkwing.log(正式版无 JS console,真机定位自适应问题靠它)。 */
  mediaLog: (msg: string) => invoke<void>('media_log', { msg }).catch(() => {}),
  /** 历史图片小票(相对名)→ 可显缩略图的 localhost URL(重开会话回看发过的图)。 */
  attachmentUrl: (file: string) => invoke<string>('attachment_url', { file }),
  /** 远程渠道状态(设置页):开关/已配凭证/白名单/连接态(凭证不过桥)。 */
  remoteStatus: () => invoke<RemoteChannelView[]>('remote_status'),
  /** 保存远程渠道配置后调:停旧起新(类比 provider 保存即重建)。 */
  reloadChannels: () => invoke<void>('reload_channels'),
  /** 微信扫码登录起手:拿二维码(SVG + 备用链接 + 轮询 qrcode)。 */
  weixinLoginStart: () => invoke<WeixinQrStart>('weixin_login_start'),
  /** 微信扫码轮询一次:前端循环调;confirmed 时 core 已把账号(token/入口/身份)进绑定列表。 */
  weixinLoginPoll: (qrcode: string, baseUrl: string | null, verifyCode: string | null) =>
    invoke<WeixinQrPoll>('weixin_login_poll', { qrcode, baseUrl, verifyCode }),
  /** 微信绑定列表(多绑定 = 一人一 bot):绑定者 user_id;空串 = 旧版迁移的无身份绑定。不含 token。 */
  weixinAccounts: () => invoke<string[]>('weixin_accounts'),
  /** 解绑一个微信账号(空串 = 旧迁移绑定);解绑后调 reloadChannels 生效。 */
  weixinUnbind: (userId: string) => invoke<void>('weixin_unbind', { userId }),
  /** 开听写:立即返回,进展走 app_event 的 voice 车道(首次会触发模型用时下载)。 */
  voiceListenStart: () => invoke<void>('voice_listen_start'),
  /** 停听写:accept = 立即定稿(已听到的送识别);false = 取消丢弃。幂等。 */
  voiceListenStop: (accept: boolean) => invoke<void>('voice_listen_stop', { accept }),
  voiceStatus: () => invoke<VoiceStatus>('voice_status'),
  /** 免手唤醒开关(写设置+起停一体;首次开会下 KWS 模型 + 预合成应答音,较慢)。 */
  voiceWakeSet: (enabled: boolean) => invoke<VoiceStatus>('voice_wake_set', { enabled }),
  /** 旁听仲裁(唤醒确认层「呼名+续句」):临时回合无 Channel,终态经全局车道 kind=overheard*。 */
  sendOverheard: (convId: number, text: string, speaker?: number) =>
    invoke<void>('send_overheard', { convId, text, speaker }),
  /** 浏览器采集推流(层1 AEC 采集端):16k mono i16 LE 帧,raw body 免 JSON(~10Hz)。 */
  voicePushAudio: (pcm: Uint8Array) => invoke<void>('voice_push_audio', pcm),
  /** 唤醒回合念完 → 开跟进窗(免唤醒接话):常态 6s;媒体在播传 true → 3s 短窗少压音量。 */
  voiceFollowUp: (mediaPlaying: boolean) => invoke<void>('voice_follow_up', { mediaPlaying }),
  /** 换音色/语速/在线离线档后:唤醒在跑则后台重建应答音(不重启唤醒/麦);没开唤醒则 no-op。 */
  voiceRefreshPrompts: () => invoke<void>('voice_refresh_prompts'),
  /** 唤醒回合失败/取消/被忽略 → 直接回待唤醒。 */
  voiceWakeResume: () => invoke<void>('voice_wake_resume'),
  /** TTS 在念(含重听)时唤醒循环丢帧(自激防护)。 */
  voiceWakeSuspend: (on: boolean) => invoke<void>('voice_wake_suspend', { on }),
  /** 录音标定唤醒:立即返回,进展走 voice 车道(calib_progress/state/calib_result)。 */
  voiceCalibrateWake: () => invoke<void>('voice_calibrate_wake'),
  /** 取消进行中的唤醒标定(幂等)。 */
  voiceCalibrateCancel: () => invoke<void>('voice_calibrate_cancel'),
  /** 家人列表(含已录声纹标记)。 */
  listFamily: () => invoke<FamilyMember[]>('list_family'),
  addFamily: (name: string) => invoke<{ id: number; name: string }>('add_family', { name }),
  removeFamily: (id: number) => invoke<void>('remove_family', { id }),
  /** 给某家人改名(renameUser 改的是默认用户,这条按 id)。 */
  renameFamily: (id: number, name: string) => invoke<void>('rename_family', { id, name }),
  /** 渠道对话列表(家人页「远程对话」区)。 */
  listChannelChats: () => invoke<ChannelChat[]>('list_channel_chats'),
  /** 指认某条渠道对话归哪位家人(null = 取消指认)。 */
  bindChannelChat: (id: number, userId: number | null) =>
    invoke<void>('bind_channel_chat', { id, userId }),
  /** 给某家人录声纹(录 3 段取平均):立即返回,进展/终态走 voice 的 enroll 事件
   *  (preparing→recording×3→saved/failed);saved 后重拉 list_family 刷新「已录」。 */
  voiceEnroll: (userId: number) => invoke<void>('voice_enroll', { userId }),
  /** 忘掉某家人的声纹(只删声纹,人/记忆不动):同步返回。 */
  voiceUnenroll: (userId: number) => invoke<void>('voice_unenroll', { userId }),
  /** 句级 TTS:合成进缓存(命中秒回),返回可挂 <audio> 的 localhost URL。 */
  ttsSynthesize: (text: string) => invoke<string>('tts_synthesize', { text }),
  /** 设置页音色试听(试听句出自字典——core 不产文案)。 */
  voicePreview: (speaker: string, text: string) =>
    invoke<string>('voice_preview', { speaker, text }),
  /** 列出克隆音色(内置预置 + 用户自录)。 */
  listVoiceClones: () =>
    invoke<{ id: string; name: string; wavFile: string; transcript: string; lang: string; builtin: boolean; createdAt: number }[]>(
      'list_voice_clones',
    ),
  /** 录一段参考音 → 自动转写,返回草稿(未落库);进展走 voice 事件。 */
  voiceCloneRecord: () =>
    invoke<{ cloneId: string; transcript: string }>('voice_clone_record'),
  /** 导入本地音频文件(前端已解码/重采样成 16k 单声道 wav 的 base64)→ 转写,返回草稿(未落库)。 */
  voiceCloneImport: (wavBase64: string) =>
    invoke<{ cloneId: string; transcript: string }>('voice_clone_import', { wavBase64 }),
  /** 确认录入:用(可能改过的)文字稿 + 名字落库。 */
  voiceCloneSave: (cloneId: string, name: string, transcript: string) =>
    invoke<{ id: string; name: string }>('voice_clone_save', { cloneId, name, transcript }),
  /** 重命名克隆音色。 */
  renameVoiceClone: (cloneId: string, name: string) =>
    invoke<void>('rename_voice_clone', { cloneId, name }),
  /** 删除克隆音色(内置不可删)。 */
  deleteVoiceClone: (cloneId: string) => invoke<void>('delete_voice_clone', { cloneId }),
  /** 小本本。userId 省略 = 当前主人;传家人 id = 主人查看 TA 的记忆(§渠道归人第二步)。 */
  listMemories: (userId?: number) => invoke<Memory[]>('list_memories', { userId }),
  /** 删记忆。userId 省略 = 当前主人;传家人 id = 主人删 TA 的记忆。 */
  deleteMemory: (id: number, userId?: number) => invoke<void>('delete_memory', { id, userId }),
  // 记忆维护流水(§13.7 调阈值用:回看每轮衰减/下沉/升层/合并/硬清了多少)。
  memoryMaintenanceLog: (limit?: number) =>
    invoke<MaintenanceLog[]>('memory_maintenance_log', { limit }),
  /** 家里的事(家庭备忘)。userId 省略 = 当前主人;传家人 id = TA 视角(home 共享那份都在)。 */
  listBriefings: (userId?: number) => invoke<Briefing[]>('list_briefings', { userId }),
  deleteBriefing: (id: number) => invoke<void>('delete_briefing', { id }),
  /** 没办完的事(切片2 小账)。userId 语义同 listMemories(主人管理面)。 */
  listTodos: (userId?: number) => invoke<Todo[]>('list_todos', { userId }),
  /** 勾掉一件待办(办完 / 不用了):了结不删行,之后不再进前缀。 */
  finishTodo: (id: number, userId?: number) => invoke<void>('finish_todo', { id, userId }),
  /** 「这些日子」家庭日记(home 共有,不随「看谁的」切)。 */
  listDiary: () => invoke<DiaryEntry[]>('list_diary'),
  deleteDiary: (id: number) => invoke<boolean>('delete_diary', { id }),
  /** 提醒页:当前用户待触发的提醒 + 取消(jobs 域,按时间升序)。 */
  listReminders: () => invoke<Reminder[]>('list_reminders'),
  cancelReminder: (id: number) => invoke<void>('cancel_reminder', { id }),
  /** 操作记录页(文件能力):最近的文件操作批次 + 撤销/重做(功能性,非安全承诺)。 */
  listFsops: () => invoke<FsOp[]>('list_fsops'),
  fsopsUndo: (id: number) => invoke<void>('fsops_undo', { id }),
  fsopsRedo: (id: number) => invoke<void>('fsops_redo', { id }),
  /** 确认卡应答(§7.8):HUD 卡/悬浮窗按钮直连;false = 卡已收尾(过期/别处先点)。 */
  confirmAction: (id: number, allow: boolean, via: string) =>
    invoke<boolean>('confirm_action', { id, allow, via }),
  /** 操作记录页「确认过的操作」分组:最近确认流水(全家,主人管理面)。 */
  listConfirms: () => invoke<ConfirmLog[]>('list_confirms'),
  /** 口头确认(§7.8):卡属于当前语音回合时开一段听音;false = 没开听(cpal 源/唤醒没跑)。 */
  voiceConfirmListen: (id: number) => invoke<boolean>('voice_confirm_listen', { id }),
  /** 开机自启(PLAN §12):OS 是真相源,不进 DB。 */
  autostartEnabled: () => invoke<boolean>('autostart_enabled'),
  setAutostart: (on: boolean) => invoke<void>('set_autostart', { on }),
  /** 托盘菜单文案注入(§6:文案在前端字典,boot 后传给壳层建菜单)。 */
  setTrayMenu: (open: string, showFloat: string, quit: string) =>
    invoke<void>('set_tray_menu', { open, showFloat, quit }),
  quitApp: () => invoke<void>('quit_app'),
  /** 更新装完后重启(走核心 app.restart;Win 多由安装器拉起、此路主要给 mac/兜底)。 */
  relaunchApp: () => invoke<void>('relaunch_app'),
  // ---- 数据目录「搬家」(datadir) ----
  /** 当前数据根 / 待清理旧根 / 失效路径(设置页一行 + boot 检查)。 */
  dataLocation: () => invoke<DataLocation>('data_location'),
  /** 唤起系统原生目录选择器;取消 = null。 */
  pickDataFolder: () => invoke<string | null>('pick_data_folder'),
  /** 搬家预检(选完目录、确认前):目标路径 + 体积 + 可行性。 */
  relocatePrecheck: (picked: string) => invoke<RelocateCheck>('relocate_precheck', { picked }),
  /** 执行搬家:拷贝(HUD 进度)→ 翻指针 → 立即重启(成功不返回,页面随重启刷新)。 */
  relocateData: (picked: string) => invoke<void>('relocate_data', { picked }),
  /** 搬家后删除旧数据(保留指针)。 */
  cleanupOldData: () => invoke<void>('cleanup_old_data'),
  /** 搬家后保留旧数据(只清提示,不删盘)。 */
  keepOldData: () => invoke<void>('keep_old_data'),
  /** 数据位置失效时「恢复默认」:清指针 → 重启从默认位置起(全新数据)。 */
  dataResetToDefault: () => invoke<void>('data_reset_to_default'),
  /** 在系统文件管理器里打开数据文件夹。 */
  revealDataDir: () => invoke<void>('reveal_data_dir'),
  /** 一键备份:在所选目录导出 larkwing-backup-<时间戳>.zip(DB 快照 + 克隆音色),返回包路径。 */
  backupData: (destDir: string) => invoke<string>('backup_data', { destDir }),

  /** 原生文件选择器挑备份包(zip);取消 = null。 */
  pickBackupFile: () => invoke<string | null>('pick_backup_file'),

  /** 恢复预检(纯校验):zip 结构 + DB 魔数 + 迁移版本前向检查。 */
  restorePrecheck: (zip: string) => invoke<RestoreCheck>('restore_precheck', { zip }),

  /** 执行恢复:负载暂存 → 自动重启,下次启动开库前落位(现库留 pre-restore 保险副本)。成功不返回。 */
  restoreData: (zip: string) => invoke<void>('restore_data', { zip }),
}

/** 托盘点「显示悬浮窗」→ 壳层 emit,主窗据此重开悬浮窗(置 enabled + show)。 */
export function onShowFloat(cb: () => void): void {
  if (!isTauri()) return
  void listen('lw:show-float', () => cb())
}

/** 壳层(仅 Windows)轮询「别的程序是否全屏」→ 变化时推事实给主窗,让常驻悬浮窗让位
 *  (游戏 / 全屏视频别被浮窗打扰)。Mac 不发此事件(原生 space 已不覆盖别 app 全屏),
 *  前端默认 false 即维持原行为。 */
export function onForegroundFullscreen(cb: (fullscreen: boolean) => void): void {
  if (!isTauri()) return
  void listen<boolean>('lw:foreground-fullscreen', (e) => cb(e.payload))
}

/** 主窗 hide/show 时壳层发的可见性信号(只为 main 发):藏托盘后据此暂停动画省 CPU。
 *  见 usePageVisible —— 透明窗的 RAF 不会被 Chromium 自动节流,得显式停。 */
export function onWinVisible(cb: (visible: boolean) => void): void {
  if (!isTauri()) return
  void listen<boolean>('lw:win-visible', (e) => cb(e.payload))
}
