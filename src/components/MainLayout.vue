<script setup lang="ts">
import { ref, computed, onMounted, onUnmounted, nextTick, watch } from 'vue'
import dogIdle from '../assets/dog-idle.png'
import dogRun1 from '../assets/dog-run-1.png'
import dogRun2 from '../assets/dog-run-2.png'
import dogRun3 from '../assets/dog-run-3.png'
import dogRun4 from '../assets/dog-run-4.png'
import dogRun5 from '../assets/dog-run-5.png'
import catIdle from '../assets/cat-idle.png'
import catRun1 from '../assets/cat-run-1.png'
import catRun2 from '../assets/cat-run-2.png'
import catRun3 from '../assets/cat-run-3.png'
import catRun4 from '../assets/cat-run-4.png'
import catRun5 from '../assets/cat-run-5.png'
import titanIdle1 from '../assets/titan-idle-1.png'
import titanIdle2 from '../assets/titan-idle-2.png'
import titanRun1 from '../assets/titan-run-1.png'
import titanRun2 from '../assets/titan-run-2.png'
import titanRun3 from '../assets/titan-run-3.png'
import titanRun4 from '../assets/titan-run-4.png'
import { useI18n } from 'vue-i18n'
import { useChat, type TurnStats, type UiMessage } from '../composables/useChat'
import { useSettings } from '../composables/useSettings'
import { onTranscribed, useVoice } from '../composables/useVoice'
import { useSpeech } from '../composables/useSpeech'
import { fmtMs, fmtTokens, fmtUsd } from '../lib/fmt'
import MemoryView from '../views/MemoryView.vue'
import SettingsView from '../views/SettingsView.vue'
import PlayerBar from './PlayerBar.vue'
import UsageStrip from './UsageStrip.vue'

// 主界面骨架。数据源 = useChat(VM):Tauri 壳里走真 IPC,纯浏览器预览自动降级假数据。
defineProps<{ booting?: boolean }>()

const { t, te } = useI18n()

const settings = useSettings()
const panelOpen = ref(true)
const shape = computed(() => (settings.get('ui.bubble_shape') === 'cut' ? 'cut' : 'round'))
const petName = computed(() => settings.get('ui.pet_name') || t('pet.name'))
const textScale = computed(() => (settings.get('ui.text_scale') === 'large' ? '16.5px' : '14px'))
const activeRail = ref<'chat' | 'scenes' | 'memory' | 'settings'>('chat')

const { state: chat, send: chatSend, cancel, selectConversation, newConversation, saveApiKey } = useChat()
const messages = computed(() => chat.messages)

const input = ref('')
function send() {
  const t = input.value.trim()
  if (!t) return
  chatSend(t) // 流中再发 = 自动取消旧回合(engine 保证 partial 先落库)
  input.value = ''
}

// —— 语音(PLAN §11):听写(mic,UI 交互不念)与唤醒(wake,语音会话必念)
//    都从这里进既有 send 链;via 决定回合形态(二分纪律) ——
const voice = useVoice()
onTranscribed((text, via, speaker) => chatSend(text, via, speaker))

// —— 朗读(B 期):状态/停念/重听;正在念时点麦克风 = 停念+开听(一步打断) ——
const speech = useSpeech()
function micToggle() {
  if (speech.state.playing) speech.abort()
  voice.toggle()
}
function replay(text: string) {
  speech.speakText(text)
}

// 波形:9 根柱,电平驱动,固定相位差(纯视觉,不求频谱真实)
const BAR_PHASE = [0.55, 0.85, 0.65, 1, 0.75, 0.95, 0.6, 0.8, 0.5]
const bars = computed(() => BAR_PHASE.map((p) => 12 + Math.round(voice.state.level * p * 88)))

const listenHint = computed(() =>
  voice.state.phase === 'preparing' ? t('voice.preparing')
  : voice.state.phase === 'transcribing' ? t('voice.transcribing')
  : voice.state.heard ? t('voice.heard')
  : t('voice.listening')
)
// 没听清的轻提示借 placeholder 一闪而过(3s 后 useVoice 自动清)
const fieldPlaceholder = computed(() => {
  const r = voice.state.lastEnd
  if (r === 'no_speech' || r === 'error') return t(`voice.${r}`)
  return t('chat.placeholder', { name: petName.value })
})

// 听写快捷键 = 麦克风按钮的键盘等价物(app 内,聊天页生效;不是全局热键——
// 按键盘的人必在屏幕前,PLAN §11 五轮归位)。Ctrl+M 避开输入法(robot:Ctrl+Space 不能用)
function onVoiceKey(e: KeyboardEvent) {
  if (activeRail.value !== 'chat') return
  if (e.key === 'Escape' && voice.state.phase !== 'idle') {
    voice.stop(false)
    return
  }
  if ((e.ctrlKey || e.metaKey) && (e.key === 'm' || e.key === 'M')) {
    e.preventDefault()
    voice.toggle()
  }
}

