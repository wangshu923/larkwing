<script setup lang="ts">
// 设置台:tab 导航 + 意图措辞(设计稿见会话纪要:常规|大脑|声音·暗|家人|远程·暗|系统)。
// 暗 tab 可点、进 teaser 页 —— 能点的必有反应(铁律3),绝不放灰掉的死控件。
import { computed, onMounted, onUnmounted, reactive, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import { api, isTauri, setFloatVisible, type ProviderView, type VoiceStatus } from '../lib/backend'
import { useChat } from '../composables/useChat'
import { useSettings } from '../composables/useSettings'

const emit = defineEmits<{ (e: 'close'): void }>()
const { t } = useI18n()
const settings = useSettings()
const { state: chat } = useChat()

type TabId = 'general' | 'brain' | 'voice' | 'family' | 'remote' | 'system'
const tabs: { id: TabId; future?: boolean }[] = [
  { id: 'general' },
  { id: 'brain' },
  { id: 'voice' },
  { id: 'family' },
  { id: 'remote', future: true },
  { id: 'system' },
]
const tab = ref<TabId>('general')

// —— 声音 tab(PLAN §11):第一层 音色+自动朗读;高级 语速/耐心/音量/麦克风/组件状态 ——
const voiceInfo = ref<VoiceStatus | null>(null)
watch(tab, (v) => {
  if (v === 'voice' && !voiceInfo.value) void loadVoice()
  if (v === 'system') void loadAutostart()
})
async function loadVoice() {
  if (!isTauri()) {
    // 浏览器预览:与 core 音色目录同构的假数据(纯看交互)
    voiceInfo.value = {
      asrReady: false,
      vadReady: false,
      kwsReady: false,
      wakeRunning: false,
      keywords: ['小七'],
      devices: ['MacBook 麦克风(预览)', 'USB 会议麦(预览)'],
      speakers: [
        { id: 'zh-CN-XiaoxiaoNeural', name: '晓晓 · 温柔' },
        { id: 'zh-CN-XiaoyiNeural', name: '晓伊 · 可爱' },
        { id: 'zh-CN-XiaohanNeural', name: '晓涵 · 讲故事' },
        { id: 'zh-CN-XiaochenNeural', name: '晓辰 · 活泼' },
        { id: 'zh-CN-YunxiNeural', name: '云希 · 少年' },
        { id: 'zh-CN-YunjianNeural', name: '云健 · 沉稳' },
      ],
    }
  } else {
    try {
      voiceInfo.value = await api.voiceStatus()
    } catch (e) {
      console.error('读取语音状态失败', e)
    }
  }
  // 唤醒词框实绑当前词(既显示又可改),不靠 placeholder —— 消除"空框/灰字"歧义,
  // 且唤醒词只此一处可填(第一层那行改成只读状态,不再像输入框)
  keywordsDraft.value = (voiceInfo.value?.keywords ?? []).join('、')
}
// 喊名字唤醒(C 期):开关 = voice_wake_set 一体化入口(写库 + 起停;首次开会
// 下 KWS 模型 + 预合成应答音,按钮转菊花)。wakeRunning 是事实,失败自然回弹。
const wakeBusy = ref(false)
// 开启失败的可见出口(铁律 §3.5:能点的必有反应、出错有友好退路)。原先失败只
// console.error → Win 上看不到任何反馈,开关只闪一下就回弹 = 用户眼里"打不开"。
const wakeError = ref('')
// 唤醒词合法性,与后端 encode_keywords 同口径:逐词必须纯中文,否则整词被丢弃;
// 一个能用的都没有 = 根本起不来(就是唤醒词填 "BT" 这种坑)。空 = 用默认词,不算问题。
function keywordIssue(text: string): 'ok' | 'all-bad' | 'some-bad' {
  const words = text.split(/[、,，;；\s]+/).map((s) => s.trim()).filter(Boolean)
  if (words.length === 0) return 'ok'
  const good = words.filter((w) => /^[一-鿿]+$/.test(w)) // 纯中文(CJK 基本区)
  if (good.length === 0) return 'all-bad'
  return good.length < words.length ? 'some-bad' : 'ok'
}
async function toggleWake() {
  if (wakeBusy.value) return
  const target = !(voiceInfo.value?.wakeRunning ?? false)
  wakeError.value = ''
  // 拦在最前(也拦在调后端之前):唤醒词全不是中文时后端必抛(keywords_buf 为空),
  // 与其白跑一趟再静默回弹,不如就地给精确话(这正是 "BT" 打不开的根因)。
  if (target) {
    const eff = keywordsDraft.value.trim() || (voiceInfo.value?.keywords ?? []).join('、')
    if (keywordIssue(eff) === 'all-bad') {
      wakeError.value = t('settings.voice.keywordsAllInvalid')
      return
    }
  }
  if (!isTauri()) {
    if (voiceInfo.value) voiceInfo.value.wakeRunning = target // 预览:纯看交互
    return
  }
  wakeBusy.value = true
  try {
    voiceInfo.value = await api.voiceWakeSet(target)
  } catch (e) {
    console.error('唤醒开关失败', e) // message 进日志,给用户的是友好兜底文案
    wakeError.value = t('settings.voice.wakeFailed')
  } finally {
    wakeBusy.value = false
  }
}
// 唤醒词:失焦保存;开着唤醒时重启循环让新词立即生效。边打边提示非中文(拦输入)
const keywordsDraft = ref('')
const keywordWarn = computed(() => keywordIssue(keywordsDraft.value))
watch(keywordsDraft, () => (wakeError.value = '')) // 一动词就清掉上次开启失败的红字,别让它发霉
async function saveKeywords() {
  const v = keywordsDraft.value.trim()
  if (!v) return
  await settings.set('voice.wake.keywords', v)
  if (isTauri() && voiceInfo.value?.wakeRunning) {
    try {
      await api.voiceWakeSet(false)
      voiceInfo.value = await api.voiceWakeSet(true)
    } catch (e) {
      console.error('唤醒词生效失败', e)
      wakeError.value = t('settings.voice.wakeFailed')
    }
  } else if (isTauri()) {
    voiceInfo.value = await api.voiceStatus().catch(() => voiceInfo.value)
  }
}

const previewing = ref('')
async function previewSpeaker(id: string) {
  settings.set('voice.speaker', id)
  if (!isTauri() || previewing.value) return
  previewing.value = id
  try {
    const url = await api.voicePreview(id, t('settings.voice.previewLine'))
    const a = new Audio(url)
    a.addEventListener('ended', () => (previewing.value = ''))
    a.addEventListener('error', () => (previewing.value = ''))
    void a.play()
  } catch (e) {
    console.error('试听失败', e)
    previewing.value = ''
  }
}
function setMic(ev: Event) {
  settings.set('voice.input_device', (ev.target as HTMLSelectElement).value)
}

// 唯一脉冲:全局任何时刻最多一个光点,指向当前唯一需要行动的事(现在 = 缺钥匙)
const needKey = computed(() => chat.ready && !chat.hasApiKey)

// —— 系统 tab:开机自启(OS 真相源,独立命令)+ 桌面悬浮窗(ui.float.* 设置,PLAN §12) ——
// dev(tauri dev)下前端走 Vite devUrl、二进制在 target/debug —— 此时设自启会写一条指向 debug exe 的注册表项,
// 开机却连不上 1420 → 白屏。故 dev build 禁用开关(import.meta.env.DEV 精确反映"前端是否走 dev server")。
const isDev = import.meta.env.DEV
const autostart = ref(false)
const autostartBusy = ref(false)
async function loadAutostart() {
  if (!isTauri()) return
  try {
    autostart.value = await api.autostartEnabled()
  } catch (e) {
    console.error('读取开机自启失败', e)
  }
}
async function toggleAutostart() {
  if (autostartBusy.value || isDev) return // dev 下禁用(见上);UI 已 disabled,这里再挡一道
  const target = !autostart.value
  if (!isTauri()) {
    autostart.value = target
    return
  }
  autostartBusy.value = true
  try {
    await api.setAutostart(target)
    autostart.value = await api.autostartEnabled() // 回读 OS 真值
  } catch (e) {
    console.error('设置开机自启失败', e)
  } finally {
    autostartBusy.value = false
  }
}
const floatEnabled = computed(() => settings.get('ui.float.enabled') !== '0')
function toggleFloat() {
  const target = !floatEnabled.value
  settings.set('ui.float.enabled', target ? '1' : '0')
  void setFloatVisible(target) // 立即反映;主窗聚焦联动留 E 期
}
function setFloatOpacity(pct: number) {
  settings.set('ui.float.opacity', (pct / 100).toFixed(2))
}
// 待机轮播是否带"今日花费 / 余额"(opt-in;默认家庭脸不显,看板控自己开)
const floatShowUsage = computed(() => settings.get('ui.float.show_usage') === '1')
function toggleFloatUsage() {
  settings.set('ui.float.show_usage', floatShowUsage.value ? '0' : '1')
}

// 段选控件的数据驱动写法:一行配置 = 一个设置项
const segs = computed(() => ({
  character: {
    key: 'ui.character',
    options: ['titan', 'dog', 'cat'].map((v) => ({ v, label: t(`settings.general.char_${v}`) })),
  },
  bubble: {
    key: 'ui.bubble_shape',
    options: ['round', 'cut'].map((v) => ({ v, label: t(`settings.general.bubble_${v}`) })),
  },
  textScale: {
    key: 'ui.text_scale',
    options: ['standard', 'large'].map((v) => ({ v, label: t(`settings.general.scale_${v}`) })),
  },
  strategy: {
    key: 'llm.strategy',
    options: ['thrifty', 'balanced', 'smart_first'].map((v) => ({
      v,
      label: t(`settings.brain.strategy_${v}`),
    })),
  },
  mode: {
    key: 'llm.thinking',
    options: ['off', 'light', 'medium', 'heavy'].map((v) => ({
      v,
      label: t(`settings.brain.mode_${v}`),
    })),
  },
  rate: {
    key: 'voice.rate',
    options: ['slow', 'standard', 'fast'].map((v) => ({ v, label: t(`settings.voice.rate_${v}`) })),
  },
  patience: {
    key: 'voice.patience',
    options: ['snappy', 'standard', 'relaxed'].map((v) => ({
      v,
      label: t(`settings.voice.patience_${v}`),
    })),
  },
}))

// 标题里的名字跟「叫我什么」联动(ui.pet_name 空 = 默认名 pet.name);
// 与 MainLayout / FloatWindow 同一口径 —— 这是当前 agent 的名字,不是 app 名
const petName = computed(() => settings.get('ui.pet_name') || t('pet.name'))

// 叫我什么:框里直接显示当前生效名(空库就显示默认名,跟标题同一个值——
// 上面是什么,下面就是什么);沿用「唤醒词框」先例:实绑当前值、不靠 placeholder,
// 消除"空框=到底叫啥"的歧义。存库仍保持"空 = 跟随默认名":清空或填回默认名
// 都存空,默认名将来变(换肤/宪法)时自动跟随,不在库里钉死字面量。
const petDraft = ref(petName.value)
function savePetName() {
  const v = petDraft.value.trim()
  settings.set('ui.pet_name', v && v !== t('pet.name') ? v : '')
  petDraft.value = petName.value // 回填:清空后框里也显示回默认名,始终与标题一致
}

// 我的性格:一句话人格覆盖层(进稳定前缀,下一句话生效);空 = 纯出厂人设
const styleDraft = ref(settings.get('persona.style'))
function saveStyle() {
  settings.set('persona.style', styleDraft.value.trim())
}

// 供应商卡片:预设全预填,用户按需改。钥匙草稿按卡隔离,回车/失焦保存,空草稿不动钥匙
const keyDrafts = reactive<Record<string, string>>({})
function saveKey(p: ProviderView) {
  const k = (keyDrafts[p.id] ?? '').trim()
  if (!k) return
  settings.saveProvider({ id: p.id, apiKey: k })
  keyDrafts[p.id] = ''
}
function saveField(p: ProviderView, field: 'baseUrl' | 'model', ev: Event) {
  const v = (ev.target as HTMLInputElement).value.trim()
  if (!v || v === p[field]) return
  settings.saveProvider({ id: p.id, [field]: v })
}
function protoLabel(p: ProviderView) {
  return p.protocol === 'anthropic_compat' ? 'ANTHROPIC 兼容' : 'OPENAI 兼容'
}

// 自定义卡:「自己接一个大脑」
const adding = ref(false)
const custom = reactive({ name: '', protocol: 'openai_compat', baseUrl: '', model: '', key: '' })
const customReady = computed(() => custom.name.trim() && custom.baseUrl.trim() && custom.model.trim())
async function addCustom() {
  if (!customReady.value) return
  const ok = await settings.saveProvider({
    id: `custom-${Date.now().toString(36)}`,
    name: custom.name.trim(),
    protocol: custom.protocol,
    baseUrl: custom.baseUrl.trim(),
    model: custom.model.trim(),
    apiKey: custom.key.trim() || undefined,
  })
  if (ok) {
    adding.value = false
    Object.assign(custom, { name: '', protocol: 'openai_compat', baseUrl: '', model: '', key: '' })
  }
}

// 改名:行内编辑
const nameEditing = ref(false)
const nameDraft = ref('')
function startRename() {
  nameDraft.value = settings.state.userName
  nameEditing.value = true
}
function submitRename() {
  settings.rename(nameDraft.value)
  nameEditing.value = false
}

function onKeydown(e: KeyboardEvent) {
  if (e.key === 'Escape') emit('close')
}
onMounted(() => window.addEventListener('keydown', onKeydown))
onUnmounted(() => window.removeEventListener('keydown', onKeydown))
</script>

<template>
  <section class="settings">
    <header class="s-head" data-tauri-drag-region>
      <div class="s-title">
        <b>{{ t('settings.title') }}</b>
        <span class="s-mono">{{ petName }} · CONSOLE</span>
        <small>{{ t('settings.tagline') }}</small>
      </div>
      <button class="s-back" @click="emit('close')">{{ t('settings.back') }}</button>
    </header>

    <nav class="s-tabs">
      <button
        v-for="tb in tabs"
        :key="tb.id"
        class="s-tab"
        :class="{ on: tab === tb.id, future: tb.future }"
        @click="tab = tb.id"
      >
        {{ t(`settings.tabs.${tb.id}`) }}
        <span v-if="tb.id === 'brain' && needKey" class="dot amber" :title="t('settings.brain.keyMissing')"></span>
        <span v-if="tb.future" class="s-mono badge">{{ t('settings.loading') }}</span>
      </button>
    </nav>

    <div class="s-body">
      <!-- 常规 -->
      <div v-if="tab === 'general'">
        <div class="row">
          <span class="label">{{ t('settings.general.petName') }}</span>
          <input
            v-model="petDraft"
            class="s-input"
            :placeholder="t('settings.general.petNamePlaceholder')"
            @blur="savePetName"
            @keyup.enter="savePetName"
          />
        </div>
        <div class="row">
          <span class="label">{{ t('settings.general.personaStyle') }}</span>
          <input
            v-model="styleDraft"
            class="s-input"
            maxlength="60"
            :placeholder="t('settings.general.personaStylePlaceholder')"
            @blur="saveStyle"
            @keyup.enter="saveStyle"
          />
        </div>
        <div v-for="(seg, name) in { character: segs.character, bubble: segs.bubble, textScale: segs.textScale }" :key="name" class="row">
          <span class="label">{{ t(`settings.general.${name}`) }}</span>
          <span class="seg">
            <button
              v-for="o in seg.options"
              :key="o.v"
              :class="{ on: settings.get(seg.key) === o.v }"
              @click="settings.set(seg.key, o.v)"
            >{{ o.label }}</button>
          </span>
        </div>
      </div>

      <!-- 大脑:高频策略行在上,供应商卡片(装机区)在下 -->
      <div v-else-if="tab === 'brain'">
        <div v-for="(seg, name) in { strategy: segs.strategy, mode: segs.mode }" :key="name" class="row">
          <span class="label">{{ t(`settings.brain.${name}`) }}</span>
          <span class="seg">
            <button
              v-for="o in seg.options"
              :key="o.v"
              :class="{ on: settings.get(seg.key) === o.v }"
              @click="settings.set(seg.key, o.v)"
            >{{ o.label }}</button>
          </span>
        </div>

        <p class="section">{{ t('settings.brain.providers') }}</p>
        <div v-for="p in settings.state.providers" :key="p.id" class="pcard" :class="{ off: !p.enabled }">
          <div class="p-head">
            <b>{{ p.name }}</b>
            <span class="s-mono proto">{{ protoLabel(p) }}</span>
            <span :class="p.keySet ? 'ok-text' : 'amber-text'">
              {{ !p.enabled ? t('settings.brain.cardOff') : p.keySet ? t('settings.brain.keyOk') : t('settings.brain.keyMissing') }}
            </span>
            <span class="p-actions">
              <button class="link" @click="settings.saveProvider({ id: p.id, enabled: !p.enabled })">
                {{ p.enabled ? t('settings.brain.disable') : t('settings.brain.enable') }}
              </button>
              <button v-if="!p.builtin" class="link danger" @click="settings.removeProvider(p.id)">
                {{ t('settings.brain.remove') }}
              </button>
            </span>
          </div>
          <div class="p-grid">
            <label>{{ t('settings.brain.keyField') }}</label>
            <input
              v-model="keyDrafts[p.id]"
              class="s-input"
              :placeholder="p.keyMasked || t('settings.brain.keyPlaceholder')"
              @keyup.enter="saveKey(p)"
              @blur="saveKey(p)"
            />
            <label>{{ t('settings.brain.endpoint') }}</label>
            <input class="s-input s-mono-input" :value="p.baseUrl" @change="saveField(p, 'baseUrl', $event)" />
            <label>{{ t('settings.brain.model') }}</label>
            <input class="s-input s-mono-input" :value="p.model" @change="saveField(p, 'model', $event)" />
          </div>
        </div>

        <!-- 自己接一个大脑 -->
        <button v-if="!adding" class="add-card" @click="adding = true">{{ t('settings.brain.addCustom') }}</button>
        <div v-else class="pcard">
          <div class="p-head">
            <b>{{ t('settings.brain.addCustom') }}</b>
            <span class="seg">
              <button :class="{ on: custom.protocol === 'openai_compat' }" @click="custom.protocol = 'openai_compat'">OpenAI 兼容</button>
              <button :class="{ on: custom.protocol === 'anthropic_compat' }" @click="custom.protocol = 'anthropic_compat'">Anthropic 兼容</button>
            </span>
          </div>
          <div class="p-grid">
            <label>{{ t('settings.brain.customName') }}</label>
            <input v-model="custom.name" class="s-input" :placeholder="t('settings.brain.customNamePlaceholder')" />
            <label>{{ t('settings.brain.endpoint') }}</label>
            <input v-model="custom.baseUrl" class="s-input s-mono-input" placeholder="https://…/v1" />
            <label>{{ t('settings.brain.model') }}</label>
            <input v-model="custom.model" class="s-input s-mono-input" placeholder="model-id" />
            <label>{{ t('settings.brain.keyField') }}</label>
            <input v-model="custom.key" class="s-input" :placeholder="t('settings.brain.keyPlaceholder')" />
          </div>
          <div class="p-foot">
            <button class="link" :disabled="!customReady" @click="addCustom">{{ t('settings.brain.addSave') }}</button>
            <button class="link dim" @click="adding = false">{{ t('settings.brain.addCancel') }}</button>
          </div>
        </div>
      </div>

      <!-- 家人 -->
      <div v-else-if="tab === 'family'">
        <div class="row">
          <span class="label">{{ t('settings.family.current') }}</span>
          <span v-if="!nameEditing" class="key-state">
            <span class="chip on">{{ settings.state.userName }}</span>
            <button class="link" @click="startRename">{{ t('settings.family.rename') }}</button>
          </span>
          <span v-else class="key-edit">
            <input
              v-model="nameDraft"
              class="s-input"
              :placeholder="t('settings.family.namePlaceholder')"
              @keyup.enter="submitRename"
            />
            <button class="link" :disabled="!nameDraft.trim()" @click="submitRename">
              {{ t('settings.family.save') }}
            </button>
          </span>
        </div>
        <p class="hint">
          <span class="chip future">{{ t('settings.family.addSoon') }}</span>
          {{ t('settings.family.addSoonHint') }}
        </p>
      </div>

      <!-- 声音(PLAN §11):第一层只放高频两项;高级分组线下(强默认收口) -->
      <div v-else-if="tab === 'voice'">
        <div class="row v-speaker">
          <span class="label">{{ t('settings.voice.speaker') }}</span>
          <span class="sp-list">
            <button
              v-for="sp in voiceInfo?.speakers ?? []"
              :key="sp.id"
              class="chip sp"
              :class="{ on: settings.get('voice.speaker') === sp.id, busy: previewing === sp.id }"
              :title="t('settings.voice.preview')"
              @click="previewSpeaker(sp.id)"
            >{{ sp.name }}</button>
          </span>
        </div>
        <!-- 朗读策略固定「跟着我」(语音问才念,UI 交互安静):不放旋钮(铁律 §3.1
             强默认收口;always/off 仍可从设置库手动写)。喊名字唤醒 = 常驻监听的隐私
             边界,必须第一层(PLAN §11) -->
        <div class="row">
          <span class="label">{{ t('settings.voice.wake') }}</span>
          <span class="key-state">
            <!-- 只读状态(不是输入框):开着没 + 现在听哪个词;改词在下面「唤醒词」一处 -->
            <span class="wake-cur">
              {{ voiceInfo?.wakeRunning
                ? t('settings.voice.wakeListening', { kw: (voiceInfo?.keywords ?? []).join('、') })
                : t('settings.voice.wakeIdle') }}
            </span>
            <button class="link" :disabled="wakeBusy" @click="toggleWake">
              {{ wakeBusy ? t('settings.voice.wakeBusy') : voiceInfo?.wakeRunning ? t('settings.voice.wakeOff') : t('settings.voice.wakeOn') }}
            </button>
          </span>
        </div>
        <!-- 失败有了去处:不再只闪一下回弹(铁律 §3.5) -->
        <p v-if="wakeError" class="hint err">{{ wakeError }}</p>
        <p v-else class="hint">{{ t('settings.voice.wakeHint') }}</p>

        <p class="section">{{ t('settings.voice.advanced') }}</p>
        <div class="row">
          <span class="label">{{ t('settings.voice.keywords') }}</span>
          <input
            v-model="keywordsDraft"
            class="s-input"
            :class="{ bad: keywordWarn === 'all-bad' }"
            :placeholder="(voiceInfo?.keywords ?? []).join('、') || t('settings.voice.keywordsPlaceholder')"
            @blur="saveKeywords"
            @keyup.enter="saveKeywords"
          />
        </div>
        <!-- 拦输入:非中文当场标出来,不等点「打开」才暴露 -->
        <p v-if="keywordWarn === 'all-bad'" class="hint err">{{ t('settings.voice.keywordsAllInvalid') }}</p>
        <p v-else-if="keywordWarn === 'some-bad'" class="hint warn">{{ t('settings.voice.keywordsSomeInvalid') }}</p>
        <p v-else class="hint">{{ t('settings.voice.keywordsHint') }}</p>
        <div v-for="(seg, name) in { rate: segs.rate, patience: segs.patience }" :key="name" class="row">
          <span class="label">{{ t(`settings.voice.${name}`) }}</span>
          <span class="seg">
            <button
              v-for="o in seg.options"
              :key="o.v"
              :class="{ on: settings.get(seg.key) === o.v }"
              @click="settings.set(seg.key, o.v)"
            >{{ o.label }}</button>
          </span>
        </div>
        <div class="row">
          <span class="label">{{ t('settings.voice.volume') }}</span>
          <input
            class="v-vol"
            type="range"
            min="0"
            max="100"
            :value="Number(settings.get('voice.volume') || '100')"
            @input="settings.set('voice.volume', String(($event.target as HTMLInputElement).value))"
          />
        </div>
        <div class="row">
          <span class="label">{{ t('settings.voice.micDevice') }}</span>
          <select class="s-input v-mic" :value="settings.get('voice.input_device')" @change="setMic">
            <option value="">{{ t('settings.voice.micDefault') }}</option>
            <option v-for="d in voiceInfo?.devices ?? []" :key="d" :value="d">{{ d }}</option>
          </select>
        </div>
        <!-- 在线/离线合成档(D 期):离线断网也能说,但要下个大模型、音色单一 -->
        <div class="row">
          <span class="label">{{ t('settings.voice.ttsBackend') }}</span>
          <span class="seg">
            <button
              v-for="b in ['online', 'offline']"
              :key="b"
              :class="{ on: (settings.get('voice.tts_backend') || 'online') === b }"
              @click="settings.set('voice.tts_backend', b)"
            >{{ t(`settings.voice.tts_${b}`) }}</button>
          </span>
        </div>
        <div class="row">
          <span class="label">{{ t('settings.voice.component') }}</span>
          <span class="s-mono comp" :class="{ ok: voiceInfo?.asrReady }">
            {{ voiceInfo?.asrReady ? t('settings.voice.compReady') : t('settings.voice.compMissing') }}
          </span>
        </div>
        <!-- Windows「通信活动自动压低」联动(robot 真机坑):常驻唤醒麦会让系统把
             其它声音压 80%,app 改不了,只能引导用户改系统设置 -->
        <p class="hint">{{ t('settings.voice.winDuckHint') }}</p>
      </div>

      <!-- 远程:暗页 teaser,只预告不放假控件 -->
      <div v-else-if="tab === 'remote'" class="teaser">
        <span class="s-mono badge">{{ t('settings.remote.badge') }}</span>
        <p>{{ t('settings.remote.teaser') }}</p>
      </div>

      <!-- 系统:开机与桌面(PLAN §12)+ 关于 -->
      <div v-else-if="tab === 'system'">
        <p class="section">{{ t('settings.system.desktop') }}</p>
        <div class="row">
          <span class="label">{{ t('settings.system.autostart') }}</span>
          <span class="key-state">
            <span class="chip" :class="{ on: autostart }">{{ autostart ? t('settings.system.on') : t('settings.system.off') }}</span>
            <button class="link" :disabled="autostartBusy || isDev" @click="toggleAutostart">
              {{ isDev ? t('settings.system.autostartDevTag') : autostartBusy ? t('settings.system.busy') : autostart ? t('settings.system.turnOff') : t('settings.system.turnOn') }}
            </button>
          </span>
        </div>
        <p class="hint">{{ isDev ? t('settings.system.autostartDev') : t('settings.system.autostartHint') }}</p>
        <div class="row">
          <span class="label">{{ t('settings.system.floatWin') }}</span>
          <span class="key-state">
            <span class="chip" :class="{ on: floatEnabled }">{{ floatEnabled ? t('settings.system.on') : t('settings.system.off') }}</span>
            <button class="link" @click="toggleFloat">{{ floatEnabled ? t('settings.system.turnOff') : t('settings.system.turnOn') }}</button>
          </span>
        </div>
        <div v-if="floatEnabled" class="row">
          <span class="label">{{ t('settings.system.floatOpacity') }}</span>
          <input
            class="v-vol"
            type="range"
            min="40"
            max="100"
            :value="Math.round(Number(settings.get('ui.float.opacity') || '0.92') * 100)"
            @input="setFloatOpacity(Number(($event.target as HTMLInputElement).value))"
          />
        </div>
        <div v-if="floatEnabled" class="row">
          <span class="label">{{ t('settings.system.floatUsage') }}</span>
          <span class="key-state">
            <span class="chip" :class="{ on: floatShowUsage }">{{ floatShowUsage ? t('settings.system.on') : t('settings.system.off') }}</span>
            <button class="link" @click="toggleFloatUsage">{{ floatShowUsage ? t('settings.system.turnOff') : t('settings.system.turnOn') }}</button>
          </span>
        </div>
        <p class="hint">{{ t('settings.system.floatHint') }}</p>

        <p class="section">{{ t('settings.system.about') }}</p>
        <div class="row">
          <span class="label">{{ t('settings.system.version') }}</span>
          <span class="s-mono">v0.1 · {{ t('settings.system.selfId') }}</span>
        </div>
      </div>
    </div>
  </section>
</template>

<style scoped>
.settings { flex: 1; display: flex; flex-direction: column; min-width: 0; padding: 18px 26px; overflow-y: auto; }
/* padding-right 让「回去聊天」避开右上角窗控三键(二轮真机修复:不再重叠) */
.s-head { display: flex; align-items: flex-start; justify-content: space-between; margin-bottom: 14px; padding-right: 70px; }
.s-title b { font-size: 16px; color: var(--txt); }
.s-title small { display: block; margin-top: 3px; font-size: 12px; color: var(--txt2); }
.s-mono { font-family: ui-monospace, "SF Mono", monospace; font-size: 10px; letter-spacing: 2px; color: var(--txt2); margin-left: 8px; }
.s-back { background: none; border: 1px solid var(--line); border-radius: 9px; color: var(--txt2); cursor: pointer; padding: 5px 10px; font-size: 12px; }
.s-back:hover { color: var(--cy); border-color: var(--cy); }

.s-tabs { display: flex; gap: 7px; border-bottom: 1px solid var(--line); padding-bottom: 10px; margin-bottom: 14px; flex-wrap: wrap; }
.s-tab {
  position: relative; background: rgba(95, 200, 255, 0.04); border: 1px solid var(--line); border-radius: 10px;
  color: var(--txt2); cursor: pointer; padding: 7px 14px; font-size: 13px;
  display: inline-flex; align-items: center; gap: 6px; transition: color .15s, border-color .15s;
}
.s-tab:hover { color: var(--txt); border-color: rgba(95, 200, 255, 0.4); }
.s-tab.on { color: var(--cy); border-color: rgba(95, 200, 255, 0.45); background: rgba(95, 200, 255, 0.1); }
.s-tab.future { opacity: .62; }
.badge { margin-left: 0; letter-spacing: 1px; }

/* 唯一脉冲:缺钥匙时全局唯一的光点 */
.dot { width: 6px; height: 6px; border-radius: 50%; }
.dot.amber { background: #ffc85f; box-shadow: 0 0 8px #ffc85f; animation: led 2.4s ease-in-out infinite; }
@keyframes led { 0%, 100% { opacity: 1; } 50% { opacity: .3; } }

.s-body { max-width: 640px; }
.row { display: flex; align-items: center; justify-content: space-between; gap: 14px; padding: 13px 0; border-bottom: 1px solid var(--line); font-size: 13.5px; }
.label { color: var(--txt); flex: 0 0 auto; }

.seg { display: inline-flex; border: 1px solid var(--line); border-radius: 10px; overflow: hidden; }
.seg button { background: none; border: none; color: var(--txt2); cursor: pointer; padding: 6px 13px; font-size: 12.5px; }
.seg button + button { border-left: 1px solid var(--line); }
.seg button.on { color: var(--cy); background: rgba(95, 200, 255, 0.12); }

.s-input {
  background: rgba(8, 20, 38, 0.6); border: 1px solid var(--line); border-radius: 10px;
  padding: 7px 11px; color: var(--txt); font-size: 13px; outline: none; min-width: 220px;
}
.s-input:focus { border-color: var(--cy); }

.key-state, .key-edit { display: inline-flex; align-items: center; gap: 10px; }
.ok-text { color: #5fe0b0; font-size: 12.5px; }
.amber-text { color: #ffc85f; font-size: 12.5px; }
.link { background: none; border: none; color: var(--cy); cursor: pointer; font-size: 12.5px; padding: 0; }
.link:disabled { opacity: .4; cursor: default; }

.chip { border: 1px solid var(--line); border-radius: 9px; padding: 4px 11px; font-size: 12.5px; color: var(--txt); }
.chip.on { border-color: rgba(95, 200, 255, 0.45); color: var(--cy); }
.chip.future { opacity: .62; color: var(--txt2); }
.hint { font-size: 12px; color: var(--txt2); line-height: 1.7; display: flex; align-items: center; gap: 10px; padding-top: 13px; }
.hint.err { color: #f09595; }
.hint.warn { color: #ffc85f; }
.s-input.bad { border-color: #f09595; }

.teaser { padding: 26px 0; color: var(--txt2); font-size: 13.5px; }
.teaser p { margin: 10px 0 0; }

.section { margin: 18px 0 9px; font-size: 11.5px; letter-spacing: 2px; color: var(--txt2); }

/* 供应商卡片 */
.pcard { border: 1px solid var(--line); border-radius: 12px; padding: 12px 14px; margin-bottom: 10px; background: rgba(95, 200, 255, 0.03); }
.pcard.off { opacity: .55; }
.p-head { display: flex; align-items: center; gap: 10px; font-size: 13.5px; flex-wrap: wrap; }
.p-head b { color: var(--txt); }
.proto { letter-spacing: 1px; border: 1px solid var(--line); border-radius: 6px; padding: 2px 6px; margin-left: 0; }
.p-head .ok-text, .p-head .amber-text { font-size: 12px; }
.p-actions { margin-left: auto; display: inline-flex; gap: 14px; }
.p-grid { display: grid; grid-template-columns: 52px minmax(0, 1fr); gap: 8px 12px; align-items: center; margin-top: 11px; font-size: 12.5px; }
.p-grid label { color: var(--txt2); }
.p-grid .s-input { width: 100%; min-width: 0; }
.s-mono-input { font-family: ui-monospace, "SF Mono", monospace; font-size: 12px; }
.p-foot { display: flex; gap: 16px; margin-top: 11px; }
.danger { color: #f09595; }
.dim { color: var(--txt2); }
.add-card { width: 100%; padding: 10px; border: 1px dashed var(--line); border-radius: 12px; background: none; color: var(--txt2); cursor: pointer; font-size: 12.5px; margin-bottom: 12px; }
.add-card:hover { color: var(--cy); border-color: var(--cy); }

/* —— 声音 tab —— */
.v-speaker { align-items: flex-start; }
.sp-list { display: flex; flex-wrap: wrap; gap: 8px; justify-content: flex-end; max-width: 420px; }
.chip.sp { cursor: pointer; background: rgba(95, 200, 255, 0.04); transition: border-color .15s, color .15s; }
.chip.sp:hover { border-color: rgba(95, 200, 255, 0.45); }
.chip.sp.on { border-color: rgba(95, 200, 255, 0.55); color: var(--cy); background: rgba(95, 200, 255, 0.1); }
.chip.sp.busy { animation: led 1.2s ease-in-out infinite; }
.v-vol { width: 220px; accent-color: var(--cy); }
.v-mic { max-width: 280px; }
.comp { letter-spacing: 1px; color: var(--txt2); }
.comp.ok { color: #5fe0b0; }

/* 唤醒状态:纯文本(刻意不做成 chip/输入框样,免和下面「唤醒词」框混淆) */
.wake-cur { color: var(--txt2); font-size: 12.5px; }
</style>
