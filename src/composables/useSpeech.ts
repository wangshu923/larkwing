// 朗读 VM(PLAN §11 B 期):跑道驱动切分 + 句级合成队列 + 双 <audio> 交替播放。
// 唯一调节量 = 跑道(已排队还能播多久):跑道短切得急(及时),长切得稳(韵律);
// 停顿/收尾双兜底保证"绝不断粮静场、绝不漏念短答"。参数 = PLAN §11 锁死值,不暴露。
// 队列 = 队首消费模型:current 在播(已出队),queue 待播 —— 前端 VM 是唯一播放
// 编排者,robot 的双播 dedup 问题在此结构性消失。
// 浏览器预览:无 Tauri 不出声,但切分/队列/时序全真跑(可目测切点)。

import { reactive } from 'vue'
import { api, isTauri } from '../lib/backend'
import { useSettings } from './useSettings'

// ---- 跑道切分锁死参数(PLAN §11) ----
const CPS_ZH = 4.5 // 中文语速估算:字/秒(拿到音频元数据后用真实时长校正)
const CPS_EN = 15 // 英文:字符/秒
const RUNWAY_HOT_MS = 1500
const RUNWAY_COOL_MS = 4000
const HOT_MIN = 14 // 急档:≥14 字遇任意边界切(冷启动调稳,2026-06-12 用户拍板)
const HOT_HARD = 32 // 急档:≥32 字无边界硬切("累计字数触发")
const COOL_WEAK_AT = 40
const LAZY_MIN = 20
const LAZY_WEAK_AT = 100
const LAZY_HARD = 140
const STALL_MS = 1200 // LLM 停流多久算"卡住"
const STALL_GRACE_MS = 1000 // 卡住后再宽限多久就全量硬切
const INFLIGHT_MAX = 2 // 并行合成上限(顺序入队,完成乱序无妨)
const SYNTH_TIMEOUT_MS = 12000 // 单句合成兜底:在线 TTS 偶发挂起会让队列卡死→playing 永真→唤醒被永久丢帧;超时即当失败跳过
// 克隆音色走本地 ZipVoice,CPU 合成慢(冷启 ~17s、暖 6–8s),12s 会把每句都判超时→自定义音色"不应答"。
// 选了 clone:<id> 时放宽到 45s(仍兜真挂起;串行锁下两句排队也容得下)。
const SYNTH_TIMEOUT_CLONE_MS = 45000
const PLAY_GRACE_MS = 5000 // 播放看门狗余量:真实时长 + 此值还没 ended 就强制推进

const STRONG = new Set(['。', '!', '?', '…', ';', '\n', '!', '?', ';'])
const WEAK = new Set([',', '、', ':', ',', ':', '—'])

interface Chunk {
  text: string
  status: 'pending' | 'synth' | 'ready' | 'failed'
  url?: string
  durMs?: number
}

const state = reactive({
  /** 有声播放中(驱动"正在说…"状态与停念按钮)。 */
  playing: false,
  /** 还有活没干完(回合在流/队列没播完):唤醒跟进窗等它落 false 才开。 */
  busy: false,
})

// ---- 模块内部(非响应式) ----
let queue: Chunk[] = [] // 待播(含合成中)
let current: Chunk | null = null // 正在播(已出队)
let buffer = '' // 未切出的增量文本
let turnActive = false
let inCode = false // ``` 围栏内扣住不切
let lastDeltaAt = 0
let stallTimer: ReturnType<typeof setInterval> | undefined
let players: [HTMLAudioElement, HTMLAudioElement] | null = null
let playerFlip = 0
let fakeEndTimer: ReturnType<typeof setTimeout> | undefined
let playWatchdog: ReturnType<typeof setTimeout> | undefined // 播放看门狗:ended 没触发时强制推进
const settings = useSettings()

function syncBusy() {
  state.busy = turnActive || current != null || queue.length > 0
}

