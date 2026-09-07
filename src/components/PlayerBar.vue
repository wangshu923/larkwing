<script setup lang="ts">
// 播放条(音频形态;视频走 VideoOverlay):封面 + 走带 + 播放模式 + 曲目列表 + 标题/进度 + 音轨/词/倍速/音量/停止。
// 按钮直连 VM,不绕 LLM。登录建议气泡也长在这排(有提示就出,与是否在放无关)。
// 快捷键与视频浮层共用一张表(useMediaKeys):焦点不在输入框时接管,打字优先;点播放条任意处 = 键盘归
// 播放器(tabindex 可获焦,细描边提示),Esc 把焦点还给输入框。点封面 = 展开「正在播放」大卡
//(大图 + 歌名/歌手/专辑 + 三行歌词 + 走带),再点封面 / ✕ / Esc 收回。
import { computed, nextTick, onMounted, onUnmounted, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import EpisodeList from './EpisodeList.vue'
import KeyHelpCard from './KeyHelpCard.vue'
import { useContextMenu } from '../composables/useContextMenu'
import { useLyrics } from '../composables/useLyrics'
import { useMedia, type PlayMode } from '../composables/useMedia'
import { useMediaKeys, VOL_STEP } from '../composables/useMediaKeys'
import { useScrubHover } from '../composables/useScrubHover'
import { useSettings } from '../composables/useSettings'
import { fmtClock } from '../lib/fmt'

const { t } = useI18n()
const settings = useSettings()
const menu = useContextMenu()
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
  cycleMode,
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
/** 上/下一首:除了「放完就停」,列表都是环 —— 到头回卷,按钮永不禁用。 */
const wraps = computed(() => state.playMode !== 'once')
/** 歌手 · 专辑(有啥显啥;都没有就不占位)。 */
const subline = computed(() => {
  const c = state.current
  return [c?.author, c?.album].filter((s): s is string => !!s).join(' · ')
})

/* —— 播放模式(一个钮三档,§7.1;图标 / 文案跟 state.playMode 走,core 是真相)—— */
const MODE_KEY: Record<PlayMode, string> = {
  once: 'media.mode.once',
  loop_all: 'media.mode.loopAll',
  loop_one: 'media.mode.loopOne',
  shuffle: 'media.mode.shuffle',
}
const modeLabel = (m: PlayMode) => t(MODE_KEY[m])
const modeTitle = computed(() => t('media.modePick', { mode: modeLabel(state.playMode) }))

/* —— 封面:有 cover_url 才是图,加载失败回落 ♪(§3.5 不给破图);换曲复位 —— */
const coverBroken = ref(false)
watch(
  () => state.current?.cover_url,
  () => (coverBroken.value = false),
)
const coverSrc = computed(() => (coverBroken.value ? undefined : state.current?.cover_url))
/** 「正在播放」大卡(点封面开 / 关)。 */
const cardOpen = ref(false)

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
/** 滚歌词(本地音频旁挂 .lrc):有带时间轴的词才出「词」按钮;默认显示、可关(记住)。大卡显三行。 */
const {
  available: lyricsAvailable,
  current: lyricLine,
  around: lyricAround,
} = useLyrics(
  computed(() => state.current?.lyrics),
  computed(() => state.position),
)
const lyricsOn = computed(() => settings.get('ui.lyrics') !== '0')
function toggleLyrics() {
  settings.set('ui.lyrics', lyricsOn.value ? '0' : '1')
}
/** 曲目列表(多首才有):播放条上方的下拉卡,点一行跳到那一首。 */
const listOpen = ref(false)
/** 快捷键速查(H 键)。 */
const helpOpen = ref(false)
// 换成别的形态(停了 / 放视频)时把三块浮层都收掉,别留在下一次播放上
watch(showBar, (s) => {
  if (!s) {
    listOpen.value = false
    helpOpen.value = false
    cardOpen.value = false
  }
})

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

/* —— 快捷键(与视频浮层同一张表,音频面裁剪:N/P 换曲、R 模式、C 歌词;不占 PageUp/Down)——
 * OSD:音频没有画面,读数闪在播放条上方的小药丸里(0.9s 自隐)。 */
const osd = ref<string | null>(null)
let osdTimer = 0
function flashOsd(text: string) {
  osd.value = text
  clearTimeout(osdTimer)
  osdTimer = window.setTimeout(() => (osd.value = null), 900)
}
/** 点播放条任意处 = 键盘归播放器:按钮点完把焦点挪到条本身,免得空格再点一次刚才那个钮。 */
function focusBar() {
  playerEl.value?.focus({ preventScroll: true })
}
/** Esc:先收浮层(帮助 → 列表 → 大卡),都没开 = 放下键盘,把焦点还给输入框。 */
function escape() {
  if (helpOpen.value) helpOpen.value = false
  else if (listOpen.value) listOpen.value = false
  else if (cardOpen.value) cardOpen.value = false
  else {
    ;(document.activeElement as HTMLElement | null)?.blur()
    window.dispatchEvent(new CustomEvent('lw:focus-input'))
  }
}
const keys = useMediaKeys({
  surface: 'audio',
  blocked: () => menu.state.open || state.current?.kind !== 'audio',
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
  cycleMode: () => modeLabel(cycleMode()),
  toggleList: () => (listOpen.value = !listOpen.value),
  audioTrackCount: () => audioTrackCount.value,
  cycleAudioTrack,
  audioTrackLabel: () => audioLabel.value,
  captionCount: () => (lyricsAvailable.value ? 1 : 0),
  cycleCaption: toggleLyrics,
  captionLabel: () => (lyricsOn.value ? t('media.osd.lyricsOn') : t('media.osd.lyricsOff')),
  escape,
  toggleHelp: () => (helpOpen.value = !helpOpen.value),
})
const kb = keys.kb
function onKey(e: KeyboardEvent) {
  keys.onKey(e)
}
onMounted(() => window.addEventListener('keydown', onKey))
onUnmounted(() => {
  window.removeEventListener('keydown', onKey)
  clearTimeout(osdTimer)
})
</script>

<template>
  <div v-if="state.loginHint" class="login-chip">
    <button class="chip" @click="loginNow">{{ t('media.loginChip') }}</button>
    <button class="chip ghost" @click="dismissLoginHint">{{ t('media.loginDismiss') }}</button>
  </div>

  <Transition name="lyrline" mode="out-in">
    <div v-if="showBar && lyricsOn && lyricLine && !cardOpen" :key="lyricLine" class="lyric-line">
      {{ lyricLine }}
    </div>
  </Transition>

  <div v-if="showBar" ref="playerEl" class="player" tabindex="0" @click="focusBar">
    <!-- 封面(有图显图,没图 ♪ 占位);点开「正在播放」大卡 -->
    <button
      class="cover"
      :class="{ on: cardOpen }"
      @click="cardOpen = !cardOpen"
      :title="cardOpen ? t('media.coverClose') : t('media.coverOpen')"
    >
      <img v-if="coverSrc" :src="coverSrc" alt="" decoding="async" @error="coverBroken = true" />
      <span v-else class="note" :class="{ live: state.status === 'playing' }">♪</span>
    </button>
    <button v-if="playlist" class="pbtn" @click="prev" :title="kb(t('media.prevTrack'), 'P')">⏮</button>
    <button
      class="pbtn"
      @click="toggle"
      :title="kb(state.status === 'playing' ? t('media.pause') : t('media.play'), 'Space')"
    >
      {{ state.status === 'playing' ? '⏸' : '▶' }}
    </button>
    <button v-if="playlist" class="pbtn" @click="next" :title="kb(t('media.nextTrack'), 'N')">⏭</button>
    <!-- 播放模式:一个钮三档(歌单:列表循环 → 单曲 → 随机;单曲:放完就停 ↔ 单曲循环)。
         图标走内联 SVG(WindowControls 同款 stroke:currentColor):🔁🔀 这类码位是 emoji 表现形,
         永远渲染彩色、无视 CSS color,与其余单色按钮打架 -->
    <button class="pbtn" :class="{ on: state.playMode !== 'once' }" @click="cycleMode" :title="kb(modeTitle, 'R')">
      <svg v-if="state.playMode === 'once'" viewBox="0 0 24 24">
        <path d="M4 12h13" />
        <path d="m13 8 4 4-4 4" />
        <path d="M20 6v12" />
      </svg>
      <svg v-else-if="state.playMode === 'loop_one'" viewBox="0 0 24 24">
        <path d="M4 12V9a3 3 0 0 1 3-3h13" />
        <path d="m17 3 3 3-3 3" />
        <path d="M20 12v3a3 3 0 0 1-3 3H4" />
        <path d="m7 15-3 3 3 3" />
        <path d="m10.8 10.6 1.6-1.2v5.2" />
      </svg>
      <svg v-else-if="state.playMode === 'shuffle'" viewBox="0 0 24 24">
        <path d="M3 7h4l10 10h4" />
        <path d="M3 17h4l10-10h4" />
        <path d="m18 4 3 3-3 3" />
        <path d="m18 14 3 3-3 3" />
      </svg>
      <svg v-else viewBox="0 0 24 24">
        <path d="M4 12V9a3 3 0 0 1 3-3h13" />
        <path d="m17 3 3 3-3 3" />
        <path d="M20 12v3a3 3 0 0 1-3 3H4" />
        <path d="m7 15-3 3 3 3" />
      </svg>
    </button>
    <button
      v-if="playlist"
      class="pbtn"
      :class="{ on: listOpen }"
      @click="listOpen = !listOpen"
      :title="kb(t('media.trackList'), 'L')"
    >
      <svg viewBox="0 0 24 24">
        <path d="M8 6h13" />
        <path d="M8 12h13" />
        <path d="M8 18h13" />
        <path d="M3 6h.01" />
        <path d="M3 12h.01" />
        <path d="M3 18h.01" />
      </svg>
    </button>
    <div class="mid">
      <div class="title-row">
        <span class="title">{{ state.current!.title }}</span>
        <span v-if="subline" class="author">· {{ subline }}</span>
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
    <button
      v-if="audioTrackCount >= 2"
      class="pbtn track"
      @click="cycleAudioTrack"
      :title="kb(t('media.audioTrack', { label: audioLabel }), 'A')"
    >
      {{ audioLabel }}
    </button>
    <button
      v-if="lyricsAvailable"
      class="pbtn track"
      :class="{ on: lyricsOn }"
      @click="toggleLyrics"
      :title="kb(lyricsOn ? t('media.lyricsHide') : t('media.lyricsShow'), 'C')"
    >
      词
    </button>
    <!-- 倍速(有声书/故事 1.25x 常用):点开档位菜单,滚轮微调;与视频浮层同一张档位表 -->
    <button
      class="pbtn track"
      :class="{ on: state.rate !== 1 }"
      @click="openRateMenu"
      @wheel.prevent="stepRate($event.deltaY < 0 ? 1 : -1)"
      :title="t('media.speedPick', { rate: state.rate })"
    >
      {{ state.rate }}x
    </button>
    <span class="vol" :title="kb(t('media.volume'), '↑↓')">
      <button class="vol-ico" @click="toggleMute" :title="kb(state.muted ? t('media.unmute') : t('media.mute'), 'M')">
        <svg v-if="state.muted || state.volume === 0" viewBox="0 0 24 24">
          <path d="M11 5 6.5 8.5H3.5v7h3L11 19z" />
          <path d="m15.5 9.5 5 5" />
          <path d="m20.5 9.5-5 5" />
        </svg>
        <svg v-else viewBox="0 0 24 24">
          <path d="M11 5 6.5 8.5H3.5v7h3L11 19z" />
          <path d="M15 9.5a3.8 3.8 0 0 1 0 5" />
          <path d="M18 7a7.5 7.5 0 0 1 0 10" />
        </svg>
      </button>
      <input
        class="vol-slider"
        type="range"
        min="0"
        max="100"
        :value="state.muted ? 0 : Math.round(state.volume * 100)"
        @input="onVolume"
        :style="{ '--pct': (state.muted ? 0 : state.volume * 100) + '%' }"
      />
    </span>
    <button class="pbtn stop" @click="stop" :title="t('media.stop')">⏹</button>

    <!-- 按键 OSD:播放条上方闪一下读数(音频没有画面可盖) -->
    <Transition name="osd">
      <div v-if="osd" class="osd" aria-live="polite">{{ osd }}</div>
    </Transition>
    <EpisodeList v-model:open="listOpen" variant="drop" />
    <div v-if="helpOpen" class="help-drop" @pointerdown.stop @click.stop>
      <KeyHelpCard :rows="keys.help.value" variant="drop" />
    </div>
    <!-- 「正在播放」大卡:大封面 + 歌名 / 歌手 · 专辑 + 三行歌词 + 一排走带(点封面 / ✕ / Esc 收) -->
    <div v-if="cardOpen" class="np-card" @pointerdown.stop @click.stop>
      <div class="np-art">
        <img v-if="coverSrc" :src="coverSrc" alt="" decoding="async" @error="coverBroken = true" />
        <span v-else class="np-note">♪</span>
      </div>
      <div class="np-info">
        <div class="np-head">
          <span class="np-kicker">{{ t('media.nowPlaying') }}</span>
          <button class="np-x" @click="cardOpen = false" :title="t('media.coverClose')">✕</button>
        </div>
        <div class="np-title" :title="state.current!.title">{{ state.current!.title }}</div>
        <div v-if="subline" class="np-sub">{{ subline }}</div>
        <div v-if="playlist" class="np-sub dim">
          {{ t('media.trackOf', { cur: playlist.index + 1, total: playlist.total }) }}
        </div>
        <div v-if="lyricsAvailable && lyricsOn" class="np-lyrics">
          <div class="l side">{{ lyricAround.prev }}</div>
          <div class="l cur">{{ lyricAround.cur }}</div>
          <div class="l side">{{ lyricAround.next }}</div>
        </div>
        <div class="np-ctl">
          <button v-if="playlist" class="pbtn" @click="prev" :title="kb(t('media.prevTrack'), 'P')">⏮</button>
          <button class="pbtn big" @click="toggle" :title="kb(state.status === 'playing' ? t('media.pause') : t('media.play'), 'Space')">
            {{ state.status === 'playing' ? '⏸' : '▶' }}
          </button>
          <button v-if="playlist" class="pbtn" @click="next" :title="kb(t('media.nextTrack'), 'N')">⏭</button>
          <span class="np-mode" @click="cycleMode" :title="kb(modeTitle, 'R')">{{ modeLabel(state.playMode) }}</span>
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
.player {
  /* 从 :root 继承科幻 token(原先自带一份 --p-* 副本,已删) */
  position: relative; /* 曲目列表 / 帮助 / 大卡 / OSD 的定位锚 */
  display: flex; align-items: center; gap: 10px;
  padding: 8px 12px; border-radius: 13px;
  background: var(--surface-deep); border: 1px solid var(--line);
  backdrop-filter: blur(6px); -webkit-backdrop-filter: blur(6px);
  outline: none;
  transition: border-color .15s, box-shadow .15s;
}
/* 键盘归播放器的提示(点条 / Tab 进来都算;:focus 而非 :focus-visible —— 鼠标点进来也要看得见) */
.player:focus { border-color: rgba(var(--accent-rgb), 0.55); box-shadow: 0 0 0 1px rgba(var(--accent-rgb), 0.28); }
.pbtn {
  width: 34px; height: 34px; flex: none;
  display: inline-flex; align-items: center; justify-content: center;
  border: 1px solid var(--line); border-radius: 10px; cursor: pointer; font-size: 13px;
  background: rgba(var(--accent-rgb), 0.1); color: var(--accent);
  transition: border-color .15s, background .15s, box-shadow .15s;
}
.pbtn svg, .vol-ico svg {
  fill: none; stroke: currentColor; stroke-width: 2;
  stroke-linecap: round; stroke-linejoin: round;
}
.pbtn svg { width: 15px; height: 15px; }
.pbtn:hover { border-color: var(--accent); box-shadow: 0 0 12px rgba(var(--accent-rgb), 0.3); }
.pbtn:disabled { opacity: .32; cursor: default; border-color: var(--line); box-shadow: none; }
.pbtn.on {
  border-color: var(--accent);
  background: rgba(var(--accent-rgb), 0.22);
  box-shadow: 0 0 10px rgba(var(--accent-rgb), 0.35);
}
.pbtn.track { width: auto; min-width: 34px; padding: 0 8px; font-size: 11px; white-space: nowrap; }
.pbtn.big { width: 40px; height: 40px; font-size: 15px; }
/* 封面:40px 方图(cover-fit),没图时 ♪ 在同一方框里 —— 位置恒定,有没有图不跳版 */
.cover {
  width: 40px; height: 40px; flex: none; padding: 0; overflow: hidden;
  border: 1px solid var(--line); border-radius: 9px; cursor: pointer;
  background: rgba(var(--accent-rgb), 0.08); color: var(--accent);
  display: inline-flex; align-items: center; justify-content: center;
  transition: border-color .15s, box-shadow .15s;
}
.cover img { width: 100%; height: 100%; object-fit: cover; display: block; }
.cover:hover, .cover.on { border-color: var(--accent); box-shadow: 0 0 12px rgba(var(--accent-rgb), 0.3); }
/* 曲目列表:从播放条上方展开(组件自身 right/width 定位,这里只定纵向锚) */
.eplist.drop { bottom: calc(100% + 8px); left: 8px; right: auto; }
.help-drop { position: absolute; z-index: 6; bottom: calc(100% + 8px); right: 8px; display: flex; }
.pbtn.stop { color: var(--attn); border-color: rgba(var(--attn-rgb), 0.35); }
.pbtn.stop:hover { border-color: var(--attn); box-shadow: 0 0 12px rgba(var(--attn-rgb), 0.3); }

.mid { flex: 1; min-width: 0; display: flex; flex-direction: column; gap: 5px; }
.title-row { display: flex; align-items: center; gap: 7px; font-size: 12px; }
.note { color: var(--accent); font-size: 15px; }
.note.live { animation: bounce 1s ease-in-out infinite; }
@keyframes bounce { 0%, 100% { transform: translateY(0); } 50% { transform: translateY(-2px); } }
.title { min-width: 0; color: var(--text); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.author { flex: 1; min-width: 0; color: var(--text-dim); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.title-row .title:not(:has(+ .author)) { flex: 1; }
.ep {
  flex: none; color: var(--accent); font-size: 10.5px; letter-spacing: .3px;
  padding: 1px 7px; border-radius: 999px;
  background: rgba(var(--accent-rgb), 0.12); border: 1px solid rgba(var(--accent-rgb), 0.28);
}
.clock { flex: none; margin-left: auto; color: var(--text-dim); font: 10.5px/1 ui-monospace, "SF Mono", monospace; letter-spacing: .5px; }

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
.vol-ico {
  display: inline-flex; color: var(--accent); opacity: .75; cursor: pointer;
  border: none; background: none; padding: 2px;
}
.vol-ico:hover { opacity: 1; }
.vol-ico svg { width: 14px; height: 14px; }
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

/* 按键 OSD:条上方居中的小药丸(语义 token,随皮肤) */
.osd {
  position: absolute; left: 50%; bottom: calc(100% + 8px); z-index: 7;
  transform: translateX(-50%);
  padding: 5px 13px; border-radius: 999px; pointer-events: none; white-space: nowrap;
  background: var(--surface); color: var(--text);
  border: 1px solid rgba(var(--accent-rgb), 0.35);
  box-shadow: 0 8px 24px rgba(0, 0, 0, 0.25);
  font: 12px/1 ui-monospace, "SF Mono", monospace; letter-spacing: .4px;
}
.osd-enter-active, .osd-leave-active { transition: opacity .18s ease, transform .18s ease; }
.osd-enter-from, .osd-leave-to { opacity: 0; transform: translateX(-50%) translateY(4px); }

/* 「正在播放」大卡:播放条上方铺满一行,大封面在左、信息在右 */
.np-card {
  position: absolute; z-index: 6; left: 0; right: 0; bottom: calc(100% + 8px);
  display: flex; gap: 16px; padding: 14px;
  border-radius: 14px; background: var(--surface); border: 1px solid rgba(var(--accent-rgb), 0.3);
  box-shadow: 0 18px 60px rgba(0, 0, 0, 0.35);
  backdrop-filter: blur(14px); -webkit-backdrop-filter: blur(14px);
}
.np-art {
  flex: none; width: 180px; height: 180px; border-radius: 12px; overflow: hidden;
  background: rgba(var(--accent-rgb), 0.08); border: 1px solid var(--line);
  display: flex; align-items: center; justify-content: center; color: var(--accent);
}
.np-art img { width: 100%; height: 100%; object-fit: cover; display: block; }
.np-note { font-size: 56px; opacity: .8; }
.np-info { flex: 1; min-width: 0; display: flex; flex-direction: column; gap: 6px; }
.np-head { display: flex; align-items: center; justify-content: space-between; }
.np-kicker { font-size: 11px; letter-spacing: .6px; color: var(--accent); }
.np-x {
  width: 24px; height: 24px; border-radius: 7px; cursor: pointer; font-size: 12px;
  border: 1px solid var(--line); background: rgba(var(--accent-rgb), 0.08); color: var(--accent);
}
.np-x:hover { border-color: var(--accent); }
.np-title { font-size: 16px; color: var(--text); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.np-sub { font-size: 12px; color: var(--text-dim); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.np-sub.dim { opacity: .8; }
.np-lyrics { margin-top: 6px; display: flex; flex-direction: column; gap: 4px; min-height: 60px; }
.np-lyrics .l { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-size: 12.5px; color: var(--text-dim); opacity: .6; }
.np-lyrics .l.cur { font-size: 14px; color: var(--accent); opacity: 1; text-shadow: 0 0 12px rgba(var(--accent-rgb), 0.35); }
.np-ctl { margin-top: auto; display: flex; align-items: center; gap: 8px; }
.np-mode {
  margin-left: 4px; padding: 3px 10px; border-radius: 999px; cursor: pointer; font-size: 11px;
  color: var(--accent); background: rgba(var(--accent-rgb), 0.1); border: 1px solid rgba(var(--accent-rgb), 0.3);
}
.np-mode:hover { border-color: var(--accent); }

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
