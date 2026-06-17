// 会「页面不可见就自动暂停」的 requestAnimationFrame 循环。
// 传入的 fn 每帧调一次 —— 不要在 fn 里自己再 requestAnimationFrame,调度与暂停都归本 helper。
// 解决主窗藏托盘后 RAF 仍 60fps 空跑烧 CPU(根因见 usePageVisible)。
import { onMounted, onUnmounted, watch } from 'vue'
import { usePageVisible } from './usePageVisible'

export function useRafLoop(fn: (ts: number) => void, opts?: { fps?: number }) {
  const visible = usePageVisible()
  // 可选限帧:氛围背景 30fps 肉眼无差,但绘制/续航砍半。仍每帧排 RAF(对齐 vsync),
  // 只是没到间隔就跳过 fn 的实际工作。fn 内若按 dt(ts-last)算物理,跳帧也不变速。
  const minDelta = opts?.fps ? 1000 / opts.fps : 0
  let raf = 0
  let lastRun = 0
  const tick = (ts: number) => {
    raf = requestAnimationFrame(tick)
    if (minDelta && ts - lastRun < minDelta - 0.5) return // 没到目标间隔 → 跳过本帧绘制
    lastRun = ts
    fn(ts)
  }
  const start = () => {
    if (!raf && visible.value) raf = requestAnimationFrame(tick)
  }
  const stop = () => {
    if (raf) {
      cancelAnimationFrame(raf)
      raf = 0
    }
  }
  onMounted(start)
  watch(visible, (v) => (v ? start() : stop())) // 可见性翻转即停/续
  onUnmounted(stop)
  return { start, stop }
}
