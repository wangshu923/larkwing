<script setup lang="ts">
// 桌宠漫游:旺财在聊天区自由游走(2026-06-17 砍掉「撞气泡」交互 —— 每帧只挪自己一张图,
// 开销近乎为零)。从 MainLayout 抽出(职责干净 + 自带右键由头像承载)。
// bounds = 漫游边界容器(聊天滚动区;只拿它量视口尺寸/算指针坐标——本体挂在它旁边的同框层上,
// 不是滚动内容,见 roamFrame 末尾的 ⚠️);paused = true 时空转(不在聊天页);
// 隐藏桌宠由父层 v-if 卸载(RAF 经 useRafLoop 自动停)。形象态读 useCharacter(与头像共用)。
//
// 戏份层(2026-08-13,#10 A 层,零新美术):后台任务/思考/放歌 → 线稿 SVG 道具 + CSS 动效
// 叠加在现有帧上——搬箱子(下载/解压…)、放大镜(扫盘/看网页…)、头顶「…」泡(思考)、
// 音符+摇摆(放歌)。信号全是现成的(tasks / mood / media),解析单源 usePetActivity
// 与悬浮窗 orb 角标共用;道具与角色无关,三套形象一次全覆盖。
//
// 行为层(2026-08-31,B1,仍零新美术):在戏份之上给它生物感 —— 空闲久了找角落睡觉、
// 想事情来回踱步、任务完工蹦两下撒星星/失败蔫掉、手机来消息叼信封跑一趟、你打字它凑到
// 输入框上方围观;还能被鼠标拎起来(松手抛物线落地压扁回弹)、点一下惊跳。
// **本组件是纯执行层**:戏份/行为在 MainLayout 决策(单实例 + 广播给悬浮窗,用户拍板
// 「同步动画状态」——两窗永不分裂),经 props 传入;这里只管把行为翻译成运动/姿态。
// 一次性姿势走 WAAPI(el.animate,连点也能重播),持续姿态走 class;帧图不动 ——
// B2 出动作帧后同一套行为直接换真姿势。戳/拖经 emit('interact') 回决策层重置睡意。
import { computed, ref, watch } from 'vue'
import { useRafLoop } from '../composables/useRafLoop'
import { useCharacter } from '../composables/useCharacter'
import { glyphOf, type PetActivity, type PropGlyph } from '../composables/usePetActivity'
import type { PetBehavior } from '../composables/usePetBehavior'
import PetProp from './PetProp.vue'

const props = defineProps<{
  bounds: HTMLElement | null
  paused?: boolean
  activity: PetActivity | null
  behavior: PetBehavior
}>()
const emit = defineEmits<{ interact: [] }>()
const { pack } = useCharacter()

const roamer = ref<HTMLElement | null>(null)
const bodyEl = ref<HTMLElement | null>(null)
let dogX = 220, dogY = 150
let tgtX = 220, tgtY = 150
let pauseFrames = 0
let facing = 1 // 1=朝右,-1=朝左
let gaitTick = 0
let gaitPhase = 0 // 步态相位:run 帧下标
let legFrames = 0 // 本段航程已飞帧数(fly 角色起步姿态用)
let started = false // bounds 就绪后才起步(避开父子挂载时序)
const ROAM_SPEED = 0.3 // 漫游速度系数(1=原速;越小越慢);同时缩放位移与步态,免"脚打滑"
const roamerSrc = ref(pack.value.idle[0])
const roamerFlipped = ref(false)
const moving = ref(false) // 在走 or 停驻(摇摆只在停驻时,别跟步态打架)

/** 戏份(道具层)与行为(运动层)都由 MainLayout 决策后传入(含 ?demo 轮播)。 */
const activity = computed(() => props.activity)
const behavior = computed(() => props.behavior)

// ─────────────────── 行为的停驻姿态与一次性表演 ───────────────────

