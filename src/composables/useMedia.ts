// 播放器 VM:全 app 一个播放位。core 经事件车道发"放这个/控制",按钮直连这里
// (不绕 LLM);音频用隐形 Audio 元素,视频元素由 VideoOverlay 挂载时登记进来。
// 浏览器预览:?demo=player 注入假"正在播放",纯看视觉。

import { reactive } from 'vue'
import {
  api,
  emitNowPlaying,
  isTauri,
  onAppEvent,
  onNowPlaying,
  win,
  windowLabel,
  type MediaEvent,
  type NowPlaying,
} from '../lib/backend'
import { i18n } from '../i18n'

export type PlayStatus = 'idle' | 'loading' | 'playing' | 'paused'

const state = reactive({
  current: null as NowPlaying | null,
  status: 'idle' as PlayStatus,
  /** 播放位置/总长(秒)。混流视频无原生 seek,position 含基准位移。 */
  position: 0,
  duration: 0,
  /** 音量 0–1:跨播放粘住(用户调好的音量别每次重置)。 */
  volume: 1,
  /** 倍速:每次新播放复位 1(mpv 时代的教训——倍速粘住,放完电影再放歌还是 2 倍)。 */
  rate: 1,
  /** 视频全屏中(HUD 缩成迷你胶囊的信号)。 */
  fullscreen: false,
  /** 建议气泡:扫码登录(首次播放后 core 提示一次;登录成功自动撤)。 */
  loginHint: null as string | null,
})

let audio: HTMLAudioElement | null = null
let videoEl: HTMLVideoElement | null = null
/** 混流视频 seek = 换 src 重启,这里记基准秒数,显示时间 = base + currentTime。 */
let videoBase = 0
let wired = false

function ensureAudio(): HTMLAudioElement {
  if (!audio) {
    audio = new Audio()
    audio.addEventListener('timeupdate', () => {
      if (state.current?.kind === 'audio') state.position = audio!.currentTime
    })
    audio.addEventListener('durationchange', () => {
      if (state.current?.kind === 'audio' && Number.isFinite(audio!.duration)) {
        state.duration = audio!.duration
      }
    })
    audio.addEventListener('playing', () => (state.status = 'playing'))
    audio.addEventListener('pause', () => {
      if (state.status !== 'idle') state.status = 'paused'
    })
    audio.addEventListener('ended', stop)
    audio.addEventListener('error', () => {
      if (state.current?.kind === 'audio') state.status = 'paused'
    })
  }
  audio.volume = state.volume
  return audio
}

/** VideoOverlay 挂载/卸载时登记播放元素(全 app 只有一个)。 */
export function registerVideoEl(el: HTMLVideoElement | null) {
  videoEl = el
  if (!el) return
  el.addEventListener('timeupdate', () => {
    if (state.current?.kind === 'video') state.position = videoBase + el.currentTime
  })
  // 时长:本地/直转单文件的真时长直到元数据加载才知道(np.duration_seconds 常为空 →
  // 进度条死、显示 /0:00 拖不动)。混流(/m/ fMP4)无可靠时长(el.duration=Infinity/NaN),
  // 保留 resolver 给的 np.duration_seconds 不被覆盖。
  const syncDuration = () => {
    const cur = state.current
    if (cur?.kind !== 'video' || cur.stream_url.includes('/m/')) return
    if (Number.isFinite(el.duration) && el.duration > 0) state.duration = el.duration
  }
  el.addEventListener('loadedmetadata', syncDuration)
  el.addEventListener('durationchange', syncDuration)
  el.addEventListener('playing', () => (state.status = 'playing'))
  el.addEventListener('pause', () => {
    if (state.status !== 'idle') state.status = 'paused'
  })
  el.addEventListener('ended', stop)
  // 出错别卡在 loading(否则换台/混流 seek 失败时 spinner 转不停)
  el.addEventListener('error', () => {
    if (state.current?.kind === 'video') state.status = 'paused'
  })
  el.volume = state.volume
  el.playbackRate = state.rate
  if (state.current?.kind === 'video') {
    el.src = state.current.stream_url
    void el.play().catch(() => {})
  }
}

function play(np: NowPlaying) {
  // 悬浮窗(独立 WebView)只显示"正在放",不实际出声 —— 否则与主窗双播(robot 双播坑的多窗变体)
  if (windowLabel() === 'float') {
    state.current = np
    state.status = 'playing'
    state.position = 0
    state.duration = np.duration_seconds ?? 0
    return
  }
  stopElements()
  state.current = np
  state.status = 'loading'
  state.position = 0
  state.duration = np.duration_seconds ?? 0
  state.rate = 1 // 倍速不跨播放粘住;音量粘住
  videoBase = 0
  syncToPeers() // 广播"在放这个"给悬浮窗镜像
  if (np.kind === 'audio') {
    const a = ensureAudio()
    a.playbackRate = 1
    a.src = np.stream_url
    void a.play().catch(() => (state.status = 'paused'))
  } else if (np.kind === 'video') {
    if (videoEl) {
      videoEl.playbackRate = 1
      videoEl.volume = state.volume
      videoEl.src = np.stream_url
      void videoEl.play().catch(() => (state.status = 'paused'))
    }
    // videoEl 还没挂:VideoOverlay 随 current 出现,registerVideoEl 接力起播。
    // 视频默认全屏(用户要求):置位放在 videoEl 守卫外,后挂场景也直接铺满、不窗口化闪一下;
    //.maximized 绑 state.fullscreen,浮层挂载瞬间即全屏。此处必是主窗(float 已在函数开头早退)。
    state.fullscreen = true
    void win.setFullscreen(true)
  }
}

