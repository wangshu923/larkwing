// ViewModel(MVVM 的 VM):订阅 TurnEvent、调 commands;旺财状态机从事件流推导。
// 纯浏览器预览时自动降级为本地假流式,保住"UI 优先、浏览器热更新"的工作流。

import { reactive, watch } from 'vue'
import {
  api,
  isTauri,
  onAppEvent,
  type AccountBalance,
  type AttachmentRef,
  type Conversation,
  type DayUsage,
  type ErrorKind,
  type Message,
  type OutAttachment,
  type TraceStep,
  type TurnTrace,
  type UsageDigest,
  type UsageTotals,
} from '../lib/backend'
import { applyLocale, i18n } from '../i18n'
import { hydrateUserName, onProvidersUsable, useSettings } from './useSettings'
import { useSpeech } from './useSpeech'
import { useMedia } from './useMedia'

const t = i18n.global.t

export type Mood = 'idle' | 'thinking' | 'speaking'

/** 一条回复的读数(气泡 hover 浮现):ms = 端到端体感时长(发出→收尾,含工具轮)。 */
export interface TurnStats {
  ms: number
  input_tokens: number
  output_tokens: number
  cache_hit_tokens: number
  cost_usd: number | null
}

/** 气泡里的附件:图缩略 / 文件小票。dataUrl 仅本会话内存有(重载后历史只剩 kind/name)。 */
export interface UiAttachment {
  kind: 'image' | 'doc'
  name: string
  mime?: string
  dataUrl?: string
  /** 出站 base64(发送用,不进持久展示态)。 */
  base64?: string
}

export interface UiMessage {
  id: number
  role: 'user' | 'wang'
  text: string
  /** 仅本次会话内产生的回复有;历史消息不回填(流水在库里,要看走分析)。 */
  stats?: TurnStats
  /** 本条带过的附件(图缩略 / 文件小票)。 */
  attachments?: UiAttachment[]
  /** 「想了想」轨迹(PLAN §9):折叠药丸 + 展开技术细节(工具名/入参/结果 + CoT 原文)。空 = 不显药丸。 */
  trace?: { steps: TraceStep[]; reasoning?: string }
  /** 这条消息的时刻(unix ms):用户气泡 hover 显时间。历史取 created_at,在飞取发送时刻。 */
  at?: number
}

const state = reactive({
  ready: false,
  inTauri: false,
  hasApiKey: true,
  convId: 0,
  mood: 'idle' as Mood,
  /** 工具状态泡:i18n 键(如 'tool.remember'),空 = 没在用工具。 */
  toolAction: '',
  messages: [] as UiMessage[],
  /** 排队区(Phase A):7274 还在回话时打字发的消息攒这儿,整轮结束自动合并成下一轮发出。 */
  queue: [] as { text: string; attachments: UiAttachment[] }[],
  conversations: [] as Conversation[],
  openingLine: '', // 真值来自场景数据(boot);空时显示字典里的 fallback
  /** 记账灯带:本回合消耗(工具轮累加)/ 今日累计 / 账户余额;null = 还没数据。 */
  usage: {
    turn: null as UsageDigest | null,
    today: null as DayUsage | null,
    /** 当前话题(会话)的累计:库聚合,重启不丢、切话题跟着切。 */
    conv: null as UsageTotals | null,
    balance: null as AccountBalance | null,
    /** 在飞回合的起点(ms 时间戳);null = 没在飞。 */
    turnStartedAt: null as number | null,
    /** 在飞回合已用时长(ms),VM 时钟 5 次/秒刷新;null = 没在飞。灯带与在飞气泡共用。 */
    liveMs: null as number | null,
  },
})

let localId = -1 // 本地占位 id 用负数,与落库的正数 id 永不冲突
let turnSeq = 0 // 回合序号:流中再发会开新回合,旧回合的迟到事件按此作废,不串台(并发发送防竞态)
let turnInFlight = false // 回合是否在飞:打字发送遇它=进排队区(Phase A:整轮结束自动合并发)
// 插队(PLAN §9 B):已 inject 出去、等 Injected 事件落地成气泡的展示态(带缩略图)。FIFO 与事件序对齐。
let pendingInjects: { text: string; attachments: UiAttachment[] }[] = []
let bootStarted = false
let fakeTimer: ReturnType<typeof setTimeout> | undefined

// 设置页那边贴好钥匙 → 这边的"没钥匙"提示(composer key-row)同步消失
onProvidersUsable(() => {
  state.hasApiKey = true
})