const sleeping = ref(false) // 到达角落后才算真睡着(路上还是走路)
const watching = ref(false) // 到达输入框上方后蹲下歪头
const pokeBang = ref(false) // 被戳后头顶「!」一秒
let bangTimer = 0
/** 庆祝星星粒子(几个 span 的 CSS 动画,不上 canvas)。 */
const stars = ref<{ id: number; dx: number; dy: number; delay: number }[]>([])
let starSeq = 0

/** 一次性姿势走 WAAPI:每次都从头播(class 重复添加不重播,连点/连发也有反馈)。 */
function pose(kf: Keyframe[], ms: number, iterations = 1) {
  bodyEl.value?.animate(kf, { duration: ms, iterations, easing: 'ease-out' })
}
const hop = (times = 1) =>
  pose(
    [
      { transform: 'translateY(0)' },
      { transform: 'translateY(-13px)', offset: 0.4 },
      { transform: 'translateY(0)' },
    ],
    420,
    times,
  )
const squash = () =>
  pose(
    [
      { transform: 'scale(1.28, 0.68)' },
      { transform: 'scale(0.94, 1.08)', offset: 0.55 },
      { transform: 'scale(1, 1)' },
    ],
    340,
  )
const stretchUp = () =>
  pose(
    [
      { transform: 'scale(1, 1)' },
      { transform: 'scale(1.08, 1.16)', offset: 0.45 },
      { transform: 'scale(1, 1)' },
    ],
    520,
  )
const droop = () =>
  pose(
    [
      { transform: 'none' },
      { transform: 'scaleY(0.88) translateY(3px)', offset: 0.3 },
      { transform: 'scaleY(0.88) translateY(3px)', offset: 0.8 },
      { transform: 'none' },
    ],
    1600,
  )

function spawnBurst() {
  const n = 6
  stars.value = Array.from({ length: n }, (_, i) => {
    const a = (Math.PI * 2 * i) / n + Math.random() * 0.6
    const d = 24 + Math.random() * 16
    return { id: starSeq++, dx: Math.cos(a) * d, dy: Math.sin(a) * d - 10, delay: Math.random() * 140 }
  })
  window.setTimeout(() => (stars.value = []), 1100)
}

// ───────────────────── 拖拽物理 + 戳一戳 ─────────────────────
// 只有宠物本体开 pointer-events(几十 px,不挡选字):拎起来跟手 + 随水平速度摆,
// 松手抛物线落地压扁回弹;快速点一下(没拖动)= 惊跳回头。

const dragging = ref(false)
const dragRotate = ref(0)
let pointerPending = false // 按下了、还没确定是拖是点
let downAt = 0
let downX = 0, downY = 0
let grabDx = 0, grabDy = 0 // 抓点相对角色中心的偏移(拎哪儿跟哪儿)
let lastPx = 0, lastPy = 0, lastPt = 0 // 速度采样
let velX = 0, velY = 0
let falling = false
const GRAVITY = 0.55
const FLOOR_MARGIN = 34

/** pointer 事件坐标 → bounds 视口坐标(= dogX/dogY 的坐标系;.roamer 的舞台层与 bounds 同框同原点,渲染直接用)。 */
function toLocal(e: PointerEvent): { x: number; y: number } {
  const r = props.bounds?.getBoundingClientRect()
  return r ? { x: e.clientX - r.left, y: e.clientY - r.top } : { x: e.clientX, y: e.clientY }
}

function onPointerDown(e: PointerEvent) {
  if (e.button !== 0 || !props.bounds) return
  emit('interact') // 有人动它:回决策层重置睡意(伸懒腰在 behavior 离开 sleep 时播)
  pointerPending = true
  downAt = performance.now()
  downX = e.clientX
  downY = e.clientY
  const p = toLocal(e)
  grabDx = dogX - p.x
  grabDy = dogY - p.y
  lastPx = p.x; lastPy = p.y; lastPt = downAt
  velX = 0; velY = 0
  try {
    ;(e.currentTarget as HTMLElement).setPointerCapture(e.pointerId)
  } catch {
    // 指针已失效(极快的 down→cancel)/ 合成事件:抓不住就不抓,拖拽退化成戳,无害
  }
  e.preventDefault()
}