const apiKey = ref('')
function submitKey() {
  const k = apiKey.value.trim()
  if (!k) return
  saveApiKey(k)
  apiKey.value = ''
}

// 旺财状态机:由 TurnEvent 流推导(VM 持表现状态,engine 只发事实)。
// 听写态(VoiceEvent)优先盖过回合态;工具泡优先于通用"思考中":
// label 是字典键(tool.*),未知键(新工具配旧前端)兜底 tool.unknown —— 增量演化约定
const statusText = computed(() => {
  if (voice.state.phase === 'listening') return t('status.listening')
  if (voice.state.phase === 'transcribing') return t('status.transcribing')
  if (chat.toolAction && chat.mood === 'thinking') {
    return te(chat.toolAction) ? t(chat.toolAction) : t('tool.unknown')
  }
  // 回合收尾后音频可能还在念:speaking 体感由 speech.playing 接棒
  if (chat.mood === 'idle' && speech.state.playing) return t('status.speaking')
  return chat.mood === 'thinking' ? t('status.thinking')
    : chat.mood === 'speaking' ? t('status.speaking')
    : t('status.idle')
})

// 气泡 hover 浮现的读数行(时间是重点,排第一):3.2s · ↑1.2K ↓340 · 缓存 86% · ≈$0.0008
function fmtStats(s: TurnStats): string {
  const parts = [fmtMs(s.ms), `↑${fmtTokens(s.input_tokens)} ↓${fmtTokens(s.output_tokens)}`]
  if (s.input_tokens > 0) {
    parts.push(`${t('strip.cache')} ${Math.round((s.cache_hit_tokens / s.input_tokens) * 100)}%`)
  }
  if (s.cost_usd != null) parts.push('≈' + fmtUsd(s.cost_usd))
  return parts.join(' · ')
}

// 在飞的那条回复:读数不等 hover,直接常驻跳秒(完成后由 stats 接管,退回 hover 档案)
function isLiveBubble(m: UiMessage, mi: number): boolean {
  return chat.usage.liveMs != null && m.role === 'wang' && mi === chat.messages.length - 1
}
const liveLine = computed(() => {
  const ms = chat.usage.liveMs
  if (ms == null) return ''
  const parts = [fmtMs(ms)]
  const u = chat.usage.turn // 工具回合:轮间已有部分 token 读数,跟着跳
  if (u) parts.push(`↑${fmtTokens(u.input_tokens)} ↓${fmtTokens(u.output_tokens)}`)
  return parts.join(' · ')
})

function fmtTime(ts: number): string {
  const diff = Date.now() - ts
  if (diff < 60_000) return t('time.justNow')
  if (diff < 3600_000) return t('time.minutesAgo', { n: Math.floor(diff / 60_000) })
  if (diff < 86400_000) return t('time.hoursAgo', { n: Math.floor(diff / 3600_000) })
  if (diff < 2 * 86400_000) return t('time.yesterday')
  const d = new Date(ts)
  return `${d.getMonth() + 1}/${d.getDate()}`
}

// —— 穿梭:旺财自由游走,经过的文字被它撞开挤开(物理避让),走了弹回 ——
const streamEl = ref<HTMLElement | null>(null)
const roamer = ref<HTMLElement | null>(null)
let raf = 0
let dogX = 220, dogY = 150
let tgtX = 220, tgtY = 150
let pauseFrames = 0
let facing = 1 // 1=朝右,-1=朝左
let gaitTick = 0
let gaitPhase = 0 // 步态相位:run 帧下标
let crowd = 0 // 0=旷野 1=字堆里,平滑过渡
let pushingN = 0 // 上一帧正在挤开的字数
let legFrames = 0 // 本段航程已飞帧数(fly 角色起步姿态用)
// 角色包规范见 scripts/make-roamer-frames.py:每角色 1 张 idle + 5 张 run(192 见方、
// 体量归一、朝右),文件名顺序即步态环播放顺序;px 是体量对齐后的显示尺寸
// idle 是数组:单帧=静止蹲坐;多帧(如悬浮机器人上浮/下沉两帧)=停驻时慢速循环
// tick=换帧节拍(帧/步),越大步子越沉;fly 角色不用 tick,按航段选帧(run 约定 [前倾,巡航A,巡航B,收势])
const characters = [
  // 泰坦(默认):四帧双足循环 近腿落地→承重过腿→远腿落地→腾空换腿;
  // idle 两帧=光学眼/散热栅呼吸;步频 9 走吨位感
  { idle: [titanIdle1, titanIdle2], run: [titanRun1, titanRun2, titanRun3, titanRun4], px: 63, fly: false, tick: 12 },
  { idle: [dogIdle], run: [dogRun1, dogRun2, dogRun3, dogRun4, dogRun5], px: 52, fly: false, tick: 4 },
  { idle: [catIdle], run: [catRun1, catRun2, catRun3, catRun4, catRun5], px: 66, fly: false, tick: 4 },
]
// 形象选择 = 设置项 ui.character(每用户持久化);点头像轮换与设置页选择同一份状态
const charIds = ['titan', 'dog', 'cat'] as const
const charIdx = computed(() => Math.max(0, charIds.indexOf(settings.get('ui.character') as (typeof charIds)[number])))
const pack = computed(() => characters[charIdx.value])
function switchCharacter() {
  settings.set('ui.character', charIds[(charIdx.value + 1) % charIds.length])
  gaitTick = 0
  gaitPhase = 0
}
// 预解码,避免换帧/换角色时盒子沿用旧图宽高比闪一下
characters.forEach((c) => [...c.idle, ...c.run].forEach((u) => { const im = new Image(); im.src = u }))
const roamerSrc = ref(titanIdle1)
const roamerFlipped = ref(false)

