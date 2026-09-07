<script setup lang="ts">
// 视频浮层:current.kind === 'video' 时出现。<video> 挂载即向 VM 登记。
// 全屏 = 原生窗口全屏(非 HTML5 requestFullscreen —— 后者在 WebView2 上与 DWM 合成器打架,
// 闪烁/退出穿帮),包壳加 .maximized 铺满;混流流无原生 seek,滑杆松手 = 换 src 重启(?t=)。
// 窗口态 = 非模态应用内小视窗(webrender 可见任务窗同款形态:右下停靠、标题栏拖动、拖角缩放),
// 底下界面照常可用。不开真原生第二窗:播放引擎(MSE/relay 会话/useMedia VM)全在主窗 WebView,
// 挪窗 = 悬浮窗双播陷阱同族 + 采集端 AEC 参考信号断链(§7.5)。
import { computed, nextTick, onMounted, onUnmounted, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import EpisodeList from './EpisodeList.vue'
import KeyHelpCard from './KeyHelpCard.vue'
import { useContextMenu, type MenuItem } from '../composables/useContextMenu'
import { registerVideoEl, useMedia } from '../composables/useMedia'
import { useMediaKeys, VOL_STEP } from '../composables/useMediaKeys'
import { useScrubHover, useScrubThumb } from '../composables/useScrubHover'
import { win } from '../lib/backend'
import { fmtClock } from '../lib/fmt'

const { t } = useI18n()
const {
  state,
  toggle,
  stop,
  seek,
  seekBy,
  setVolume,
  toggleMute,
  stepRate,
  openRateMenu,
  next,
  prev,
  cycleAudioTrack,
  audioTrackLabel,
  cycleSubtitle,
  subtitleLabel,
  markSkip,
  autoNext,
} = useMedia()
const menu = useContextMenu()

/** 多集剧集才出集数指示 + 上/下一集按钮(单集/电影为 null,不出现)。 */
const playlist = computed(() => state.current?.playlist ?? null)
/** ≥2 条音轨才出切换钮(双语片);label = 当前轨的友好名(国语/英语/元数据标题)。 */
const audioTrackCount = computed(() => state.current?.audio_tracks?.length ?? 0)
const audioLabel = computed(() => audioTrackLabel(state.current?.audio_track ?? 0))
const subtitles = computed(() => state.current?.subtitles ?? [])
// 字幕按钮的文案:关/第几条(纯显示层,不碰管线)
const subtitleText = computed(() =>
  state.subtitle >= 1
    ? t("media.subtitleOn", { name: subtitleLabel(state.subtitle - 1) })
    : t("media.subtitleOff"),
)

/** 「怎么放的」徽章:直连/自适应/免转码=省(ok 绿),转码中=吃 CPU(attn 琥珀),混流=中性(accent)。
 *  route 缺省(浏览器预览假数据 / 老数据)→ 不显。key 由 core PlaybackRoute snake → camel 对齐字典。 */
const ROUTE_TONE: Record<string, 'ok' | 'attn' | 'accent'> = {
  direct: 'ok',
  hls_copy: 'ok',
  dash: 'accent',
  remux: 'accent',
  hls_transcode: 'attn',
}
const routeInfo = computed(() => {
  const r = state.current?.route
  if (!r) return null
  const camel = r.replace(/_([a-z])/g, (_, c: string) => c.toUpperCase()) // hls_copy → hlsCopy
  return { label: t(`media.route.${camel}`), hint: t(`media.route.${camel}Hint`), tone: ROUTE_TONE[r] ?? 'accent' }
})

function onVolume(e: Event) {
  setVolume(Number((e.target as HTMLInputElement).value) / 100)
}

const video = ref<HTMLVideoElement | null>(null)
/** 每个播放会话一个**全新** <video> 元素(key = 本次会话的流地址,每次注册都换 token):
 *  WKWebView 在「元素出声中原地拆 MSE、复用同一元素开新 MediaSource」后,新会话音频 SB 的
 *  append 会被引擎粘住的旧轨道状态静默丢弃(buffered 恒空 → 切轨无声,2026-07-22 真机定案);
 *  换新元素 = 零残留。watch(video) 会对新元素自动 registerVideoEl 重接线。 */
const videoKey = computed(() => state.current?.stream_url ?? 'idle')
const show = computed(() => state.current?.kind === 'video')

// 起播即接管焦点:把焦点从底下的元素(典型 = 聊天输入框)拿走,否则 onKey 的
// 「输入框让位」会把快捷键全吞掉——打字"放个片"回车起播,焦点仍在 textarea,
// 空格/方向键全打进输入框(全屏与否同病;进全屏那次点击恰好移走焦点才显得"全屏才灵")。
// 窗口态非模态:用户点回输入框打字 = 快捷键让位;点一下小窗(grabFocus)= 拿回快捷键。
watch(
  show,
  (s) => {
    if (s) (document.activeElement as HTMLElement | null)?.blur()
  },
  { immediate: true },
)

/* —— 窗口态小视窗:位置/宽度(会话内记住,组件常驻挂载 ref 即活)——
 * pos = null 表示还停靠在默认右下角(CSS right/bottom 锚定);拖过一次即换显式 left/top。 */
const panelEl = ref<HTMLElement | null>(null)
const pos = ref<{ x: number; y: number } | null>(null)
const boxW = ref(380) // 默认宽度对齐 webrender 任务窗(380×260 量级)
const MIN_W = 280
const EDGE = 8 // 拖动/缩放时距视口边的最小留白

const panelStyle = computed(() => {
  if (state.fullscreen) return {} // 影院态交给 .maximized(inset:0);内联清空免得盖过类
  const s: Record<string, string> = { width: boxW.value + 'px' }
  if (pos.value) {
    s.left = pos.value.x + 'px'
    s.top = pos.value.y + 'px'
  } else {
    s.right = '16px'
    s.bottom = '92px' // 默认停靠右下,抬高避开底部输入区
  }
  return s
})
/** 窄框放不下整排控件:压缩模式只留 播放/进度/当前时刻/全屏,拖宽自然全回来。 */
const compact = computed(() => !state.fullscreen && boxW.value < 560)

function clampPos(x: number, y: number, w: number, h: number) {
  return {
    x: Math.min(Math.max(x, EDGE), Math.max(EDGE, window.innerWidth - w - EDGE)),
    y: Math.min(Math.max(y, EDGE), Math.max(EDGE, window.innerHeight - h - EDGE)),
  }
}

/** 标题栏拖动(按钮除外);先把「停靠右下」折算成显式坐标再跟手,视觉零跳变。
 *  用 window 级 move/up(不靠 pointer capture),拖出面板也不丢跟踪。 */
function onDragStart(e: PointerEvent) {
  if (state.fullscreen) return
  if ((e.target as HTMLElement).closest('button')) return
  const el = panelEl.value
  if (!el) return
  const rect = el.getBoundingClientRect()
  const dx = e.clientX - rect.left
  const dy = e.clientY - rect.top
  pos.value = { x: rect.left, y: rect.top }
  const move = (ev: PointerEvent) => {
    pos.value = clampPos(ev.clientX - dx, ev.clientY - dy, rect.width, rect.height)
  }
  const up = () => {
    window.removeEventListener('pointermove', move)
    window.removeEventListener('pointerup', up)
  }
  window.addEventListener('pointermove', move)
  window.addEventListener('pointerup', up)
  e.preventDefault() // 拖动不选中标题文字
}

/** 右下角把手:拖宽,高度随视频比例自己长(左上角钉住的标准角缩放语义)。
 *  每步 rAF 后按新尺寸 clamp 位置:右缘/下缘顶到视口就整框左移/上移,把手始终跟手
 *  ——停靠在右下角的默认态因此也能直接拖大,不会「没有生长空间」。 */
function onGripDown(e: PointerEvent) {
  if (state.fullscreen) return
  const el = panelEl.value
  if (!el) return
  const rect = el.getBoundingClientRect()
  pos.value = { x: rect.left, y: rect.top } // 停靠态先钉住左上角(右缘锚定会反向生长)
  const startX = e.clientX
  const startW = rect.width
  const move = (ev: PointerEvent) => {
    const cap = Math.max(MIN_W, window.innerWidth * 0.9)
    boxW.value = Math.round(Math.min(Math.max(startW + (ev.clientX - startX), MIN_W), cap))
    requestAnimationFrame(() => {
      const r = el.getBoundingClientRect()
      if (pos.value) pos.value = clampPos(pos.value.x, pos.value.y, r.width, r.height)
    })
  }
  const up = () => {
    window.removeEventListener('pointermove', move)
    window.removeEventListener('pointerup', up)
  }
  window.addEventListener('pointermove', move)
  window.addEventListener('pointerup', up)
  e.preventDefault()
  e.stopPropagation()
}

/** 点一下小窗 = 拿回快捷键:非模态下点视频不会自动改焦点,空格仍会打进聊天输入框。 */
function grabFocus() {
  ;(document.activeElement as HTMLElement | null)?.blur()
}

// 进度条:拖动中只动视觉(scrub),无视 timeupdate 抢拇指;松手(change)才真 seek 一次。
// —— 否则 @input 每 tick 都 seek:本地是 currentTime 风暴,混流是每 tick 重启 ffmpeg。
const dragging = ref(false)
const scrub = ref(0) // 拖动中的百分比 0..100
const pct = computed(() =>
  dragging.value
    ? scrub.value
    : state.duration > 0
      ? Math.min(100, (state.position / state.duration) * 100)
      : 0,
)
/** 时钟:拖动中显示目标位,否则显示真实播放位。 */
const displayPos = computed(() =>
  dragging.value ? (scrub.value / 100) * state.duration : state.position,
)

function onScrubInput(e: Event) {
  dragging.value = true
  scrub.value = Number((e.target as HTMLInputElement).value)
}
function onScrubCommit(e: Event) {
  const v = Number((e.target as HTMLInputElement).value)
  dragging.value = false
  if (state.duration > 0) seek((v / 100) * state.duration)
}

/* —— 光标处的读数:第几分几秒 + 那一刻的画面 ——
 * 治"盲拖":原先要按下去拖起来才看得到目标时间(而且读数在最左边、离光标很远),
 * 按下之前完全没数 —— 而 range 是点哪跳哪。缩略图只有本地片有(thumb_url 有值才出)。 */
const durationRef = computed(() => state.duration)
// 夹在面板里(不是夹在进度条里):带缩略图的气泡比窗口态的进度条还宽,夹轨道会顶出小窗。
const { trackEl, hoverPct, hoverTime, bubbleLeft, bubbleW, onMove, onLeave } =
  useScrubHover(durationRef, { thumbWidth: 11, clampTo: panelEl })
const { src: thumbSrc, available: thumbAvailable } = useScrubThumb(
  computed(() => state.current?.thumb_url),
  hoverTime,
  durationRef,
)
const bubbleEl = ref<HTMLElement | null>(null)
/** 气泡宽度实测(用来把它夹在面板里不出框):有没有图两种宽度,变了就重量一次。
 *  用 nextTick 而不是 rAF —— 要的是"DOM 更新完",而 rAF 在窗口隐藏时压根不触发(§8.1)。 */
watch([hoverPct, thumbSrc], async () => {
  await nextTick()
  bubbleW.value = bubbleEl.value?.offsetWidth ?? 0
})

/** 原生窗口全屏(乐观置位,resize 兜底校准);视频默认全屏的进/退也走它。 */
async function toggleFullscreen() {
  const next = !state.fullscreen
  state.fullscreen = next
  await win.setFullscreen(next)
}

/* —— 看片快捷键(§4.11 用户拍板 2026-09-07 键位表;键位表本体在 useMediaKeys,与音频播放条共用)——
 * 按键分发、帮助浮层(H)、按钮 tooltip 都从同一张表生成;全部动作汇到与按钮 / 嘴控同一执行口(useMedia)。
 * 只在视频浮层在前、且焦点不在输入框时接管。 */
/** OSD:每次按键在画面中央闪一下读数(「1.75x」「+15s」「音量 60%」),0.9s 自隐。 */
const osd = ref<string | null>(null)
let osdTimer = 0
function flashOsd(text: string) {
  osd.value = text
  clearTimeout(osdTimer)
  osdTimer = window.setTimeout(() => (osd.value = null), 900)
}
/** 快捷键速查浮层(H 键 / 「?」钮)。 */
const helpOpen = ref(false)
/** 剧集列表面板(L 键 / 标题栏「≡」钮;多集才有)。全屏 = 右侧侧栏,窗口态 = 标题栏下拉。 */
const listOpen = ref(false)

/* —— 片头 / 片尾(core 汇成的 NowPlaying.skip:手标 > B 站标注 > 章节 > 指纹检测)——
 * 片头:自然播进 [start, end) → 跳到 end,右下 OSD「已跳过片头 · 回看」5 秒可点回去;用户自己拖进片头
 *   = 想看,不跳(本集不再自动跳)。
 * 片尾:自然越过起点且有下一集 → 3 秒倒计时切集(用户拍板 2026-09-07),可取消;拖进片尾区 = 想看字幕,不切。
 * 「自然播放 vs 拖动」靠相邻两次 timeupdate 的跨度判(> SEEK_JUMP_S = 拖了)。换集全部复位。 */
const skipInfo = computed(() => state.current?.skip ?? null)
const SEEK_JUMP_S = 2.5
const COUNTDOWN_S = 3
const skipOsd = ref<{ text: string; undoTo: number } | null>(null)
let skipOsdTimer = 0
const countdown = ref<number | null>(null)
let countdownTimer = 0
let lastPos = 0
let introHandled = false
let outroHandled = false
/** 有没有「下一集」可切:顺序未到末集,或模式不是「放完就停」(列表循环 / 单曲 / 随机 → core auto_next 会回卷 / 重放 / 挑)。 */
const wraps = computed(() => state.playMode !== 'once')
const hasNext = computed(() => {
  const p = playlist.value
  return !!p && (p.index + 1 < p.total || wraps.value)
})
function showSkipOsd(text: string, undoTo: number) {
  skipOsd.value = { text, undoTo }
  clearTimeout(skipOsdTimer)
  skipOsdTimer = window.setTimeout(() => (skipOsd.value = null), 5000)
}
function undoSkip() {
  const u = skipOsd.value
  if (!u) return
  skipOsd.value = null
  seek(u.undoTo)
}
function startCountdown() {
  countdown.value = COUNTDOWN_S
  clearInterval(countdownTimer)
  countdownTimer = window.setInterval(() => {
    if (countdown.value == null) return
    countdown.value -= 1
    if (countdown.value <= 0) {
      cancelCountdown()
      autoNext() // 与自然播完同一条路:core 按顺序 / 循环 / 随机定下一集
    }
  }, 1000)
}
function cancelCountdown() {
  clearInterval(countdownTimer)
  countdown.value = null
}
/** 跳过片头(S 键 / 嘴控走 core 的 seek):记为已处理,OSD 给回看口。 */
function skipIntroNow() {
  const s = skipInfo.value
  if (!s?.intro) return false
  introHandled = true
  seek(s.intro.end)
  showSkipOsd(t('media.skip.skipped'), s.intro.start)
  return true
}
watch(
  () => state.current?.stream_url,
  () => {
    introHandled = false
    outroHandled = false
    lastPos = 0
    cancelCountdown()
    skipOsd.value = null
  },
)
watch(
  () => state.position,
  (p) => {
    const jumped = Math.abs(p - lastPos) > SEEK_JUMP_S
    lastPos = p
    const s = skipInfo.value
    if (!s || state.status !== 'playing' || dragging.value) return
    if (s.intro && !introHandled && p >= s.intro.start && p < s.intro.end - 0.5) {
      introHandled = true
      if (!jumped) skipIntroNow() // 拖进片头 = 想看,只记不跳
    }
    if (s.outro_start != null && !outroHandled && countdown.value == null && p >= s.outro_start) {
      outroHandled = true
      if (!jumped && hasNext.value) startCountdown()
    }
  },
)
onUnmounted(() => {
  cancelCountdown()
  clearTimeout(skipOsdTimer)
})

/** 片头片尾标记菜单(进度条右键 = 光标处的时间;剪刀钮 / S 键 = 当前播放位):三项标记 + 本集在用的
 *  信息一行 + 有手标才出「清除」。落库 / 重算在 core,与嘴控「片头到这里」同一入口。 */
function skipMenuItems(at: number): MenuItem[] {
  const s = skipInfo.value
  const items: MenuItem[] = []
  if (s) {
    const parts: string[] = []
    if (s.intro) parts.push(t('media.skip.intro', { a: fmtClock(s.intro.start), b: fmtClock(s.intro.end) }))
    if (s.outro_start != null) parts.push(t('media.skip.outro', { t: fmtClock(s.outro_start) }))
    items.push({ label: t('media.skip.now', { info: parts.join(' · ') }), disabled: true }, { separator: true })
  }
  const mark = (a: 'intro_start' | 'intro_end' | 'outro_start') => () => {
    markSkip(a, at)
    flashOsd(t('media.skip.marked'))
  }
  items.push(
    { label: t('media.skip.introStart', { t: fmtClock(at) }), action: mark('intro_start') },
    { label: t('media.skip.introEnd', { t: fmtClock(at) }), action: mark('intro_end') },
    { label: t('media.skip.outroStart', { t: fmtClock(at) }), action: mark('outro_start') },
  )
  if (s?.source === 'manual') {
    items.push(
      { separator: true },
      {
        label: t('media.skip.clear'),
        danger: true,
        action: () => {
          markSkip('skip_clear')
          flashOsd(t('media.skip.cleared'))
        },
      },
    )
  }
  return items
}
function onScrubMenu(e: MouseEvent) {
  if (!playlist.value) return // 单部电影没有片头片尾可标(core 也会退回),别弹空菜单
  menu.openMenu(e, skipMenuItems(hoverTime.value ?? state.position))
}
function openSkipMenu(e: MouseEvent) {
  menu.openMenu(e, skipMenuItems(state.position))
}
/** 键盘打开标记菜单:没有鼠标位置,合成一个落在面板中央的事件(openMenu 只读 clientX/Y)。 */
function openSkipMenuAtCenter() {
  const r = panelEl.value?.getBoundingClientRect()
  if (!r) return
  openSkipMenu(new MouseEvent('contextmenu', { clientX: r.left + r.width / 2, clientY: r.top + r.height / 2 }))
}

const keys = useMediaKeys({
  surface: 'video',
  // 倍速菜单(全局右键菜单宿主)开着:Esc / 键盘交给它,别顺手把全屏也退了;没在放视频就不接管
  blocked: () => menu.state.open || !show.value,
  osd: flashOsd,
  status: () => state.status,
  playPause: toggle,
  seekBy,
  volumePct: () => Math.round(state.volume * 100),
  volumeStep: (dir) => setVolume(state.volume + dir * VOL_STEP),
  muted: () => state.muted,
  toggleMute,
  rate: () => state.rate,
  stepRate,
  hasPlaylist: () => !!playlist.value,
  next,
  prev,
  toggleList: () => (listOpen.value = !listOpen.value),
  skip: () => {
    // 有片头且还没过 → 跳过;否则打开标记菜单(第一次按 S 就学会怎么标)
    const s = skipInfo.value
    if (s?.intro && state.position < s.intro.end - 0.5) skipIntroNow()
    else openSkipMenuAtCenter()
  },
  audioTrackCount: () => audioTrackCount.value,
  cycleAudioTrack,
  audioTrackLabel: () => audioTrackLabel(state.current?.audio_track ?? 0),
  captionCount: () => subtitles.value.length,
  cycleCaption: cycleSubtitle,
  captionLabel: () => subtitleText.value,
  toggleFullscreen: () => void toggleFullscreen(),
  // Esc 退全屏:tao 原生全屏在 Windows 不可靠响应 Esc,自己接管
  escape: () => {
    if (helpOpen.value) helpOpen.value = false
    else if (listOpen.value) listOpen.value = false
    else if (state.fullscreen) void toggleFullscreen()
  },
  toggleHelp: () => (helpOpen.value = !helpOpen.value),
})
const kb = keys.kb

function onKey(e: KeyboardEvent) {
  if (keys.onKey(e)) showControls() // 调整后让控制条浮现一下(全屏态)
}

// 控制条覆盖在画面上,播放中 2.8s 无操作自动隐藏(鼠标一动即现)。两种模式同一套:
// 全屏影院藏上下两条 + 光标;窗口小窗只藏底部控制条(标题栏 = 拖动把手,常显)。
const controlsVisible = ref(true)
let hideTimer = 0
function showControls() {
  controlsVisible.value = true
  clearTimeout(hideTimer)
  if (state.status === 'playing') {
    hideTimer = window.setTimeout(() => (controlsVisible.value = false), 2800)
  }
}
watch(
  () => state.fullscreen,
  (fs) => {
    showControls() // 切换模式先亮一下(播放中会自动再藏)
    // 置顶跟随影院态:全屏 = 看片别被盖;窗口小窗 = 别拿整个主窗压着别的程序。
    // 起播(useMedia)/stop 两端各自已设,这里兜「中途进出全屏」;重复设置幂等无害。
    if (show.value) void win.setAlwaysOnTop(fs)
  },
)
watch(
  () => state.status,
  (s) => {
    if (s === 'playing') showControls()
    else {
      clearTimeout(hideTimer) // 暂停/加载时别把控制条藏了
      controlsVisible.value = true
    }
  },
)

let stopResize = () => {}
watch(video, (el) => registerVideoEl(el))
onMounted(() => {
  window.addEventListener('keydown', onKey)
  // 与真实窗口态校准:只在有视频(show)时纠 state.fullscreen。没视频时若也跟随窗口全屏,会把
  // 「手动最大化 / 窗口全屏」误写进这个"影院全屏"状态,让 WindowControls 误藏三键 → 卡死出不来
  // (2026-07-11 根因:两组件对 media.fullscreen 的语义漂移)。没视频时它恒为 false,三键正常显示。
  stopResize = win.onResized(async () => {
    if (!show.value) return
    state.fullscreen = await win.isFullscreen()
    // 主窗变小可能把小窗甩出视口:按新视口收窄宽度 + 拉回位置(停靠态右下锚定,天然不越界)
    if (!state.fullscreen && pos.value) {
      boxW.value = Math.min(boxW.value, Math.max(MIN_W, window.innerWidth - EDGE * 2))
      requestAnimationFrame(() => {
        const el = panelEl.value
        if (!el || !pos.value) return
        const r = el.getBoundingClientRect()
        pos.value = clampPos(pos.value.x, pos.value.y, r.width, r.height)
      })
    }
  })
})
onUnmounted(() => {
  window.removeEventListener('keydown', onKey)
  clearTimeout(hideTimer)
  stopResize()
  registerVideoEl(null)
})
</script>

<template>
  <div
    v-if="show"
    ref="panelEl"
    class="panel"
    :class="{ maximized: state.fullscreen, 'controls-hidden': !controlsVisible }"
    :style="panelStyle"
    @mousemove="showControls"
    @pointerdown="grabFocus"
  >
    <header class="bar top" @pointerdown="onDragStart">
      <span class="title">{{ state.current!.title }}</span>
      <span
        v-if="routeInfo"
        class="route"
        :class="'tone-' + routeInfo.tone"
        :title="routeInfo.hint"
        >{{ routeInfo.label }}</span
      >
      <span v-if="playlist" class="ep">{{
        t('media.episodeOf', { cur: playlist.index + 1, total: playlist.total })
      }}</span>
      <!-- 选集:多集才有;全屏右侧侧栏 / 窗口态标题栏下拉(L) -->
      <button
        v-if="playlist"
        class="vbtn"
        :class="{ on: listOpen }"
        @click="listOpen = !listOpen"
        :title="kb(t('media.episodeList'), 'L')"
      >
        ≡
      </button>
      <button class="vbtn" @click="stop" :title="t('media.closeVideo')">✕</button>
    </header>
    <EpisodeList v-model:open="listOpen" :variant="state.fullscreen ? 'side' : 'drop'" />
    <!-- poster:网络源的封面(B 站视频封面)在加载黑屏期先顶着;本地片没有,不设 -->
    <video :key="videoKey" ref="video" class="screen" playsinline :poster="state.current?.cover_url" @dblclick="toggleFullscreen">
      <!-- 字幕:core 现转现回 WebVTT;默认全 disabled,按钮/嘴控切 mode(见 useMedia.setSubtitle) -->
      <track v-for="(s, i) in subtitles" :key="s.url" kind="subtitles" :src="s.url" :srclang="s.lang" :label="subtitleLabel(i)" />
    </video>
    <div v-if="state.status === 'loading'" class="spinner" aria-hidden="true"></div>
    <!-- 按键 OSD:画面中央闪一下读数(倍速 / 音量 / ±秒),0.9s 自隐 -->
    <Transition name="osd">
      <div v-if="osd" class="osd" aria-live="polite">{{ osd }}</div>
    </Transition>
    <!-- 跳过片头的回看口(5 秒);片尾倒计时切集(可取消)。都挂右下、控制条之上 -->
    <button v-if="skipOsd" class="skip-osd" @click.stop="undoSkip">
      {{ skipOsd.text }} · {{ t('media.skip.undo') }}
    </button>
    <div v-if="countdown != null" class="countdown" @pointerdown.stop>
      <span>{{ t('media.skip.nextIn', { s: countdown }) }}</span>
      <button class="vbtn small" @click.stop="cancelCountdown">{{ t('media.skip.cancel') }}</button>
    </div>
    <!-- 快捷键速查(H):从同一张键位表生成;点外 / Esc / H 关 -->
    <div v-if="helpOpen" class="help" @click="helpOpen = false" @pointerdown.stop>
      <KeyHelpCard :rows="keys.help.value" variant="overlay" />
    </div>
    <footer class="bar bottom">
      <button
        v-if="playlist"
        class="vbtn"
        @click="prev"
        :disabled="!wraps && playlist.index <= 0"
        :title="kb(t('media.prevEp'), 'PageUp')"
      >
        ⏮
      </button>
      <button class="vbtn" @click="toggle" :title="kb(state.status === 'playing' ? t('media.pause') : t('media.play'), 'Space')">
        {{ state.status === 'playing' ? '⏸' : '▶' }}
      </button>
      <button
        v-if="playlist"
        class="vbtn"
        @click="next"
        :disabled="!wraps && playlist.index >= playlist.total - 1"
        :title="kb(t('media.nextEp'), 'PageDown')"
      >
        ⏭
      </button>
      <span class="clock"
        >{{ fmtClock(displayPos) }}<template v-if="!compact"> / {{ fmtClock(state.duration) }}</template></span
      >
      <!-- 进度条外面套一层 hover 检测:光标所在处的时间(+ 本地片的那一刻画面)。
           拖动中 range 会抓走 pointer capture,但事件照样冒泡到这层 → 气泡跟着拇指走。 -->
      <div
        ref="trackEl"
        class="scrub-track"
        @pointermove="onMove"
        @pointerleave="onLeave"
        @pointercancel="onLeave"
        @contextmenu="onScrubMenu"
      >
        <div
          v-if="hoverPct !== null"
          ref="bubbleEl"
          class="hover-bubble"
          :style="{ left: bubbleLeft + 'px' }"
        >
          <img
            v-if="thumbAvailable && thumbSrc"
            class="hover-thumb"
            :src="thumbSrc"
            alt=""
            decoding="async"
          />
          <span class="hover-time">{{ fmtClock(hoverTime ?? 0) }}</span>
        </div>
        <input
          class="slider"
          type="range"
          min="0"
          max="100"
          step="0.1"
          :value="pct"
          @input="onScrubInput"
          @change="onScrubCommit"
          :style="{ '--pct': pct + '%' }"
        />
      </div>
      <button
        v-if="audioTrackCount >= 2 && !compact"
        class="vbtn rate"
        @click="cycleAudioTrack"
        :title="kb(t('media.audioTrack', { label: audioLabel }), 'A')"
      >
        {{ audioLabel }}
      </button>
      <!-- 字幕:有才出现(没字幕的片不该多一个点了没反应的按钮);关 → 第一条 → … → 关 -->
      <button
        v-if="subtitles.length >= 1 && !compact"
        class="vbtn rate"
        :class="{ on: state.subtitle >= 1 }"
        @click="cycleSubtitle"
        :title="kb(subtitleText, 'C')"
      >
        CC
      </button>
      <!-- 片头片尾标记(多集才有):点开菜单,时间取当前播放位;进度条右键 = 光标处的时间 -->
      <button
        v-if="playlist && !compact"
        class="vbtn"
        :class="{ on: !!skipInfo }"
        @click="openSkipMenu"
        :title="kb(t('media.skip.menu'), 'S')"
      >
        <svg viewBox="0 0 24 24" aria-hidden="true">
          <circle cx="6" cy="6" r="3" />
          <circle cx="6" cy="18" r="3" />
          <path d="M20 4 8.1 15.9" />
          <path d="M14.5 14.5 20 20" />
          <path d="M8.1 8.1 12 12" />
        </svg>
      </button>
      <!-- 倍速:点开档位菜单(全局右键菜单宿主,当前档打勾);悬停滚轮一档一档调 -->
      <button
        v-if="!compact"
        class="vbtn rate"
        @click="openRateMenu"
        @wheel.prevent="stepRate($event.deltaY < 0 ? 1 : -1)"
        :title="t('media.speedPick', { rate: state.rate })"
      >
        {{ state.rate }}x
      </button>
      <!-- 静音:基准音量不动,取消即回(M);内联 SVG 而非 🔇 emoji —— emoji 恒彩色无视 CSS color -->
      <button
        v-if="!compact"
        class="vbtn"
        :class="{ on: state.muted }"
        @click="toggleMute"
        :title="kb(state.muted ? t('media.unmute') : t('media.mute'), 'M')"
      >
        <svg viewBox="0 0 24 24" aria-hidden="true">
          <path d="M4 9v6h4l5 4V5L8 9H4z" />
          <template v-if="state.muted">
            <path d="m17 9 4 6" />
            <path d="m21 9-4 6" />
          </template>
          <template v-else>
            <path d="M16.5 8.5a5 5 0 0 1 0 7" />
            <path d="M19 6a8.5 8.5 0 0 1 0 12" />
          </template>
        </svg>
      </button>
      <input
        v-if="!compact"
        class="vol-slider"
        type="range"
        min="0"
        max="100"
        :value="state.muted ? 0 : Math.round(state.volume * 100)"
        @input="onVolume"
        :title="t('media.volume')"
        :style="{ '--pct': (state.muted ? 0 : state.volume * 100) + '%' }"
      />
      <button v-if="!compact" class="vbtn" @click="helpOpen = !helpOpen" :title="kb(t('media.keys.title'), 'H')">?</button>
      <button class="vbtn" @click="toggleFullscreen" :title="kb(t('media.fullscreen'), 'F')">⛶</button>
    </footer>
    <div v-if="!state.fullscreen" class="grip" @pointerdown="onGripDown" aria-hidden="true"></div>
  </div>
</template>

<style scoped>
/* 窗口态 = 非模态应用内小视窗(webrender 可见任务窗同款形态):右下停靠、可拖、拖角缩放。
   位置/宽度走内联样式(panelStyle);全屏时内联清空,交给 .maximized 铺满。 */
.panel {
  position: fixed; z-index: 30;
  display: flex; flex-direction: column;
  border-radius: 14px; overflow: hidden;
  background: var(--surface); /* 窗口模式机框随皮肤(科幻=玻璃,护眼/暖萌=近不透明);全屏下被 #000 覆盖 */
  border: 1px solid rgba(var(--accent-rgb), 0.22);
  box-shadow: 0 18px 60px rgba(0, 0, 0, 0.55), 0 0 30px rgba(var(--accent-rgb), 0.08);
}
/* 全屏 = 原生窗口全屏 + 这个类铺满(不再用 :fullscreen 伪类)。影院视图:画面铺满整屏(黑底、
   无边框无投影),控制条覆盖在画面上(不再夹小画面、不再露主窗一圈透明边框)。 */
.panel.maximized {
  inset: 0; width: 100%; height: 100%;
  border: none; border-radius: 0; box-shadow: none; background: #000;
}
.panel.maximized .screen {
  position: absolute; inset: 0; z-index: 0;
  width: 100%; height: 100%; min-height: 0; max-height: none;
  object-fit: contain; /* 不裁不拉伸,留黑边 */
}
/* 底部控制条:两种模式都覆盖在画面上(黑底渐变),播放中 2.8s 无操作自动隐藏(鼠标一动即现)。
   覆盖媒体豁免:恒亮浅字不随皮肤(浅皮深字压在视频上读不清),同 #000 视频底。 */
.bar.bottom {
  position: absolute; left: 0; right: 0; bottom: 0; z-index: 2;
  padding-bottom: 12px;
  background: linear-gradient(to top, rgba(0, 0, 0, 0.65), rgba(0, 0, 0, 0));
  color: #eaf2fb;
  transition: opacity 0.25s ease;
}
.bar.bottom .clock { color: rgba(234, 242, 251, 0.72); }
.panel.controls-hidden .bar.bottom { opacity: 0; pointer-events: none; }
/* 窗口态标题栏 = 拖动把手(细条、常显、随皮肤);全屏影院才转覆盖式,跟控制条一起隐、并藏光标 */
.bar.top { cursor: grab; user-select: none; -webkit-user-select: none; touch-action: none; }
.panel:not(.maximized) .bar.top { padding: 6px 10px; }
.panel:not(.maximized) .bar.top .vbtn { width: 24px; height: 24px; border-radius: 7px; font-size: 12px; }
.panel.maximized .bar.top {
  cursor: default;
  position: absolute; top: 0; left: 0; right: 0; z-index: 2;
  background: linear-gradient(to bottom, rgba(0, 0, 0, 0.65), rgba(0, 0, 0, 0));
  color: #eaf2fb;
  transition: opacity 0.25s ease;
}
.panel.maximized.controls-hidden { cursor: none; }
.panel.maximized.controls-hidden .bar.top { opacity: 0; pointer-events: none; }

.screen { width: 100%; max-height: 78vh; background: #000; display: block; }

/* 加载/混流换台 spinner:黑屏期间显示"在转",别看着像卡死(混流 ?t= seek 必有黑屏间隙)。 */
.spinner {
  position: absolute; top: 50%; left: 50%; z-index: 1;
  width: 34px; height: 34px; margin: -17px 0 0 -17px;
  border: 3px solid rgba(var(--accent-rgb), 0.22);
  border-top-color: var(--accent); border-radius: 50%;
  animation: lw-spin 0.8s linear infinite; pointer-events: none;
}
@keyframes lw-spin { to { transform: rotate(360deg); } }

.bar {
  display: flex; align-items: center; gap: 10px;
  padding: 9px 13px;
  color: var(--text); font-size: 13px;
}
.title { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; letter-spacing: .4px; }
.ep {
  flex: none; color: var(--accent); font-size: 11.5px; letter-spacing: .4px;
  padding: 2px 8px; border-radius: 999px;
  background: rgba(var(--accent-rgb), 0.12); border: 1px solid rgba(var(--accent-rgb), 0.28);
}
/* 「怎么放的」徽章:自设 color 覆盖全屏态强制的浅字(否则读不出 tone);语义 token,随皮肤。 */
.route {
  flex: none; font-size: 11px; letter-spacing: .3px; white-space: nowrap;
  padding: 2px 8px; border-radius: 999px; cursor: default;
}
.route.tone-ok { color: var(--ok); background: rgba(var(--ok-rgb), 0.12); border: 1px solid rgba(var(--ok-rgb), 0.30); }
.route.tone-attn { color: var(--attn); background: rgba(var(--attn-rgb), 0.14); border: 1px solid rgba(var(--attn-rgb), 0.34); }
.route.tone-accent { color: var(--accent); background: rgba(var(--accent-rgb), 0.10); border: 1px solid rgba(var(--accent-rgb), 0.26); }
.clock { color: var(--text-dim); font: 11px/1 ui-monospace, "SF Mono", monospace; letter-spacing: .5px; flex: none; }

.vbtn {
  width: 32px; height: 32px; flex: none;
  border: 1px solid rgba(var(--accent-rgb), 0.18); border-radius: 9px; cursor: pointer;
  background: rgba(var(--accent-rgb), 0.08); color: var(--accent); font-size: 13px;
}
.vbtn:hover { border-color: var(--accent); box-shadow: 0 0 12px rgba(var(--accent-rgb), 0.3); }
.vbtn:disabled { opacity: .32; cursor: default; border-color: rgba(var(--accent-rgb), 0.12); box-shadow: none; }

/* 进度条的定位容器:滑杆照旧 flex:1 撑满它,hover 气泡绝对定位挂在它上方(不占位)。
   `.slider { margin: 0 }` 是要紧的:Chrome 给 input[type=range] 的 UA 默认样式带 margin:2px,
   两端各缩 2px → 容器宽 ≠ 真实轨道宽,hover 换算出来的秒数与拇指就差那么几像素(预览量出来
   145.5 vs 141.5)。清掉之后"量容器"就等于"量轨道",useScrubHover 只需要一个参照物。 */
.scrub-track { position: relative; flex: 1; min-width: 0; display: flex; align-items: center; }
.scrub-track .slider { margin: 0; }

/* 光标处读数:小图在上、时间在下,夹在条内不出框(bubbleLeft 已算好);
   覆盖媒体豁免同控制条 —— 恒黑底浅字,压在画面上才读得清。 */
.hover-bubble {
  position: absolute; bottom: 15px; z-index: 4;
  transform: translateX(-50%); pointer-events: none;
  display: flex; flex-direction: column; align-items: center; gap: 4px;
  padding: 4px; border-radius: 9px;
  background: rgba(0, 0, 0, 0.8);
  border: 1px solid rgba(var(--accent-rgb), 0.3);
  box-shadow: 0 8px 24px rgba(0, 0, 0, 0.55);
}
/* 宽度与后端抽帧的 THUMB_WIDTH 一致(高度按片子比例走,不写死免变形) */
.hover-thumb { display: block; width: 192px; height: auto; border-radius: 5px; background: #000; }
.hover-time {
  font: 11px/1 ui-monospace, "SF Mono", monospace; letter-spacing: 0.5px;
  color: #eaf2fb; padding: 1px 3px; white-space: nowrap;
}

.slider {
  -webkit-appearance: none; appearance: none; flex: 1; height: 3px; border-radius: 2px;
  background: linear-gradient(90deg, var(--accent) var(--pct), rgba(var(--accent-rgb), 0.14) var(--pct));
  outline: none; cursor: pointer;
}
.slider::-webkit-slider-thumb {
  -webkit-appearance: none; appearance: none;
  width: 11px; height: 11px; border-radius: 50%;
  background: var(--accent); box-shadow: 0 0 8px rgba(var(--accent-rgb), 0.8);
}

.vbtn.rate { width: auto; padding: 0 9px; font: 11px/1 ui-monospace, "SF Mono", monospace; }
.vbtn svg { width: 16px; height: 16px; fill: none; stroke: currentColor; stroke-width: 1.8; stroke-linecap: round; stroke-linejoin: round; }
.vbtn.on { border-color: var(--accent); background: rgba(var(--accent-rgb), 0.22); }

/* 选集下拉(窗口态):挂在标题栏下方;全屏侧栏由组件自己定位 */
.eplist.drop { top: 46px; max-height: min(320px, calc(100% - 110px)); }

/* 跳过片头回看口 / 片尾倒计时:右下角、控制条之上(覆盖媒体豁免:恒亮浅字压黑底) */
.skip-osd, .countdown {
  position: absolute; right: 16px; bottom: 64px; z-index: 3;
  display: flex; align-items: center; gap: 10px;
  padding: 8px 14px; border-radius: 999px;
  background: rgba(0, 0, 0, 0.72); color: #eaf2fb; font-size: 12.5px;
  border: 1px solid rgba(var(--accent-rgb), 0.35);
  box-shadow: 0 8px 24px rgba(0, 0, 0, 0.45);
}
.skip-osd { cursor: pointer; }
.skip-osd:hover { border-color: var(--accent); }
.vbtn.small { width: auto; height: 24px; padding: 0 10px; font-size: 11.5px; }

/* 按键 OSD:画面中央的读数药丸(覆盖媒体豁免:恒亮浅字压黑底,不随皮肤) */
.osd {
  position: absolute; top: 50%; left: 50%; z-index: 3;
  transform: translate(-50%, -50%);
  padding: 8px 18px; border-radius: 999px;
  background: rgba(0, 0, 0, 0.62); color: #eaf2fb;
  font: 15px/1 ui-monospace, "SF Mono", monospace; letter-spacing: 0.5px;
  pointer-events: none; white-space: nowrap;
}
.osd-enter-active, .osd-leave-active { transition: opacity 0.18s ease, transform 0.18s ease; }
.osd-enter-from, .osd-leave-to { opacity: 0; transform: translate(-50%, -50%) scale(0.92); }

/* 快捷键速查:盖在画面上的半透黑幕 + 卡片(KeyHelpCard overlay 观感,键帽 / 说明两列) */
.help {
  position: absolute; inset: 0; z-index: 5;
  display: flex; align-items: center; justify-content: center;
  background: rgba(0, 0, 0, 0.45);
}
.vol-slider {
  -webkit-appearance: none; appearance: none; width: 70px; height: 3px; border-radius: 2px; flex: none;
  background: linear-gradient(90deg, var(--accent) var(--pct), rgba(var(--accent-rgb), 0.14) var(--pct));
  outline: none; cursor: pointer;
}
.vol-slider::-webkit-slider-thumb {
  -webkit-appearance: none; appearance: none;
  width: 9px; height: 9px; border-radius: 50%;
  background: var(--accent); box-shadow: 0 0 6px rgba(var(--accent-rgb), 0.8);
}

/* 右下角缩放把手(浮在控制条上层;控制条自动隐藏后仍可用) */
.grip {
  position: absolute; right: 0; bottom: 0; z-index: 3;
  width: 16px; height: 16px; cursor: nwse-resize;
}
.grip::before {
  content: ''; position: absolute; right: 4px; bottom: 4px;
  width: 7px; height: 7px;
  border-right: 2px solid rgba(var(--accent-rgb), 0.55);
  border-bottom: 2px solid rgba(var(--accent-rgb), 0.55);
}
</style>
