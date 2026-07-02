// 全局响度均衡 / 夜间模式:在播放出口(<audio>/<video>)挂一条 Web Audio 处理链,对所有播放
// 一视同仁(电影 / 歌 / DASH / HLS 混流 / 旺财的嗓音)—— 整段统一提量、限幅防炸、夜间自动压平大动态。
// 做的是「整段一起抬」,不挑人声(与用户讨论:转码后是**整体**偏小,不是对白被埋)。
//
// 链路:source → compressor(均衡:日间轻 / 夜间强)→ makeup(补偿增益)→ limiter(削尖峰防炸)→ 输出。
// 旺财的嗓音也进链(用户要求「都一起」),但**永远走日间档、绝不夜间压**——晚上你正跟它说话,得听清;
// 且它是优先声道(压别人靠既有 duck、不被压)。
//
// §8.1 兜底:Web Audio + crossorigin/CORS 在 WebView2 的表现只能 Windows 真机验。这里三重保守:
//   ① 总开关 audio.leveling 关 → 根本不接管元素、原样播放(兼作「万一某机器出问题」的一键恢复,改后重启生效);
//   ② createMediaElementSource 失败 → 不接管,原样播放;
//   ③ 建链中途失败(源已重路由)→ 源直连输出,至少不失声。
//   参数为真机可调项,这里给保守起点。

import { useSettings } from './useSettings'

const settings = useSettings()

// —— 处理参数(真机可调项 §8.1;linear makeup:1.25≈+2dB,1.5≈+3.5dB) ——
const DAY = { threshold: -20, knee: 24, ratio: 2.5, attack: 0.02, release: 0.28, makeup: 1.2 }
const NIGHT = { threshold: -34, knee: 20, ratio: 8, attack: 0.004, release: 0.4, makeup: 1.5 }
// 限幅器:近砖墙(高比率、低阈值、极快启动),只削最尖的峰值防「系统音量开大被吓到」。
const LIMITER = { threshold: -1.5, knee: 0, ratio: 20, attack: 0.002, release: 0.12 }

interface Chain {
  src: MediaElementAudioSourceNode
  comp: DynamicsCompressorNode
  makeup: GainNode
  limiter: DynamicsCompressorNode
  /** 是否跟随夜间时段:媒体=true;旺财嗓音=false(永远日间档、不被夜间压)。 */
  followNight: boolean
}

let ctx: AudioContext | null = null
// 每个元素只 createMediaElementSource 一次;元素→链,便于卸载时断开(视频元素随浮层重挂会换新)。
const chains = new Map<HTMLMediaElement, Chain>()
let ticker: ReturnType<typeof setInterval> | undefined

function levelingOn(): boolean {
  return (settings.get('audio.leveling') || '1') !== '0'
}

function parseHm(v: string): number {
  const m = /^(\d{1,2}):(\d{2})$/.exec((v || '').trim())
  if (!m) return -1
  const h = Number(m[1])
  const min = Number(m[2])
  return h < 24 && min < 60 ? h * 60 + min : -1
}

/** 现在夜间模式是否生效:off=否;on=是;auto=在 [start,end) 时段内(可跨零点)。 */
function nightActive(): boolean {
  const mode = settings.get('audio.night_mode') || 'auto'
  if (mode === 'on') return true
  if (mode !== 'auto') return false
  const s = parseHm(settings.get('audio.night_start') || '22:00')
  const e = parseHm(settings.get('audio.night_end') || '07:00')
  if (s < 0 || e < 0 || s === e) return false
  const now = new Date()
  const cur = now.getHours() * 60 + now.getMinutes()
  return s < e ? cur >= s && cur < e : cur >= s || cur < e // s>e = 跨零点(22:00→07:00)
}

function ensureCtx(): AudioContext | null {
  if (ctx) return ctx
  try {
    const AC = window.AudioContext || (window as unknown as { webkitAudioContext?: typeof AudioContext }).webkitAudioContext
    if (!AC) return null
    ctx = new AC()
  } catch {
    ctx = null
  }
  return ctx
}

function applyLimiter(n: DynamicsCompressorNode) {
  n.threshold.value = LIMITER.threshold
  n.knee.value = LIMITER.knee
  n.ratio.value = LIMITER.ratio
  n.attack.value = LIMITER.attack
  n.release.value = LIMITER.release
}

