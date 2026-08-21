// 进度条 hover 读数:光标在条上哪儿 → 那是第几秒。VideoOverlay(看片)与 PlayerBar(听)共用。
//
// 为什么要它:拖动中虽然早就显示目标时间了(不是显示播放位),但读数在控制条最左边,拖的时候
// 眼睛在拇指上、根本不会去看;更要紧的是**按下之前**没有任何读数,而 range 是点哪跳哪 —— 想
// 「先看看这儿是几分几秒,再决定点不点」现在做不到。这就是"盲拖"的本体。
//
// 刻意不换掉 <input type="range">:自绘进度条要自己接键盘/触摸/无障碍,而 VideoOverlay 的
// 快捷键逻辑还专门给 range 开了"不让位"的口子(见 onKey)。这里只在它外面套一层 hover 检测。

import { computed, ref, watch, type Ref } from 'vue'

export type ScrubHoverOptions = {
  /** range 拇指直径(px):与两处 CSS 的 ::-webkit-slider-thumb 尺寸一致(视频 11、音频 10)。
   *  可用轨道两端各被半个拇指占掉,不减它算出来的时间在首尾会偏。 */
  thumbWidth?: number
  /** 气泡夹在谁里面(视频小窗的面板 / 播放条本身)。**不能夹在进度条里**:带缩略图的气泡
   *  (200px)常比小窗的进度条(140px)还宽,夹轨道等于让它顶出面板。缺省 = 夹轨道(纯时间
   *  气泡很窄,够用)。 */
  clampTo?: Ref<HTMLElement | null>
}

/** 气泡与夹持容器边缘的留白(px)。 */
const EDGE = 6

export function useScrubHover(duration: Ref<number>, opts: ScrubHoverOptions = {}) {
  const thumbW = opts.thumbWidth ?? 11
  /** 套在滑杆外面的定位容器(气泡按它的坐标系摆)。 */
  const trackEl = ref<HTMLElement | null>(null)
  /** 光标处的百分比 0..100;null = 光标不在条上(或时长未知 → 恒不出气泡)。 */
  const hoverPct = ref<number | null>(null)
  /** 气泡自身宽度,用来把它夹住不出框(渲染后由调用方的 ref 量出来)。 */
  const bubbleW = ref(0)
  /** 光标那一刻量下来的几何。**刻意不在 computed 里读布局** —— 那样窗口/面板尺寸变了不会重算。 */
  const geom = ref({ trackLeft: 0, trackW: 0, clampLeft: 0, clampRight: 0 })

  const hoverTime = computed(() =>
    hoverPct.value === null ? null : (hoverPct.value / 100) * duration.value,
  )

  /** 气泡左缘(px,相对 trackEl;配 CSS 的 translateX(-50%) = 圆心跟着光标):
   *  跟着光标走,但夹在 clampTo 里不出框;气泡比容器还宽时居中(夹不住就别乱夹)。 */
  const bubbleLeft = computed(() => {
    if (hoverPct.value === null) return 0
    const g = geom.value
    const half = bubbleW.value / 2
    const cx = g.trackLeft + (hoverPct.value / 100) * (g.trackW - thumbW) + thumbW / 2
    const lo = g.clampLeft + half + EDGE
    const hi = g.clampRight - half - EDGE
    const clamped =
      hi < lo ? (g.clampLeft + g.clampRight) / 2 : Math.min(Math.max(cx, lo), hi)
    return clamped - g.trackLeft
  })

  function onMove(e: PointerEvent) {
    // 时长未知就没有"第几秒"可言(混流 /m/ 那条路 el.duration 是 Infinity/NaN,core 也没给
    // duration_seconds)→ 干脆不出气泡,别显示一个算不出来的数(§3.5)。
    if (!(duration.value > 0)) {
      hoverPct.value = null
      return
    }
    const el = trackEl.value
    if (!el) return
    const rect = el.getBoundingClientRect()
    const clamp = opts.clampTo?.value?.getBoundingClientRect() ?? rect
    geom.value = {
      trackLeft: rect.left,
      trackW: rect.width,
      clampLeft: clamp.left,
      clampRight: clamp.right,
    }
    hoverPct.value = pctFromX(e.clientX, rect.left, rect.width, thumbW)
  }

  function onLeave() {
    hoverPct.value = null
  }

  return { trackEl, hoverPct, hoverTime, bubbleLeft, bubbleW, onMove, onLeave }
}

