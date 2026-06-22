<script setup lang="ts">
// 一键更新卡(清单 ⑤·A):右下角非阻塞卡。两态 ——
//   ① 发现新版:版本 + 更新说明 + 「更新 / 稍后」;点「更新」= 起后台下载任务(进度去任务 HUD),卡收起。
//   ② 已下载:「现在重启更新?」+ 「立即更新 / 稍后」;点「立即更新」= 装 + 重启。
// 下载进度不在卡里(在任务 HUD,不阻塞操作)。只主窗挂。只用语义 token(§6.7),换肤跟随。
import { useI18n } from 'vue-i18n'
import { useUpdater } from '../composables/useUpdater'

const { t } = useI18n()
const { state, download, install, dismiss } = useUpdater()
</script>

<template>
  <transition name="upd">
    <div v-if="state.downloaded" class="upd-card" role="dialog">
      <div class="upd-head">
        <span class="upd-dot"></span>
        <b>{{ t('update.ready') }}</b>
      </div>
      <div class="upd-acts">
        <button class="upd-btn primary" @click="install">{{ t('update.installNow') }}</button>
        <button class="upd-btn" @click="dismiss">{{ t('update.later') }}</button>
      </div>
    </div>
    <div v-else-if="state.available" class="upd-card" role="dialog">
      <div class="upd-head">
        <span class="upd-dot"></span>
        <b>{{ t('update.found', { version: state.available.version }) }}</b>
      </div>
      <p v-if="state.available.notes" class="upd-notes">{{ state.available.notes }}</p>
      <div class="upd-acts">
        <button class="upd-btn primary" @click="download">{{ t('update.update') }}</button>
        <button class="upd-btn" @click="dismiss">{{ t('update.later') }}</button>
      </div>
    </div>
  </transition>
</template>

<style scoped>
.upd-card {
  position: fixed;
  right: 18px;
  bottom: 18px;
  z-index: 100;
  width: min(340px, calc(100vw - 36px));
  padding: 14px 16px;
  border-radius: 13px;
  /* 同 toast:--surface 各皮透明度不一 → 叠在不透明 --bg 上,任何皮肤/背景都看得清 */
  background-color: var(--bg);
  background-image: linear-gradient(var(--surface), var(--surface));
  border: 1px solid var(--line);
  box-shadow: 0 16px 44px rgba(0, 0, 0, 0.4);
  color: var(--text);
}
.upd-head { display: flex; align-items: center; gap: 9px; font-size: 14px; }
.upd-dot { width: 8px; height: 8px; border-radius: 50%; background: var(--accent); box-shadow: 0 0 8px var(--accent); flex: none; }
.upd-notes {
  margin: 9px 0 0;
  font-size: 12.5px;
  line-height: 1.6;
  color: var(--text-dim);
  white-space: pre-line;
  max-height: 7.5em;
  overflow-y: auto;
  scrollbar-gutter: stable;
}
.upd-acts { display: flex; gap: 10px; margin-top: 14px; }
.upd-btn {
  flex: 1;
  padding: 8px 12px;
  border-radius: 9px;
  border: 1px solid var(--line);
  background: transparent;
  color: var(--text);
  font-size: 13px;
  cursor: pointer;
  transition: border-color 0.15s, background 0.15s;
}
.upd-btn:hover { border-color: var(--accent); }
.upd-btn.primary { background: var(--accent); border-color: var(--accent); color: var(--bg); font-weight: 600; }
.upd-enter-active, .upd-leave-active { transition: opacity 0.3s ease, transform 0.3s ease; }
.upd-enter-from, .upd-leave-to { opacity: 0; transform: translateY(12px); }
</style>
