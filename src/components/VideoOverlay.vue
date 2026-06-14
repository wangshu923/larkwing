<script setup lang="ts">
// 视频浮层:current.kind === 'video' 时出现。<video> 挂载即向 VM 登记,
// 全屏作用在包壳上(控制条跟着进全屏);混流流无原生 seek,滑杆 = 换 src 重启。
import { computed, onMounted, onUnmounted, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import { registerVideoEl, useMedia } from '../composables/useMedia'
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

const wrap = ref<HTMLElement | null>(null)
const video = ref<HTMLVideoElement | null>(null)
const show = computed(() => state.current?.kind === 'video')

const pct = computed(() =>
  state.duration > 0 ? Math.min(100, (state.position / state.duration) * 100) : 0,
)

function onSeek(e: Event) {
  const v = Number((e.target as HTMLInputElement).value)
  if (state.duration > 0) seek((v / 100) * state.duration)
}

function toggleFullscreen() {
  if (document.fullscreenElement) void document.exitFullscreen()
  else if (wrap.value) void wrap.value.requestFullscreen()
}

function syncFullscreen() {
  state.fullscreen = !!document.fullscreenElement
}

watch(video, (el) => registerVideoEl(el))
onMounted(() => document.addEventListener('fullscreenchange', syncFullscreen))
onUnmounted(() => {
  document.removeEventListener('fullscreenchange', syncFullscreen)
  registerVideoEl(null)
})
</script>

<template>
  <div v-if="show" class="veil">
    <div class="panel" ref="wrap">
      <header class="bar top">
        <span class="title">{{ state.current!.title }}</span>
        <button class="vbtn" @click="stop" :title="t('media.closeVideo')">✕</button>
      </header>
      <video ref="video" class="screen" playsinline></video>
      <footer class="bar bottom">
        <button class="vbtn" @click="toggle">
          {{ state.status === 'playing' ? '⏸' : '▶' }}
        </button>
        <span class="clock">{{ fmtClock(state.position) }} / {{ fmtClock(state.duration) }}</span>
        <input
          class="slider"
          type="range"
          min="0"
          max="100"
          step="0.1"
          :value="pct"
          @input="onSeek"
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
.panel:fullscreen { width: 100%; border-radius: 0; }
.panel:fullscreen .screen { flex: 1; max-height: none; }

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
