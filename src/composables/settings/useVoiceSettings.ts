// 设置·声音:音色与试听 / 唤醒开关与标定 / 朗读与响度 / 识别档位 / 采集与回声消除 / 音色克隆。
// 从 SettingsView 抽出(2026-09-08);逻辑一行没改,只是搬了个家。

import { computed, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import { api, emitWakeChanged, isTauri, type VoiceStatus } from '../../lib/backend'
import { useToast } from '../useToast'
import { refreshAudioMode } from '../useAudioGraph'
import { useWakeCalib } from '../useWakeCalib'
import { useCaptureRoute } from '../useCaptureRoute'
import { audioFileToWavBase64 } from '../useAudioDecode'
import { useSettings } from '../useSettings'

export function useVoiceSettings() {
  const { t } = useI18n()
  const settings = useSettings()

  // —— 声音 tab(PLAN §11):第一层 音色+自动朗读;高级 语速/耐心/音量/麦克风/组件状态 ——
  const voiceInfo = ref<VoiceStatus | null>(null)
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

  // 识别模型档(四档,见 asrOpts):写库 → 开着唤醒就重启循环让新模型生效
  // (同 sensitivity;新模型首次会用时下载)→ 刷状态行(组件就绪反映的是当前选中的模型)。
  async function onAsrModel(v: string) {
    await settings.set('voice.asr.model', v)
    await restartWakeIfRunning()
    if (isTauri()) voiceInfo.value = await api.voiceStatus().catch(() => voiceInfo.value)
  }
  /** 识别模型下拉项(SkinSelect;四档 = 名字 + 一句话特点,文案随 locale;
   *  值与 Rust from_setting / set_setting 白名单同源,2026-08-28 扩 4 档)。 */
  const asrOpts = computed(() => [
    { value: 'sense-voice', label: t('settings.voice.asr_sense') },
    { value: 'firered-ctc', label: t('settings.voice.asr_firered') },
    { value: 'funasr-nano', label: t('settings.voice.asr_nano') },
    { value: 'paraformer', label: t('settings.voice.asr_paraformer') },
  ])
  /** 麦克风下拉项(默认 + 设备列表)。 */
  // 麦克风双列表(采集双源,2026-07-06 收尾):browser 源(默认)= enumerateDevices 的
  // deviceId(存 voice.input_device_web);cpal 源 = 人类可读设备名(存 voice.input_device)。
  // 两套命名空间分键,切源各回各的选择。浏览器设备的 label 要麦克风权限到手后才有
  // (桥起过一次即有),没有就编号兜底。
  const captureSource = computed(() => settings.get('voice.capture.source') || 'auto')
  const captureRoute = useCaptureRoute()
  /** auto 档解析后的生效源(core 按「输出是不是耳机」定;还没问到按 browser);
   *  显式档 = 偏好本身。麦选择器按它切列表。 */
  const effectiveCapture = computed(() =>
    captureSource.value === 'auto'
      ? captureRoute.state.effective || 'browser'
      : captureSource.value,
  )
  // 回声消除三态 = 采集源的用户语言(§7.5;2026-08-12 加「自动」并转默认):
  // 自动 = 按默认输出定(耳机 → 关:自播进不了麦零收益,mac 上开着还会被系统通话处理弄糊
  // 自家播放;扬声器 → 开);开 = browser 采集(getUserMedia AEC3);关 = cpal 原始采集。
  // 切换即写 + 重启唤醒换管;browser 起不来时 useMicBridge 自愈回落 cpal + toast。
  // NS/AGC 不暴露(实验定案:NS 双开啃双讲人声,锁死在代码)。
  async function onEchoCancel(v: string) {
    await settings.set('voice.capture.source', v) // 'auto' | 'browser' | 'cpal'
    await captureRoute.refresh() // auto:立即解析生效值(别等轮询);显式:镜像同步
    await restartWakeIfRunning()
    if (effectiveCapture.value === 'browser') void loadWebMics() // 生效 browser 顺手刷设备列表
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
    effectiveCapture.value === 'browser'
      ? [{ value: '', label: t('settings.voice.micDefault') }, ...webMics.value]
      : [
          { value: '', label: t('settings.voice.micDefault') },
          ...(voiceInfo.value?.devices ?? []).map((d) => ({ value: d, label: d })),
        ],
  )
  const micValue = computed(() =>
    effectiveCapture.value === 'browser'
      ? settings.get('voice.input_device_web')
      : settings.get('voice.input_device'),
  )

  // 自定义音色:选本地音频文件 → 前端解码/重采样成 16k → 后端转写出草稿 → 起名/改稿 → 保存。
  const cloneFile = ref<HTMLInputElement | null>(null)
  const cloneBusy = ref(false)
  const cloneRecording = ref(false) // 现场录音中(命令 await 到 VAD 静音自动收尾)
  const cloneErr = ref('')
  const cloneDraft = ref<{
    cloneId: string
    name: string
    transcript: string
    issue: 'clipped' | 'noisy' | 'noDenoise' | null
  } | null>(null)
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
      cloneDraft.value = { cloneId: d.cloneId, name: '', transcript: d.transcript, issue: d.check?.issue ?? null }
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
      cloneDraft.value = {
        cloneId: d.cloneId,
        name: f.name.replace(/\.[^.]+$/, ''),
        transcript: d.transcript,
        issue: d.check?.issue ?? null,
      }
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
    if (effectiveCapture.value === 'browser') {
      // 浏览器采集:换麦由 useMicBridge 热重启(停旧流起新流),core 推流管不动 → 不用重启唤醒
      void settings.set('voice.input_device_web', v)
      return
    }
    void settings.set('voice.input_device', v)
    void restartWakeIfRunning() // cpal:换麦立即生效,运行中的唤醒重开采集用新设备
  }

  return {
    asrOpts,
    calib,
    cancelCalib,
    startCalib,
    calibStepLabel,
    calibVerdict,
    cancelClone,
    captureRoute,
    captureSource,
    cloneArm,
    cloneBusy,
    cloneDraft,
    cloneErr,
    cloneFile,
    cloneRecording,
    loadVoice,
    loadWebMics,
    micOpts,
    micValue,
    onAsrModel,
    onCustomFile,
    onEchoCancel,
    onLeveling,
    onNightMode,
    onNightTime,
    onTtsBackend,
    onVoiceSeg,
    pickCustomVoice,
    previewSpeaker,
    previewing,
    recordClone,
    removeClone,
    restartWakeIfRunning,
    saveClone,
    saveSensitivity,
    setMic,
    toggleWake,
    voiceInfo,
    wakeBusy,
    wakeError,
    wakeShortName,
  }
}
