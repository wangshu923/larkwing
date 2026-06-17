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

if (typeof document !== 'undefined') {
  document.addEventListener('visibilitychange', () => {
    visible.value = document.visibilityState === 'visible'
  })
}
onWinVisible((v) => (visible.value = v))
// 初值兜底:透明窗在自启静默藏窗时 document.hidden 可能仍报 false → 查 OS 真相纠正
if (isTauri()) void win.isHidden().then((h) => (visible.value = !h))

/** 返回全局可见性 ref(单例;隐藏 = false)。 */
export function usePageVisible() {
  return visible
}