function toUi(m: Message): UiMessage {
  const ui: UiMessage = { id: m.id, role: m.role === 'user' ? 'user' : 'wang', text: m.content, at: m.created_at }
  // 历史里的附件小票:从 user 行 payload(UserMeta)解出,显「📷/📄 名字」(无 dataUrl)
  if (m.role === 'user' && m.payload) {
    try {
      const refs = (JSON.parse(m.payload) as { attachments?: AttachmentRef[] }).attachments
      if (refs?.length) {
        ui.attachments = refs.map((a) => ({
          kind: a.kind === 'image' ? 'image' : 'doc',
          name: a.name,
          mime: a.mime,
        }))
      }
    } catch {
      /* payload 非 JSON / 旧格式:忽略 */
    }
  }
  return ui
}

/** 内部行不进聊天流:'tool' 行、'event' 行(定时触发,7274 的转述才是给人看的)、
 *  空话的 assistant 行(纯 tool_call 轮)、__IGNORE__ 行(唤醒跟进窗听到环境音,
 *  模型按法条判定"不是对我说的"——机制标记,永不示人)。 */
function visible(m: Message): boolean {
  return (
    m.role !== 'tool' &&
    m.role !== 'event' &&
    !(m.role === 'assistant' && (!m.content || m.content.trim() === '__IGNORE__'))
  )
}

/** 错误 kind → 旺财口吻的友好文案(铁律 §3.5:message 进日志,不给普通人看)。 */
function friendly(kind?: ErrorKind): string {
  const known: ErrorKind[] = ['no_api_key', 'bad_api_key', 'network']
  return kind && known.includes(kind) ? t(`err.${kind}`) : t('err.fallback')
}

function pushOpening() {
  state.messages.push({
    id: localId--,
    role: 'wang',
    text: state.openingLine || t('chat.openingFallback'),
  })
}

/** 一轮 usage 累进"本回合"(工具回合多轮);可估价的轮才累钱,估不出的由 today.unpriced 兜着。 */
function applyRound(round: UsageDigest) {
  const turn = state.usage.turn ?? {
    input_tokens: 0,
    output_tokens: 0,
    cache_hit_tokens: 0,
    cost_usd: null,
    elapsed_ms: 0,
    ttft_ms: null,
  }
  turn.input_tokens += round.input_tokens
  turn.output_tokens += round.output_tokens
  turn.cache_hit_tokens += round.cache_hit_tokens
  if (round.cost_usd != null) turn.cost_usd = (turn.cost_usd ?? 0) + round.cost_usd
  turn.elapsed_ms += round.elapsed_ms // LLM 轮时长之和(端到端体感另在 settleTurn 测)
  turn.ttft_ms ??= round.ttft_ms // 首轮的首字延迟 = 用户等到第一点动静的时间
  state.usage.turn = turn
}

/** 在飞时钟:VM 持有唯一一份,灯带和在飞气泡都只是它的视图。 */
let liveTimer: ReturnType<typeof setInterval> | undefined
function startLiveClock() {
  state.usage.liveMs = 0
  clearInterval(liveTimer)
  liveTimer = setInterval(() => {
    const t0 = state.usage.turnStartedAt
    if (t0 != null) state.usage.liveMs = Date.now() - t0
  }, 200)
}

/** 回合收尾:时钟停摆清零(空闲不显示旧值)+ 把读数钉在这条回复上(气泡 hover 浮现)。 */
function settleTurn(msg?: UiMessage) {
  clearInterval(liveTimer)
  state.usage.liveMs = null
  const started = state.usage.turnStartedAt
  state.usage.turnStartedAt = null
  if (started == null) return
  const turn = state.usage.turn
  if (msg && msg.text && turn) {
    msg.stats = {
      ms: Date.now() - started,
      input_tokens: turn.input_tokens,
      output_tokens: turn.output_tokens,
      cache_hit_tokens: turn.cache_hit_tokens,
      cost_usd: turn.cost_usd,
    }
  }
}

// —— 打字机平滑层 ——
// SSE 一块常是好几个字,直接上屏就"一段段的"。块先进码点队列,定时器按"真实流逝时间"放字:
// 速率 = max(60, 积压×4) 字/秒 —— 积压大自适应加速(约 1/4 秒追平,永不越落越远),
// 积压小逐字蹦。不用 rAF:窗口最小化/隐藏时 rAF 停摆,收尾回调会永远挂着;
// setTimeout 被节流成 1 秒一拍也没事 —— 按 dt 计步,一拍补一秒的量,收尾必达。
// 收尾(done/failed)回调挂在 finish 上,等字放完才执行 —— 结尾不"啪"地补一大段。
const tw = {
  msg: null as UiMessage | null,
  chars: [] as string[], // 按码点切,emoji 不会被劈成半个代理对
  shown: 0,
  finish: null as (() => void) | null,
  timer: undefined as ReturnType<typeof setTimeout> | undefined,
}