function onPointerMove(e: PointerEvent) {
  if (!pointerPending && !dragging.value) return
  if (pointerPending && Math.hypot(e.clientX - downX, e.clientY - downY) > 5) {
    pointerPending = false
    dragging.value = true
    falling = false
  }
  if (!dragging.value) return
  const p = toLocal(e)
  const now = performance.now()
  const dt = Math.max(now - lastPt, 1)
  // 速度采样(px/帧,按 60fps 折算)+ 低通,拿来做拎着晃的摆角与松手初速
  velX = velX * 0.75 + (((p.x - lastPx) / dt) * 16.7) * 0.25
  velY = velY * 0.75 + (((p.y - lastPy) / dt) * 16.7) * 0.25
  lastPx = p.x; lastPy = p.y; lastPt = now
  dogX = p.x + grabDx
  dogY = p.y + grabDy
  dragRotate.value = Math.max(-24, Math.min(24, velX * 2.2))
}

function onPointerUp(e: PointerEvent) {
  if (dragging.value) {
    dragging.value = false
    dragRotate.value = 0
    falling = true // 松手:带着甩出去的初速做抛物线,roamFrame 里落地
    return
  }
  if (pointerPending && performance.now() - downAt < 300) {
    // 快速点一下 = 戳:惊跳 + 头顶「!」
    hop()
    pokeBang.value = true
    clearTimeout(bangTimer)
    bangTimer = window.setTimeout(() => (pokeBang.value = false), 900)
  }
  pointerPending = false
  void e
}

// ───────────────────── 行为 → 目标点策略 ─────────────────────

let paceAnchorX = 0
let paceDir = 1

/** 行为切换:进/出时定目标点、播转场姿势。运动本体仍在 roamFrame。 */
watch(behavior, (nb, ob) => {
  if (ob === 'sleep') {
    sleeping.value = false
    stretchUp() // 醒来伸个懒腰
  }
  if (ob === 'watch') watching.value = false
  const s = props.bounds
  switch (nb) {
    case 'sleep': {
      if (!s) break
      // 就近找地方趴:朝最近的角落挪,但至多走 120px(困了不横穿全屏赶路,也让睡姿快点看到)
      const w = s.clientWidth, h = s.clientHeight
      const corners = [
        { x: 64, y: 64 },
        { x: Math.max(64, w - 64), y: 64 },
        { x: 64, y: Math.max(64, h - 72) },
        { x: Math.max(64, w - 64), y: Math.max(64, h - 72) },
      ]
      corners.sort((a, b) => Math.hypot(a.x - dogX, a.y - dogY) - Math.hypot(b.x - dogX, b.y - dogY))
      const c = corners[0]
      const d = Math.hypot(c.x - dogX, c.y - dogY)
      const k = d > 120 ? 120 / d : 1
      tgtX = dogX + (c.x - dogX) * k
      tgtY = dogY + (c.y - dogY) * k
      break
    }
    case 'pace':
      paceAnchorX = dogX
      paceDir = 1
      tgtX = dogX + 40
      tgtY = dogY
      break
    case 'watch':
      if (!s) break
      // 输入框在聊天区下缘:走到底边中央蹲着看你写(极矮容器〔预览空会话实锤 clientHeight
      // 只有几十 px〕y 会算成负 → 兜底钳回,别把桌宠送出天花板)
      tgtX = s.clientWidth * 0.5 + (Math.random() * 120 - 60)
      tgtY = Math.max(40, s.clientHeight - 44)
      break
    case 'deliver':
      if (!s) break
      // 会话列表在左侧 rail:叼着信封往左下跑一趟
      tgtX = 36
      tgtY = Math.max(40, s.clientHeight - 56)
      break
    case 'celebrate':
      tgtX = dogX
      tgtY = dogY
      hop(2)
      spawnBurst()
      break
    case 'dazed':
      tgtX = dogX
      tgtY = dogY
      droop()
      break
    default:
      if (started) newTarget()
  }
})