// 缓存每个字的原始中心位置(相对 stream)
let charData: { el: HTMLElement; ox: number; oy: number }[] = []
let layoutSig = '' // 布局签名:内容高/容器宽高,变了说明缓存坐标失效
let charsDirty = false // 消息变更标脏;真正的测量合并到 roamFrame 里做(流式每字一次也不卡)
function collectChars() {
  const s = streamEl.value
  // 面板隐藏时(尺寸全 0)别测,等可见了再说,否则所有坐标坍缩到一点
  if (!s || !s.clientWidth) return
  layoutSig = `${s.scrollHeight}x${s.clientWidth}x${s.clientHeight}`
  charData = (Array.from(s.querySelectorAll('.ch')) as HTMLElement[]).map((el) => {
    // 用 offsetLeft/Top 链路(布局坐标,天然无视 transform/过渡动画):
    // 之前"清 transform 再量 rect"会在字正被挤开时量到过渡中途的位置,污染缓存
    let ox = el.offsetWidth / 2
    let oy = el.offsetHeight / 2
    let n: HTMLElement | null = el
    while (n && n !== s) { ox += n.offsetLeft; oy += n.offsetTop; n = n.offsetParent as HTMLElement | null }
    return { el, ox, oy }
  })
}
function newTarget() {
  const s = streamEl.value
  if (!s) return
  legFrames = 0
  // 目标偏向文字(随机挑个字钻过去),让旺财老在字堆里穿、经过就把字撞开
  if (charData.length && Math.random() < 0.85) {
    const c = charData[Math.floor(Math.random() * charData.length)]
    tgtX = c.ox + (Math.random() - 0.5) * 30
    tgtY = c.oy + (Math.random() - 0.5) * 24
  } else {
    tgtX = 50 + Math.random() * Math.max(80, s.clientWidth - 110)
    tgtY = 40 + Math.random() * Math.max(80, s.clientHeight - 90)
  }
}
function roamFrame() {
  // 设置/回忆页打开时聊天整列隐藏:跳过本帧所有测量/位移,空转等回来
  if (activeRail.value === 'settings' || activeRail.value === 'memory') {
    raf = requestAnimationFrame(roamFrame)
    return
  }
  // 布局漂移(图片加载/窗口缩放/内容回流)或消息变更(标脏)都会让缓存坐标失效,重测
  const sEl = streamEl.value
  if (sEl && (charsDirty || `${sEl.scrollHeight}x${sEl.clientWidth}x${sEl.clientHeight}` !== layoutSig)) {
    collectChars()
    charsDirty = false
  }
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
    // 字堆里费劲挤(慢)、旷野撒欢(快):crowd 随"正在挤开的字数"平滑过渡;
    // 腿照常倒腾,慢速 + 快腿 = 挤过去的挣扎感
    crowd += ((pushingN ? 1 : 0) - crowd) * 0.12
    const dist = Math.hypot(dx, dy)
    const step = Math.min(dist * 0.04, 2.2 - 1.3 * crowd)
    dogX += (dx / dist) * step
    dogY += (dy / dist) * step
    if (Math.abs(dx) > 1) facing = dx >= 0 ? 1 : -1
    // 6 帧跑动循环(素材朝右),朝左时整体镜像
    const cp = pack.value
    if (cp.fly) {
      // 飞行:整机倾角不能快轮(会抽搐),按航段选帧——临近收势 > 起步前倾 > 巡航两帧慢摆
      legFrames++
      if (dist < 70) { roamerSrc.value = cp.run[3] }
      else if (legFrames < 26) { roamerSrc.value = cp.run[0] }
      else {
        if (++gaitTick >= 24) { gaitTick = 0; gaitPhase ^= 1 }
        roamerSrc.value = cp.run[1 + (gaitPhase & 1)]
      }
    } else {
      if (++gaitTick >= cp.tick) { gaitTick = 0; gaitPhase = (gaitPhase + 1) % cp.run.length }
      roamerSrc.value = cp.run[gaitPhase]
    }
    roamerFlipped.value = facing < 0
  }
  // 图片自身 -50% 居中,这里直接写中心点(蹲/跑画布不同大也不会跳位)
  if (roamer.value) roamer.value.style.transform = `translate(${dogX}px, ${dogY}px)`

  // 文字物理避让:离旺财越近、被推越远;走远了弹回
  const R = 44
  let pn = 0
  for (const c of charData) {
    const ux = c.ox - dogX
    const uy = c.oy - dogY
    const d = Math.hypot(ux, uy) || 1
    if (d < R) {
      pn++
      const f = ((R - d) / R) * 20
      c.el.style.transform = `translate(${(ux / d) * f}px, ${(uy / d) * f}px)`
    } else if (c.el.style.transform) {
      c.el.style.transform = ''
    }
  }
  pushingN = pn
  raf = requestAnimationFrame(roamFrame)
}
onMounted(() => nextTick(() => { collectChars(); newTarget(); raf = requestAnimationFrame(roamFrame) }))
onMounted(() => window.addEventListener('keydown', onVoiceKey))
onUnmounted(() => window.removeEventListener('keydown', onVoiceKey))
let lastLen = 0
watch(messages, () => nextTick(() => {
  charsDirty = true
  const s = streamEl.value
  if (!s) return
  // 新气泡无条件贴底;流式增量只在"本来就在底部附近"时跟随,不打断用户翻历史
  const newBubble = chat.messages.length !== lastLen
  lastLen = chat.messages.length
  if (newBubble || s.scrollHeight - s.scrollTop - s.clientHeight < 90) s.scrollTop = s.scrollHeight
}), { deep: true })
onUnmounted(() => cancelAnimationFrame(raf))
</script>

