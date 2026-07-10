// 会「页面不可见就自动暂停」的 requestAnimationFrame 循环。
// 传入的 fn 每帧调一次 —— 不要在 fn 里自己再 requestAnimationFrame,调度与暂停都归本 helper。
// 解决主窗藏托盘后 RAF 仍 60fps 空跑烧 CPU(根因见 usePageVisible)。
import { onMounted, onUnmounted, watch } from 'vue'
import { onRevive, usePageVisible } from './usePageVisible'

export function useRafLoop(fn: (ts: number) => void, opts?: { fps?: number }) {
  const visible = usePageVisible()
  // 可选限帧:氛围背景 30fps 肉眼无差,但绘制/续航砍半。仍每帧排 RAF(对齐 vsync),
  // 只是没到间隔就跳过 fn 的实际工作。fn 内若按 dt(ts-last)算物理,跳帧也不变速。
  const minDelta = opts?.fps ? 1000 / opts.fps : 0
  let raf = 0
  let lastRun = 0
  let lastTick = 0 // 最近一次 rAF 回调真的来过的时刻(限帧只跳过 fn,回调本身每帧都来)
  const tick = (ts: number) => {
    lastTick = performance.now()
    raf = requestAnimationFrame(tick)
    if (minDelta && ts - lastRun < minDelta - 0.5) return // 没到目标间隔 → 跳过本帧绘制
    lastRun = ts
    fn(ts)
  }
  const start = () => {
    if (!raf && visible.value) {
      lastTick = performance.now()
      raf = requestAnimationFrame(tick)
    }
  }
  const stop = () => {
    if (raf) {
      cancelAnimationFrame(raf)
      raf = 0
    }
  }
  // 看门狗:手里攥着 rAF id(自认为在跑)、回调却迟迟不来 = 又一枚「死 id」。watch 翻转 /
  // revive 脉冲都只在“信号到达那一刻”重排一次 —— 若那一瞬 WebView2 合成器还没醒(show 紧跟
  // emit 的真机时序),重排进去的 rAF 同样被吞,此后再无信号,照样冻死(§8.1 开机自启冻死
  // 第三轮病灶)。低频自检、发现停摆就清 id 重排,直到真跑起来;正常停着(raf=0)不碰,
  // 不破「隐藏即停」的省 CPU 语义。
  let dog = 0
  onMounted(() => {
    start()
    dog = window.setInterval(() => {
      if (raf && visible.value && performance.now() - lastTick > 1200) {
        stop()
        start()
      }
    }, 1500)
  })
  // 可见性翻转:**先 stop() 清掉旧 raf id 再按需 start()**。先清后排是关键 —— 隐藏期排过的 rAF
  // 在 WebView 里可能被丢弃却留下非 0 的 raf id,直接 start() 会被 `!raf` 守卫挡住、永不重排
  // (开机自启 hidden→show 后只剩静态画面、动画冻住、头像点了不切的根因)。先归零再排即可。
  watch(visible, (v) => {
    stop()
    if (v) start()
  })
  // 「窗口变可见」脉冲:即便 visible 没翻转(自启期误判可见 → 隐藏窗里排了死 rAF id、watch 不触发),
  // 也强制先清死 id 再重排 —— 修「开机自启后打开主窗,遛弯桌宠/背景仍冻住、头像点了不切」(§8.1)。
  const offRevive = onRevive(() => {
    stop()
    if (visible.value) start()
  })
  onUnmounted(() => {
    window.clearInterval(dog)
    offRevive()
    stop()
  })
  return { start, stop }
}
