// 全局「页面此刻该不该动」信号 —— 动画循环(useRafLoop)据此暂停/恢复。
//
// 根因(§8.1):关主窗 = 隐藏到托盘、进程不退(PLAN §12),而主窗是 transparent 窗 →
// Chromium 的遮挡检测对透明窗失效、不会把藏起来的窗口的 requestAnimationFrame 自动节流掉;
// 我们的背景/遛弯动画又没有任何可见性判断,于是藏到托盘后照样 60fps 空跑、白烧 CPU。
//
// 双触发(哪条在 Windows 上都可能不灵,所以都上):
//   ① 标准 Page Visibility(document.visibilitychange)—— 覆盖最小化;WebView2 报得准时够用。
//   ② 壳层 lw:win-visible 事件 —— 主窗 hide/show 时 Rust 主动发(只为 main 发,否则关悬浮窗
//      会误停主窗),不赌 WebView2 的遮挡行为。
// 另在加载时异步查一次窗口真实可见性(OS 真相),纠正开机自启静默藏窗(--autostart)的初值。
import { ref } from 'vue'
import { isTauri, onWinVisible, win } from '../lib/backend'

const visible = ref(typeof document === 'undefined' ? true : !document.hidden)

// 收到过权威可见性信号(visibilitychange / 壳层 lw:win-visible)后,异步初值查询不再覆盖它 ——
// 否则慢一拍 resolve 的 isHidden() 会把已生效的 show 信号冲回旧值(自启 hidden→show 竞态)。
let settled = false
// 「窗口变可见」脉冲:每次收到权威 visible=true 都触发订阅者(**即便 visible 值没发生翻转**)。
// 给 useRafLoop 兜底 —— 自启期透明窗误判可见 → 隐藏窗里排了永不触发的死 rAF id 时,visible 不会
// false→true 翻转、watch(visible) 不触发;这条脉冲强制「清死 id 再重排」(§8.1 开机自启冷启动画冻死)。
const reviveCbs = new Set<() => void>()
export function onRevive(cb: () => void): () => void {
  reviveCbs.add(cb)
  return () => reviveCbs.delete(cb)
}
const apply = (v: boolean) => {
  settled = true
  visible.value = v
  if (v) reviveCbs.forEach((cb) => cb())
}

if (typeof document !== 'undefined') {
  document.addEventListener('visibilitychange', () => apply(document.visibilityState === 'visible'))
}
onWinVisible(apply)
// 初值兜底:透明窗在自启静默藏窗时 document.hidden 可能仍报 false → 查 OS 真相纠正。
// 仅在还没收到权威事件时采用(settled 守卫),防 stale 覆盖已到达的 show/hide 信号。
if (isTauri()) void win.isHidden().then((h) => { if (!settled) visible.value = !h })

/** 返回全局可见性 ref(单例;隐藏 = false)。 */
export function usePageVisible() {
  return visible
}