function twStart(msg: UiMessage) {
  tw.msg = msg
  tw.chars = []
  tw.shown = 0
  tw.finish = null
  clearTimeout(tw.timer)
  let last = performance.now()
  const loop = () => {
    if (!tw.msg) return
    const now = performance.now()
    const dt = Math.min(now - last, 1000) // 被后台节流也只按 1 秒补,不瞬移
    last = now
    const backlog = tw.chars.length - tw.shown
    if (backlog > 0) {
      const rate = Math.max(60, backlog * 4) // 字/秒
      const step = Math.min(backlog, Math.max(1, Math.round((rate * dt) / 1000)))
      tw.msg.text += tw.chars.slice(tw.shown, tw.shown + step).join('')
      tw.shown += step
    } else if (tw.finish) {
      // 存货放完且流已收尾:执行收尾回调,打字机退场
      const done = tw.finish
      tw.msg = null
      tw.finish = null
      done()
      return
    }
    tw.timer = setTimeout(loop, 16)
  }
  tw.timer = setTimeout(loop, 16)
}

function twPush(text: string) {
  if (tw.msg) tw.chars.push(...Array.from(text))
}

/** 流收尾:字放完后执行 cb(没起过打字机就直接执行)。 */
function twEnd(cb: () => void) {
  if (!tw.msg) {
    cb()
    return
  }
  tw.finish = cb
}

/** 立刻放完剩余的字并执行挂着的收尾(切会话/新回合/取消等不等动画的场合)。 */
function twFlush() {
  if (!tw.msg) return
  clearTimeout(tw.timer)
  if (tw.shown < tw.chars.length) tw.msg.text += tw.chars.slice(tw.shown).join('')
  tw.shown = tw.chars.length
  const done = tw.finish
  tw.msg = null
  tw.finish = null
  done?.()
}

/** 话题累计(库聚合):开机/切话题取初值;之后随 TurnEvent::Usage 的快照刷新。 */
function loadConvUsage(convId: number) {
  state.usage.conv = null // 先熄灯,别让旧话题的数挂在新话题上
  if (!state.inTauri) return
  api
    .usageConversation(convId)
    .then((c) => {
      if (state.convId === convId) state.usage.conv = c // 查询期间又切走了就别写
    })
    .catch(() => {})
}

/** 历史/提醒/自启回合的气泡读数(PLAN §11 D):load 会话后从库回填 stats,让这些气泡
 *  也能 hover 看时间/token(在飞回合走 TurnEvent::Usage 实时常显,不经这里)。 */
async function hydrateStats(convId: number) {
  if (!state.inTauri) return
  try {
    const list = await api.conversationStats(convId)
    if (state.convId !== convId) return // 查询期间切走了,别把旧账写到新话题
    const map = new Map(list.map((s) => [s.message_id, s]))
    for (const m of state.messages) {
      // 负 id = 本地占位(开场白),无库读数;只回填落库的 assistant 气泡
      if (m.role === 'wang' && m.id > 0 && map.has(m.id)) {
        const s = map.get(m.id)!
        m.stats = {
          ms: s.ms,
          input_tokens: s.input_tokens,
          output_tokens: s.output_tokens,
          cache_hit_tokens: s.cache_hit_tokens,
          cost_usd: s.cost_usd,
        }
      }
    }
  } catch (e) {
    console.error('读取历史读数失败', e)
  }
}

/** 历史/自启回合的「想了想」轨迹(PLAN §9):load 会话后回填到代表气泡
 *  (在飞回合由 TurnEvent 实时攒,不走这条)。 */
async function hydrateTrace(convId: number) {
  if (!state.inTauri) return
  try {
    const list = await api.conversationTrace(convId)
    if (state.convId !== convId) return
    const map = new Map(list.map((tr) => [tr.message_id, tr]))
    for (const m of state.messages) {
      if (m.role === 'wang' && m.id > 0 && map.has(m.id)) {
        const tr = map.get(m.id)!
        m.trace = { steps: tr.steps, reasoning: tr.reasoning ?? undefined }
      }
    }
  } catch (e) {
    console.error('读取思考轨迹失败', e)
  }
}

