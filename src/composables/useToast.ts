// 轻量全局提示(toast):用户主动操作失败时给一句友好反馈,替代「catch 里只 console、用户毫无察觉」
// 的静默失败(§3.5 不静默失败)。core 只发 kind,文案由调用方按 locale 选好再传进来(§6.6 core 不产文案)。
// 单例模块态(同 useChat 的 reactive state 模式),全窗共用一处;只在主窗挂宿主(ToastHost)。
import { reactive } from 'vue'

export type ToastKind = 'error' | 'ok' | 'info'
export interface Toast {
  id: number
  kind: ToastKind
  text: string
}

const state = reactive({ list: [] as Toast[] })
let seq = 0
// 停留时长:够读完一句、不赖着挡视线(UI 时序细节,同动画时长;非产品默认值)。
const TTL = 4200

function dismiss(id: number) {
  const i = state.list.findIndex((t) => t.id === id)
  if (i >= 0) state.list.splice(i, 1)
}

function show(kind: ToastKind, text: string) {
  if (!text) return
  const id = ++seq
  state.list.push({ id, kind, text })
  window.setTimeout(() => dismiss(id), TTL)
}

export function useToast() {
  return {
    toasts: state.list,
    show,
    dismiss,
    error: (text: string) => show('error', text),
    ok: (text: string) => show('ok', text),
    info: (text: string) => show('info', text),
  }
}
