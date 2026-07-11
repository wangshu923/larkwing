// 听写 VM(PLAN §11 A 期「按住说话」):订阅 voice 事件车道,驱动麦克风按钮/波形/状态。
// 编排者 = 这一层(宪法 §5 交互渠道):Transcribed 文本经回调走既有 send 链,与打字同形;
// 听写窗口 duck 自家播放器(robot capture_duck 在我们架构里就是这几行)。
// 浏览器预览降级:假电平 + 假识别文本(UI 优先工作流,?demo 不需要,点了就动)。

import { reactive } from 'vue'
import { api, isTauri, onAppEvent, onWakeChanged, type VoicePhase } from '../lib/backend'
import { useMedia } from './useMedia'
import { useSpeech } from './useSpeech'

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
  /** 确认层在核(KWS 候选 → 三段式定夺):轻视觉「在听」,不出声;拒绝/定夺即灭。 */
  candidate: false,
  /** 免手唤醒此刻在跑(事实,来自 voiceStatus / lw:wake);悬浮窗待机栏据此显「等你喊…」。 */
  wakeArmed: false,
  /** 当前唤醒词(显示用;= 名字派生,单源在后端 voice::wake_keywords)。 */
  wakeKeywords: [] as string[],
  /** 声纹注册进展(家人页录声纹,D 期第二步):驱动对应家人卡的「准备中/第N遍/成功/失败」。
   *  userId=0 → 没有进行中;stage='' | preparing | recording | saved | failed。 */
  enroll: { userId: 0, stage: '' as string, done: 0, total: 0 },
})

/** 声纹注册终态(saved/failed)回调:SettingsView 注入 → toast + 重拉家人 + 重启唤醒。 */
let onEnrollDoneCb: ((userId: number, ok: boolean) => void) | null = null
export function onEnrollDone(cb: (userId: number, ok: boolean) => void) {
  onEnrollDoneCb = cb
}

/** Transcribed → send 链的接线口(MainLayout 注入,避免组合式互相 import)。
 *  speaker = 声纹认出的家人 user_id(D 期),记忆归 TA;undefined = 走会话用户。 */
let onText: ((text: string, via: 'mic' | 'wake', speaker?: number) => void) | null = null
export function onTranscribed(cb: (text: string, via: 'mic' | 'wake', speaker?: number) => void) {
  onText = cb
}

/** 旁听(呼名+续句)→ sendOverheard 的接线口(MainLayout 注入,同 onTranscribed 手法):
 *  由它决定目标会话(语音会话/当前桌面会话),这里不认识 useChat。 */
let onOverheardCb: ((text: string, speaker?: number) => void) | null = null
export function onOverheard(cb: (text: string, speaker?: number) => void) {
  onOverheardCb = cb
}

let wired = false
let media: ReturnType<typeof useMedia> | null = null
let endHintTimer: ReturnType<typeof setTimeout> | undefined

/** 听写窗口压低自家播放器到 20%(robot 验证比例,锁死);收摊恢复。压低/恢复只切折算系数,
 *  不动基准音量 → 期间用户「大点声」改的是基准、恢复后生效(不再被无脑还原冲掉)。幂等。 */