<template>
  <div class="layout" :class="{ booting, closed: !panelOpen, cut: shape === 'cut' }" :style="{ fontSize: textScale }">
    <!-- 左:图标栏 -->
    <nav class="rail">
      <div class="rail-top">
        <button class="rb" :class="{ on: activeRail === 'chat' }" @click="activeRail = 'chat'" :title="t('nav.chat')">
          <svg viewBox="0 0 24 24"><path d="M5 5h14a1 1 0 011 1v8a1 1 0 01-1 1H9l-4 4V6a1 1 0 011-1z" /></svg>
          <span>{{ t('nav.chat') }}</span>
        </button>
        <button class="rb" :class="{ on: activeRail === 'scenes' }" @click="activeRail = 'scenes'" :title="t('nav.scenes')">
          <svg viewBox="0 0 24 24"><rect x="4" y="4" width="7" height="7" rx="1" /><rect x="13" y="4" width="7" height="7" rx="1" /><rect x="4" y="13" width="7" height="7" rx="1" /><rect x="13" y="13" width="7" height="7" rx="1" /></svg>
          <span>{{ t('nav.scenes') }}</span>
        </button>
        <button class="rb" :class="{ on: activeRail === 'memory' }" @click="activeRail = 'memory'" :title="t('nav.memory')">
          <svg viewBox="0 0 24 24"><path d="M7 4h10v16l-5-3-5 3z" /></svg>
          <span>{{ t('nav.memory') }}</span>
        </button>
      </div>
      <button class="rb gear" :class="{ on: activeRail === 'settings' }" :title="t('nav.settings')" @click="activeRail = 'settings'">
        <svg viewBox="0 0 24 24"><circle cx="12" cy="12" r="7" /><circle cx="12" cy="12" r="2.6" /></svg>
        <span>{{ t('nav.settings') }}</span>
        <!-- 唯一脉冲:缺钥匙时指路(与设置页大脑 tab 同一信号) -->
        <span v-if="chat.ready && !chat.hasApiKey" class="gear-dot"></span>
      </button>
    </nav>

    <!-- 中:最近(可关;设置/回忆页打开时整列让位) -->
    <aside class="recents" v-show="panelOpen && activeRail !== 'settings' && activeRail !== 'memory'">
      <header class="rc-head">
        <span>{{ t('recents.title') }}</span>
        <button class="collapse" @click="panelOpen = false" :title="t('recents.collapse')">‹</button>
      </header>
      <ul class="rc-list">
        <li
          v-for="s in chat.conversations"
          :key="s.id"
          :class="{ on: s.id === chat.convId }"
          @click="selectConversation(s.id)"
        >
          <span class="rc-title">{{ s.title || t('recents.untitled') }}</span>
          <span class="rc-time">{{ fmtTime(s.updated_at) }}</span>
        </li>
      </ul>
      <button class="rc-new" @click="newConversation">{{ t('recents.newTopic') }}</button>
    </aside>

    <!-- 设置台/回忆页:rail 目的地,整区接管(聊天 v-show 保活,状态无损) -->
    <SettingsView v-if="activeRail === 'settings'" @close="activeRail = 'chat'" />
    <MemoryView v-if="activeRail === 'memory'" @close="activeRail = 'chat'" />

    <!-- 右:对话主体 -->
    <main class="chat" v-show="activeRail !== 'settings' && activeRail !== 'memory'">
      <header class="chat-head" data-tauri-drag-region>
        <button v-if="!panelOpen" class="reopen" @click="panelOpen = true" :title="t('recents.expand')">›</button>
        <img :src="pack.idle[0]" class="head-av" :alt="petName" :title="t('avatar.switchTitle')" style="height: 46px; width: auto; cursor: pointer;" @click="switchCharacter" />
        <div class="who"><b>{{ petName }}</b><small><span class="led"></span>{{ statusText }}</small></div>
      </header>

      <div class="stream" ref="streamEl">
        <div v-for="(m, mi) in messages" :key="m.id" class="bubble" :class="m.role">
          <span v-for="(ch, ci) in Array.from(m.text)" :key="ci" class="ch">{{ ch }}</span>
          <!-- 完成的回复:读数默认隐身,hover 浮现;在飞的回复:跳秒常驻,不用 hover -->
          <span v-if="m.stats" class="turn-meta">{{ fmtStats(m.stats) }}</span>
          <span v-else-if="isLiveBubble(m, mi)" class="turn-meta live">{{ liveLine }}</span>
          <!-- 再听一遍(hover 浮现;缓存命中秒回) -->
          <button
            v-if="m.role === 'wang' && m.text && chat.inTauri"
            class="replay"
            :title="t('chat.replay')"
            @click="replay(m.text)"
          >
            <!-- 耳机:再听一遍 = 重播(听),与语音输入的话筒区分 -->
            <svg viewBox="0 0 24 24"><path d="M4.5 14v-2a7.5 7.5 0 0 1 15 0v2" /><rect x="3" y="13.5" width="3.6" height="6.6" rx="1.8" /><rect x="17.4" y="13.5" width="3.6" height="6.6" rx="1.8" /></svg>
          </button>
        </div>
        <div class="roamer" ref="roamer"><img :class="{ flipped: roamerFlipped }" :src="roamerSrc" alt="" :style="{ width: pack.px + 'px' }" /></div>
      </div>

      <div class="composer">
        <!-- 这里没有场景/模式切换器,也永远不会有(铁律 §3.2:用户只面对一个 7274) -->
        <div v-if="chat.ready && !chat.hasApiKey" class="input-row key-row">
          <input
            v-model="apiKey"
            class="field"
            type="password"
            :placeholder="t('key.placeholder')"
            @keyup.enter="submitKey"
          />
          <button class="send key-save" :disabled="!apiKey.trim()" @click="submitKey" :title="t('key.save')">✓</button>
        </div>
        <!-- 播放条 + 登录建议气泡(音频形态;视频走全局 VideoOverlay) -->
        <PlayerBar />
        <!-- 听写态:输入框位变波形(点击 = 立即定稿发送;✕/Esc = 取消) -->
        <div v-if="voice.state.phase !== 'idle'" class="input-row">
          <div
            class="field listen-field"
            :class="[voice.state.phase, { heard: voice.state.heard }]"
            @click="voice.stop(true)"
          >
            <span class="wave"><i v-for="(h, i) in bars" :key="i" :style="{ height: h + '%' }"></i></span>
            <span class="listen-hint">{{ listenHint }}</span>
          </div>
          <button class="send cancel-listen" @click="voice.stop(false)" :title="t('chat.micCancelTitle')">✕</button>
        </div>
        <div v-else class="input-row">
          <!-- 语音输入 = 输入框内的小话筒(轻量,不跟发送键并排抢戏;界面优先,语音只是输入之一) -->
          <span class="field-wrap">
            <input v-model="input" class="field has-mic" :placeholder="fieldPlaceholder" @keyup.enter="send" />
            <button class="mic-inline" @click="micToggle()" :title="t('chat.micTitle')">
              <svg viewBox="0 0 24 24"><rect x="9.2" y="3.2" width="5.6" height="10.4" rx="2.8" /><path d="M5.8 11.2a6.2 6.2 0 0 0 12.4 0M12 17.6v3.2M8.8 20.8h6.4" /></svg>
            </button>
          </span>
          <!-- 停止键覆盖两种"它在动"的状态:回合在飞 / 音频在念(点击都立即安静) -->
          <button v-if="chat.mood !== 'idle' || speech.state.playing" class="send stop" @click="cancel()" :title="t('chat.stop')">⏹</button>
          <button v-else class="send" @click="send" :disabled="!input.trim()" :title="t('chat.send')">➤</button>
        </div>
        <!-- 记账灯带:本轮消耗 / 今日累计 / 余额(数据缺席的段自己熄灯) -->
        <UsageStrip />
      </div>
    </main>
  </div>
