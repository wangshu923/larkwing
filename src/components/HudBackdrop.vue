<script setup lang="ts">
import { ref, onMounted, onUnmounted } from 'vue'

// 科幻 HUD 背景:透视网格(CSS) + 雷达环/扫描臂/数据节点(canvas) + 扫描线/角标(CSS)。
const cv = ref<HTMLCanvasElement | null>(null)
const root = ref<HTMLElement | null>(null)
let ctx: CanvasRenderingContext2D | null = null
let raf = 0
let w = 0, h = 0, dpr = 1, last = 0, t = 0

interface Node { x: number; y: number; vx: number; vy: number }
let nodes: Node[] = []
let stars: { x: number; y: number; r: number; a: number }[] = []

const CY = 'rgba(95,210,200,'   // 青
const BL = 'rgba(111,180,230,'  // 蓝

function build() {
  nodes = []
  const nc = Math.round((w * h) / 16000) + 18
  for (let i = 0; i < nc; i++)
    nodes.push({ x: Math.random() * w, y: Math.random() * h, vx: (Math.random() - 0.5) * 12, vy: (Math.random() - 0.5) * 12 })
  stars = []
  const sc = Math.round((w * h) / 6000)
  for (let i = 0; i < sc; i++)
    stars.push({ x: Math.random() * w, y: Math.random() * h, r: Math.random() * 1.1 + 0.2, a: Math.random() * 0.4 + 0.15 })
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

  // 底层稀疏星点
  for (const s of stars) {
    ctx.beginPath(); ctx.arc(s.x, s.y, s.r, 0, 6.283)
    ctx.fillStyle = 'rgba(200,225,255,' + s.a.toFixed(3) + ')'; ctx.fill()
  }

  // 数据节点漂移 + 临近连线(网络感)
  for (const n of nodes) {
    n.x += n.vx * dt; n.y += n.vy * dt
    if (n.x < 0) n.x += w; if (n.x > w) n.x -= w
    if (n.y < 0) n.y += h; if (n.y > h) n.y -= h
  }
  const D = 130
  for (let i = 0; i < nodes.length; i++) {
    for (let j = i + 1; j < nodes.length; j++) {
      const a = nodes[i], b = nodes[j]
      const d = Math.hypot(a.x - b.x, a.y - b.y)
      if (d < D) {
        ctx.beginPath(); ctx.moveTo(a.x, a.y); ctx.lineTo(b.x, b.y)
        ctx.strokeStyle = CY + (0.12 * (1 - d / D)).toFixed(3) + ')'; ctx.lineWidth = 1; ctx.stroke()
      }
    }
  }
  for (const n of nodes) {
    ctx.beginPath(); ctx.arc(n.x, n.y, 1.4, 0, 6.283)
    ctx.fillStyle = CY + '0.55)'; ctx.fill()
  }

  // —— 中心雷达 HUD ——
  const cx = w / 2, cy = h / 2
  const R = Math.min(w, h) * 0.34
  ctx.lineWidth = 1
  for (const rr of [R * 0.45, R * 0.72, R]) {
    ctx.beginPath(); ctx.arc(cx, cy, rr, 0, 6.283); ctx.strokeStyle = BL + '0.16)'; ctx.stroke()
  }
  // 外环刻度
  for (let k = 0; k < 72; k++) {
    const ang = k / 72 * 6.283
    const r1 = R * (k % 6 === 0 ? 0.94 : 0.97)
    ctx.beginPath()
    ctx.moveTo(cx + Math.cos(ang) * r1, cy + Math.sin(ang) * r1)
    ctx.lineTo(cx + Math.cos(ang) * R, cy + Math.sin(ang) * R)
    ctx.strokeStyle = BL + (k % 6 === 0 ? '0.30)' : '0.15)'); ctx.stroke()
  }
  // 十字准星
  ctx.strokeStyle = CY + '0.20)'
  ctx.beginPath()
  ctx.moveTo(cx - R * 1.12, cy); ctx.lineTo(cx + R * 1.12, cy)
  ctx.moveTo(cx, cy - R * 1.12); ctx.lineTo(cx, cy + R * 1.12)
  ctx.stroke()
  // 旋转扫描臂 + 余晖扇形
  const sweep = t * 0.7
  const g = ctx.createRadialGradient(cx, cy, 0, cx, cy, R)
  g.addColorStop(0, CY + '0.20)'); g.addColorStop(1, CY + '0)')
  ctx.beginPath(); ctx.moveTo(cx, cy); ctx.arc(cx, cy, R, sweep - 0.55, sweep); ctx.closePath()
  ctx.fillStyle = g; ctx.fill()
  ctx.beginPath(); ctx.moveTo(cx, cy); ctx.lineTo(cx + Math.cos(sweep) * R, cy + Math.sin(sweep) * R)
  ctx.strokeStyle = 'rgba(150,240,230,0.55)'; ctx.lineWidth = 1.5; ctx.stroke()

  raf = requestAnimationFrame(frame)
}