/** 余额是锦上添花:节流轻刷(回合后/开机),失败静默保留旧值。 */
let balanceFetchedAt = 0
function refreshBalance(force = false) {
  if (!state.inTauri || !state.hasApiKey) return
  if (!force && Date.now() - balanceFetchedAt < 60_000) return
  balanceFetchedAt = Date.now()
  api
    .llmBalance()
    .then((b) => {
      if (b) state.usage.balance = b
    })
    .catch(() => {})
}

/** 没有可用 LLM(没钥匙/全停用)时的开机提示:只展示不落库,引导到钥匙行或设置页。 */
function pushNoLlmHint() {
  state.messages.push({ id: localId--, role: 'wang', text: t('chat.noLlm') })
}

async function boot() {
  if (bootStarted) return
  bootStarted = true
  state.inTauri = isTauri()

  if (!state.inTauri) {
    // 浏览器预览:假数据,纯看视觉;?nokey 可预览"没接上大脑"的形态
    state.hasApiKey = !new URLSearchParams(location.search).has('nokey')
    state.convId = 1
    state.conversations = [
      { id: 1, user_id: 1, scene_id: 'companion', title: '今天的碎碎念', created_at: 0, updated_at: Date.now() },
      { id: 2, user_id: 1, scene_id: 'companion', title: '周末去哪儿玩', created_at: 0, updated_at: Date.now() - 86400_000 },
    ]
    pushOpening()
    if (!state.hasApiKey) pushNoLlmHint()
    if (state.hasApiKey) {
      // 灯带的假读数(纯看视觉)
      state.usage.today = {
        date: new Date().toISOString().slice(0, 10),
        input_tokens: 48_320,
        output_tokens: 9_154,
        cache_hit_tokens: 39_800,
        cost_usd: 0.0173,
        unpriced: false,
      }
      state.usage.balance = { currency: 'CNY', amount: '9.84' }
    }
    state.ready = true
    return
  }

  // 自启回合(提醒/定时)完成的动静:开着该会话且空闲就重拉消息,列表顺手刷新
  onAppEvent((ev) => {
    if (ev.type !== 'conversation') return
    refreshConversations()
    if (ev.data.conv_id === state.convId && state.mood === 'idle') {
      api
        .loadConversation(state.convId)
        .then((msgs) => {
          state.messages = msgs.filter(visible).map(toUi)
          void hydrateStats(state.convId) // 提醒/自启回合的气泡也带上 hover 读数
          void hydrateTrace(state.convId) // …和「想了想」轨迹
          // 提醒到点自动开口(PLAN §11 B 期):设备主动叫人,off 档才闭嘴;
          // 回合没标〔语音〕(屏幕排版),念之前净化兜底。跨会话提醒念话 C 期随唤醒流程
          const last = msgs.filter(visible).at(-1)
          if (
            ev.data.kind === 'reminder' &&
            last &&
            last.role === 'assistant' &&
            useSettings().get('voice.auto_speak') !== 'off'
          ) {
            useSpeech().speakText(last.content)
          }
        })
        .catch(() => {})
    }
  })

  try {
    const snap = await api.boot()
    applyLocale(snap.locale) // 用户级语言(与皮肤同款),core 只过桥不产文案
    hydrateUserName(snap.user.name) // 设置页"现在陪着"用,单向过桥
    state.hasApiKey = snap.hasApiKey
    state.convId = snap.conversation.id
    state.messages = snap.messages.filter(visible).map(toUi)
    if (snap.openingLine) {
      state.openingLine = snap.openingLine
      pushOpening() // 空会话给开场白(只展示,不落库)
    }
    if (!snap.hasApiKey) pushNoLlmHint() // 没有可用大脑:开机就说,不等用户撞墙
    state.conversations = await api.listConversations()
    // 灯带初值:今日账本 + 话题累计 + 余额(都是锦上添花,失败静默不挡开机)
    api.usageToday().then((d) => (state.usage.today = d)).catch(() => {})
    loadConvUsage(snap.conversation.id)
    void hydrateStats(snap.conversation.id) // 首屏历史气泡的 hover 读数
    void hydrateTrace(snap.conversation.id) // 首屏历史气泡的「想了想」轨迹
    refreshBalance(true)
  } catch (e) {
    console.error('boot 失败', e)
    state.messages.push({ id: localId--, role: 'wang', text: friendly('internal') })
  }
  state.ready = true
}

