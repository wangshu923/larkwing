// 动作确认卡 VM(§7.8 确认闸):订阅全局事件车道的 confirm 卡(全量快照,终态即收卡信号)。
// 主窗(TasksOverlay 渲染 + 语音回合念出来问)与悬浮窗(展开面板可点)各持一份单例、各自 wire。
// 应答走 api.confirmAction(先到先得:桌面/悬浮窗/语音/渠道回话谁快算谁);false = 卡已收尾。
// 浏览器预览:?demo=confirm 注入假卡,纯看视觉(UI 优先工作流)。

import { computed, reactive } from 'vue'
import { api, isTauri, onAppEvent, windowLabel, type ConfirmCard } from '../lib/backend'
import { i18n } from '../i18n'

/** 动作短语(kind + 目标原文 → 人话):卡片/悬浮窗/记录页/语音问句共用,动词在字典(§6.6)。 */
export function confirmActionPhrase(card: Pick<ConfirmCard, 'kind' | 'action'>): string {
  const t = i18n.global.t
  if (card.kind === 'submit') {
    return card.action ? t('confirm.act.submit', { text: card.action }) : t('confirm.act.submitBare')
  }
  if (card.kind === 'press') return t('confirm.act.press', { text: card.action })
  return t('confirm.act.click', { text: card.action })
}

const state = reactive({
  cards: [] as ConfirmCard[],
  /** 倒计时时钟(有 pending 卡时 1s 一跳;没卡不跑,省 RAF/interval)。 */
  now: Date.now(),
})

const FINAL_LINGER_MS = 2600
let wired = false
let ticker: ReturnType<typeof setInterval> | undefined

function syncTicker() {
  const hasPending = state.cards.some((c) => c.state === 'pending')
  if (hasPending && !ticker) {
    ticker = setInterval(() => {
      state.now = Date.now()
    }, 1000)
  } else if (!hasPending && ticker) {
    clearInterval(ticker)
    ticker = undefined
  }
}

function upsert(card: ConfirmCard) {
  const i = state.cards.findIndex((c) => c.id === card.id)
  if (i >= 0) state.cards[i] = card
  else if (card.state === 'pending') state.cards.push(card)
  else return // 终态卡但没见过 pending(错过/别的窗处理了):不冒尸体
  if (card.state !== 'pending') setTimeout(() => remove(card.id), FINAL_LINGER_MS)
  syncTicker()
}

function remove(id: number) {
  const i = state.cards.findIndex((c) => c.id === id)
  if (i >= 0) state.cards.splice(i, 1)
  syncTicker()
}

/** 点头/摇头(via = desktop | float,记进审计「谁点的」)。卡已收尾(过期/别处先点)则直接收卡。 */
async function resolve(id: number, allow: boolean, via: string) {
  if (!isTauri()) {
    // 浏览器预览:本地模拟终态(能看到「继续了/没执行」的收尾观感)
    const cur = state.cards.find((c) => c.id === id)
    if (cur) upsert({ ...cur, state: allow ? 'allowed' : 'denied', via })
    return
  }
  const ok = await api.confirmAction(id, allow, via).catch(() => false)
  if (!ok) remove(id) // 权威终态卡也会来,这是双保险
}

/** 剩余秒数(倒计时;core 超时是权威,这只是展示)。 */
function remaining(card: ConfirmCard): number {
  return Math.max(0, Math.ceil((card.deadline_ms - state.now) / 1000))
}

/** pending 卡到达钩子(主窗注册:语音回合念出来问 + 开口头确认听音;悬浮窗不注册)。 */
type PendingHook = (card: ConfirmCard) => void
let pendingHook: PendingHook | undefined
function onPending(hook: PendingHook) {
  pendingHook = hook
}

function wire() {
  if (wired) return
  wired = true
  if (isTauri()) {
    onAppEvent((ev) => {
      if (ev.type !== 'confirm') return
      const fresh = ev.data.state === 'pending' && !state.cards.some((c) => c.id === ev.data.id)
      upsert(ev.data)
      if (fresh) pendingHook?.(ev.data)
    })
    return
  }
  // 浏览器预览的假卡(看视觉/调样式;点按钮走本地模拟终态)
  if (new URLSearchParams(location.search).get('demo')?.includes('confirm')) {
    upsert({
      id: 1,
      user_id: 1,
      conv_id: 1,
      origin: 'ui',
      host: 'pay.example.com',
      action: '确认支付 ¥128.00',
      kind: 'click',
      state: 'pending',
      deadline_ms: Date.now() + 60_000,
    })
    upsert({
      id: 2,
      user_id: 1,
      conv_id: 1,
      origin: 'ui',
      host: 'shop.example.com',
      action: '立即购买',
      kind: 'submit',
      state: 'pending',
      deadline_ms: Date.now() + 45_000,
    })
  }
}

export function useConfirm() {
  wire()
  const pending = computed(() => state.cards.filter((c) => c.state === 'pending'))
  const isFloat = windowLabel() === 'float'
  return { state, pending, resolve, remaining, onPending, via: isFloat ? 'float' : 'desktop' }
}
