<script setup lang="ts">
import { ref, onMounted, onUnmounted } from 'vue'

// 暖萌皮(旺财)背景:柔奶油底 + 几团缓慢漂浮的暖色光晕(桃/腮红/橙),无网格无扫描无 HUD。
// 中心留白给旺财让位(同科幻皮)。色值用 :root 的暖萌原始色板(换肤架构的"art"层)。
defineProps<{ booting?: boolean }>()

const root = ref<HTMLElement | null>(null)
function onMove(e: MouseEvent) {
  const el = root.value
  if (!el) return
  el.style.setProperty('--px', ((e.clientX / window.innerWidth - 0.5) * 16).toFixed(1) + 'px')
  el.style.setProperty('--py', ((e.clientY / window.innerHeight - 0.5) * 16).toFixed(1) + 'px')
}
onMounted(() => window.addEventListener('mousemove', onMove))
onUnmounted(() => window.removeEventListener('mousemove', onMove))
</script>

<template>
  <div class="warm" ref="root" data-tauri-drag-region :class="{ booting }">
    <!-- 缓慢漂浮的暖色光团(柔焦);中心留白 -->
    <span class="blob b1"></span>
    <span class="blob b2"></span>
    <span class="blob b3"></span>
    <span class="blob b4"></span>
    <!-- 顶部一缕暖光 + 底部托色 -->
    <div class="top-glow"></div>
    <div class="grain"></div>
  </div>
</template>

<style scoped>
.warm {
  position: fixed; inset: 0; overflow: hidden;
  border-radius: 10px;
  /* 奶油底:亮奶白 → 奶橙(原方向,但调淡一档)—— 与浅奶油卡片同族、融成一片不割裂,
     不晃眼靠下面把光团/顶光压淡 + 前景字/描边加重(可读),不靠把底色压沉。 */
  background: radial-gradient(120% 92% at 50% 0%, #fffefb 0%, #fff6ec 50%, #ffeede 100%);
}

/* 柔焦光团:低饱和暖色,大模糊,缓慢漂移(无机械感);整体压淡,只做氛围不抢戏 */
.blob {
  position: absolute; border-radius: 50%; filter: blur(48px);
  transform: translate(var(--px, 0), var(--py, 0));
  will-change: transform; pointer-events: none; opacity: .3;
}
.b1 { width: 340px; height: 340px; left: -60px; top: 8%; background: radial-gradient(circle, var(--peach), transparent 70%); animation: drift1 26s ease-in-out infinite; }
.b2 { width: 300px; height: 300px; right: -40px; top: 12%; background: radial-gradient(circle, var(--blush), transparent 70%); opacity: .24; animation: drift2 31s ease-in-out infinite; }
.b3 { width: 380px; height: 380px; left: 18%; bottom: -120px; background: radial-gradient(circle, rgba(210, 150, 98, 0.45), transparent 70%); opacity: .22; animation: drift3 35s ease-in-out infinite; }
.b4 { width: 240px; height: 240px; right: 16%; bottom: -60px; background: radial-gradient(circle, rgba(244, 224, 198, 0.7), transparent 70%); animation: drift1 29s ease-in-out infinite reverse; }

@keyframes drift1 { 0%, 100% { translate: 0 0; } 50% { translate: 26px -22px; } }
@keyframes drift2 { 0%, 100% { translate: 0 0; } 50% { translate: -30px 24px; } }
@keyframes drift3 { 0%, 100% { translate: 0 0; } 50% { translate: 20px -28px; } }

/* 顶部一缕暖光,极淡,给"光从上方来"的暖意 */
.top-glow {
  position: absolute; left: -10%; right: -10%; top: -12%; height: 42%; pointer-events: none;
  background: radial-gradient(60% 100% at 50% 0%, rgba(255, 244, 228, 0.36), transparent 72%);
}
/* 一点点纹理,免大色块发腻(极淡) */
.grain {
  position: absolute; inset: 0; pointer-events: none; opacity: .025;
  background-image: radial-gradient(rgba(120, 80, 40, 0.6) 0.5px, transparent 0.6px);
  background-size: 4px 4px;
}

/* 入场:整体柔和淡入(配合 .app-stage 的缩放) */
.warm.booting .blob { animation-duration: .9s; animation-iteration-count: 1; }
.warm.booting .top-glow, .warm.booting .grain { animation: warmFade 1s ease-out backwards; }
.warm.booting::before { content: ""; position: absolute; inset: 0; animation: warmFade .7s ease-out backwards; }
@keyframes warmFade { from { opacity: 0; } }
</style>
