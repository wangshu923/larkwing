<script setup lang="ts">
import { computed, onMounted, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import NeonBackdrop from './components/NeonBackdrop.vue'
import WarmBackdrop from './components/WarmBackdrop.vue'
import GreenBackdrop from './components/GreenBackdrop.vue'
import NightBackdrop from './components/NightBackdrop.vue'
import MainLayout from './components/MainLayout.vue'
import TasksOverlay from './components/TasksOverlay.vue'
import VideoOverlay from './components/VideoOverlay.vue'
import WindowControls from './components/WindowControls.vue'
import FloatWindow from './components/FloatWindow.vue'
import ContextMenu from './components/ContextMenu.vue'
import { useBoot } from './composables/useBoot'
import { useChat } from './composables/useChat'
import { useSettings } from './composables/useSettings'
import {
  api,
  isTauri,
  onOpenConversation,
  onShowFloat,
  setFloatVisible,
  win,
  windowLabel,
} from './lib/backend'

const { t } = useI18n()

// 窗口分流(PLAN §12):float 标签 = 悬浮窗(独立 WebView),否则主窗全套。
const isFloat = windowLabel() === 'float'

// 启动编排(仅主窗):phase = 'boot' → 'ready';背景与主界面各自订阅它做入场。
const { phase, run, skip } = useBoot(1800)
if (!isFloat) run()
const booting = computed(() => !isFloat && phase.value === 'boot')

// 皮肤驱动背景:语义 token 负责换色,背景组件按皮肤切(科幻=霓虹辉光,暖萌=柔光晕,
// 护眼绿/暗夜=安静柔晕);skin 由 boot 过桥设到 <html data-skin>,切换即时反映。
// 未知皮肤(脏数据)回落科幻 —— 与 useSettings.applySkin 的兜底同向。
const settings = useSettings()
const BACKDROPS: Record<string, typeof NeonBackdrop> = {
  warm: WarmBackdrop,
  green: GreenBackdrop,
  night: NightBackdrop,
}
const backdrop = computed(() => BACKDROPS[settings.state.skin] || NeonBackdrop)

// 数据「搬家」提示(仅主窗,§3.5 不静默):位置失效 → 恢复弹窗;搬完有旧数据 → 清理弹窗。
const dataNotice = ref<'missing' | 'old' | null>(null)
const dataNoticePath = ref('')
const dataBusy = ref(false)
async function noticeAction(kind: 'reset' | 'quit' | 'delete' | 'keep') {
  if (dataBusy.value) return
  dataBusy.value = true
  try {
    if (kind === 'reset') await api.dataResetToDefault() // 重启,不返回
    else if (kind === 'quit') await api.quitApp()
    else if (kind === 'delete') await api.cleanupOldData()
    else await api.keepOldData()
    dataNotice.value = null
  } catch (e) {
    console.error('数据提示操作失败', e)
  } finally {
    dataBusy.value = false
  }
}

// 主窗专属编排(PLAN §12):托盘菜单文案 + 悬浮窗显隐(enabled × 主窗是否在前)+ 通知跳会话。
if (!isFloat && isTauri()) {
  void api.setTrayMenu(t('tray.open'), t('tray.showFloat'), t('tray.quit'))
  const floatOn = () => settings.get('ui.float.enabled') !== '0'
  // 显隐规则(§12 E 修订 2026-06-14):悬浮窗与主窗共存——master 开关 ui.float.enabled 开着就常驻,
  // 不再随主窗聚焦藏匿(用户:开了就一直有)。唯一例外:主窗全屏(沉浸观感,如看视频)时让位,退出即恢复
  // ——float 是 always_on_top,不显式藏会浮在全屏画面上。全屏切换会触发 resize,借 onResized 兜住。
  let lastFs: boolean | null = null
  const syncFloat = async () => {
    if (!settings.state.ready) return
    const fs = await win.isFullscreen()
    if (fs === lastFs) return // 拖拽改窗口大小也发 resize;只在全屏态真变化时动手,免反复 show/hide
    lastFs = fs
    void setFloatVisible(floatOn() && !fs)
  }
  watch(() => settings.state.ready, () => void syncFloat(), { immediate: true })
  win.onResized(syncFloat)
  // 悬浮窗点通知 → 主窗切到该会话
  onOpenConversation((convId) => useChat().selectConversation(convId))
  // 托盘「显示悬浮窗」→ 重开:置 master 开关(持久化 + 广播)再显示;
  // enabled=1 后续 syncFloat(全屏切换等)也维持显示,不会被策略重新藏掉。
  onShowFloat(() => {
    settings.set('ui.float.enabled', '1')
    void setFloatVisible(true)
  })
  // boot 后查一次数据位置:失效 → 恢复弹窗;有旧数据残留 → 清理弹窗(主动来找用户,不用回设置页)。
  onMounted(async () => {
    try {
      const loc = await api.dataLocation()
      if (loc.missing) {
        dataNotice.value = 'missing'
        dataNoticePath.value = loc.missing
      } else if (loc.oldRoot) {
        dataNotice.value = 'old'
        dataNoticePath.value = loc.oldRoot
      }
    } catch (e) {
      console.error('数据位置检查失败', e)
    }
  })
}
// 备选皮:HudBackdrop / StarfieldBackdrop / ScifiBackdrop / HologramBackdrop。
</script>

<template>
  <!-- 右键菜单宿主:主窗 / 悬浮窗各自 WebView 顶层都挂一个(光标处弹出,见 useContextMenu) -->
  <ContextMenu />
  <!-- 悬浮窗模式:独立 WebView,只渲染它自己(PLAN §12) -->
  <FloatWindow v-if="isFloat" />
  <template v-else>
    <div class="app-stage" :class="{ booting }">
      <component :is="backdrop" :booting="booting" />
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
    <!-- 数据「搬家」提示:位置失效(恢复)/ 搬完旧数据待清理(主动弹一次,§3.5) -->
    <transition name="boot-hint">
      <div v-if="dataNotice" class="data-modal-veil">
        <div class="data-modal">
          <template v-if="dataNotice === 'missing'">
            <h3>{{ t('dataNotice.missingTitle') }}</h3>
            <p>{{ t('dataNotice.missingBody') }}</p>
            <p class="path">{{ dataNoticePath }}</p>
            <div class="acts">
              <button class="m-btn primary" :disabled="dataBusy" @click="noticeAction('quit')">{{ t('dataNotice.quit') }}</button>
              <button class="m-btn" :disabled="dataBusy" @click="noticeAction('reset')">{{ t('dataNotice.reset') }}</button>
            </div>
          </template>
          <template v-else>
            <h3>{{ t('dataNotice.oldTitle') }}</h3>
            <p>{{ t('dataNotice.oldBody') }}</p>
            <p class="path">{{ dataNoticePath }}</p>
            <div class="acts">
              <button class="m-btn primary" :disabled="dataBusy" @click="noticeAction('delete')">{{ t('dataNotice.delete') }}</button>
              <button class="m-btn" :disabled="dataBusy" @click="noticeAction('keep')">{{ t('dataNotice.keep') }}</button>
            </div>
          </template>
        </div>
      </div>
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

/* 数据「搬家」提示弹窗(主窗;语义 token,§6.7 不写死色) */
.data-modal-veil {
  position: fixed; inset: 0; z-index: 120; display: flex; align-items: center; justify-content: center;
  background: rgba(var(--veil-rgb, 0 0 0), 0.55); backdrop-filter: blur(3px);
}
.data-modal {
  width: min(420px, 86vw); padding: 24px 26px; border-radius: 14px;
  background: var(--surface); border: 1px solid var(--line); box-shadow: 0 24px 70px rgba(0, 0, 0, 0.45);
}
.data-modal h3 { margin: 0 0 10px; font-size: 16px; color: var(--text); }
.data-modal p { margin: 0 0 8px; font-size: 13px; line-height: 1.7; color: var(--text-dim); }
.data-modal .path {
  font-family: ui-monospace, "SF Mono", monospace; font-size: 11.5px; color: var(--text);
  background: rgba(var(--text-rgb, 255 255 255), 0.05); border-radius: 8px; padding: 8px 10px; word-break: break-all;
}
.data-modal .acts { display: flex; gap: 12px; margin-top: 18px; }
.data-modal .m-btn {
  flex: 1; padding: 9px 12px; border-radius: 9px; border: 1px solid var(--line);
  background: transparent; color: var(--text); font-size: 13px; cursor: pointer; transition: border-color .15s, background .15s;
}
.data-modal .m-btn:hover { border-color: var(--accent); }
.data-modal .m-btn.primary { background: var(--accent); border-color: var(--accent); color: var(--bg); font-weight: 600; }
.data-modal .m-btn:disabled { opacity: .5; cursor: default; }
</style>
