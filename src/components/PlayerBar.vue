<script setup lang="ts">
// 播放条(音频形态;视频走 VideoOverlay):标题 + 播放/暂停 + 进度 + 停止。
// 按钮直连 VM,不绕 LLM。登录建议气泡也长在这排(有提示就出,与是否在放无关)。
import { computed } from 'vue'
import { useI18n } from 'vue-i18n'
import { useMedia } from '../composables/useMedia'
import { fmtClock } from '../lib/fmt'

const { t } = useI18n()
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
const pct = computed(() =>
  state.duration > 0 ? Math.min(100, (state.position / state.duration) * 100) : 0,
)

function onSeek(e: Event) {
  const v = Number((e.target as HTMLInputElement).value)
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

  <div v-if="showBar" class="player">
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
    <div class="mid">
      <div class="title-row">
        <span class="note" :class="{ live: state.status === 'playing' }">♪</span>
        <span class="title">{{ state.current!.title }}</span>
        <span v-if="playlist" class="ep">{{
          t('media.trackOf', { cur: playlist.index + 1, total: playlist.total })
        }}</span>
        <span class="clock">{{ fmtClock(state.position) }} / {{ fmtClock(state.duration) }}</span>
      </div>
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
