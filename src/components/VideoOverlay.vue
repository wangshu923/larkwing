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
const { state, toggle, stop, seek, setVolume, setRate } = useMedia()

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

/** Esc 退全屏:tao 原生全屏在 Windows 不可靠响应 Esc,自己接管。 */
function onKey(e: KeyboardEvent) {
  if (e.key === 'Escape' && state.fullscreen) {
    e.preventDefault()
    e.stopPropagation()
    void toggleFullscreen()
  }
}

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
  stopResize()
  registerVideoEl(null)
})
</script>

<template>
  <div v-if="show" class="veil">
    <div class="panel" :class="{ maximized: state.fullscreen }">
      <header class="bar top">
        <span class="title">{{ state.current!.title }}</span>
        <button class="vbtn" @click="stop" :title="t('media.closeVideo')">✕</button>
      </header>
      <video ref="video" class="screen" playsinline></video>
      <footer class="bar bottom">
        <button class="vbtn" @click="toggle">
          {{ state.status === 'playing' ? '⏸' : '▶' }}
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
  background: rgba(3, 8, 18, 0.6);
  backdrop-filter: blur(5px); -webkit-backdrop-filter: blur(5px);
}
.panel {
  width: min(80vw, 980px);
  display: flex; flex-direction: column;
  border-radius: 14px; overflow: hidden;
  background: rgba(8, 18, 36, 0.92);
  border: 1px solid rgba(95, 200, 255, 0.22);
  box-shadow: 0 18px 60px rgba(0, 0, 0, 0.55), 0 0 30px rgba(95, 200, 255, 0.08);
}
/* 全屏 = 原生窗口全屏 + 这个类铺满(不再用 :fullscreen 伪类;height:100% 让包壳在 .veil 内撑开)。 */
.panel.maximized { width: 100%; height: 100%; border-radius: 0; }
.panel.maximized .screen { flex: 1; max-height: none; }

.screen { width: 100%; max-height: 62vh; background: #000; display: block; }

.bar {
  display: flex; align-items: center; gap: 10px;
  padding: 9px 13px;
  color: #d4e6f7; font-size: 13px;
}
.title { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; letter-spacing: .4px; }
.clock { color: #85a4c0; font: 11px/1 ui-monospace, "SF Mono", monospace; letter-spacing: .5px; flex: none; }

.vbtn {
  width: 32px; height: 32px; flex: none;
  border: 1px solid rgba(95, 200, 255, 0.18); border-radius: 9px; cursor: pointer;
  background: rgba(95, 200, 255, 0.08); color: #5fd2ff; font-size: 13px;
}
.vbtn:hover { border-color: #5fd2ff; box-shadow: 0 0 12px rgba(95, 200, 255, 0.3); }

.slider {
  -webkit-appearance: none; appearance: none; flex: 1; height: 3px; border-radius: 2px;
  background: linear-gradient(90deg, #5fd2ff var(--pct), rgba(95, 200, 255, 0.14) var(--pct));
  outline: none; cursor: pointer;
}
.slider::-webkit-slider-thumb {
  -webkit-appearance: none; appearance: none;
  width: 11px; height: 11px; border-radius: 50%;
  background: #5fd2ff; box-shadow: 0 0 8px rgba(95, 210, 255, 0.8);
}

.vbtn.rate { width: auto; padding: 0 9px; font: 11px/1 ui-monospace, "SF Mono", monospace; }
.vol-slider {
  -webkit-appearance: none; appearance: none; width: 70px; height: 3px; border-radius: 2px; flex: none;
  background: linear-gradient(90deg, #5fd2ff var(--pct), rgba(95, 200, 255, 0.14) var(--pct));
  outline: none; cursor: pointer;
}
.vol-slider::-webkit-slider-thumb {
  -webkit-appearance: none; appearance: none;
  width: 9px; height: 9px; border-radius: 50%;
  background: #5fd2ff; box-shadow: 0 0 6px rgba(95, 210, 255, 0.8);
}
</style>
