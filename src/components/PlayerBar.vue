<script setup lang="ts">
// 播放条(音频形态;视频走 VideoOverlay):标题 + 播放/暂停 + 进度 + 停止。
// 按钮直连 VM,不绕 LLM。登录建议气泡也长在这排(有提示就出,与是否在放无关)。
import { computed, nextTick, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import { useLyrics } from '../composables/useLyrics'
import { useMedia } from '../composables/useMedia'
import { useScrubHover } from '../composables/useScrubHover'
import { useSettings } from '../composables/useSettings'
import { fmtClock } from '../lib/fmt'

const { t } = useI18n()
const settings = useSettings()
const {
  state,
  toggle,
  stop,
  seek,
  setVolume,
  next,
  prev,
  cycleLoop,
  toggleShuffle,
  cycleAudioTrack,
  audioTrackLabel,
  loginNow,
  dismissLoginHint,
} = useMedia()

const showBar = computed(() => state.current?.kind === 'audio')
/** 多集音频(评书/儿歌合集等)才出集数 + 上/下一首。 */
const playlist = computed(() => state.current?.playlist ?? null)
/** ≥2 条音轨才出切换钮(有声书双语版这类;label = 当前轨友好名)。 */
const audioTrackCount = computed(() => state.current?.audio_tracks?.length ?? 0)
const audioLabel = computed(() => audioTrackLabel(state.current?.audio_track ?? 0))
/** 上/下一首在「随机」或「列表循环」时永不禁用(随机恒有下一首;循环到头回卷)。 */
const freeMove = computed(() => state.shuffle || state.loopMode === 'all')
const loopTitle = computed(() =>
  state.loopMode === 'one'
    ? t('media.loopOne')
    : state.loopMode === 'all'
      ? t('media.loopAll')
      : t('media.loopOff'),
)
// 进度条:拖动中只动视觉(scrub),不被 timeupdate 抢拇指;松手(change)才真 seek 一次
// —— 与 VideoOverlay 同款。原先 @input 每 tick 就 seek:拖有声书就是一串 currentTime 风暴,
// 而且读数被真实播放位盖住、跟不上光标。
const dragging = ref(false)
const scrub = ref(0)
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

/** 光标处是第几分几秒(音频没有画面,只出时间;缩略图那半边是视频的事)。 */
const durationRef = computed(() => state.duration)
const playerEl = ref<HTMLElement | null>(null)
const { trackEl, hoverPct, hoverTime, bubbleLeft, bubbleW, onMove, onLeave } = useScrubHover(
  durationRef,
  { thumbWidth: 10, clampTo: playerEl },
)
const bubbleEl = ref<HTMLElement | null>(null)
// nextTick(不是 rAF):要的是「DOM 更新完」,rAF 在窗口隐藏时压根不触发(§8.1)
watch(hoverPct, async () => {
  await nextTick()
  bubbleW.value = bubbleEl.value?.offsetWidth ?? 0
})
/** 滚歌词(本地音频旁挂 .lrc):有带时间轴的词才出「词」按钮;默认显示、可关(记住)。 */
const { available: lyricsAvailable, current: lyricLine } = useLyrics(
  computed(() => state.current?.lyrics),
  computed(() => state.position),
)
const lyricsOn = computed(() => settings.get('ui.lyrics') !== '0')
function toggleLyrics() {
  settings.set('ui.lyrics', lyricsOn.value ? '0' : '1')
}

function onScrubInput(e: Event) {
  dragging.value = true
  scrub.value = Number((e.target as HTMLInputElement).value)
}
function onScrubCommit(e: Event) {
  const v = Number((e.target as HTMLInputElement).value)
  dragging.value = false
  if (state.duration > 0) seek((v / 100) * state.duration)
}

function onVolume(e: Event) {
  setVolume(Number((e.target as HTMLInputElement).value) / 100)
}
</script>

<template>
  <div v-if="state.loginHint" class="login-chip">
    <button class="chip" @click="loginNow">{{ t('media.loginChip') }}</button>
    <button class="chip ghost" @click="dismissLoginHint">{{ t('media.loginDismiss') }}</button>
  </div>

  <Transition name="lyrline" mode="out-in">
    <div v-if="showBar && lyricsOn && lyricLine" :key="lyricLine" class="lyric-line">
      {{ lyricLine }}
    </div>
  </Transition>

  <div v-if="showBar" ref="playerEl" class="player">
    <button
      v-if="playlist"
      class="pbtn"
      @click="prev"
      :disabled="!freeMove && playlist.index <= 0"
      :title="t('media.prevTrack')"
    >
      ⏮
    </button>
    <button
      class="pbtn"
      @click="toggle"
      :title="state.status === 'playing' ? t('media.pause') : t('media.play')"
    >
      {{ state.status === 'playing' ? '⏸' : '▶' }}
    </button>
    <button
      v-if="playlist"
      class="pbtn"
      @click="next"
      :disabled="!freeMove && playlist.index >= playlist.total - 1"
      :title="t('media.nextTrack')"
    >
      ⏭
    </button>
    <button class="pbtn" :class="{ on: state.loopMode !== 'off' }" @click="cycleLoop" :title="loopTitle">
      {{ state.loopMode === 'one' ? '🔂' : '🔁' }}
    </button>
    <button
      v-if="playlist"
      class="pbtn"
      :class="{ on: state.shuffle }"
      @click="toggleShuffle"
      :title="state.shuffle ? t('media.shuffleOn') : t('media.shuffleOff')"
    >
      🔀
    </button>
    <button
      v-if="audioTrackCount >= 2"
      class="pbtn track"
      @click="cycleAudioTrack"
      :title="t('media.audioTrack', { label: audioLabel })"
    >
      {{ audioLabel }}
    </button>
    <button
      v-if="lyricsAvailable"
      class="pbtn track"
      :class="{ on: lyricsOn }"
      @click="toggleLyrics"
      :title="lyricsOn ? t('media.lyricsHide') : t('media.lyricsShow')"
    >
      词
    </button>
    <div class="mid">
      <div class="title-row">
        <span class="note" :class="{ live: state.status === 'playing' }">♪</span>
        <span class="title">{{ state.current!.title }}</span>
        <span v-if="playlist" class="ep">{{
          t('media.trackOf', { cur: playlist.index + 1, total: playlist.total })
        }}</span>
        <span class="clock">{{ fmtClock(displayPos) }} / {{ fmtClock(state.duration) }}</span>
      </div>
      <!-- 套一层 hover 检测:光标所在处是第几分几秒(按下之前就知道会跳到哪) -->
      <div
        ref="trackEl"
        class="scrub-track"
        @pointermove="onMove"
        @pointerleave="onLeave"
        @pointercancel="onLeave"
      >
        <div
          v-if="hoverPct !== null"
          ref="bubbleEl"
          class="hover-bubble"
          :style="{ left: bubbleLeft + 'px' }"
        >
          {{ fmtClock(hoverTime ?? 0) }}
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
    </div>
    <span class="vol" :title="t('media.volume')">
      <span class="vol-ico">{{ state.volume === 0 ? '🔇' : '🔊' }}</span>
      <input
        class="vol-slider"
        type="range"
        min="0"
        max="100"
        :value="Math.round(state.volume * 100)"
        @input="onVolume"
        :style="{ '--pct': state.volume * 100 + '%' }"
      />
    </span>
    <button class="pbtn stop" @click="stop" :title="t('media.stop')">⏹</button>
  </div>
</template>

<style scoped>
.player {
  /* 从 :root 继承科幻 token(原先自带一份 --p-* 副本,已删) */
  display: flex; align-items: center; gap: 10px;
  padding: 8px 12px; border-radius: 13px;
  background: var(--surface-deep); border: 1px solid var(--line);
  backdrop-filter: blur(6px); -webkit-backdrop-filter: blur(6px);
}
.pbtn {
  width: 34px; height: 34px; flex: none;
  border: 1px solid var(--line); border-radius: 10px; cursor: pointer; font-size: 13px;
  background: rgba(var(--accent-rgb), 0.1); color: var(--accent);
  transition: border-color .15s, background .15s, box-shadow .15s;
}
.pbtn:hover { border-color: var(--accent); box-shadow: 0 0 12px rgba(var(--accent-rgb), 0.3); }
.pbtn:disabled { opacity: .32; cursor: default; border-color: var(--line); box-shadow: none; }
.pbtn.on {
  border-color: var(--accent);
  background: rgba(var(--accent-rgb), 0.22);
  box-shadow: 0 0 10px rgba(var(--accent-rgb), 0.35);
}
.pbtn.track { width: auto; min-width: 34px; padding: 0 8px; font-size: 11px; white-space: nowrap; }
.pbtn.stop { color: var(--attn); border-color: rgba(var(--attn-rgb), 0.35); }
.pbtn.stop:hover { border-color: var(--attn); box-shadow: 0 0 12px rgba(var(--attn-rgb), 0.3); }

.mid { flex: 1; min-width: 0; display: flex; flex-direction: column; gap: 5px; }
.title-row { display: flex; align-items: center; gap: 7px; font-size: 12px; }
.note { color: var(--accent); }
.note.live { animation: bounce 1s ease-in-out infinite; }
@keyframes bounce { 0%, 100% { transform: translateY(0); } 50% { transform: translateY(-2px); } }
.title { flex: 1; min-width: 0; color: var(--text); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.ep {
  flex: none; color: var(--accent); font-size: 10.5px; letter-spacing: .3px;
  padding: 1px 7px; border-radius: 999px;
  background: rgba(var(--accent-rgb), 0.12); border: 1px solid rgba(var(--accent-rgb), 0.28);
}
.clock { color: var(--text-dim); font: 10.5px/1 ui-monospace, "SF Mono", monospace; letter-spacing: .5px; }

/* 进度条的定位容器:滑杆照旧撑满,hover 时间气泡绝对定位挂上方(不占位、不吃指针)。
   这条不压在视频画面上 → 用语义 token,跟着皮肤走(§6.7)。 */
/* 进度条定位容器(名字别用 .track —— 与音轨/「词」按钮的 .pbtn.track 撞车)。
   清掉 range 的 UA 默认 margin:2px:否则容器宽 ≠ 真实轨道宽,hover 秒数与拇指差几像素 */
.scrub-track { position: relative; width: 100%; display: flex; align-items: center; }
.scrub-track .slider { margin: 0; }
/* bottom 得抬到 30px:播放条只有一行高,13px 会让气泡正压在标题/时钟那行上(预览量过:
   气泡 522–541 vs 标题行 527–546)。抬到条外反而干净,不遮任何内容。 */
.hover-bubble {
  position: absolute; bottom: 30px; z-index: 2;
  transform: translateX(-50%); pointer-events: none; white-space: nowrap;
  padding: 3px 7px; border-radius: 7px;
  font: 10.5px/1 ui-monospace, "SF Mono", monospace; letter-spacing: .5px;
  color: var(--text); background: var(--surface-deep);
  border: 1px solid rgba(var(--accent-rgb), 0.3);
  backdrop-filter: blur(6px); -webkit-backdrop-filter: blur(6px);
}

.slider {
  -webkit-appearance: none; appearance: none; width: 100%; height: 3px; border-radius: 2px;
  background: linear-gradient(90deg, var(--accent) var(--pct), rgba(var(--accent-rgb), 0.14) var(--pct));
  outline: none; cursor: pointer;
}
.slider::-webkit-slider-thumb {
  -webkit-appearance: none; appearance: none;
  width: 10px; height: 10px; border-radius: 50%;
  background: var(--accent); box-shadow: 0 0 8px rgba(var(--accent-rgb), 0.8);
}

.vol { display: inline-flex; align-items: center; gap: 5px; flex: none; }
.vol-ico { font-size: 11px; opacity: .75; }
.vol-slider {
  -webkit-appearance: none; appearance: none; width: 64px; height: 3px; border-radius: 2px;
  background: linear-gradient(90deg, var(--accent) var(--pct), rgba(var(--accent-rgb), 0.14) var(--pct));
  outline: none; cursor: pointer;
}
.vol-slider::-webkit-slider-thumb {
  -webkit-appearance: none; appearance: none;
  width: 9px; height: 9px; border-radius: 50%;
  background: var(--accent); box-shadow: 0 0 6px rgba(var(--accent-rgb), 0.8);
}

.lyric-line {
  text-align: center; font-size: 13px; color: var(--accent);
  padding: 2px 12px 7px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
  text-shadow: 0 0 12px rgba(var(--accent-rgb), 0.35);
}
.lyrline-enter-active, .lyrline-leave-active { transition: opacity .18s, transform .18s; }
.lyrline-enter-from { opacity: 0; transform: translateY(6px); }
.lyrline-leave-to { opacity: 0; transform: translateY(-6px); }

.login-chip { display: flex; gap: 8px; }
.chip {
  padding: 6px 13px; border-radius: 999px; cursor: pointer; font-size: 12px;
  background: rgba(var(--accent-rgb), 0.1); border: 1px solid rgba(var(--accent-rgb), 0.35);
  color: var(--accent);
  transition: border-color .15s, box-shadow .15s;
}
.chip:hover { border-color: var(--accent); box-shadow: 0 0 12px rgba(var(--accent-rgb), 0.3); }
.chip.ghost { background: none; border-color: var(--line); color: var(--text-dim); }
.chip.ghost:hover { border-color: var(--text-dim); box-shadow: none; }
</style>
