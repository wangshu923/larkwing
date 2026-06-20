// 听写 VM(PLAN §11 A 期「按住说话」):订阅 voice 事件车道,驱动麦克风按钮/波形/状态。
// 编排者 = 这一层(宪法 §5 交互渠道):Transcribed 文本经回调走既有 send 链,与打字同形;
// 听写窗口 duck 自家播放器(robot capture_duck 在我们架构里就是这几行)。
// 浏览器预览降级:假电平 + 假识别文本(UI 优先工作流,?demo 不需要,点了就动)。

import { reactive } from 'vue'
import { api, isTauri, onAppEvent, onWakeChanged, type VoicePhase } from '../lib/backend'
import { useMedia } from './useMedia'

const state = reactive({
  phase: 'idle' as VoicePhase,
  /** 实时电平 0..=1(listening 期 ~10Hz),驱动波形。 */
  level: 0,
  /** 本次会话 VAD 已判到开口(提示语从"在听…"切到"嗯嗯,在记"体感)。 */
  heard: false,
  /** 上次无文本收尾的原因(no_speech 给一闪而过的轻提示);空 = 无。 */
  lastEnd: '',
  /** 唤醒交互区间(喊名命中 → 回待唤醒):duck 全程保持,UI 可标"语音会话中"。 */
  wakeActive: false,
  /** 免手唤醒此刻在跑(事实,来自 voiceStatus / lw:wake);悬浮窗待机栏据此显「等你喊…」。 */
  wakeArmed: false,
  /** 当前唤醒词(显示用;默认「小七」)。 */
  wakeKeywords: [] as string[],
})

/** Transcribed → send 链的接线口(MainLayout 注入,避免组合式互相 import)。
 *  speaker = 声纹认出的家人 user_id(D 期),记忆归 TA;undefined = 走会话用户。 */
let onText: ((text: string, via: 'mic' | 'wake', speaker?: number) => void) | null = null
export function onTranscribed(cb: (text: string, via: 'mic' | 'wake', speaker?: number) => void) {
  onText = cb
}

let wired = false
let media: ReturnType<typeof useMedia> | null = null
let duckSaved: number | null = null
let endHintTimer: ReturnType<typeof setTimeout> | undefined

/** 听写窗口压低自家播放器到 20%,收摊恢复原值(robot 验证比例,锁死)。 */
function duck() {
  if (!media || duckSaved != null) return
  duckSaved = media.state.volume
  media.setVolume(duckSaved * 0.2)
}
function restoreDuck() {
  if (media && duckSaved != null) {
    media.setVolume(duckSaved)
    duckSaved = null
  }
}

function applyPhase(p: VoicePhase) {
  state.phase = p
  if (p === 'listening') {
    state.heard = false
    state.lastEnd = ''
    duck()
  }
  if (p === 'idle') {
    state.level = 0
    // 唤醒交互区间内(回合在飞/跟进窗之间)保持 duck:电影别在它说话间隙轰回来
    if (!state.wakeActive) restoreDuck()
  }
}

/** 唤醒区间收尾(告退/跟进窗安静结束/回合周期兜底/出错):恢复外放音量。 */
const WAKE_END_REASONS = new Set(['farewell', 'follow_up_idle', 'wake_done', 'error'])

function flashEndReason(reason: string) {
  if (WAKE_END_REASONS.has(reason)) {
    state.wakeActive = false
    restoreDuck()
  }
  if (reason === 'cancelled' || reason === 'follow_up_idle' || reason === 'wake_done') return // 安静收尾不打扰
  state.lastEnd = reason
  clearTimeout(endHintTimer)
  endHintTimer = setTimeout(() => (state.lastEnd = ''), 3200)
}

function wire() {
  if (wired) return
  wired = true
  media = useMedia()
  if (!isTauri()) {
    // 浏览器预览:?demo=float 让头像显示 armed 竖耳环(纯看视觉)
    if (new URLSearchParams(location.search).get('demo')?.includes('float')) {
      state.wakeArmed = true
    }
    return
  }
  // 唤醒是"事实"(开机自启时可能已起来):启动先拉一次兜底,之后靠 lw:wake 实时跟随
  api
    .voiceStatus()
    .then((s) => {
      state.wakeArmed = s.wakeRunning
      state.wakeKeywords = s.keywords
    })
    .catch(() => {})
  onWakeChanged((running, keywords) => {
    state.wakeArmed = running
    if (keywords.length) state.wakeKeywords = keywords
  })
  onAppEvent((ev) => {
    if (ev.type !== 'voice') return
    const v = ev.data
    switch (v.type) {
      case 'state':
        applyPhase(v.data.phase)
        break
      case 'level':
        state.level = v.data.level
        break
      case 'speech_started':
        state.heard = true
        break
      case 'wake_triggered':
        // 喊名命中:全区间 duck(robot capture_duck 的扩展版——罩到回合念完)
        state.wakeActive = true
        state.lastEnd = ''
        duck()
        break
      case 'transcribed':
        onText?.(v.data.text, v.data.via === 'wake' ? 'wake' : 'mic', v.data.speaker_id)
        break
      case 'listen_ended':
        flashEndReason(v.data.reason)
        break
    }
  })
}

function start() {
  if (state.phase !== 'idle') return
  if (!isTauri()) {
    fakeListen()
    return
  }
  api.voiceListenStart().catch((e) => console.error('听写启动失败', e))
}

/** accept = 立即定稿(把已听到的送识别);false = 取消丢弃。 */
function stop(accept = true) {
  if (!isTauri()) {
    fakeStop(accept)
    return
  }
  api.voiceListenStop(accept).catch(() => {})
}

/** 麦克风按钮/快捷键共用:闲 → 开听;在听 → 立即定稿。 */
function toggle() {
  if (state.phase === 'idle') start()
  else if (state.phase === 'listening') stop(true)
}

// ---- 浏览器预览假听写(看波形/状态视觉;preparing→listening→transcribing 全过一遍) ----
let fakeLevelTimer: ReturnType<typeof setInterval> | undefined
let fakeEndTimer: ReturnType<typeof setTimeout> | undefined
function fakeListen() {
  applyPhase('preparing')
  fakeEndTimer = setTimeout(() => {
    applyPhase('listening')
    fakeLevelTimer = setInterval(() => {
      state.level = 0.12 + Math.random() * 0.8
      if (!state.heard && Math.random() < 0.25) state.heard = true
    }, 90)
    fakeEndTimer = setTimeout(() => fakeStop(true), 4200)
  }, 500)
}
function fakeStop(accept: boolean) {
  clearInterval(fakeLevelTimer)
  clearTimeout(fakeEndTimer)
  if (state.phase === 'idle') return
  if (!accept) {
    applyPhase('idle')
    return
  }
  applyPhase('transcribing')
  setTimeout(() => {
    applyPhase('idle')
    onText?.('(浏览器预览)我刚才说的话变成了这行字', 'mic', undefined)
  }, 700)
}

export function useVoice() {
  wire()
  return { state, start, stop, toggle }
}