function duck() {
  media?.setDucked(true)
}
function restoreDuck() {
  media?.setDucked(false)
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

/** 空闲(非唤醒区间、非听写)才恢复 duck:确认层拒绝/旁听蒸发后归位用。 */
function maybeRestoreDuck() {
  if (!state.wakeActive && state.phase === 'idle') restoreDuck()
}

// —— 旁听 duck 生命周期:候选时提前压低(确认层的 ASR 别被电影轰),之后三条路 ——
// ① 三段式判成经典唤醒 → wake_triggered 接管(既有 wakeActive 区间);
// ② 判幻听 wake_rejected / 仲裁蒸发 overheard_dismissed → 恢复;
// ③ 仲裁转正 overheard → 等念完再恢复(轮询 useSpeech;30s 兜底防吊死)。
let overheardGuard: ReturnType<typeof setTimeout> | undefined
let speechWait: ReturnType<typeof setInterval> | undefined
function clearOverheardTimers() {
  clearTimeout(overheardGuard)
  clearInterval(speechWait)
  overheardGuard = speechWait = undefined
}
function restoreAfterSpeech() {
  clearOverheardTimers()
  const speech = useSpeech()
  const startedAt = Date.now()
  speechWait = setInterval(() => {
    const talking = speech.state.playing || speech.state.busy
    if (!talking || Date.now() - startedAt > 30_000) {
      clearOverheardTimers()
      maybeRestoreDuck()
    }
  }, 300)
}

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
    // 旁听终态(engine 内消费的临时回合,经会话车道回来):恢复 duck 的另一半在这
    if (ev.type === 'conversation') {
      const k = ev.data.kind
      if (k === 'overheard_dismissed') {
        clearOverheardTimers()
        maybeRestoreDuck()
      } else if (k === 'overheard') {
        restoreAfterSpeech() // 转正:回应要念,念完再恢复外放
      }
      return
    }
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
      case 'wake_candidate':
        // 确认层在核:提前 duck(压低电影给 ASR 让路)+ 轻视觉「在听」,不出声
        state.candidate = true
        duck()
        break
      case 'wake_rejected':
        // 幻听拒绝:零打扰归位
        state.candidate = false
        maybeRestoreDuck()
        break
      case 'wake_running':
        // core 的权威开关广播(boot 自动恢复/停/意外退出都发):armed 与 mic bridge
        // (browser 采集源的开麦条件)跟它走。治「开机后开关显示开、叫不答应」——
        // 启动那次 voiceStatus 兜底查询常赶在 core wake_start(加载模型,秒级)完成前,
        // armed 定格 false → 永不开麦;现在 core 起来那刻会推这条(2026-07-11 真机实锤)。
        state.wakeArmed = v.data.running
        if (v.data.keywords.length) state.wakeKeywords = v.data.keywords
        break
      case 'overheard':
        // 呼名+续句 → 交模型仲裁;duck 保持,30s 兜底(仲裁挂了也别永远压着电影)
        state.candidate = false
        clearOverheardTimers()
        overheardGuard = setTimeout(maybeRestoreDuck, 30_000)
        onOverheardCb?.(v.data.text, v.data.speaker_id)
        break
      case 'wake_triggered':
        // 喊名命中:全区间 duck(robot capture_duck 的扩展版——罩到回合念完)
        state.candidate = false
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
      case 'enroll': {
        const e = v.data
        state.enroll = { userId: e.user_id, stage: e.stage, done: e.done ?? 0, total: e.total ?? 0 }
        if (e.stage === 'saved' || e.stage === 'failed') onEnrollDoneCb?.(e.user_id, e.stage === 'saved')
        break
      }
    }
  })
}

/** 给某家人录声纹(录 3 段取平均);点了立刻置 preparing 给反馈,之后由 enroll 事件推进。 */
function startEnroll(userId: number) {
  if (!isTauri()) {
    fakeEnroll(userId)
    return
  }
  state.enroll = { userId, stage: 'preparing', done: 0, total: 0 }
  api.voiceEnroll(userId).catch(() => {
    state.enroll = { userId, stage: 'failed', done: 0, total: 0 }
    onEnrollDoneCb?.(userId, false)
  })
}

/** 忘掉某家人的声纹(只删声纹,人/记忆不动)。 */
function unenroll(userId: number) {
  if (!isTauri()) return Promise.resolve()
  return api.voiceUnenroll(userId)
}

// 浏览器预览假注册:走一遍 preparing→录3遍→saved,让家人卡进度动起来(UI 优先)
function fakeEnroll(userId: number) {
  state.enroll = { userId, stage: 'preparing', done: 0, total: 3 }
  let done = 0
  const step = () => {
    if (done < 3) {
      state.enroll = { userId, stage: 'recording', done, total: 3 }
      done++
      setTimeout(step, 900)
    } else {
      state.enroll = { userId, stage: 'saved', done: 3, total: 3 }
      onEnrollDoneCb?.(userId, true)
    }
  }
  setTimeout(step, 700)
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
  return { state, start, stop, toggle, startEnroll, unenroll }
}
