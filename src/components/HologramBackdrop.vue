<script setup lang="ts">
import { onMounted, onUnmounted, ref } from 'vue'
import { useRafLoop } from '../composables/useRafLoop'

// 浅色全息玻璃背景:浅蓝灰通透底 + 柔彩光晕 + 磨砂玻璃面板(CSS)
//   + 数据粒子连线 + 右下角小雷达(canvas)。中心留白,给旺财让位。
const cv = ref<HTMLCanvasElement | null>(null)
const root = ref<HTMLElement | null>(null)
let ctx: CanvasRenderingContext2D | null = null
let w = 0, h = 0, dpr = 1, last = 0, t = 0

interface Node { x: number; y: number; vx: number; vy: number }
let nodes: Node[] = []

const LINE = 'rgba(56,140,200,'  // 浅底上的青蓝

function build() {
  nodes = []
  const nc = Math.round((w * h) / 18000) + 14
  for (let i = 0; i < nc; i++)
    nodes.push({ x: Math.random() * w, y: Math.random() * h, vx: (Math.random() - 0.5) * 10, vy: (Math.random() - 0.5) * 10 })
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

function frame(ts: number) {
  if (!ctx) return
  const dt = last ? Math.min((ts - last) / 1000, 0.05) : 0.016
  last = ts; t += dt
  ctx.clearRect(0, 0, w, h)

  // 数据节点 + 临近连线
  for (const n of nodes) {
    n.x += n.vx * dt; n.y += n.vy * dt
    if (n.x < 0) n.x += w; if (n.x > w) n.x -= w
    if (n.y < 0) n.y += h; if (n.y > h) n.y -= h
  }
  const D = 140
  for (let i = 0; i < nodes.length; i++) {
    for (let j = i + 1; j < nodes.length; j++) {
      const a = nodes[i], b = nodes[j]
      const d = Math.hypot(a.x - b.x, a.y - b.y)
      if (d < D) {
        ctx.beginPath(); ctx.moveTo(a.x, a.y); ctx.lineTo(b.x, b.y)
        ctx.strokeStyle = LINE + (0.13 * (1 - d / D)).toFixed(3) + ')'; ctx.lineWidth = 1; ctx.stroke()
      }
    }
  }
  for (const n of nodes) {
    ctx.beginPath(); ctx.arc(n.x, n.y, 1.6, 0, 6.283)
    ctx.fillStyle = LINE + '0.5)'; ctx.fill()
  }

  // —— 右下角小雷达部件(退角,不抢中心) ——
  const cx = w - 92, cy = h - 92, R = 60
  ctx.lineWidth = 1
  for (const rr of [R * 0.5, R * 0.78, R]) {
    ctx.beginPath(); ctx.arc(cx, cy, rr, 0, 6.283); ctx.strokeStyle = LINE + '0.28)'; ctx.stroke()
  }
  for (let k = 0; k < 48; k++) {
    const ang = k / 48 * 6.283
    const r1 = R * (k % 4 === 0 ? 0.9 : 0.95)
    ctx.beginPath()
    ctx.moveTo(cx + Math.cos(ang) * r1, cy + Math.sin(ang) * r1)
    ctx.lineTo(cx + Math.cos(ang) * R, cy + Math.sin(ang) * R)
    ctx.strokeStyle = LINE + (k % 4 === 0 ? '0.34)' : '0.18)'); ctx.stroke()
  }
  const sweep = t * 0.8
  const g = ctx.createRadialGradient(cx, cy, 0, cx, cy, R)
  g.addColorStop(0, 'rgba(70,170,220,0.22)'); g.addColorStop(1, 'rgba(70,170,220,0)')
  ctx.beginPath(); ctx.moveTo(cx, cy); ctx.arc(cx, cy, R, sweep - 0.6, sweep); ctx.closePath()
  ctx.fillStyle = g; ctx.fill()
  ctx.beginPath(); ctx.moveTo(cx, cy); ctx.lineTo(cx + Math.cos(sweep) * R, cy + Math.sin(sweep) * R)
  ctx.strokeStyle = 'rgba(50,130,190,0.5)'; ctx.lineWidth = 1.2; ctx.stroke()
}

function onMove(e: MouseEvent) {
  const el = root.value
  if (!el) return
  el.style.setProperty('--px', ((e.clientX / window.innerWidth - 0.5) * 16).toFixed(1) + 'px')
  el.style.setProperty('--py', ((e.clientY / window.innerHeight - 0.5) * 16).toFixed(1) + 'px')
}

onMounted(() => {
  const c = cv.value
  if (!c) return
  ctx = c.getContext('2d')
  resize()
  window.addEventListener('resize', resize)
  window.addEventListener('mousemove', onMove)
})
onUnmounted(() => {
  window.removeEventListener('resize', resize)
  window.removeEventListener('mousemove', onMove)
})
useRafLoop(frame, { fps: 30 }) // 不可见自动暂停 + 限 30fps
</script>

<template>
  <div class="holo" ref="root" data-tauri-drag-region>
    <div class="grid-floor"></div>
    <canvas ref="cv" class="holo-canvas"></canvas>
    <div class="glass g1"></div>
    <div class="glass g2"></div>
    <div class="sheen"></div>
    <span class="corner tl"></span><span class="corner tr"></span>
    <span class="corner bl"></span><span class="corner br"></span>
  </div>
</template>

<style scoped>
.holo {
  position: fixed; inset: 0; overflow: hidden;
  background:
    radial-gradient(65% 55% at 50% -8%, #f6fbff 0%, transparent 70%),
    linear-gradient(165deg, #e9f1fa 0%, #d6e3f1 52%, #c3d4e6 100%);
  perspective: 900px;
}

/* 柔和彩色光晕(浅青 / 浅紫 / 浅蓝) */
.holo::before {
  content: ""; position: absolute; inset: -12%; z-index: 0; pointer-events: none;
  background:
    radial-gradient(38% 32% at 20% 24%, rgba(120, 200, 235, 0.40), transparent 70%),
    radial-gradient(40% 34% at 82% 28%, rgba(170, 165, 235, 0.30), transparent 72%),
    radial-gradient(46% 40% at 62% 86%, rgba(130, 215, 210, 0.28), transparent 72%);
  filter: blur(8px);
}

/* 透视网格地板(浅青,随鼠标轻微视差) */
.grid-floor {
  position: absolute; left: -60%; right: -60%; bottom: -30%; height: 150%; z-index: 0;
  background-image:
    linear-gradient(rgba(70, 150, 205, 0.22) 1px, transparent 1px),
    linear-gradient(90deg, rgba(70, 150, 205, 0.16) 1px, transparent 1px);
  background-size: 54px 54px;
  transform: translate(var(--px, 0), var(--py, 0)) rotateX(75deg);
  transform-origin: bottom center;
  transition: transform .2s ease-out;
  -webkit-mask-image: linear-gradient(to top, rgba(0,0,0,0.85), transparent 58%);
          mask-image: linear-gradient(to top, rgba(0,0,0,0.85), transparent 58%);
  pointer-events: none;
}

.holo-canvas { position: absolute; inset: 0; z-index: 1; width: 100%; height: 100%; display: block; pointer-events: none; }

/* 漂浮磨砂玻璃面板(全息浮层) */
.glass {
  position: absolute; z-index: 2; pointer-events: none;
  border-radius: 18px;
  background: rgba(255, 255, 255, 0.28);
  border: 1px solid rgba(255, 255, 255, 0.6);
  backdrop-filter: blur(9px) saturate(1.2);
  -webkit-backdrop-filter: blur(9px) saturate(1.2);
  box-shadow: 0 18px 48px rgba(70, 110, 160, 0.16), inset 0 1px 0 rgba(255, 255, 255, 0.7);
}
.glass.g1 { left: 9%; top: 15%; width: 210px; height: 140px; transform: rotate(-4deg); }
.glass.g2 { right: 10%; top: 30%; width: 170px; height: 120px; transform: rotate(3deg); }

/* 玻璃斜向高光 */
.sheen {
  position: absolute; inset: 0; z-index: 2; pointer-events: none;
  background: linear-gradient(118deg, transparent 34%, rgba(255, 255, 255, 0.5) 48%, transparent 58%);
  filter: blur(6px);
  opacity: .5;
}

/* 四角 HUD 角标(浅底上用中青蓝) */
.corner { position: absolute; width: 24px; height: 24px; z-index: 4; pointer-events: none; border: 1.5px solid rgba(60, 140, 200, 0.5); }
.corner.tl { top: 14px; left: 14px; border-right: none; border-bottom: none; }
.corner.tr { top: 14px; right: 14px; border-left: none; border-bottom: none; }
.corner.bl { bottom: 14px; left: 14px; border-right: none; border-top: none; }
.corner.br { bottom: 14px; right: 14px; border-left: none; border-top: none; }
</style>