function onMove(e: MouseEvent) {
  const el = root.value
  if (!el) return
  el.style.setProperty('--px', ((e.clientX / window.innerWidth - 0.5) * 18).toFixed(1) + 'px')
  el.style.setProperty('--py', ((e.clientY / window.innerHeight - 0.5) * 18).toFixed(1) + 'px')
}

onMounted(() => {
  const c = cv.value
  if (!c) return
  ctx = c.getContext('2d')
  resize()
  window.addEventListener('resize', resize)
  window.addEventListener('mousemove', onMove)
  raf = requestAnimationFrame(frame)
})

onUnmounted(() => {
  cancelAnimationFrame(raf)
  window.removeEventListener('resize', resize)
  window.removeEventListener('mousemove', onMove)
})
</script>

<template>
  <div class="hud" ref="root" data-tauri-drag-region>
    <div class="grid-floor"></div>
    <canvas ref="cv" class="hud-canvas"></canvas>
    <div class="scanlines"></div>
    <span class="corner tl"></span><span class="corner tr"></span>
    <span class="corner bl"></span><span class="corner br"></span>
  </div>
</template>

<style scoped>
.hud {
  position: fixed; inset: 0; overflow: hidden;
  background: radial-gradient(135% 110% at 50% 0%, #082636 0%, #04111e 46%, #02070f 100%);
  perspective: 900px;
}

/* 透视网格地板(随鼠标轻微视差) */
.grid-floor {
  position: absolute; left: -60%; right: -60%; bottom: -28%; height: 150%;
  background-image:
    linear-gradient(rgba(95, 210, 235, 0.30) 1px, transparent 1px),
    linear-gradient(90deg, rgba(95, 210, 235, 0.22) 1px, transparent 1px);
  background-size: 52px 52px;
  transform: translate(var(--px, 0), var(--py, 0)) rotateX(74deg);
  transform-origin: bottom center;
  transition: transform .2s ease-out;
  -webkit-mask-image: linear-gradient(to top, rgba(0,0,0,0.9), transparent 60%);
          mask-image: linear-gradient(to top, rgba(0,0,0,0.9), transparent 60%);
  pointer-events: none;
}

.hud-canvas { position: absolute; inset: 0; z-index: 1; width: 100%; height: 100%; display: block; pointer-events: none; }

/* 扫描线(CRT 质感) */
.scanlines {
  position: absolute; inset: 0; z-index: 3; pointer-events: none;
  background: repeating-linear-gradient(0deg, rgba(0,0,0,0) 0 2px, rgba(2,18,26,0.22) 3px);
  opacity: .55;
}

/* 四角 HUD 角标 */
.corner { position: absolute; width: 26px; height: 26px; z-index: 4; pointer-events: none; border: 1.5px solid rgba(95,200,235,0.45); }
.corner.tl { top: 14px; left: 14px; border-right: none; border-bottom: none; }
.corner.tr { top: 14px; right: 14px; border-left: none; border-bottom: none; }
.corner.bl { bottom: 14px; left: 14px; border-right: none; border-top: none; }
.corner.br { bottom: 14px; right: 14px; border-left: none; border-top: none; }
</style>