function newTarget() {
  const s = props.bounds
  if (!s) return
  legFrames = 0
  // 自由游走:聊天区里随机挑个落点
  tgtX = 50 + Math.random() * Math.max(80, s.clientWidth - 110)
  tgtY = 40 + Math.random() * Math.max(80, s.clientHeight - 90)
}

/** 停驻时到底摆什么姿势/换不换目标:按行为分派(睡着/围观不再走;踱步到点折返)。 */
function onArrived() {
  const idles = pack.value.idle
  roamerSrc.value = idles[Math.floor(pauseFrames / 20) % idles.length]
  roamerFlipped.value = false
  moving.value = false
  gaitTick = 0
  gaitPhase = 0
  pauseFrames++
  switch (behavior.value) {
    case 'sleep':
      sleeping.value = true // 趴下:呼吸 class + Zzz 浮标接管,不再挪窝
      return
    case 'watch':
      watching.value = true // 蹲在输入框上方歪头看
      return
    case 'deliver':
      return // 信送到了:拿着信封原地等行为窗过期(几秒),别叼着乱跑
    case 'celebrate':
    case 'dazed':
      return // 一次性表演在原地(watch(behavior) 已触发),别走动
    case 'pace':
      if (pauseFrames > 8) {
        paceDir = -paceDir
        tgtX = paceAnchorX + 40 * paceDir
        tgtY = dogY
        pauseFrames = 0
      }
      return
    default:
      if (pauseFrames > 45) {
        newTarget()
        pauseFrames = 0
      }
  }
}

function roamFrame() {
  if (props.paused || !props.bounds) return // 不在聊天页 / 容器未就绪:空转等回来
  if (!started) { newTarget(); started = true } // 惰性起步:bounds 一就绪就定第一个落点

  if (dragging.value) {
    // 拎在手上:位置随 pointermove 写,这里只管姿态帧
    roamerSrc.value = pack.value.idle[0]
    moving.value = false
  } else if (falling) {
    // 松手:抛物线落地(地面 = 聊天区下缘),落地压扁回弹
    velY += GRAVITY
    dogX += velX
    dogY += velY
    const s = props.bounds
    const floor = Math.max(60, s.clientHeight - FLOOR_MARGIN) // 极矮容器兜底,别落到负坐标
    if (dogX < 30) { dogX = 30; velX = Math.abs(velX) * 0.4 }
    if (dogX > s.clientWidth - 30) { dogX = s.clientWidth - 30; velX = -Math.abs(velX) * 0.4 }
    if (dogY >= floor) {
      dogY = floor
      falling = false
      squash()
      tgtX = dogX
      tgtY = dogY
      pauseFrames = 0
    }
    roamerSrc.value = pack.value.idle[0]
    moving.value = false
  } else {
    const dx = tgtX - dogX
    const dy = tgtY - dogY
    if (Math.hypot(dx, dy) < 6) {
      onArrived()
    } else {
      const dist = Math.hypot(dx, dy)
      // 有目的地的走路都比闲逛快:送信小跑 > 去围观 > 找地方睡;踱步是小碎步,慢
      const pace =
        behavior.value === 'deliver' ? 1.8
        : behavior.value === 'watch' ? 1.6
        : behavior.value === 'sleep' ? 1.4
        : behavior.value === 'pace' ? 0.6
        : 1
      const step = Math.min(dist * 0.04, 2.2) * ROAM_SPEED * pace
      dogX += (dx / dist) * step
      dogY += (dy / dist) * step
      if (Math.abs(dx) > 1) facing = dx >= 0 ? 1 : -1
      const cp = pack.value
      if (cp.fly) {
        // 飞行:整机倾角不能快轮(会抽搐),按航段选帧——临近收势 > 起步前倾 > 巡航两帧慢摆
        legFrames++
        if (dist < 70) { roamerSrc.value = cp.run[3] }
        else if (legFrames < 26) { roamerSrc.value = cp.run[0] }
        else {
          if (++gaitTick >= 24 / ROAM_SPEED) { gaitTick = 0; gaitPhase ^= 1 }
          roamerSrc.value = cp.run[1 + (gaitPhase & 1)]
        }
      } else {
        if (++gaitTick >= cp.tick / (ROAM_SPEED * pace)) { gaitTick = 0; gaitPhase = (gaitPhase + 1) % cp.run.length }
        roamerSrc.value = cp.run[gaitPhase]
      }
      roamerFlipped.value = facing < 0
      moving.value = true
      sleeping.value = false
      watching.value = false
    }
  }
  // 图片自身 -50% 居中,这里直接写中心点(蹲/跑画布不同大也不会跳位)。
  // .roamer 悬在滚动区**外**的同框层(MainLayout 的 .stream-wrap)上,dogX/dogY 就是视口坐标,直接写。
  // ⚠️ 别再把它挂回 .stream 里、每帧加 scrollTop 补偿(2026-07-04 → 2026-09-04 的老做法):
  // 绝对定位 + transform 的盒子会算进滚动容器的可滚动范围;桌宠站到底边附近时,看不见的姿态盒
  // (.body 被 img 撑成 px×px、从原点向下延伸半个身位)探出视口 → 「贴底滚动」把 scrollTop 推下去
  // → 下一帧桌宠随 scrollTop 再探出 → 回合在飞时每条思考增量触发一次贴底 = 聊天流无休止滚进空白
  // (2026-09-04 真机实锤;预览 1:1 复现:24 条增量 scrollTop 663→844)。
  if (roamer.value) roamer.value.style.transform = `translate(${dogX}px, ${dogY}px)`
}