async function refreshConversations() {
  if (!state.inTauri) return
  try {
    state.conversations = await api.listConversations()
  } catch (e) {
    console.error('刷新会话列表失败', e)
  }
}

/** 回合任一出口收尾:解除"在飞" + 把排队区攒的消息合并成下一轮发出(Phase A:整轮结束自动发)。 */
function settleInFlight() {
  turnInFlight = false
  flushQueue()
}
/** 把排队区攒的消息合并成一条作为下一轮发出(文本换行拼接,附件并起来);在飞或空则不动。 */
function flushQueue() {
  if (turnInFlight || !state.queue.length) return
  const items = state.queue.splice(0, state.queue.length)
  const text = items.map((it) => it.text).filter(Boolean).join('\n')
  const attachments = items.flatMap((it) => it.attachments)
  send(text, 'typed', undefined, attachments)
}
/** 排队区移除一条(还没发出前反悔)。 */
function dequeue(i: number) {
  state.queue.splice(i, 1)
}

/** 插队(PLAN §9 B):把排队区合并成一条塞进正在跑的回合,下一轮 LLM 就带上(不打断)。
 *  后端没接住(没在飞 / 回合正收尾)→ 退化为普通发送起新回合。 */
function inject() {
  if (!state.queue.length) return
  const items = state.queue.splice(0, state.queue.length)
  const text = items.map((it) => it.text).filter(Boolean).join('\n')
  const attachments = items.flatMap((it) => it.attachments)
  if (!state.inTauri) {
    // 浏览器预览无真引擎/在飞回合:退化为普通发送(fakeStream 单轮,插队≈追加)
    send(text, 'typed', undefined, attachments)
    return
  }
  const out: OutAttachment[] = attachments
    .filter((a) => a.base64)
    .map((a) => ({ name: a.name, mime: a.mime || '', data: a.base64! }))
  const stash = { text, attachments } // 展示态(带缩略图)暂存,等 Injected 事件落地成气泡
  pendingInjects.push(stash)
  const fallback = () => {
    const i = pendingInjects.indexOf(stash)
    if (i >= 0) pendingInjects.splice(i, 1) // 撤回暂存
    send(text, 'typed', undefined, attachments) // 改普通发送起新回合
  }
  api
    .injectMessage(state.convId, text, { input: 'typed', speak: false }, out)
    .then((delivered) => {
      if (!delivered) fallback()
    })
    .catch(fallback)
}

