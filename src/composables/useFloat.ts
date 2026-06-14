// 悬浮窗 VM(PLAN §12 形态 C):独立 WebView,订阅同一全局事件车道(app_event)。
// 汇成两类——"进行中"(钉住:聆听 / 正在放 / 下载,复用 useTasks·useMedia·useVoice)
// 与"通知"(瞬时:旺财主动说的话,新 → 旧,自动淡出)。点条目 → 唤主窗。
// 注:float 与 main 是两个 WebView,各持一份单例、各自 wire();靠广播事件对齐,不共享内存。

import { computed, reactive } from 'vue'
import { api, isTauri, onAppEvent, summonWindow } from '../lib/backend'
import { useTasks } from './useTasks'
import { useMedia } from './useMedia'
import { useVoice } from './useVoice'

export interface FloatNotice {
  id: number
  text: string // 旺财说的话(从会话最新 assistant 消息取;模型产出,非 core 文案)
  conv_id: number
  kind: string
}

const state = reactive({
  notices: [] as FloatNotice[],
  expanded: false,
})

const LINGER_MS = 8000
let nid = 1
let wired = false

function pushNotice(text: string, convId: number, kind: string) {
  const notice = { id: nid++, text, conv_id: convId, kind }
  state.notices.unshift(notice) // 新 → 旧
  if (state.notices.length > 4) state.notices.length = 4 // 最多留最新 4 条
  setTimeout(() => dismissNotice(notice.id), LINGER_MS)
}

function dismissNotice(id: number) {
  const i = state.notices.findIndex((n) => n.id === id)
  if (i >= 0) state.notices.splice(i, 1)
}

function wire() {
  if (wired) return
  wired = true
  if (!isTauri()) {
    // 浏览器预览:?demo=float 塞两条假通知,纯看视觉
    if (new URLSearchParams(location.search).get('demo')?.includes('float')) {
      pushNotice('该吃药啦~记得喝口温水', 1, 'reminder')
      pushNotice('豆豆的家长会今晚 7 点哦', 2, 'reminder')
    }
    return
  }
  // 旺财主动说话(提醒到点 / 自启回合):取该会话最新一条有内容的 assistant 文本上墙
  onAppEvent((ev) => {
    if (ev.type !== 'conversation') return
    api
      .loadConversation(ev.data.conv_id)
      .then((msgs) => {
        const last = [...msgs]
          .reverse()
          .find(
            (m) =>
              m.role === 'assistant' && m.content.trim() && m.content.trim() !== '__IGNORE__',
          )
        if (last) pushNotice(last.content, ev.data.conv_id, ev.data.kind)
      })
      .catch(() => {})
  })
}

export function useFloat() {
  wire()
  const tasks = useTasks()
  const media = useMedia()
  const voice = useVoice()
  // 进行中(钉住不滚):运行中的任务 + 正在播放 + 聆听态
  const running = computed(() => tasks.state.tasks.filter((t) => t.state === 'running'))
  const nowPlaying = computed(() => media.state.current)
  const listening = computed(
    () => voice.state.phase === 'listening' || voice.state.phase === 'transcribing',
  )
  const level = computed(() => voice.state.level) // 聆听波形(胶囊条 + 面板共用)
  const newCount = computed(() => state.notices.length)
  const openMain = () => void summonWindow('main')
  return { state, running, nowPlaying, listening, level, newCount, dismissNotice, openMain }
}
