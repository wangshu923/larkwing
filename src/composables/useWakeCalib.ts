// 唤醒录音标定 VM(PLAN §11 后续):驱动「录几遍 → 一次扫描定拼写+阈值」向导。
// 编排者 = 这一层(宪法 §5);core 只采集+扫描,进展走 voice 车道
// (calib_progress / state / calib_result)。设置页·声音 tab 用,替代盲调灵敏度滑块。
// 浏览器预览降级:假进度自动推进 + 落一个假灵敏度(UI 优先,点了就动)。

import { reactive } from 'vue'
import { api, isTauri, onAppEvent } from '../lib/backend'

export interface CalibResult {
  ok: boolean
  sensitivity: number
  recall: number
  adoptedSpelling: boolean
  verdict: string // good | noisy | hard | cancelled | error
}

const state = reactive({
  /** 标定进行中(录音/计算);收尾(done/idle)= false。 */
  running: false,
  /** 阶段:idle 未开始 | recording 录样本 | computing 扫描中 | done 出结果。 */
  phase: 'idle' as 'idle' | 'recording' | 'computing' | 'done',
  /** 正在录第 step/total 段(step 从 1 计;最后一段 = 底噪/环境音)。 */
  step: 0,
  total: 0,
  /** VAD/采集此刻在听(驱动录音脉冲动画)。 */
  listening: false,
  /** 最近一次结果(done 后非空);新一轮 start 清空。 */
  result: null as CalibResult | null,
})

let wired = false
let fakeTimer: ReturnType<typeof setTimeout> | undefined

function wire() {
  if (wired) return
  wired = true
  if (!isTauri()) return
  onAppEvent((ev) => {
    if (ev.type !== 'voice') return
    const v = ev.data
    if (v.type === 'calib_progress') {
      state.phase = 'recording'
      state.step = v.data.step
      state.total = v.data.total
      return
    }
    if (v.type === 'calib_result') {
      state.result = {
        ok: v.data.ok,
        sensitivity: v.data.sensitivity,
        recall: v.data.recall,
        adoptedSpelling: v.data.adopted_spelling,
        verdict: v.data.verdict,
      }
      state.phase = 'done'
      state.listening = false
      state.running = false
      return
    }
    // 仅在标定进行中消费通用 state(否则会被听写/唤醒的 state 干扰)
    if (state.running && v.type === 'state') {
      state.listening = v.data.phase === 'listening'
      // 最后一段(底噪)录完回 idle → 进入"计算中"
      if (v.data.phase === 'idle' && state.total > 0 && state.step >= state.total) {
        state.phase = 'computing'
      }
    }
  })
}

function start() {
  if (state.running) return
  state.running = true
  state.phase = 'recording'
  state.step = 0
  state.total = 0
  state.listening = false
  state.result = null
  if (!isTauri()) {
    fakeRun()
    return
  }
  api.voiceCalibrateWake().catch((e) => {
    console.error('唤醒标定启动失败', e)
    state.running = false
    state.phase = 'idle'
  })
}

function cancel() {
  if (!state.running) return
  if (!isTauri()) {
    clearTimeout(fakeTimer)
    state.running = false
    state.phase = 'idle'
    state.listening = false
    return
  }
  api.voiceCalibrateCancel().catch(() => {})
}

/** 一轮过后回到可再来的初始态(关向导/再校准前调)。 */
function reset() {
  if (state.running) return
  state.phase = 'idle'
  state.step = 0
  state.total = 0
  state.result = null
}

// ---- 浏览器预览假标定(看向导流程视觉:录 5 段 + 1 底噪 → 计算 → 结果) ----
function fakeRun() {
  const total = 6
  const tick = (step: number) => {
    if (!state.running) return
    if (step > total) {
      state.phase = 'computing'
      fakeTimer = setTimeout(() => {
        if (!state.running) return
        state.result = {
          ok: true,
          sensitivity: 40,
          recall: 1.0,
          adoptedSpelling: false,
          verdict: 'good',
        }
        state.phase = 'done'
        state.listening = false
        state.running = false
      }, 1200)
      return
    }
    state.phase = 'recording'
    state.step = step
    state.total = total
    state.listening = true
    fakeTimer = setTimeout(() => {
      state.listening = false
      fakeTimer = setTimeout(() => tick(step + 1), 350)
    }, 900)
  }
  tick(1)
}

export function useWakeCalib() {
  wire()
  return { state, start, cancel, reset }
}