/** 光标 x → 百分比(0..100)。纯函数,单独导出方便算得对不对一眼看清:
 *  range 的可用轨道是 [left + 半个拇指, right - 半个拇指],不减就会"最左边不是 0、最右边到不了头"。 */
export function pctFromX(x: number, left: number, width: number, thumbW: number): number {
  const usable = width - thumbW
  if (usable <= 0) return 0
  const p = ((x - left - thumbW / 2) / usable) * 100
  return Math.min(100, Math.max(0, p))
}

/* ——— 缩略图那半边(只有视频、只有本地片有) ——— */

/** 时间量化格(秒):**与后端 relay.rs 的 `THUMB_GRID` 同值**。后端还会自己落一次格
 *  (键规范化),所以万一两边不一致也只是白丢缓存命中、不会出错。 */
const THUMB_GRID = 10
/** hover 抖动防抖(ms):时间气泡即时跟手,取图等手停一下 —— 横扫一趟不会连着起 ffmpeg。 */
const THUMB_DEBOUNCE_MS = 120
/** 连着这么多个**不同格**都取不到图 = 这片抽不出帧(编码怪/文件坏),本次播放不再试。
 *  为什么不是"错一次就放弃":最右端那一格常常越过片尾、本来就没帧,而 hover 到最右边太常见了。 */
const THUMB_GIVE_UP_AFTER = 3

/**
 * 进度条 hover 缩略图:按格取图、载完再换(不闪)、取不到就安静放弃。
 * `base` = `NowPlaying.thumb_url`(没值 = 这片没有缩略图,整套逻辑空转不发请求)。
 */
export function useScrubThumb(
  base: Ref<string | undefined>,
  hoverTime: Ref<number | null>,
  duration: Ref<number>,
) {
  /** 已经加载好、可以显示的那张(null = 还没有/不显示)。 */
  const src = ref<string | null>(null)
  const gaveUp = ref(false)
  /** 有图可显示时才占位(没图的片子气泡里就只有时间)。 */
  const available = computed(() => !!base.value && !gaveUp.value)

  let timer: ReturnType<typeof setTimeout> | undefined
  let gen = 0 // 播放会话代次:换片/换集时作废在途的加载
  let loadedUrl: string | null = null
  /** 取不到图的格(本次播放内不再重试 —— 尤其是越过片尾那一格,hover 到最右边太常见)。 */
  let failed = new Set<string>()

  // 换片/换集(base 变)= 全部复位:上一部片的图绝不留在下一部片的进度条上
  watch(base, () => {
    gen++
    clearTimeout(timer)
    src.value = null
    loadedUrl = null
    gaveUp.value = false
    failed = new Set()
  })

  watch(hoverTime, (t) => {
    if (!available.value || t === null || !(duration.value > 0)) return
    clearTimeout(timer)
    timer = setTimeout(() => load(t), THUMB_DEBOUNCE_MS)
  })

  function load(t: number) {
    // 夹到片尾之前:hover 到最右端时 t == duration,那一秒往往已经没有帧了(抽不出 → 404)
    const at = Math.floor(Math.min(t, Math.max(0, duration.value - 0.5)) / THUMB_GRID) * THUMB_GRID
    const url = `${base.value}?t=${at}`
    if (url === loadedUrl || failed.has(url)) return // 同一格 / 这格取过没有:连请求都不发
    const my = gen
    const img = new Image()
    img.onload = () => {
      if (my !== gen) return // 已经换片了,这张作废
      loadedUrl = url
      src.value = url
    }
    img.onerror = () => {
      if (my !== gen) return
      failed.add(url)
      if (failed.size >= THUMB_GIVE_UP_AFTER) {
        gaveUp.value = true // 这片抽不出帧 → 退成"只有时间气泡",别再留着上一张误导人
        src.value = null
      }
    }
    img.src = url
  }

  return { src, available }
}