/** 念话期间唤醒循环丢帧(自激防护:KWS 别把 TTS 的声音当唤醒词);去重只发翻转沿。 */
let suspendSent = false
function syncWakeSuspend() {
  if (!isTauri()) return
  if (state.playing !== suspendSent) {
    suspendSent = state.playing
    api.voiceWakeSuspend(state.playing).catch(() => {})
  }
}

function audioPair(): [HTMLAudioElement, HTMLAudioElement] {
  if (!players) {
    const mk = () => {
      const a = new Audio()
      a.preload = 'auto'
      a.addEventListener('ended', advance)
      a.addEventListener('error', advance) // 单句坏了跳过,不卡整条队列
      return a
    }
    players = [mk(), mk()]
  }
  return players
}

function ttsVolume(): number {
  const v = Number(settings.get('voice.volume') || '100')
  return Number.isFinite(v) ? Math.min(1, Math.max(0, v / 100)) : 1
}

// ---- 念前净化(法条管源头,这里管漏网;纯函数) ----
export function speakable(raw: string): string {
  let s = raw
  s = s.replace(/```[\s\S]*?```/g, ' ') // 整块代码不念
  s = s.replace(/^\s*\|.*$/gm, ' ') // 表格行不念
  s = s.replace(/!?\[([^\]]*)\]\([^)]*\)/g, '$1') // 链接/图片 → 锚文本
  s = s.replace(/https?:\/\/\S+/g, ' ') // 裸 URL 不念
  s = s.replace(/[*_~#>`]+/g, '') // 强调/标题/引用记号剥掉
  s = s.replace(/^\s*[-·•]\s+/gm, '') // 列表点
  // emoji 念不出(法条已禁〔语音〕回合用 emoji,这里兜模型漏网的);只剥图形符号/修饰符,
  // 不碰数字/#/*(Extended_Pictographic 不含这些,中文与标点也不误伤)。
  s = s.replace(/[\p{Extended_Pictographic}\u{1F3FB}-\u{1F3FF}\u{200D}\u{20E3}\u{FE0F}]/gu, '')
  return s.replace(/[ \t]+/g, ' ').replace(/\s*\n\s*/g, '\n').trim()
}

/** 在 text 里找切点(返回切到的码点数,0 = 不切)。纯函数:档位由参数表达。 */
export function findCut(
  text: string,
  opts: { min: number; weakAt: number | null; hard: number | null },
): number {
  const chars = Array.from(text)
  let firstWeakAfterMin = 0
  for (let i = 0; i < chars.length; i++) {
    const pos = i + 1
    const ch = chars[i]
    // 英文句点守卫:前不是数字、后是空白才算句界(防 3.14 / v2.0 被切;
    // 流式尾部的 '.' 后继未知,等下一个字符再判)
    let strong = STRONG.has(ch)
    if (!strong && ch === '.') {
      const prev = chars[i - 1]
      const next = chars[i + 1]
      strong = !(prev && /\d/.test(prev)) && next !== undefined && /\s/.test(next)
    }
    if (pos >= opts.min && strong) return pos
    if (WEAK.has(ch) && pos >= opts.min) {
      if (opts.weakAt != null && pos >= opts.weakAt) return pos
      if (!firstWeakAfterMin) firstWeakAfterMin = pos
    }
    if (opts.hard != null && pos >= opts.hard) {
      return firstWeakAfterMin || pos // 硬切前若有合法弱边界,宁切弱边界
    }
  }
  return 0
}

/** 估算播放时长(ms):CJK / 拉丁分速计。 */
export function estimateMs(text: string): number {
  let cjk = 0
  let latin = 0
  for (const ch of text) {
    if (/[㐀-鿿豈-﫿]/.test(ch)) cjk++
    else if (!/\s/.test(ch)) latin++
  }
  return (cjk / CPS_ZH + latin / CPS_EN) * 1000
}

/** 跑道 = 在播剩余 + 队列 + 缓冲(估算,元数据到了用真值)。 */
function runwayMs(): number {
  let ms = 0
  if (current) {
    const total = current.durMs ?? estimateMs(current.text)
    const played = isTauri() && players ? players[playerFlip].currentTime * 1000 : 0
    ms += Math.max(0, total - played)
  }
  for (const c of queue) ms += c.durMs ?? estimateMs(c.text)
  ms += estimateMs(buffer)
  return ms
}

function cutOpts(runway: number): { min: number; weakAt: number | null; hard: number | null } {
  if (runway < RUNWAY_HOT_MS) return { min: HOT_MIN, weakAt: HOT_MIN, hard: HOT_HARD }
  if (runway < RUNWAY_COOL_MS) return { min: 1, weakAt: COOL_WEAK_AT, hard: null }
  return { min: LAZY_MIN, weakAt: LAZY_WEAK_AT, hard: LAZY_HARD }
}

function enqueue(text: string) {
  const clean = speakable(text)
  if (!clean) return
  queue.push({ text: clean, status: 'pending' })
  syncBusy()
  pump()
}

function evaluate() {
  // 奇数个 ``` = 代码块没闭合,扣住整个缓冲(闭合后 speakable 会整块剥掉)
  inCode = ((buffer.match(/```/g) || []).length & 1) === 1
  if (inCode) return
  for (;;) {
    const n = findCut(buffer, cutOpts(runwayMs()))
    if (!n) break
    const chars = Array.from(buffer)
    enqueue(chars.slice(0, n).join(''))
    buffer = chars.slice(n).join('')
  }
}

/** 合成带超时:在线 TTS 偶发挂起(promise 既不 resolve 也不 reject)会让该句永停在
 *  synth、队列永不空 → playing/busy 永真 → 唤醒循环被永久 suspend 丢帧。超时即拒绝,
 *  上游 catch 把这句标 failed 跳过,队列得以继续清空。 */
function synthWithTimeout(text: string): Promise<string> {
  // 克隆音色(本地 ZipVoice)合成慢,给足时间;其余(在线 edge / 离线 melo)照旧 12s。
  const ms = settings.get('voice.speaker').startsWith('clone:')
    ? SYNTH_TIMEOUT_CLONE_MS
    : SYNTH_TIMEOUT_MS
  return Promise.race([
    api.ttsSynthesize(text),
    new Promise<string>((_, reject) => setTimeout(() => reject(new Error('synth-timeout')), ms)),
  ])
}

/** 合成泵:≤2 在飞;预览态直接标 ready(时序照真,不出声)。 */
function pump() {
  if (!isTauri()) {
    for (const c of queue) if (c.status === 'pending') c.status = 'ready'
    tryPlay()
    return
  }
  let inflight = queue.filter((c) => c.status === 'synth').length
  if (current?.status === 'synth') inflight++
  for (const c of queue) {
    if (inflight >= INFLIGHT_MAX) break
    if (c.status !== 'pending') continue
    c.status = 'synth'
    inflight++
    synthWithTimeout(c.text)
      .then((url) => {
        c.url = url
        c.status = 'ready'
        tryPlay()
        pump()
      })
      .catch((e) => {
        console.error('TTS 合成失败/超时(这句跳过,文字照常显示)', e)
        c.status = 'failed'
        tryPlay()
        pump()
      })
  }
  tryPlay()
}

function tryPlay() {
  if (current) {
    preloadNext()
    return
  }
  while (queue[0]?.status === 'failed') queue.shift() // 坏句出队
  const head = queue[0]
  if (!head || head.status !== 'ready') {
    maybeFinish()
    return
  }
  current = queue.shift()!
  state.playing = true
  syncBusy()
  syncWakeSuspend()
  if (!isTauri()) {
    fakeEndTimer = setTimeout(advance, current.durMs ?? estimateMs(current.text))
    return
  }
  playerFlip ^= 1
  const el = audioPair()[playerFlip]
  const c = current
  el.volume = ttsVolume()
  if (el.src !== c.url) el.src = c.url! // 预载命中就不重设(保住缓冲)
  // 播放看门狗:正常靠 'ended' 推进;万一 ended/error 都没触发(解码异常/被媒体抢占
  // 音频),到点强制推进——别让 current 卡住→playing 永真→唤醒被永久 suspend。
  const armWatchdog = (ms: number) => {
    clearTimeout(playWatchdog)
    playWatchdog = setTimeout(() => {
      console.warn('TTS 播放未收尾,看门狗强制推进')
      advance()
    }, ms)
  }
  armWatchdog((c.durMs ?? estimateMs(c.text)) * 1.5 + PLAY_GRACE_MS)
  el.addEventListener(
    'loadedmetadata',
    () => {
      if (Number.isFinite(el.duration)) {
        c.durMs = el.duration * 1000
        armWatchdog(c.durMs + PLAY_GRACE_MS) // 有真实时长后收紧,避免误杀长句
      }
    },
    { once: true },
  )
  void el.play().catch(() => advance())
  preloadNext()
}

/** 双元素交替:下一句 ready 就喂进另一个元素预载,句间间隙几十 ms。 */
function preloadNext() {
  if (!isTauri() || !players) return
  const next = queue[0]
  if (next?.status === 'ready' && next.url) {
    const other = players[playerFlip ^ 1]
    if (other.src !== next.url) other.src = next.url
  }
}

function advance() {
  clearTimeout(fakeEndTimer)
  clearTimeout(playWatchdog)
  current = null
  syncBusy()
  tryPlay()
}

function maybeFinish() {
  if (!turnActive && !current && queue.length === 0) {
    state.playing = false
    clearInterval(stallTimer)
    stallTimer = undefined
    syncBusy()
    syncWakeSuspend()
  }
}

/** 停顿兜底(连续性铁条):LLM 卡住且跑道告急 → 先弱边界,再宽限后全量硬切。 */
function stallTick() {
  if (!turnActive || !buffer.trim() || inCode) return
  const idle = Date.now() - lastDeltaAt
  if (idle < STALL_MS || runwayMs() >= 2000) return
  const weak = findCut(buffer, { min: 1, weakAt: 1, hard: null })
  if (weak) {
    const chars = Array.from(buffer)
    enqueue(chars.slice(0, weak).join(''))
    buffer = chars.slice(weak).join('')
  } else if (idle > STALL_MS + STALL_GRACE_MS) {
    enqueue(buffer)
    buffer = ''
  }
}

// ---- 对 useChat / UI 的接口 ----

/** 要念的回合开始:新话压旧话(先闭嘴清场)。 */
function beginTurn() {
  abort()
  turnActive = true
  syncBusy()
  lastDeltaAt = Date.now()
  if (!stallTimer) stallTimer = setInterval(stallTick, 300)
}

function pushDelta(text: string) {
  if (!turnActive) return
  buffer += text
  lastDeltaAt = Date.now()
  evaluate()
}

/** 回合收尾(done):残余缓冲无条件 flush——「好呀」无标点短答的兜底。 */
function endTurn() {
  if (!turnActive) return
  turnActive = false
  const tail = buffer
  buffer = ''
  inCode = false
  if (tail.trim()) enqueue(tail)
  syncBusy()
  maybeFinish()
}

/** 打断/失败/取消:立刻闭嘴,丢掉没念的。 */
function abort() {
  turnActive = false
  buffer = ''
  inCode = false
  queue = []
  current = null
  clearTimeout(fakeEndTimer)
  clearTimeout(playWatchdog)
  clearInterval(stallTimer)
  stallTimer = undefined
  if (players) {
    for (const el of players) {
      el.pause()
      el.removeAttribute('src')
    }
  }
  state.playing = false
  syncBusy()
  syncWakeSuspend()
}

/** 重听一条完整消息:复用流式切分管线(逐句合成播放),长文也能约 1.5s 起播,
 *  不再整段单块干等(本地 ZipVoice 克隆音色尤其明显:整段 238 字曾要 12s)。
 *  短答仍切成一两块,体感无差别;逐句缓存照样命中。 */
function speakText(full: string) {
  beginTurn()
  pushDelta(full)
  endTurn()
}

export function useSpeech() {
  return { state, beginTurn, pushDelta, endTurn, abort, speakText }
}
