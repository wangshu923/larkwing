<script setup lang="ts">
// 主窗自绘窗口控件(PLAN §12):无边框 → 右上角三键(最小化 / 全屏⇄退出 / 关闭=缩托盘)。
// 拖动 / 双击由各页面顶栏(data-tauri-drag-region)承担,不用全局覆盖层——否则会盖住
// 顶部的返回/展开等按钮的点击(二轮真机修复)。三键比初版更小、更靠角。
import { onMounted, onUnmounted, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import { win } from '../lib/backend'
import { useMedia } from '../composables/useMedia'

const { t } = useI18n()
// 视频全屏时藏三键(影院视图;退出靠 Esc / 浮层 ✕⛶)。只看 media.fullscreen:
// 手动整窗全屏(无视频)时 media.fullscreen 为 false,三键仍在,用户能点退出。
const { state: media } = useMedia()
const fullscreen = ref(false)
let stop = () => {}

async function sync() {
  fullscreen.value = await win.isFullscreen()
}
onMounted(() => {
  void sync()
  stop = win.onResized(sync) // 全屏切换会触发 resize
})
onUnmounted(() => stop())
</script>

<template>
  <div class="wc" v-show="!media.fullscreen">
    <button class="wcb" :title="t('win.minimize')" @click="win.minimize()">
      <svg viewBox="0 0 12 12"><line x1="2.5" y1="6.5" x2="9.5" y2="6.5" /></svg>
    </button>
    <button class="wcb" :title="t('win.maximize')" @click="win.toggleFullscreen()">
      <!-- 已全屏:内收四角(退出);未全屏:外扩四角(进全屏) -->
      <svg v-if="fullscreen" viewBox="0 0 12 12"><path d="M5 2v3H2M7 2v3h3M5 10V7H2M7 10V7h3" /></svg>
      <svg v-else viewBox="0 0 12 12"><path d="M2 4.5V2h2.5M10 4.5V2H7.5M2 7.5V10h2.5M10 7.5V10H7.5" /></svg>
    </button>
    <button class="wcb close" :title="t('win.close')" @click="win.hideToTray()">
      <svg viewBox="0 0 12 12"><line x1="3" y1="3" x2="9" y2="9" /><line x1="9" y1="3" x2="3" y2="9" /></svg>
    </button>
  </div>
</template>

<style scoped>
.wc {
  position: fixed;
  top: 6px;
  right: 8px;
  z-index: 61;
  display: flex;
  gap: 2px;
}
.wcb {
  width: 22px;
  height: 18px;
  display: flex;
  align-items: center;
  justify-content: center;
  background: rgba(var(--accent-rgb), 0.05);
  border: 1px solid rgba(var(--accent-rgb), 0.16);
  border-radius: 6px;
  color: var(--text-dim);
  cursor: pointer;
  transition: color 0.15s, border-color 0.15s, background 0.15s;
}
.wcb svg {
  width: 11px;
  height: 11px;
  fill: none;
  stroke: currentColor;
  stroke-width: 1.3;
  stroke-linecap: round;
  stroke-linejoin: round;
}
.wcb:hover {
  color: var(--accent);
  border-color: rgba(var(--accent-rgb), 0.45);
  background: rgba(var(--accent-rgb), 0.14);
}
.wcb.close:hover {
  color: var(--attn);
  border-color: rgba(var(--attn-rgb), 0.5);
  background: rgba(var(--attn-rgb), 0.14);
}
</style>
