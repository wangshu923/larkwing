<script setup lang="ts">
// 护眼·暗夜背景:暖炭黑底 + 顶上一缕极淡暖光(像台灯漫射)+ 一团极慢暖琥珀晕,哑光无辉光。
// 暖色温、低刺激,适合夜间整晚常驻;中心留白给旺财。纯 CSS(不走 JS RAF,不碰 §8.1 满帧坑)。
defineProps<{ booting?: boolean }>()
</script>

<template>
  <div class="night" data-tauri-drag-region :class="{ booting }">
    <span class="blob b1"></span>
    <div class="lamp"></div>
    <div class="grain"></div>
  </div>
</template>

<style scoped>
.night {
  position: fixed; inset: 0; overflow: hidden;
  border-radius: 10px;
  /* 暖炭黑:顶部略暖(暖光来向)→ 底部更沉,非冷蓝、非纯黑 */
  background: radial-gradient(120% 96% at 50% -6%, #2a251d 0%, #221e18 46%, #1b1813 100%);
}

/* 一团极淡暖琥珀晕:慢漂、大模糊,只透一点暖意,不发光 */
.blob {
  position: absolute; border-radius: 50%; filter: blur(60px);
  width: 380px; height: 380px; left: -90px; top: 6%;
  background: radial-gradient(circle, rgba(201, 169, 106, 0.28), transparent 70%);
  opacity: .5; pointer-events: none; will-change: transform;
  animation: ndrift 56s ease-in-out infinite;
}
@keyframes ndrift { 0%, 100% { translate: 0 0; } 50% { translate: 20px -14px; } }

/* 顶上一缕暖光,像台灯从上方漫下来(护眼=暖、柔、来向一致) */
.lamp {
  position: absolute; left: -10%; right: -10%; top: -14%; height: 46%; pointer-events: none;
  background: radial-gradient(58% 100% at 50% 0%, rgba(210, 170, 110, 0.16), transparent 72%);
}
/* 极淡纹理,免暗色块发腻 / 出现色带 */
.grain {
  position: absolute; inset: 0; pointer-events: none; opacity: .03;
  background-image: radial-gradient(rgba(220, 200, 160, 0.5) 0.5px, transparent 0.6px);
  background-size: 4px 4px;
}

/* 入场:整体柔和淡入(配合 .app-stage 的缩放) */
.night.booting .blob { animation-duration: 1s; animation-iteration-count: 1; }
.night.booting .lamp, .night.booting .grain { animation: nightFade 1s ease-out backwards; }
.night.booting::before { content: ""; position: absolute; inset: 0; animation: nightFade .7s ease-out backwards; }
@keyframes nightFade { from { opacity: 0; } }
</style>