function send(
  text: string,
  source: 'typed' | 'mic' | 'wake' = 'typed',
  speaker?: number,
  attachments: UiAttachment[] = [],
) {
  const content = text.trim()
  if (!content && attachments.length === 0) return

  // 排队(Phase A):7274 还在回话时,打字发送不打断 → 进排队区;整轮结束后自动合并成下一轮发出。
  // (语音 mic/wake 不排队:开口即应,沿用既有打断。)
  if (turnInFlight && source === 'typed') {
    state.queue.push({ text: content, attachments })
    return
  }

  // 本回合序号:流中再发时,engine 会先取消旧回合 → 旧 channel 迟到的 cancelled/残余 delta
  // 会回调进来。它们与新回合共用 tw/mood/clock,不挡住会把新回合的打字机拆了(新回复变空泡)。
  // 用它给每个回调闸一道:不是当前回合就直接丢弃。
  const myTurn = ++turnSeq

  // 语音会话模式(PLAN §11 二分):打字/按钮说话 = UI 交互,默认不念;
  // wake = 语音交互必念(off 档不管它——喊它就是要它说话);always 档 = 全念。
  const speech = useSpeech()
  const speak = source === 'wake' || useSettings().get('voice.auto_speak') === 'always'
  if (speak) speech.beginTurn()
  else speech.abort() // 新回合开口前,先停上一段念话(新话压旧话)

  // 唤醒回合的收尾编排:念完 → 开跟进窗;失败/取消/被忽略 → 直接回待唤醒
  const wakeTurn = source === 'wake'
  let wakeSettled = false
  const wakeFollowUp = () => {
    if (!wakeTurn || wakeSettled) return
    wakeSettled = true
    // 等念完(busy 落 false)再开跟进窗——core 在 AwaitTurn 丢帧,不会自激
    const stop = watch(
      () => speech.state.busy,
      (busy) => {
        if (busy) return
        stop()
        api.voiceFollowUp().catch(() => {})
      },
      { immediate: true },
    )
  }
  const wakeResume = () => {
    if (!wakeTurn || wakeSettled) return
    wakeSettled = true
    api.voiceWakeResume().catch(() => {})
  }
  let fullText = '' // __IGNORE__ 判定要看全量(打字机还没放完的字不算数)
  let turnHadTrace = false // 本回合动过工具或脑 → done 后补拉 trace(详情从落库 payload 重建)

  twFlush() // 上一回合的字还在放:立刻放完并收尾,别让两台打字机打架
  state.messages.push({
    id: localId--,
    role: 'user',
    text: content,
    at: Date.now(),
    // 立即显:图缩略 + 文件小票(base64 不进展示态,只随发送走)
    attachments: attachments.length
      ? attachments.map((a) => ({ kind: a.kind, name: a.name, mime: a.mime, dataUrl: a.dataUrl }))
      : undefined,
  })
  state.messages.push({ id: localId--, role: 'wang', text: '' })
  // 必须从 reactive 数组取回代理再改:reactive() 是惰性代理,push 存的是裸对象,
  // 直接改裸对象 Vue 不追踪 —— 流式逐字、deep watch、物理避让全都会失灵
  let wang = state.messages[state.messages.length - 1] // 插队会令它重指到新回复气泡
  state.mood = 'thinking'
  state.usage.turn = null // 新回合,灯带"本轮"读数清零
  state.usage.turnStartedAt = Date.now() // 端到端计时:从用户按下发送起(体感真值)
  startLiveClock()
  twStart(wang)
  turnInFlight = true // 占住"在飞":后续打字发送进排队区,直到本轮收尾

  if (!state.inTauri) {
    fakeStream(wang, `(浏览器预览)滴——收到「${content}」!进 Tauri 壳里我就接上真模型咯!`, speak)
    return
  }

  const sentConv = state.convId // 话题快照只认发送时的会话:流中切走了别把旧账写到新话题上
  const outAtts: OutAttachment[] = attachments
    .filter((a) => a.base64)
    .map((a) => ({ name: a.name, mime: a.mime || '', data: a.base64! }))
  api
    .sendMessage(state.convId, content, { input: source, speak, speaker_user: speaker }, outAtts, (ev) => {
      if (myTurn !== turnSeq) return // 旧回合的迟到事件:新回合已接管,丢弃不串台
      switch (ev.type) {
        case 'delta':
          fullText += ev.data
          twPush(ev.data) // 进平滑缓冲,rAF 逐字上屏
          if (speak) speech.pushDelta(ev.data) // 跑道切分流水线(念与显示各走各的节奏)
          state.mood = 'speaking'
          state.toolAction = '' // 开口了,工具泡退场
          break
        case 'tool_use':
          // label 是 i18n 键;它在场时旺财保持"思考中"的体感
          if (ev.data.state === 'started') {
            state.toolAction = ev.data.label
            turnHadTrace = true // 这回合有工具步骤 → done 后补拉 trace
          }
          state.mood = 'thinking'
          break
        case 'thinking':
          turnHadTrace = true
          state.mood = 'thinking'
          break
        case 'injected': {
          // 插队被回合接住(PLAN §9 B):收尾当前回复气泡 → 插用户气泡 → 开新回复气泡(后续 delta 进新泡)
          twFlush() // 当前 wang 已收到的字放完(它答的是上一段输入)
          const stash = pendingInjects.shift()
          state.messages.push({
            id: ev.data.message_id,
            role: 'user',
            text: ev.data.text,
            at: Date.now(),
            attachments:
              stash && stash.attachments.length
                ? stash.attachments.map((a) => ({ kind: a.kind, name: a.name, mime: a.mime, dataUrl: a.dataUrl }))
                : undefined,
          })
          state.messages.push({ id: localId--, role: 'wang', text: '' })
          wang = state.messages[state.messages.length - 1] // 重指到新回复气泡
          twStart(wang)
          fullText = '' // 新一段回复,__IGNORE__ 判定重置
          turnHadTrace = false
          state.mood = 'thinking'
          break
        }
        case 'usage':
          applyRound(ev.data.round)
          state.usage.today = ev.data.today // 后端账本快照,天然防双计/跨午夜
          if (state.convId === sentConv) state.usage.conv = ev.data.conv
          break
        case 'done':
          // __IGNORE__(唤醒跟进窗的环境音):吞泡不念,直接回待唤醒
          if (fullText.trim() === '__IGNORE__') {
            speech.abort()
            twFlush()
            state.messages.pop() // wang 空泡
            if (state.messages.at(-1)?.role === 'user') state.messages.pop() // 环境音"用户"泡一起吞
            settleTurn()
            state.mood = 'idle'
            state.toolAction = ''
            wakeResume()
            settleInFlight()
            break
          }
          if (speak) speech.endTurn() // 残余缓冲 flush(音频节奏独立于打字机)
          wakeFollowUp() // 唤醒回合:等念完开跟进窗(非唤醒 = no-op)
          // 收尾等字放完再生效:状态/读数与画面同步,结尾不"啪"地补一大段
          twEnd(() => {
            wang.id = ev.data.message_id // 流式文本与落库消息对账
            if (turnHadTrace) void hydrateTrace(sentConv) // 「想了想」详情从落库 payload 补拉
            settleTurn(wang) // 端到端时长定格,读数钉在气泡上
            state.mood = 'idle'
            state.toolAction = ''
            refreshConversations() // 标题/排序可能变了
            refreshBalance() // 花了钱,余额顺手轻刷(节流)
            settleInFlight() // 整轮结束:把排队区攒的合并成下一轮发出
          })
          break
        case 'failed':
          speech.abort() // 出错即闭嘴,别把半截话念完
          wakeResume() // 唤醒回合出错:回待唤醒,别让 core 干等
          twFlush() // 出错不演戏:先把已收到的字放完
          wang.text = wang.text
            ? wang.text + '\n' + t('chat.interrupted')
            : friendly(ev.data.kind)
          settleTurn() // 失败的半截话不钉读数,但计时归零
          state.mood = 'idle'
          state.toolAction = ''
          settleInFlight()
          break
        case 'cancelled':
          speech.abort()
          wakeResume()
          twFlush() // engine 已把收到的整段落库,画面同步到全量,与库一致
          if (!wang.text) state.messages.pop() // 一个字没说就被打断:别留空气泡
          settleTurn()
          state.mood = 'idle'
          state.toolAction = ''
          settleInFlight() // 停止后排队的仍发出(它们是你还想要的;不想要就在排队区划掉)
          break
      }
    })
    .catch((e) => {
      if (myTurn !== turnSeq) return // 旧回合的迟到失败:新回合已接管,忽略
      // 前置错误(AppError):没 key / 会话不存在 / 建连失败
      speech.abort()
      wakeResume()
      twFlush()
      wang.text = friendly((e as { kind?: ErrorKind })?.kind)
      settleTurn()
      state.mood = 'idle'
      settleInFlight()
    })
}

