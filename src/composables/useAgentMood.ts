// 回合 mood VM(PLAN §12 修订):订阅全局事件车道的 mood 事件,供悬浮窗显「正在想/正在说」。
// 主窗不用它(它走自己的 per-turn mood,见 useChat);只有第二窗(float)消费这条。
// 浏览器预览无总线 → 静默保持 idle(?demo 想看效果可手动设)。

import { reactive } from 'vue'
import { onAppEvent } from '../lib/backend'

export type AgentMood = 'idle' | 'thinking' | 'speaking'

const state = reactive({ mood: 'idle' as AgentMood })

let wired = false
function wire() {
  if (wired) return
  wired = true
  onAppEvent((ev) => {
    if (ev.type === 'mood') state.mood = ev.data
  })
}

export function useAgentMood() {
  wire()
  return { state }
}