// 换形象:重置步态 + **立即换成新角色静止帧**(不等下一帧;rAF 万一没在跑也立刻反映切换,
// 免「切了没反应」——roamerSrc 平时只在 roamFrame 里更新)。
watch(pack, () => {
  gaitTick = 0
  gaitPhase = 0
  roamerSrc.value = pack.value.idle[0]
  roamerFlipped.value = false
})

useRafLoop(roamFrame) // 页面不可见(藏托盘/最小化)时自动暂停遛弯循环

// ───────────────────── 渲染:道具/浮标/粒子 ─────────────────────

/** 手上道具:送信 > 搬/查;睡着/被拎着收道具(手上还举着箱子睡觉太怪)。 */
const handGlyph = computed<PropGlyph | null>(() => {
  if (behavior.value === 'deliver') return 'mail'
  if (sleeping.value || dragging.value) return null
  if (activity.value === 'carry' || activity.value === 'inspect') return glyphOf(activity.value)
  return null
})
/** 头顶浮标:被戳「!」 > 蔫「?」 > 睡 Zzz > 思考「…」 > 放歌音符。 */
const headGlyph = computed<PropGlyph | null>(() => {
  if (pokeBang.value) return 'bang'
  if (behavior.value === 'dazed') return 'question'
  if (sleeping.value) return 'zzz'
  if (activity.value === 'think') return 'dots'
  if (activity.value === 'groove') return 'note'
  return null
})

/** 手上道具(箱子/放大镜/信封):贴在行进方向前侧,朝向翻面跟随(道具随手镜像)。 */
const handStyle = computed(() => {
  const px = pack.value.px
  const s = Math.round(px * 0.42)
  const x = roamerFlipped.value ? -Math.round(px * 0.34) - s : Math.round(px * 0.34)
  return {
    width: `${s}px`,
    height: `${s}px`,
    left: `${x}px`,
    top: `${Math.round(px * -0.04)}px`,
    transform: roamerFlipped.value ? 'scaleX(-1)' : undefined,
  }
})
/** 头顶浮标(音符/想事泡/Zzz/!/?):居中悬于头上,不随翻面镜像(泡里的点镜像会看着怪)。 */
const headStyle = computed(() => {
  const px = pack.value.px
  const s = Math.round(px * 0.44)
  return {
    width: `${s}px`,
    height: `${s}px`,
    left: `${-Math.round(s / 2)}px`,
    top: `${-Math.round(px * 0.78)}px`,
  }
})
</script>

