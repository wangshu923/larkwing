<script setup lang="ts">
// 桌宠漫游:旺财在聊天区自由游走(2026-06-17 砍掉「撞气泡」交互 —— 每帧只挪自己一张图,
// 开销近乎为零)。从 MainLayout 抽出(职责干净 + 自带右键由头像承载)。
// bounds = 漫游边界容器(聊天滚动区);paused = true 时空转(不在聊天页);
// 隐藏桌宠由父层 v-if 卸载(RAF 经 useRafLoop 自动停)。形象态读 useCharacter(与头像共用)。
import { ref, watch } from 'vue'
import { useRafLoop } from '../composables/useRafLoop'
import { useCharacter } from '../composables/useCharacter'

const props = defineProps<{ bounds: HTMLElement | null; paused?: boolean }>()
const { pack } = useCharacter()

const roamer = ref<HTMLElement | null>(null)
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

function newTarget() {
  const s = props.bounds
  if (!s) return
  legFrames = 0
  // 自由游走:聊天区里随机挑个落点
  tgtX = 50 + Math.random() * Math.max(80, s.clientWidth - 110)
  tgtY = 40 + Math.random() * Math.max(80, s.clientHeight - 90)
}

function roamFrame() {
  if (props.paused || !props.bounds) return // 不在聊天页 / 容器未就绪:空转等回来
  if (!started) { newTarget(); started = true } // 惰性起步:bounds 一就绪就定第一个落点
  const dx = tgtX - dogX
  const dy = tgtY - dogY
  if (Math.hypot(dx, dy) < 6) {
    // 多帧 idle 慢速循环(每 20 帧换一帧,悬停浮动感);单帧角色等价于静止
    const idles = pack.value.idle
    roamerSrc.value = idles[Math.floor(pauseFrames / 20) % idles.length]
    roamerFlipped.value = false
    gaitTick = 0; gaitPhase = 0
    if (++pauseFrames > 45) { newTarget(); pauseFrames = 0 }
  } else {
    const dist = Math.hypot(dx, dy)
    const step = Math.min(dist * 0.04, 2.2) * ROAM_SPEED
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
      if (++gaitTick >= cp.tick / ROAM_SPEED) { gaitTick = 0; gaitPhase = (gaitPhase + 1) % cp.run.length }
      roamerSrc.value = cp.run[gaitPhase]
    }
    roamerFlipped.value = facing < 0
  }
  // 图片自身 -50% 居中,这里直接写中心点(蹲/跑画布不同大也不会跳位)。
  // ⚠️ 叠加 scrollTop:.roamer 绝对定位在 .stream(滚动容器)里,top:0 = 内容顶而非视口顶;
  // dogX/dogY 是「视口坐标」(newTarget 用 clientHeight 挑落点)→ 写入时加当前 scrollTop,
  // 桌宠才始终在**可见区**遛弯。否则会话一长它被钉在内容最上方、滚到最新 turn 就看不见了
  // (2026-07-04 真机实锤)。
  if (roamer.value) {
    const off = props.bounds ? props.bounds.scrollTop : 0
    roamer.value.style.transform = `translate(${dogX}px, ${dogY + off}px)`
  }
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
</script>

<template>
  <div class="roamer" ref="roamer">
    <img :class="{ flipped: roamerFlipped }" :src="roamerSrc" alt="" :style="{ width: pack.px + 'px' }" />
  </div>
</template>

<style scoped>
.roamer { position: absolute; top: 0; left: 0; z-index: 6; pointer-events: none; will-change: transform; }
.roamer img { display: block; transform: translate(-50%, -50%); }
.roamer img.flipped { transform: translate(-50%, -50%) scaleX(-1); }
</style>
