<script setup lang="ts">
// Larkwing 品牌标记:一只从发光核心升起的程式化云雀羽翼(name = lark + wing)。
// 人格中立、可换肤:全程 currentColor,默认取 --accent → 科幻青 / 暖萌橙随皮肤自动切,组件零改动。
// 用途:启动闪屏 / 关于页 / 空状态 / 加载占位。形象(狗猫等出镜角色)走 ui.character,与本标记互不相干。
withDefaults(defineProps<{
  size?: number | string   // 像素数或任意 CSS 长度(如 '2rem')
  glow?: boolean           // 加一层柔辉光(drop-shadow);静态展示开,频繁动效场景慎用
  title?: string           // 无障碍标签
}>(), { size: 28, glow: false, title: 'Larkwing' })
</script>

<template>
  <svg
    class="lw-mark" :class="{ glow }"
    :width="size" :height="size" viewBox="0 0 200 200"
    role="img" :aria-label="title" xmlns="http://www.w3.org/2000/svg"
  >
    <g transform="translate(-6 -2)" fill="currentColor">
      <!-- 核心光环 -->
      <circle cx="68" cy="138" r="21" fill="none" stroke="currentColor" stroke-width="2" opacity="0.3" />
      <!-- 五片羽毛:自核心呈扇形向右上方升起(单色,跟随 currentColor) -->
      <path d="M0 0 Q42 -26 100 0 Q42 26 0 0 Z" transform="translate(68 138) rotate(-6) scale(0.80)" />
      <path d="M0 0 Q42 -26 100 0 Q42 26 0 0 Z" transform="translate(68 138) rotate(-22) scale(0.95)" />
      <path d="M0 0 Q42 -26 100 0 Q42 26 0 0 Z" transform="translate(68 138) rotate(-39) scale(1.02)" />
      <path d="M0 0 Q42 -26 100 0 Q42 26 0 0 Z" transform="translate(68 138) rotate(-56) scale(0.99)" />
      <path d="M0 0 Q42 -26 100 0 Q42 26 0 0 Z" transform="translate(68 138) rotate(-73) scale(0.84)" />
      <!-- 核心光点 -->
      <circle cx="68" cy="138" r="10" fill="currentColor" />
    </g>
  </svg>
</template>

<style scoped>
.lw-mark { color: var(--accent); display: inline-block; vertical-align: middle; }
.lw-mark.glow { filter: drop-shadow(0 0 5px rgba(var(--accent-rgb), 0.55)); }
</style>
