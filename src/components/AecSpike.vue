<script setup lang="ts">
// ⚗️ AEC spike(临时验证件,拿到真机结论就删 —— 层1「采集端浏览器 AEC」的第 0 步):
// WebView2 = Chromium,内置 WebRTC AEC3;参考信号 = Chromium 自己在播的全部音频
// (TTS relay /tts/ 与电影 MSE 都在内)。本页验:开 echoCancellation 采麦时,
// 自播声音在麦克风流里被消到什么程度 —— 决定「采集迁前端 + 删自激闸门」那一大步走不走。
// 用法:让 app 放电影 / 说话 → AEC 开/关各录一段 → 看电平差 + 听回放 + 下载 wav 对比。
// 全部浏览器 API、不碰 core;Mac 预览跑通链路,力度数字必须 Windows 真机拿(§8.1)。
import { computed, ref, onUnmounted } from 'vue'
import { useI18n } from 'vue-i18n'
import { api, isTauri } from '../lib/backend'
import { useSettings } from '../composables/useSettings'

const { t } = useI18n()
const settings = useSettings()

// —— 采集源切换(层1 接入):browser = 唤醒/听写改吃 getUserMedia 消完回声的推流。
//    切换即写设置;唤醒开着就重启一次让循环换管(restartWakeIfRunning 同款)。
const capBrowser = computed(() => settings.get('voice.capture.source') === 'browser')
const switching = ref(false)
async function toggleCapture() {
  if (switching.value) return
  switching.value = true
  try {
    await settings.set('voice.capture.source', capBrowser.value ? 'cpal' : 'browser')
    if (isTauri()) {
      const s = await api.voiceStatus()
      if (s.wakeRunning) {
        await api.voiceWakeSet(false)
        await api.voiceWakeSet(true)
      }
    }
  } catch (e) {
    console.error('切换采集源失败', e)
  } finally {
    switching.value = false
  }
}

const open = ref(false)
const aecOn = ref(true)
// 降噪独立开关(默认关):2026-07-06 Mac 实测 AEC+NS 双开会把双讲人声啃到 ASR 全灭
// (音乐消得极好但人也没了)——NS 疑似真凶,拆开单测才能归因。
const nsOn = ref(false)
const running = ref(false)
const recording = ref(false)
const level = ref(0) // 实时 RMS 0..1
const peakDb = ref(-90) // 2s 窗峰值 dBFS(回声残余最直观的读数)
const actual = ref('') // 浏览器实际生效的约束(echoCancellation 真开了没)
const err = ref('')
const wavUrl = ref('')
const recSecs = ref(0)

let ctx: AudioContext | null = null
let stream: MediaStream | null = null
let node: AudioWorkletNode | null = null
let chunks: Float32Array[] = []
let winPeak = 0
let winAt = 0
let recTimer: ReturnType<typeof setInterval> | undefined

// 采样点直通的最小 worklet(inline blob,免打包文件)
const WORKLET_SRC = `registerProcessor('lw-aec-tap', class extends AudioWorkletProcessor {
  process(inputs) {
    const ch = inputs[0] && inputs[0][0]
    if (ch) this.port.postMessage(ch.slice(0))
    return true
  }
})`

async function start() {
  err.value = ''
  try {
    stream = await navigator.mediaDevices.getUserMedia({
      audio: {
        echoCancellation: aecOn.value,
        noiseSuppression: nsOn.value,
        autoGainControl: false, // 增益锁死:电平对比才有意义
        channelCount: 1,
      },
    })
    const st = stream.getAudioTracks()[0]?.getSettings() as MediaTrackSettings & {
      echoCancellation?: boolean
      noiseSuppression?: boolean
    }
    actual.value = `echoCancellation=${st?.echoCancellation} noiseSuppression=${st?.noiseSuppression} sr=${st?.sampleRate ?? '?'}`
    ctx = new AudioContext()
    const url = URL.createObjectURL(new Blob([WORKLET_SRC], { type: 'application/javascript' }))
    await ctx.audioWorklet.addModule(url)
    URL.revokeObjectURL(url)
    const src = ctx.createMediaStreamSource(stream)
    node = new AudioWorkletNode(ctx, 'lw-aec-tap')
    node.port.onmessage = (e: MessageEvent<Float32Array>) => {
      const buf = e.data
      let sum = 0
      for (let i = 0; i < buf.length; i++) {
        const s = buf[i]
        sum += s * s
        const a = Math.abs(s)
        if (a > winPeak) winPeak = a
      }
      level.value = Math.min(1, Math.sqrt(sum / buf.length) * 6)
      const now = performance.now()
      if (now - winAt > 2000) {
        peakDb.value = Math.round(20 * Math.log10(Math.max(winPeak, 1e-5)))
        winPeak = 0
        winAt = now
      }
      if (recording.value) chunks.push(buf)
    }
    src.connect(node) // 不接 destination:纯采集,绝不把麦回放出去
    running.value = true
  } catch (e) {
    err.value = String(e)
    stop()
  }
}

function stop() {
  recording.value = false
  clearInterval(recTimer)
  node?.disconnect()
  node = null
  stream?.getTracks().forEach((tr) => tr.stop())
  stream = null
  void ctx?.close()
  ctx = null
  running.value = false
  level.value = 0
}

/** 录 15s(或手动停)→ 16-bit PCM WAV,回放/下载对比 AEC 开关的残余回声。 */
function recordToggle() {
  if (!running.value) return
  if (recording.value) {
    finishRecording()
    return
  }
  chunks = []
  recSecs.value = 0
  recording.value = true
  recTimer = setInterval(() => {
    recSecs.value += 1
    if (recSecs.value >= 15) finishRecording()
  }, 1000)
}