/** 把一条链切到日间/夜间档;compressor 参数瞬切(不会咔哒),makeup 增益平滑爬(防切换爆音)。 */
function applyMode(ch: Chain, night: boolean) {
  const p = night ? NIGHT : DAY
  const t = ctx ? ctx.currentTime : 0
  ch.comp.threshold.setValueAtTime(p.threshold, t)
  ch.comp.knee.setValueAtTime(p.knee, t)
  ch.comp.ratio.setValueAtTime(p.ratio, t)
  ch.comp.attack.setValueAtTime(p.attack, t)
  ch.comp.release.setValueAtTime(p.release, t)
  ch.makeup.gain.setTargetAtTime(p.makeup, t, 0.4)
}

function applyBypass(ch: Chain) {
  const t = ctx ? ctx.currentTime : 0
  ch.comp.threshold.setValueAtTime(0, t) // 0dB 阈 + 1:1 = 不压
  ch.comp.ratio.setValueAtTime(1, t)
  ch.makeup.gain.setTargetAtTime(1, t, 0.3) // 增益回 1(限幅仍留着削尖峰,无害)
}

/** 重算所有链的档位(时钟到点 / 用户改设置时调)。总开关关 → 旁路(增益回 1、不压),常态即时生效;
 *  旺财链恒日间。彻底停用(如某机器 crossorigin 出问题要一键恢复原样)则关开关后重启 app。 */
export function refreshAudioMode() {
  const on = levelingOn()
  const isNight = nightActive()
  for (const ch of chains.values()) {
    if (!on) applyBypass(ch)
    else applyMode(ch, ch.followNight && isNight)
  }
}

function attach(el: HTMLMediaElement, followNight: boolean) {
  if (!levelingOn()) return // 总开关关:不接管、也不设 crossorigin → 原样播放(§8.1 一键恢复,改后重启生效)
  if (chains.has(el)) return // 幂等:一个元素只挂一次
  const c = ensureCtx()
  if (!c) return
  // crossorigin 必须在设 src **之前**设好,Web Audio 才能不「污染静音」地接管跨源(relay 回环口)音频。
  // relay 的 /f/ /m/ /s/ 都已放行 CORS(见 relay.rs);关总开关时不设它 = 回到无 crossorigin 的原样播放。
  try {
    el.crossOrigin = 'anonymous'
  } catch {
    /* 尽力 */
  }
  // AudioContext 常以 suspended 起(自动播放策略);元素一 play(用户手势触发)就恢复。
  el.addEventListener('play', () => void c.resume().catch(() => {}))
  let src: MediaElementAudioSourceNode
  try {
    // ← 此刻起该元素音频改由本链输出,后续务必连到 destination,否则会静音。
    src = c.createMediaElementSource(el)
  } catch (e) {
    console.warn('[lw][audio] createMediaElementSource 失败,原样播放', e)
    return
  }
  try {
    const comp = c.createDynamicsCompressor()
    const makeup = c.createGain()
    const limiter = c.createDynamicsCompressor()
    applyLimiter(limiter)
    const ch: Chain = { src, comp, makeup, limiter, followNight }
    applyMode(ch, followNight && nightActive())
    src.connect(comp)
    comp.connect(makeup)
    makeup.connect(limiter)
    limiter.connect(c.destination)
    chains.set(el, ch)
    if (!ticker) ticker = setInterval(refreshAudioMode, 60_000) // 60s 轮询接住夜间时段边界
  } catch (e) {
    // 源已重路由但建链失败 → 直连输出,至少不失声(§8.1 兜底 ③)。
    console.warn('[lw][audio] 处理链构建失败,源直连输出', e)
    try {
      src.connect(c.destination)
    } catch {
      /* 尽力 */
    }
  }
}

/** 媒体(电影/歌/混流):跟随日间/夜间档。 */
export function attachMedia(el: HTMLMediaElement) {
  attach(el, true)
}

/** 旺财的嗓音:进链求一致 + 削尖峰,但恒日间档(晚上你正跟它说话,不压它);它压别人靠既有 duck。 */
export function attachTts(el: HTMLMediaElement) {
  attach(el, false)
}

/** 元素卸载(如视频浮层关闭、换新 <video>):断开并释放它的链,防泄漏与多源争抢输出。 */
export function detachAudio(el: HTMLMediaElement | null) {
  if (!el) return
  const ch = chains.get(el)
  if (!ch) return
  for (const n of [ch.src, ch.comp, ch.makeup, ch.limiter]) {
    try {
      n.disconnect()
    } catch {
      /* 已断开无妨 */
    }
  }
  chains.delete(el)
}
