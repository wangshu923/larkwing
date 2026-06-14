// IPC 封装:类型与 commands 一一对应(契约见 PLAN §5)。
// 这是前端唯一 import @tauri-apps/api 的地方。

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
  created_at: number
  updated_at: number
}

export interface Message {
  id: number
  conversation_id: number
  role: string // 'user' | 'assistant' | 'tool'
  content: string
  created_at: number
  /** 工具轮附加数据(JSON,engine 私有词汇);UI 只用它判断"这行别渲染气泡"。 */
  payload?: string | null
}

/** 小本本一条(回忆页);kind: fact/profile/summary(宪法 §6,细化 TBD)。 */
export interface Memory {
  id: number
  user_id: number
  kind: string
  content: string
  created_at: number
  updated_at: number
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
export interface FloatIdle {
  next_reminder?: FloatReminder
  latest_line?: string
}

export type TurnEvent =
  | { type: 'delta'; data: string }
  | { type: 'thinking'; data: string }
  // label = i18n 键(如 'tool.remember'),文案在字典;绝不露"工具"概念
  | { type: 'tool_use'; data: { label: string; state: 'started' | 'finished' } }
  // 记账灯带:本轮消耗 + 今日/会话累计快照(工具回合每轮一次;累计来自库,前端只展示)
  | { type: 'usage'; data: { round: UsageDigest; today: DayUsage; conv: UsageTotals } }
  | { type: 'done'; data: { message_id: number } }
  | { type: 'failed'; data: AppError }
  | { type: 'cancelled' }

// ---- 全局事件车道(bus → "app_event"):回合之外的事 ----

/** 文案引用:key 进字典,params 是命名插值(core 不产文案铁规的线上形态)。 */
export interface TextRef {
  key: string
  params?: Record<string, unknown>
}

/** 任务进度快照:按 task_id upsert,每条都是全量,错过即追平。 */
export interface TaskView {
  task_id: number
  kind: string
  label: TextRef
  state: 'running' | 'done' | 'failed'
  progress?: number
  step?: TextRef
  error?: TextRef
}

export interface NowPlaying {
  kind: 'audio' | 'video'
  title: string
  author?: string
  duration_seconds?: number
  stream_url: string
  page_url: string
  source: string
}

export type MediaEvent =
  | { type: 'play'; data: NowPlaying }
  | { type: 'control'; data: { action: string; value?: number } }
  | { type: 'auth_required'; data: { source: string } }
  | { type: 'login_hint'; data: { source: string } }
  | { type: 'logged_in'; data: { source: string } }

// ---- 语音车道(PLAN §11):听写会话的状态与产出 ----

export type VoicePhase = 'idle' | 'preparing' | 'listening' | 'transcribing'

export type VoiceEvent =
  | { type: 'state'; data: { phase: VoicePhase } }
  | { type: 'level'; data: { level: number } }
  | { type: 'speech_started' }
  // 喊名命中(C 期):前端开全区间 duck(到回待唤醒才恢复)
  | { type: 'wake_triggered' }
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

/** 设置页「语音组件」状态行 + 麦克风设备列表 + 音色目录 + 唤醒状态。 */
export interface VoiceStatus {
  asrReady: boolean
  vadReady: boolean
  kwsReady: boolean
  /** 唤醒循环此刻在跑(事实;settings 里的 enabled 只是意向)。 */
  wakeRunning: boolean
  keywords: string[]
  devices: string[]
  speakers: { id: string; name: string }[]
}

/** 输入形态(语音会话模式,PLAN §11):发送瞬间物化,真相在库。省略 = 打字默认形。 */
export interface UserMeta {
  input: 'typed' | 'mic' | 'wake'
  speak: boolean
  /** 声纹识别出的家人(D 期):本回合记忆归 TA。省略 = 走会话用户。 */
  speaker_user?: number
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

export type AppEvent =
  | { type: 'task'; data: TaskView }
  | { type: 'media'; data: MediaEvent }
  // 自启回合(提醒/定时)完成:某会话有动静,UI 刷新列表/重拉当前会话
  | { type: 'conversation'; data: { conv_id: number; kind: string } }
  | { type: 'voice'; data: VoiceEvent }
  // 回合 mood(PLAN §12 修订):悬浮窗显「正在想/正在说」;主窗用自己的 per-turn mood,忽略这条
  | { type: 'mood'; data: 'idle' | 'thinking' | 'speaking' }

/** 订阅全局事件车道;未知 type 忽略(与 TurnEvent 同一增量演化约定)。 */
export function onAppEvent(cb: (ev: AppEvent) => void): void {
  if (!isTauri()) return
  void listen<AppEvent>('app_event', (e) => cb(e.payload))
}

/** 纯浏览器预览(vite dev 不在 Tauri 壳里)时为 false,VM 层据此降级假数据。 */
export function isTauri(): boolean {
  return typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window
}

// ---- 窗口 / 托盘 / 开机启动(PLAN §12):全收口在此,守"前端唯一碰 @tauri-apps/api"纪律 ----

/** 当前 WebView 窗口标签('main' | 'float');浏览器预览看 ?window= 兜底。 */
export function windowLabel(): string {
  if (!isTauri()) return new URLSearchParams(location.search).get('window') ?? 'main'
  try {
    return getCurrentWindow().label
  } catch {
    return 'main'
  }
}

/** 主窗自绘三键:无边框窗口操作,作用于当前窗口;非 Tauri 预览下 no-op。 */
export const win = {
  minimize: () => {
    if (isTauri()) void getCurrentWindow().minimize()
  },
  /** 中键:真全屏切换(用户要"全屏的感觉" —— 非 mac zoom 式最大化,那个会留系统菜单栏)。 */
  toggleFullscreen: async () => {
    if (!isTauri()) return
    const w = getCurrentWindow()
    await w.setFullscreen(!(await w.isFullscreen()))
  },
  isFullscreen: (): Promise<boolean> =>
    isTauri() ? getCurrentWindow().isFullscreen() : Promise.resolve(false),
  /** ✕ = 隐藏到托盘,不退进程(真退出走托盘菜单 quit)。 */
  hideToTray: () => {
    if (isTauri()) void getCurrentWindow().hide()
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
  /** 设窗口盒(逻辑像素):setPosition + setSize 一把搞定。 */
  setBox: async (x: number, y: number, w: number, h: number) => {
    if (!isTauri()) return
    const win = getCurrentWindow()
    await win.setPosition(new LogicalPosition(Math.round(x), Math.round(y)))
    await win.setSize(new LogicalSize(Math.round(w), Math.round(h)))
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
}

/** main 侧按 enabled 显隐悬浮窗(显示不抢焦点 —— 被动呈现)。 */
export async function setFloatVisible(visible: boolean) {
  if (!isTauri()) return
  const w = await Window.getByLabel('float')
  if (!w) return
  if (visible) await w.show()
  else await w.hide()
}

/** 当前窗口是否聚焦(主窗启动判初值用)。 */
export function isWindowFocused(): Promise<boolean> {
  return isTauri() ? getCurrentWindow().isFocused() : Promise.resolve(true)
}

/** 监听本窗聚焦变化(主窗在前藏悬浮窗、退后显;PLAN §12 E 显隐规则);返回取消订阅。 */
export function onWindowFocus(cb: (focused: boolean) => void): () => void {
  if (!isTauri()) return () => {}
  let un = () => {}
  void getCurrentWindow()
    .onFocusChanged((e) => cb(e.payload))
    .then((f) => (un = f))
  return () => un()
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

export const api = {
  boot: () => invoke<BootSnapshot>('boot'),

  /** command 立即返回;事件经 Channel 持续推送直到 done/failed/cancelled。 */
  sendMessage(convId: number, text: string, meta: UserMeta | null, onEvent: (ev: TurnEvent) => void) {
    const channel = new Channel<TurnEvent>()
    channel.onmessage = onEvent
    return invoke<void>('send_message', { convId, text, meta, onEvent: channel })
  },

  cancelGeneration: (convId: number) => invoke<void>('cancel_generation', { convId }),
  usageToday: () => invoke<DayUsage>('usage_today'),
  usageConversation: (convId: number) => invoke<UsageTotals>('usage_conversation', { convId }),
  /** 历史/提醒气泡读数(load 会话后回填,让自启回合也能 hover 看时间/token)。 */
  conversationStats: (convId: number) => invoke<MsgStats[]>('conversation_stats', { convId }),
  llmBalance: () => invoke<AccountBalance | null>('llm_balance'),
  /** 悬浮窗待机轮播:下个提醒 + 最近一句(余额/今日花费复用 llmBalance/usageToday)。 */
  floatIdle: () => invoke<FloatIdle>('float_idle'),
  newConversation: () => invoke<Conversation>('new_conversation'),
  listConversations: () => invoke<Conversation[]>('list_conversations'),
  loadConversation: (convId: number) => invoke<Message[]>('load_conversation', { convId }),
  deleteConversation: (convId: number) => invoke<void>('delete_conversation', { convId }),
  setApiKey: (key: string) => invoke<void>('set_api_key', { key }),
  setSkin: (skinId: string) => invoke<void>('set_skin', { skinId }),
  listSettings: () => invoke<SettingEntry[]>('list_settings'),
  setSetting: (key: string, value: string) => invoke<void>('set_setting', { key, value }),
  renameUser: (name: string) => invoke<User>('rename_user', { name }),
  listProviders: () => invoke<ProviderView[]>('list_providers'),
  saveProvider: (patch: ProviderPatch) => invoke<ProviderView[]>('save_provider', { patch }),
  removeProvider: (id: string) => invoke<ProviderView[]>('remove_provider', { id }),
  /** 扫码登录窗口;title 从字典取(原生窗口标题没法事后翻译)。 */
  mediaLogin: (source: string, title: string) => invoke<void>('media_login', { source, title }),
  /** 开听写:立即返回,进展走 app_event 的 voice 车道(首次会触发模型用时下载)。 */
  voiceListenStart: () => invoke<void>('voice_listen_start'),
  /** 停听写:accept = 立即定稿(已听到的送识别);false = 取消丢弃。幂等。 */
  voiceListenStop: (accept: boolean) => invoke<void>('voice_listen_stop', { accept }),
  voiceStatus: () => invoke<VoiceStatus>('voice_status'),
  /** 免手唤醒开关(写设置+起停一体;首次开会下 KWS 模型 + 预合成应答音,较慢)。 */
  voiceWakeSet: (enabled: boolean) => invoke<VoiceStatus>('voice_wake_set', { enabled }),
  /** 唤醒回合念完 → 开 6s 跟进窗(免唤醒接话)。 */
  voiceFollowUp: () => invoke<void>('voice_follow_up'),
  /** 唤醒回合失败/取消/被忽略 → 直接回待唤醒。 */
  voiceWakeResume: () => invoke<void>('voice_wake_resume'),
  /** TTS 在念(含重听)时唤醒循环丢帧(自激防护)。 */
  voiceWakeSuspend: (on: boolean) => invoke<void>('voice_wake_suspend', { on }),
  /** 家人列表(含已录声纹标记)。 */
  listFamily: () => invoke<FamilyMember[]>('list_family'),
  addFamily: (name: string) => invoke<{ id: number; name: string }>('add_family', { name }),
  removeFamily: (id: number) => invoke<void>('remove_family', { id }),
  /** 给某家人录声纹:立即返回,进展走 voice 事件(Listening→Idle),完成后重拉 list_family。 */
  voiceEnroll: (userId: number) => invoke<void>('voice_enroll', { userId }),
  /** 句级 TTS:合成进缓存(命中秒回),返回可挂 <audio> 的 localhost URL。 */
  ttsSynthesize: (text: string) => invoke<string>('tts_synthesize', { text }),
  /** 设置页音色试听(试听句出自字典——core 不产文案)。 */
  voicePreview: (speaker: string, text: string) =>
    invoke<string>('voice_preview', { speaker, text }),
  listMemories: () => invoke<Memory[]>('list_memories'),
  deleteMemory: (id: number) => invoke<void>('delete_memory', { id }),
  listBriefings: () => invoke<Briefing[]>('list_briefings'),
  deleteBriefing: (id: number) => invoke<void>('delete_briefing', { id }),
  /** 开机自启(PLAN §12):OS 是真相源,不进 DB。 */
  autostartEnabled: () => invoke<boolean>('autostart_enabled'),
  setAutostart: (on: boolean) => invoke<void>('set_autostart', { on }),
  /** 托盘菜单文案注入(§6:文案在前端字典,boot 后传给壳层建菜单)。 */
  setTrayMenu: (open: string, quit: string) => invoke<void>('set_tray_menu', { open, quit }),
}