function finishRecording() {
  recording.value = false
  clearInterval(recTimer)
  const sr = ctx?.sampleRate ?? 48000
  const total = chunks.reduce((n, c) => n + c.length, 0)
  if (!total) return
  const pcm = new Int16Array(total)
  let o = 0
  for (const c of chunks) {
    for (let i = 0; i < c.length; i++) {
      pcm[o++] = Math.max(-32768, Math.min(32767, Math.round(c[i] * 32767)))
    }
  }
  const wav = new ArrayBuffer(44 + pcm.length * 2)
  const v = new DataView(wav)
  const w = (off: number, s: string) => {
    for (let i = 0; i < s.length; i++) v.setUint8(off + i, s.charCodeAt(i))
  }
  w(0, 'RIFF')
  v.setUint32(4, 36 + pcm.length * 2, true)
  w(8, 'WAVE')
  w(12, 'fmt ')
  v.setUint32(16, 16, true)
  v.setUint16(20, 1, true) // PCM
  v.setUint16(22, 1, true) // mono
  v.setUint32(24, sr, true)
  v.setUint32(28, sr * 2, true)
  v.setUint16(32, 2, true)
  v.setUint16(34, 16, true)
  w(36, 'data')
  v.setUint32(40, pcm.length * 2, true)
  new Int16Array(wav, 44).set(pcm)
  if (wavUrl.value) URL.revokeObjectURL(wavUrl.value)
  wavUrl.value = URL.createObjectURL(new Blob([wav], { type: 'audio/wav' }))
  chunks = []
}

async function toggleAec() {
  aecOn.value = !aecOn.value
  if (running.value) {
    stop()
    await start()
  }
}

async function toggleNs() {
  nsOn.value = !nsOn.value
  if (running.value) {
    stop()
    await start()
  }
}

onUnmounted(() => {
  stop()
  if (wavUrl.value) URL.revokeObjectURL(wavUrl.value)
})
</script>

<template>
  <div class="aec-spike">
    <button class="aec-head" @click="open = !open">
      <span class="aec-caret" :class="{ open }">▸</span>
      {{ t('settings.voice.aec.title') }}
    </button>
    <div v-if="open" class="aec-body">
      <p class="hint">{{ t('settings.voice.aec.hint') }}</p>
      <div class="aec-row">
        <button class="aec-btn" @click="running ? stop() : start()">
          {{ running ? t('settings.voice.aec.stop') : t('settings.voice.aec.start') }}
        </button>
        <button class="aec-btn" :class="{ on: aecOn }" @click="toggleAec()">
          AEC {{ aecOn ? 'ON' : 'OFF' }}
        </button>
        <button class="aec-btn" :class="{ on: nsOn }" @click="toggleNs()">
          降噪 {{ nsOn ? 'ON' : 'OFF' }}
        </button>
        <button class="aec-btn" :disabled="!running" @click="recordToggle()">
          {{ recording ? `⏹ ${recSecs}s` : t('settings.voice.aec.record') }}
        </button>
      </div>
      <div class="aec-row">
        <button class="aec-btn" :class="{ on: capBrowser }" :disabled="switching" @click="toggleCapture()">
          {{ t('settings.voice.aec.capture') }} {{ capBrowser ? 'ON' : 'OFF' }}
        </button>
        <span class="aec-read">{{ t('settings.voice.aec.captureHint') }}</span>
      </div>
      <div class="aec-meter">
        <div class="aec-fill" :style="{ width: `${Math.round(level * 100)}%` }"></div>
      </div>
      <p class="aec-read mono">peak(2s) {{ peakDb }} dBFS <span v-if="actual"> · {{ actual }}</span></p>
      <p v-if="err" class="aec-err mono">{{ err }}</p>
      <div v-if="wavUrl" class="aec-row">
        <audio :src="wavUrl" controls class="aec-audio"></audio>
        <a class="aec-btn" :href="wavUrl" :download="`aec-${aecOn ? 'on' : 'off'}-ns-${nsOn ? 'on' : 'off'}.wav`">
          {{ t('settings.voice.aec.download') }}
        </a>
      </div>
    </div>
  </div>
</template>

<style scoped>
.aec-spike { margin-top: 18px; border-top: 1px dashed var(--line); padding-top: 10px; }
.aec-head {
  background: none; border: none; color: var(--text-dim); font-size: 12px;
  cursor: pointer; padding: 2px 0; display: inline-flex; align-items: center; gap: 6px;
}
.aec-head:hover { color: var(--text); }
.aec-caret { display: inline-block; transition: transform 0.15s; }
.aec-caret.open { transform: rotate(90deg); }
.aec-body { padding: 8px 0 2px; }
.aec-row { display: flex; gap: 8px; align-items: center; margin: 8px 0; flex-wrap: wrap; }
.aec-btn {
  padding: 4px 12px; font-size: 12px; border-radius: 6px; cursor: pointer;
  border: 1px solid var(--line); background: var(--surface); color: var(--text);
}
.aec-btn:hover { border-color: var(--accent); }
.aec-btn.on { border-color: var(--accent); color: var(--accent); }
.aec-btn:disabled { opacity: 0.45; cursor: default; }
.aec-meter {
  height: 8px; border-radius: 4px; overflow: hidden;
  background: rgba(var(--accent-rgb), 0.12); max-width: 420px;
}
.aec-fill { height: 100%; background: var(--accent); transition: width 60ms linear; }
.aec-read { font-size: 11px; color: var(--text-dim); }
.aec-err { font-size: 11px; color: var(--danger); word-break: break-all; }
.aec-audio { height: 30px; max-width: 300px; }
.mono { font-family: ui-monospace, monospace; }
</style>
