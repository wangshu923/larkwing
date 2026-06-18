<script setup lang="ts">
// 护眼·豆沙绿背景:淡豆沙绿底 + 两团极淡极慢的青绿光晕(柔焦),无网格/扫描/HUD/辉光。
// 护眼优先"静":比暖萌更慢更淡,中心留白给旺财让位。纯 CSS(不走 JS RAF,不碰 §8.1 满帧坑)。
defineProps<{ booting?: boolean }>()
</script>

<template>
  <div class="green" data-tauri-drag-region :class="{ booting }">
    <span class="blob b1"></span>
    <span class="blob b2"></span>
    <div class="top-glow"></div>
    <div class="grain"></div>
  </div>
</template>

<style scoped>
.green {
  position: fixed; inset: 0; overflow: hidden;
  border-radius: 10px;
  /* 豆沙绿底:淡绿白 → 豆沙绿(顶亮底沉一点点),与浅绿卡片同族、融成一片不割裂。再淡一档。 */
  background: radial-gradient(120% 92% at 50% 0%, #eef5e6 0%, #e6f0d9 48%, #dde9d0 100%);
}

/* 柔焦绿光团:低饱和,大模糊,极慢漂移(护眼忌动得快);整体压淡只做氛围 */
.blob {
  position: absolute; border-radius: 50%; filter: blur(52px);
  will-change: transform; pointer-events: none; opacity: .22;
}
.b1 { width: 360px; height: 360px; left: -70px; top: 10%; background: radial-gradient(circle, rgba(120, 168, 120, 0.55), transparent 70%); animation: gdrift1 44s ease-in-out infinite; }
.b2 { width: 320px; height: 320px; right: -50px; bottom: -110px; background: radial-gradient(circle, rgba(150, 190, 140, 0.5), transparent 70%); opacity: .18; animation: gdrift2 52s ease-in-out infinite; }

@keyframes gdrift1 { 0%, 100% { translate: 0 0; } 50% { translate: 18px -16px; } }
@keyframes gdrift2 { 0%, 100% { translate: 0 0; } 50% { translate: -16px 14px; } }

/* 顶部一缕极淡天光,给"光从上方来"的自然感 */
.top-glow {
  position: absolute; left: -10%; right: -10%; top: -12%; height: 40%; pointer-events: none;
  background: radial-gradient(60% 100% at 50% 0%, rgba(240, 248, 232, 0.4), transparent 72%);
}
/* 极淡纹理,免大色块发腻 */
.grain {
  position: absolute; inset: 0; pointer-events: none; opacity: .02;
  background-image: radial-gradient(rgba(50, 70, 40, 0.6) 0.5px, transparent 0.6px);
  background-size: 4px 4px;
}

/* 入场:整体柔和淡入(配合 .app-stage 的缩放) */
.green.booting .blob { animation-duration: 1s; animation-iteration-count: 1; }
.green.booting .top-glow, .green.booting .grain { animation: greenFade 1s ease-out backwards; }
.green.booting::before { content: ""; position: absolute; inset: 0; animation: greenFade .7s ease-out backwards; }
@keyframes greenFade { from { opacity: 0; } }
</style>
