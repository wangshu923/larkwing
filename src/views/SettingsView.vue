<script setup lang="ts">
// 设置台:tab 导航 + 意图措辞(设计稿见会话纪要:常规|大脑|声音·暗|家人|远程·暗|系统)。
// 暗 tab 可点、进 teaser 页 —— 能点的必有反应(铁律3),绝不放灰掉的死控件。
import { computed, onMounted, onUnmounted, reactive, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import { api, appVersion, emitWakeChanged, isTauri, openExternal, setFloatVisible, type ChannelChat, type FamilyMember, type ModelMeta, type ModelOverride, type ModelTier, type ProviderView, type RemoteChannelView, type VoiceStatus } from '../lib/backend'
import { applyLocale } from '../i18n'
import { useChat } from '../composables/useChat'
import { hydrateUser, useSettings } from '../composables/useSettings'
import { useUpdater } from '../composables/useUpdater'
import { useToast } from '../composables/useToast'
import { refreshAudioMode } from '../composables/useAudioGraph'
import { useWakeCalib } from '../composables/useWakeCalib'
import { useVoice, onEnrollDone } from '../composables/useVoice'
import { audioFileToWavBase64 } from '../composables/useAudioDecode'
import SkinSelect from '../components/SkinSelect.vue'

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

// —— 声音 tab(PLAN §11):第一层 音色+自动朗读;高级 语速/耐心/音量/麦克风/组件状态 ——
const voiceInfo = ref<VoiceStatus | null>(null)
watch(tab, (v) => {
  if (v === 'voice' && !voiceInfo.value) void loadVoice()
  if (v === 'voice') void loadWebMics() // 每次进声音页刷新浏览器麦列表(设备可热插拔)
  if (v === 'system') {
    void loadAutostart()
    void loadDataLocation()
    if (!appVer.value) void appVersion().then((x) => (appVer.value = x))
  }
})
async function loadVoice() {
  if (!isTauri()) {
    // 浏览器预览:与 core 音色目录同构的假数据(纯看交互)
    voiceInfo.value = {
      asrReady: false,
      vadReady: false,
      kwsReady: false,
      wakeRunning: false,
      keywords: ['示例唤醒词'], // 预览占位(非默认值副本——唤醒词=名字派生,单源在后端 voice/mod.rs::wake_keywords,§4.11)
      wakeFallback: false,
      devices: ['MacBook 麦克风(预览)', 'USB 会议麦(预览)'],
      speakers: [
        { id: 'zh-CN-XiaoxiaoNeural', name: '晓晓 · 温柔' },
        { id: 'zh-CN-XiaoyiNeural', name: '晓伊 · 可爱' },
        { id: 'zh-CN-YunxiNeural', name: '云希 · 少年' },
        { id: 'zh-CN-YunjianNeural', name: '云健 · 沉稳' },
        { id: 'clone:demo', name: '我的声音(示例)', isClone: true, builtin: false },
      ],
      defaultSpeaker: 'zh-CN-XiaoxiaoNeural', // 预览假数据(§6.6 豁免);真机时来自后端 VoiceStatus.defaultSpeaker
    }
  } else {
    try {
      voiceInfo.value = await api.voiceStatus()
    } catch (e) {
      console.error('读取语音状态失败', e)
    }
  }
}
// 喊名字唤醒(C 期):开关 = voice_wake_set 一体化入口(写库 + 起停;首次开会
// 下 KWS 模型 + 预合成应答音,按钮转菊花)。wakeRunning 是事实,失败自然回弹。
const wakeBusy = ref(false)
// 开启失败的可见出口(铁律 §3.5:能点的必有反应、出错有友好退路)。原先失败只
// console.error → Win 上看不到任何反馈,开关只闪一下就回弹 = 用户眼里"打不开"。
const wakeError = ref('')
async function toggleWake() {
  if (wakeBusy.value) return
  const target = !(voiceInfo.value?.wakeRunning ?? false)
  wakeError.value = ''
  // 唤醒词 = 名字派生(后端 wake_keywords 单源):派生不出会自动回落默认词,
  // 永远编得出 → 原「全非中文拦在最前」的预检不再需要,喊不了在下面 wakeFallback 提示。
  if (!isTauri()) {
    if (voiceInfo.value) voiceInfo.value.wakeRunning = target // 预览:纯看交互
    return
  }
  wakeBusy.value = true
  try {
    const s = await api.voiceWakeSet(target)
    voiceInfo.value = s
    emitWakeChanged(s.wakeRunning, s.keywords) // 实时同步给悬浮窗待机栏
  } catch (e) {
    console.error('唤醒开关失败', e) // message 进日志,给用户的是友好兜底文案
    wakeError.value = t('settings.voice.wakeFailed')
  } finally {
    wakeBusy.value = false
  }
}
// 名字只有一个音(单派生词且单字)→ 提示喊起来可能不灵(召回物理,标定也难救)
const wakeShortName = computed(() => {
  const k = voiceInfo.value?.keywords ?? []
  return !voiceInfo.value?.wakeFallback && k.length === 1 && (k[0]?.length ?? 0) === 1
})

// 影响"正在监听的唤醒循环"的设置(阈值/名字→唤醒词/麦克风/耐心)改完 → 自动 off→on 重启,
// 让新值立即生效,不让用户手动重启(这不是服务器,改一下就重启体验差)。重启很轻
// (模型已缓存,只重建 spotter/VAD/采集,无声、亚秒级);唤醒没开就只刷状态,下次开自然用上。
// ⚠️ 唤醒开没开必须现问 core,不吃本页缓存:voiceInfo 是进声音 tab 才懒加载的,改名在
// 别的 tab 时它还是 null → 旧版在这里静默 return,唤醒词根本没换(2026-07-11 真机实锤
// 「改名后喊新名字不应」)。
async function restartWakeIfRunning() {
  if (!isTauri()) return
  try {
    const cur = await api.voiceStatus()
    if (!cur.wakeRunning) {
      // 没在听:词已随名字派生落库,刷一下状态让「听哪个词」与悬浮窗跟上即可
      voiceInfo.value = cur
      emitWakeChanged(cur.wakeRunning, cur.keywords)
      return
    }
    await api.voiceWakeSet(false)
    const s = await api.voiceWakeSet(true)
    voiceInfo.value = s
    emitWakeChanged(s.wakeRunning, s.keywords) // 实时同步给悬浮窗待机栏
  } catch (e) {
    console.error('设置生效(重启唤醒)失败', e)
    wakeError.value = t('settings.voice.wakeFailed')
  }
}

// 唤醒灵敏度:滑块松手(@change)保存 + 重启生效;@input 只更新值,拖动途中不反复重启。
async function saveSensitivity(v: number) {
  await settings.set('voice.wake.sensitivity', String(v))
  await restartWakeIfRunning()
}

// 录音标定:录几遍唤醒词 → 一次扫描定灵敏度(必要时连触发拼写)。落定后同步滑块+语音状态
// (core 已直接写库并按需重启唤醒,这里只把前端反应态追平)。
const { state: calib, start: startCalib, cancel: cancelCalib } = useWakeCalib()
watch(
  () => calib.phase,
  async (p) => {
    if (p === 'done' && calib.result?.ok) {
      await settings.set('voice.wake.sensitivity', String(calib.result.sensitivity))
      if (isTauri()) voiceInfo.value = await api.voiceStatus().catch(() => voiceInfo.value)
    }
  },
)
const calibStepLabel = computed(() => {
  if (calib.phase === 'preparing') return t('settings.voice.calibPreparing')
  if (calib.phase === 'computing') return t('settings.voice.calibComputing')
  if (calib.total > 0 && calib.step >= calib.total) return t('settings.voice.calibAmbient')
  return t('settings.voice.calibRound', { n: calib.step, total: Math.max(calib.total - 1, 0) })
})
const calibVerdict = computed(() =>
  calib.result ? t(`settings.voice.calibVerdict_${calib.result.verdict}`, { sens: calib.result.sensitivity }) : '',
)

// 影响唤醒应答音的设置(音色/语速/在线离线档):set 后让后端按新设置后台重建应答音银行
// (唤醒在跑就热替换,KWS 检测/麦克风不动;没开唤醒则 no-op)。问题1-B:换这些不再要重启。
function refreshAckPrompts() {
  if (isTauri()) void api.voiceRefreshPrompts()
}

// 语速/耐心 seg:语速是 TTS,回复下句即用;但唤醒应答音是预合成的,得让它后台重建(B)。
// 耐心改 VAD 静音容忍,捕获参数烤在唤醒循环里,只能重启唤醒才换。
function onVoiceSeg(key: string, v: string) {
  void settings.set(key, v)
  if (key === 'voice.patience') void restartWakeIfRunning()
  else if (key === 'voice.rate') refreshAckPrompts()
}

// 在线/离线 TTS 档:档变了应答音引擎也变(edge mp3 ↔ melo wav)→ 后台重建应答音(B)。
function onTtsBackend(b: string) {
  void settings.set('voice.tts_backend', b)
  refreshAckPrompts()
}

// 响度均衡 / 夜间模式(客户端 Web Audio,见 useAudioGraph):改设置即持久化 + 让已挂的处理链现读现调档
// (常态即时生效;总开关关→旁路,某机器彻底恢复需重启)。app 级 audio.* 键,§6.8 两边各加一行。
function onLeveling(v: string) {
  void settings.set('audio.leveling', v)
  refreshAudioMode()
}
function onNightMode(v: string) {
  void settings.set('audio.night_mode', v)
  refreshAudioMode()
}
function onNightTime(key: string, ev: Event) {
  const v = (ev.target as HTMLInputElement).value // 原生 time 输入回 "HH:MM"
  if (!v) return
  void settings.set(key, v)
  refreshAudioMode()
}

// 识别模型档(sense-voice 默认 / firered-ctc 更准):写库 → 开着唤醒就重启循环让新模型生效
// (同 sensitivity;新模型首次会用时下载)→ 刷状态行(组件就绪反映的是当前选中的模型)。
async function onAsrModel(v: string) {
  await settings.set('voice.asr.model', v)
  await restartWakeIfRunning()
  if (isTauri()) voiceInfo.value = await api.voiceStatus().catch(() => voiceInfo.value)
}
/** 识别模型下拉项(SkinSelect;两档,文案随 locale)。 */
const asrOpts = computed(() => [
  { value: 'sense-voice', label: t('settings.voice.asr_standard') },
  { value: 'firered-ctc', label: t('settings.voice.asr_accurate') },
])
/** 麦克风下拉项(默认 + 设备列表)。 */
// 麦克风双列表(采集双源,2026-07-06 收尾):browser 源(默认)= enumerateDevices 的
// deviceId(存 voice.input_device_web);cpal 源 = 人类可读设备名(存 voice.input_device)。
// 两套命名空间分键,切源各回各的选择。浏览器设备的 label 要麦克风权限到手后才有
// (桥起过一次即有),没有就编号兜底。
const captureSource = computed(() => settings.get('voice.capture.source') || 'browser')
// 回声消除开关 = 采集源的用户语言(§7.5 正式位,2026-07-10 从试验块挪来;试验块已删):
// 开 = browser 采集(getUserMedia AEC3,消掉自己放的电影/说话声);关 = cpal 原始采集。
// 切换即写 + 重启唤醒换管;browser 起不来时 useMicBridge 自愈回落 cpal + toast,
// 这颗开关是显式的一键回退。NS/AGC 不暴露(实验定案:NS 双开啃双讲人声,锁死在代码)。
async function onEchoCancel(v: string) {
  await settings.set('voice.capture.source', v === '1' ? 'browser' : 'cpal')
  await restartWakeIfRunning()
  if (v === '1') void loadWebMics() // 切回 browser 源顺手刷新设备列表
}
const webMics = ref<{ value: string; label: string }[]>([])
async function loadWebMics() {
  try {
    const devs = await navigator.mediaDevices.enumerateDevices()
    webMics.value = devs
      .filter((d) => d.kind === 'audioinput' && d.deviceId && d.deviceId !== 'default')
      .map((d, i) => ({
        value: d.deviceId,
        label: d.label || t('settings.voice.micUnnamed', { n: i + 1 }),
      }))
  } catch {
    webMics.value = []
  }
}
const micOpts = computed(() =>
  captureSource.value === 'browser'
    ? [{ value: '', label: t('settings.voice.micDefault') }, ...webMics.value]
    : [
        { value: '', label: t('settings.voice.micDefault') },
        ...(voiceInfo.value?.devices ?? []).map((d) => ({ value: d, label: d })),
      ],
)
const micValue = computed(() =>
  captureSource.value === 'browser'
    ? settings.get('voice.input_device_web')
    : settings.get('voice.input_device'),
)

// 自定义音色:选本地音频文件 → 前端解码/重采样成 16k → 后端转写出草稿 → 起名/改稿 → 保存。
const cloneFile = ref<HTMLInputElement | null>(null)
const cloneBusy = ref(false)
const cloneRecording = ref(false) // 现场录音中(命令 await 到 VAD 静音自动收尾)
const cloneErr = ref('')
const cloneDraft = ref<{ cloneId: string; name: string; transcript: string } | null>(null)
function pickCustomVoice() {
  cloneErr.value = ''
  cloneFile.value?.click()
}
// 现场录一段参考音:走麦克风(后端 VAD 自动起止、截到上限、转写出草稿),与导入同样进 cloneDraft。
// 录音期间后端已挂起唤醒监听;命令 await 到录完+转写才返回。名字留空(没文件名)→ 占位提示填。
async function recordClone() {
  if (cloneBusy.value) return
  cloneErr.value = ''
  cloneDraft.value = null
  cloneBusy.value = true
  cloneRecording.value = true
  try {
    const d = await api.voiceCloneRecord()
    cloneDraft.value = { cloneId: d.cloneId, name: '', transcript: d.transcript }
  } catch (e) {
    console.error('录音克隆失败', e)
    cloneErr.value = t('settings.voice.cloneRecordFailed')
  } finally {
    cloneRecording.value = false
    cloneBusy.value = false
  }
}
async function onCustomFile(ev: Event) {
  const input = ev.target as HTMLInputElement
  const f = input.files?.[0]
  input.value = '' // 允许重选同一文件
  if (!f) return
  cloneBusy.value = true
  cloneErr.value = ''
  cloneDraft.value = null
  try {
    const { base64 } = await audioFileToWavBase64(f)
    const d = await api.voiceCloneImport(base64)
    cloneDraft.value = { cloneId: d.cloneId, name: f.name.replace(/\.[^.]+$/, ''), transcript: d.transcript }
  } catch (e) {
    console.error('导入音色失败', e)
    cloneErr.value = t('settings.voice.cloneImportFailed')
  } finally {
    cloneBusy.value = false
  }
}
async function saveClone() {
  const d = cloneDraft.value
  if (!d || !d.name.trim() || !d.transcript.trim()) return
  cloneBusy.value = true
  try {
    await api.voiceCloneSave(d.cloneId, d.name.trim(), d.transcript.trim())
    cloneDraft.value = null
    await loadVoice()
  } catch (e) {
    console.error('保存音色失败', e)
    cloneErr.value = t('settings.voice.cloneSaveFailed')
  } finally {
    cloneBusy.value = false
  }
}
function cancelClone() {
  cloneDraft.value = null
  cloneErr.value = ''
}
// 删除二次确认走 arm 模式(同 MemoryView):Tauri WebView 里 window.confirm 不可靠(返回 falsy)。
const cloneArm = ref('')
async function removeClone(speakerId: string) {
  if (cloneArm.value !== speakerId) {
    cloneArm.value = speakerId // 第一下:亮起「删?」,再点一下才真删
    return
  }
  cloneArm.value = ''
  try {
    await api.deleteVoiceClone(speakerId.replace(/^clone:/, ''))
    await loadVoice()
  } catch (e) {
    console.error('删除音色失败', e)
  }
}

const previewing = ref('')
let previewAudio: HTMLAudioElement | null = null
// 试听合成兜底超时:克隆走本地 ZipVoice(CPU 慢、冷启十几秒)给足 45s;在线/离线 TTS 12s。
// 与 useSpeech 的 SYNTH_TIMEOUT_* 同源意图(那边流式念话、这里设置页试听),值保持一致。
const PREVIEW_TIMEOUT_MS = 12000
const PREVIEW_TIMEOUT_CLONE_MS = 45000
async function previewSpeaker(id: string) {
  settings.set('voice.speaker', id)
  refreshAckPrompts() // 换音色 → 后台重建唤醒应答音(问题1-B,不重启唤醒)
  if (!isTauri()) return
  // 换试听:先停掉上一条(后点覆盖先点;原来在放就 return 会吞掉本次点击)。
  if (previewAudio) {
    previewAudio.pause()
    previewAudio = null
  }
  previewing.value = id
  // 失败/超时都给一句提示,别再静默(§3.5):合成报错 / 参考音坏 / 播不出来时,用户先前只看到
  // chip 一直转或什么都没有;现在明确告知,后端 voice_preview 也会把真实错误落 logs/larkwing.log。
  let settled = false
  const finish = (failed: boolean, e?: unknown) => {
    if (settled) return
    settled = true
    if (previewing.value === id) previewing.value = ''
    if (failed) {
      console.error('试听失败', e)
      useToast().error(t('settings.voice.previewFailed'))
    }
  }
  const ms = id.startsWith('clone:') ? PREVIEW_TIMEOUT_CLONE_MS : PREVIEW_TIMEOUT_MS
  let timer: ReturnType<typeof setTimeout> | undefined
  const timeout = new Promise<never>((_, reject) => {
    timer = setTimeout(() => reject(new Error('preview-timeout')), ms)
  })
  const req = api.voicePreview(id, t('settings.voice.previewLine'))
  req.catch(() => {}) // 超时先行时,别让它稍后 reject 冒成未处理拒绝
  try {
    const url = await Promise.race([req, timeout])
    clearTimeout(timer)
    if (previewing.value !== id) return // 合成期间又点了别的:只认最后那次
    const a = new Audio(url)
    previewAudio = a
    a.addEventListener('ended', () => {
      if (previewAudio === a) previewAudio = null
      finish(false)
    })
    a.addEventListener('error', () => {
      if (previewAudio === a) previewAudio = null
      finish(true, new Error('audio-error')) // 合成出了 URL 却播不出来(空音频/格式)也算失败
    })
    void a.play().catch((e) => finish(true, e))
  } catch (e) {
    clearTimeout(timer)
    finish(true, e)
  }
}
function setMic(v: string) {
  if (captureSource.value === 'browser') {
    // 浏览器采集:换麦由 useMicBridge 热重启(停旧流起新流),core 推流管不动 → 不用重启唤醒
    void settings.set('voice.input_device_web', v)
    return
  }
  void settings.set('voice.input_device', v)
  void restartWakeIfRunning() // cpal:换麦立即生效,运行中的唤醒重开采集用新设备
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
// 主动关怀总开关(PLAN ★主动关怀里程碑):关 = 悬浮窗不投关怀候选、in-chat 场景续接 chips 也收起。
const careEnabled = computed(() => settings.get('care.enabled') !== '0')
function toggleCare() {
  settings.set('care.enabled', careEnabled.value ? '0' : '1')
}

// 数据目录「搬家」(datadir):选目录 → 预检 → 内联确认 → 执行(HUD 进度,完后自动重启)。
const dataRoot = ref('')
const oldDataRoot = ref<string | null>(null)
const relocateBusy = ref(false)
const relocateError = ref('')
const pendingMove = ref<{ picked: string; newRoot: string; needBytes: number } | null>(null)
const gb = (n: number) => (n / 1073741824).toFixed(1)
async function loadDataLocation() {
  if (!isTauri()) return
  try {
    const loc = await api.dataLocation()
    dataRoot.value = loc.root
    oldDataRoot.value = loc.oldRoot
  } catch (e) {
    console.error('读取数据位置失败', e)
  }
}
function revealData() {
  if (isTauri()) void api.revealDataDir()
}
async function relocate() {
  if (relocateBusy.value || !isTauri()) return
  relocateError.value = ''
  pendingMove.value = null
  const picked = await api.pickDataFolder()
  if (!picked) return
  try {
    const check = await api.relocatePrecheck(picked)
    if (!check.ok) {
      relocateError.value = t(`settings.system.dataErr.${check.reason ?? 'failed'}`)
      return
    }
    pendingMove.value = { picked, newRoot: check.newRoot ?? '', needBytes: check.needBytes }
  } catch (e) {
    console.error('搬家预检失败', e)
    relocateError.value = t('settings.system.relocateFailed')
  }
}
async function confirmRelocate() {
  const pm = pendingMove.value
  if (!pm) return
  relocateBusy.value = true
  try {
    await api.relocateData(pm.picked) // 成功 = 翻指针后自动重启,页面随之刷新,不会走到这下面
  } catch (e) {
    relocateBusy.value = false
    pendingMove.value = null
    console.error('搬家失败', e)
    relocateError.value = t('settings.system.relocateFailed')
  }
}
function cancelRelocate() {
  pendingMove.value = null
}
// 一键备份:选目录 → 导出 larkwing-backup-<时间戳>.zip(DB 快照 + 克隆音色)。不重启。
const backupBusy = ref(false)
const backupMsg = ref('')
const backupErr = ref(false)
async function backupNow() {
  if (backupBusy.value || relocateBusy.value || !isTauri()) return
  backupMsg.value = ''
  backupErr.value = false
  const dest = await api.pickDataFolder()
  if (!dest) return
  backupBusy.value = true
  try {
    const zip = await api.backupData(dest)
    backupMsg.value = t('settings.system.backupDone', { path: zip })
  } catch (e) {
    console.error('备份失败', e)
    backupErr.value = true
    backupMsg.value = t('settings.system.backupFailed')
  } finally {
    backupBusy.value = false
  }
}
// 从备份恢复:选 zip → 预检(结构/魔数/迁移版本)→ 内联确认 → 负载暂存 + 自动重启,
// 下次启动开库前落位(现库留 pre-restore 保险副本)。backup 的另一半。
const restoreBusy = ref(false)
const restoreError = ref('')
const pendingRestore = ref<{ zip: string; dbBytes: number; clones: number } | null>(null)
const mb = (n: number) => (Math.max(n, 104858) / 1048576).toFixed(1)
async function restorePick() {
  if (restoreBusy.value || relocateBusy.value || backupBusy.value || !isTauri()) return
  restoreError.value = ''
  pendingRestore.value = null
  const zip = await api.pickBackupFile()
  if (!zip) return
  try {
    const check = await api.restorePrecheck(zip)
    if (!check.ok) {
      restoreError.value = t(`settings.system.restoreErr.${check.reason ?? 'not_backup'}`, { name: petName.value })
      return
    }
    pendingRestore.value = { zip, dbBytes: check.dbBytes, clones: check.clones }
  } catch (e) {
    console.error('恢复预检失败', e)
    restoreError.value = t('settings.system.restoreFailed')
  }
}
async function confirmRestore() {
  const pr = pendingRestore.value
  if (!pr) return
  restoreBusy.value = true
  try {
    await api.restoreData(pr.zip) // 成功 = 暂存后自动重启落位,不会走到这下面
  } catch (e) {
    restoreBusy.value = false
    pendingRestore.value = null
    console.error('恢复失败', e)
    restoreError.value = t('settings.system.restoreFailed')
  }
}
function cancelRestore() {
  pendingRestore.value = null
}
async function cleanupOld() {
  try {
    await api.cleanupOldData()
  } catch (e) {
    console.error('清理旧数据失败', e)
  } finally {
    oldDataRoot.value = null
  }
}
async function keepOld() {
  try {
    await api.keepOldData()
  } catch (e) {
    console.error(e)
  } finally {
    oldDataRoot.value = null
  }
}

// 天气服务(PLAN 天气块):默认免 key Open-Meteo;接和风走 JWT —— 复制全局应用公钥到和风控制台,
// 把项目 ID / 凭据 ID / API Host 三件套填回来。后端三件套齐 + 全局私钥已生成即切和风源。
const appPublicKey = ref('')
const pubKeyCopied = ref(false)
onMounted(async () => {
  // 进设置即 ensure(幂等):服务页一直有公钥可复制。非 Tauri(纯浏览器预览)拿不到后端,留空。
  if (isTauri()) appPublicKey.value = await api.ensureAppKeypair()
})
function copyPublicKey() {
  const text = appPublicKey.value
  if (!text || !navigator.clipboard?.writeText) return
  navigator.clipboard.writeText(text).then(() => {
    pubKeyCopied.value = true
    window.setTimeout(() => (pubKeyCopied.value = false), 1500)
  })
}
// 三件套都是非秘密标识符,直存(后端校验 host 要 http(s));齐备才视作"已接和风"。
const weatherConfigured = computed(
  () =>
    !!settings.get('weather.qweather.host') &&
    !!settings.get('weather.qweather.project_id') &&
    !!settings.get('weather.qweather.credential_id'),
)
function setQWeather(key: string, ev: Event) {
  settings.set(key, (ev.target as HTMLInputElement).value.trim())
}
// 全局代理:开关 net.proxy_enabled 控总闸,地址 net.proxy 始终保留(默认已填,免空)。
// 下载/LLM 现读即生效,无需重启。关掉只停用、不丢地址(铁律:地址可保存下来)。
const proxyEnabled = computed(() => settings.get('net.proxy_enabled') === '1')
function toggleProxy() {
  const target = !proxyEnabled.value
  // 开启时把当前地址(可能是默认值)一并落库,确保后端选路与界面显示一致。
  if (target) {
    const addr = settings.get('net.proxy').trim()
    if (addr) void settings.set('net.proxy', addr)
  }
  void settings.set('net.proxy_enabled', target ? '1' : '0')
}
function setProxy(ev: Event) {
  settings.set('net.proxy', (ev.target as HTMLInputElement).value.trim())
}
function openQWeatherSite() {
  void openExternal('https://dev.qweather.com/')
}

// —— 远程渠道(Telegram/钉钉 bot,PLAN 远程渠道):自包含 tab(同 providers,不走 useSettings) ——
// 凭证写得进读不回(同 provider key 的「空串视同不改」);状态读 remote_status,保存后 reload_channels。
const remoteChannels = ref<RemoteChannelView[]>([])
const tgToken = ref('') // 本地写入框,提交即清空(凭证永不回显)
const dtKey = ref('')
const dtSecret = ref('')
const fallback = (id: string): RemoteChannelView => ({
  id, enabled: false, configured: false, allowed_chats: '', running: false, last_error: null,
})
const tg = computed<RemoteChannelView>(
  () => remoteChannels.value.find((c) => c.id === 'telegram') ?? fallback('telegram'),
)
const dt = computed<RemoteChannelView>(
  () => remoteChannels.value.find((c) => c.id === 'dingtalk') ?? fallback('dingtalk'),
)
const wx = computed<RemoteChannelView>(
  () => remoteChannels.value.find((c) => c.id === 'weixin') ?? fallback('weixin'),
)
// 微信扫码登录状态机(QR-based,区别于 TG/钉钉粘贴 token):起手拿二维码 → 轮询状态 → confirmed 落库。
const wxQrSvg = ref('') // 二维码 SVG(v-html 直接展示,免前端二维码依赖)
const wxQrUrl = ref('') // 备用链接(扫不了时点开)
const wxLoginStatus = ref('') // '' | wait | scaned | need_verifycode | verify_blocked | expired | confirmed | already | error
const wxVerifyCode = ref('') // 手机上显示的配对码(need_verifycode 时输入,下次轮询自动带上)
let wxQrcode = '' // 轮询标识(非响应式)
let wxBaseUrl: string | null = null // IDC 重定向后的轮询地址
let wxLoginSeq = 0 // 代次:重新扫码 / 切走 tab 时作废旧轮询循环
async function loadRemote() {
  if (!isTauri()) return
  remoteChannels.value = await api.remoteStatus().catch(() => [])
  wxAccounts.value = await api.weixinAccounts().catch(() => [])
}
// 微信多绑定(一人一 bot):绑定者 user_id 列表;空串 = 旧版迁移的无身份绑定
const wxAccounts = ref<string[]>([])
async function unbindWeixin(userId: string) {
  try {
    await api.weixinUnbind(userId)
    await api.reloadChannels()
    await loadRemote()
  } catch (e) {
    console.error('解绑微信失败', e)
    useToast().error(t('settings.remote.weixin.unbindFailed'))
  }
}
async function toggleRemote(id: string, on: boolean) {
  await api.setSetting(`remote.${id}.enabled`, on ? '1' : '0')
  await api.reloadChannels()
  await loadRemote()
}
/** 写凭证:空 = 不改(读不回);写完清空输入框,再重启渠道。 */
async function saveRemoteCred(key: string, val: string) {
  const v = val.trim()
  if (!v) return
  await api.setSetting(key, v)
  if (key.endsWith('.token')) tgToken.value = ''
  if (key.endsWith('.app_key')) dtKey.value = ''
  if (key.endsWith('.app_secret')) dtSecret.value = ''
  await api.reloadChannels()
  await loadRemote()
}
async function saveRemote(key: string, ev: Event) {
  await api.setSetting(key, (ev.target as HTMLInputElement).value.trim())
  await api.reloadChannels()
  await loadRemote()
}
function remoteStatusText(c: RemoteChannelView): string {
  if (c.last_error) return t('settings.remote.statusError')
  if (c.running) return t('settings.remote.statusOn')
  if (!c.enabled) return t('settings.remote.statusOff')
  if (!c.configured) return t('settings.remote.statusUnconfigured')
  return t('settings.remote.statusStarting')
}

/** 微信扫码登录起手:拿二维码并展示,开轮询循环。 */
async function startWeixinLogin() {
  const seq = ++wxLoginSeq // 作废上一次(重复点/重扫)
  wxVerifyCode.value = ''
  wxBaseUrl = null
  wxLoginStatus.value = 'wait'
  wxQrSvg.value = ''
  wxQrUrl.value = ''
  try {
    const s = await api.weixinLoginStart()
    if (seq !== wxLoginSeq) return
    wxQrcode = s.qrcode
    wxQrUrl.value = s.qr_url
    wxQrSvg.value = s.qr_svg
  } catch {
    wxLoginStatus.value = 'error'
    useToast().error(t('settings.remote.weixin.startFailed'))
    return
  }
  void pollWeixinLogin(seq)
}

/** 轮询扫码状态:每次 poll 命令本身会长挂到服务端事件;小憩防打爆。confirmed 即连(自动开渠道)。 */
async function pollWeixinLogin(seq: number) {
  const nap = (ms: number) => new Promise((r) => setTimeout(r, ms))
  while (seq === wxLoginSeq) {
    let r
    try {
      r = await api.weixinLoginPoll(wxQrcode, wxBaseUrl, wxVerifyCode.value || null)
    } catch {
      await nap(1500)
      continue
    }
    if (seq !== wxLoginSeq) return
    if (r.status === 'redirect') {
      wxBaseUrl = r.base_url // IDC 重定向:下次轮询换地址
      continue
    }
    wxLoginStatus.value = r.status
    if (r.status === 'confirmed' || r.status === 'already') {
      wxQrSvg.value = ''
      // 扫码即连(§3 强默认):自动开启渠道并刷新状态(token 已由 core 落库)
      await api.setSetting('remote.weixin.enabled', '1')
      await api.reloadChannels()
      await loadRemote()
      return
    }
    if (r.status === 'expired' || r.status === 'verify_blocked') {
      wxQrSvg.value = '' // 收起过期码,让用户重扫
      return
    }
    await nap(1200) // wait / scaned / need_verifycode:继续
  }
}
// 切到远程/家人 tab 时拉一次(切走不轮询)
watch(tab, (v) => {
  if (v === 'remote') void loadRemote()
  else {
    wxLoginSeq++ // 离开远程 tab:作废在跑的扫码轮询
    wxQrSvg.value = ''
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

// 标题里的名字跟「叫我什么」联动(ui.pet_name 空 = 默认名 pet.name);
// 与 MainLayout / FloatWindow 同一口径 —— 这是当前 agent 的名字,不是 app 名
const petName = computed(() => settings.get('ui.pet_name') || t('pet.name'))

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
  const vendor = p.protocol === 'anthropic_compat' ? 'ANTHROPIC' : 'OPENAI'
  return t('settings.brain.compat', { vendor })
}

// 「高级」:按模型纠正档位/价格/上下文窗口(空 = 用目录猜测)。按 provider 折叠,展开时懒取 meta。
const advOpen = reactive<Record<string, boolean>>({})
const modelMeta = reactive<Record<string, ModelMeta>>({}) // 键 = model id
// 浏览器预览没有后端:给个保守假 meta(纯目录未知态),让面板照样能渲染/点
const FAKE_META: ModelMeta = { guess: { tier: 'balanced', inUsdPerM: null, outUsdPerM: null, ctxWindowTokens: null, billing: 'cached' }, over: null }
async function fetchMeta(model: string) {
  if (!model) return
  modelMeta[model] = isTauri() ? await api.modelMeta(model) : { ...FAKE_META }
}
function toggleAdv(p: ProviderView) {
  advOpen[p.id] = !advOpen[p.id]
  if (advOpen[p.id] && !modelMeta[p.model]) fetchMeta(p.model)
}
// 占位提示:目录猜测值(null = 目录也不知道)
function autoHint(v: number | null): string {
  return t('settings.brain.auto', { v: v == null ? '—' : String(v) })
}
// 窗口以 K 为单位展示(存的是原始 token);占位猜测也折成 K
function guessWinHint(v: number | null): string {
  return t('settings.brain.auto', { v: v == null ? '—' : `${Math.round(v / 1000)}K` })
}
function ovWinK(over: ModelOverride | null): number | '' {
  return over?.ctxWindowTokens ? Math.round(over.ctxWindowTokens / 1000) : ''
}
function tierLabel(tier: ModelTier): string {
  return t(`settings.brain.tier_${tier}`)
}
/** 档位下拉项(SkinSelect):首项「自动(=目录猜测)」带猜测档标签,其余三档。 */
function tierOpts(meta: ModelMeta) {
  return [
    { value: '', label: t('settings.brain.tierAuto', { tier: tierLabel(meta.guess.tier) }) },
    { value: 'light', label: t('settings.brain.tier_light') },
    { value: 'balanced', label: t('settings.brain.tier_balanced') },
    { value: 'smart', label: t('settings.brain.tier_smart') },
  ]
}
/** 计价方式下拉项(SkinSelect)。 */
const billingOpts = computed(() => [
  { value: '', label: t('settings.brain.billingAuto') },
  { value: 'cached', label: t('settings.brain.billing_cached') },
  { value: 'uncached', label: t('settings.brain.billing_uncached') },
  { value: 'percall', label: t('settings.brain.billing_percall') },
])
/** 渠道对话「指认给谁」下拉项:还没指认 + 家人们。 */
const famOpts = computed(() => [
  { value: '', label: t('settings.family.unassigned') },
  ...family.value.map((m) => ({ value: String(m.id), label: m.name })),
])
// 改一格 → 合并进该模型的覆盖,空值删该格;后端空壳自动删整条
// 数字输入(上下文/价格)走 Event;两个下拉(档位/计价)走 SkinSelect 直接给值 → 共用核心。
async function saveOv(p: ProviderView, field: keyof ModelOverride, ev: Event) {
  await saveOvRaw(p, field, (ev.target as HTMLInputElement).value)
}
async function saveOvRaw(p: ProviderView, field: keyof ModelOverride, rawIn: string) {
  const meta = modelMeta[p.model]
  if (!meta) return
  const cur: ModelOverride = { model: p.model, ...(meta.over ?? {}) }
  const raw = rawIn.trim()
  if (field === 'tier' || field === 'billing') {
    if (raw) (cur as unknown as Record<string, unknown>)[field] = raw
    else delete cur[field]
  } else if (raw === '') {
    delete cur[field]
  } else if (field === 'ctxWindowTokens') {
    const k = parseInt(raw, 10) // 输入是 K,存原始 token
    if (Number.isFinite(k) && k > 0) cur.ctxWindowTokens = k * 1000
    else return
  } else {
    const n = parseFloat(raw)
    if (Number.isFinite(n) && n >= 0) (cur as unknown as Record<string, unknown>)[field] = n
    else return
  }
  if (isTauri()) {
    try {
      await api.setModelOverride(cur)
    } catch {
      useToast().error(t('toast.actionFailed'))
      return
    }
  }
  await fetchMeta(p.model) // 回读(后端可能把空壳删了)
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

// 家人页(渠道归人 = 多用户第一步,声纹后置):家人列表 CRUD + 手机对话指认给家人。
// 指认后 TA 在手机上说的「提醒我 / 我喜欢…」归 TA 自己(speaker_user 缝,记忆归人 §6)。
const family = ref<FamilyMember[]>([])
const chats = ref<ChannelChat[]>([])
const famError = ref(false)
const famEditing = ref(0) // 行内改名中的家人 id;0 = 没有
const famDraft = ref('')
const famNew = ref('')
const famArm = ref(0) // 删除二次确认中的家人 id(clone 删除同款手势)
async function loadFamily() {
  if (!isTauri()) {
    // 浏览器预览假数据(UI 摸手感;与后端 FamilyMember/ChannelChat 同构)。
    // 预览没有 boot 过桥 → 补上当前用户 id,让「(你)/防删自己」渲染与真机一致
    if (!settings.state.userId) hydrateUser(1, settings.state.userName)
    family.value = [
      { id: 1, name: settings.state.userName, skin_id: 'scifi', created_at: 0, last_active_at: 0, enrolled: false },
      { id: 2, name: '豆豆', skin_id: 'scifi', created_at: 0, last_active_at: 0, enrolled: false },
    ]
    chats.value = [
      { id: 1, channel: 'telegram', ext_id: '12345678', conv_id: 9, user_id: 2, label: 'Doudou' },
      { id: 2, channel: 'dingtalk', ext_id: 'cidXXXX', conv_id: 10, user_id: null, label: '妈妈' },
    ]
    return
  }
  famError.value = false
  try {
    const [f, c] = await Promise.all([api.listFamily(), api.listChannelChats()])
    family.value = f
    chats.value = c
  } catch {
    famError.value = true // 初载失败别装空(§6.6):错误态 + 重试
  }
}
function startFamRename(m: FamilyMember) {
  famDraft.value = m.name
  famEditing.value = m.id
}
async function saveFamRename(m: FamilyMember) {
  const name = famDraft.value.trim()
  famEditing.value = 0
  if (!name || name === m.name) return
  try {
    // 自己走既有 rename 链(同步顶栏「现在陪着」名);家人按 id 改
    if (m.id === settings.state.userId) await settings.rename(name)
    else await api.renameFamily(m.id, name)
    await loadFamily()
  } catch {
    useToast().error(t('toast.actionFailed'))
  }
}
async function addFam() {
  const name = famNew.value.trim()
  if (!name) return
  try {
    await api.addFamily(name)
    famNew.value = ''
    await loadFamily()
  } catch {
    useToast().error(t('toast.actionFailed'))
  }
}
async function removeFam(m: FamilyMember) {
  if (famArm.value !== m.id) {
    famArm.value = m.id
    return
  }
  famArm.value = 0
  try {
    await api.removeFamily(m.id)
    await loadFamily()
  } catch {
    useToast().error(t('toast.deleteFailed'))
  }
}
async function bindChat(c: ChannelChat, v: string) {
  const uid = v ? Number(v) : null
  try {
    await api.bindChannelChat(c.id, uid)
    c.user_id = uid
  } catch {
    useToast().error(t('toast.actionFailed'))
    await loadFamily() // 失败把 select 拉回真值
  }
}
function channelName(id: string): string {
  return id === 'telegram' ? 'Telegram' : id === 'dingtalk' ? t('settings.family.dingtalk') : id
}

// 声纹注册(多用户第二步):让旺财凭声音认出家人 → TA 说的话记忆/提醒归 TA(§渠道归人第二步)。
// 录 3 段取平均(core);进度/终态走 voice 的 enroll 事件。owner 也可录(便于与家人区分,§4.2)。
const voice = useVoice()
/** 某家人此刻是否正在录(preparing/recording)——决定卡片显进度还是显按钮。 */
function enrollBusy(id: number): boolean {
  const e = voice.state.enroll
  return e.userId === id && (e.stage === 'preparing' || e.stage === 'recording')
}
function enrollLabel(id: number): string {
  const e = voice.state.enroll
  if (e.userId !== id) return ''
  return e.stage === 'preparing'
    ? t('settings.family.enrollPreparing')
    : t('settings.family.enrollRecording', { n: e.done + 1, total: e.total })
}
function startEnrollFam(m: FamilyMember) {
  voice.startEnroll(m.id)
}
async function forgetVoice(m: FamilyMember) {
  try {
    await voice.unenroll(m.id)
    if (!isTauri()) m.enrolled = false
    else {
      await loadFamily()
      await restartWakeIfRunning() // 唤醒循环要重载声纹库(少一个候选)才生效
    }
    useToast().ok(t('settings.family.forgetDone'))
  } catch {
    useToast().error(t('toast.actionFailed'))
  }
}
// 注册终态:成功 = toast + 刷新「已录」+ 唤醒在跑就重启让新声纹生效;失败 = toast 请重试(§3.5)
onEnrollDone(async (userId, ok) => {
  if (!ok) {
    useToast().error(t('settings.family.enrollFailed'))
    return
  }
  useToast().ok(t('settings.family.enrollDone'))
  if (!isTauri()) {
    const m = family.value.find((x) => x.id === userId)
    if (m) m.enrolled = true // 预览:本地标已录看视觉
  } else {
    await loadFamily()
    await restartWakeIfRunning() // 首次注册后唤醒循环要重载声纹库才认得出
  }
})

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
            <input class="s-input s-mono-input" :value="p.model" @change="saveField(p, 'model', $event)" />
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
            <p class="adv-hint">{{ t('settings.brain.advHint') }}</p>
          </div>
        </div>

        <!-- 自己接一个大脑 -->
        <button v-if="!adding" class="add-card" @click="adding = true">{{ t('settings.brain.addCustom') }}</button>
        <div v-else class="pcard">
          <div class="p-head">
            <b>{{ t('settings.brain.addCustom') }}</b>
            <span class="seg">
              <button :class="{ on: custom.protocol === 'openai_compat' }" @click="custom.protocol = 'openai_compat'">{{ t('settings.brain.compat', { vendor: 'OpenAI' }) }}</button>
              <button :class="{ on: custom.protocol === 'anthropic_compat' }" @click="custom.protocol = 'anthropic_compat'">{{ t('settings.brain.compat', { vendor: 'Anthropic' }) }}</button>
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
              v-for="v in ['1', '0']"
              :key="v"
              :class="{ on: captureSource === (v === '1' ? 'browser' : 'cpal') }"
              @click="onEchoCancel(v)"
            >{{ t(v === '1' ? 'settings.audio.on' : 'settings.audio.off') }}</button>
          </span>
        </div>
        <p class="hint">{{ t('settings.voice.echoCancelHint') }}</p>
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
        <!-- 识别模型(2026-06 用户要求放出来选):默认 SenseVoice 快;Whisper 对小孩/口音更稳;
             FireRed 中文最准。模型用时下载;换档后开着唤醒会重启循环让新模型生效(同 sensitivity) -->
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
