<script setup lang="ts">
// 戏份道具(纯观感,零美术依赖):线稿风 SVG,只用语义 token(§6.7)→ 换肤自动跟随;
// 与角色无关(titan/狗/猫一次全覆盖),动效各自内置。尺寸由父容器定(width/height 100%)。
import type { PetActivity } from '../composables/usePetActivity'

defineProps<{ activity: PetActivity }>()
</script>

<template>
  <!-- 搬箱子(下载/解压/打包/加工…) -->
  <svg v-if="activity === 'carry'" class="prop bob" viewBox="0 0 24 24">
    <rect x="4" y="7.5" width="16" height="12.5" rx="1.5" />
    <path d="M4 13h16M12 7.5V20M4 7.5l2.2-3.2h11.6L20 7.5" />
  </svg>
  <!-- 放大镜(扫盘/看网页/解析/找歌词) -->
  <svg v-else-if="activity === 'inspect'" class="prop sweep" viewBox="0 0 24 24">
    <circle cx="10" cy="10" r="6" />
    <path d="M14.6 14.6L21 21" />
  </svg>
  <!-- 音符(放着歌/片) -->
  <svg v-else-if="activity === 'groove'" class="prop rise" viewBox="0 0 24 24">
    <path d="M9 18V6l8-2v11" />
    <circle cx="6.6" cy="18" r="2.4" />
    <circle cx="14.6" cy="15" r="2.4" />
  </svg>
  <!-- 想事情(思考中) -->
  <svg v-else class="prop" viewBox="0 0 24 24">
    <rect x="2" y="6" width="20" height="12" rx="6" class="bubble" />
    <circle cx="8" cy="12" r="1.7" class="dot" />
    <circle cx="12" cy="12" r="1.7" class="dot d2" />
    <circle cx="16" cy="12" r="1.7" class="dot d3" />
  </svg>
</template>

<style scoped>
.prop {
  display: block;
  width: 100%;
  height: 100%;
  fill: rgba(var(--accent-rgb), 0.16);
  stroke: var(--accent);
  stroke-width: 1.7;
  stroke-linecap: round;
  stroke-linejoin: round;
}
.prop .bubble { fill: var(--surface); stroke: var(--line); }
.prop .dot { fill: var(--text-dim); stroke: none; animation: prop-blink 1.2s infinite; }
.prop .d2 { animation-delay: 0.2s; }
.prop .d3 { animation-delay: 0.4s; }
.bob { animation: prop-bob 0.9s ease-in-out infinite; }
.sweep { animation: prop-sweep 1.6s ease-in-out infinite; }
.rise { animation: prop-rise 1.4s ease-in-out infinite; }
@keyframes prop-bob {
  0%, 100% { transform: translateY(0); }
  50% { transform: translateY(2px); }
}
@keyframes prop-sweep {
  0%, 100% { transform: translate(0, 0) rotate(0deg); }
  50% { transform: translate(2px, 1.5px) rotate(9deg); }
}
@keyframes prop-rise {
  0% { transform: translateY(1.5px); opacity: 0.75; }
  55% { transform: translateY(-1.5px); opacity: 1; }
  100% { transform: translateY(1.5px); opacity: 0.75; }
}
@keyframes prop-blink {
  0%, 100% { opacity: 0.25; }
  30% { opacity: 1; }
}
</style>
