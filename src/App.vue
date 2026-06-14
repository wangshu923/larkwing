<script setup lang="ts">
import { computed, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import NeonBackdrop from './components/NeonBackdrop.vue'
import MainLayout from './components/MainLayout.vue'
import TasksOverlay from './components/TasksOverlay.vue'
import VideoOverlay from './components/VideoOverlay.vue'
import WindowControls from './components/WindowControls.vue'
import FloatWindow from './components/FloatWindow.vue'
import { useBoot } from './composables/useBoot'
import { useChat } from './composables/useChat'
import { useSettings } from './composables/useSettings'
import {
  api,
  isTauri,
  isWindowFocused,
  onOpenConversation,
  onWindowFocus,
  setFloatVisible,
  windowLabel,
} from './lib/backend'

const { t } = useI18n()

// 窗口分流(PLAN §12):float 标签 = 悬浮窗(独立 WebView),否则主窗全套。
const isFloat = windowLabel() === 'float'

// 启动编排(仅主窗):phase = 'boot' → 'ready';背景与主界面各自订阅它做入场。
const { phase, run, skip } = useBoot(1800)
if (!isFloat) run()
const booting = computed(() => !isFloat && phase.value === 'boot')

// 主窗专属编排(PLAN §12):托盘菜单文案 + 悬浮窗显隐(enabled × 主窗是否在前)+ 通知跳会话。
if (!isFloat && isTauri()) {
  const settings = useSettings()
  void api.setTrayMenu(t('tray.open'), t('tray.quit'))
  const floatOn = () => settings.get('ui.float.enabled') !== '0'
  // 显隐规则(§12 E):主窗在前藏悬浮窗、退后 / 最小化显;启动按当前聚焦定初值
  // (静默启动主窗不在前 → 显;正常启动主窗在前 → 藏)。
  watch(
    () => settings.state.ready,
    async (ready) => {
      if (!ready) return
      const focused = await isWindowFocused()
      void setFloatVisible(floatOn() && !focused)
    },
    { immediate: true },
  )
  onWindowFocus((focused) => void setFloatVisible(floatOn() && !focused))
  // 悬浮窗点通知 → 主窗切到该会话
  onOpenConversation((convId) => useChat().selectConversation(convId))
}
// 备选皮:HudBackdrop / StarfieldBackdrop / ScifiBackdrop / HologramBackdrop / ChatView。
</script>

<template>
  <!-- 悬浮窗模式:独立 WebView,只渲染它自己(PLAN §12) -->
  <FloatWindow v-if="isFloat" />
  <template v-else>
    <div class="app-stage" :class="{ booting }">
      <NeonBackdrop :booting="booting" />
      <MainLayout :booting="booting" />
      <!-- 全局浮层:任务 HUD(右缘)+ 视频面板;聊天/设置切换不影响它们 -->
      <TasksOverlay />
      <VideoOverlay />
      <!-- 主窗自绘三键(无边框补窗控,PLAN §12) -->
      <WindowControls />
    </div>
    <transition name="boot-hint">
      <div v-if="booting" class="skip-hint" @click="skip">{{ t('boot.skip') }}</div>
    </transition>
  </template>
</template>

<style>
/* 整窗入场:从中心缩放放大(配合透明窗口 → 画面从中间长出来) */
.app-stage { position: fixed; inset: 0; transform-origin: center center; }
.app-stage.booting { animation: stageZoom .72s cubic-bezier(.2, .75, .25, 1) both; }
@keyframes stageZoom {
  from { opacity: 0; transform: scale(.16) rotate(-7deg); filter: blur(12px); }
  55% { opacity: 1; }
  to { opacity: 1; transform: scale(1) rotate(0deg); filter: blur(0); }
}

.skip-hint {
  position: fixed; bottom: 18px; left: 50%; transform: translateX(-50%); z-index: 50;
  font: 12px/1 ui-monospace, "SF Mono", monospace; letter-spacing: 2px;
  color: rgba(150, 210, 255, 0.55); pointer-events: none; user-select: none;
}
.boot-hint-enter-active, .boot-hint-leave-active { transition: opacity .4s ease; }
.boot-hint-enter-from, .boot-hint-leave-to { opacity: 0; }
</style>
