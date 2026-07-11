<script setup lang="ts">
// 主窗自绘窗口控件(PLAN §12;2026-07-11 分平台重做,见 AGENT §7.6):
//   Windows/Linux → 右上角三键:最小化 / 最大化⇄还原 / 关闭=缩托盘(无边框补窗控)。
//   macOS         → 本组件整个不渲染,改用原生红绿灯(标准窗 decorations:true,见 tauri.macos.conf.json);
//                   绿灯 = 原生真全屏(进独立 Space),全屏 / 最小化 / 关闭全交 OS、退出口 OS 保证。
// 拖动 / 双击由各页面顶栏(data-tauri-drag-region)承担,不用全局覆盖层——否则会盖住
// 顶部的返回/展开等按钮的点击(二轮真机修复)。三键比初版更小、更靠角。
//
// 「方块」= 最大化而非全屏:铺满工作区但三键永在、随时还原,结构上不困人;沉浸全屏只留给看视频
// (VideoOverlay 影院模式)。⚠️ Windows 无边框窗最大化会盖任务栏(#7103),由 Rust `WM_GETMINMAXINFO`
// hack 把最大化尺寸钉到工作区修(见 `winmax.rs` / §8.1)——前端照常调 toggleMaximize。
import { computed, onMounted, onUnmounted, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import { isMacOS, win } from '../lib/backend'
import { useMedia } from '../composables/useMedia'

const { t } = useI18n()
// 藏三键的唯一场景 = 视频影院全屏(有浮层 ✕⛶ / Esc 退出);纯窗口最大化/全屏时三键永在,不困人。
// 判据用 current.kind==='video' && fullscreen 双条件 —— 只认"真的在看视频且全屏",
// 不再裸看 media.fullscreen(那个会被窗口全屏态污染,是老 bug 的根)。
const { state: media } = useMedia()
const cinema = computed(() => media.current?.kind === 'video' && media.fullscreen)
const maximized = ref(false)
let stop = () => {}

async function sync() {
  maximized.value = await win.isMaximized()
}
onMounted(() => {
  if (isMacOS) return // Mac 用原生红绿灯,本组件不参与(连 resize 订阅都不挂)
  void sync()
  stop = win.onResized(sync) // 最大化 / 还原会触发 resize,同步图标
})
onUnmounted(() => stop())
</script>

<template>
  <div v-if="!isMacOS" class="wc" v-show="!cinema">
    <button class="wcb" :title="t('win.minimize')" @click="win.minimize()">
      <svg viewBox="0 0 12 12"><line x1="2.5" y1="6.5" x2="9.5" y2="6.5" /></svg>
    </button>
    <button
      class="wcb"
      :title="maximized ? t('win.restore') : t('win.maximize')"
      @click="win.toggleMaximize()"
    >
      <!-- 已最大化:双叠方块(还原);未最大化:单方块(最大化) -->
      <svg v-if="maximized" viewBox="0 0 12 12">
        <rect x="2.3" y="4" width="5.4" height="5.4" rx="0.6" />
        <path d="M4.6 4V2.3H9.7V7.4H8" />
      </svg>
      <svg v-else viewBox="0 0 12 12"><rect x="2.6" y="2.6" width="6.8" height="6.8" rx="0.6" /></svg>
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
