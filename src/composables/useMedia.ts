// 播放器 VM:全 app 一个播放位。core 经事件车道发"放这个/控制",按钮直连这里
// (不绕 LLM);音频用隐形 Audio 元素,视频元素由 VideoOverlay 挂载时登记进来。
// 浏览器预览:?demo=player 注入假"正在播放",纯看视觉。

import { reactive } from 'vue'
import {
  api,
  emitMediaControl,
  emitNowPlaying,
  isTauri,
  onAppEvent,
  onMediaControl,
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
/** 视频起播前主窗是否藏在托盘:停时据此藏回去(别看完视频凭空冒出主界面)。 */
let videoWasHidden = false
let wired = false
/** 悬浮窗(独立 WebView):不出声、只镜像主窗;播控转发给主窗执行(窗口标签恒定,缓存一次)。 */
const isFloat = windowLabel() === 'float'

// ── shaka(MSE 自适应流):只在放 DASH/HLS(np.manifest_url 有值,如 B 站)时**懒加载**,音频/本地
// 直传不碰它(省 ~400KB)。播放器自己管时间轴 → 原生 seek + 音画同步,治「混流 + ?t= 重启 seek」的
// 错位(那是固有缺陷,见 relay::Entry::Dash)。shaka 经 MSE 接管 <video>,故不再手设 el.src。
let shakaLib: any = null
let shakaPlayer: any = null
async function loadShaka(): Promise<any> {
  if (!shakaLib) {
    // 【诊断版】用 shaka 调试构建 + 详细日志,把「卡在哪」打到控制台(生产 compiled 版不打日志)。
    // 黑屏定位完恢复成 `import('shaka-player')`(生产版)。
    const mod: any = await import('shaka-player/dist/shaka-player.compiled.debug.js')
    shakaLib = mod.default ?? mod
    try {
      shakaLib.polyfill?.installAll?.()
      shakaLib.log?.setLevel?.(shakaLib.log.Level.DEBUG)
    } catch {
      /* 尽力 */
    }
  }
  return shakaLib
}
async function destroyShaka() {
  const p = shakaPlayer
  shakaPlayer = null
  if (p) {
    try {
      await p.destroy()
    } catch {
      /* 已销毁/未挂载 */
    }
  }
}
/** 把当前视频装进 <video>:自适应流(manifest_url)走 shaka(MSE);否则原生 src(直传/本地混流)。
 *  play() 与 registerVideoEl(后挂场景)都走它。异步加载期间若已切走/停了,据 state.current 比对退出。 */
async function loadVideoInto(el: HTMLVideoElement) {
  const cur = state.current
  if (!cur || cur.kind !== 'video') return
  el.playbackRate = 1
  el.volume = state.volume
  if (!cur.manifest_url) {
    el.src = cur.stream_url
    void el.play().catch(() => (state.status = 'paused'))
    return
  }
  try {
    console.log('[lw] shaka 加载 manifest:', cur.manifest_url) // 【诊断】
    const shaka = await loadShaka()
    await destroyShaka()
    if (state.current !== cur) return // 加载期间已切走/停
    const player = new shaka.Player()
    shakaPlayer = player
    await player.attach(el)
    player.addEventListener('error', (e: any) => {
      console.error('[lw][shaka] error', e?.detail ?? e) // 【诊断】shaka 报错(带 code/category)
      if (state.current?.kind === 'video') state.status = 'paused'
    })
    player.addEventListener('buffering', (e: any) => console.log('[lw][shaka] buffering=', e?.buffering)) // 【诊断】
    el.addEventListener('waiting', () => console.log('[lw] <video> waiting(等数据,卡了)')) // 【诊断】
    el.addEventListener('canplay', () => console.log('[lw] <video> canplay,buffered=', el.buffered.length ? `${el.buffered.start(0)}-${el.buffered.end(0)}` : '空')) // 【诊断】
    await player.load(cur.manifest_url)
    console.log('[lw] shaka.load 完成;el.duration=', el.duration, 'buffered段=', el.buffered.length) // 【诊断】
    if (state.current !== cur) {
      void destroyShaka()
      return
    }
    el.play().then(() => console.log('[lw] play() 成功')).catch((err) => console.warn('[lw] play() 被拒:', err?.name, err?.message)) // 【诊断】
  } catch (e) {
    console.error('[lw][shaka] load failed', e) // 【诊断】
    if (state.current === cur) state.status = 'paused'
  }
}

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
    audio.addEventListener('playing', () => {
      state.status = 'playing'
      syncToPeers() // 状态变化也镜像给悬浮窗(迷你播控的播/暂停图标据此翻转)
    })
    audio.addEventListener('pause', () => {
      if (state.status !== 'idle') state.status = 'paused'
      syncToPeers()
    })
    audio.addEventListener('ended', onEnded)
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
  if (!el) {
    void destroyShaka() // 浮层卸载:拆掉 shaka(它接管了那个 <video>)
    return
  }
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
  el.addEventListener('playing', () => {
    state.status = 'playing'
    syncToPeers()
  })
  el.addEventListener('pause', () => {
    if (state.status !== 'idle') state.status = 'paused'
    syncToPeers()
  })
  el.addEventListener('ended', onEnded)
  // 出错别卡在 loading(否则换台/混流 seek 失败时 spinner 转不停)
  el.addEventListener('error', () => {
    if (state.current?.kind === 'video') state.status = 'paused'
  })
  el.volume = state.volume
  el.playbackRate = state.rate
  if (state.current?.kind === 'video') {
    void loadVideoInto(el) // 后挂场景:接力起播(自适应走 shaka,否则原生 src)
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
  // 自动续播(同为视频、已在全屏):接着放下一集,**不重做**唤窗/置顶/全屏,videoWasHidden 也保留
  // (整季放完 stop() 时才据它决定是否藏回托盘)→ 集与集之间无缝、不闪窗口化。
  const continuation = state.current?.kind === 'video' && np.kind === 'video' && state.fullscreen
  stopElements()
  state.current = np
  state.status = 'loading'
  state.position = 0
  state.duration = np.duration_seconds ?? 0
  state.rate = 1 // 倍速不跨播放粘住;音量粘住
  videoBase = 0
  if (!continuation) videoWasHidden = false // 新播放复位;续播保留首集起的"是否藏着"
  syncToPeers() // 广播"在放这个"给悬浮窗镜像
  if (np.kind === 'audio') {
    const a = ensureAudio()
    a.playbackRate = 1
    a.src = np.stream_url
    void a.play().catch(() => (state.status = 'paused'))
  } else if (np.kind === 'video') {
    if (videoEl) void loadVideoInto(videoEl) // 自适应走 shaka,否则原生 src
    // videoEl 还没挂:VideoOverlay 随 current 出现,registerVideoEl 接力起播(同样走 loadVideoInto)。
    if (!continuation) {
      // 首次起播视频:叫主窗到最前(藏在托盘/别的窗后面时只闻其声)+ 置顶(别被盖住)+ 全屏(用户要求)。
      // 置位放在 videoEl 守卫外,后挂场景也直接铺满、不窗口化闪一下;.maximized 绑 state.fullscreen,
      // 浮层挂载瞬间即全屏。此处必是主窗(float 已在函数开头早退)。
      state.fullscreen = true // 同步置位:.maximized 立即生效,浮层挂载即全屏(不闪窗口化)
      // 窗口动作串行:读"是否藏着"必须在 show 之前(停时据此藏回托盘),再 show + 置顶 + 全屏。
      void (async () => {
        videoWasHidden = await win.isHidden()
        await win.bringToFront()
        await win.setAlwaysOnTop(true)
        await win.setFullscreen(true)
      })()
    }
    // continuation:已经全屏置顶,什么都不做(state.fullscreen 维持 true、窗口不动)。
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
  // 悬浮窗:不出声,把播/暂停转发给主窗执行;按镜像来的状态判方向
  if (isFloat) return emitMediaControl(state.status === 'playing' ? 'pause' : 'resume')
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

/** 主窗(唯一真播放位)把当下播放态广播出去:① 给悬浮窗镜像(被动跟随);② 回报给 core,
 *  让模型下个回合拿到「此刻」真相(修「歌放完了却以为还在播」)。绝对态快照 → 幂等;
 *  只主窗发,悬浮窗自身调用是 no-op(它是镜像、不当真相源)。所有播放态切换都经此(play/暂停/
 *  ended/stop 的监听都调它),所以回报 core 一处接上即全覆盖。 */
function syncToPeers() {
  if (isFloat) return
  emitNowPlaying(state.current, state.status)
  // fire-and-forget;非 Tauri(浏览器预览)跳过,失败不打断播放
  if (isTauri()) void api.reportMediaState(state.status, state.current?.title ?? null).catch(() => {})
}

function stopElements() {
  if (audio) {
    audio.pause()
    audio.removeAttribute('src')
  }
  void destroyShaka() // 自适应流:先拆 shaka(它经 MSE 接管了 <video>),再清原生 src
  if (videoEl) {
    videoEl.pause()
    videoEl.removeAttribute('src')
    try {
      videoEl.load() // 复位元素,清掉 MSE 残留
    } catch {
      /**/
    }
  }
}

function stop() {
  if (isFloat) return emitMediaControl('stop') // 悬浮窗转发,真停在主窗(它清完会广播 null 回来)
  stopElements()
  // 退出视频的窗口态:退全屏 + 撤置顶(✕/ended/模型 stop 都汇到这里)。float 不碰自身窗口
  // ——它的"播放"只是镜像,对悬浮窗做 setFullscreen/setAlwaysOnTop 会误伤它(它常驻置顶)。
  if (windowLabel() !== 'float') {
    if (state.fullscreen) void win.setFullscreen(false)
    void win.setAlwaysOnTop(false)
    // 起播前主窗藏在托盘 → 看完藏回去(先退全屏后 hide,FIFO 保证下次唤出不残留全屏)。
    if (videoWasHidden) win.hideToTray()
  }
  videoWasHidden = false
  state.current = null
  state.status = 'idle'
  state.position = 0
  state.duration = 0
  state.fullscreen = false
  syncToPeers() // 广播"停了"给悬浮窗(修:UI 点停止 / 自然播完时它仍显在放)
}

/** 一集放完:有下一集 → 自动续播(core 现取现播,publishes Play 接力,保持全屏);否则正常停。
 *  只在主窗触发(悬浮窗不实际播放、不会冒 ended)。 */
function onEnded() {
  const pl = state.current?.playlist
  if (pl && pl.index < pl.total - 1) {
    if (isTauri()) {
      state.status = 'loading' // 续播解析的空档显 spinner(别看着像卡死)
      void api.mediaAdvance(1).catch(() => stop()) // 切集失败兜底停
    } else {
      stop() // 浏览器预览无 core
    }
    return
  }
  stop() // 末集 / 单集:正常收尾
}

/** 上/下一集(+1/-1):播放器按钮 + 嘴控都最终汇到 core 的 advance(全局队列);任意窗口可调
 *  —— 与 play/pause 这类"本地元素操作"不同,切集是 core 现取现播,float 调了也由主窗接力。 */
function advance(delta: number) {
  if (!isTauri()) return
  void api.mediaAdvance(delta).catch(() => {})
}

/** seek:自适应流(shaka)/ 音频 / 直转单文件走**原生** currentTime(播放器管时间轴,精确 + 同步);
 *  只有本地转码的渐进混流(/m/、无 manifest)才换 src 重启(?t=)—— Stage 2 上 HLS 后这条也会消失。 */
function seek(seconds: number) {
  const cur = state.current
  if (!cur) return
  if (cur.kind === 'audio' && audio) {
    audio.currentTime = seconds
    return
  }
  if (cur.kind === 'video' && videoEl) {
    if (!cur.manifest_url && cur.stream_url.includes('/m/')) {
      videoBase = seconds
      state.status = 'loading' // 换 src 重启混流,黑屏期间显示 spinner(别看着像卡死);playing 事件复位
      const base = cur.stream_url.split('?')[0]
      videoEl.src = `${base}?t=${seconds.toFixed(1)}`
      void videoEl.play().catch(() => (state.status = 'paused'))
    } else {
      videoEl.currentTime = seconds // shaka 自适应 / 直传:原生精确 seek
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

/** 执行一个播控动作(词表外忽略)。嘴控(core)与悬浮窗迷你播控的转发都汇到这里;
 *  只在主窗(真播放位)调用 —— 此处 pause/resume/stop 等都是本地实控,不再转发。 */
function applyControl(action: string, value?: number) {
  if (action === 'pause') pause()
  else if (action === 'resume') resume()
  else if (action === 'stop') stop()
  else if (action === 'louder') setVolume(state.volume + 0.2)
  else if (action === 'softer') setVolume(state.volume - 0.2)
  else if (action === 'speed' && value != null) setRate(value)
  else if (action === 'seek' && value != null) seek(value)
}

function onMedia(ev: MediaEvent) {
  switch (ev.type) {
    case 'play':
      play(ev.data)
      break
    case 'control':
      // 嘴控(core 已校验);只主窗执行 —— 悬浮窗处理会再转发回主窗,徒增重复
      if (!isFloat) applyControl(ev.data.action, ev.data.value)
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
    if (isFloat) {
      // 悬浮窗 = 主窗播放位的被动镜像:current/status 跟主窗广播走(放/暂停/停都追平)
      onNowPlaying((np, status) => {
        state.current = np
        state.status = (np ? status : 'idle') as PlayStatus
      })
    } else {
      // 主窗(唯一真播放位):收悬浮窗迷你播控的转发,与嘴控汇到同一执行口
      onMediaControl((action, value) => applyControl(action, value))
    }
    return
  }
  const demo = new URLSearchParams(location.search).get('demo') ?? ''
  if (demo.includes('series')) {
    // 多集视频(动画片续播)视觉预览:VideoOverlay 的集数指示 + 上/下一集
    state.current = {
      kind: 'video',
      title: '小猪佩奇 第3集 踩泥坑',
      duration_seconds: 320,
      stream_url: '',
      page_url: '#',
      source: 'local',
      playlist: { index: 2, total: 12, resumed: false },
    }
    state.status = 'playing'
    state.duration = 320
    state.position = 88
    state.fullscreen = false
  } else if (demo.includes('player')) {
    state.current = {
      kind: 'audio',
      title: '西游记 第7回 收服白龙马',
      author: '单田芳 评书',
      duration_seconds: 225,
      stream_url: '',
      page_url: '#',
      source: 'bilibili',
      // 多集音频(评书/儿歌合集):播放条出集数 + 上/下一集
      playlist: { index: 6, total: 30, resumed: false },
    }
    state.status = 'playing'
    state.duration = 225
    state.position = 67
    state.loginHint = 'bilibili'
  }
}

export function useMedia() {
  wire()
  return {
    state,
    toggle,
    pause,
    resume,
    stop,
    seek,
    setVolume,
    setRate,
    next: () => advance(1),
    prev: () => advance(-1),
    loginNow,
    dismissLoginHint,
  }
}