function activeEl(): HTMLMediaElement | null {
  if (!state.current) return null
  return state.current.kind === 'audio' ? audio : videoEl
}

function pause() {
  activeEl()?.pause()
}

function resume() {
  void activeEl()?.play().catch(() => {})
}

function toggle() {
  state.status === 'playing' ? pause() : resume()
}

/** 音量 0–1:作用到两个元素(切音频/视频不丢),跨播放粘住。 */
function setVolume(v: number) {
  state.volume = Math.min(1, Math.max(0, v))
  if (audio) audio.volume = state.volume
  if (videoEl) videoEl.volume = state.volume
}

/** 倍速 0.25–3:作用到当前元素;新播放复位 1。 */
function setRate(v: number) {
  state.rate = Math.min(3, Math.max(0.25, v))
  const el = activeEl()
  if (el) el.playbackRate = state.rate
}

/** 主窗(唯一真播放位)把 current 全量广播给悬浮窗(被动镜像)。绝对态快照 → 幂等;
 *  只主窗发、只悬浮窗收(见 wire),无回声环。悬浮窗自身调用是 no-op(它不该出声当真相源)。 */
function syncToPeers() {
  if (windowLabel() !== 'float') emitNowPlaying(state.current)
}

function stopElements() {
  if (audio) {
    audio.pause()
    audio.removeAttribute('src')
  }
  if (videoEl) {
    videoEl.pause()
    videoEl.removeAttribute('src')
  }
}

function stop() {
  stopElements()
  // 关播放器顺带退原生全屏,别把整个 app 卡在全屏(✕/ended/模型 stop 都汇到这里)。
  // 守卫:float 的 fullscreen 恒 false(play 早退,从不置位)→ 永不误退悬浮窗。
  if (state.fullscreen) void win.setFullscreen(false)
  state.current = null
  state.status = 'idle'
  state.position = 0
  state.duration = 0
  state.fullscreen = false
  syncToPeers() // 广播"停了"给悬浮窗(修:UI 点停止 / 自然播完时它仍显在放)
}

/** seek:音频/直转流走原生(转发层透传 Range);混流视频换 src 重启(?t=)。 */
function seek(seconds: number) {
  const cur = state.current
  if (!cur) return
  if (cur.kind === 'audio' && audio) {
    audio.currentTime = seconds
    return
  }
  if (cur.kind === 'video' && videoEl) {
    if (cur.stream_url.includes('/m/')) {
      videoBase = seconds
      state.status = 'loading' // 换 src 重启混流,黑屏期间显示 spinner(别看着像卡死);playing 事件复位
      const base = cur.stream_url.split('?')[0]
      videoEl.src = `${base}?t=${seconds.toFixed(1)}`
      void videoEl.play().catch(() => (state.status = 'paused'))
    } else {
      videoEl.currentTime = seconds
    }
    state.position = seconds
  }
}

function dismissLoginHint() {
  state.loginHint = null
}

function loginNow() {
  const source = state.loginHint ?? state.current?.source ?? 'bilibili'
  state.loginHint = null
  void api.mediaLogin(source, i18n.global.t('media.loginTitle'))
}

function onMedia(ev: MediaEvent) {
  switch (ev.type) {
    case 'play':
      play(ev.data)
      break
    case 'control':
      // 模型侧控制(用户用嘴说的);词表外的动作忽略。校验在 core,这里只执行
      if (ev.data.action === 'pause') pause()
      else if (ev.data.action === 'resume') resume()
      else if (ev.data.action === 'stop') stop()
      else if (ev.data.action === 'louder') setVolume(state.volume + 0.2)
      else if (ev.data.action === 'softer') setVolume(state.volume - 0.2)
      else if (ev.data.action === 'speed' && ev.data.value != null) setRate(ev.data.value)
      else if (ev.data.action === 'seek' && ev.data.value != null) seek(ev.data.value)
      break
    case 'auth_required':
    case 'login_hint':
      state.loginHint = ev.data.source
      break
    case 'logged_in':
      state.loginHint = null
      break
  }
}

function wire() {
  if (wired) return
  wired = true
  if (isTauri()) {
    onAppEvent((ev) => {
      if (ev.type === 'media') onMedia(ev.data)
    })
    // 悬浮窗 = 主窗播放位的被动镜像:current 跟主窗广播走,主窗停了它才清得掉。
    if (windowLabel() === 'float') {
      onNowPlaying((np) => {
        state.current = np
        state.status = np ? 'playing' : 'idle'
      })
    }
    return
  }
  const demo = new URLSearchParams(location.search).get('demo') ?? ''
  if (demo.includes('player')) {
    state.current = {
      kind: 'audio',
      title: '恭喜发财 刘德华 官方MV',
      author: '华仔频道',
      duration_seconds: 225,
      stream_url: '',
      page_url: '#',
      source: 'bilibili',
    }
    state.status = 'playing'
    state.duration = 225
    state.position = 67
    state.loginHint = 'bilibili'
  }
}

export function useMedia() {
  wire()
  return { state, toggle, pause, resume, stop, seek, setVolume, setRate, loginNow, dismissLoginHint }
}
