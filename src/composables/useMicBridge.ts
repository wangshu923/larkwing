// 浏览器采集桥(层1「采集端 AEC」,AGENT §7.5):getUserMedia({echoCancellation}) 消完
// 自播回声的麦克风流 → AudioWorklet 攒 100ms → i16 LE 推 core(voice_push_audio)。
// WebView2 = Chromium AEC3:参考信号 = 它自己在播的全部音频(TTS relay 与电影 MSE 天然在内)。
//
// 起停跟随「core 需要麦」:采集源 = browser 且(唤醒常驻 / 听写中 / 标定中 / 录声纹中)
// 才开,条件消失即停 —— 不常驻占麦。约束定值:AEC on / NS off(2026-07-06 Mac 矩阵:NS
// 对双讲无益)/ AGC off(管线自带 peak_normalize)。AudioContext 直接定 16k,浏览器内部
// 重采样(Chromium/WebKit 都支持);万一实际率不同,主线程线性重采样兜底。
// 失败自愈(§3.5 绝不静默聋):麦起不来(权限拒/无设备)→ toast + 切回 cpal + 重启唤醒。
import { watchEffect } from 'vue'
import { api, isTauri } from '../lib/backend'
import { i18n } from '../i18n'
import { useSettings } from './useSettings'
import { useToast } from './useToast'
import { useVoice } from './useVoice'
import { useWakeCalib } from './useWakeCalib'

let wired = false
let running = false
let starting = false
let currentDev = '' // 正在用的 deviceId(''=系统默认);换麦 = 停旧起新,core 推流管不动
let failedOnce = false // 自愈只做一次,避免失败循环刷 toast
let ctx: AudioContext | null = null
let stream: MediaStream | null = null
let node: AudioWorkletNode | null = null

// 攒 1600 样本(100ms @16k)再过桥:10 次 IPC/秒,单包 3.2KB
const WORKLET_SRC = `registerProcessor('lw-mic-16k', class extends AudioWorkletProcessor {
  constructor() { super(); this.buf = []; this.n = 0 }
  process(inputs) {
    const ch = inputs[0] && inputs[0][0]
    if (ch) {
      this.buf.push(ch.slice(0)); this.n += ch.length
      if (this.n >= 1600) {
        const all = new Float32Array(this.n); let o = 0
        for (const b of this.buf) { all.set(b, o); o += b.length }
        this.port.postMessage(all, [all.buffer])
        this.buf = []; this.n = 0
      }
    }
    return true
  }
})`

/** 线性重采样兜底(浏览器拒绝 16k AudioContext 时才用;质量对 AEC 后语音足够)。 */
function resampleTo16k(f32: Float32Array, from: number): Float32Array {
  if (from === 16000) return f32
  const ratio = from / 16000
  const out = new Float32Array(Math.floor(f32.length / ratio))
  for (let i = 0; i < out.length; i++) {
    const pos = i * ratio
    const lo = Math.floor(pos)
    const hi = Math.min(lo + 1, f32.length - 1)
    out[i] = f32[lo] + (f32[hi] - f32[lo]) * (pos - lo)
  }
  return out
}

function toI16Bytes(f32: Float32Array): Uint8Array {
  const out = new Int16Array(f32.length)
  for (let i = 0; i < f32.length; i++) {
    const s = Math.max(-1, Math.min(1, f32[i]))
    out[i] = Math.round(s * 32767)
  }
  return new Uint8Array(out.buffer)
}

async function start(dev: string) {
  if (running || starting) return
  starting = true
  try {
    stream = await navigator.mediaDevices.getUserMedia({
      audio: {
        echoCancellation: true,
        noiseSuppression: false,
        autoGainControl: false,
        channelCount: 1,
        // ideal(非 exact):选中的麦拔了就回系统默认,别让整条耳朵失败
        ...(dev ? { deviceId: { ideal: dev } } : {}),
      },
    })
    ctx = new AudioContext({ sampleRate: 16000 })
    const url = URL.createObjectURL(new Blob([WORKLET_SRC], { type: 'application/javascript' }))
    await ctx.audioWorklet.addModule(url)
    URL.revokeObjectURL(url)
    const src = ctx.createMediaStreamSource(stream)
    node = new AudioWorkletNode(ctx, 'lw-mic-16k')
    const rate = ctx.sampleRate
    node.port.onmessage = (e: MessageEvent<Float32Array>) => {
      const pcm = resampleTo16k(e.data, rate)
      void api.voicePushAudio(toI16Bytes(pcm)).catch(() => {})
    }
    src.connect(node) // 不接 destination:纯采集,绝不回放
    currentDev = dev
    running = true
    console.info(`[micBridge] 浏览器采集开(AEC on / NS off,ctx=${rate}Hz,dev=${dev || '默认'})`)
  } catch (e) {
    stopInner()
    void fallbackToCpal(e)
  } finally {
    starting = false
  }
}

/** 自愈回落(§3.5 绝不静默聋):浏览器麦起不来(权限拒/无设备)→ 切回系统采集(cpal)
 *  并重启唤醒换管;toast 告知。只做一次 —— 别在失败循环里刷屏。 */
async function fallbackToCpal(err: unknown) {
  console.error('[micBridge] 浏览器采集启动失败,回落系统采集(cpal)', err)
  if (failedOnce) return
  failedOnce = true
  useToast().error(i18n.global.t('toast.captureFallback'))
  try {
    await useSettings().set('voice.capture.source', 'cpal') // watchEffect 随之收摊
    const s = await api.voiceStatus()
    if (s.wakeRunning) {
      await api.voiceWakeSet(false)
      await api.voiceWakeSet(true)
    }
  } catch (e) {
    console.error('[micBridge] 回落 cpal 失败', e)
  }
}

function stopInner() {
  node?.disconnect()
  node = null
  stream?.getTracks().forEach((t) => t.stop())
  stream = null
  void ctx?.close()
  ctx = null
  if (running) console.info('[micBridge] 浏览器采集停')
  running = false
}

/** 主窗挂一次:条件驱动起停(响应设置切换 / 唤醒开关 / 听写与标定、录声纹的生命周期)。 */
export function useMicBridge() {
  if (wired) return
  wired = true
  if (!isTauri()) return
  const settings = useSettings()
  const voice = useVoice()
  const calib = useWakeCalib()
  watchEffect(() => {
    const dev = settings.get('voice.input_device_web') || '' // 依赖跟踪:设置页换麦即热重启
    const need =
      settings.get('voice.capture.source') === 'browser' &&
      (voice.state.wakeArmed ||
        voice.state.phase !== 'idle' ||
        ['preparing', 'recording'].includes(voice.state.enroll.stage) ||
        calib.state.running)
    if (!need) {
      stopInner()
      return
    }
    if (running && dev !== currentDev) stopInner() // 换麦:停旧起新(core 推流管不动,帧无缝续)
    void start(dev)
  })
}
