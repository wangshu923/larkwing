// 采集路由镜像(「回声消除 = 自动」档,AGENT §7.5 2026-08-12):core 按「默认输出是不是
// 耳机」解析 voice.capture.source='auto' 的生效值(耳机 → cpal:自播进不了麦 AEC 零收益,
// mac 上开麦还会被系统通话处理弄糊自家播放;扬声器 → browser:AEC 治自我唤醒的根)。
// 这里维护前端镜像 + auto 档低频轮询,跟随「戴上/摘下耳机」这类输出切换:生效值变了 →
// 重启唤醒换管(与设置页切档、MicBridge 自愈同一条 wakeSet 双拍路)。显式 browser/cpal
// 不轮询,镜像即偏好。
import { reactive } from 'vue'
import { api, isTauri } from '../lib/backend'
import { useSettings } from './useSettings'

/** auto 档轮询间隔:跟随戴上/摘下耳机,几秒内换管(§4.11 单源;一次 IPC + 平台查询皆微秒级)。 */
const ROUTE_POLL_MS = 5000

const state = reactive({
  /** 解析后的生效采集源;'' = auto 档还没问到 core(MicBridge 见 '' 不开麦,防误占)。 */
  effective: '' as '' | 'browser' | 'cpal',
  /** 默认输出是不是耳机(null = 探不出/还没问)。设置页「自动」行的提示用。 */
  headphones: null as boolean | null,
})
let wired = false
let restarting = false

async function tick() {
  const pref = useSettings().get('voice.capture.source') || 'auto'
  if (pref === 'browser' || pref === 'cpal') {
    state.effective = pref
    return
  }
  try {
    const r = await api.voiceRoute()
    const prev = state.effective
    state.effective = r.effective === 'cpal' ? 'cpal' : 'browser'
    state.headphones = r.headphones
    // 生效值真变了(且不是首查)→ 唤醒在跑就重启换管(useMicBridge.fallbackToCpal 同款双拍)
    if (prev !== '' && prev !== state.effective && !restarting) {
      restarting = true
      try {
        const s = await api.voiceStatus()
        if (s.wakeRunning) {
          await api.voiceWakeSet(false)
          await api.voiceWakeSet(true)
        }
      } catch (e) {
        console.error('[captureRoute] 输出形态变了,重启唤醒换管失败', e)
      } finally {
        restarting = false
      }
    }
  } catch {
    /* 查询失败:保持上次镜像,下个周期再试 */
  }
}

export function useCaptureRoute() {
  if (!wired) {
    wired = true
    if (isTauri()) {
      void tick()
      window.setInterval(() => void tick(), ROUTE_POLL_MS)
    }
  }
  // refresh:设置页切档后立即解析,别等下个轮询周期
  return { state, refresh: tick }
}