<template>
  <div class="roamer" ref="roamer">
    <!-- body = 姿态容器(原点即角色中心;注意它**不是**零尺寸——被 block 的 img 撑成 px×px、
         从原点向右下延伸,img 再 -50% 挪回居中,所以看不见的盒子挂在形象下方半个身位):
         停驻摇摆(放歌)/呼吸(睡)/歪头(围观)走 class;蹦跳/压扁/伸懒腰/蔫这类一次性姿势走 WAAPI;
         拎着时随甩动速度摆(inline)。 -->
    <div
      ref="bodyEl"
      class="body"
      :class="{
        sway: activity === 'groove' && !moving && !dragging && !sleeping && !watching,
        breathe: sleeping,
        tilt: watching && !moving,
        grabbed: dragging,
      }"
      :style="dragging ? { transform: `rotate(${dragRotate}deg)` } : undefined"
    >
      <img
        :class="{ flipped: roamerFlipped }"
        :src="roamerSrc"
        alt=""
        draggable="false"
        :style="{ width: pack.px + 'px' }"
        @pointerdown="onPointerDown"
        @pointermove="onPointerMove"
        @pointerup="onPointerUp"
        @pointercancel="onPointerUp"
      />
      <div v-if="handGlyph" class="hand" :style="handStyle">
        <PetProp :glyph="handGlyph" />
      </div>
      <!-- 庆祝星星:几个 span 放射即收(CSS 变量定向,不上 canvas) -->
      <span
        v-for="s in stars"
        :key="s.id"
        class="star"
        :style="{ '--dx': s.dx + 'px', '--dy': s.dy + 'px', animationDelay: s.delay + 'ms' }"
        >✦</span
      >
    </div>
    <div v-if="headGlyph" class="head" :style="headStyle">
      <PetProp :glyph="headGlyph" />
    </div>
  </div>
</template>

<style scoped>
.roamer { position: absolute; top: 0; left: 0; z-index: 6; pointer-events: none; will-change: transform; }
.body { position: absolute; top: 0; left: 0; }
.body.sway { animation: pet-sway 1.15s ease-in-out infinite; }
.body.breathe { animation: pet-breathe 2.4s ease-in-out infinite; }
.body.tilt { transform: rotate(9deg); transition: transform 0.3s ease; }
.roamer img {
  display: block;
  transform: translate(-50%, -50%);
  pointer-events: auto; /* 只有本体可交互:拎/戳;周围仍点穿,不挡选字 */
  cursor: grab;
  touch-action: none;
  user-select: none;
  -webkit-user-drag: none;
}
.body.grabbed img { cursor: grabbing; }
.roamer img.flipped { transform: translate(-50%, -50%) scaleX(-1); }
.hand,
.head { position: absolute; }
.star {
  position: absolute;
  left: -5px;
  top: -8px;
  font-size: 11px;
  color: var(--accent);
  opacity: 0;
  pointer-events: none;
  animation: pet-star 0.9s ease-out forwards;
}
@keyframes pet-sway {
  0%, 100% { transform: rotate(-4deg); }
  50% { transform: rotate(4deg); }
}
@keyframes pet-breathe {
  0%, 100% { transform: scale(1, 1); }
  50% { transform: scale(1.03, 0.96); }
}
@keyframes pet-star {
  0% { transform: translate(0, 0) scale(0.5); opacity: 1; }
  100% { transform: translate(var(--dx), var(--dy)) scale(1.1); opacity: 0; }
}
</style>
