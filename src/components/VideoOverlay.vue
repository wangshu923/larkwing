<script setup lang="ts">
// 视频浮层:current.kind === 'video' 时出现。<video> 挂载即向 VM 登记。
// 全屏 = 原生窗口全屏(非 HTML5 requestFullscreen —— 后者在 WebView2 上与 DWM 合成器打架,
// 闪烁/退出穿帮),包壳加 .maximized 铺满;混流流无原生 seek,滑杆松手 = 换 src 重启(?t=)。
import { computed, onMounted, onUnmounted, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import { registerVideoEl, useMedia } from '../composables/useMedia'
import { win } from '../lib/backend'
import { fmtClock } from '../lib/fmt'

const { t } = useI18n()
const { state, toggle, stop, seek, setVolume, setRate, next, prev } = useMedia()

/** 多集剧集才出集数指示 + 上/下一集按钮(单集/电影为 null,不出现)。 */
const playlist = computed(() => state.current?.playlist ?? null)

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
const show = computed(() => state.current?.kind === 'video')

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
  // 正在输入框/可编辑区打字 → 让位,别抢键
  const t = e.target as HTMLElement | null
  if (t && (t.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(t.tagName))) return
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

// 全屏 = 影院视图:控制条覆盖在画面上,播放中 2.8s 无操作自动隐藏(鼠标一动即现);
// 窗口模式常显。这样全屏不再"一直挂着标题栏 X、画面被上下条夹小"。
const controlsVisible = ref(true)
let hideTimer = 0
function showControls() {
  if (!state.fullscreen) return
  controlsVisible.value = true
  clearTimeout(hideTimer)
  if (state.status === 'playing') {
    hideTimer = window.setTimeout(() => (controlsVisible.value = false), 2800)
  }
}
watch(
  () => state.fullscreen,
  (fs) => {
    clearTimeout(hideTimer)
    controlsVisible.value = true
    if (fs) showControls()
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
  // 与真实窗口态校准:WindowControls 的 F11 / OS 拒绝都会触发 resize,纠回 state.fullscreen
  //(TasksOverlay 缩 mini、本浮层 .maximized 都依赖它)。
  stopResize = win.onResized(async () => {
    state.fullscreen = await win.isFullscreen()
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
  <div v-if="show" class="veil">
    <div
      class="panel"
      :class="{ maximized: state.fullscreen, 'controls-hidden': state.fullscreen && !controlsVisible }"
      @mousemove="showControls"
    >
      <header class="bar top">
        <span class="title">{{ state.current!.title }}</span>
        <span v-if="playlist" class="ep">{{
          t('media.episodeOf', { cur: playlist.index + 1, total: playlist.total })
        }}</span>
        <button class="vbtn" @click="stop" :title="t('media.closeVideo')">✕</button>
      </header>
      <video ref="video" class="screen" playsinline @dblclick="toggleFullscreen"></video>
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
        <span class="clock">{{ fmtClock(displayPos) }} / {{ fmtClock(state.duration) }}</span>
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
        <button class="vbtn rate" @click="cycleRate" :title="t('media.speed')">
          {{ state.rate }}x
        </button>
        <input
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
    </div>
  </div>
</template>

<style scoped>
.veil {
  position: fixed; inset: 0; z-index: 30;
  display: flex; align-items: center; justify-content: center;
  background: rgba(var(--veil-rgb, 0 0 0), 0.6); /* 模态暗罩:同 App.vue 数据弹窗约定,各皮一致地压暗背景 */
  backdrop-filter: blur(5px); -webkit-backdrop-filter: blur(5px);
}
.panel {
  position: relative;
  width: min(80vw, 980px);
  display: flex; flex-direction: column;
  border-radius: 14px; overflow: hidden;
  background: var(--surface); /* 窗口模式机框随皮肤(科幻=玻璃,护眼/暖萌=近不透明);全屏下被 #000 覆盖 */
  border: 1px solid rgba(var(--accent-rgb), 0.22);
  box-shadow: 0 18px 60px rgba(0, 0, 0, 0.55), 0 0 30px rgba(var(--accent-rgb), 0.08);
}
/* 全屏 = 原生窗口全屏 + 这个类铺满(不再用 :fullscreen 伪类)。影院视图:画面铺满整屏(黑底、
   无边框无投影),控制条覆盖在画面上(不再夹小画面、不再露主窗一圈透明边框)。 */
.panel.maximized {
  width: 100%; height: 100%;
  border: none; border-radius: 0; box-shadow: none; background: #000;
}
.panel.maximized .screen {
  position: absolute; inset: 0; z-index: 0;
  width: 100%; height: 100%; min-height: 0; max-height: none;
  object-fit: contain; /* 不裁不拉伸,留黑边 */
}
.panel.maximized .bar {
  position: absolute; left: 0; right: 0; z-index: 2;
  transition: opacity 0.25s ease;
}
.panel.maximized .bar.top { top: 0; background: linear-gradient(to bottom, rgba(0, 0, 0, 0.65), rgba(0, 0, 0, 0)); }
.panel.maximized .bar.bottom { bottom: 0; padding-bottom: 14px; background: linear-gradient(to top, rgba(0, 0, 0, 0.65), rgba(0, 0, 0, 0)); }
/* 影院视图播放中自动隐藏控制条(鼠标一动即现) */
.panel.controls-hidden { cursor: none; }
.panel.controls-hidden .bar { opacity: 0; pointer-events: none; }

.screen { width: 100%; max-height: 62vh; background: #000; display: block; }

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
/* 全屏 = 控制条覆盖在视频画面上(黑底渐变) → 文字/时钟恒亮:这是「覆盖媒体」豁免(同 #000 视频底),
   不随皮肤,否则浅皮的深色字压在视频上读不清。 */
.panel.maximized .bar { color: #eaf2fb; }
.panel.maximized .clock { color: rgba(234, 242, 251, 0.72); }
.title { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; letter-spacing: .4px; }
.ep {
  flex: none; color: var(--accent); font-size: 11.5px; letter-spacing: .4px;
  padding: 2px 8px; border-radius: 999px;
  background: rgba(var(--accent-rgb), 0.12); border: 1px solid rgba(var(--accent-rgb), 0.28);
}
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
</style>