/** 停止按钮:取消在飞回合(幂等)+ 停念。半截话由 engine 落库。 */
function cancel() {
  useSpeech().abort() // 念话即停(turn 已结束、只剩音频在播的场合也走这条)
  if (!state.inTauri) {
    clearTimeout(fakeTimer)
    twFlush()
    settleTurn()
    state.mood = 'idle'
    settleInFlight()
    return
  }
  api.cancelGeneration(state.convId).catch(() => {})
}

async function selectConversation(convId: number) {
  if (convId === state.convId) return
  twFlush() // 旧会话还在放字:立刻收尾,别让打字机写进看不见的孤儿气泡
  state.queue.splice(0) // 换话题:排队区清空(它属于旧会话的在飞流程)
  turnInFlight = false
  state.convId = convId
  loadConvUsage(convId) // 灯带"话题"段跟着切
  if (!state.inTauri) return
  try {
    const msgs = await api.loadConversation(convId)
    state.messages = msgs.filter(visible).map(toUi)
    void hydrateStats(convId) // 切回的历史会话:气泡 hover 读数从库回填
    void hydrateTrace(convId) // …和「想了想」轨迹
    if (!msgs.length) pushOpening()
    state.mood = 'idle'
  } catch (e) {
    console.error('加载会话失败', e)
  }
}

async function newConversation() {
  twFlush()
  state.queue.splice(0) // 新话题:排队区清空
  turnInFlight = false
  state.usage.conv = null // 新话题还没花过,熄灯
  if (!state.inTauri) {
    state.messages = []
    pushOpening()
    return
  }
  try {
    const conv = await api.newConversation()
    state.convId = conv.id
    state.messages = []
    pushOpening()
    await refreshConversations()
  } catch (e) {
    console.error('新建会话失败', e)
  }
}

// ---- 语音(免手唤醒)专属会话(PLAN §11):语音交互不灌进当前打字会话,自起一个;
// 活着就续,「完全无动作」满 30 分钟才弃——媒体在播算动作(看电影不打断续接),停播
// 后才开始计时;重启不接续(纯内存态)。喊醒说话时切到它显示,它照常进会话列表、可点
// 进去打字接力(交互二分:wake=语音会话,mic 按住说话/打字=当前会话)。----
const VOICE_CONV_TTL = 30 * 60 * 1000
let voiceConvId: number | null = null
let lastVoiceActivity = 0
let voiceWired = false

