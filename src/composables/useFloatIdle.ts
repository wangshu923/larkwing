// 悬浮窗待机轮播(PLAN §12):待机这一行只显示 OS 不会告诉你的东西 ——
// 下个提醒 / 最近一句旺财说的话 /(今日花费·余额 opt-in)。时间归 OS,不重复造一个钟。
// ~6s 切一条;只剩一条则静态;空池由 FloatWindow 回退到问候(float.idle)。hover 暂停。
// 注:float 独立 WebView,这层只读不发声;数据靠 float_idle 命令 + conversation 事件刷新。

import { computed, reactive } from 'vue'
import {
  api,
  isTauri,
  onAppEvent,
  type AccountBalance,
  type DayUsage,
  type FloatIdle,
} from '../lib/backend'
import { i18n } from '../i18n'
import { useSettings } from './useSettings'

export interface IdleItem {
  kind: 'reminder' | 'care' | 'cost' | 'balance'
  text: string
  /** 点击要替用户发出去的那句(仅关怀候选有):悬浮窗点它 → emitFloatSay + 唤主窗。 */
  say?: string
}

const ROTATE_MS = 6000
const t = i18n.global.t

const state = reactive({
  data: null as FloatIdle | null,
  today: null as DayUsage | null,
  balance: null as AccountBalance | null,
  tick: 0,
  paused: false,
})

let wired = false
let timer: ReturnType<typeof setInterval> | undefined

/** due_at(ms)→ HH:MM(时分;日期/区域格式归 OS,这里只取钟点)。 */
function hhmm(ms: number): string {
  const d = new Date(ms)
  return `${String(d.getHours()).padStart(2, '0')}:${String(d.getMinutes()).padStart(2, '0')}`
}

function sym(currency: string): string {
  return ({ CNY: '¥', USD: '$', EUR: '€' } as Record<string, string>)[currency] ?? `${currency} `
}

function showUsage(): boolean {
  return useSettings().get('ui.float.show_usage') === '1'
}

// 主动关怀静默时段:22:00–08:00 本地不出关怀候选(强默认、不暴露;起步值,与 audio 夜间同为前端本地时钟判断)。
function inQuietHours(): boolean {
  const h = new Date().getHours()
  return h >= 22 || h < 8
}

async function refresh() {
  if (!isTauri()) return
  try {
    state.data = await api.floatIdle()
  } catch {
    /* 取不到就让池子空着,FloatWindow 回退问候 */
  }
  if (showUsage()) {
    api.usageToday().then((d) => (state.today = d)).catch(() => {})
    api.llmBalance().then((b) => { if (b) state.balance = b }).catch(() => {})
  }
}

function wire() {
  if (wired) return
  wired = true
  if (!isTauri()) {
    // 浏览器预览:?demo=float 塞一条提醒,纯看轮播视觉(唤醒那条由 useVoice 的 demo 塞)
    if (new URLSearchParams(location.search).get('demo')?.includes('float')) {
      state.data = {
        next_reminder: { content: '吃药', due_at: Date.now() + 3 * 3600_000 },
        care: { kind: 'resume', title: '星海漫游', updated_at: Date.now() - 26 * 3600_000 },
      }
    }
  } else {
    void refresh()
    // 旺财说了话 / 提醒到点 → 最近一句 & 待办都可能变,顺手刷
    onAppEvent((ev) => {
      if (ev.type === 'conversation') void refresh()
    })
  }
  // 轮播节拍:tick++,current 取模轮转(只剩一条则恒定;hover 暂停)
  timer = setInterval(() => {
    if (!state.paused) state.tick++
  }, ROTATE_MS)
}

const items = computed<IdleItem[]>(() => {
  const out: IdleItem[] = []
  // 待机轮播只显示 OS 不会主动告诉你的事:下个提醒 +(opt-in)今日花费/余额。
  // 「在等唤醒」不进文字条(用户:留头像的竖耳环示意即可),空池由 FloatWindow 回退到「我有空」。
  const r = state.data?.next_reminder
  if (r) out.push({ kind: 'reminder', text: `${hhmm(r.due_at)}  ${r.content}` })
  // (去掉"最近一句旺财说的话":用户反馈意义不大)
  // 主动关怀候选(PLAN ★主动关怀里程碑,切片1 = L0):后端已按 care.enabled 决定给不给,
  // 这里只加"静默时段"的门(22:00–08:00 本地不打扰;同 audio 夜间也在前端算本地时钟)。
  const care = state.data?.care
  if (care?.kind === 'resume' && !inQuietHours()) {
    out.push({
      kind: 'care',
      text: t('care.resume', { title: care.title }),
      say: t('care.resumeSay', { title: care.title }),
    })
  }
  if (showUsage()) {
    if (state.today) {
      out.push({ kind: 'cost', text: t('float.todayCost', { amount: `$${state.today.cost_usd.toFixed(3)}` }) })
    }
    if (state.balance) {
      out.push({ kind: 'balance', text: t('float.balance', { amount: `${sym(state.balance.currency)}${state.balance.amount}` }) })
    }
  }
  return out
})

const current = computed<IdleItem | null>(() =>
  items.value.length ? items.value[state.tick % items.value.length] : null,
)

export function useFloatIdle() {
  wire()
  return {
    items,
    current,
    refresh,
    setPaused: (p: boolean) => {
      state.paused = p
    },
  }
}
