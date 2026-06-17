<script setup lang="ts">
import { onMounted, onUnmounted, ref } from 'vue'
import { useRafLoop } from '../composables/useRafLoop'

// 深蓝霓虹辉光科幻背景:深蓝底 + 发光透视网格/地平线 + 饱和辉光光晕(CSS)
//   + 发光数据粒子连线 + 右下角旋转霓虹弧(canvas)。中心留白,给旺财让位。
defineProps<{ booting?: boolean }>()

const cv = ref<HTMLCanvasElement | null>(null)
const root = ref<HTMLElement | null>(null)
let ctx: CanvasRenderingContext2D | null = null
let w = 0, h = 0, dpr = 1, last = 0, t = 0

interface Node { x: number; y: number; vx: number; vy: number; c: string }
let nodes: Node[] = []

const NEON = ['rgba(95,210,255,', 'rgba(95,210,255,', 'rgba(120,160,255,', 'rgba(180,120,245,']

function build() {
  nodes = []
  const nc = Math.round((w * h) / 17000) + 16
  for (let i = 0; i < nc; i++)
    nodes.push({
      x: Math.random() * w, y: Math.random() * h,
      vx: (Math.random() - 0.5) * 11, vy: (Math.random() - 0.5) * 11,
      c: NEON[Math.floor(Math.random() * NEON.length)],
    })
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

  // 数据节点 + 连线(2026-06-17 去掉 shadowBlur 辉光:省 CPU;氛围靠 CSS 底光撑)
  for (const n of nodes) {
    n.x += n.vx * dt; n.y += n.vy * dt
    if (n.x < 0) n.x += w; if (n.x > w) n.x -= w
    if (n.y < 0) n.y += h; if (n.y > h) n.y -= h
  }
  const D = 150
  ctx.lineWidth = 1
  for (let i = 0; i < nodes.length; i++) {
    for (let j = i + 1; j < nodes.length; j++) {
      const a = nodes[i], b = nodes[j]
      const d = Math.hypot(a.x - b.x, a.y - b.y)
      if (d < D) {
        ctx.beginPath(); ctx.moveTo(a.x, a.y); ctx.lineTo(b.x, b.y)
        ctx.strokeStyle = 'rgba(95,200,255,' + (0.16 * (1 - d / D)).toFixed(3) + ')'; ctx.stroke()
      }
    }
  }
  for (const n of nodes) {
    ctx.beginPath(); ctx.arc(n.x, n.y, 1.7, 0, 6.283)
    ctx.fillStyle = n.c + '0.9)'; ctx.fill()
  }

  // 居中弧组(科技装饰,旋转)
  const cx = w / 2, cy = h / 2
  for (let r = 0; r < 3; r++) {
    const rad = 70 + r * 26
    const a0 = t * (0.3 + r * 0.12) + r
    const a1 = a0 + Math.PI * (0.7 - r * 0.12)
    ctx.beginPath(); ctx.arc(cx, cy, rad, a0, a1)
    ctx.strokeStyle = r === 1 ? 'rgba(180,120,245,0.55)' : 'rgba(95,210,255,0.5)'
    ctx.lineWidth = 1.6; ctx.stroke()
  }
}

