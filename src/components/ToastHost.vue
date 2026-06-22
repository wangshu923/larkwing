<script setup lang="ts">
// 提示宿主:只在主窗顶层挂一个(App.vue),渲染 useToast 的队列。顶部居中浮现、点一下/到时自动消失。
// 只用语义 token(§6.7 不写死颜色):error→--danger / ok→--ok / info→--accent,换肤自动跟随。
import { useToast } from '../composables/useToast'

const { toasts, dismiss } = useToast()
</script>

<template>
  <div class="toast-host" aria-live="polite">
    <TransitionGroup name="toast">
      <div
        v-for="tt in toasts"
        :key="tt.id"
        class="toast"
        :class="tt.kind"
        role="status"
        @click="dismiss(tt.id)"
      >
        <span class="dot"></span>
        <span class="msg">{{ tt.text }}</span>
      </div>
    </TransitionGroup>
  </div>
</template>

<style scoped>
.toast-host {
  position: fixed;
  top: 14px;
  left: 50%;
  transform: translateX(-50%);
  z-index: 130; /* 在数据弹窗(120)之上:操作失败的提示永不被挡 */
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 8px;
  pointer-events: none; /* 容器不挡点击,只有 toast 本身可点关 */
  max-width: min(440px, 92vw);
}
.toast {
  pointer-events: auto;
  display: flex;
  align-items: center;
  gap: 9px;
  max-width: 100%;
  padding: 10px 15px;
  border-radius: 11px;
  /* --surface 各皮透明度不一(科幻 0.55 玻璃)→ 叠在不透明 --bg 上,保证提示在任何皮肤/背景上都看得清 */
  background-color: var(--bg);
  background-image: linear-gradient(var(--surface), var(--surface));
  border: 1px solid var(--line);
  border-left: 3px solid var(--accent);
  box-shadow: 0 12px 34px rgba(0, 0, 0, 0.38);
  color: var(--text);
  font-size: 13px;
  line-height: 1.5;
  cursor: pointer;
}
.toast.error { border-left-color: var(--danger); }
.toast.ok { border-left-color: var(--ok); }
.toast.info { border-left-color: var(--accent); }
.dot {
  flex: none;
  width: 7px;
  height: 7px;
  border-radius: 50%;
  background: var(--accent);
}
.toast.error .dot { background: var(--danger); }
.toast.ok .dot { background: var(--ok); }
.toast.info .dot { background: var(--accent); }
.msg {
  min-width: 0;
  word-break: break-word;
}

/* 顶部滑入 + 淡出;move 让下方 toast 关闭后上移平滑 */
.toast-enter-active,
.toast-leave-active { transition: opacity 0.28s ease, transform 0.28s ease; }
.toast-enter-from { opacity: 0; transform: translateY(-10px); }
.toast-leave-to { opacity: 0; transform: translateY(-8px); }
.toast-move { transition: transform 0.28s ease; }
</style>