</template>

<style scoped>
.layout {
  --txt: #d4e6f7;
  --txt2: #85a4c0;
  --cy: #5fd2ff;
  --glass: rgba(10, 24, 46, 0.55);
  --glass-2: rgba(14, 32, 58, 0.45);
  --line: rgba(95, 200, 255, 0.16);

  position: fixed; inset: 0; z-index: 5;
  display: flex; gap: 0;
  color: var(--txt);
  font-family: -apple-system, "PingFang SC", "Segoe UI", sans-serif;
  font-size: 14px;
}
.layout.booting { animation: layIn .6s ease .5s backwards; }
@keyframes layIn { from { opacity: 0; transform: translateY(10px); } }

/* —— 左图标栏 —— */
.rail {
  flex: 0 0 64px; display: flex; flex-direction: column; justify-content: space-between;
  padding: 16px 0; background: var(--glass);
  backdrop-filter: blur(10px); -webkit-backdrop-filter: blur(10px);
  border-right: 1px solid var(--line);
}
.rail-top { display: flex; flex-direction: column; gap: 6px; }
.rb {
  background: rgba(95, 200, 255, 0.04); border: 1px solid var(--line); border-radius: 11px;
  cursor: pointer; color: var(--txt2);
  display: flex; flex-direction: column; align-items: center; gap: 4px;
  width: 48px; margin: 0 auto; padding: 9px 0; font-size: 10px; letter-spacing: 1px;
  position: relative; transition: color .15s, border-color .15s, background .15s;
}
.rb svg { width: 21px; height: 21px; fill: none; stroke: currentColor; stroke-width: 1.7; stroke-linejoin: round; }
.rb:hover { color: var(--txt); border-color: rgba(95, 200, 255, 0.4); }
.rb.on { color: var(--cy); border-color: rgba(95, 200, 255, 0.45); background: rgba(95, 200, 255, 0.1); }
.rb.on::after {
  content: ""; position: absolute; top: 0; left: 0; width: 5px; height: 5px; margin: -2.5px;
  border-radius: 50%; background: var(--cy); box-shadow: 0 0 8px 1px var(--cy);
  animation: orbit 3s linear infinite;
}
@keyframes orbit {
  0% { top: 0; left: 0; } 25% { top: 0; left: 100%; }
  50% { top: 100%; left: 100%; } 75% { top: 100%; left: 0; } 100% { top: 0; left: 0; }
}
/* 唯一脉冲:缺钥匙时齿轮上的琥珀光点 */
.gear-dot {
  position: absolute; top: 5px; right: 5px; width: 6px; height: 6px; border-radius: 50%;
  background: #ffc85f; box-shadow: 0 0 8px #ffc85f; animation: led 2.4s ease-in-out infinite;
}

