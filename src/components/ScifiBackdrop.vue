<script setup lang="ts">
import { ref, onMounted, onUnmounted } from 'vue'

// 鼠标视差:背景随指针轻微纵深倾斜,让纯背景"活"起来。
const depth = ref<HTMLElement | null>(null)

function onMove(e: MouseEvent) {
  const ry = (e.clientX / window.innerWidth - 0.5) * 10
  const rx = -(e.clientY / window.innerHeight - 0.5) * 10
  const el = depth.value
  if (el) {
    el.style.setProperty('--ry', ry.toFixed(2) + 'deg')
    el.style.setProperty('--rx', rx.toFixed(2) + 'deg')
  }
}

onMounted(() => window.addEventListener('mousemove', onMove))
onUnmounted(() => window.removeEventListener('mousemove', onMove))
</script>

<template>
  <!-- 整块背景可拖动(无系统标题栏后用它移动窗口) -->
  <div class="deck" data-tauri-drag-region>
    <div class="depth" ref="depth">
      <div class="horizon"></div>
    </div>
  </div>
</template>

<style scoped>
/* 科幻皮(scifi)· 背景层 —— 移植自 dashbord。
   色值先内联,做正式换肤时再抽到 [data-theme="scifi"] 的主题 token。 */
.deck {
  position: fixed;
  inset: 0;
  overflow: hidden;
  background:
    radial-gradient(40% 50% at 16% 18%, rgba(45, 100, 175, 0.18), transparent 60%),
    radial-gradient(38% 46% at 86% 18%, rgba(28, 120, 120, 0.14), transparent 60%),
    linear-gradient(160deg, #070f1c, #03060d 78%);
  box-shadow: inset 0 0 170px rgba(0, 0, 0, 0.65);
  perspective: 1200px;
  perspective-origin: 50% 42%;
}

/* 缓慢扫描带 */
.deck::after {
  content: "";
  position: absolute; left: 0; right: 0; top: -160px; height: 160px;
  pointer-events: none; z-index: 5;
  background: linear-gradient(180deg, transparent, rgba(120, 200, 255, 0.05), transparent);
  animation: deck-scan 8s linear infinite;
}
@keyframes deck-scan { to { transform: translateY(calc(100vh + 160px)); } }

/* 3D 纵深空间(随鼠标轻微倾斜 → 视差) */
.depth {
  position: absolute; inset: 0; z-index: 0; pointer-events: none;
  transform-style: preserve-3d;
  transform: rotateX(var(--rx, 0deg)) rotateY(var(--ry, 0deg));
  transition: transform .25s ease-out;
  will-change: transform;
}

/* 透视地板网格(向远处收) */
.depth::before {
  content: ""; position: absolute; left: -60%; right: -60%; bottom: -25%; height: 150%;
  background-image:
    linear-gradient(rgba(120, 215, 255, 0.52) 1px, transparent 1px),
    linear-gradient(90deg, rgba(120, 215, 255, 0.34) 1px, transparent 1px);
  background-size: 54px 54px;
  transform: translateZ(-220px) rotateX(72deg);
  transform-origin: bottom center;
  -webkit-mask-image: linear-gradient(to top, rgba(0,0,0,0.95), transparent 66%);
          mask-image: linear-gradient(to top, rgba(0,0,0,0.95), transparent 66%);
}

/* 远景背网格(更淡、更远) */
.depth::after {
  content: ""; position: absolute; inset: -15%;
  background-image:
    linear-gradient(rgba(110, 200, 250, 0.17) 1px, transparent 1px),
    linear-gradient(90deg, rgba(110, 200, 250, 0.13) 1px, transparent 1px);
  background-size: 60px 60px;
  transform: translateZ(-340px) scale(1.4);
  -webkit-mask-image: radial-gradient(circle at 50% 40%, black, transparent 72%);
          mask-image: radial-gradient(circle at 50% 40%, black, transparent 72%);
}

/* 发光地平线(地板收束处 → 强距离感) */
.horizon {
  position: absolute; left: -25%; right: -25%; bottom: 37%; height: 2px;
  background: linear-gradient(90deg, transparent, rgba(125, 220, 255, 0.9), transparent);
  box-shadow: 0 0 22px 5px rgba(95, 200, 255, 0.45);
  transform: translateZ(-200px);
}

@media (prefers-reduced-motion: reduce) {
  .deck::after, .depth { animation: none !important; transition: none !important; }
}
</style>
