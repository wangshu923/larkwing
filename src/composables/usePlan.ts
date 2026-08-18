// 「计划」卡 VM(§6.5 会话内工作备忘):订阅全局事件车道的 plan 快照(全量语义,
// items 空 = 收卡)。主窗(TasksOverlay 计划卡)与悬浮窗(展开面板一行)各持一份单例。
// 纯展示、无交互按钮 —— 计划是 agent 自己的备忘,用户看的是「它干到哪了」。
// 浏览器预览:?demo=plan 注入假卡,纯看视觉(UI 优先工作流)。

import { computed, reactive } from 'vue'
import { isTauri, onAppEvent, type PlanCard } from '../lib/backend'

const state = reactive({
  cards: [] as PlanCard[],
})

let wired = false

function upsert(card: PlanCard) {
  const i = state.cards.findIndex((c) => c.conv_id === card.conv_id)
  if (!card.items.length) {
    // 空快照 = 清空/删会话:收卡
    if (i >= 0) state.cards.splice(i, 1)
    return
  }
  if (i >= 0) state.cards[i] = card
  else state.cards.push(card)
}

/** 进度短句素材:已完成数 / 总数 / 下一个未完项。 */
export function planProgress(card: PlanCard): { done: number; total: number; next?: string } {
  const done = card.items.filter((i) => i.done).length
  return { done, total: card.items.length, next: card.items.find((i) => !i.done)?.text }
}

function wire() {
  if (wired) return
  wired = true
  if (isTauri()) {
    onAppEvent((ev) => {
      if (ev.type !== 'plan') return
      upsert(ev.data)
    })
    return
  }
  // 浏览器预览的假卡(看视觉/调样式)
  if (new URLSearchParams(location.search).get('demo')?.includes('plan')) {
    upsert({
      conv_id: 1,
      title: '整理示例曲库',
      items: [
        { text: '扫一遍下载文件夹', done: true },
        { text: '按歌手建文件夹挪进去', done: true },
        { text: '给没歌词的补歌词', done: false },
        { text: '整理结果发手机', done: false },
      ],
    })
  }
}

export function usePlan() {
  wire()
  const cards = computed(() => state.cards)
  return { state, cards }
}
