<script setup lang="ts">
// 桌宠漫游:旺财在聊天区自由游走(2026-06-17 砍掉「撞气泡」交互 —— 每帧只挪自己一张图,
// 开销近乎为零)。从 MainLayout 抽出(职责干净 + 自带右键由头像承载)。
// bounds = 漫游边界容器(聊天滚动区);paused = true 时空转(不在聊天页);
// 隐藏桌宠由父层 v-if 卸载(RAF 经 useRafLoop 自动停)。形象态读 useCharacter(与头像共用)。
//
// 戏份层(2026-08-13,#10 A 层,零新美术):后台任务/思考/放歌 → 线稿 SVG 道具 + CSS 动效
// 叠加在现有帧上——搬箱子(下载/解压…)、放大镜(扫盘/看网页…)、头顶「…」泡(思考)、
// 音符+摇摆(放歌)。信号全是现成的(tasks / mood / media),解析单源 usePetActivity
// 与悬浮窗 orb 角标共用;道具与角色无关,三套形象一次全覆盖。
import { computed, ref, watch } from 'vue'
import { useRafLoop } from '../composables/useRafLoop'
import { useCharacter } from '../composables/useCharacter'
import { useChat } from '../composables/useChat'
import { useMedia } from '../composables/useMedia'
import { useTasks } from '../composables/useTasks'
import { resolveActivity, usePetDemoActivity, type PetActivity } from '../composables/usePetActivity'
import PetProp from './PetProp.vue'

const props = defineProps<{ bounds: HTMLElement | null; paused?: boolean }>()
const { pack } = useCharacter()
const chat = useChat()
const media = useMedia()
const tasks = useTasks()

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
const moving = ref(false) // 在走 or 停驻(摇摆只在停驻时,别跟步态打架)

const demoAct = usePetDemoActivity()
/** 此刻的戏份:任务(搬/查)> 思考 > 放歌;无 = null 纯遛弯。 */
const activity = computed<PetActivity | null>(
  () =>
    demoAct.value ??
    resolveActivity(
      tasks.state.tasks.filter((t) => t.state === 'running').map((t) => t.kind),
      chat.state.mood === 'thinking',
      media.state.status === 'playing',
    ),
)

/** 手上道具(箱子/放大镜):贴在行进方向前侧,朝向翻面跟随(道具随手镜像)。 */
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
/** 头顶浮标(音符/想事泡):居中悬于头上,不随翻面镜像(泡里的点镜像会看着怪)。 */
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
    moving.value = false
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
    moving.value = true
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
    <!-- body = 摇摆容器(零尺寸,原点即角色中心;groove 且停驻时轻摆) -->
    <div class="body" :class="{ sway: activity === 'groove' && !moving }">
      <img :class="{ flipped: roamerFlipped }" :src="roamerSrc" alt="" :style="{ width: pack.px + 'px' }" />
      <div v-if="activity === 'carry' || activity === 'inspect'" class="hand" :style="handStyle">
        <PetProp :activity="activity" />
      </div>
    </div>
    <div v-if="activity === 'groove' || activity === 'think'" class="head" :style="headStyle">
      <PetProp :activity="activity" />
    </div>
  </div>
</template>

<style scoped>
.roamer { position: absolute; top: 0; left: 0; z-index: 6; pointer-events: none; will-change: transform; }
.body { position: absolute; top: 0; left: 0; }
.body.sway { animation: pet-sway 1.15s ease-in-out infinite; }
.roamer img { display: block; transform: translate(-50%, -50%); }
.roamer img.flipped { transform: translate(-50%, -50%) scaleX(-1); }
.hand,
.head { position: absolute; }
@keyframes pet-sway {
  0%, 100% { transform: rotate(-4deg); }
  50% { transform: rotate(4deg); }
}
</style>
