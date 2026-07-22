<script setup lang="ts">
// 视频浮层:current.kind === 'video' 时出现。<video> 挂载即向 VM 登记。
// 全屏 = 原生窗口全屏(非 HTML5 requestFullscreen —— 后者在 WebView2 上与 DWM 合成器打架,
// 闪烁/退出穿帮),包壳加 .maximized 铺满;混流流无原生 seek,滑杆松手 = 换 src 重启(?t=)。
// 窗口态 = 非模态应用内小视窗(webrender 可见任务窗同款形态:右下停靠、标题栏拖动、拖角缩放),
// 底下界面照常可用。不开真原生第二窗:播放引擎(MSE/relay 会话/useMedia VM)全在主窗 WebView,
// 挪窗 = 悬浮窗双播陷阱同族 + 采集端 AEC 参考信号断链(§7.5)。
import { computed, onMounted, onUnmounted, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import { registerVideoEl, useMedia } from '../composables/useMedia'
import { win } from '../lib/backend'
import { fmtClock } from '../lib/fmt'

const { t } = useI18n()
const { state, toggle, stop, seek, setVolume, setRate, next, prev, cycleAudioTrack, audioTrackLabel } =
  useMedia()

/** 多集剧集才出集数指示 + 上/下一集按钮(单集/电影为 null,不出现)。 */
const playlist = computed(() => state.current?.playlist ?? null)
/** ≥2 条音轨才出切换钮(双语片);label = 当前轨的友好名(国语/英语/元数据标题)。 */
const audioTrackCount = computed(() => state.current?.audio_tracks?.length ?? 0)
const audioLabel = computed(() => audioTrackLabel(state.current?.audio_track ?? 0))

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

/** 倍速循环挡位(点一下进一档,家庭场景不需要精调)。 */
const RATES = [1, 1.25, 1.5, 2, 0.75]
function cycleRate() {
  const i = RATES.indexOf(state.rate)
  setRate(RATES[(i + 1) % RATES.length] ?? 1)
}

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

/** 原生窗口全屏(乐观置位,resize 兜底校准);视频默认全屏的进/退也走它。 */
async function toggleFullscreen() {
  const next = !state.fullscreen
  state.fullscreen = next
  await win.setFullscreen(next)
}

/** 看片快捷键:空格=播放/暂停、↑↓=音量、←→=快进退 20s、Esc=退全屏。
 * 在输入框打字时不抢键;空格/方向键会 preventDefault(否则页面滚动/翻页)。 */
const SEEK_STEP = 20 // 秒
const VOL_STEP = 0.1
function onKey(e: KeyboardEvent) {
  // Esc 退全屏:tao 原生全屏在 Windows 不可靠响应 Esc,自己接管。
  if (e.key === 'Escape' && state.fullscreen) {
    e.preventDefault()
    e.stopPropagation()
    void toggleFullscreen()
    return
  }
  // 正在真文本输入(输入框/可编辑区打字)→ 让位,别抢键。浮层自己的滑杆(range)不算:
  // 拖完进度条/音量条焦点留在滑杆上,快捷键要照常生效(行为恒定,不随焦点漂)。
  const t = e.target as HTMLElement | null
  if (
    t &&
    (t.isContentEditable ||
      (/^(INPUT|TEXTAREA|SELECT)$/.test(t.tagName) && (t as HTMLInputElement).type !== 'range'))
  )
    return
  // 带修饰键的组合留给系统/其它快捷键
  if (e.ctrlKey || e.metaKey || e.altKey) return
  // 没有在播放的内容就不接管(避免在别的视图误吞键)
  if (!state.current) return
  switch (e.key) {
    case ' ':
    case 'Spacebar': // 老 Edge/IE 的空格键名
      e.preventDefault()
      toggle()
      break
    case 'ArrowUp':
      e.preventDefault()
      setVolume(state.volume + VOL_STEP)
      break
    case 'ArrowDown':
      e.preventDefault()
      setVolume(state.volume - VOL_STEP)
      break
    case 'ArrowLeft':
      e.preventDefault()
      seek(Math.max(0, state.position - SEEK_STEP))
      break
    case 'ArrowRight': {
      e.preventDefault()
      const cap = state.duration > 0 ? state.duration : state.position + SEEK_STEP
      seek(Math.min(cap, state.position + SEEK_STEP))
      break
    }
    default:
      return
  }
  showControls() // 调整后让控制条/OSD 浮现一下(全屏态)
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
      <button class="vbtn" @click="stop" :title="t('media.closeVideo')">✕</button>
    </header>
    <video :key="videoKey" ref="video" class="screen" playsinline @dblclick="toggleFullscreen"></video>
    <div v-if="state.status === 'loading'" class="spinner" aria-hidden="true"></div>
    <footer class="bar bottom">
      <button
        v-if="playlist"
        class="vbtn"
        @click="prev"
        :disabled="playlist.index <= 0"
        :title="t('media.prevEp')"
      >
        ⏮
      </button>
      <button class="vbtn" @click="toggle">
        {{ state.status === 'playing' ? '⏸' : '▶' }}
      </button>
      <button
        v-if="playlist"
        class="vbtn"
        @click="next"
        :disabled="playlist.index >= playlist.total - 1"
        :title="t('media.nextEp')"
      >
        ⏭
      </button>
      <span class="clock"
        >{{ fmtClock(displayPos) }}<template v-if="!compact"> / {{ fmtClock(state.duration) }}</template></span
      >
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
      <button
        v-if="audioTrackCount >= 2 && !compact"
        class="vbtn rate"
        @click="cycleAudioTrack"
        :title="t('media.audioTrack', { label: audioLabel })"
      >
        {{ audioLabel }}
      </button>
      <button v-if="!compact" class="vbtn rate" @click="cycleRate" :title="t('media.speed')">
        {{ state.rate }}x
      </button>
      <input
        v-if="!compact"
        class="vol-slider"
        type="range"
        min="0"
        max="100"
        :value="Math.round(state.volume * 100)"
        @input="onVolume"
        :title="t('media.volume')"
        :style="{ '--pct': state.volume * 100 + '%' }"
      />
      <button class="vbtn" @click="toggleFullscreen" :title="t('media.fullscreen')">⛶</button>
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
