<script setup lang="ts">
// 右键菜单宿主:App.vue 顶层挂一个,主窗 / 悬浮窗各自的 WebView 都有。
// 弹出后量尺寸做视口夹紧(贴右/下边自动回收);点外部 / Esc / 滚动 / 缩放 / 失焦即关。
// 科幻皮:只用语义 token,毛玻璃 + 描边(同任务卡 / 悬浮窗面板气质)。
import { nextTick, onMounted, onUnmounted, ref, watch } from 'vue'
import { useContextMenu, type MenuItem } from '../composables/useContextMenu'

const { state, closeMenu } = useContextMenu()
const menuEl = ref<HTMLElement | null>(null)

// 冒出后量真实尺寸,超出视口右/下边就回收(光标点在屏幕边角也不溢出)
watch(
  () => state.open,
  (open) => {
    if (!open) return
    nextTick(() => {
      const el = menuEl.value
      if (!el) return
      const r = el.getBoundingClientRect()
      const vw = window.innerWidth
      const vh = window.innerHeight
      if (state.x + r.width > vw - 8) state.x = Math.max(8, vw - r.width - 8)
      if (state.y + r.height > vh - 8) state.y = Math.max(8, vh - r.height - 8)
    })
  },
)

function onItem(item: MenuItem) {
  if (item.disabled || item.separator) return
  closeMenu() // 先关再执行:动作里可能再弹菜单 / 改 DOM
  item.action?.()
}

// 点菜单之外(capture,抢在目标 click 前)、Esc、滚动 / 缩放 / 失焦 —— 一律关闭
function onWinDown(e: MouseEvent) {
  if (menuEl.value && !menuEl.value.contains(e.target as Node)) closeMenu()
}
function onKey(e: KeyboardEvent) {
  if (e.key === 'Escape') closeMenu()
}
onMounted(() => {
  window.addEventListener('mousedown', onWinDown, true)
  window.addEventListener('keydown', onKey)
  window.addEventListener('scroll', closeMenu, true)
  window.addEventListener('resize', closeMenu)
  window.addEventListener('blur', closeMenu)
})
onUnmounted(() => {
  window.removeEventListener('mousedown', onWinDown, true)
  window.removeEventListener('keydown', onKey)
  window.removeEventListener('scroll', closeMenu, true)
  window.removeEventListener('resize', closeMenu)
  window.removeEventListener('blur', closeMenu)
})
</script>

<template>
  <Teleport to="body">
    <div
      v-if="state.open"
      ref="menuEl"
      class="ctx"
      :style="{ left: state.x + 'px', top: state.y + 'px' }"
      @contextmenu.prevent
    >
      <template v-for="(item, i) in state.items" :key="i">
        <div v-if="item.separator" class="ctx-sep"></div>
        <button
          v-else
          class="ctx-item"
          :class="{ danger: item.danger, disabled: item.disabled }"
          :disabled="item.disabled"
          @click="onItem(item)"
        >
          {{ item.label }}
        </button>
      </template>
    </div>
  </Teleport>
</template>

<style scoped>
.ctx {
  position: fixed;
  z-index: 9999;
  min-width: 144px;
  padding: 5px;
  border-radius: 11px;
  background: var(--surface);
  border: 1px solid var(--line);
  backdrop-filter: blur(14px);
  -webkit-backdrop-filter: blur(14px);
  box-shadow: 0 12px 34px rgba(0, 0, 0, 0.42);
  display: flex;
  flex-direction: column;
  gap: 1px;
  user-select: none;
  animation: ctxIn 0.12s ease;
}
@keyframes ctxIn {
  from {
    opacity: 0;
    transform: translateY(-4px) scale(0.98);
  }
}
.ctx-item {
  text-align: left;
  background: none;
  border: none;
  cursor: pointer;
  color: var(--text);
  font-family: inherit;
  font-size: 13px;
  line-height: 1.2;
  padding: 8px 12px;
  border-radius: 7px;
  white-space: nowrap;
  transition: background 0.12s, color 0.12s;
}
.ctx-item:hover:not(:disabled) {
  background: rgba(var(--accent-rgb), 0.14);
  color: var(--accent);
}
.ctx-item.danger {
  color: var(--danger);
}
.ctx-item.danger:hover:not(:disabled) {
  background: rgba(var(--danger-rgb), 0.14);
  color: var(--danger);
}
.ctx-item:disabled {
  opacity: 0.4;
  cursor: default;
}
.ctx-sep {
  height: 1px;
  margin: 4px 8px;
  background: var(--line);
}
</style>
