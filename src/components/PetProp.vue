<script setup lang="ts">
// 戏份道具(纯观感,零美术依赖):线稿风 SVG,只用语义 token(§6.7)→ 换肤自动跟随;
// 与角色无关(titan/狗/猫一次全覆盖),动效各自内置。尺寸由父容器定(width/height 100%)。
// B1 起按「图形」渲染(glyph;戏份→图形的映射单源在 usePetActivity::glyphOf),
// 行为层的浮标(Zzz/惊叹/问号/信封)与戏份四样同一套线稿语汇。
import type { PropGlyph } from '../composables/usePetActivity'

defineProps<{ glyph: PropGlyph }>()
</script>

<template>
  <!-- 搬箱子(下载/解压/打包…) -->
  <svg v-if="glyph === 'box'" class="prop bob" viewBox="0 0 24 24">
    <rect x="4" y="7.5" width="16" height="12.5" rx="1.5" />
    <path d="M4 13h16M12 7.5V20M4 7.5l2.2-3.2h11.6L20 7.5" />
  </svg>
  <!-- 放大镜(扫盘/看网页/解析/找歌词) -->
  <svg v-else-if="glyph === 'lens'" class="prop sweep" viewBox="0 0 24 24">
    <circle cx="10" cy="10" r="6" />
    <path d="M14.6 14.6L21 21" />
  </svg>
  <!-- 音符(放着歌/片) -->
  <svg v-else-if="glyph === 'note'" class="prop rise" viewBox="0 0 24 24">
    <path d="M9 18V6l8-2v11" />
    <circle cx="6.6" cy="18" r="2.4" />
    <circle cx="14.6" cy="15" r="2.4" />
  </svg>
  <!-- 睡觉 Zzz(行为层:打盹) -->
  <svg v-else-if="glyph === 'zzz'" class="prop" viewBox="0 0 24 24">
    <path class="z z1" d="M3 15h6l-6 6h6" />
    <path class="z z2" d="M12.5 9h4.6l-4.6 4.6h4.6" />
    <path class="z z3" d="M18.5 3.5h3.4l-3.4 3.4h3.4" />
  </svg>
  <!-- 惊叹(被戳了一下) -->
  <svg v-else-if="glyph === 'bang'" class="prop pop" viewBox="0 0 24 24">
    <path d="M12 3.5v11" class="thick" />
    <circle cx="12" cy="20" r="1.6" class="dotfill" />
  </svg>
  <!-- 问号(任务没成,懵) -->
  <svg v-else-if="glyph === 'question'" class="prop pop" viewBox="0 0 24 24">
    <path d="M7.5 8.2a4.5 4.5 0 1 1 6.4 4.1c-1.2.6-1.9 1.4-1.9 2.9" class="thick" />
    <circle cx="12" cy="20" r="1.6" class="dotfill" />
  </svg>
  <!-- 信封(送信:手机渠道有动静) -->
  <svg v-else-if="glyph === 'mail'" class="prop bob" viewBox="0 0 24 24">
    <rect x="3" y="6" width="18" height="13" rx="1.5" />
    <path d="M3.5 7l8.5 6.5L20.5 7" />
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
.prop .z { fill: none; animation: prop-blink 2.1s infinite; }
.prop .z2 { animation-delay: 0.4s; }
.prop .z3 { animation-delay: 0.8s; }
.prop .thick { fill: none; stroke-width: 2.4; }
.prop .dotfill { fill: var(--accent); stroke: none; }
.bob { animation: prop-bob 0.9s ease-in-out infinite; }
.sweep { animation: prop-sweep 1.6s ease-in-out infinite; }
.rise { animation: prop-rise 1.4s ease-in-out infinite; }
.pop { animation: prop-pop 0.5s ease-out; }
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
@keyframes prop-pop {
  0% { transform: scale(0.4); opacity: 0; }
  60% { transform: scale(1.15); opacity: 1; }
  100% { transform: scale(1); }
}
</style>
