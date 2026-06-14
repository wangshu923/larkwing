<script setup lang="ts">
import { ref, onMounted, onUnmounted } from 'vue'

// 「璀璨星河」背景:夜空渐变 + 星云/银河(CSS)+ 繁星闪烁 + 流星(canvas)。
const cv = ref<HTMLCanvasElement | null>(null)
let ctx: CanvasRenderingContext2D | null = null
let raf = 0
let w = 0, h = 0, dpr = 1
let last = 0
let meteorTimer = 1.5

interface Star { x: number; y: number; r: number; base: number; sp: number; ph: number; color: string }
interface Meteor { x: number; y: number; vx: number; vy: number; life: number; max: number }
let stars: Star[] = []
let meteors: Meteor[] = []

// 大多数冷白,少量淡蓝 / 暖金 / 微紫 —— 制造"璀璨"层次
const STAR_COLORS = [
  'rgba(255,255,255,', 'rgba(255,255,255,', 'rgba(255,255,255,',
  'rgba(184,206,255,', 'rgba(255,228,186,', 'rgba(223,186,255,',
]

function build() {
  const count = Math.round((w * h) / 1300)
  stars = []
  for (let i = 0; i < count; i++) {
    stars.push({
      x: Math.random() * w,
      y: Math.random() * h,
      r: Math.random() * 1.4 + 0.25,
      base: Math.random() * 0.5 + 0.45,
      sp: Math.random() * 1.5 + 0.4,
      ph: Math.random() * Math.PI * 2,
      color: STAR_COLORS[Math.floor(Math.random() * STAR_COLORS.length)],
    })
  }
}

function resize() {
  const c = cv.value
  if (!c || !ctx) return
  dpr = Math.min(window.devicePixelRatio || 1, 2)
  w = c.clientWidth; h = c.clientHeight
  c.width = Math.round(w * dpr); c.height = Math.round(h * dpr)
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0)
  build()
}

function spawnMeteor() {
  meteors.push({
    x: Math.random() * w * 0.8 + w * 0.1,
    y: Math.random() * h * 0.35,
    vx: Math.cos(Math.PI * (0.18 + Math.random() * 0.10)) * (Math.random() * 220 + 320),
    vy: Math.sin(Math.PI * (0.18 + Math.random() * 0.10)) * (Math.random() * 220 + 320),
    life: 0,
    max: Math.random() * 0.5 + 0.6,
  })
}

function frame(ts: number) {
  if (!ctx) return
  const dt = last ? Math.min((ts - last) / 1000, 0.05) : 0.016
  last = ts
  ctx.clearRect(0, 0, w, h)

  // 繁星闪烁
  for (const s of stars) {
    s.ph += s.sp * dt
    const a = Math.max(0, s.base * (0.55 + 0.45 * Math.sin(s.ph)))
    ctx.beginPath()
    ctx.arc(s.x, s.y, s.r, 0, Math.PI * 2)
    ctx.fillStyle = s.color + a.toFixed(3) + ')'
    ctx.fill()
    if (s.r > 1.1) {  // 亮星加柔光晕
      ctx.beginPath()
      ctx.arc(s.x, s.y, s.r * 3.4, 0, Math.PI * 2)
      ctx.fillStyle = s.color + (a * 0.1).toFixed(3) + ')'
      ctx.fill()
    }
  }

  // 流星
  meteorTimer -= dt
  if (meteorTimer <= 0) { spawnMeteor(); meteorTimer = Math.random() * 4 + 2.5 }
  for (let i = meteors.length - 1; i >= 0; i--) {
    const m = meteors[i]
    m.life += dt
    m.x += m.vx * dt; m.y += m.vy * dt
    const p = m.life / m.max
    if (p >= 1 || m.x > w + 60 || m.y > h + 60) { meteors.splice(i, 1); continue }
    const len = Math.hypot(m.vx, m.vy)
    const tx = m.x - (m.vx / len) * 130
    const ty = m.y - (m.vy / len) * 130
    const alpha = Math.sin(Math.min(p, 1) * Math.PI)  // 渐显渐隐
    const g = ctx.createLinearGradient(m.x, m.y, tx, ty)
    g.addColorStop(0, `rgba(255,255,255,${(0.9 * alpha).toFixed(3)})`)
    g.addColorStop(0.3, `rgba(190,215,255,${(0.5 * alpha).toFixed(3)})`)
    g.addColorStop(1, 'rgba(190,215,255,0)')
    ctx.strokeStyle = g
    ctx.lineWidth = 2
    ctx.lineCap = 'round'
    ctx.beginPath(); ctx.moveTo(m.x, m.y); ctx.lineTo(tx, ty); ctx.stroke()
    ctx.beginPath(); ctx.arc(m.x, m.y, 1.7, 0, Math.PI * 2)
    ctx.fillStyle = `rgba(255,255,255,${alpha.toFixed(3)})`; ctx.fill()
  }

  raf = requestAnimationFrame(frame)
}

onMounted(() => {
  const c = cv.value
  if (!c) return
  ctx = c.getContext('2d')
  resize()
  window.addEventListener('resize', resize)
  raf = requestAnimationFrame(frame)
})

onUnmounted(() => {
  cancelAnimationFrame(raf)
  window.removeEventListener('resize', resize)
})
</script>

<template>
  <!-- 整块可拖动(无系统标题栏后用它移动窗口) -->
  <div class="galaxy" data-tauri-drag-region>
    <canvas ref="cv" class="stars"></canvas>
  </div>
</template>

<style scoped>
/* 璀璨星河 · 背景层。色值先内联,做正式换肤时抽到 [data-theme="galaxy"] 主题 token。 */
.galaxy {
  position: fixed;
  inset: 0;
  overflow: hidden;
  /* 夜空:顶部偏亮的深蓝紫,向下沉到近黑 */
  background: radial-gradient(125% 90% at 50% 6%, #1c1448 0%, #0c0c2a 36%, #050518 68%, #01010b 100%);
}

/* 星云光晕:几团柔和彩色,营造星河的"厚度" */
.galaxy::before {
  content: ""; position: absolute; inset: -12%; z-index: 0; pointer-events: none;
  background:
    radial-gradient(36% 30% at 22% 30%, rgba(124, 84, 222, 0.30), transparent 70%),
    radial-gradient(42% 34% at 80% 24%, rgba(58, 128, 232, 0.24), transparent 72%),
    radial-gradient(48% 40% at 62% 80%, rgba(222, 92, 182, 0.20), transparent 72%),
    radial-gradient(32% 28% at 36% 72%, rgba(72, 204, 200, 0.16), transparent 72%);
  filter: blur(6px);
}

/* 斜跨的银河带 */
.galaxy::after {
  content: ""; position: absolute; left: -25%; right: -25%; top: 26%; height: 52%; z-index: 1; pointer-events: none;
  background: linear-gradient(102deg, transparent 8%, rgba(180, 172, 255, 0.10) 34%, rgba(255, 224, 248, 0.16) 50%, rgba(176, 202, 255, 0.10) 66%, transparent 92%);
  transform: rotate(-17deg);
  filter: blur(16px);
}

canvas.stars {
  position: absolute; inset: 0; z-index: 2;
  width: 100%; height: 100%; display: block;
  pointer-events: none;
}
</style>
