<script setup lang="ts">
// 设置台:tab 导航 + 意图措辞(设计稿见会话纪要:常规|大脑|声音·暗|家人|远程·暗|系统)。
// 暗 tab 可点、进 teaser 页 —— 能点的必有反应(铁律3),绝不放灰掉的死控件。
import { computed, onMounted, onUnmounted, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import { appVersion, isTauri, openExternal } from '../lib/backend'
import { applyLocale } from '../i18n'
import { useChat } from '../composables/useChat'
import { useSettings } from '../composables/useSettings'
import { useUpdater } from '../composables/useUpdater'
import { useToast } from '../composables/useToast'
import SkinSelect from '../components/SkinSelect.vue'
import ModelPickList from '../components/ModelPickList.vue'
import { useVoiceSettings } from '../composables/settings/useVoiceSettings'
import { useFamilySettings } from '../composables/settings/useFamilySettings'
import { useBrainSettings } from '../composables/settings/useBrainSettings'
import { useDataSettings } from '../composables/settings/useDataSettings'
import { useSystemSettings } from '../composables/settings/useSystemSettings'
import { useScopeSettings } from '../composables/settings/useScopeSettings'
import { useRemoteSettings } from '../composables/settings/useRemoteSettings'

const emit = defineEmits<{ (e: 'close'): void }>()
const { t } = useI18n()
const settings = useSettings()
const { state: chat } = useChat()

type TabId = 'general' | 'brain' | 'voice' | 'family' | 'remote' | 'services' | 'system'
const tabs: { id: TabId; future?: boolean }[] = [
  { id: 'general' },
  { id: 'brain' },
  { id: 'voice' },
  { id: 'family' },
  { id: 'remote' },
  { id: 'services' },
  { id: 'system' },
]
const tab = ref<TabId>('general')

// 各 tab 的逻辑住在 composables/settings/*(2026-09-08 从本文件抽出:script 曾 1663 行)。
// 同名解构 → 模板绑定名一个没变;这里只留 tab 导航、常规 tab 那几项、和「切 tab 拉什么」。
// 标题里的名字跟「叫我什么」联动(ui.pet_name 空 = 默认名 pet.name);
// 与 MainLayout / FloatWindow 同一口径 —— 这是当前 agent 的名字,不是 app 名
const petName = computed(() => settings.get('ui.pet_name') || t('pet.name'))

const { asrOpts, calib, cancelCalib, startCalib, calibStepLabel, calibVerdict, cancelClone, captureRoute, captureSource, cloneArm, cloneBusy, cloneDraft, cloneErr, cloneFile, cloneRecording, loadVoice, loadWebMics, micOpts, micValue, onAsrModel, onCustomFile, onEchoCancel, onLeveling, onNightMode, onNightTime, onTtsBackend, onVoiceSeg, pickCustomVoice, previewSpeaker, previewing, recordClone, removeClone, restartWakeIfRunning, saveClone, saveSensitivity, setMic, toggleWake, voiceInfo, wakeBusy, wakeError, wakeShortName } = useVoiceSettings()
const { addFam, bindChat, channelName, chats, enrollBusy, enrollLabel, famArm, famDraft, famEditing, famError, famNew, famOpts, family, forgetVoice, loadFamily, removeFam, saveFamRename, startEnrollFam, startFamRename, voice } = useFamilySettings(restartWakeIfRunning)
const { CUSTOM_PICK, ENDPOINT_PICK, addCustom, adding, advOpen, autoHint, billingOpts, custom, customInput, customPreset, customReady, endpointInput, endpointQuery, guessWinHint, keyDrafts, loadPick, loadPickCustom, modelDraft, modelInputs, modelMeta, onEndpointKey, onPickKey, onPickKeyCustom, openCustom, ovWinK, pickActive, pickEndpoint, pickErr, pickFiltered, pickLists, pickLoading, pickModel, pickModelCustom, pickOpen, presetRows, protoLabel, protoOpts, saveField, saveKey, saveOv, saveOvRaw, tierOpts, toggleAdv, toggleEndpointPick, togglePick, togglePickCustom, visionOpts } = useBrainSettings()
const { autoBackup, autoBackupBusy, autoBackupLine, autoBackupOff, autoBackupPick, backupBusy, backupErr, backupMsg, backupNow, cancelRelocate, cancelRestore, cleanupOld, confirmRelocate, confirmRestore, dataRoot, gb, keepOld, loadDataLocation, mb, oldDataRoot, pendingMove, pendingRestore, relocate, relocateBusy, relocateError, restoreBusy, restoreError, restorePick, revealData } = useDataSettings(petName)
const { appPublicKey, autostart, autostartBusy, careEnabled, copyPublicKey, credAdding, credBusy, credForm, credHosts, dropCred, floatEnabled, floatShowUsage, isDev, loadAutostart, loadCreds, openQWeatherSite, proxyEnabled, pubKeyCopied, saveCred, setFloatOpacity, setProxy, setQWeather, toggleAutostart, toggleCare, toggleFloat, toggleFloatUsage, toggleProxy, weatherConfigured } = useSystemSettings()
const { addScopeFolder, baselineMode, baselineModeOpts, loadScopes, removeScopeRow, scopeDesktop, scopeDownloads, scopeModeOpts, scopes, setBaselineMode, setScopeMode, userScopes } = useScopeSettings()
const { dt, dtKey, dtSecret, leaveRemoteTab, loadRemote, remoteStatusText, saveRemote, saveRemoteCred, startWeixinLogin, tg, tgToken, toggleRemote, unbindWeixin, wx, wxAccounts, wxLoginStatus, wxQrSvg, wxQrUrl, wxVerifyCode } = useRemoteSettings()
// 关于·版本:真身读 tauri.conf.json(x.y.z),只在系统 tab 拉一次
const appVer = ref('')

// 主动「检查更新」:复用 useUpdater().check()(自动每日检查的手动入口,文案/逻辑早备好只差按钮)。
// true=有新版 → UpdateCard 因 state.available 自动弹,不重复 toast;false=已最新;null=失败 → 各自反馈(§3.5 不静默)。
const updChecking = ref(false)
async function checkUpdate() {
  if (updChecking.value) return
  updChecking.value = true
  try {
    const r = await useUpdater().check()
    if (r === false) useToast().info(t('update.upToDate'))
    else if (r === null) useToast().error(t('update.checkFailed'))
  } finally {
    updChecking.value = false
  }
}

watch(tab, (v) => {
  if (v === 'voice' && !voiceInfo.value) void loadVoice()
  if (v === 'voice') void loadWebMics() // 每次进声音页刷新浏览器麦列表(设备可热插拔)
  if (v === 'system') {
    void loadAutostart()
    void loadDataLocation()
    void loadScopes()
    void loadCreds()
    if (!appVer.value) void appVersion().then((x) => (appVer.value = x))
  }
})
// 唯一脉冲:全局任何时刻最多一个光点,指向当前唯一需要行动的事(现在 = 缺钥匙)
const needKey = computed(() => chat.ready && !chat.hasApiKey)

// 切到远程/家人 tab 时拉一次(切走不轮询)
watch(tab, (v) => {
  if (v === 'remote') void loadRemote()
  else {
    leaveRemoteTab() // 离开远程 tab:作废在跑的扫码轮询 + 收起二维码
  }
  if (v === 'family') void loadFamily()
})

// 段选控件的数据驱动写法:一行配置 = 一个设置项
const segs = computed(() => ({
  character: {
    key: 'ui.character',
    options: ['titan', 'dog', 'cat'].map((v) => ({ v, label: t(`settings.general.char_${v}`) })),
  },
  // 桌宠遛弯显隐(值反义:'0'=显示 / '1'=隐藏);右键「隐藏桌宠」后从这里恢复
  pet: {
    key: 'ui.pet.hidden',
    options: [
      { v: '0', label: t('settings.general.pet_show') },
      { v: '1', label: t('settings.general.pet_hide') },
    ],
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

// 界面语言:选项用各语言自称(不翻译),用户在任何当前语言下都能认出自己那项。
// 切换即时落库 + applyLocale 当窗实时刷新;持久化靠 boot 的 applyLocale(snap.locale)。
const localeOptions = [
  { v: 'zh-CN', label: '中文' },
  { v: 'en', label: 'English' },
]
function setLocale(v: string) {
  settings.set('ui.locale', v)
  applyLocale(v)
}

// 叫我什么:框里直接显示当前生效名(空库就显示默认名,跟标题同一个值——
// 上面是什么,下面就是什么);沿用「唤醒词框」先例:实绑当前值、不靠 placeholder,
// 消除"空框=到底叫啥"的歧义。存库仍保持"空 = 跟随默认名":清空或填回默认名
// 都存空,默认名将来变(换肤/宪法)时自动跟随,不在库里钉死字面量。
const petDraft = ref(petName.value)
async function savePetName() {
  const v = petDraft.value.trim()
  const next = v && v !== t('pet.name') ? v : ''
  const changed = next !== (settings.get('ui.pet_name') || '')
  await settings.set('ui.pet_name', next)
  petDraft.value = petName.value // 回填:清空后框里也显示回默认名,始终与标题一致
  // 名字就是唤醒词(派生,§8.2):改名 → 开着唤醒就重启循环换词即时生效;没开则
  // restartWakeIfRunning 内部会刷状态,让「听哪个词」跟着新名字走(它现问 core,
  // 不吃本页缓存——原先看 voiceInfo 快照,没进过声音 tab 时恒 null → 改名不生效)。
  if (changed && isTauri()) {
    await restartWakeIfRunning()
  }
}

// 我的性格:一句话人格覆盖层(进稳定前缀,下一句话生效);空 = 纯出厂人设
const styleDraft = ref(settings.get('persona.style'))
function saveStyle() {
  settings.set('persona.style', styleDraft.value.trim())
}
// 性格快捷选择:一键填入预设(用户仍可在框里继续改);点「中性」= 清空回到默认中性底座。
// 文案走 i18n → 英文用户点了存英文;命中哪个预设就高亮哪个(改成自定义文本则都不亮)。
const PERSONA_PRESET_KEYS = ['warm', 'lively', 'composed', 'gentle', 'witty'] as const
const personaPresets = computed(() =>
  PERSONA_PRESET_KEYS.map((k) => ({
    k,
    label: t(`settings.general.personaPresets.${k}.label`),
    text: t(`settings.general.personaPresets.${k}.text`),
  })),
)
function applyPreset(text: string) {
  styleDraft.value = text
  saveStyle()
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

    <!-- 表头 + tab 留在滚动区外,内容再长也不随之滚走;滚动条贴内容列(P0) -->
    <div class="view-scroll">
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
        <div class="row persona-row">
          <span class="label">{{ t('settings.general.personaStyle') }}</span>
          <div class="persona-field">
            <textarea
              v-model="styleDraft"
              class="s-input persona-text"
              maxlength="500"
              rows="3"
              :placeholder="t('settings.general.personaStylePlaceholder')"
              @blur="saveStyle"
            ></textarea>
            <div class="persona-chips" :aria-label="t('settings.general.personaQuick')">
              <button
                class="chip preset mini"
                :class="{ on: !styleDraft.trim() }"
                @click="applyPreset('')"
              >{{ t('settings.general.personaPresets.neutral') }}</button>
              <button
                v-for="p in personaPresets"
                :key="p.k"
                class="chip preset mini"
                :class="{ on: styleDraft.trim() === p.text }"
                :title="p.text"
                @click="applyPreset(p.text)"
              >{{ p.label }}</button>
            </div>
          </div>
        </div>
        <div class="row">
          <span class="label">{{ t('settings.general.skin') }}</span>
          <span class="seg">
            <button
              v-for="s in ['scifi', 'warm', 'green', 'night']"
              :key="s"
              :class="{ on: settings.state.skin === s }"
              @click="settings.setSkin(s)"
            >{{ t(`settings.general.skin_${s}`) }}</button>
          </span>
        </div>
        <div v-for="(seg, name) in { character: segs.character, pet: segs.pet, bubble: segs.bubble, textScale: segs.textScale }" :key="name" class="row">
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
        <div class="row">
          <span class="label">{{ t('settings.general.language') }}</span>
          <span class="seg">
            <button
              v-for="o in localeOptions"
              :key="o.v"
              :class="{ on: settings.get('ui.locale') === o.v }"
              @click="setLocale(o.v)"
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
            <!-- 模型:自由文本 + 行尾 ▾ 拉接入点清单(combobox);行/按钮 pointerdown.prevent 保住输入框焦点,
                 免得点选时 blur 先把打了半截的文字当模型存上 -->
            <div class="model-pick" :class="{ open: pickOpen === p.id }">
              <input
                :ref="(el) => { modelInputs[p.id] = el as HTMLInputElement | null }"
                class="s-input s-mono-input"
                :value="modelDraft[p.id] ?? p.model"
                role="combobox"
                :aria-expanded="pickOpen === p.id"
                @change="saveField(p, 'model', $event)"
                @input="modelDraft[p.id] = ($event.target as HTMLInputElement).value; pickActive = -1"
                @keydown="onPickKey(p, $event)"
              />
              <button
                type="button"
                class="pick-btn"
                :title="t('settings.brain.pickModel')"
                :aria-label="t('settings.brain.pickModel')"
                @pointerdown.prevent
                @click="togglePick(p)"
              >▾</button>
              <ModelPickList
                v-if="pickOpen === p.id"
                :loading="pickLoading"
                :err="pickErr"
                :total="pickLists[p.id]?.length ?? 0"
                :rows="pickFiltered"
                :current="p.model"
                :active="pickActive"
                @pick="(id: string) => pickModel(p, id)"
                @refresh="loadPick(p, true)"
                @hover="(i: number) => (pickActive = i)"
              />
            </div>
          </div>
          <!-- 高级:按模型纠正档位/价格/上下文窗口(空 = 用目录猜测,纠错而非配置 §3) -->
          <button class="link adv-toggle" @click="toggleAdv(p)">
            {{ t('settings.brain.advanced') }} {{ advOpen[p.id] ? '▴' : '▾' }}
          </button>
          <div v-if="advOpen[p.id] && modelMeta[p.model]" class="p-grid adv-grid">
            <label>{{ t('settings.brain.tier') }}</label>
            <SkinSelect
              :model-value="modelMeta[p.model].over?.tier ?? ''"
              :options="tierOpts(modelMeta[p.model])"
              :aria-label="t('settings.brain.tier')"
              @update:model-value="(v: string) => saveOvRaw(p, 'tier', v)"
            />
            <label>{{ t('settings.brain.ctxWindow') }}</label>
            <input class="s-input s-mono-input" type="number" min="1"
              :value="ovWinK(modelMeta[p.model].over)"
              :placeholder="guessWinHint(modelMeta[p.model].guess.ctxWindowTokens)"
              @change="saveOv(p, 'ctxWindowTokens', $event)" />
            <label>{{ t('settings.brain.priceIn') }}</label>
            <input class="s-input s-mono-input" type="number" min="0" step="0.01"
              :value="modelMeta[p.model].over?.inUsdPerM ?? ''"
              :placeholder="autoHint(modelMeta[p.model].guess.inUsdPerM)"
              @change="saveOv(p, 'inUsdPerM', $event)" />
            <label>{{ t('settings.brain.priceOut') }}</label>
            <input class="s-input s-mono-input" type="number" min="0" step="0.01"
              :value="modelMeta[p.model].over?.outUsdPerM ?? ''"
              :placeholder="autoHint(modelMeta[p.model].guess.outUsdPerM)"
              @change="saveOv(p, 'outUsdPerM', $event)" />
            <label>{{ t('settings.brain.billing') }}</label>
            <SkinSelect
              :model-value="modelMeta[p.model].over?.billing ?? ''"
              :options="billingOpts"
              :aria-label="t('settings.brain.billing')"
              @update:model-value="(v: string) => saveOvRaw(p, 'billing', v)"
            />
            <label>{{ t('settings.brain.vision') }}</label>
            <SkinSelect
              :model-value="modelMeta[p.model].over?.vision == null ? '' : String(modelMeta[p.model].over?.vision)"
              :options="visionOpts(modelMeta[p.model])"
              :aria-label="t('settings.brain.vision')"
              @update:model-value="(v: string) => saveOvRaw(p, 'vision', v)"
            />
            <p class="adv-hint">{{ t('settings.brain.advHint') }}</p>
          </div>
        </div>

        <!-- 自己接一个大脑 -->
        <button v-if="!adding" class="add-card" @click="openCustom">{{ t('settings.brain.addCustom') }}</button>
        <div v-else class="pcard">
          <div class="p-head">
            <b>{{ t('settings.brain.addCustom') }}</b>
          </div>
          <!-- 从预设开始:选一家自动填名字 / 协议 / 接入点;模型刻意不预填(不写死),贴钥匙后用 ▾ 现查再接入 -->
          <div class="p-grid custom-grid">
            <label>{{ t('settings.brain.customName') }}</label>
            <input v-model="custom.name" class="s-input" :placeholder="t('settings.brain.customNamePlaceholder')" />
            <label>{{ t('settings.brain.protocol') }}</label>
            <SkinSelect :model-value="custom.protocol" :options="protoOpts" :aria-label="t('settings.brain.protocol')" @update:model-value="(v: string) => (custom.protocol = v)" />
            <label>{{ t('settings.brain.endpoint') }}</label>
            <!-- 接入点 ▾ = 厂商预设(千问 / 豆包 / Kimi / 智谱 / 混元 / OpenAI / Gemini / Ollama):选一家连名字 / 协议一起填,
                 手填任意地址照旧。入口放在接入点框本身——人到这个框上找快捷选择(用户实锤),不另起一行 -->
            <div class="model-pick" :class="{ open: pickOpen === ENDPOINT_PICK }">
              <input
                ref="endpointInput"
                v-model="custom.baseUrl"
                class="s-input s-mono-input"
                placeholder="https://…/v1"
                role="combobox"
                :aria-expanded="pickOpen === ENDPOINT_PICK"
                @input="endpointQuery = ($event.target as HTMLInputElement).value; pickActive = -1"
                @keydown="onEndpointKey"
              />
              <button type="button" class="pick-btn" :title="t('settings.brain.preset')" :aria-label="t('settings.brain.preset')" @pointerdown.prevent @click="toggleEndpointPick">▾</button>
              <div v-if="pickOpen === ENDPOINT_PICK" class="pick-list" role="listbox" @pointerdown.prevent>
                <p v-if="!presetRows.length" class="pick-msg">{{ t('settings.brain.presetNoMatch') }}</p>
                <div
                  v-for="(pr, i) in presetRows"
                  :key="pr.id"
                  class="pick-row"
                  role="option"
                  :aria-selected="pr.id === customPreset"
                  :class="{ sel: pr.id === customPreset, active: i === pickActive }"
                  @click="pickEndpoint(pr.id)"
                  @mousemove="pickActive = i"
                >
                  <span class="pick-name">{{ pr.name }}</span>
                  <span class="pick-url">{{ pr.baseUrl }}</span>
                </div>
              </div>
            </div>
            <label>{{ t('settings.brain.keyField') }}</label>
            <input v-model="custom.key" class="s-input" :placeholder="t('settings.brain.keyPlaceholder')" />
            <label>{{ t('settings.brain.model') }}</label>
            <div class="model-pick" :class="{ open: pickOpen === CUSTOM_PICK }">
              <input
                ref="customInput"
                v-model="custom.model"
                class="s-input s-mono-input"
                placeholder="model-id"
                role="combobox"
                :aria-expanded="pickOpen === CUSTOM_PICK"
                @input="pickActive = -1"
                @keydown="onPickKeyCustom"
              />
              <button type="button" class="pick-btn" :title="t('settings.brain.pickModel')" :aria-label="t('settings.brain.pickModel')" @pointerdown.prevent @click="togglePickCustom">▾</button>
              <ModelPickList
                v-if="pickOpen === CUSTOM_PICK"
                :loading="pickLoading"
                :err="pickErr"
                :total="pickLists[CUSTOM_PICK]?.length ?? 0"
                :rows="pickFiltered"
                :current="custom.model"
                :active="pickActive"
                @pick="pickModelCustom"
                @refresh="loadPickCustom(true)"
                @hover="(i: number) => (pickActive = i)"
              />
            </div>
          </div>
          <div class="p-foot">
            <button class="link" :disabled="!customReady" @click="addCustom">{{ t('settings.brain.addSave') }}</button>
            <button class="link dim" @click="adding = false">{{ t('settings.brain.addCancel') }}</button>
          </div>
        </div>
      </div>

      <!-- 家人:渠道归人第一步(声纹后置)——没有「切换用户」概念,谁说话就是谁 -->
      <div v-else-if="tab === 'family'">
        <div v-if="famError" class="lp-error">
          {{ t('common.loadError') }}
          <button class="lp-retry" @click="loadFamily">{{ t('common.retry') }}</button>
        </div>
        <template v-else>
          <div v-for="m in family" :key="m.id" class="row fam-row">
            <template v-if="famEditing === m.id">
              <span class="key-edit">
                <input
                  v-model="famDraft"
                  class="s-input"
                  :placeholder="t('settings.family.namePlaceholder')"
                  @keyup.enter="saveFamRename(m)"
                />
                <button class="link" :disabled="!famDraft.trim()" @click="saveFamRename(m)">
                  {{ t('settings.family.save') }}
                </button>
              </span>
            </template>
            <template v-else>
              <span class="chip" :class="{ on: m.id === settings.state.userId }">{{ m.name }}</span>
              <small v-if="m.id === settings.state.userId" class="fam-you">{{ t('settings.family.you') }}</small>
              <button class="link" @click="startFamRename(m)">{{ t('settings.family.rename') }}</button>
              <!-- 声纹:让旺财凭声音认出 TA(录 3 段);认出后 TA 的话记忆归 TA。owner 也可录以便区分 -->
              <span v-if="enrollBusy(m.id)" class="fam-enroll-hint">{{ enrollLabel(m.id) }}</span>
              <template v-else>
                <small v-if="m.enrolled" class="fam-enrolled">{{ t('settings.family.enrolled') }}</small>
                <button class="link" @click="startEnrollFam(m)">
                  {{ m.enrolled ? t('settings.family.reEnroll') : t('settings.family.enroll') }}
                </button>
                <button v-if="m.enrolled" class="link fam-forget" @click="forgetVoice(m)">
                  {{ t('settings.family.forgetVoice') }}
                </button>
              </template>
              <button
                v-if="m.id !== settings.state.userId"
                class="chip-del"
                :class="{ armed: famArm === m.id }"
                @click="removeFam(m)"
              >{{ famArm === m.id ? t('settings.family.deleteArm') : '✕' }}</button>
            </template>
          </div>
          <div class="row">
            <span class="key-edit">
              <input
                v-model="famNew"
                class="s-input"
                :placeholder="t('settings.family.addPlaceholder')"
                @keyup.enter="addFam"
              />
              <button class="link" :disabled="!famNew.trim()" @click="addFam">
                {{ t('settings.family.add') }}
              </button>
            </span>
          </div>
          <p class="hint">{{ t('settings.family.membersHint') }}</p>
          <p class="hint">{{ t('settings.family.enrollHint', { name: petName }) }}</p>

          <p class="section">{{ t('settings.family.chats') }}</p>
          <p v-if="!chats.length" class="hint">{{ t('settings.family.chatsEmpty') }}</p>
          <div v-for="c in chats" :key="c.id" class="row fam-row">
            <span class="chip">{{ channelName(c.channel) }}</span>
            <span class="fam-chat-label" :title="c.ext_id">{{ c.label || c.ext_id }}</span>
            <SkinSelect
              class="fam-select"
              :model-value="String(c.user_id ?? '')"
              :options="famOpts"
              :aria-label="t('settings.family.unassigned')"
              @update:model-value="(v: string) => bindChat(c, v)"
            />
          </div>
          <p v-if="chats.length" class="hint">{{ t('settings.family.chatsHint', { name: petName }) }}</p>
        </template>
      </div>

      <!-- 声音(PLAN §11):第一层只放高频两项;高级分组线下(强默认收口) -->
      <div v-else-if="tab === 'voice'">
        <div class="row v-speaker">
          <span class="label">{{ t('settings.voice.speaker') }}</span>
          <span class="sp-list">
            <!-- chip 与它的删除 ✕ 包成一组:flex-wrap 换行时一起走,别把 ✕ 拆到下一行开头 -->
            <span v-for="sp in voiceInfo?.speakers ?? []" :key="sp.id" class="sp-pair">
              <button
                class="chip sp"
                :class="{ on: (settings.get('voice.speaker') || voiceInfo?.defaultSpeaker) === sp.id, busy: previewing === sp.id }"
                :title="t('settings.voice.preview')"
                @click="previewSpeaker(sp.id)"
              >{{ sp.name }}</button>
              <button
                v-if="sp.isClone && !sp.builtin"
                class="chip-del"
                :class="{ armed: cloneArm === sp.id }"
                :title="t('settings.voice.cloneDelete')"
                @click="removeClone(sp.id)"
              >{{ cloneArm === sp.id ? t('settings.voice.cloneDeleteArm') : '✕' }}</button>
            </span>
            <button class="chip sp custom" :class="{ recording: cloneRecording }" :disabled="cloneBusy" @click="recordClone">
              {{ cloneRecording ? t('settings.voice.cloneRecording') : '🎙 ' + t('settings.voice.cloneRecord') }}
            </button>
            <button class="chip sp custom" :disabled="cloneBusy" @click="pickCustomVoice">
              {{ cloneBusy && !cloneRecording && !cloneDraft ? t('settings.voice.cloneImporting') : '+ ' + t('settings.voice.customAdd') }}
            </button>
            <input ref="cloneFile" type="file" accept="audio/*" class="hidden-file" @change="onCustomFile" />
          </span>
        </div>
        <!-- 自定义音色草稿:转写可改 + 起名 → 保存落库(选文件→解码→import→draft→save) -->
        <div v-if="cloneErr" class="row v-speaker">
          <span class="label"></span>
          <span class="clone-err">{{ cloneErr }}</span>
        </div>
        <div v-if="cloneDraft" class="row v-speaker clone-edit">
          <span class="label">{{ t('settings.voice.customAdd') }}</span>
          <div class="clone-form">
            <input
              v-model="cloneDraft.name"
              class="clone-input"
              :placeholder="t('settings.voice.cloneNamePlaceholder')"
            />
            <textarea
              v-model="cloneDraft.transcript"
              class="clone-text"
              rows="3"
              :placeholder="t('settings.voice.transcriptPlaceholder')"
            ></textarea>
            <p class="clone-hint">{{ t('settings.voice.transcriptHint') }}</p>
            <!-- 参考音体检:录音有毛病就说清怎么改(克隆会把录音条件当音色一起学走);
                 没毛病不出声(§3 收敛)。仍可直接保存 —— 是建议不是闸。 -->
            <p v-if="cloneDraft.issue" class="clone-warn">
              {{ t(`settings.voice.refAudio.${cloneDraft.issue}`) }}
            </p>
            <div class="clone-actions">
              <button class="chip" :disabled="cloneBusy" @click="cancelClone">
                {{ t('settings.voice.cloneCancel') }}
              </button>
              <button
                class="chip on"
                :disabled="cloneBusy || !cloneDraft.name.trim() || !cloneDraft.transcript.trim()"
                @click="saveClone"
              >{{ cloneBusy ? t('settings.voice.cloneSaving') : t('settings.voice.cloneSave') }}</button>
            </div>
          </div>
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
        <!-- 唤醒词 = 名字派生(没有独立设置,§8.2「起什么名字就怎么唤醒」):
             名字喊不了(英文单词)→ 如实提示回落词;名字只有一个音 → 提示可能不灵 -->
        <p v-if="voiceInfo?.wakeFallback" class="hint warn">
          {{ t('settings.voice.wakeNameFallback', { name: petName, kw: (voiceInfo?.keywords ?? []).join('、') }) }}
        </p>
        <p v-else-if="wakeShortName" class="hint">{{ t('settings.voice.wakeShortName') }}</p>

        <p class="section">{{ t('settings.voice.advanced') }}</p>
        <div class="row">
          <span class="label">{{ t('settings.voice.sensitivity') }}</span>
          <span class="sens">
            <small>{{ t('settings.voice.sensSteady') }}</small>
            <input
              class="v-vol"
              type="range"
              min="0"
              max="100"
              step="5"
              :value="Number(settings.get('voice.wake.sensitivity') || '100')"
              @input="settings.set('voice.wake.sensitivity', ($event.target as HTMLInputElement).value)"
              @change="saveSensitivity(Number(($event.target as HTMLInputElement).value))"
            />
            <small>{{ t('settings.voice.sensKeen') }}</small>
            <!-- 录音标定挪进灵敏度行当小入口:不想盲拖就录几遍,按真实发音+环境(必要时连触发拼写)定到正好 -->
            <button v-if="calib.running" class="link calib-link" @click="cancelCalib">{{ t('settings.voice.calibCancel') }}</button>
            <button v-else class="link calib-link" @click="startCalib">
              {{ calib.phase === 'done' ? t('settings.voice.calibAgain') : t('settings.voice.calibStart') }}
            </button>
          </span>
        </div>
        <!-- 平时不占版面(去掉了原来两段大提示);只有标定进行中给念词引导、刚结束报一句结果 -->
        <p v-if="calib.running" class="hint">
          <span class="calib-live" :class="{ pulse: calib.listening }">{{ calibStepLabel }}</span>
          {{ t('settings.voice.calibSayHint', { kw: (voiceInfo?.keywords ?? []).join('、') }) }}
        </p>
        <p v-else-if="calib.phase === 'done' && calib.result" class="hint" :class="{ ok: calib.result.ok }">{{ calibVerdict }}</p>
        <div v-for="(seg, name) in { rate: segs.rate, patience: segs.patience }" :key="name" class="row">
          <span class="label">{{ t(`settings.voice.${name}`) }}</span>
          <span class="seg">
            <button
              v-for="o in seg.options"
              :key="o.v"
              :class="{ on: settings.get(seg.key) === o.v }"
              @click="onVoiceSeg(seg.key, o.v)"
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
          <span class="label">{{ t('settings.voice.echoCancel') }}</span>
          <span class="seg">
            <button
              v-for="v in ['auto', 'browser', 'cpal']"
              :key="v"
              :class="{ on: captureSource === v }"
              @click="onEchoCancel(v)"
            >{{
              t(
                v === 'auto'
                  ? 'settings.voice.aecAuto'
                  : v === 'browser'
                    ? 'settings.audio.on'
                    : 'settings.audio.off',
              )
            }}</button>
          </span>
        </div>
        <p class="hint">{{ t('settings.voice.echoCancelHint') }}</p>
        <p v-if="captureSource === 'auto'" class="hint">
          {{
            captureRoute.state.headphones === true
              ? t('settings.voice.aecAutoHp')
              : captureRoute.state.headphones === false
                ? t('settings.voice.aecAutoSpk')
                : t('settings.voice.aecAutoUnknown')
          }}
        </p>
        <div class="row">
          <span class="label">{{ t('settings.voice.micDevice') }}</span>
          <SkinSelect
            class="v-mic"
            :model-value="micValue"
            :options="micOpts"
            :aria-label="t('settings.voice.micDevice')"
            @update:model-value="setMic"
          />
        </div>
        <!-- 在线/离线合成档(D 期):离线断网也能说,但要下个大模型、音色单一 -->
        <div class="row">
          <span class="label">{{ t('settings.voice.ttsBackend') }}</span>
          <span class="seg">
            <button
              v-for="b in ['online', 'offline']"
              :key="b"
              :class="{ on: (settings.get('voice.tts_backend') || 'online') === b }"
              @click="onTtsBackend(b)"
            >{{ t(`settings.voice.tts_${b}`) }}</button>
          </span>
        </div>
        <!-- 识别模型(2026-06 放出来选;2026-08-28 扩 4 档防单一下载源挂掉):SenseVoice 快(默认)/
             FireRed 最准 / Fun-ASR Nano 远场抗噪方言 / Paraformer 老牌备胎。
             模型用时下载;换档后开着唤醒会重启循环让新模型生效(同 sensitivity) -->
        <div class="row">
          <span class="label">{{ t('settings.voice.asrModel') }}</span>
          <SkinSelect
            class="v-mic"
            :model-value="settings.get('voice.asr.model') || 'sense-voice'"
            :options="asrOpts"
            :aria-label="t('settings.voice.asrModel')"
            @update:model-value="onAsrModel"
          />
        </div>
        <p class="hint">{{ t('settings.voice.asrModelHint') }}</p>
        <div class="row">
          <span class="label">{{ t('settings.voice.component') }}</span>
          <span class="s-mono comp" :class="{ ok: voiceInfo?.asrReady }">
            {{ voiceInfo?.asrReady ? t('settings.voice.compReady') : t('settings.voice.compMissing') }}
          </span>
        </div>
        <!-- Windows「通信活动自动压低」联动(robot 真机坑):常驻唤醒麦会让系统把
             其它声音压 80%,app 改不了,只能引导用户改系统设置 -->
        <p class="hint">{{ t('settings.voice.winDuckHint') }}</p>
        <p class="hint">{{ t('settings.voice.winMicHint') }}</p>
        <!-- 播放响度均衡 / 夜间模式(客户端 Web Audio;电影/歌一起稳、不炸,夜间自动压平大动态)。
             旺财嗓音也进链但恒日间档、不被夜间压(晚上你正跟它说话)。总开关关=不接管、原样播放。 -->
        <div class="row">
          <span class="label">{{ t('settings.audio.leveling') }}</span>
          <span class="seg">
            <button
              v-for="v in ['1', '0']"
              :key="v"
              :class="{ on: (settings.get('audio.leveling') || '1') === v }"
              @click="onLeveling(v)"
            >{{ t(v === '1' ? 'settings.audio.on' : 'settings.audio.off') }}</button>
          </span>
        </div>
        <div class="row">
          <span class="label">{{ t('settings.audio.nightMode') }}</span>
          <span class="seg">
            <button
              v-for="v in ['off', 'on', 'auto']"
              :key="v"
              :class="{ on: (settings.get('audio.night_mode') || 'auto') === v }"
              @click="onNightMode(v)"
            >{{ t(`settings.audio.night_${v}`) }}</button>
          </span>
        </div>
        <div v-if="(settings.get('audio.night_mode') || 'auto') === 'auto'" class="row">
          <span class="label">{{ t('settings.audio.nightWindow') }}</span>
          <span class="v-night">
            <input
              class="s-input v-time"
              type="time"
              :value="settings.get('audio.night_start') || '22:00'"
              @change="onNightTime('audio.night_start', $event)"
            />
            <span class="v-time-sep">–</span>
            <input
              class="s-input v-time"
              type="time"
              :value="settings.get('audio.night_end') || '07:00'"
              @change="onNightTime('audio.night_end', $event)"
            />
          </span>
        </div>
        <p class="hint">{{ t('settings.audio.hint', { name: petName }) }}</p>

        <!-- ⚗️ 临时:采集端 AEC spike(层1 第0步),拿到 Windows 真机结论就删 -->
      </div>

      <!-- 远程渠道:手机上跟旺财对话(Telegram/钉钉 bot)。凭证写得进读不回(同供应商 key) -->
      <div v-else-if="tab === 'remote'">
        <p class="section">{{ t('settings.remote.telegram.title') }}</p>
        <p class="hint">{{ t('settings.remote.telegram.hint', { name: petName }) }}</p>

        <div class="row">
          <span class="label">{{ t('settings.remote.enable') }}</span>
          <span class="chip" :class="{ on: tg.enabled }">{{ tg.enabled ? t('settings.system.on') : t('settings.system.off') }}</span>
          <button class="link" @click="toggleRemote('telegram', !tg.enabled)">
            {{ tg.enabled ? t('settings.system.turnOff') : t('settings.system.turnOn') }}
          </button>
        </div>
        <div class="row">
          <span class="label">{{ t('settings.remote.telegram.token') }}</span>
          <input
            v-model="tgToken"
            class="s-input s-mono-input"
            :placeholder="tg.configured ? t('settings.remote.tokenSet') : t('settings.remote.telegram.tokenPlaceholder')"
            @change="saveRemoteCred('remote.telegram.token', tgToken)"
          />
        </div>
        <div class="row">
          <span class="label">{{ t('settings.remote.telegram.allowed') }}</span>
          <input
            class="s-input s-mono-input"
            :value="tg.allowed_chats"
            :placeholder="t('settings.remote.telegram.allowedPlaceholder')"
            @change="saveRemote('remote.telegram.allowed_chats', $event)"
          />
        </div>
        <div class="row">
          <span class="label">{{ t('settings.remote.status') }}</span>
          <span class="chip" :class="{ on: tg.running, warn: !!tg.last_error }">{{ remoteStatusText(tg) }}</span>
        </div>

        <p class="hint">{{ t('settings.remote.telegram.steps') }}</p>
        <p class="hint">
          {{ t('settings.remote.telegram.linkPre') }}
          <button class="link" @click="openExternal('https://t.me/botfather')">@BotFather</button>
        </p>

        <p class="section dt-sec">{{ t('settings.remote.dingtalk.title') }}</p>
        <p class="hint">{{ t('settings.remote.dingtalk.hint', { name: petName }) }}</p>
        <div class="row">
          <span class="label">{{ t('settings.remote.enable') }}</span>
          <span class="chip" :class="{ on: dt.enabled }">{{ dt.enabled ? t('settings.system.on') : t('settings.system.off') }}</span>
          <button class="link" @click="toggleRemote('dingtalk', !dt.enabled)">
            {{ dt.enabled ? t('settings.system.turnOff') : t('settings.system.turnOn') }}
          </button>
        </div>
        <div class="row">
          <span class="label">{{ t('settings.remote.dingtalk.appKey') }}</span>
          <input
            v-model="dtKey"
            class="s-input s-mono-input"
            :placeholder="dt.configured ? t('settings.remote.tokenSet') : t('settings.remote.dingtalk.appKeyPlaceholder')"
            @change="saveRemoteCred('remote.dingtalk.app_key', dtKey)"
          />
        </div>
        <div class="row">
          <span class="label">{{ t('settings.remote.dingtalk.appSecret') }}</span>
          <input
            v-model="dtSecret"
            class="s-input s-mono-input"
            :placeholder="dt.configured ? t('settings.remote.tokenSet') : t('settings.remote.dingtalk.appSecretPlaceholder')"
            @change="saveRemoteCred('remote.dingtalk.app_secret', dtSecret)"
          />
        </div>
        <div class="row">
          <span class="label">{{ t('settings.remote.status') }}</span>
          <span class="chip" :class="{ on: dt.running, warn: !!dt.last_error }">{{ remoteStatusText(dt) }}</span>
        </div>
        <p class="hint">{{ t('settings.remote.dingtalk.steps') }}</p>
        <p class="hint">
          {{ t('settings.remote.dingtalk.linkPre') }}
          <button class="link" @click="openExternal('https://open-dev.dingtalk.com/')">open-dev.dingtalk.com</button>
        </p>

        <!-- 微信(腾讯 iLink bot):扫码登录(区别于 TG/钉钉粘贴 token),confirmed 即连 -->
        <p class="section dt-sec">{{ t('settings.remote.weixin.title') }}</p>
        <p class="hint">{{ t('settings.remote.weixin.hint', { name: petName }) }}</p>
        <div class="row">
          <span class="label">{{ t('settings.remote.enable') }}</span>
          <span class="chip" :class="{ on: wx.enabled }">{{ wx.enabled ? t('settings.system.on') : t('settings.system.off') }}</span>
          <button class="link" @click="toggleRemote('weixin', !wx.enabled)">
            {{ wx.enabled ? t('settings.system.turnOff') : t('settings.system.turnOn') }}
          </button>
        </div>
        <div class="row">
          <span class="label">{{ t('settings.remote.weixin.login') }}</span>
          <button class="link" @click="startWeixinLogin">
            {{ wx.configured ? t('settings.remote.weixin.relogin') : t('settings.remote.weixin.scan') }}
          </button>
        </div>
        <div v-if="wxQrSvg" class="wx-qr">
          <!-- eslint-disable-next-line vue/no-v-html -- SVG 由 core qrcode crate 生成(非用户内容),可信 -->
          <div class="wx-qr-img" v-html="wxQrSvg"></div>
          <p class="hint">{{ t('settings.remote.weixin.scanHint') }}</p>
          <p v-if="wxLoginStatus === 'scaned'" class="hint">{{ t('settings.remote.weixin.scaned') }}</p>
          <div v-if="wxLoginStatus === 'need_verifycode'" class="row">
            <span class="label">{{ t('settings.remote.weixin.code') }}</span>
            <input v-model="wxVerifyCode" class="s-input" :placeholder="t('settings.remote.weixin.codePlaceholder')" />
          </div>
          <p class="hint">
            {{ t('settings.remote.weixin.linkPre') }}
            <button class="link" @click="openExternal(wxQrUrl)">{{ t('settings.remote.weixin.linkText') }}</button>
          </p>
        </div>
        <p v-if="wxLoginStatus === 'expired'" class="hint">{{ t('settings.remote.weixin.expired') }}</p>
        <p v-if="wxLoginStatus === 'verify_blocked'" class="hint">{{ t('settings.remote.weixin.blocked') }}</p>
        <!-- 多绑定列表(一人一 bot):每行一个绑定者,可单独解绑 -->
        <div v-for="a in wxAccounts" :key="a" class="row">
          <span class="label">{{ t('settings.remote.weixin.bound') }}</span>
          <span class="s-mono-text">{{ a || t('settings.remote.weixin.legacyBound') }}</span>
          <button class="link" @click="unbindWeixin(a)">{{ t('settings.remote.weixin.unbind') }}</button>
        </div>
        <div class="row">
          <span class="label">{{ t('settings.remote.weixin.allowed') }}</span>
          <input
            class="s-input s-mono-input"
            :value="wx.allowed_chats"
            :placeholder="t('settings.remote.weixin.allowedPlaceholder')"
            @change="saveRemote('remote.weixin.allowed_users', $event)"
          />
        </div>
        <div class="row">
          <span class="label">{{ t('settings.remote.status') }}</span>
          <span class="chip" :class="{ on: wx.running, warn: !!wx.last_error }">{{ remoteStatusText(wx) }}</span>
        </div>
        <p class="hint">{{ t('settings.remote.weixin.steps') }}</p>
        <p class="hint wx-risk">{{ t('settings.remote.weixin.risk') }}</p>
      </div>

      <!-- 服务/接入:外部数据源与设备接入(天气走和风 JWT;以后智能家居 HA 等同构进驻) -->
      <div v-else-if="tab === 'services'">
        <p class="section">{{ t('settings.services.weather') }}</p>
        <p class="hint">{{ t('settings.services.weatherHint') }}</p>

        <!-- 全局应用公钥:一直显示,复制到和风控制台创建 JWT 凭据(所有 Ed25519 服务共用这一把) -->
        <div class="row">
          <span class="label">{{ t('settings.services.pubKey') }}</span>
          <button class="link" :disabled="!appPublicKey" @click="copyPublicKey">
            {{ pubKeyCopied ? t('settings.services.pubKeyCopied') : t('settings.services.pubKeyCopy') }}
          </button>
        </div>
        <textarea
          class="s-input s-mono-input pubkey-box"
          readonly
          rows="3"
          :value="appPublicKey || t('settings.services.pubKeyPending')"
        ></textarea>

        <div class="row">
          <span class="label">{{ t('settings.services.projectId') }}</span>
          <input
            class="s-input s-mono-input"
            :value="settings.get('weather.qweather.project_id')"
            :placeholder="t('settings.services.projectIdPlaceholder')"
            @change="setQWeather('weather.qweather.project_id', $event)"
          />
        </div>
        <div class="row">
          <span class="label">{{ t('settings.services.credentialId') }}</span>
          <input
            class="s-input s-mono-input"
            :value="settings.get('weather.qweather.credential_id')"
            :placeholder="t('settings.services.credentialIdPlaceholder')"
            @change="setQWeather('weather.qweather.credential_id', $event)"
          />
        </div>
        <div class="row">
          <span class="label">{{ t('settings.services.weatherHost') }}</span>
          <input
            class="s-input s-mono-input"
            :value="settings.get('weather.qweather.host')"
            :placeholder="t('settings.services.weatherHostPlaceholder')"
            @change="setQWeather('weather.qweather.host', $event)"
          />
        </div>

        <div class="row">
          <span class="label">{{ t('settings.services.weatherSource') }}</span>
          <span class="chip" :class="{ on: weatherConfigured }">
            {{ weatherConfigured ? t('settings.services.weatherSrcQweather') : t('settings.services.weatherSrcFree') }}
          </span>
        </div>

        <p class="hint">{{ t('settings.services.weatherSteps') }}</p>
        <p class="hint">
          {{ t('settings.services.weatherLinkPre') }}
          <button class="link" @click="openQWeatherSite">{{ t('settings.services.weatherLink') }}</button>
        </p>
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
        <p class="hint">{{ isDev ? t('settings.system.autostartDev') : t('settings.system.autostartHint', { name: petName }) }}</p>
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
        <div class="row">
          <span class="label">{{ t('settings.system.care') }}</span>
          <span class="key-state">
            <span class="chip" :class="{ on: careEnabled }">{{ careEnabled ? t('settings.system.on') : t('settings.system.off') }}</span>
            <button class="link" @click="toggleCare">{{ careEnabled ? t('settings.system.turnOff') : t('settings.system.turnOn') }}</button>
          </span>
        </div>
        <p class="hint">{{ t('settings.system.careHint', { name: petName }) }}</p>

        <p class="section">{{ t('settings.scopes.title') }}</p>
        <p class="hint">{{ t('settings.scopes.hint', { name: petName }) }}</p>
        <!-- 内置区:程序数据(说明行)+ 下载/桌面(出厂「可存入」,可升「完全访问」) -->
        <div class="row">
          <span class="label">{{ t('settings.scopes.dataDir') }}</span>
          <span class="key-state scope-note">{{ t('settings.scopes.dataDirNote') }}</span>
        </div>
        <div v-if="scopeDownloads" class="row">
          <span class="label" :title="scopeDownloads">{{ t('settings.scopes.downloads') }}</span>
          <span class="key-state">
            <SkinSelect
              :model-value="baselineMode(scopeDownloads)"
              :options="baselineModeOpts"
              :aria-label="t('settings.scopes.title')"
              @update:model-value="(v: string) => setBaselineMode(scopeDownloads, v)"
            />
          </span>
        </div>
        <div v-if="scopeDesktop" class="row">
          <span class="label" :title="scopeDesktop">{{ t('settings.scopes.desktop') }}</span>
          <span class="key-state">
            <SkinSelect
              :model-value="baselineMode(scopeDesktop)"
              :options="baselineModeOpts"
              :aria-label="t('settings.scopes.title')"
              @update:model-value="(v: string) => setBaselineMode(scopeDesktop, v)"
            />
          </span>
        </div>
        <!-- 用户授权的文件夹:三档可调、可移除 -->
        <div v-for="e in userScopes" :key="e.path" class="row">
          <span class="label scope-path" :title="e.path">{{ e.path }}</span>
          <span class="key-state">
            <SkinSelect
              :model-value="e.mode"
              :options="scopeModeOpts"
              :aria-label="t('settings.scopes.title')"
              @update:model-value="(v: string) => setScopeMode(e.path, v)"
            />
            <button class="link" @click="removeScopeRow(e.path)">{{ t('settings.scopes.remove') }}</button>
          </span>
        </div>
        <div class="row">
          <span class="label"></span>
          <span class="key-state">
            <button class="link" @click="addScopeFolder">{{ t('settings.scopes.add') }}</button>
          </span>
        </div>
        <p class="hint">{{ t('settings.scopes.askHint', { name: petName }) }}</p>

        <p class="section">{{ t('settings.system.storage') }}</p>
        <div class="row">
          <span class="label">{{ t('settings.system.dataLocation') }}</span>
          <span class="key-state">
            <button class="link" :disabled="relocateBusy || restoreBusy" @click="revealData">{{ t('settings.system.dataReveal') }}</button>
            <button class="link" :disabled="relocateBusy || backupBusy || restoreBusy" @click="backupNow">{{ backupBusy ? t('settings.system.backingUp') : t('settings.system.backup') }}</button>
            <button class="link" :disabled="relocateBusy || backupBusy || restoreBusy" @click="restorePick">{{ restoreBusy ? t('settings.system.restoring') : t('settings.system.restore') }}</button>
            <button class="link" :disabled="relocateBusy || backupBusy || restoreBusy" @click="relocate">{{ relocateBusy ? t('settings.system.relocating') : t('settings.system.relocate') }}</button>
          </span>
        </div>
        <p class="hint s-mono">{{ dataRoot || '—' }}</p>
        <p v-if="backupMsg" class="hint" :class="{ 'data-err': backupErr, 's-mono': !backupErr }">{{ backupMsg }}</p>
        <div v-if="pendingMove" class="data-confirm">
          <p>{{ t('settings.system.relocateConfirm', { path: pendingMove.newRoot, size: gb(pendingMove.needBytes), name: petName }) }}</p>
          <span class="key-state">
            <button class="link strong" @click="confirmRelocate">{{ t('settings.system.relocateGo') }}</button>
            <button class="link" @click="cancelRelocate">{{ t('settings.system.relocateCancel') }}</button>
          </span>
        </div>
        <p v-if="relocateError" class="hint data-err">{{ relocateError }}</p>
        <div v-if="pendingRestore" class="data-confirm">
          <p>{{ t('settings.system.restoreConfirm', { size: mb(pendingRestore.dbBytes), clones: pendingRestore.clones, name: petName }) }}</p>
          <span class="key-state">
            <button class="link strong" :disabled="restoreBusy" @click="confirmRestore">{{ t('settings.system.restoreGo') }}</button>
            <button class="link" :disabled="restoreBusy" @click="cancelRestore">{{ t('settings.system.restoreCancel') }}</button>
          </span>
        </div>
        <p v-if="restoreError" class="hint data-err">{{ restoreError }}</p>
        <p class="hint">{{ t('settings.system.dataLocationHint') }}</p>
        <!-- 自动备份:选目录即开(每周一份、留 10 份、机器轮转);清空 = 关。默认关——目标必须是
             另一块盘/目录,只有用户知道备到哪(§4.11 2026-08-31)。 -->
        <div class="row">
          <span class="label">{{ t('settings.system.autoBackup') }}</span>
          <span class="key-state">
            <button class="link" :disabled="autoBackupBusy" @click="autoBackupPick">
              {{ autoBackupBusy ? t('settings.system.autoBackupWorking') : (autoBackup.dir ? t('settings.system.autoBackupChange') : t('settings.system.autoBackupOn')) }}
            </button>
            <button v-if="autoBackup.dir" class="link" :disabled="autoBackupBusy" @click="autoBackupOff">{{ t('settings.system.autoBackupOffBtn') }}</button>
          </span>
        </div>
        <p v-if="autoBackup.dir" class="hint s-mono">{{ autoBackup.dir }}</p>
        <p v-if="autoBackupLine" class="hint" :class="{ 'data-err': !!autoBackup.lastError && !autoBackup.lastOkMs }">{{ autoBackupLine }}</p>
        <p class="hint">{{ t('settings.system.autoBackupHint') }}</p>
        <div v-if="oldDataRoot" class="row">
          <span class="label">{{ t('settings.system.oldData') }}</span>
          <span class="key-state">
            <button class="link" @click="cleanupOld">{{ t('settings.system.oldDataDelete') }}</button>
            <button class="link" @click="keepOld">{{ t('settings.system.oldDataKeep') }}</button>
          </span>
        </div>
        <p v-if="oldDataRoot" class="hint s-mono">{{ oldDataRoot }}</p>

        <p class="section">{{ t('settings.system.network') }}</p>
        <!-- 一行:代理 + 开关 + 地址输入(开关切 net.proxy_enabled,地址始终可改、关掉也留) -->
        <div class="row">
          <span class="label">{{ t('settings.system.proxy') }}</span>
          <span class="key-state proxy-line">
            <button class="link" @click="toggleProxy">{{ proxyEnabled ? t('settings.system.turnOff') : t('settings.system.turnOn') }}</button>
            <input
              class="s-input s-mono-input"
              :class="{ off: !proxyEnabled }"
              :value="settings.get('net.proxy')"
              :placeholder="t('settings.system.proxyPlaceholder')"
              @change="setProxy"
            />
          </span>
        </div>
        <p class="hint">{{ t('settings.system.proxyHint') }}</p>

        <!-- 下载认证:给需要账号的地址(WebDAV / 自家 NAS / 网盘挂载)存账号,下载时按
             网址自动带上。密码只进不出(存进系统密钥串),所以列表只显示网址。 -->
        <p class="section">{{ t('settings.system.credTitle') }}</p>
        <div v-for="h in credHosts" :key="h" class="row">
          <span class="label s-mono-input">{{ h }}</span>
          <span class="key-state">
            <button class="link" :disabled="credBusy" @click="dropCred(h)">
              {{ t('settings.system.credRemove') }}
            </button>
          </span>
        </div>
        <div v-if="credAdding" class="row cred-form">
          <input
            v-model="credForm.host"
            class="s-input s-mono-input"
            :placeholder="t('settings.system.credHostPlaceholder')"
          />
          <input
            v-model="credForm.user"
            class="s-input"
            :placeholder="t('settings.system.credUser')"
          />
          <input
            v-model="credForm.password"
            class="s-input"
            type="password"
            :placeholder="t('settings.system.credPassword')"
          />
          <button class="link" :disabled="credBusy || !credForm.host.trim()" @click="saveCred">
            {{ credBusy ? t('settings.system.busy') : t('settings.system.credSave') }}
          </button>
          <button class="link" :disabled="credBusy" @click="credAdding = false">
            {{ t('settings.system.credCancel') }}
          </button>
        </div>
        <div v-else class="row">
          <span class="label">{{ credHosts.length ? '' : t('settings.system.credEmpty') }}</span>
          <span class="key-state">
            <button class="link" @click="credAdding = true">{{ t('settings.system.credAdd') }}</button>
          </span>
        </div>
        <p class="hint">{{ t('settings.system.credHint') }}</p>

        <p class="section">{{ t('settings.system.about') }}</p>
        <div class="row">
          <span class="label">{{ t('settings.system.version') }}</span>
          <span class="key-state">
            <span class="s-mono">v{{ appVer || '0.1.0' }} · {{ t('settings.system.selfId') }}</span>
            <button class="link" :disabled="updChecking" @click="checkUpdate">
              {{ updChecking ? t('update.checking') : t('update.check') }}
            </button>
          </span>
        </div>
      </div>
    </div>
    </div>
    <!-- 搬家中:全屏遮罩 + 详细进度在 HUD(完成后自动重启)。期间锁交互,别让新写入落老盘。 -->
    <div v-if="relocateBusy" class="relocate-veil">
      <div class="relocate-card">
        <div class="spinner" />
        <p>{{ t('settings.system.relocatingTitle') }}</p>
        <p class="sub">{{ t('settings.system.relocatingSub', { name: petName }) }}</p>
      </div>
    </div>
  </section>
</template>

<style scoped>
/* 数据「搬家」:内联确认条 + 错误 + 搬家中遮罩 */
.data-confirm { margin-top: 12px; padding: 12px 14px; border: 1px solid var(--accent); border-radius: 10px; background: rgba(var(--accent-rgb), 0.06); display: flex; flex-direction: column; gap: 10px; }
.data-confirm p { font-size: 12.5px; color: var(--text); line-height: 1.6; word-break: break-all; }
.link.strong { font-weight: 600; }
.data-err { color: var(--danger); }
.relocate-veil { position: fixed; inset: 0; z-index: 50; display: flex; align-items: center; justify-content: center; background: rgba(var(--veil-rgb, 0 0 0), 0.55); backdrop-filter: blur(2px); }
.relocate-card { display: flex; flex-direction: column; align-items: center; gap: 12px; padding: 28px 34px; border-radius: 14px; background: var(--surface); border: 1px solid var(--line); box-shadow: 0 20px 60px rgba(0, 0, 0, 0.4); max-width: 360px; text-align: center; }
.relocate-card p { font-size: 14px; color: var(--text); }
.relocate-card .sub { font-size: 12px; color: var(--text-dim); line-height: 1.6; }
.relocate-card .spinner { width: 30px; height: 30px; border: 3px solid var(--line); border-top-color: var(--accent); border-radius: 50%; animation: relocate-spin 0.8s linear infinite; }
@keyframes relocate-spin { to { transform: rotate(360deg); } }
/* 滚动交给 .view-scroll(全局);.settings 只当竖向骨架,表头/tab 钉在滚动区外 */
/* 居中:标题/tab/内容体都限宽 712 一起居中,宽窗口不再右边空一大块(共用壳 .view-shell 同步) */
.settings { flex: 1; display: flex; flex-direction: column; min-width: 0; align-items: center; }
/* padding-right 让「回去聊天」避开右上角窗控三键(二轮真机修复:不再重叠) */
.s-head { display: flex; align-items: flex-start; justify-content: space-between; gap: 12px; padding: 16px 26px 12px; padding-right: 84px; width: 100%; max-width: 712px; }
.s-title b { font-size: 16px; color: var(--text); }
.s-title small { display: block; margin-top: 3px; font-size: 12px; color: var(--text-dim); }
.s-mono { font-family: ui-monospace, "SF Mono", monospace; font-size: 10px; letter-spacing: 2px; color: var(--text-dim); margin-left: 8px; }
.s-back { background: none; border: 1px solid var(--line); border-radius: 9px; color: var(--text-dim); cursor: pointer; padding: 5px 10px; font-size: 12px; }
.s-back:hover { color: var(--accent); border-color: var(--accent); }

.s-tabs { display: flex; gap: 7px; border-bottom: 1px solid var(--line); padding: 0 26px 10px; margin-bottom: 0; flex-wrap: wrap; width: 100%; max-width: 712px; }
.s-tab {
  position: relative; background: rgba(var(--accent-rgb), 0.04); border: 1px solid var(--line); border-radius: 10px;
  color: var(--text-dim); cursor: pointer; padding: 7px 14px; font-size: 13px;
  display: inline-flex; align-items: center; gap: 6px; transition: color .15s, border-color .15s;
}
.s-tab:hover { color: var(--text); border-color: rgba(var(--accent-rgb), 0.4); }
.s-tab.on { color: var(--accent); border-color: rgba(var(--accent-rgb), 0.45); background: rgba(var(--accent-rgb), 0.1); }
.s-tab.future { opacity: .62; }
.badge { margin-left: 0; letter-spacing: 1px; }

/* 唯一脉冲:缺钥匙时全局唯一的光点 */
.dot { width: 6px; height: 6px; border-radius: 50%; }
.dot.amber { background: var(--warn); box-shadow: 0 0 8px var(--warn); animation: led 2.4s ease-in-out infinite; }
@keyframes led { 0%, 100% { opacity: 1; } 50% { opacity: .3; } }

.s-body { max-width: 640px; }
/* flex-wrap:控件(尤其长英文段选)放不下时整组折到次行,而非压缩裁字或溢出 */
.row { display: flex; flex-wrap: wrap; align-items: center; justify-content: space-between; gap: 10px 14px; padding: 13px 0; border-bottom: 1px solid var(--line); font-size: 13.5px; }
.label { color: var(--text); flex: 0 0 auto; }

/* flex: none —— 段选不被行压缩(否则 overflow:hidden 会裁掉按钮文字);宁可整组换行 */
.seg { display: inline-flex; flex: none; max-width: 100%; border: 1px solid var(--line); border-radius: 10px; overflow: hidden; }
.seg button { background: none; border: none; color: var(--text-dim); cursor: pointer; padding: 6px 13px; font-size: 12.5px; }
.seg button + button { border-left: 1px solid var(--line); }
.seg button.on { color: var(--accent); background: rgba(var(--accent-rgb), 0.12); }

.s-input {
  background: var(--surface-deep); border: 1px solid var(--line); border-radius: 10px;
  padding: 7px 11px; color: var(--text); font-size: 13px; outline: none; min-width: 220px;
}
.s-input:focus { border-color: var(--accent); }
/* 展开的下拉列表:原生 <option> 是 OS/Chromium 渲染,只有底色/字色吃 CSS(WebView2 认),
   给它上语义 token 至少和皮肤同色(不再白底默认);高亮行等 popup chrome 系统控、控不全,
   要像素级贴皮得换自定义下拉组件(见对话记档)。 */
.s-input option { background: var(--surface-deep); color: var(--text); }
/* 代理一行:开关 + 地址输入同排,输入框吃满 label 右侧空间;关掉时淡一档(状态可读,地址仍可改) */
.proxy-line { flex: 1; justify-content: flex-end; min-width: 0; }
.proxy-line .s-input { flex: 1; min-width: 0; max-width: 320px; }
/* 下载认证的新增行:三个输入 + 两个按钮,窄了自然折行(§6.6 英文更长要留 wrap) */
.cred-form { justify-content: flex-start; }
.cred-form .s-input { flex: 1 1 150px; min-width: 0; max-width: 260px; }
.s-input.off { opacity: .5; }

.key-state, .key-edit { display: inline-flex; align-items: center; gap: 10px; }
.ok-text { color: var(--ok); font-size: 12.5px; }
.amber-text { color: var(--warn); font-size: 12.5px; }
.link { background: none; border: none; color: var(--accent); cursor: pointer; font-size: 12.5px; padding: 0; }
.link:disabled { opacity: .4; cursor: default; }

.chip { border: 1px solid var(--line); border-radius: 9px; padding: 4px 11px; font-size: 12.5px; color: var(--text); }
.chip.on { border-color: rgba(var(--accent-rgb), 0.45); color: var(--accent); }
.chip.warn { border-color: rgba(var(--attn-rgb), 0.5); color: var(--attn); }
.section.dt-sec { margin-top: 20px; }
.chip.future { opacity: .62; color: var(--text-dim); }
/* 微信扫码:二维码必须暗码浅底才扫得出(功能要求,故底色写死白、不随皮肤——同 QR 打印惯例) */
.wx-qr { margin: 10px 0 4px; }
.wx-qr-img { width: 200px; height: 200px; background: #fff; border-radius: 10px; padding: 8px; box-sizing: border-box; }
.wx-qr-img :deep(svg) { width: 100%; height: 100%; display: block; }
.wx-risk { opacity: .7; font-size: 12px; }
/* 性格:textarea + 紧贴下方的小快捷 chip(复用音色 chip 薄玻璃质感) */
.persona-row { align-items: flex-start; }
.persona-field { display: flex; flex-direction: column; gap: 6px; flex: 1 1 340px; max-width: 440px; min-width: 220px; }
.persona-text { width: 100%; min-height: 4.4em; line-height: 1.55; resize: vertical; font-family: inherit; }
.persona-chips { display: flex; flex-wrap: wrap; gap: 5px; }
.chip.preset { cursor: pointer; background: rgba(var(--accent-rgb), 0.04); transition: border-color .15s, color .15s, background .15s; }
.chip.preset:hover { border-color: rgba(var(--accent-rgb), 0.45); }
.chip.preset.on { border-color: rgba(var(--accent-rgb), 0.55); color: var(--accent); background: rgba(var(--accent-rgb), 0.1); }
.chip.preset.mini { padding: 2px 8px; font-size: 11px; border-radius: 7px; }
.hint { font-size: 12px; color: var(--text-dim); line-height: 1.7; display: flex; align-items: center; gap: 10px; padding-top: 13px; }
/* 授权圈(§7.2):路径行等宽 + 截断;数据目录说明行弱化 */
.scope-path { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-family: ui-monospace, "SF Mono", monospace; font-size: 12px; }
.scope-note { font-size: 12px; color: var(--text-dim); }
.hint.err { color: var(--danger); }
.hint.warn { color: var(--warn); }
.hint.ok { color: var(--ok); }
/* 灵敏度行里的标定小入口:贴着「灵敏」,别撑行 */
.sens .calib-link { margin-left: 4px; white-space: nowrap; }
.s-input.bad { border-color: var(--danger); }

.teaser { padding: 26px 0; color: var(--text-dim); font-size: 13.5px; }
.teaser p { margin: 10px 0 0; }

.section { margin: 18px 0 9px; font-size: 11.5px; letter-spacing: 2px; color: var(--text-dim); }

/* 供应商卡片 */
.pcard { border: 1px solid var(--line); border-radius: 12px; padding: 12px 14px; margin-bottom: 10px; background: rgba(var(--accent-rgb), 0.03); }
.pcard.off { opacity: .55; }
.p-head { display: flex; align-items: center; gap: 10px; font-size: 13.5px; flex-wrap: wrap; }
.p-head b { color: var(--text); }
.proto { letter-spacing: 1px; border: 1px solid var(--line); border-radius: 6px; padding: 2px 6px; margin-left: 0; }
.p-head .ok-text, .p-head .amber-text { font-size: 12px; }
.p-actions { margin-left: auto; display: inline-flex; gap: 14px; }
.p-grid { display: grid; grid-template-columns: 52px minmax(0, 1fr); gap: 8px 12px; align-items: center; margin-top: 11px; font-size: 12.5px; }
.p-grid label { color: var(--text-dim); }
.p-grid .s-input { width: 100%; min-width: 0; }
/* 高级格里的 SkinSelect(档位/计价)填满单元格(root 默认 inline-block 不自动撑) */
.adv-grid .skinsel { width: 100%; }
/* 高级折叠:档位/价/窗口纠错。标签更长 → 加宽标签列;提示占满两列 */
.adv-toggle { margin-top: 10px; font-size: 12px; color: var(--text-dim); }
.adv-grid { grid-template-columns: 96px minmax(0, 1fr); margin-top: 8px; padding-top: 10px; border-top: 1px dashed var(--line); }
.adv-grid .adv-hint { grid-column: 1 / -1; margin: 2px 0 0; color: var(--text-dim); font-size: 11.5px; }
.s-mono-input { font-family: ui-monospace, "SF Mono", monospace; font-size: 12px; }
/* 模型下拉(combobox):自由文本输入 + 行尾 ▾;弹层本体在 ModelPickList.vue(已接入卡 / 草稿卡两处共用),
   锚在这个容器的左右缘。 */
.model-pick { position: relative; display: flex; align-items: center; min-width: 0; }
.model-pick .s-input { padding-right: 30px; }
.pick-btn {
  position: absolute; right: 6px; top: 50%; transform: translateY(-50%);
  width: 22px; height: 22px; padding: 0; border: none; border-radius: 6px;
  background: none; color: var(--text-dim); cursor: pointer; font-size: 11px; line-height: 22px;
}
.pick-btn:hover, .model-pick.open .pick-btn { color: var(--accent); background: rgba(var(--accent-rgb), 0.12); }
/* 「自己接一个大脑」草稿卡:标签比模板卡长一档(预设 / 协议),两个 SkinSelect 填满单元格 */
.custom-grid { grid-template-columns: 64px minmax(0, 1fr); }
.custom-grid .skinsel { width: 100%; }
/* 微信绑定行:绑定者 id(等宽截断)+ 行尾解绑 */
.s-mono-text { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-family: ui-monospace, "SF Mono", monospace; font-size: 12px; color: var(--text-dim); }
/* 全局应用公钥框:多行 PEM,占满宽、不可拽缩、整段可读(给用户复制到服务控制台) */
.pubkey-box { display: block; width: 100%; min-width: 0; resize: none; margin-top: 4px; line-height: 1.45; white-space: pre-wrap; word-break: break-all; color: var(--text-dim); }
.p-foot { display: flex; gap: 16px; margin-top: 11px; }
.danger { color: var(--danger); }
.dim { color: var(--text-dim); }
.add-card { width: 100%; padding: 10px; border: 1px dashed var(--line); border-radius: 12px; background: none; color: var(--text-dim); cursor: pointer; font-size: 12.5px; margin-bottom: 12px; }
.add-card:hover { color: var(--accent); border-color: var(--accent); }

/* —— 声音 tab —— */
.v-speaker { align-items: flex-start; }
.sp-list { display: flex; flex-wrap: wrap; gap: 8px; justify-content: flex-end; max-width: 420px; }
.sp-pair { display: inline-flex; align-items: center; gap: 2px; } /* chip+✕ 永远同行换行 */
.chip.sp.custom { border-style: dashed; opacity: 0.75; }
.chip.sp.custom:hover { opacity: 1; }
.chip-del { border: none; background: transparent; color: var(--text-dim); cursor: pointer; font-size: 11px; padding: 0 2px; align-self: center; opacity: 0.55; }
.chip-del:hover { color: var(--danger); opacity: 1; }
.chip-del.armed { color: var(--danger); opacity: 1; font-weight: 600; }
/* 家人页:行内成员卡 + 渠道对话指认(全语义 token,换肤跟随)。
   .row 默认 space-between 会把中间按钮拉开 → 改左对齐,删除钮 margin-left:auto 靠右 */
.fam-row { justify-content: flex-start; gap: 10px; }
.fam-row .chip-del { margin-left: auto; }
.fam-you { color: var(--text-dim); font-size: 11px; }
.fam-chat-label { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; color: var(--text-dim); font-size: 12.5px; }
.fam-select { flex: 0 0 auto; width: 180px; margin-left: auto; }
/* 声纹注册:进度提示(录音中辉光跟随 accent)/ 已录徽标(ok token)/ 忘掉声音(dim→danger) */
.fam-enroll-hint { color: var(--accent); font-size: 11.5px; white-space: nowrap; }
.fam-enrolled { color: var(--ok); font-size: 11px; white-space: nowrap; }
.fam-enrolled::before { content: '✓ '; }
.fam-forget { color: var(--text-dim); }
.fam-forget:hover { color: var(--danger); }
.hidden-file { display: none; }
.clone-edit { align-items: flex-start; }
.clone-form { display: flex; flex-direction: column; gap: 6px; max-width: 420px; width: 100%; }
.clone-input, .clone-text { background: var(--surface-deep); border: 1px solid var(--line); border-radius: 8px; color: inherit; font: inherit; padding: 6px 9px; }
.clone-text { resize: vertical; min-height: 56px; }
.clone-hint { font-size: 11.5px; opacity: 0.6; margin: 0; }
/* 参考音体检提示:警示色但不是错误(保存照旧能点)——语义 token,换肤跟随(§6.7) */
.clone-warn { font-size: 12px; color: var(--warn); margin: 0; }
.clone-actions { display: flex; gap: 8px; justify-content: flex-end; }
/* 与音色 chip 同一套薄玻璃质感(否则 <button> 默认灰底会很出戏);保存走 .on 青色描边 */
.clone-actions .chip { cursor: pointer; background: rgba(var(--accent-rgb), 0.04); transition: border-color .15s, color .15s, background .15s; }
.clone-actions .chip:hover:not(:disabled) { border-color: rgba(var(--accent-rgb), 0.45); }
.clone-actions .chip.on { border-color: rgba(var(--accent-rgb), 0.55); color: var(--accent); background: rgba(var(--accent-rgb), 0.1); }
.clone-actions .chip:disabled { opacity: 0.4; cursor: default; }
.clone-err { color: var(--danger); font-size: 12.5px; }
.chip.sp { cursor: pointer; background: rgba(var(--accent-rgb), 0.04); transition: border-color .15s, color .15s; }
.chip.sp:hover { border-color: rgba(var(--accent-rgb), 0.45); }
.chip.sp.on { border-color: rgba(var(--accent-rgb), 0.55); color: var(--accent); background: rgba(var(--accent-rgb), 0.1); }
.chip.sp.busy { animation: led 1.2s ease-in-out infinite; }
.v-vol { width: 220px; accent-color: var(--accent); }
.sens { display: inline-flex; align-items: center; gap: 10px; }
.sens small { color: var(--text-dim); font-size: 12px; white-space: nowrap; }
.sens .v-vol { width: 150px; }
.v-mic { width: 280px; max-width: 280px; }
.comp { letter-spacing: 1px; color: var(--text-dim); }
.comp.ok { color: var(--ok); }

/* 唤醒状态:纯文本(刻意不做成 chip/输入框样,免和下面「唤醒词」框混淆) */
.wake-cur { color: var(--text-dim); font-size: 12.5px; }

/* 录音标定:进行中文本走辉光脉冲(复用 led),结果走成功绿/中性灰 */
.calib-live { color: var(--accent); font-size: 12.5px; }
.calib-live.pulse { animation: led 1.2s ease-in-out infinite; }
.calib-done { color: var(--text-dim); font-size: 12.5px; }
.calib-done.ok { color: var(--ok); }

/* 现场录音中:辉光脉冲提示「在听」(复用 led 动画) */
.chip.sp.custom.recording { color: var(--attn); border-color: rgba(var(--attn-rgb), 0.6); animation: led 1.2s ease-in-out infinite; }

/* 夜间模式自动时段:两个原生 time 输入 + 分隔号,横排紧凑(别被 .s-input 撑满整行) */
.v-night { display: inline-flex; align-items: center; gap: 8px; }
.v-time { width: auto; min-width: 0; }
.v-time-sep { color: var(--text-dim); }
</style>