/* —— 中:最近 —— */
.recents {
  flex: 0 0 216px; display: flex; flex-direction: column;
  background: transparent;
  border-right: 1px solid var(--line);
}
.rc-head { display: flex; align-items: center; justify-content: space-between; padding: 16px 16px 10px; font-size: 12px; letter-spacing: 2px; color: var(--txt2); }
.collapse { background: none; border: none; color: var(--txt2); cursor: pointer; font-size: 18px; line-height: 1; }
.collapse:hover { color: var(--cy); }
.rc-list { list-style: none; margin: 0; padding: 0 8px; flex: 1; overflow-y: auto; }
.rc-list li {
  margin-bottom: 8px; padding: 10px 12px; border-radius: 10px; cursor: pointer;
  display: flex; flex-direction: column; gap: 3px;
  background: rgba(14, 32, 58, 0.4); border: 1px solid var(--line);
  backdrop-filter: blur(6px); -webkit-backdrop-filter: blur(6px);
  transition: border-color .15s, background .15s;
}
.rc-list li:hover { border-color: rgba(95, 200, 255, 0.4); }
.rc-list li.on { background: rgba(95, 200, 255, 0.12); border-color: rgba(95, 200, 255, 0.5); box-shadow: 0 0 12px rgba(95, 200, 255, 0.12); }
.rc-title { font-size: 13px; color: var(--txt); }
.rc-time { font-size: 11px; color: var(--txt2); }
.rc-new { margin: 10px; padding: 9px; border-radius: 10px; background: none; border: 1px dashed var(--line); color: var(--txt2); cursor: pointer; font-size: 12.5px; }
.rc-new:hover { color: var(--cy); border-color: var(--cy); }

