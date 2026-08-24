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
// 上墙新鲜度:回合刚收尾,那条回复必然是秒级前落的。给足余量(时钟漂 + 加载往返),
// 但足以挡住「拿到几小时前的旧回复当新通知」。
const NOTICE_FRESH_MS = 60_000
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
    // ⚠️ **不是每条 conversation 事件都意味着「它刚说了话」(2026-08-22 修)**:
    // ① 旁听蒸发(kind=overheard_dismissed)整轮零痕迹、**什么都没落库**,outcome 却是 Done;
    // ② 回合失败(outcome=failed)也没有新回复。
    // 这两种情况下「取会话最新一条 assistant」拿到的是**上一次**的旧回复 —— 于是悬浮窗
    // 把一条几小时前的老话当成新通知弹出来。加两道闸,再加一道新鲜度兜底。
    if (ev.data.kind === 'overheard_dismissed' || ev.data.outcome !== 'done') return
    api
      .loadConversation(ev.data.conv_id)
      .then((msgs) => {
        const last = [...msgs]
          .reverse()
          .find(
            (m) =>
              m.role === 'assistant' && m.content.trim() && m.content.trim() !== '__IGNORE__',
          )
        // 新鲜度兜底:这条真是刚落库的才上墙(将来再冒出别的「没落新内容」的 kind 也不会翻车)
        if (last && Date.now() - last.created_at <= NOTICE_FRESH_MS) {
          pushNotice(last.content, ev.data.conv_id, ev.data.kind)
        }
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
  const mediaPlaying = computed(() => media.state.status === 'playing') // 迷你播控:播/暂停图标
  const mediaToggle = media.toggle // 悬浮窗里点 → 转发主窗暂停/继续(useMedia 内已按 isFloat 分流)
  const mediaStop = media.stop
  const listening = computed(
    () => voice.state.phase === 'listening' || voice.state.phase === 'transcribing',
  )
  const level = computed(() => voice.state.level) // 聆听波形(胶囊条 + 面板共用)
  const wakeArmed = computed(() => voice.state.wakeArmed) // 免手唤醒在跑(头像加"竖耳"环)
  const newCount = computed(() => state.notices.length)
  const openMain = () => void summonWindow('main')
  return {
    state,
    running,
    nowPlaying,
    mediaPlaying,
    mediaToggle,
    mediaStop,
    listening,
    level,
    wakeArmed,
    newCount,
    dismissNotice,
    openMain,
  }
}
