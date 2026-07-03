<script setup lang="ts">
// 皮肤化下拉:替代原生 <select> —— 原生 <option> 弹层是 OS/Chromium 渲染,只认不透明底色、
// 高亮行等控不到(§6.7 记债)。这里用普通元素自绘列表,全走语义 token、跟随换肤,弹层样式
// 与 ContextMenu 同款(var(--surface) + backdrop-blur)。键盘可达 + 点外关闭。
// 值一律按字符串比较(调用方 number/string 混用时用 String() 归一)。
import { computed, onBeforeUnmount, ref } from 'vue'

const props = defineProps<{
  modelValue: string | number | null | undefined
  options: { value: string; label: string }[]
  disabled?: boolean
  ariaLabel?: string
}>()
const emit = defineEmits<{ 'update:modelValue': [string] }>()

const root = ref<HTMLElement | null>(null)
const open = ref(false)
const active = ref(-1) // 键盘高亮项下标
const val = computed(() => String(props.modelValue ?? ''))
const curLabel = computed(
  () => props.options.find((o) => o.value === val.value)?.label ?? props.options[0]?.label ?? '',
)

function onDocDown(e: PointerEvent) {
  if (root.value && !root.value.contains(e.target as Node)) close()
}
function openList() {
  if (props.disabled) return
  open.value = true
  active.value = Math.max(0, props.options.findIndex((o) => o.value === val.value))
  document.addEventListener('pointerdown', onDocDown, true)
}
function close() {
  open.value = false
  document.removeEventListener('pointerdown', onDocDown, true)
}
function toggle() {
  open.value ? close() : openList()
}
function pick(v: string) {
  emit('update:modelValue', v)
  close()
}
function onKey(e: KeyboardEvent) {
  if (props.disabled) return
  if (!open.value) {
    if (e.key === 'Enter' || e.key === ' ' || e.key === 'ArrowDown') {
      e.preventDefault()
      openList()
    }
    return
  }
  if (e.key === 'Escape') {
    e.preventDefault()
    close()
  } else if (e.key === 'ArrowDown') {
    e.preventDefault()
    active.value = Math.min(props.options.length - 1, active.value + 1)
  } else if (e.key === 'ArrowUp') {
    e.preventDefault()
    active.value = Math.max(0, active.value - 1)
  } else if (e.key === 'Enter' || e.key === ' ') {
    e.preventDefault()
    const o = props.options[active.value]
    if (o) pick(o.value)
  }
}
onBeforeUnmount(() => document.removeEventListener('pointerdown', onDocDown, true))
</script>

<template>
  <div ref="root" class="skinsel" :class="{ open, disabled }" @keydown="onKey">
    <button
      type="button"
      class="skinsel-btn s-input"
      role="combobox"
      :aria-expanded="open"
      :aria-label="ariaLabel"
      :disabled="disabled"
      @click="toggle"
    >
      <span class="skinsel-cur">{{ curLabel }}</span>
      <span class="skinsel-arrow">▾</span>
    </button>
    <ul v-if="open" class="skinsel-list" role="listbox">
      <li
        v-for="(o, i) in options"
        :key="o.value"
        role="option"
        :aria-selected="o.value === val"
        :class="{ sel: o.value === val, active: i === active }"
        @click="pick(o.value)"
        @mousemove="active = i"
      >
        {{ o.label }}
      </li>
    </ul>
  </div>
</template>

<style scoped>
.skinsel { position: relative; display: inline-block; }
.skinsel-btn {
  display: inline-flex; align-items: center; justify-content: space-between; gap: 8px;
  width: 100%; cursor: pointer; text-align: left;
}
.skinsel-btn:disabled { opacity: 0.5; cursor: default; }
.skinsel-cur { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.skinsel-arrow { color: var(--text-dim); font-size: 11px; flex: 0 0 auto; }
.skinsel.open .skinsel-arrow { color: var(--accent); }
/* 弹层:与 ContextMenu 同款玻璃(translucent surface + blur → 隔行也读得清),全语义 token */
.skinsel-list {
  position: absolute; z-index: 40; top: calc(100% + 4px); left: 0; right: 0;
  margin: 0; padding: 4px; list-style: none;
  background: var(--surface); border: 1px solid var(--line); border-radius: 10px;
  backdrop-filter: blur(14px); -webkit-backdrop-filter: blur(14px);
  box-shadow: 0 12px 30px rgba(0, 0, 0, 0.4);
  max-height: 260px; overflow-y: auto; scrollbar-gutter: stable;
}
.skinsel-list li {
  padding: 7px 10px; border-radius: 7px; color: var(--text); font-size: 13px;
  cursor: pointer; white-space: nowrap; overflow: hidden; text-overflow: ellipsis;
  transition: background 0.12s, color 0.12s;
}
.skinsel-list li.active { background: rgba(var(--accent-rgb), 0.14); }
.skinsel-list li.sel { color: var(--accent); }
.skinsel-list li.sel::after { content: '✓'; float: right; margin-left: 10px; }
</style>