/* —— 右:对话 —— */
.chat { flex: 1; display: flex; flex-direction: column; min-width: 0; position: relative; }
.chat > * { position: relative; z-index: 1; }
.chat::before {
  content: ""; position: absolute; inset: 0; z-index: 0; pointer-events: none;
  background: linear-gradient(180deg, rgba(6, 16, 34, 0.18), rgba(6, 16, 34, 0.44));
}
/* 右内边距留出右上角窗控三键的位置(无边框补窗控,PLAN §12) */
.chat-head { display: flex; align-items: center; gap: 10px; padding: 14px 84px 14px 20px; border-bottom: 1px solid var(--line); }
.head-av { transition: transform .15s; }
.reopen { background: none; border: 1px solid var(--line); color: var(--txt2); cursor: pointer; border-radius: 8px; width: 26px; height: 26px; font-size: 16px; }
.reopen:hover { color: var(--cy); border-color: var(--cy); }
.who { display: flex; flex-direction: column; line-height: 1.25; }
.who b { font-size: 15px; color: var(--txt); }

.stream { flex: 1; overflow-y: auto; padding: 22px 26px; display: flex; flex-direction: column; gap: 13px; position: relative; }
.roamer { position: absolute; top: 0; left: 0; z-index: 6; pointer-events: none; will-change: transform; }
.stream::-webkit-scrollbar { width: 8px; }
.stream::-webkit-scrollbar-thumb { background: rgba(95, 200, 255, 0.18); border-radius: 4px; }
.stream::-webkit-scrollbar-thumb:hover { background: rgba(95, 200, 255, 0.34); }
.bubble {
  max-width: 70%; padding: 11px 15px; border-radius: 16px; font-size: 14px; line-height: 1.55;
  backdrop-filter: blur(9px); -webkit-backdrop-filter: blur(9px);
  box-shadow: 0 6px 20px rgba(0, 0, 0, 0.28);
  white-space: pre-wrap; word-break: break-word;
  position: relative;
}
/* 回复读数:贴在气泡下沿,默认隐身,hover 浮现(不挤布局,不打扰陪伴感) */
.turn-meta {
  position: absolute; top: 100%; left: 13px; margin-top: 3px;
  font: 10px/1 ui-monospace, "SF Mono", monospace; letter-spacing: 0.6px;
  color: var(--cy); text-shadow: 0 0 8px rgba(95, 200, 255, 0.3);
  white-space: nowrap; pointer-events: none; user-select: none;
  opacity: 0; transform: translateY(-2px); transition: opacity .18s ease, transform .18s ease;
  z-index: 7;
}
.bubble:hover .turn-meta { opacity: 0.9; transform: translateY(0); }
/* 在飞读数:常驻可见,轻微呼吸 —— 跳秒本身就是"我在干活"的信号 */
.turn-meta.live { transform: translateY(0); animation: metaLive 1.6s ease-in-out infinite; }
@keyframes metaLive { 0%, 100% { opacity: 0.85; } 50% { opacity: 0.45; } }
.bubble.wang {
  align-self: flex-start; background: rgba(20, 46, 78, 0.55);
  border: 1px solid var(--line); border-bottom-left-radius: 5px; color: var(--txt);
}
.bubble.user {
  align-self: flex-end; background: rgba(95, 175, 235, 0.22);
  border: 1px solid rgba(120, 200, 255, 0.3); border-bottom-right-radius: 5px; color: #eaf4ff;
}

/* 每个字一个 span,被旺财撞开时 transform 位移,走远了弹回(transition) */
.ch { display: inline-block; white-space: pre; transition: transform .16s ease-out; }

.roamer img { display: block; transform: translate(-50%, -50%); }
.roamer img.flipped { transform: translate(-50%, -50%) scaleX(-1); }