function mediaBusy(): boolean {
  const s = useMedia().state.status
  // 暂停也算「还在看」(用户确认):暂停不开始计时,会话续着。
  return s === 'playing' || s === 'loading' || s === 'paused'
}

/** 只有「停止」(回到 idle:手动停止 或 电影自然播完)才记一笔活动、从此刻起算那
 *  30 分钟;暂停(paused)视为还在看,不计时(用户确认的口径)。 */
function wireVoiceActivity() {
  if (voiceWired) return
  voiceWired = true
  const media = useMedia()
  watch(
    () => media.state.status,
    (s, prev) => {
      if (s === 'idle' && prev !== 'idle') lastVoiceActivity = Date.now()
    },
  )
}

/** 喊醒说话前调:有活的语音会话→切过去续;无/过期→新起一个。两种都切到它显示。 */
async function ensureVoiceConv() {
  const alive =
    voiceConvId != null &&
    state.conversations.some((c) => c.id === voiceConvId) &&
    (mediaBusy() || Date.now() - lastVoiceActivity <= VOICE_CONV_TTL)
  if (alive) {
    if (state.convId !== voiceConvId) await selectConversation(voiceConvId!)
  } else {
    await newConversation()
    voiceConvId = state.convId
  }
  lastVoiceActivity = Date.now()
}

async function saveApiKey(key: string) {
  if (!state.inTauri) return
  try {
    await api.setApiKey(key)
    state.hasApiKey = true
    state.messages.push({ id: localId--, role: 'wang', text: t('key.saved') })
    refreshBalance(true) // 钥匙刚接上,灯带亮余额
  } catch (e) {
    state.messages.push({ id: localId--, role: 'wang', text: friendly((e as { kind?: ErrorKind })?.kind) })
  }
}

// 浏览器预览的假流式:按"SSE 块"的粒度隔一阵推一坨(模拟真流的网络形状),
// 平滑层负责逐字蹦 —— 预览看到的就是真流的体感;speak 态连切分器一起真跑(不出声)
function fakeStream(msg: UiMessage, full: string, speak = false) {
  const speech = useSpeech()
  state.mood = 'speaking'
  clearTimeout(fakeTimer)
  const chars = Array.from(full)
  let i = 0
  const pushChunk = () => {
    const n = 6 + Math.floor(Math.random() * 18)
    const piece = chars.slice(i, i + n).join('')
    twPush(piece)
    if (speak) speech.pushDelta(piece)
    i += n
    if (i < chars.length) {
      fakeTimer = setTimeout(pushChunk, 120 + Math.random() * 200)
      return
    }
    if (speak) speech.endTurn()
    twEnd(() => {
      state.mood = 'idle'
      // 假记账:灯带/气泡读数在纯浏览器预览里也有活数据可看
      const round = {
        input_tokens: 800 + Math.floor(Math.random() * 600),
        output_tokens: chars.length * 2,
        cache_hit_tokens: 512,
        cost_usd: 0.0006,
        elapsed_ms: chars.length * 30,
        ttft_ms: 220,
      }
      applyRound(round)
      if (state.usage.today) {
        state.usage.today.input_tokens += round.input_tokens
        state.usage.today.output_tokens += round.output_tokens
        state.usage.today.cache_hit_tokens += round.cache_hit_tokens
        state.usage.today.cost_usd += round.cost_usd
      }
      // 话题累计:预览里手动滚(真流由后端快照)
      const conv = state.usage.conv ?? {
        input_tokens: 0,
        output_tokens: 0,
        cache_hit_tokens: 0,
        cost_usd: 0,
        unpriced_rounds: 0,
      }
      conv.input_tokens += round.input_tokens
      conv.output_tokens += round.output_tokens
      conv.cache_hit_tokens += round.cache_hit_tokens
      conv.cost_usd += round.cost_usd
      state.usage.conv = conv
      settleTurn(msg)
      settleInFlight() // 假流收尾也走同一收口:排队区有就接着发(浏览器预览能验排队)
    })
  }
  pushChunk()
}

export function useChat() {
  void boot()
  wireVoiceActivity()
  return { state, send, cancel, selectConversation, newConversation, ensureVoiceConv, saveApiKey, dequeue, inject }
}
