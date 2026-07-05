<script setup lang="ts">
import { ref, computed, onMounted, onUnmounted, nextTick, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import { useChat, type TurnStats, type UiMessage, type UiAttachment } from '../composables/useChat'
import { useSettings } from '../composables/useSettings'
import { onTranscribed, useVoice } from '../composables/useVoice'
import { useSpeech } from '../composables/useSpeech'
import { useContextMenu } from '../composables/useContextMenu'
import { useCharacter } from '../composables/useCharacter'
import { useMedia } from '../composables/useMedia'
import { fmtMs, fmtTokens, fmtUsd } from '../lib/fmt'
import { openExternal, api, type SearchHit } from '../lib/backend'
import { renderMarkdown } from '../lib/md'
import { copyText } from '../lib/clipboard'
import MemoryView from '../views/MemoryView.vue'
import OpsView from '../views/OpsView.vue'
import RemindersView from '../views/RemindersView.vue'
import SettingsView from '../views/SettingsView.vue'
import PlayerBar from './PlayerBar.vue'
import UsageStrip from './UsageStrip.vue'
import PetRoamer from './PetRoamer.vue'

// 主界面骨架。数据源 = useChat(VM):Tauri 壳里走真 IPC,纯浏览器预览自动降级假数据。
defineProps<{ booting?: boolean }>()

const { t, te } = useI18n()

const settings = useSettings()
const panelOpen = ref(true)
const shape = computed(() => (settings.get('ui.bubble_shape') === 'cut' ? 'cut' : 'round'))
const petName = computed(() => settings.get('ui.pet_name') || t('pet.name'))
const textScale = computed(() => (settings.get('ui.text_scale') === 'large' ? '16.5px' : '14px'))
const activeRail = ref<'chat' | 'reminders' | 'memory' | 'ops' | 'settings'>('chat')

const { state: chat, send: chatSend, cancel, selectConversation, newConversation, ensureVoiceConv, saveApiKey, dequeue, inject, renameConversation, togglePinConversation, deleteConversation } = useChat()
const messages = computed(() => chat.messages)

// 日期分隔条文案:今天 / 昨天 / 月-日(跨年带年份)。core 不产文案,这里走 i18n。
function dayLabel(ts: number): string {
  const d = new Date(ts)
  const now = new Date()
  const yest = new Date(now.getFullYear(), now.getMonth(), now.getDate() - 1)
  if (d.toDateString() === now.toDateString()) return t('time.today')
  if (d.toDateString() === yest.toDateString()) return t('time.yesterday')
  return d.getFullYear() === now.getFullYear()
    ? `${d.getMonth() + 1}/${d.getDate()}`
    : `${d.getFullYear()}/${d.getMonth() + 1}/${d.getDate()}`
}

// 消息流布局派生(模板照旧 v-for messages,不改结构):① 跨天在该条前插日期分隔条;
// ② 相邻同角色打 .cont(收紧间距,读出"一轮里连续几句")。都按 message id 索引。
const streamLayout = computed(() => {
  const sep: Record<number, string> = {}
  const cont = new Set<number>()
  let lastDay = ''
  let lastRole = ''
  for (const m of chat.messages) {
    if (m.at) {
      const day = new Date(m.at).toDateString()
      if (day !== lastDay) {
        // 顶部首条若就是"今天"不立分隔(免得每次打开都顶一条"今天");老会话/真跨天才立
        if (lastDay !== '' || day !== new Date().toDateString()) {
          sep[m.id] = dayLabel(m.at)
          lastRole = '' // 分隔之后第一条不算同角色续接
        }
        lastDay = day
      }
    }
    if (lastRole === m.role) cont.add(m.id)
    lastRole = m.role
  }
  return { sep, cont }
})

// —— 会话列表右键菜单(桌面右键):重命名(行内改名)/ 钉住 / 删除 ——
const { openMenu } = useContextMenu()
const renamingId = ref<number | null>(null)
const renameText = ref('')
const renameInput = ref<HTMLInputElement | null>(null)

function openConvMenu(e: MouseEvent, s: { id: number; title: string; pinned: boolean }) {
  openMenu(e, [
    { label: t('ctx.rename'), action: () => startRename(s) },
    { label: s.pinned ? t('ctx.unpin') : t('ctx.pin'), action: () => togglePinConversation(s.id) },
    { separator: true },
    { label: t('ctx.delete'), danger: true, action: () => deleteConversation(s.id) },
  ])
}
function startRename(s: { id: number; title: string }) {
  renamingId.value = s.id
  renameText.value = s.title || ''
  nextTick(() => {
    renameInput.value?.focus()
    renameInput.value?.select()
  })
}
function commitRename() {
  if (renamingId.value == null) return
  void renameConversation(renamingId.value, renameText.value)
  renamingId.value = null
}
function cancelRename() {
  renamingId.value = null
}

const input = ref('')
const pending = ref<UiAttachment[]>([])
const fileInput = ref<HTMLInputElement | null>(null)
const inputEl = ref<HTMLTextAreaElement | null>(null) // 多行输入框:Enter 发送、Shift+Enter 换行、随内容长高
const MAX_ATT = 12 * 1024 * 1024 // 单文件 12MB 上限,别把大文件灌进上下文

// 输入框随内容自适应高度:先归零再按 scrollHeight 撑开,封顶 ~5 行后内部滚动(不顶走聊天区)
const INPUT_MAX_H = 132
function autoGrow() {
  const el = inputEl.value
  if (!el) return
  el.style.height = 'auto'
  el.style.height = Math.min(el.scrollHeight, INPUT_MAX_H) + 'px'
}
// 回车键位(纯打字便利,与语音二分无关):Enter 发送、Shift+Enter 换行;
// 输入法选词中(isComposing / keyCode 229)绝不当发送 —— 否则中文用户每次选词回车都误发。
function onInputKeydown(e: KeyboardEvent) {
  if (e.key !== 'Enter' || e.shiftKey || e.isComposing || e.keyCode === 229) return
  e.preventDefault()
  send()
}

function send() {
  const text = input.value.trim()
  if (!text && pending.value.length === 0) return
  chatSend(text, 'typed', undefined, pending.value) // 流中再发 = 自动取消旧回合(partial 先落库)
  input.value = ''
  pending.value = []
  nextTick(autoGrow) // 清空后缩回单行高度
}

// 加图片/文件:选择器 / 粘贴 / 拖拽 三入口,统一读成 dataUrl(图预览)+ base64(出站)
function openPicker() {
  fileInput.value?.click()
}
function onPick(e: Event) {
  const el = e.target as HTMLInputElement
  if (el.files) addFiles(el.files)
  el.value = '' // 同名文件可再次选
}
function addFiles(files: FileList | File[]) {
  for (const f of Array.from(files)) {
    if (f.size > MAX_ATT) continue // 超限静默跳过(将来给个轻提示)
    const reader = new FileReader()
    reader.onload = () => {
      const dataUrl = String(reader.result)
      const base64 = dataUrl.slice(dataUrl.indexOf(',') + 1)
      const mime = f.type || ''
      // 截图/粘贴的 blob 偶尔无名 → 派生个中性名(展示标签,§6.5 豁免),免空白小票
      const name = f.name || `clip.${mime.split('/')[1] || 'png'}`
      pending.value.push({ kind: mime.startsWith('image/') ? 'image' : 'doc', name, mime, dataUrl, base64 })
    }
    reader.readAsDataURL(f)
  }
}
function removePending(i: number) {
  pending.value.splice(i, 1)
}
// 从 clipboard/drag 的 DataTransfer 收文件:既看 .files(从文件管理器复制的文件多在此),
// 也扫 .items 里 kind==='file' 的——**截图/复制的图片走 items、.files 常为空**(头号坑),
// 按 名+大小 去重(两边都报同一文件时)。
function collectFiles(dt: DataTransfer): File[] {
  const out: File[] = []
  const seen = new Set<string>()
  const push = (f: File | null) => {
    if (!f) return
    const k = `${f.name}|${f.size}`
    if (seen.has(k)) return
    seen.add(k)
    out.push(f)
  }
  for (const f of Array.from(dt.files)) push(f)
  for (const it of Array.from(dt.items)) if (it.kind === 'file') push(it.getAsFile())
  return out
}
function onPaste(e: ClipboardEvent) {
  if (!e.clipboardData) return
  const files = collectFiles(e.clipboardData)
  if (files.length) {
    addFiles(files)
    e.preventDefault() // 只在真有文件时拦,纯文本粘贴照常
  }
}

// 拖入高亮:整块聊天区都当 drop 区(不只输入行)。enter/leave 用计数,避免划过子元素时 leave 冒泡导致闪烁。
const dragging = ref(false)
let dragDepth = 0
function dragHasFiles(e: DragEvent) {
  return Array.from(e.dataTransfer?.types ?? []).includes('Files')
}
function onDragEnter(e: DragEvent) {
  if (!dragHasFiles(e)) return // 拖文本/选区进来不亮(只对文件)
  dragDepth++
  dragging.value = true
}
function onDragLeave() {
  dragDepth = Math.max(0, dragDepth - 1)
  if (dragDepth === 0) dragging.value = false
}
function onDrop(e: DragEvent) {
  dragDepth = 0
  dragging.value = false
  const files = e.dataTransfer ? collectFiles(e.dataTransfer) : []
  if (files.length) addFiles(files)
}

// —— 语音(PLAN §11):听写(mic,UI 交互不念)与唤醒(wake,语音会话必念)
//    都从这里进既有 send 链;via 决定回合形态(二分纪律) ——
const voice = useVoice()
onTranscribed(async (text, via, speaker) => {
  if (via === 'wake') await ensureVoiceConv() // 唤醒走语音专属会话;mic/打字进当前会话(交互二分)
  chatSend(text, via, speaker)
})

// —— 朗读(B 期):状态/停念/重听;正在念时点麦克风 = 停念+开听(一步打断) ——
const speech = useSpeech()
function micToggle() {
  if (speech.state.playing) speech.abort()
  voice.toggle()
}
function replay(text: string) {
  speech.speakText(text)
}

// 气泡里 markdown 链接:WebView 直接导航会把整个 app 顶走,一律拦下(preventDefault 保命);
// http(s) 链接交系统浏览器(openExternal:Tauri 走 opener 插件,只放行 http(s))
function onStreamClick(e: MouseEvent) {
  const a = (e.target as HTMLElement | null)?.closest('a[href]') as HTMLAnchorElement | null
  if (!a) return
  e.preventDefault()
  void openExternal(a.getAttribute('href') || '')
}

// 「想了想」漏出(PLAN §9):折叠药丸只露"想了想·N 步"(§3 干净默认);
// 展开 = 工具名/入参/结果 + CoT 原文(给好奇/专业用户;非专业用户不必点开)。展开态按 message id 记
const traceOpen = ref<Set<number>>(new Set())
function toggleTrace(id: number) {
  if (traceOpen.value.has(id)) traceOpen.value.delete(id)
  else traceOpen.value.add(id)
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

// 聊天搜索:输入即查(去抖 180ms),跨会话子串命中;点结果跳该会话并清空搜索框。
const searchQuery = ref('')
const searchHits = ref<SearchHit[]>([])
let searchTimer: ReturnType<typeof setTimeout> | undefined
watch(searchQuery, (q) => {
  clearTimeout(searchTimer)
  const query = q.trim()
  if (!query || !chat.inTauri) {
    searchHits.value = []
    return
  }
  searchTimer = setTimeout(async () => {
    try {
      searchHits.value = await api.searchMessages(query, 50)
    } catch (e) {
      console.error('搜索失败', e)
      searchHits.value = []
    }
  }, 180)
})
async function openHit(convId: number) {
  await selectConversation(convId)
  searchQuery.value = ''
  searchHits.value = []
}

// 起步建议气泡(发现性,§3.2「替用户说一句话」):空会话(还没用户消息)才显;点一下=替用户发出去。
const suggestions = computed(() => [
  t('chat.suggest.s1'),
  t('chat.suggest.s2'),
  t('chat.suggest.s3'),
  t('chat.suggest.s4'),
  t('chat.suggest.s5'),
  t('chat.suggest.s6'),
])
const showSuggestions = computed(
  () => chat.ready && chat.hasApiKey && !messages.value.some((m) => m.role === 'user'),
)
function sendSuggestion(text: string) {
  chatSend(text, 'typed')
}

// 场景触发建议气泡(§3.3 发现性):跟着当下状态走、替用户说下一句。点一下=正常发送。
// 档位(specific 先于 ambient):刚整理完文件 > 刚设好提醒 > 正在放歌;回合收尾(idle)才显,
// 别在旺财还在动手时就催下一句。扩展点:再加档 = 加一个触发判断 + 一组 chips。
const { state: media } = useMedia()
// 最新一条旺财消息动过哪些工具(live 与落库回放都带 ui_key;user 之后的新回合会换新气泡)
const lastWangToolKeys = computed<Set<string>>(() => {
  const list = messages.value
  for (let i = list.length - 1; i >= 0; i--) {
    const m = list[i]
    if (m.role === 'wang') return new Set((m.trace?.steps ?? []).map((s) => s.ui_key))
  }
  return new Set()
})
// fs 写原语(动过 = 「刚整理完文件」档;fs_undo 不算触发,撤销完就别再劝撤销)
const FS_WRITE_KEYS = [
  'tool.fs_move',
  'tool.fs_copy',
  'tool.fs_trash',
  'tool.fs_write_text',
  'tool.fs_append',
  'tool.fs_edit',
  'tool.fs_mkdir',
]
const contextSuggestions = computed<string[]>(() => {
  if (!chat.ready || !chat.hasApiKey) return []
  // 主动关怀总开关关 → 不出场景续接 chips(PLAN ★主动关怀里程碑,切片1 A 部分)
  if (settings.get('care.enabled') === '0') return []
  // 回合进行中不出气泡(等收尾再给「下一句」建议)
  if (chat.mood !== 'idle') return []
  const keys = lastWangToolKeys.value
  // 刚动过文件:看改了什么 / 撤销(可逆是功能性的,§7.2)
  if (FS_WRITE_KEYS.some((k) => keys.has(k))) {
    return [t('chat.ctx.fsChanged'), t('chat.ctx.fsUndo')]
  }
  // 刚设好提醒:看全部 / 反悔取消
  if (keys.has('tool.reminder_set')) {
    return [t('chat.ctx.remindList'), t('chat.ctx.remindCancel')]
  }
  // 正在放歌(音频形态):给音乐跟进建议
  if (media.current?.kind === 'audio' && (media.status === 'playing' || media.status === 'paused')) {
    return [t('chat.ctx.moreLike'), t('chat.ctx.calmer'), t('chat.ctx.somethingElse')]
  }
  return []
})

// 用户消息 hover 的精确时刻:当天显 HH:MM,跨天加日期(比相对时间更"看时间")
function fmtClock(ts?: number): string {
  if (!ts) return ''
  const d = new Date(ts)
  const pad = (n: number) => String(n).padStart(2, '0')
  const hm = `${pad(d.getHours())}:${pad(d.getMinutes())}`
  return d.toDateString() === new Date().toDateString() ? hm : `${d.getMonth() + 1}/${d.getDate()} ${hm}`
}

// 复制用户消息原文;成功闪一下 ✓。优先 async clipboard,失败(无焦点/旧环境)兜底 execCommand
const copiedId = ref<number | null>(null)
// 用户气泡 hover 的复制钮:复制整条 + 闪一下 ✓(copyText 抽到 lib/clipboard 共用)
function copyMsg(m: UiMessage) {
  copyText(m.text, () => {
    copiedId.value = m.id
    setTimeout(() => {
      if (copiedId.value === m.id) copiedId.value = null
    }, 1500)
  })
}

// 气泡右键(双方):有选中文本则复制选中片段,否则整条;助手气泡再给「朗读」
function openBubbleMenu(e: MouseEvent, m: UiMessage) {
  const sel = window.getSelection()?.toString().trim() ?? ''
  const items = [{ label: sel ? t('ctx.copySelection') : t('ctx.copy'), action: () => copyText(sel || m.text) }]
  if (m.role === 'wang' && m.text && chat.inTauri) {
    items.push({ label: t('ctx.readAloud'), action: () => replay(m.text) })
  }
  openMenu(e, items)
}

// —— 桌宠:漫游逻辑已抽到 ./PetRoamer.vue,形象态归 useCharacter(头像与桌宠共用)。
//    这里只留聊天滚动容器引用(兼作桌宠漫游边界)+ 桌宠右键菜单 + 隐藏开关 ——
const streamEl = ref<HTMLElement | null>(null)
const { pack, switchCharacter } = useCharacter()
const petHidden = computed(() => settings.get('ui.pet.hidden') === '1')

// 桌宠 / 头像右键:换形象 / 打开设置 / 隐藏桌宠(隐藏=置 ui.pet.hidden,设置页可恢复)
function openPetMenu(e: MouseEvent) {
  openMenu(e, [
    { label: t('ctx.switchChar'), action: switchCharacter },
    { label: t('ctx.openSettings'), action: () => { activeRail.value = 'settings' } },
    { separator: true },
    { label: t('ctx.hidePet'), action: () => void settings.set('ui.pet.hidden', '1') },
  ])
}

onMounted(() => window.addEventListener('keydown', onVoiceKey))
onUnmounted(() => window.removeEventListener('keydown', onVoiceKey))
let lastLen = 0
watch(messages, () => nextTick(() => {
  const s = streamEl.value
  if (!s) return
  // 新气泡无条件贴底;流式增量只在"本来就在底部附近"时跟随,不打断用户翻历史
  const newBubble = chat.messages.length !== lastLen
  lastLen = chat.messages.length
  if (newBubble || s.scrollHeight - s.scrollTop - s.clientHeight < 90) s.scrollTop = s.scrollHeight
}), { deep: true })
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
        <button class="rb" :class="{ on: activeRail === 'reminders' }" @click="activeRail = 'reminders'" :title="t('nav.reminders')">
          <svg viewBox="0 0 24 24"><circle cx="12" cy="13" r="8" /><path d="M12 9v4l2.5 1.5" /><path d="M5 4.5 8 7M19 4.5 16 7" /></svg>
          <span>{{ t('nav.reminders') }}</span>
        </button>
        <button class="rb" :class="{ on: activeRail === 'memory' }" @click="activeRail = 'memory'" :title="t('nav.memory')">
          <svg viewBox="0 0 24 24"><path d="M7 4h10v16l-5-3-5 3z" /></svg>
          <span>{{ t('nav.memory') }}</span>
        </button>
        <button class="rb" :class="{ on: activeRail === 'ops' }" @click="activeRail = 'ops'" :title="t('nav.ops')">
          <!-- 足迹:两只脚印(斜向walk),不再用钟表——免与上面「提醒」的闹钟撞图 -->
          <svg viewBox="0 0 24 24"><g transform="translate(8 13.5) rotate(-16)"><ellipse cx="0" cy="-1.9" rx="2.1" ry="2.6" /><ellipse cx="-0.1" cy="2.5" rx="1.2" ry="1.5" /></g><g transform="translate(15.6 9) rotate(-16)"><ellipse cx="0" cy="-1.9" rx="2.1" ry="2.6" /><ellipse cx="-0.1" cy="2.5" rx="1.2" ry="1.5" /></g></svg>
          <span>{{ t('nav.ops') }}</span>
        </button>
      </div>
      <button class="rb gear" :class="{ on: activeRail === 'settings' }" :title="t('nav.settings')" @click="activeRail = 'settings'">
        <svg viewBox="0 0 24 24"><circle cx="12" cy="12" r="7" /><circle cx="12" cy="12" r="2.6" /></svg>
        <span>{{ t('nav.settings') }}</span>
        <!-- 唯一脉冲:缺钥匙时指路(与设置页大脑 tab 同一信号) -->
        <span v-if="chat.ready && !chat.hasApiKey" class="gear-dot"></span>
      </button>
    </nav>

    <!-- 中:最近(可关;设置/回忆/记录/提醒页打开时整列让位) -->
    <aside class="recents" v-show="panelOpen && activeRail !== 'settings' && activeRail !== 'memory' && activeRail !== 'ops' && activeRail !== 'reminders'">
      <header class="rc-head">
        <span>{{ t('recents.title') }}</span>
        <button class="collapse" @click="panelOpen = false" :title="t('recents.collapse')">‹</button>
      </header>
      <div class="rc-search">
        <svg class="rc-search-ic" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
          <circle cx="11" cy="11" r="7" />
          <path d="m21 21-4.3-4.3" />
        </svg>
        <input v-model="searchQuery" class="rc-search-input" :placeholder="t('recents.searchPlaceholder')" />
        <button v-if="searchQuery" class="rc-search-clear" @click="searchQuery = ''" :title="t('recents.searchClear')">×</button>
      </div>
      <ul v-if="searchQuery.trim()" class="rc-list rc-results">
        <li v-for="(h, hi) in searchHits" :key="hi" @click="openHit(h.conversation_id)">
          <span class="rc-title">{{ h.conversation_title || t('recents.untitled') }}</span>
          <div class="rc-snippet">{{ h.snippet }}</div>
          <span class="rc-time">{{ fmtTime(h.created_at) }}</span>
        </li>
        <li v-if="!searchHits.length" class="rc-empty">{{ t('recents.searchEmpty') }}</li>
      </ul>
      <ul v-else class="rc-list">
        <li
          v-for="s in chat.conversations"
          :key="s.id"
          :class="{ on: s.id === chat.convId, pinned: s.pinned }"
          @click="selectConversation(s.id)"
          @contextmenu="openConvMenu($event, s)"
        >
          <!-- 钉住标:右上角图钉(描边、跟随主题色,与渠道图标同语言) -->
          <span v-if="s.pinned" class="rc-pin" :title="t('recents.pinned')">
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
              <path d="M9 4h6M10 4v6l-3 3h10l-3-3V4M12 16v4" />
            </svg>
          </span>
          <!-- 重命名:行内变输入框(回车/失焦提交,Esc 取消);否则显标题 -->
          <input
            v-if="renamingId === s.id"
            ref="renameInput"
            v-model="renameText"
            class="rc-rename"
            @click.stop
            @keyup.enter="commitRename"
            @keyup.esc="cancelRename"
            @blur="commitRename"
          />
          <span v-else class="rc-title">{{ s.title || t('recents.untitled') }}</span>
          <div class="rc-meta">
            <!-- 发起人显性化:家人 / 渠道指认的人显名字;系统会话显「系统」;主人自己的会话不显(是你)。 -->
            <span v-if="s.owner_name || s.channel === 'system'" class="rc-owner">{{ s.owner_name || t('channel.system') }}</span>
            <span class="rc-time">{{ fmtTime(s.updated_at) }}</span>
            <!-- 有动静标:不在该会话时,后台/切走的回合收尾打标(done=完成 failed=失败),进入即清 -->
            <span
              v-if="chat.convBadges[s.id]"
              class="rc-badge"
              :class="'rc-badge-' + chat.convBadges[s.id]"
              :title="t('recents.badge.' + chat.convBadges[s.id])"
            />
            <span
              v-if="s.channel && s.channel !== 'ui'"
              class="rc-chan"
              :class="'rc-chan-' + s.channel"
              :title="t('channel.' + s.channel)"
            >
              <svg v-if="s.channel === 'voice'" class="rc-chan-ic" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                <rect x="9" y="2.5" width="6" height="11" rx="3" />
                <path d="M5.5 11a6.5 6.5 0 0 0 13 0" />
                <path d="M12 17.5V21" />
              </svg>
              <svg v-else-if="s.channel === 'system'" class="rc-chan-ic" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                <path d="M6 9.5a6 6 0 0 1 12 0c0 4.4 1.8 5.5 1.8 5.5H4.2S6 13.9 6 9.5Z" />
                <path d="M10.2 19a2 2 0 0 0 3.6 0" />
              </svg>
              <svg v-else-if="s.channel === 'telegram'" class="rc-chan-ic" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                <path d="M22 3 11 14" />
                <path d="M22 3 15 21l-4-8-8-4 19-6Z" />
              </svg>
              <svg v-else-if="s.channel === 'dingtalk'" class="rc-chan-ic" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                <path d="M20 4H4a1 1 0 0 0-1 1v10a1 1 0 0 0 1 1h3v4l5-4h8a1 1 0 0 0 1-1V5a1 1 0 0 0-1-1Z" />
              </svg>
              <span v-else class="rc-chan-dot" />
            </span>
          </div>
        </li>
      </ul>
      <button class="rc-new" @click="newConversation()">{{ t('recents.newTopic') }}</button>
    </aside>

    <!-- 设置台/回忆页:rail 目的地,整区接管(聊天 v-show 保活,状态无损) -->
    <SettingsView v-if="activeRail === 'settings'" @close="activeRail = 'chat'" />
    <MemoryView v-if="activeRail === 'memory'" @close="activeRail = 'chat'" />
    <OpsView v-if="activeRail === 'ops'" @close="activeRail = 'chat'" />
    <RemindersView v-if="activeRail === 'reminders'" @close="activeRail = 'chat'" />

    <!-- 右:对话主体 -->
    <main
      class="chat"
      v-show="activeRail !== 'settings' && activeRail !== 'memory' && activeRail !== 'ops' && activeRail !== 'reminders'"
      @dragenter.prevent="onDragEnter"
      @dragover.prevent
      @dragleave="onDragLeave"
      @drop.prevent="onDrop"
    >
      <!-- 拖文件进来时整块聊天区高亮(pointer-events:none → 不抢 drop/leave 事件) -->
      <div v-if="dragging" class="drop-veil"><span>{{ t('chat.dropHint', { name: petName }) }}</span></div>
      <header class="chat-head" data-tauri-drag-region>
        <button v-if="!panelOpen" class="reopen" @click="panelOpen = true" :title="t('recents.expand')">›</button>
        <img :src="pack.idle[0]" class="head-av" :alt="petName" :title="t('avatar.switchTitle')" style="height: 46px; width: auto; cursor: pointer;" @click="switchCharacter" @contextmenu="openPetMenu($event)" />
        <div class="who"><b>{{ petName }}</b><small><span class="led"></span>{{ statusText }}</small></div>
      </header>

      <div class="stream" ref="streamEl" @click="onStreamClick">
        <template v-for="(m, mi) in messages" :key="m.id">
          <div v-if="streamLayout.sep[m.id]" class="day-sep"><span>{{ streamLayout.sep[m.id] }}</span></div>
          <div class="bubble" :class="[m.role, { cont: streamLayout.cont.has(m.id) }]" @contextmenu="openBubbleMenu($event, m)">
          <!-- 说话人显性化:user 标非我的说话人名(家人插话 / 声纹 / 渠道归人);wang 标自动触发「⏰ 提醒」。
               「我」说的 + 旺财主动回的都不标,保持干净——只在需要区分时才冒出标签。 -->
          <span v-if="m.role === 'user' && m.speakerName" class="spk-tag spk-who">{{ m.speakerName }}</span>
          <span v-if="m.role === 'wang' && m.trigger" class="spk-tag spk-trig">
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><circle cx="12" cy="13" r="7" /><path d="M12 9.5V13l2 1.5" /><path d="M5 4 2.5 6.5" /><path d="M19 4 21.5 6.5" /></svg>
            {{ t('chat.trigger.' + m.trigger) }}
          </span>
          <!-- wang 走富文本(markdown);user 是用户原话,纯文本保留换行、不解析标记 -->
          <div v-if="m.role === 'wang'" class="md" v-html="renderMarkdown(m.text)"></div>
          <template v-else>
            <div v-if="m.attachments?.length" class="atts">
              <template v-for="(a, ai) in m.attachments" :key="ai">
                <img v-if="a.kind === 'image' && a.dataUrl" :src="a.dataUrl" class="att-img" alt="" />
                <span v-else class="att-chip">
                  <svg v-if="a.kind === 'image'" viewBox="0 0 24 24"><rect x="3" y="5" width="18" height="14" rx="2" /><path d="M3 16l5-4 4 3 3-2 6 5" /></svg>
                  <svg v-else viewBox="0 0 24 24"><path d="M6 2h8l4 4v16H6z" /><path d="M14 2v4h4" /></svg>
                  {{ a.name }}
                </span>
              </template>
            </div>
            <div v-if="m.text" class="usertext">{{ m.text }}</div>
          </template>
          <!-- 用户消息 hover:复制 + 时间(右下浮现,与 wang 的读数/重听同款克制) -->
          <span v-if="m.role === 'user'" class="user-meta">
            <button class="copy-btn" :class="{ done: copiedId === m.id }" @click="copyMsg(m)" :title="t('chat.copy')">
              <svg v-if="copiedId === m.id" viewBox="0 0 24 24"><path d="M5 12l4 4 10-10" /></svg>
              <svg v-else viewBox="0 0 24 24"><rect x="9" y="9" width="11" height="11" rx="2" /><path d="M15 9V6a2 2 0 0 0-2-2H6a2 2 0 0 0-2 2v7a2 2 0 0 0 2 2h3" /></svg>
            </button>
            <span v-if="m.at" class="u-time">{{ fmtClock(m.at) }}</span>
          </span>
          <!-- 「想了想」漏出:折叠药丸(干净默认)+ 展开技术细节(工具名/入参/结果 + CoT 原文) -->
          <div v-if="m.role === 'wang' && m.trace && (m.trace.steps.length || m.trace.reasoning)" class="think">
            <button class="think-pill" :class="{ open: traceOpen.has(m.id) }" @click="toggleTrace(m.id)">
              <svg class="think-i" viewBox="0 0 24 24"><path d="M9 18h6M10 21h4M12 3a6 6 0 0 0-4 10.5c.7.7 1 1.3 1 2.5h6c0-1.2.3-1.8 1-2.5A6 6 0 0 0 12 3z" /></svg>
              <span>{{ t('trace.title') }}<template v-if="m.trace.steps.length"> · {{ t('trace.steps', { n: m.trace.steps.length }) }}</template></span>
              <svg class="think-chev" viewBox="0 0 24 24"><path d="M6 9l6 6 6-6" /></svg>
            </button>
            <div v-if="traceOpen.has(m.id)" class="think-detail">
              <div v-for="(s, si) in m.trace.steps" :key="si" class="td-step">
                <div class="td-call">
                  <span class="td-name">{{ s.name }}</span>
                  <span v-if="s.args && s.args !== '{}'" class="td-args">{{ s.args }}</span>
                  <span v-if="s.status && s.status !== 'ok'" class="td-bad">{{ s.status }}</span>
                </div>
                <div v-if="s.result" class="td-result">{{ s.result }}</div>
              </div>
              <div v-if="m.trace.reasoning" class="td-cot">
                <div class="td-cot-h">{{ t('trace.reasoning') }}</div>
                <pre class="td-cot-body">{{ m.trace.reasoning }}</pre>
              </div>
            </div>
          </div>
          <!-- 完成的回复:读数默认隐身,hover 浮现;在飞的回复:跳秒常驻,不用 hover -->
          <span v-if="m.stats" class="turn-meta">{{ fmtStats(m.stats) }}</span>
          <span v-else-if="isLiveBubble(m, mi)" class="turn-meta live">{{ liveLine }}</span>
          <!-- 一键复制这条回复(与 user 气泡同款;右键菜单也有,这给个 hover 直达) -->
          <button
            v-if="m.role === 'wang' && m.text"
            class="copy-btn wang-copy"
            :class="{ done: copiedId === m.id }"
            @click="copyMsg(m)"
            :title="t('chat.copy')"
          >
            <svg v-if="copiedId === m.id" viewBox="0 0 24 24"><path d="M5 12l4 4 10-10" /></svg>
            <svg v-else viewBox="0 0 24 24"><rect x="9" y="9" width="11" height="11" rx="2" /><path d="M15 9V6a2 2 0 0 0-2-2H6a2 2 0 0 0-2 2v7a2 2 0 0 0 2 2h3" /></svg>
          </button>
          <!-- 朗读(把这条回复念出来;hover 浮现,缓存命中秒回) -->
          <button
            v-if="m.role === 'wang' && m.text && chat.inTauri"
            class="replay"
            :title="t('chat.replay')"
            @click="replay(m.text)"
          >
            <!-- 耳机:朗读 = 把这条念出来(听),与语音输入的话筒区分;默认不念,所以不是「重播」 -->
            <svg viewBox="0 0 24 24"><path d="M4.5 14v-2a7.5 7.5 0 0 1 15 0v2" /><rect x="3" y="13.5" width="3.6" height="6.6" rx="1.8" /><rect x="17.4" y="13.5" width="3.6" height="6.6" rx="1.8" /></svg>
          </button>
          </div>
        </template>
        <!-- 起步建议(发现性,§3.2「替用户说一句话」):空会话才显,一开口就消失;点一下替你发出去 -->
        <div v-if="showSuggestions" class="suggest">
          <button v-for="(s, si) in suggestions" :key="si" class="suggest-chip" @click="sendSuggestion(s)">{{ s }}</button>
        </div>
        <!-- 桌宠:漫游边界=聊天滚动区;不在聊天页时 paused 空转;隐藏=v-if 卸载(RAF 停) -->
        <PetRoamer v-if="!petHidden" :bounds="streamEl" :paused="activeRail !== 'chat'" />
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
        <!-- 场景触发建议气泡(§3.3):跟着当下状态(目前=正在放歌)给跟进 chips,点一下替你发出去 -->
        <div v-if="contextSuggestions.length" class="suggest suggest-ctx">
          <button v-for="(s, si) in contextSuggestions" :key="si" class="suggest-chip" @click="sendSuggestion(s)">{{ s }}</button>
        </div>
        <!-- 排队区(Phase A):7274 还在说时你发的消息,攒这儿,说完一起发;可逐条划掉 -->
        <div v-if="chat.queue.length" class="queue">
          <div class="q-head">
            <span>{{ t('chat.queueHint') }}</span>
            <button class="q-jump" @click="inject" :title="t('chat.queueJumpTitle')">{{ t('chat.queueJump') }}</button>
          </div>
          <div v-for="(q, i) in chat.queue" :key="i" class="q-item">
            <svg v-if="q.attachments.length" class="q-clip" viewBox="0 0 24 24"><path d="M8 12V7a4 4 0 0 1 8 0v9a6 6 0 0 1-12 0V8.5" /></svg>
            <span class="q-text">{{ q.text || t('chat.queueAtt') }}</span>
            <button class="q-x" @click="dequeue(i)" :title="t('chat.attRemove')">✕</button>
          </div>
        </div>
        <!-- 待发附件托盘:图缩略 + 文件小票,各带移除 -->
        <div v-if="pending.length" class="att-tray">
          <div v-for="(a, i) in pending" :key="i" class="att-pill">
            <img v-if="a.kind === 'image'" :src="a.dataUrl" class="att-thumb" alt="" />
            <svg v-else class="att-doc" viewBox="0 0 24 24"><path d="M6 2h8l4 4v16H6z" /><path d="M14 2v4h4" /></svg>
            <span class="att-name">{{ a.name }}</span>
            <button class="att-x" @click="removePending(i)" :title="t('chat.attRemove')">✕</button>
          </div>
        </div>
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
          <!-- 加图片/文件:隐藏 input + 小回形针(界面优先,附件是轻量入口) -->
          <input
            ref="fileInput"
            type="file"
            multiple
            class="file-hidden"
            accept="image/*,.pdf,.docx,.pptx,.xlsx,.txt,.md,.markdown,.json,.csv,.log,.rs,.py,.js,.ts,.vue,.html,.css,.yaml,.yml"
            @change="onPick"
          />
          <button class="attach-btn" @click="openPicker" :title="t('chat.attach')">
            <svg viewBox="0 0 24 24"><path d="M8 12V7a4 4 0 0 1 8 0v9a6 6 0 0 1-12 0V8.5" /></svg>
          </button>
          <!-- 语音输入 = 输入框内的小话筒(轻量,不跟发送键并排抢戏;界面优先,语音只是输入之一) -->
          <span class="field-wrap">
            <textarea
              ref="inputEl"
              v-model="input"
              class="field has-mic"
              rows="1"
              :placeholder="fieldPlaceholder"
              @keydown="onInputKeydown"
              @input="autoGrow"
              @paste="onPaste"
            ></textarea>
            <button class="mic-inline" @click="micToggle()" :title="t('chat.micTitle')">
              <svg viewBox="0 0 24 24"><rect x="9.2" y="3.2" width="5.6" height="10.4" rx="2.8" /><path d="M5.8 11.2a6.2 6.2 0 0 0 12.4 0M12 17.6v3.2M8.8 20.8h6.4" /></svg>
            </button>
          </span>
          <!-- 停止键覆盖两种"它在动"的状态:回合在飞 / 音频在念(点击都立即安静) -->
          <!-- 在飞且没东西可发 = 停止键;一旦有输入/附件 = 发送键(发出即进排队区,不打断) -->
          <button v-if="(chat.mood !== 'idle' || speech.state.playing) && !input.trim() && !pending.length" class="send stop" @click="cancel()" :title="t('chat.stop')">⏹</button>
          <button v-else class="send" @click="send" :disabled="!input.trim() && pending.length === 0" :title="t('chat.send')">➤</button>
        </div>
        <!-- 记账灯带:本轮消耗 / 今日累计 / 余额(数据缺席的段自己熄灯) -->
        <UsageStrip />
      </div>
    </main>
  </div>
</template>

<style scoped>
.layout {
  /* 主题 token 全在 style.css :root(科幻皮唯一色源);此处只留布局 */
  position: fixed; inset: 0; z-index: 5;
  display: flex; gap: 0;
  color: var(--text);
  font-family: -apple-system, "PingFang SC", "Segoe UI", sans-serif;
  font-size: 14px;
}
.layout.booting { animation: layIn .6s ease .5s backwards; }
@keyframes layIn { from { opacity: 0; transform: translateY(10px); } }

/* —— 左图标栏 —— */
.rail {
  flex: 0 0 72px; display: flex; flex-direction: column; justify-content: space-between;
  padding: 16px 0; background: var(--surface);
  backdrop-filter: blur(10px); -webkit-backdrop-filter: blur(10px);
  border-right: 1px solid var(--line);
}
.rail-top { display: flex; flex-direction: column; gap: 6px; }
.rb {
  background: rgba(var(--accent-rgb), 0.04); border: 1px solid var(--line); border-radius: 11px;
  cursor: pointer; color: var(--text-dim);
  display: flex; flex-direction: column; align-items: center; gap: 4px;
  width: 58px; margin: 0 auto; padding: 9px 0; font-size: 10px; letter-spacing: 1px;
  position: relative; transition: color .15s, border-color .15s, background .15s;
}
.rb svg { width: 21px; height: 21px; fill: none; stroke: currentColor; stroke-width: 1.7; stroke-linejoin: round; }
.rb:hover { color: var(--text); border-color: rgba(var(--accent-rgb), 0.4); }
.rb.on { color: var(--accent); border-color: rgba(var(--accent-rgb), 0.45); background: rgba(var(--accent-rgb), 0.1); }
.rb.on::after {
  content: ""; position: absolute; top: 0; left: 0; width: 5px; height: 5px; margin: -2.5px;
  border-radius: 50%; background: var(--accent); box-shadow: 0 0 8px 1px var(--accent);
  animation: orbit 3s linear infinite;
}
@keyframes orbit {
  0% { top: 0; left: 0; } 25% { top: 0; left: 100%; }
  50% { top: 100%; left: 100%; } 75% { top: 100%; left: 0; } 100% { top: 0; left: 0; }
}
/* 唯一脉冲:缺钥匙时齿轮上的琥珀光点 */
.gear-dot {
  position: absolute; top: 5px; right: 5px; width: 6px; height: 6px; border-radius: 50%;
  background: var(--warn); box-shadow: 0 0 8px var(--warn); animation: led 2.4s ease-in-out infinite;
}

/* —— 中:最近 —— */
.recents {
  /* 大屏不再死守 216:随视口温和放大,小屏维持 216、约 1.8K 宽封顶 320,免得聊天区独吞剩余宽度显失衡 */
  flex: 0 0 clamp(216px, 18vw, 320px); display: flex; flex-direction: column;
  background: transparent;
  border-right: 1px solid var(--line);
}
.rc-head { display: flex; align-items: center; justify-content: space-between; padding: 16px 16px 10px; font-size: 12px; letter-spacing: 2px; color: var(--text-dim); }
.collapse { background: none; border: none; color: var(--text-dim); cursor: pointer; font-size: 18px; line-height: 1; }
.collapse:hover { color: var(--accent); }
.rc-list { list-style: none; margin: 0; padding: 0 8px; flex: 1; overflow-y: auto; scrollbar-gutter: stable; }
.rc-list li {
  position: relative;
  margin-bottom: 8px; padding: 10px 12px; border-radius: 10px; cursor: pointer;
  display: flex; flex-direction: column; gap: 3px;
  background: var(--surface-2); border: 1px solid var(--line);
  backdrop-filter: blur(6px); -webkit-backdrop-filter: blur(6px);
  transition: border-color .15s, background .15s;
}
.rc-list li:hover { border-color: rgba(var(--accent-rgb), 0.4); }
.rc-list li.on { background: rgba(var(--accent-rgb), 0.12); border-color: rgba(var(--accent-rgb), 0.5); box-shadow: 0 0 12px rgba(var(--accent-rgb), 0.12); }
/* 钉住:左缘一道强调色细条 + 标题给图钉让出右内边距 */
.rc-list li.pinned { border-left: 2px solid rgba(var(--accent-rgb), 0.65); }
.rc-list li.pinned .rc-title { padding-right: 16px; }
.rc-pin { position: absolute; top: 9px; right: 10px; color: var(--accent); line-height: 0; opacity: .9; }
.rc-pin svg { width: 12px; height: 12px; display: block; }
/* 行内重命名输入:贴着标题位,克制描边,不破列表节奏 */
.rc-rename {
  font-size: 13px; color: var(--text); width: 100%;
  background: var(--surface-deep); border: 1px solid var(--accent); border-radius: 6px;
  padding: 2px 6px; outline: none; font-family: inherit;
}
.rc-title { font-size: 13px; color: var(--text); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
/* 时间行:时间靠左,渠道图标靠右(右下角,与时间同一行) */
.rc-meta { display: flex; align-items: center; justify-content: space-between; gap: 8px; }
/* 渠道小图标:界面=不渲染(基线);voice=强调色更显眼,system=克制 dim;未知渠道兜底小圆点 */
.rc-chan { display: inline-flex; flex: none; align-items: center; color: var(--text-dim); }
.rc-chan-ic { width: 11px; height: 11px; display: block; }
.rc-chan-voice { color: var(--accent); }
.rc-chan-system { color: var(--text-dim); }
.rc-chan-telegram,
.rc-chan-dingtalk { color: var(--accent); }
.rc-chan-dot { width: 7px; height: 7px; border-radius: 50%; background: currentColor; }
.rc-time { font-size: 11px; color: var(--text-dim); margin-right: auto; } /* 时间靠左,标记/渠道图标归右侧成组 */
/* 发起人显性化:家人 / 渠道指认的人 / 系统会话在标题下一行显名;主人自己的会话不显。 */
.rc-owner { font-size: 11px; color: var(--accent); opacity: 0.85; max-width: 84px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
/* 有动静标:发光小圆点(done=ok 青绿 / failed=danger 红),克制不抢标题;进入会话即清 */
.rc-badge { width: 8px; height: 8px; border-radius: 50%; flex: none; }
.rc-badge-done { background: var(--ok); box-shadow: 0 0 7px rgba(var(--ok-rgb), 0.85); }
.rc-badge-failed { background: var(--danger); box-shadow: 0 0 7px rgba(var(--danger-rgb), 0.85); }
.rc-new { margin: 10px; padding: 9px; border-radius: 10px; background: none; border: 1px dashed var(--line); color: var(--text-dim); cursor: pointer; font-size: 12.5px; }
.rc-new:hover { color: var(--accent); border-color: var(--accent); }
/* 聊天搜索:框 + 放大镜 + 清除;命中片段比标题暗一档 */
.rc-search { display: flex; align-items: center; gap: 6px; margin: 0 12px 8px; padding: 6px 9px; border-radius: 9px; background: var(--surface-deep); border: 1px solid var(--line); }
.rc-search:focus-within { border-color: rgba(var(--accent-rgb), 0.5); }
.rc-search-ic { width: 13px; height: 13px; flex: none; color: var(--text-dim); }
.rc-search-input { flex: 1; min-width: 0; background: none; border: none; outline: none; color: var(--text); font-size: 12.5px; font-family: inherit; }
.rc-search-input::placeholder { color: var(--text-dim); }
.rc-search-clear { flex: none; background: none; border: none; color: var(--text-dim); cursor: pointer; font-size: 15px; line-height: 1; padding: 0 2px; }
.rc-search-clear:hover { color: var(--accent); }
.rc-results .rc-snippet { font-size: 12px; color: var(--text-dim); line-height: 1.5; overflow: hidden; text-overflow: ellipsis; display: -webkit-box; -webkit-line-clamp: 2; -webkit-box-orient: vertical; }
.rc-empty { color: var(--text-dim); font-size: 12.5px; text-align: center; cursor: default; background: none !important; border: none !important; }
.rc-empty:hover { border-color: transparent !important; }

/* —— 右:对话 —— */
.chat { flex: 1; display: flex; flex-direction: column; min-width: 0; position: relative; }
.chat > * { position: relative; z-index: 1; }
.chat::before {
  content: ""; position: absolute; inset: 0; z-index: 0; pointer-events: none;
  background: linear-gradient(180deg, var(--veil-top), var(--veil-bottom));
}
/* 拖文件进来的高亮蒙层:盖住整块聊天区,虚线框 + 居中提示;pointer-events:none 不抢 drop/leave */
.drop-veil {
  position: absolute; inset: 10px; z-index: 6; pointer-events: none;
  display: flex; align-items: center; justify-content: center;
  border: 2px dashed rgba(var(--accent-rgb), 0.7); border-radius: 14px;
  background: rgba(var(--accent-rgb), 0.1);
}
.drop-veil span {
  padding: 10px 18px; border-radius: 10px;
  background: var(--surface); color: var(--accent); font-size: 15px; letter-spacing: .5px;
}
/* 右内边距留出右上角窗控三键的位置(无边框补窗控,PLAN §12) */
.chat-head { display: flex; align-items: center; gap: 10px; padding: 14px 84px 14px 20px; border-bottom: 1px solid var(--line); }
.head-av { transition: transform .15s; }
.reopen { background: none; border: 1px solid var(--line); color: var(--text-dim); cursor: pointer; border-radius: 8px; width: 26px; height: 26px; font-size: 16px; }
.reopen:hover { color: var(--accent); border-color: var(--accent); }
.who { display: flex; flex-direction: column; line-height: 1.25; }
.who b { font-size: 15px; color: var(--text); }

/* scrollbar-gutter:stable —— 内容撑满出现滚动条时不再左移跳动(全局 ::-webkit-scrollbar 已统一样式,不再各设一份) */
/* 间距改走气泡 margin(不用 gap):换角色拉开=turn 分组,同角色收紧;下边距给 hover 浮层留位 */
.stream { flex: 1; overflow-y: auto; scrollbar-gutter: stable; padding: 22px 20px 22px 26px; display: flex; flex-direction: column; gap: 0; position: relative; }
/* 起步建议气泡:贴在开场白下方,药丸样式跟随主题色(同 PlayerBar 登录 chip 语言) */
.suggest { display: flex; flex-wrap: wrap; gap: 8px; padding: 4px 2px; margin-top: 6px; }
.suggest-chip { padding: 7px 14px; border-radius: 999px; font-size: 12.5px; font-family: inherit; cursor: pointer; background: rgba(var(--accent-rgb), 0.1); border: 1px solid rgba(var(--accent-rgb), 0.32); color: var(--accent); transition: border-color .15s, box-shadow .15s, background .15s; }
.suggest-chip:hover { border-color: var(--accent); box-shadow: 0 0 12px rgba(var(--accent-rgb), 0.28); background: rgba(var(--accent-rgb), 0.16); }
.bubble {
  max-width: 70%; padding: 11px 15px; border-radius: 16px; font-size: 14px; line-height: 1.55;
  /* blur 是"磨砂"强度:大了会把背后星空/粒子糊没(看着像不透明)。科幻皮要透出背景动态,取小值;
     文字稳靠 --bubble-them 的 tint 兜底。warm 皮气泡本身不透(token 无 alpha),blur 在那边无副作用。 */
  backdrop-filter: blur(3px); -webkit-backdrop-filter: blur(3px);
  box-shadow: 0 6px 20px rgba(0, 0, 0, 0.28);
  word-break: break-word;
  position: relative;
  margin: 12px 0 19px; /* 上=换角色间距(turn 分组);下=给 hover 浮层(读数/耳机/复制)留位,防压到下一条 */
  transition: transform .18s ease-out;
}
.bubble:first-child { margin-top: 0; }
.bubble.cont { margin-top: 3px; } /* 相邻同角色:收紧,读出"一轮里连续几句" */
/* 回复读数:贴在气泡下沿,默认隐身,hover 浮现(不挤布局,不打扰陪伴感) */
.turn-meta {
  position: absolute; top: 100%; left: 13px; margin-top: 3px;
  font: 10px/1 ui-monospace, "SF Mono", monospace; letter-spacing: 0.6px;
  color: var(--accent); text-shadow: 0 0 8px rgba(var(--accent-rgb), 0.3);
  white-space: nowrap; pointer-events: none; user-select: none;
  opacity: 0; transform: translateY(-2px); transition: opacity .18s ease, transform .18s ease;
  z-index: 7;
  /* 窄气泡:读数比气泡宽会压到右下角的 复制/耳机 按钮(真机实锤)→ 给按钮区让位,
     超出省略号截断(72px ≈ 两个按钮 + 间距;% 相对气泡宽,宽气泡不受影响) */
  max-width: calc(100% - 72px); overflow: hidden; text-overflow: ellipsis;
}
.bubble:hover .turn-meta { opacity: 0.9; transform: translateY(0); }
/* 在飞读数:常驻可见,轻微呼吸 —— 跳秒本身就是"我在干活"的信号 */
.turn-meta.live { transform: translateY(0); animation: metaLive 1.6s ease-in-out infinite; }
@keyframes metaLive { 0%, 100% { opacity: 0.85; } 50% { opacity: 0.45; } }
.bubble.wang {
  align-self: flex-start; background: var(--bubble-them);
  border: 1px solid var(--line); border-bottom-left-radius: 5px; color: var(--text);
}
.bubble.user {
  align-self: flex-end; background: var(--bubble-me);
  border: 1px solid var(--bubble-me-line); border-bottom-right-radius: 5px; color: var(--bubble-me-text);
}
/* 说话人显性化:气泡内顶部小标签,只在需区分时出现(家人插话 / 自动触发)。
   user 侧(右)右对齐、wang 侧(左)左对齐;语义 token 跟随皮肤。 */
.spk-tag { display: block; font-size: 11px; line-height: 1; margin-bottom: 4px; opacity: 0.85; font-weight: 500; }
.bubble.user .spk-tag { text-align: right; }
.spk-who { color: var(--accent); }
.spk-trig { color: var(--attn); letter-spacing: 0.3px; }
/* ⏰ 图标走 inline SVG + currentColor,跟随 --attn 换肤(不用彩色 emoji,§6.7) */
.spk-trig svg { width: 11px; height: 11px; vertical-align: -1.5px; margin-right: 2px; }

/* —— 气泡富文本(markdown):wang 回复用,修掉逐字 span 吞换行的老问题 —— */
.md { white-space: normal; }
.md > :first-child { margin-top: 0; }
.md > :last-child { margin-bottom: 0; }
.md p { margin: 0 0 8px; }
.md ul, .md ol { margin: 6px 0; padding-left: 20px; }
.md li { margin: 2px 0; }
.md h1, .md h2, .md h3, .md h4 { margin: 10px 0 6px; font-weight: 600; line-height: 1.3; }
.md h1 { font-size: 1.3em; } .md h2 { font-size: 1.18em; } .md h3 { font-size: 1.06em; } .md h4 { font-size: 1em; }
.md code { font-family: ui-monospace, "SF Mono", monospace; font-size: .9em; background: rgba(var(--accent-rgb), 0.12); padding: 1px 5px; border-radius: 5px; }
.md pre { background: var(--surface-deep); border: 1px solid var(--line); border-radius: 9px; padding: 10px 12px; overflow-x: auto; margin: 8px 0; }
.md pre code { background: none; padding: 0; font-size: 12.5px; line-height: 1.5; }
.md blockquote { margin: 8px 0; padding: 2px 0 2px 12px; border-left: 2px solid var(--line); color: var(--text-dim); }
.md a { color: var(--accent); text-decoration: underline; text-underline-offset: 2px; cursor: pointer; }
.md strong, .md b { font-weight: 600; color: var(--text); }
.md hr { border: none; border-top: 1px solid var(--line); margin: 10px 0; }
.md table { border-collapse: collapse; margin: 8px 0; font-size: .94em; }
.md th, .md td { border: 1px solid var(--line); padding: 4px 8px; text-align: left; }
/* 用户原话:纯文本,保留换行、不解析 markdown */
.usertext { white-space: pre-wrap; word-break: break-word; }

.composer { padding: 12px 18px 16px; border-top: 1px solid var(--line); display: flex; flex-direction: column; gap: 9px; }
.input-row { display: flex; gap: 9px; align-items: flex-end; } /* 底对齐:输入框长高时,话筒/发送键贴底不上浮 */
.field {
  flex: 1; background: var(--surface-deep); border: 1px solid var(--line); border-radius: 13px;
  padding: 11px 15px; color: var(--text); font-size: 14px; outline: none;
  backdrop-filter: blur(6px); -webkit-backdrop-filter: blur(6px);
}
.field::placeholder { color: var(--text-dim); }
.field:focus { border-color: var(--accent); box-shadow: 0 0 0 2px rgba(var(--accent-rgb), 0.12); }
/* 多行输入框:去掉 textarea 默认外观,字体/行高跟随;高度由 JS autoGrow 控,封顶后内部滚动 */
textarea.field { resize: none; font-family: inherit; line-height: 1.5; max-height: 132px; overflow-y: auto; display: block; }
.send {
  width: 46px; height: 45px; flex: 0 0 auto;
  display: flex; align-items: center; justify-content: center;
  border: 1px solid var(--line); border-radius: 13px; cursor: pointer; font-size: 16px;
  background: rgba(var(--accent-rgb), 0.1); color: var(--accent);
  backdrop-filter: blur(6px); -webkit-backdrop-filter: blur(6px);
  transition: border-color .15s, background .15s, box-shadow .15s;
}
.send:hover:not(:disabled) { border-color: var(--accent); background: rgba(var(--accent-rgb), 0.2); box-shadow: 0 0 14px rgba(var(--accent-rgb), 0.3); }
.send:disabled { opacity: 0.4; cursor: default; }
.send.stop { color: var(--attn); border-color: rgba(var(--attn-rgb), 0.4); }
.send.stop:hover { border-color: var(--attn); background: rgba(var(--attn-rgb), 0.15); box-shadow: 0 0 14px rgba(var(--attn-rgb), 0.3); }
.key-row .field { border-color: rgba(var(--warn-rgb), 0.45); }

/* 语音输入:输入框内右侧小话筒(轻量,不跟发送键并排抢戏;界面优先,语音只是输入之一) */
.field-wrap { flex: 1; position: relative; display: flex; min-width: 0; }
.field.has-mic { padding-right: 42px; }
.mic-inline {
  position: absolute; right: 6px; bottom: 7px;
  width: 30px; height: 30px; padding: 0; border: none; background: none; cursor: pointer;
  color: var(--text-dim); display: flex; align-items: center; justify-content: center;
  border-radius: 8px; transition: color .15s, background .15s;
}
.mic-inline:hover { color: var(--accent); background: rgba(var(--accent-rgb), 0.12); }
.mic-inline svg { width: 17px; height: 17px; fill: none; stroke: currentColor; stroke-width: 1.7; stroke-linecap: round; display: block; }

/* —— 媒体附件(PLAN §9):加图/文件按钮 + 待发托盘 + 气泡里的图/小票 —— */
.file-hidden { display: none; }
/* 小图标按钮,无框贴左(界面优先,附件是轻量入口);留白给以后并排放更多功能键 */
.attach-btn {
  flex: 0 0 auto; align-self: center; width: 32px; height: 32px; padding: 0;
  border: none; background: none; cursor: pointer; color: var(--text-dim);
  display: flex; align-items: center; justify-content: center; border-radius: 9px;
  transition: color .15s, background .15s;
}
.attach-btn:hover { color: var(--accent); background: rgba(var(--accent-rgb), 0.12); }
.attach-btn svg { width: 17px; height: 17px; fill: none; stroke: currentColor; stroke-width: 1.7; stroke-linecap: round; stroke-linejoin: round; }

/* 排队区(Phase A):7274 还在说时发的消息,攒这儿、整轮结束自动合并发 */
.queue { display: flex; flex-direction: column; gap: 5px; padding: 2px 2px 0; }
.q-head { display: flex; align-items: center; justify-content: space-between; font-size: 11px; letter-spacing: 1px; color: var(--text-dim); }
.q-jump {
  cursor: pointer; background: rgba(var(--accent-rgb), 0.1); border: 1px solid rgba(var(--accent-rgb), 0.4);
  border-radius: 999px; padding: 3px 11px; color: var(--accent); font-size: 11px; letter-spacing: .5px;
  transition: background .15s, border-color .15s;
}
.q-jump:hover { background: rgba(var(--accent-rgb), 0.2); border-color: var(--accent); box-shadow: 0 0 10px rgba(var(--accent-rgb), 0.25); }
.q-item {
  display: flex; align-items: center; gap: 7px;
  background: rgba(var(--accent-rgb), 0.05); border: 1px dashed var(--line); border-radius: 9px;
  padding: 5px 9px; font-size: 12.5px; color: var(--text);
}
.q-clip { width: 13px; height: 13px; flex: 0 0 auto; fill: none; stroke: var(--accent); stroke-width: 1.7; stroke-linecap: round; stroke-linejoin: round; }
.q-text { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.q-x { background: none; border: none; color: var(--text-dim); cursor: pointer; font-size: 12px; line-height: 1; padding: 0 2px; flex: 0 0 auto; }
.q-x:hover { color: var(--danger); }
.att-tray { display: flex; flex-wrap: wrap; gap: 8px; }
.att-pill {
  display: flex; align-items: center; gap: 7px; max-width: 230px;
  background: var(--surface-2); border: 1px solid var(--line); border-radius: 10px; padding: 5px 8px;
  font-size: 12px; color: var(--text);
}
.att-thumb { width: 30px; height: 30px; object-fit: cover; border-radius: 6px; flex: 0 0 auto; }
.att-doc { width: 18px; height: 18px; flex: 0 0 auto; fill: none; stroke: var(--accent); stroke-width: 1.6; stroke-linejoin: round; }
.att-name { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.att-x { background: none; border: none; color: var(--text-dim); cursor: pointer; font-size: 12px; line-height: 1; padding: 0 2px; flex: 0 0 auto; }
.att-x:hover { color: var(--danger); }

.atts { display: flex; flex-wrap: wrap; gap: 8px; margin-bottom: 6px; }
.att-img { max-width: 200px; max-height: 220px; border-radius: 10px; display: block; }
.att-chip {
  display: inline-flex; align-items: center; gap: 6px;
  background: var(--surface-deep); border: 1px solid var(--line); border-radius: 9px; padding: 5px 9px;
  font-size: 12px; color: var(--text);
}
.att-chip svg { width: 15px; height: 15px; flex: 0 0 auto; fill: none; stroke: var(--accent); stroke-width: 1.6; stroke-linejoin: round; }

/* —— 「想了想」漏出(PLAN §9):折叠药丸 + 展开人格化步骤 —— */
.think { margin-top: 7px; }
.think-pill {
  display: inline-flex; align-items: center; gap: 6px; cursor: pointer;
  background: rgba(var(--accent-rgb), 0.06); border: 1px solid var(--line); border-radius: 999px;
  padding: 4px 10px; color: var(--text-dim); font-size: 12px;
  transition: color .15s, border-color .15s, background .15s;
}
.think-pill:hover { color: var(--accent); border-color: rgba(var(--accent-rgb), 0.4); }
.think-i { width: 13px; height: 13px; flex: 0 0 auto; fill: none; stroke: var(--accent); stroke-width: 1.6; stroke-linecap: round; stroke-linejoin: round; }
.think-chev { width: 13px; height: 13px; flex: 0 0 auto; fill: none; stroke: currentColor; stroke-width: 1.8; stroke-linecap: round; stroke-linejoin: round; transition: transform .18s ease; }
.think-pill.open .think-chev { transform: rotate(180deg); }
.think-detail {
  margin-top: 6px; padding: 9px 11px; border: 1px solid var(--line); border-radius: 11px;
  background: var(--surface-deep); display: flex; flex-direction: column; gap: 9px;
  animation: thinkIn .18s ease; max-width: 100%;
}
@keyframes thinkIn { from { opacity: 0; transform: translateY(-3px); } }
.td-step { display: flex; flex-direction: column; gap: 3px; }
.td-call { display: flex; flex-wrap: wrap; align-items: baseline; gap: 7px; font: 12px/1.45 ui-monospace, "SF Mono", monospace; }
.td-name { color: var(--accent); }
.td-args { color: var(--text-dim); word-break: break-all; }
.td-bad { color: var(--danger); }
.td-result {
  font: 11.5px/1.5 ui-monospace, "SF Mono", monospace; color: var(--text-dim);
  white-space: pre-wrap; word-break: break-word; max-height: 120px; overflow: auto;
  padding-left: 10px; border-left: 2px solid var(--line);
}
.td-cot { display: flex; flex-direction: column; gap: 4px; }
.td-cot-h { font-size: 11px; letter-spacing: 1px; color: var(--text-dim); }
.td-cot-body {
  margin: 0; font: 11.5px/1.6 ui-monospace, "SF Mono", monospace; color: var(--text);
  white-space: pre-wrap; word-break: break-word; max-height: 200px; overflow: auto;
  background: var(--surface); border-radius: 8px; padding: 8px 10px;
}

/* —— 听写(PLAN §11):输入框位变波形,token 体系,无新布局结构 —— */
.send.cancel-listen { color: var(--danger); border-color: rgba(var(--danger-rgb), 0.4); }
.send.cancel-listen:hover { border-color: var(--danger); background: rgba(var(--danger-rgb), 0.12); box-shadow: 0 0 14px rgba(var(--danger-rgb), 0.25); }
.listen-field {
  display: flex; align-items: center; gap: 12px; cursor: pointer; user-select: none;
  border-color: rgba(var(--accent-rgb), 0.5); box-shadow: 0 0 16px rgba(var(--accent-rgb), 0.16) inset, 0 0 10px rgba(var(--accent-rgb), 0.12);
}
.listen-field.heard { border-color: var(--accent); }
.listen-field.preparing, .listen-field.transcribing { cursor: default; }
.wave { display: flex; align-items: center; gap: 3px; height: 20px; flex: 0 0 auto; }
.wave i {
  width: 3px; min-height: 12%; background: var(--accent); border-radius: 2px;
  transition: height .09s linear; box-shadow: 0 0 6px rgba(var(--accent-rgb), 0.45);
}
/* 准备/识别中:电平没了,柱子改呼吸,别像死机 */
.listen-field.preparing .wave i, .listen-field.transcribing .wave i { animation: wavePulse 1.1s ease-in-out infinite; }
.listen-field.preparing .wave i:nth-child(odd), .listen-field.transcribing .wave i:nth-child(odd) { animation-delay: .25s; }
@keyframes wavePulse { 0%, 100% { height: 14%; } 50% { height: 64%; } }
.listen-hint { color: var(--text-dim); font-size: 12.5px; }
.listen-field.heard .listen-hint { color: var(--accent); }

/* 朗读(耳机=念出来):贴气泡右下,默认隐身 hover 浮现(与读数同款克制),小巧 */
.replay {
  position: absolute; right: 8px; bottom: -19px; z-index: 7;
  width: 19px; height: 16px; padding: 0;
  display: flex; align-items: center; justify-content: center;
  background: rgba(var(--accent-rgb), 0.08); color: var(--accent);
  border: 1px solid var(--line); border-radius: 5px; cursor: pointer;
  opacity: 0; transition: opacity .18s ease;
}
.replay svg { width: 11px; height: 11px; fill: none; stroke: currentColor; stroke-width: 1.8; stroke-linecap: round; display: block; }
.bubble:hover .replay { opacity: 0.9; }
.replay:hover { border-color: var(--accent); }

/* 用户消息 hover:复制 + 时间(右下浮现,与读数/重听同款克制) */
.user-meta {
  position: absolute; top: 100%; right: 13px; margin-top: 3px; z-index: 7;
  display: flex; align-items: center; gap: 7px;
  opacity: 0; transform: translateY(-2px);
  transition: opacity .18s ease, transform .18s ease;
}
.bubble.user:hover .user-meta { opacity: 0.95; transform: translateY(0); }
.u-time { font: 10px/1 ui-monospace, "SF Mono", monospace; letter-spacing: .5px; color: var(--text-dim); white-space: nowrap; user-select: none; }
.copy-btn {
  width: 18px; height: 16px; padding: 0; display: flex; align-items: center; justify-content: center;
  background: rgba(var(--accent-rgb), 0.08); color: var(--text-dim);
  border: 1px solid var(--line); border-radius: 5px; cursor: pointer;
  transition: color .15s, border-color .15s;
}
.copy-btn:hover { color: var(--accent); border-color: var(--accent); }
.copy-btn.done { color: var(--ok); border-color: rgba(var(--ok-rgb), 0.5); }
.copy-btn svg { width: 11px; height: 11px; fill: none; stroke: currentColor; stroke-width: 1.7; stroke-linecap: round; stroke-linejoin: round; display: block; }
/* wang 回复一键复制:与 user 同款图标,贴右下、在耳机左侧,hover 浮现(右键菜单也有,这给个直达) */
.wang-copy { position: absolute; right: 34px; bottom: -19px; z-index: 7; opacity: 0; transition: opacity .18s ease; }
.bubble:hover .wang-copy { opacity: 0.9; }
/* 日期分隔条:跨天会话的轻分隔,居中低对比、不抢气泡 */
.day-sep { align-self: center; margin: 6px 0 4px; user-select: none; }
.day-sep span { font-size: 11px; letter-spacing: .5px; color: var(--text-dim); background: var(--surface); border: 1px solid var(--line); border-radius: 999px; padding: 2px 10px; }

/* —— HUD 增强 —— */
.who small { display: flex; align-items: center; gap: 6px; font-size: 11.5px; color: var(--text-dim); }
.led { width: 6px; height: 6px; border-radius: 50%; background: var(--ok); box-shadow: 0 0 8px var(--ok); animation: led 2.4s ease-in-out infinite; }
@keyframes led { 0%, 100% { opacity: 1; } 50% { opacity: .3; } }

.rc-head { letter-spacing: 1.5px; }
/* rail 标签:窄字距 + 可换行,容得下长单词的语言(英文 Reminders 等),再长也优雅折行不溢出 */
.rb span { letter-spacing: .5px; line-height: 1.1; text-align: center; white-space: normal; overflow-wrap: break-word; max-width: 100%; }
.rc-time { font-family: ui-monospace, "SF Mono", monospace; letter-spacing: .5px; }

.rail::after, .recents::after {
  content: ""; position: absolute; top: 0; right: -1px; width: 1px; height: 72px; pointer-events: none;
  background: linear-gradient(180deg, transparent, var(--accent), transparent);
  opacity: .7; animation: flow 5.5s linear infinite;
}
@keyframes flow { 0% { transform: translateY(-72px); } 100% { transform: translateY(101vh); } }

/* 切角风格:clip-path 若加在元素上,会连"贴在气泡外沿的 hover 浮层"(读数/复制/时间/耳机)
   一起裁掉(2026-07-03 真机实锤:切角没有 hover 操作、圆角有)。改法 = 切角形状画在 ::before
   背景层(背景/描边一并搬进去),元素本身不裁 → 浮层照常浮现。.bubble 自带 backdrop-filter
   已成 stacking context,::before 的 z-index:-1 稳在内容之下、页面之上。 */
.layout.cut .bubble { border-radius: 0; box-shadow: none; filter: drop-shadow(0 6px 16px rgba(0, 0, 0, 0.3)); background: transparent; border-color: transparent; }
.layout.cut .bubble::before { content: ""; position: absolute; inset: 0; z-index: -1; background: var(--bubble-them); border: 1px solid var(--line); }
.layout.cut .bubble.wang::before { clip-path: polygon(0 0, 100% 0, 100% calc(100% - 9px), calc(100% - 9px) 100%, 0 100%); }
.layout.cut .bubble.user::before { background: var(--bubble-me); border-color: var(--bubble-me-line); clip-path: polygon(0 0, 100% 0, 100% 100%, 9px 100%, 0 calc(100% - 9px)); }
</style>