.composer { padding: 12px 18px 16px; border-top: 1px solid var(--line); display: flex; flex-direction: column; gap: 9px; }
.input-row { display: flex; gap: 9px; }
.field {
  flex: 1; background: rgba(8, 20, 38, 0.6); border: 1px solid var(--line); border-radius: 13px;
  padding: 11px 15px; color: var(--txt); font-size: 14px; outline: none;
  backdrop-filter: blur(6px); -webkit-backdrop-filter: blur(6px);
}
.field::placeholder { color: var(--txt2); }
.field:focus { border-color: var(--cy); box-shadow: 0 0 0 2px rgba(95, 200, 255, 0.12); }
.send {
  width: 46px; border: 1px solid var(--line); border-radius: 13px; cursor: pointer; font-size: 16px;
  background: rgba(95, 200, 255, 0.1); color: var(--cy);
  backdrop-filter: blur(6px); -webkit-backdrop-filter: blur(6px);
  transition: border-color .15s, background .15s, box-shadow .15s;
}
.send:hover:not(:disabled) { border-color: var(--cy); background: rgba(95, 200, 255, 0.2); box-shadow: 0 0 14px rgba(95, 200, 255, 0.3); }
.send:disabled { opacity: 0.4; cursor: default; }
.send.stop { color: #ffb86b; border-color: rgba(255, 184, 107, 0.4); }
.send.stop:hover { border-color: #ffb86b; background: rgba(255, 184, 107, 0.15); box-shadow: 0 0 14px rgba(255, 184, 107, 0.3); }
.key-row .field { border-color: rgba(255, 200, 95, 0.45); }

/* 语音输入:输入框内右侧小话筒(轻量,不跟发送键并排抢戏;界面优先,语音只是输入之一) */
.field-wrap { flex: 1; position: relative; display: flex; min-width: 0; }
.field.has-mic { padding-right: 42px; }
.mic-inline {
  position: absolute; right: 6px; top: 50%; transform: translateY(-50%);
  width: 30px; height: 30px; padding: 0; border: none; background: none; cursor: pointer;
  color: var(--txt2); display: flex; align-items: center; justify-content: center;
  border-radius: 8px; transition: color .15s, background .15s;
}
.mic-inline:hover { color: var(--cy); background: rgba(95, 200, 255, 0.12); }
.mic-inline svg { width: 17px; height: 17px; fill: none; stroke: currentColor; stroke-width: 1.7; stroke-linecap: round; display: block; }

/* —— 听写(PLAN §11):输入框位变波形,token 体系,无新布局结构 —— */
.send.cancel-listen { color: #f09595; border-color: rgba(240, 149, 149, 0.4); }
.send.cancel-listen:hover { border-color: #f09595; background: rgba(240, 149, 149, 0.12); box-shadow: 0 0 14px rgba(240, 149, 149, 0.25); }
.listen-field {
  display: flex; align-items: center; gap: 12px; cursor: pointer; user-select: none;
  border-color: rgba(95, 200, 255, 0.5); box-shadow: 0 0 16px rgba(95, 200, 255, 0.16) inset, 0 0 10px rgba(95, 200, 255, 0.12);
}
.listen-field.heard { border-color: var(--cy); }
.listen-field.preparing, .listen-field.transcribing { cursor: default; }
.wave { display: flex; align-items: center; gap: 3px; height: 20px; flex: 0 0 auto; }
.wave i {
  width: 3px; min-height: 12%; background: var(--cy); border-radius: 2px;
  transition: height .09s linear; box-shadow: 0 0 6px rgba(95, 200, 255, 0.45);
}
/* 准备/识别中:电平没了,柱子改呼吸,别像死机 */
.listen-field.preparing .wave i, .listen-field.transcribing .wave i { animation: wavePulse 1.1s ease-in-out infinite; }
.listen-field.preparing .wave i:nth-child(odd), .listen-field.transcribing .wave i:nth-child(odd) { animation-delay: .25s; }
@keyframes wavePulse { 0%, 100% { height: 14%; } 50% { height: 64%; } }
.listen-hint { color: var(--txt2); font-size: 12.5px; }
.listen-field.heard .listen-hint { color: var(--cy); }

/* 再听一遍(耳机=重播):贴气泡右下,默认隐身 hover 浮现(与读数同款克制),小巧 */
.replay {
  position: absolute; right: 8px; bottom: -22px; z-index: 7;
  width: 19px; height: 16px; padding: 0;
  display: flex; align-items: center; justify-content: center;
  background: rgba(95, 200, 255, 0.08); color: var(--cy);
  border: 1px solid var(--line); border-radius: 5px; cursor: pointer;
  opacity: 0; transition: opacity .18s ease;
}
.replay svg { width: 11px; height: 11px; fill: none; stroke: currentColor; stroke-width: 1.8; stroke-linecap: round; display: block; }
.bubble:hover .replay { opacity: 0.9; }
.replay:hover { border-color: var(--cy); }

/* —— HUD 增强 —— */
.who small { display: flex; align-items: center; gap: 6px; font-size: 11.5px; color: var(--txt2); }
.led { width: 6px; height: 6px; border-radius: 50%; background: #5fe0b0; box-shadow: 0 0 8px #5fe0b0; animation: led 2.4s ease-in-out infinite; }
@keyframes led { 0%, 100% { opacity: 1; } 50% { opacity: .3; } }

.rc-head, .rb span { letter-spacing: 1.5px; }
.rc-time { font-family: ui-monospace, "SF Mono", monospace; letter-spacing: .5px; }

.rail::after, .recents::after {
  content: ""; position: absolute; top: 0; right: -1px; width: 1px; height: 72px; pointer-events: none;
  background: linear-gradient(180deg, transparent, var(--cy), transparent);
  opacity: .7; animation: flow 5.5s linear infinite;
}
@keyframes flow { 0% { transform: translateY(-72px); } 100% { transform: translateY(101vh); } }

.layout.cut .bubble { border-radius: 0; box-shadow: none; filter: drop-shadow(0 6px 16px rgba(0, 0, 0, 0.3)); }
.layout.cut .bubble.wang { clip-path: polygon(0 0, 100% 0, 100% calc(100% - 9px), calc(100% - 9px) 100%, 0 100%); }
.layout.cut .bubble.user { clip-path: polygon(0 0, 100% 0, 100% 100%, 9px 100%, 0 calc(100% - 9px)); }
</style>
