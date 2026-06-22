<script setup lang="ts">
// 桌面悬浮窗(PLAN §12 形态 C):一个胶囊(圆头像 + 矮信息条)始终在,位置 = 锚点(不动)。
// 单击胶囊展开/收起、按住拖动。展开时内容从胶囊**向有空间的方向**冒出(屏幕下半 → 向上长、
// 上半 → 向下长;胶囊视觉位置不变 → 不跳)。独立 WebView,订阅同一 app_event。
// 信息条 = 固定优先级单行(语音/mood > 通知 > 后台 > 待机轮播);头像当状态灯;关闭 = 框外"小耳朵"。
// 透明度 ui.float.opacity。胶囊位置持久化(只记收起态)。
import { computed, onMounted, onUnmounted, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import { useAgentMood } from '../composables/useAgentMood'
import { useFloat } from '../composables/useFloat'
import { useFloatIdle } from '../composables/useFloatIdle'
import { useSettings } from '../composables/useSettings'
import { emitOpenConversation, floatWin, type TextRef } from '../lib/backend'
import titanIdle from '../assets/titan-idle-1.png'
import dogIdle from '../assets/dog-idle.png'
import catIdle from '../assets/cat-idle.png'

const { t, te } = useI18n()
const settings = useSettings()
const { state, running, nowPlaying, mediaPlaying, mediaToggle, mediaStop, listening, level, wakeArmed, dismissNotice, openMain } = useFloat()
const mood = useAgentMood()
const idle = useFloatIdle()

const avatars: Record<string, string> = { titan: titanIdle, dog: dogIdle, cat: catIdle }
const avatar = computed(() => avatars[settings.get('ui.character')] ?? titanIdle)
const petName = computed(() => settings.get('ui.pet_name') || t('pet.name'))
const baseOpacity = computed(() => Number(settings.get('ui.float.opacity') || '0.8'))
const hovering = ref(false)

const W = 160 // 窗口宽(胶囊缩短到约 6 成)
const COLLAPSED_H = 56 // 收起 = 胶囊高
const EXPANDED_H = 220 // 展开 = 胶囊 + 内容(两区 + 小标题留白)
const openUp = ref(false) // 展开方向:true = 内容在胶囊上方(向上长)
let anchorX = 0 // 胶囊(收起态)左上的屏幕坐标 —— 锚点,展开/收起都以它为基准,不累积误差
let anchorY = 0
let anchorReady = false

function txt(ref?: TextRef): string {
  if (!ref) return ''
  return te(ref.key) ? t(ref.key, (ref.params ?? {}) as Record<string, unknown>) : t('task.unknown')
}

// 胶囊只有一行:固定优先级,只显示最该看的一件(语音/mood > 通知 > 后台 > 待机轮播)。
// 这不是 robot 那种可配置排序(§2 否决);是单槽兜底梯,写死、不暴露给用户。头像另当状态灯。
type Bar = {
  text: string
  tone: 'hot' | 'normal' | 'dim'
  wave?: boolean
  pct?: number
  icon?: string
  count?: number
}
const bar = computed<Bar>(() => {
  // ① 语音 / 回合进行中(你正在跟它互动,最该知道)
  if (listening.value) return { text: t('float.listening'), tone: 'hot', wave: true }
  if (mood.state.mood === 'thinking') return { text: t('float.thinking'), tone: 'hot' }
  if (mood.state.mood === 'speaking') return { text: t('float.speaking'), tone: 'hot' }
  // ② 未读通知(显最新一条 + 计数)
  if (state.notices.length) {
    return {
      text: state.notices[0].text,
      tone: 'normal',
      count: state.notices.length > 1 ? state.notices.length : undefined,
    }
  }
  // ③ 后台活动(次要,缩成图标 + 进度)
  if (running.value.length) {
    return { text: txt(running.value[0].label), tone: 'dim', icon: '⬇', pct: running.value[0].progress ?? undefined }
  }
  if (nowPlaying.value) return { text: nowPlaying.value.title, tone: 'dim', icon: '♪' }
  // ④ 待机轮播(只显示 OS 不会告诉你的事:下个提醒/最近一句…);空池 → 问候
  return { text: idle.current.value?.text ?? t('float.idle'), tone: 'dim' }
})

// 头像状态灯(§12 v1 头像反映语音态 + idle;修订后加 thinking/speaking)。
const orbState = computed(() => {
  if (listening.value) return 'listen'
  if (mood.state.mood === 'thinking') return 'think'
  if (mood.state.mood === 'speaking') return 'speak'
  if (wakeArmed.value) return 'armed' // 待机·免手唤醒在跑:头像一圈很淡的常亮环(竖着耳朵)
  return ''
})

// 聆听波形:单个 level 标量 × 固定形状 → 一条随声音起伏的波(胶囊条 + 面板共用)。
const WAVE_SHAPE = [0.5, 1, 0.65, 0.9, 0.45]
const waveBars = computed(() => WAVE_SHAPE.map((s) => Math.round(3 + s * (0.2 + level.value * 0.8) * 12)))

// 展开:锚点固定,按屏幕空间决定向上/向下;胶囊视觉位置不变
async function expand() {
  const b = await floatWin.box()
  if (!b) {
    state.expanded = true
    return
  }
  anchorX = b.x
  anchorY = b.y
  anchorReady = true
  openUp.value = anchorY + EXPANDED_H > b.screenBottom - 8 // 向下放不下 → 向上长
  // 先翻 expanded,再 setBox:setBox 的 setPosition 触发 onMoved 时必须已 expanded+openUp,
  // 否则锚点被这次"程序化移动"污染 → 收起会跑到上面(真机 bug 修复)
  state.expanded = true
  await floatWin.setBox(
    anchorX,
    openUp.value ? anchorY - (EXPANDED_H - COLLAPSED_H) : anchorY,
    W,
    EXPANDED_H,
  )
}
async function collapse() {
  state.expanded = false
  if (anchorReady) await floatWin.setBox(anchorX, anchorY, W, COLLAPSED_H) // 回锚点
}
function toggle() {
  void (state.expanded ? collapse() : expand())
}
// 单击 / 双击区分(矮条):单击延迟一拍执行展开/收起,期间若来了双击就取消、改为开主窗。
// 拖动已分到头像手柄(见 onOrbDown),这里不再有"拖 vs 点"要消歧,纯按 click/dblclick。
let tapTimer: ReturnType<typeof setTimeout> | undefined
function onCapTap() {
  clearTimeout(tapTimer)
  tapTimer = setTimeout(() => void toggle(), 230)
}
function onCapDouble() {
  clearTimeout(tapTimer)
  openMain()
}

// 拖动:只有头像是手柄,按下即进原生拖动(imperative startDragging,不用 data-tauri-drag-region
// —— 后者在 Windows 上吞单击,#9751/#9901)。职责分区 = 头像抓着挪、矮条点开,从根上消掉
// Windows 上"整条又拖又点"的互吞(真鼠标点击常抖 >4px → 被当拖动 → 吞掉展开)。
function onOrbDown(e: MouseEvent) {
  if (e.button !== 0) return // 只左键拖
  floatWin.startDragging()
}

// 点通知 → 唤主窗 + 跳对应会话;关闭走框外"小耳朵"(只收起回胶囊,不误关整窗)
function openNotice(convId: number) {
  emitOpenConversation(convId)
  openMain()
}

let stopMoved = () => {}
let saveTimer: ReturnType<typeof setTimeout> | undefined
onMounted(async () => {
  // 恢复收起态位置(逻辑坐标);没有则沿用 setup 放的位置,只把宽度收成 W
  const pos = settings.get('ui.float.pos')
  const saved = pos ? pos.split(',').map(Number) : null
  if (saved && saved.length === 2 && saved.every(Number.isFinite)) {
    await floatWin.setBox(saved[0], saved[1], W, COLLAPSED_H)
  } else {
    const b = await floatWin.box()
    if (b) await floatWin.setBox(b.x, b.y, W, COLLAPSED_H)
  }
  const b = await floatWin.box()
  if (b) {
    anchorX = b.x
    anchorY = b.y
    anchorReady = true
  }
  // 拖动:更新锚点(展开态要扣掉向上长的偏移);收起态防抖持久化
  stopMoved = floatWin.onMoved((x, y) => {
    anchorX = x
    anchorY = state.expanded && openUp.value ? y + (EXPANDED_H - COLLAPSED_H) : y
    if (!state.expanded) {
      clearTimeout(saveTimer)
      saveTimer = setTimeout(
        () => settings.set('ui.float.pos', `${Math.round(anchorX)},${Math.round(anchorY)}`),
        500,
      )
    }
  })
})
onUnmounted(() => stopMoved())
</script>

<template>
  <div
    class="float"
    :class="{ hover: hovering }"
    :style="{ opacity: baseOpacity }"
    @mouseenter="hovering = true; idle.setPaused(true)"
    @mouseleave="hovering = false; idle.setPaused(false)"
  >
    <!-- shell:column = 胶囊顶 / 内容下(向下展开);column-reverse = 内容上 / 胶囊底(向上展开) -->
    <div class="shell" :class="{ up: openUp }">
      <!-- 胶囊:始终在(锚点不动)。矮条单击 toggle / 双击开主窗(bar 内 pointer-events:none,
           点击穿透到 cap);头像 = 拖动手柄(职责分区,避免 Win 上又拖又点互吞,见 onOrbDown) -->
      <div class="cap" @click="onCapTap" @dblclick="onCapDouble">
        <div class="bar">
          <span class="bar-text" :class="bar.tone">{{ bar.text }}</span>
          <span v-if="bar.wave" class="wave"><i v-for="(h, i) in waveBars" :key="i" :style="{ height: h + 'px' }" /></span>
          <span v-else-if="bar.icon" class="bar-badge">{{ bar.icon }}</span>
          <em v-if="bar.pct != null" class="pct">{{ Math.round(bar.pct * 100) }}%</em>
          <span v-if="bar.count" class="bar-dot">{{ bar.count }}</span>
        </div>
        <div class="orb" :class="orbState" @mousedown="onOrbDown"><img :src="avatar" :alt="petName" /></div>
      </div>

      <!-- 展开内容:两区 —— 正在进行(钉住) / 最近消息(新→旧);全显示,不取舍 -->
      <div v-if="state.expanded" class="body">
        <template v-if="listening || nowPlaying || running.length">
          <div class="ptag">{{ t('float.zoneNow') }}</div>
          <div v-if="listening" class="status">
            <i>🎙</i><span class="ellip">{{ t('float.listening') }}</span>
            <span class="wave"><i v-for="(h, i) in waveBars" :key="i" :style="{ height: h + 'px' }" /></span>
          </div>
          <div v-if="nowPlaying" class="status">
            <i>♪</i><span class="ellip">{{ nowPlaying.title }}</span>
            <!-- 迷你播控:点击转发主窗(useMedia 按 isFloat 分流;悬浮窗自身不出声) -->
            <button class="mctl" :title="mediaPlaying ? t('float.pause') : t('float.resume')" @click.stop="mediaToggle()">{{ mediaPlaying ? '⏸' : '▶' }}</button>
            <button class="mctl" :title="t('float.stop')" @click.stop="mediaStop()">⏹</button>
          </div>
          <div v-for="tk in running" :key="tk.task_id" class="status">
            <i>⬇</i><span class="ellip">{{ txt(tk.label) }}</span>
            <em v-if="tk.progress != null">{{ Math.round(tk.progress * 100) }}%</em>
          </div>
        </template>

        <template v-if="state.notices.length">
          <div class="ptag">{{ t('float.zoneNews') }}</div>
          <div v-for="n in state.notices" :key="n.id" class="notice" @click="openNotice(n.conv_id)">
            <span class="n-text">{{ n.text }}</span>
            <button class="n-x" :title="t('float.close')" @click.stop="dismissNotice(n.id)">✕</button>
          </div>
        </template>

        <!-- 待机:轮播当前条(下个提醒/最近一句…) / 问候 -->
        <div v-if="!state.notices.length && !listening && !nowPlaying && !running.length" class="empty">
          {{ idle.current.value?.text ?? t('float.idle') }}
        </div>
      </div>

      <!-- 关闭"小耳朵":挂面板外角(随展开方向换上/下角),只收起回胶囊;不进 .body 免被 overflow 裁 -->
      <button
        v-if="state.expanded"
        class="ear"
        :class="{ up: openUp }"
        :title="t('float.collapse')"
        @click.stop="collapse"
      >
        ✕
      </button>
    </div>
  </div>
</template>

<style scoped>
.float {
  /* --f-* 皮肤 token 在 style.css :root / [data-skin] 定义(随皮肤切);
     悬浮窗经 useSettings.load() 拉 api.skin() 设 <html data-skin>,故无需本地定义。 */
  width: 100vw;
  height: 100vh;
  overflow: hidden;
  font-family: -apple-system, "PingFang SC", "Segoe UI", sans-serif;
  transition: opacity 0.22s ease;
  user-select: none;
}

.shell {
  position: relative; /* "小耳朵"按它定位(挂在面板外角,不进 .body 免被 overflow 裁) */
  height: 100%;
  display: flex;
  flex-direction: column;
}
.shell.up { flex-direction: column-reverse; }

/* —— 胶囊:圆 + 矮条,固定 56 高(锚点) —— */
.cap {
  position: relative;
  flex: 0 0 56px;
  display: flex;
  align-items: center;
  padding: 0 6px;
  cursor: pointer;
}
.bar {
  flex: 1;
  min-width: 0;
  height: 34px;
  margin-left: 22px;
  padding: 0 14px 0 28px;
  pointer-events: none; /* 点击穿透到 cap → 整条可拖 / 单击 toggle */
  display: flex;
  align-items: center;
  gap: 6px;
  border-radius: 17px;
  background: var(--f-solid);
  box-shadow: 0 2px 8px rgba(0, 0, 0, 0.3);
  transition: box-shadow 0.2s ease;
}
/* hover:不改透明度(那会盖掉用户调的档),只加一圈淡淡强调色辉光表示"选中/可点" */
.float.hover .bar {
  box-shadow: 0 2px 8px rgba(0, 0, 0, 0.3), 0 0 13px rgba(var(--accent-rgb), 0.45);
}
.orb {
  position: absolute;
  left: 5px;
  top: 50%;
  transform: translateY(-50%);
  width: 44px;
  height: 44px;
  border-radius: 50%;
  background: var(--f-solid);
  box-shadow: 0 2px 8px rgba(0, 0, 0, 0.3);
  display: flex;
  align-items: center;
  justify-content: center;
  pointer-events: auto; /* 头像 = 拖动手柄(其余区域 pointer-events:none 穿透到 cap 点击展开) */
  cursor: pointer; /* 小手(用户偏好);拖动手柄但不用 move 那个"十"字光标 */
}
.orb img { width: 36px; height: 36px; object-fit: contain; pointer-events: none; }
/* 头像状态灯:语音/mood 给圆头像加辉光环(box-shadow 严格贴圆,不用 drop-shadow 防 WKWebView 方块影) */
.orb.listen { box-shadow: 0 2px 8px rgba(0, 0, 0, 0.3), 0 0 0 2px var(--f-cy); }
.orb.think { box-shadow: 0 2px 8px rgba(0, 0, 0, 0.3), 0 0 0 2px rgba(var(--accent-rgb), 0.5); }
/* 待机·麦克风在等唤醒:比 think 更轻的常亮细环,只透出"竖着耳朵"的存在感 */
.orb.armed { box-shadow: 0 2px 8px rgba(0, 0, 0, 0.3), 0 0 0 1.5px rgba(var(--accent-rgb), 0.3); }
.orb.speak {
  box-shadow: 0 2px 8px rgba(0, 0, 0, 0.3), 0 0 0 2px var(--f-cy);
  animation: orbpulse 1.4s ease-in-out infinite;
}
@keyframes orbpulse {
  0%, 100% { box-shadow: 0 2px 8px rgba(0, 0, 0, 0.3), 0 0 0 2px var(--f-cy); }
  50% { box-shadow: 0 2px 8px rgba(0, 0, 0, 0.3), 0 0 0 4px rgba(var(--accent-rgb), 0.32); }
}
.bar-text {
  flex: 1;
  min-width: 0;
  font-size: 12.5px;
  color: var(--f-txt);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
  pointer-events: none;
}
.bar-text.hot { color: var(--f-cy); }
.bar-text.dim { color: var(--f-txt2); }
.bar-dot {
  flex: 0 0 auto;
  min-width: 15px;
  height: 15px;
  padding: 0 4px;
  border-radius: 8px;
  background: rgba(var(--accent-rgb), 0.22);
  color: var(--f-cy);
  font-size: 10px;
  display: flex;
  align-items: center;
  justify-content: center;
  pointer-events: none;
}
.bar-badge { flex: 0 0 auto; font-size: 12.5px; pointer-events: none; }
.pct {
  flex: 0 0 auto;
  font-style: normal;
  font-family: ui-monospace, "SF Mono", monospace;
  color: var(--f-cy);
  font-size: 11px;
  pointer-events: none;
}

/* —— 聆听波形:胶囊条 + 面板共用,高度由 voice.level 实时驱动 —— */
.wave { flex: 0 0 auto; display: flex; align-items: center; gap: 2px; height: 16px; pointer-events: none; }
.wave i { display: block; width: 2px; border-radius: 1px; background: var(--f-cy); transition: height 0.09s linear; }

/* —— 展开内容:毛玻璃面板 —— */
.body {
  flex: 1;
  min-height: 0;
  position: relative;
  margin: 0 10px 10px; /* 四周留 10px:给"小耳朵"挂外角的余地(下/上展开各让一边) */
  padding: 8px;
  border-radius: 14px;
  background: var(--f-glass);
  border: 1px solid var(--f-line);
  backdrop-filter: blur(12px);
  -webkit-backdrop-filter: blur(12px);
  /* 不放外 box-shadow:窗口只比面板大一圈,外投影会被窗口矩形裁成硬边"方块影"(同形态 C 踩坑②)——玻璃 + 描边分层已足够 */
  overflow-y: auto;
  display: flex;
  flex-direction: column;
  gap: 5px;
}
.shell.up .body { margin: 10px 10px 0; } /* 向上展开:内容在上,顶部留边 */
.body::-webkit-scrollbar { width: 6px; }
.body::-webkit-scrollbar-thumb { background: rgba(var(--accent-rgb), 0.2); border-radius: 3px; }

/* 区小标题:正在进行 / 最近消息(多条时分得清) */
.ptag {
  font-size: 11px;
  color: var(--f-txt2);
  letter-spacing: 0.03em;
  padding: 2px 2px 0;
}

/* —— 关闭"小耳朵":挂面板外角的小圆钮,半在面板半探出(随展开方向上/下) —— */
.ear {
  position: absolute;
  right: 1px;
  bottom: 1px; /* 向下展开:面板在下,耳朵挂右下外角 */
  z-index: 3;
  width: 18px;
  height: 18px;
  padding: 0;
  border-radius: 50%;
  background: var(--f-solid);
  border: 0.5px solid var(--f-line);
  color: var(--f-txt2);
  font-size: 10px;
  line-height: 1;
  cursor: pointer;
  display: flex;
  align-items: center;
  justify-content: center;
}
.ear.up { bottom: auto; top: 1px; } /* 向上展开:面板在上,耳朵挂右上外角 */
.ear:hover { color: var(--attn); border-color: var(--attn); }

.notice {
  display: flex;
  align-items: flex-start;
  gap: 6px;
  padding: 7px 8px;
  border-radius: 9px;
  background: rgba(var(--accent-rgb), 0.06);
  border: 1px solid var(--f-line);
  cursor: pointer;
  transition: border-color 0.15s, background 0.15s;
}
.notice:hover { border-color: var(--f-cy); background: rgba(var(--accent-rgb), 0.12); }
.n-text {
  flex: 1;
  font-size: 12.5px;
  line-height: 1.45;
  color: var(--f-txt);
  display: -webkit-box;
  -webkit-line-clamp: 2;
  -webkit-box-orient: vertical;
  overflow: hidden;
}
.n-x {
  flex: 0 0 auto;
  background: none;
  border: none;
  color: var(--f-txt2);
  cursor: pointer;
  font-size: 10px;
  padding: 1px 3px;
  line-height: 1;
}
.n-x:hover { color: var(--attn); }

.status {
  display: flex;
  align-items: center;
  gap: 7px;
  font-size: 11.5px;
  color: var(--f-txt2);
  padding: 2px 4px;
}
.status i { font-style: normal; flex: 0 0 auto; }
.status .ellip { flex: 1; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.status em {
  font-style: normal;
  font-family: ui-monospace, "SF Mono", monospace;
  color: var(--f-cy);
  font-size: 11px;
}
/* 迷你播控钮:幽灵小钮,贴在"正在放"行尾(展开面板内,可点) */
.mctl {
  flex: 0 0 auto;
  width: 18px;
  height: 18px;
  padding: 0;
  border: none;
  border-radius: 5px;
  background: rgba(var(--accent-rgb), 0.1);
  color: var(--f-txt);
  font-size: 10px;
  line-height: 1;
  cursor: pointer;
  display: flex;
  align-items: center;
  justify-content: center;
}
.mctl:hover { background: rgba(var(--accent-rgb), 0.22); color: var(--f-cy); }
.empty { font-size: 11.5px; color: var(--f-txt2); text-align: center; padding: 18px 0; }
</style>
