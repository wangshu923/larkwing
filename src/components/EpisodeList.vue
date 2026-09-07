<script setup lang="ts">
// 剧集 / 曲目列表:多集内容的「选集」面板。开着时向 core 按需取整份队列(不塞进每条 Play 事件),
// 当前集高亮并滚到可见;点一行 = 跳到那一集(与嘴控「看第五集」同一 core 入口)。
// 键盘:↑↓ 挑、Enter 跳、Esc 关(事件在面板内截住,不冒泡给视频浮层的快捷键表)。
// 视频浮层与音频条共用;观感由 `variant` 定(全屏侧栏 / 窗口态下拉),颜色只用语义 token(§6.7),
// 视频上盖的那份走「覆盖媒体豁免」恒亮浅字(同控制条)。
import { computed, nextTick, onUnmounted, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import { useMedia } from '../composables/useMedia'
import type { PlaylistView } from '../lib/backend'

const props = defineProps<{
  /** side = 全屏影院右侧滑出;drop = 窗口态 / 音频条上方的下拉卡。 */
  variant: 'side' | 'drop'
}>()
const open = defineModel<boolean>('open', { default: false })

const { t } = useI18n()
const { state, fetchPlaylist, jumpTo } = useMedia()

const view = ref<PlaylistView | null>(null)
const active = ref(0) // 键盘游标(0 起)
const listEl = ref<HTMLElement | null>(null)
const isAudio = computed(() => state.current?.kind === 'audio')
const heading = computed(() => {
  const name = view.value?.title
  const base = t(isAudio.value ? 'media.trackList' : 'media.episodeList')
  return name ? `${base} · ${name}` : base
})

async function refresh() {
  view.value = await fetchPlaylist()
  active.value = view.value?.index ?? 0
  await nextTick()
  scrollActiveIntoView()
}
function scrollActiveIntoView() {
  const li = listEl.value?.querySelector<HTMLElement>('li.cur') ?? listEl.value?.querySelector<HTMLElement>('li.act')
  li?.scrollIntoView({ block: 'nearest' })
}
// 打开时取一次;开着时切了集(Play 事件换了 current)再取一次对齐高亮
watch(open, (o) => {
  if (o) void refresh()
})
watch(
  () => state.current?.playlist?.index,
  () => {
    if (open.value) void refresh()
  },
)

function pick(i: number) {
  jumpTo(i + 1)
  open.value = false
}
function onKey(e: KeyboardEvent) {
  const n = view.value?.entries.length ?? 0
  if (e.key === 'Escape') {
    open.value = false
  } else if (e.key === 'ArrowDown' && n) {
    active.value = Math.min(n - 1, active.value + 1)
  } else if (e.key === 'ArrowUp' && n) {
    active.value = Math.max(0, active.value - 1)
  } else if (e.key === 'Enter' && n) {
    pick(active.value)
  } else {
    return
  }
  e.preventDefault()
  e.stopPropagation()
  void nextTick(scrollActiveIntoView)
}
// 开着时在 window 捕获阶段接键(视频浮层的快捷键表也挂在 window,捕获阶段先到手、stopPropagation 截住)
watch(open, (o) => {
  if (o) window.addEventListener('keydown', onKey, true)
  else window.removeEventListener('keydown', onKey, true)
})
onUnmounted(() => window.removeEventListener('keydown', onKey, true))
</script>

<template>
  <div v-if="open" class="eplist" :class="variant" @pointerdown.stop @click.stop>
    <header>
      <span class="hd">{{ heading }}</span>
      <span v-if="view" class="cnt">{{ view.index + 1 }}/{{ view.entries.length }}</span>
      <button class="x" @click="open = false" aria-label="close">✕</button>
    </header>
    <ul ref="listEl" role="listbox">
      <li
        v-for="(e, i) in view?.entries ?? []"
        :key="i"
        role="option"
        :class="{ cur: i === view?.index, act: i === active }"
        :aria-selected="i === view?.index"
        @click="pick(i)"
        @mouseenter="active = i"
      >
        <span class="no">{{ i + 1 }}</span>
        <span class="tt">{{ e.title }}</span>
      </li>
    </ul>
  </div>
</template>

<style scoped>
.eplist {
  position: absolute; z-index: 6;
  display: flex; flex-direction: column;
  color: #eaf2fb; /* 覆盖在视频 / 深色播放条上:恒亮浅字(同控制条的媒体豁免) */
  background: rgba(0, 0, 0, 0.82);
  border: 1px solid rgba(var(--accent-rgb), 0.3);
  box-shadow: 0 18px 60px rgba(0, 0, 0, 0.55);
  backdrop-filter: blur(14px); -webkit-backdrop-filter: blur(14px);
}
/* 全屏影院:右侧滑出的侧栏,上下留空不压住两条控制条 */
.eplist.side { top: 56px; right: 16px; bottom: 76px; width: min(340px, 40%); border-radius: 14px; }
/* 窗口态 / 音频条:锚在触发钮附近的下拉卡(由父组件定位 top/bottom) */
.eplist.drop { right: 8px; width: min(320px, 92%); max-height: 320px; border-radius: 12px; }
header {
  display: flex; align-items: center; gap: 8px; flex: none;
  padding: 10px 12px 8px; font-size: 12.5px; letter-spacing: 0.4px;
  border-bottom: 1px solid rgba(var(--accent-rgb), 0.18);
}
.hd { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; color: var(--accent); }
.cnt { flex: none; font: 11px/1 ui-monospace, "SF Mono", monospace; color: rgba(234, 242, 251, 0.7); }
.x {
  flex: none; width: 24px; height: 24px; border-radius: 7px; cursor: pointer;
  border: 1px solid rgba(var(--accent-rgb), 0.18); background: rgba(var(--accent-rgb), 0.08); color: var(--accent);
  font-size: 12px;
}
ul { list-style: none; margin: 0; padding: 6px; overflow: auto; flex: 1; min-height: 0; scrollbar-gutter: stable; }
li {
  display: flex; align-items: center; gap: 10px; padding: 7px 9px; border-radius: 8px; cursor: pointer;
  font-size: 12.5px; line-height: 1.3;
}
li .no { flex: none; min-width: 26px; text-align: right; font: 11px/1 ui-monospace, "SF Mono", monospace; color: rgba(234, 242, 251, 0.6); }
li .tt { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
li.act { background: rgba(var(--accent-rgb), 0.12); }
li.cur { color: var(--accent); }
li.cur .no { color: var(--accent); }
li.cur::after { content: '▶'; font-size: 9px; color: var(--accent); }
</style>