function onMove(e: MouseEvent) {
  const el = root.value
  if (!el) return
  el.style.setProperty('--px', ((e.clientX / window.innerWidth - 0.5) * 22).toFixed(1) + 'px')
  el.style.setProperty('--py', ((e.clientY / window.innerHeight - 0.5) * 22).toFixed(1) + 'px')
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
useRafLoop(frame, { fps: 30 }) // 不可见自动暂停 + 限 30fps(氛围背景肉眼无差,绘制/续航砍半)
</script>

<template>
  <div class="neon" ref="root" data-tauri-drag-region :class="{ booting }">
    <div class="grid-floor"></div>
    <div class="boot-sweep"></div>
    <div class="horizon"></div>
    <div class="scan"></div>
    <canvas ref="cv" class="neon-canvas"></canvas>
    <span class="corner tl"></span><span class="corner tr"></span>
    <span class="corner bl"></span><span class="corner br"></span>
  </div>
</template>

<style scoped>
.neon {
  position: fixed; inset: 0; overflow: hidden;
  border-radius: 10px;
  /* 深蓝 navy:够深以衬托辉光,但带蓝、不死黑 */
  background: radial-gradient(125% 95% at 50% -8%, #15315c 0%, #0c2042 38%, #071528 70%, #040d1c 100%);
  perspective: 1000px;
}

/* 饱和辉光光晕(在深底上"发光") */
.neon::before {
  content: ""; position: absolute; inset: -12%; z-index: 0; pointer-events: none;
  background:
    radial-gradient(38% 32% at 16% 24%, rgba(40, 150, 230, 0.45), transparent 66%),
    radial-gradient(36% 30% at 86% 20%, rgba(150, 90, 235, 0.34), transparent 68%),
    radial-gradient(52% 44% at 60% 96%, rgba(40, 210, 200, 0.30), transparent 70%);
}

/* 发光透视网格地板(随鼠标视差) */
.grid-floor {
  position: absolute; left: -60%; right: -60%; bottom: -26%; height: 150%; z-index: 1;
  background-image:
    linear-gradient(rgba(95, 220, 255, 0.45) 1px, transparent 1px),
    linear-gradient(90deg, rgba(95, 220, 255, 0.30) 1px, transparent 1px);
  background-size: 56px 56px;
  transform: translate(var(--px, 0), var(--py, 0)) rotateX(76deg);
  transform-origin: bottom center;
  transition: transform .2s ease-out;
  filter: drop-shadow(0 0 4px rgba(80, 200, 255, 0.45));
  -webkit-mask-image: linear-gradient(to top, rgba(0,0,0,0.95), transparent 60%);
          mask-image: linear-gradient(to top, rgba(0,0,0,0.95), transparent 60%);
  pointer-events: none;
}

/* 地平辉光(融入式,替代之前割裂的硬亮线) */
.horizon {
  position: absolute; left: -10%; right: -10%; top: 50%; height: 150px; z-index: 0; pointer-events: none;
  background: radial-gradient(58% 100% at 50% 50%, rgba(85, 205, 255, 0.20), transparent 72%);
  filter: blur(12px);
}

/* 缓慢扫描带 */
.scan {
  position: absolute; left: 0; right: 0; top: -30%; height: 30%; z-index: 2; pointer-events: none;
  background: linear-gradient(180deg, transparent, rgba(120, 210, 255, 0.06), transparent);
  animation: neon-scan 9s linear infinite;
}
@keyframes neon-scan { to { transform: translateY(460%); } }

.neon-canvas { position: absolute; inset: 0; z-index: 3; width: 100%; height: 100%; display: block; pointer-events: none; }

/* 发光角标 */
.corner { position: absolute; width: 26px; height: 26px; z-index: 4; pointer-events: none; border: 1.5px solid rgba(110, 215, 255, 0.6); filter: drop-shadow(0 0 3px rgba(95, 205, 255, 0.6)); }
.corner.tl { top: 14px; left: 14px; border-right: none; border-bottom: none; }
.corner.tr { top: 14px; right: 14px; border-left: none; border-bottom: none; }
.corner.bl { bottom: 14px; left: 14px; border-right: none; border-top: none; }
.corner.br { bottom: 14px; right: 14px; border-left: none; border-top: none; }

/* —— 启动入场(仅 .booting 时播,各元素错峰;最终态 = 默认态) —— */
.boot-sweep { position: absolute; inset: 0; z-index: 5; pointer-events: none; opacity: 0; }
.neon.booting .boot-sweep {
  background: linear-gradient(180deg, transparent, rgba(130, 220, 255, 0.12), transparent);
  animation: bootSweep 1.1s ease-in 0.9s backwards;
}
.neon.booting::before { animation: bootFade .55s ease-out backwards; }
.neon.booting .grid-floor { animation: bootFade .8s ease-out .25s backwards; }
.neon.booting .horizon { animation: bootFade .7s ease-out .4s backwards; }
.neon.booting .neon-canvas { animation: bootFade .9s ease-out .75s backwards; }
.neon.booting .corner { animation: bootPop .45s cubic-bezier(.2, .8, .3, 1.25) backwards; }
.neon.booting .corner.tl { animation-delay: .55s; }
.neon.booting .corner.tr { animation-delay: .65s; }
.neon.booting .corner.bl { animation-delay: .65s; }
.neon.booting .corner.br { animation-delay: .75s; }

@keyframes bootFade { from { opacity: 0; } }
@keyframes bootPop { from { opacity: 0; transform: scale(.5); } }
@keyframes bootSweep {
  0% { opacity: 0; transform: translateY(-100%); }
  30% { opacity: 1; }
  100% { opacity: 0; transform: translateY(100%); }
}
</style>
