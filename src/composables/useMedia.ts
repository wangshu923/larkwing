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
import { attachMedia, detachAudio } from './useAudioGraph'
import { isAdaptiveUrl, playAdaptive, type AdaptiveController } from './localAdaptive'

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
  /** 循环模式(core 是真相源,这里是镜像:Play 事件全量捎带 + Control 事件增量对齐)。
   *  one=单曲(落 el.loop 原生无缝循环);all=列表(core auto_next 回卷;没队列时也落 el.loop)。 */
  loopMode: 'off' as 'off' | 'one' | 'all',
  /** 随机播放镜像(多集队列才可能 true;挑歌在 core)。 */
  shuffle: false,
  /** 视频全屏中(HUD 缩成迷你胶囊的信号)。 */
  fullscreen: false,
  /** 建议气泡:扫码登录(首次播放后 core 提示一次;登录成功自动撤)。 */
  loginHint: null as string | null,
})

let audio: HTMLAudioElement | null = null
let videoEl: HTMLVideoElement | null = null
/** 混流视频 seek = 换 src 重启,这里记基准秒数,显示时间 = base + currentTime。 */
let videoBase = 0
// 唤醒避让(duck):语音交互期间把播放压低,让 7274 的话被听见。`state.volume` 是**基准**
// (用户意图音量),实时元素音量 = 基准 × (ducked ? 0.2 : 1)。duck 期间用户调音量改的是基准,
// 恢复时新基准生效 —— 修「50%→喊→压低→大点声→恢复又被无脑还原成 50%、改动丢失」的 bug。
const DUCK_RATIO = 0.2
let ducked = false
/** 实时元素音量 = 基准(state.volume)按是否避让折算。 */
function liveVolume(): number {
  return ducked ? state.volume * DUCK_RATIO : state.volume
}
/** 把当前实时音量刷到两个元素(切音频/视频不丢)。即时设置 = 抢占进行中的渐变。 */
function applyVolume() {
  cancelFade()
  if (audio) audio.volume = liveVolume()
  if (videoEl) videoEl.volume = liveVolume()
}

// 避让恢复用渐变(逐渐爬回基准),不一下子轰回来 —— 压低仍即时(让 7274 的话马上被听见)。
// 700ms 用户反馈「还是有点快」(2026-07-02)→ 放慢到 2.5s,电影声音缓缓浮回来。
const DUCK_RESTORE_FADE_MS = 2500
let fadeTimer: ReturnType<typeof setInterval> | undefined
function cancelFade() {
  if (fadeTimer) {
    clearInterval(fadeTimer)
    fadeTimer = undefined
  }
}
/** 把两个元素音量在 ms 内从各自当前值平滑爬到目标(liveVolume);新的音量改动/压低会抢占。 */
function fadeToLive(ms: number) {
  cancelFade()
  const els = [audio, videoEl].filter(Boolean) as HTMLMediaElement[]
  if (!els.length) return
  const target = liveVolume()
  const from = els.map((e) => e.volume)
  const STEP_MS = 40
  const steps = Math.max(1, Math.round(ms / STEP_MS))
  let i = 0
  fadeTimer = setInterval(() => {
    i += 1
    const k = Math.min(1, i / steps)
    els.forEach((e, idx) => (e.volume = from[idx] + (target - from[idx]) * k))
    if (k >= 1) cancelFade()
  }, STEP_MS)
}
/** 一次性起播定位:元数据就绪后把播放头挪到 at 秒(切音轨重建管线后 core 经
 *  NowPlaying.resume_at 要求「接着刚才的位置放」)。at 无效 = no-op。 */
function applyResume(el: HTMLMediaElement, at?: number) {
  if (!at || at <= 0) return
  el.addEventListener(
    'loadedmetadata',
    () => {
      try {
        el.currentTime = at
      } catch {
        /* 元数据异常时放弃回跳,从头播也比不播强 */
      }
    },
    { once: true },
  )
}

/** 多音轨收敛:用 audioTracks API 只启用选中那条(WKWebView/Safari 支持;Chromium 没有 = no-op)。
 *  **只作用于直传路**(元素直接播放含多条真实音轨的原始容器,BD remux 全轨 enabled 会混播)。
 *  ⚠️ MSE 路(自适应/HLS/DASH//m/)绝不能进来:选轨已由服务端 `-map` 完成,呈现里只有**一条**
 *  音轨——在它上面按「文件轨号」收敛,轨号 ≠0 时会把唯一音轨 disable 掉,WebKit 随即丢弃该轨
 *  全部样本(append 成功、buffered 恒空)= 切音轨无声/卡死的终极真凶(2026-07-22,四轮排查)。 */
function applyAudioTrackToEl(el: HTMLMediaElement) {
  const cur = state.current
  if (!cur || cur.manifest_url || cur.stream_url.includes('/m/')) return // 非直传:交给 -map
  const list = (el as { audioTracks?: { length: number; [i: number]: { enabled: boolean } } })
    .audioTracks
  const total = cur.audio_tracks?.length ?? 0
  if (total < 2 || !list || typeof list.length !== 'number') return
  const want = cur.audio_track ?? 0
  for (let i = 0; i < list.length; i++) list[i].enabled = i === want
}

/** 循环落到播放元素:单曲循环用原生 el.loop(无缝、ended 压根不触发);
 *  「列表循环 + 没队列」只有一首,同样落 el.loop(等价单曲)。列表循环有队列时不设
 *  loop —— ended 正常触发,由 core auto_next 回卷。 */
function syncLoopToEl() {
  const native =
    state.loopMode === 'one' || (state.loopMode === 'all' && !state.current?.playlist)
  if (audio) audio.loop = native
  if (videoEl) videoEl.loop = native
}

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
    const mod: any = await import('shaka-player')
    shakaLib = mod.default ?? mod
    try {
      shakaLib.polyfill?.installAll?.()
    } catch {
      /* 尽力 */
    }
  }
  return shakaLib
}
// 串行化所有 shaka 拆除:装新 player 前必等这条链跑完。
// 修竞态(2026-06-23):stopElements 里 `void destroyShaka()` 是 fire-and-forget(同步把 shakaPlayer
// 置 null、p.destroy() 脱钩跑),会让 loadVideoInto 里本该串行的 `await destroyShaka()` 空转 —— 随即
// 在旧 player 仍 destroy 中的同一 <video> 上 attach 新 player(WebView2/MSE 易炸,§8.1)。改成把每次
// 拆除串到链尾、destroyShaka 返回链尾;loadVideoInto `await destroyShaka()` 即等到在飞的拆除真正跑完。
let shakaTeardown: Promise<void> = Promise.resolve()
function destroyShaka(): Promise<void> {
  const p = shakaPlayer
  shakaPlayer = null
  if (p) {
    shakaTeardown = shakaTeardown.then(() => p.destroy().catch(() => {})) // 已销毁/未挂载也吞掉
  }
  return shakaTeardown
}
// 本地自适应(手写 MSE,音视频分离,0.2.6 治漂移):与 shaka 二选一,各自的 <video> 接管要拆干净。
let adaptiveCtl: AdaptiveController | null = null
/** 已为哪个 page_url 兜底回落过 muxed(每文件只兜一次,防来回重放);新 play() 复位。 */
let adaptiveFellBackFor: string | null = null
function destroyAdaptive() {
  adaptiveCtl?.stop()
  adaptiveCtl = null
}
/** 把当前视频装进 <video>:自适应流(manifest_url)走 shaka(MSE);否则原生 src(直传/本地混流)。
 *  play() 与 registerVideoEl(后挂场景)都走它。异步加载期间若已切走/停了,据 state.current 比对退出。 */
async function loadVideoInto(el: HTMLVideoElement) {
  const cur = state.current
  if (!cur || cur.kind !== 'video') return
  el.playbackRate = 1
  el.volume = liveVolume()
  // 起播定位(切音轨重建管线的「接着放」):消费一次即清,防浮层重挂载重复回跳
  const resume = cur.resume_at && cur.resume_at > 0 ? cur.resume_at : 0
  if (cur.resume_at != null) cur.resume_at = undefined
  if (!cur.manifest_url) {
    if (resume && cur.stream_url.includes('/m/')) {
      // /m/ 渐进混流无原生 seek:定位烤进 ?t=(与 seek() 同机制)
      videoBase = resume
      el.src = `${cur.stream_url.split('?')[0]}?t=${resume.toFixed(1)}`
    } else {
      el.src = cur.stream_url
      applyResume(el, resume)
    }
    void el.play().catch(() => (state.status = 'paused'))
    return
  }
  // 本地自适应(/la/):手写 MSE(音视频分离,连续音频治漂移 + 视频 copy 省 CPU)。绕开 shaka。
  // 续播位直接传进 playAdaptive(段指针直指目标,不从 0 爬灌)——绝不走 applyResume 的
  // 「先播 0 再跳」:那会撞「WKWebView 不接入播放开始后才补进的音频」→ 切轨无声(2026-07-22)。
  if (isAdaptiveUrl(cur.manifest_url)) {
    destroyAdaptive()
    await destroyShaka()
    if (state.current !== cur) return
    // 兜底:自适应(setup 或播放期)失败 → 让后端对同一文件强制走 muxed HLS(能放的老路,§3.5)。
    // 每个文件只兜底一次(避免 muxed 也失败时来回重放);why 富含现场,写进 larkwing.log 供真机定位。
    const fallbackCompat = (why?: string) => {
      if (adaptiveFellBackFor === cur.page_url) return
      adaptiveFellBackFor = cur.page_url
      console.warn('[lw][adaptive] failed → 回落 muxed HLS:', why)
      void api.mediaLog(`[adaptive] 播放失败(${why ?? '?'})→ 回落 muxed HLS`)
      if (isTauri()) void api.mediaReplayCompat(cur.page_url, cur.kind === 'audio').catch(() => {})
    }
    try {
      const ctl = await playAdaptive(el, cur.manifest_url, fallbackCompat, resume)
      if (state.current !== cur) ctl.stop() // 加载期间已切走
      else {
        adaptiveCtl = ctl
        void api.mediaLog('[adaptive] 起播 ok') // 日志留痕:setup 成功(卡在其后就知道不是 setup 问题)
      }
    } catch (e) {
      fallbackCompat('setup: ' + e)
    }
    return
  }
  applyResume(el, resume) // shaka(muxed HLS/DASH):元数据就绪后原生 seek 回续播位
  try {
    const shaka = await loadShaka()
    await destroyShaka()
    if (state.current !== cur) return // 加载期间已切走/停
    const player = new shaka.Player()
    shakaPlayer = player
    await player.attach(el)
    // 出错时打全 code/category/data + MSE 的 video.error —— 生产版也带 data,够定位
    //(本地 fMP4-HLS 黑屏就是靠它定位到 MSE append 失败,见 relay::build_frag_cmd)。
    player.addEventListener('error', (e: any) => {
      const err = e?.detail ?? e
      console.error('[lw][shaka] error', { code: err?.code, category: err?.category, data: err?.data, mediaError: el.error?.message })
      if (state.current?.kind === 'video') state.status = 'paused'
    })
    await player.load(cur.manifest_url)
    if (state.current !== cur) {
      void destroyShaka()
      return
    }
    el.play().catch(() => (state.status = 'paused'))
  } catch (e) {
    console.error('[lw][shaka] load failed', e, 'mediaError=', el.error?.message)
    if (state.current === cur) state.status = 'paused'
  }
}

function ensureAudio(): HTMLAudioElement {
  if (!audio) {
    audio = new Audio()
    attachMedia(audio) // 响度均衡:设 crossorigin + 挂处理链(须在设 src 前;总开关关则原样播放)
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
    // 多音轨收敛(≥2 轨才动作;单轨/无 API 是 no-op)
    audio.addEventListener('loadedmetadata', () => applyAudioTrackToEl(audio!))
  }
  audio.volume = liveVolume()
  syncLoopToEl()
  return audio
}

/** VideoOverlay 挂载/卸载时登记播放元素(全 app 只有一个)。 */
export function registerVideoEl(el: HTMLVideoElement | null) {
  if (videoEl && videoEl !== el) detachAudio(videoEl) // 换/卸载旧 <video>:释放它的响度均衡链,防泄漏/多源争抢
  videoEl = el
  if (!el) {
    destroyAdaptive() // 浮层卸载:拆掉手写 MSE(它接管了那个 <video>)
    void destroyShaka() // 浮层卸载:拆掉 shaka(它接管了那个 <video>)
    return
  }
  attachMedia(el) // 响度均衡:设 crossorigin + 挂处理链(须在下方 loadVideoInto 设 src 前)
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
  // 多音轨收敛:直传的多音轨片只留选中那条(治 WKWebView 全轨混播;≥2 轨才动作)
  el.addEventListener('loadedmetadata', () => applyAudioTrackToEl(el))
  el.volume = liveVolume()
  el.playbackRate = state.rate
  syncLoopToEl()
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
  // 续播 / 换片:已有视频在放、又来一个视频(自动切下一集 / 手动换片)= 接力,**不重做**唤窗/置顶/
  // 全屏,videoWasHidden 也保留(整季放完 stop() 时才据它决定是否藏回托盘)→ 无缝、不闪窗口化,
  // 且**尊重用户当前窗口模式**:窗口化看剧时切下一集不该被强行拽回全屏。
  // (原误用 `&& state.fullscreen` 当判据 → 用户退全屏成窗口播放后,下一集 continuation=false →
  //  强行 bringToFront+全屏,每个集边界都拽一次,是 bug。)只有「从无到有」起播视频才叫窗到前 + 全屏。
  const continuation = state.current?.kind === 'video' && np.kind === 'video'
  stopElements()
  adaptiveFellBackFor = null // 新播放:清兜底记忆(muxed 回落走 /hls/ 不再进自适应,不会循环)
  state.current = np
  state.status = 'loading'
  state.position = 0
  state.duration = np.duration_seconds ?? 0
  state.rate = 1 // 倍速不跨播放粘住;音量粘住
  // 循环/随机镜像:core 每次 Play 全量捎带(新播放的复位、切集/自动续播的延续,这里零猜测)。
  state.loopMode = np.loop_mode ?? 'off'
  state.shuffle = np.shuffle ?? false
  syncLoopToEl()
  videoBase = 0
  if (!continuation) videoWasHidden = false // 新播放复位;续播保留首集起的"是否藏着"
  syncToPeers() // 广播"在放这个"给悬浮窗镜像
  if (np.kind === 'audio') {
    const a = ensureAudio()
    a.playbackRate = 1
    const resume = np.resume_at && np.resume_at > 0 ? np.resume_at : 0
    if (np.resume_at != null && state.current) state.current.resume_at = undefined
    a.src = np.stream_url
    applyResume(a, resume)
    void a.play().catch(() => (state.status = 'paused'))
  } else if (np.kind === 'video') {
    // 不在旧元素上直接起播:VideoOverlay 的 <video> 按会话 key 重建(WKWebView 复用出过声的
    // 元素会拖死新会话的音频 SB),新元素挂上后由 registerVideoEl → loadVideoInto 接力起播。
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

/** 音量 0–1:设的是**基准**(用户意图),跨播放粘住;实时元素音量按是否避让折算。
 *  duck 期间调音量也是改基准 → 恢复后新音量生效(不再被无脑还原冲掉)。
 *  调完回报 core:模型的「此刻」背景里音量要跟手(才答得出「现在多大声」)。 */
function setVolume(v: number) {
  state.volume = Math.min(1, Math.max(0, v))
  applyVolume()
  reportToCore()
}

/** 唤醒避让开关(useVoice 调):on=语音交互期压低,off=恢复。改的是折算系数,基准不动 →
 *  期间 louder/softer 改基准、恢复时生效。幂等。 */
function setDucked(on: boolean) {
  if (ducked === on) return
  ducked = on
  // 压低:即时(applyVolume 内部已 cancelFade),让 7274 的话马上被听见;
  // 恢复:渐变爬回基准,避免一下子轰回来很突兀。
  if (on) applyVolume()
  else fadeToLive(DUCK_RESTORE_FADE_MS)
}

/** 倍速 0.25–3:作用到当前元素;新播放复位 1。调完回报 core(进度外推按倍速算)。 */
function setRate(v: number) {
  state.rate = Math.min(3, Math.max(0.25, v))
  const el = activeEl()
  if (el) el.playbackRate = state.rate
  reportToCore()
}

/** 回报 core 当下播放快照(「此刻」背景的数据源):状态/标题之外带**基准音量、进度、时长、
 *  倍速** —— 模型据此才能「音量调到 50」「快进 5 分钟」(相对量自己按当前值算)。
 *  只主窗(真播放位)报;fire-and-forget,非 Tauri(浏览器预览)跳过。
 *  进度在 core 侧按回报时刻 + 倍速外推,所以这里不必高频报 —— 状态切换/音量/倍速/seek 各报
 *  一次 + 播放中低频心跳(兜缓冲卡顿的外推漂移)即可。 */
function reportToCore() {
  if (isFloat || !isTauri()) return
  void api
    .reportMediaState({
      status: state.status,
      title: state.current?.title ?? null,
      volume: Math.round(state.volume * 100),
      position: state.position,
      duration: state.duration > 0 ? state.duration : null,
      rate: state.rate,
    })
    .catch(() => {})
}

/** 主窗(唯一真播放位)把当下播放态广播出去:① 给悬浮窗镜像(被动跟随);② 回报给 core,
 *  让模型下个回合拿到「此刻」真相(修「歌放完了却以为还在播」)。绝对态快照 → 幂等;
 *  只主窗发,悬浮窗自身调用是 no-op(它是镜像、不当真相源)。所有播放态切换都经此(play/暂停/
 *  ended/stop 的监听都调它),所以回报 core 一处接上即全覆盖。 */
function syncToPeers() {
  if (isFloat) return
  emitNowPlaying(state.current, state.status)
  reportToCore()
}

function stopElements() {
  if (audio) {
    audio.pause()
    audio.removeAttribute('src')
  }
  destroyAdaptive() // 手写 MSE(本地自适应)先拆:停泵/停音频流/收 MediaSource
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
  state.loopMode = 'off' // 停了就归位(core 侧下次 play() 也会复位);随机随队列生灭
  state.shuffle = false
  syncLoopToEl()
  syncToPeers() // 广播"停了"给悬浮窗(修:UI 点停止 / 自然播完时它仍显在放)
}

/** 一集放完:「接下来放什么」归 core(auto_next:顺序下一集 / 列表循环回卷 / 随机挑;
 *  true=已接管,core 现取现播 publishes Play 接力、保持全屏;false=没有下一首 → 正常停)。
 *  单曲循环由 el.loop 原生循环,ended 压根不触发。只在主窗触发(悬浮窗不实际播放、不会冒 ended)。 */
function onEnded() {
  if (state.current?.playlist && isTauri()) {
    state.status = 'loading' // 续播解析的空档显 spinner(别看着像卡死)
    api
      .mediaAutoNext()
      .then((took) => {
        if (!took) stop() // 放完了(末集且不循环 / 随机放完一轮):正常收尾
      })
      .catch(() => stop()) // 切集失败兜底停
    return
  }
  stop() // 单集(el.loop 没开才会走到 ended)/ 浏览器预览:正常收尾
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
    state.position = seconds // 先置位再回报(timeupdate 还没来,别把旧位置报出去)
    reportToCore()
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
  reportToCore() // 跳转后位置基准变了,立刻校准 core 的「此刻」进度
}

/** 播放条循环按钮:关 → 列表循环 → 单曲循环 → 关。经壳层命令走 core(校验/落状态/广播),
 *  与嘴控同一执行口;浏览器预览无 core,本地生效纯看视觉。 */
function cycleLoop() {
  const next =
    state.loopMode === 'off' ? 'loop_all' : state.loopMode === 'all' ? 'loop_one' : 'loop_off'
  if (isTauri()) void api.mediaMode(next).catch(() => {})
  else applyControl(next)
}

/** 播放条随机按钮(多集队列才显示)。 */
function toggleShuffle() {
  const next = state.shuffle ? 'shuffle_off' : 'shuffle_on'
  if (isTauri()) void api.mediaMode(next).catch(() => {})
  else applyControl(next)
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
  else if (action === 'volume' && value != null) setVolume(value / 100) // 绝对音量(core 已校验 0–100)
  else if (action === 'speed' && value != null) setRate(value)
  else if (action === 'seek' && value != null) seek(value)
  else if (action === 'loop_one' || action === 'loop_all' || action === 'loop_off') {
    // 循环/随机:core 已先落状态(嘴控/按钮同一口),这里对齐镜像 + 播放元素
    state.loopMode = action === 'loop_one' ? 'one' : action === 'loop_all' ? 'all' : 'off'
    syncLoopToEl()
  } else if (action === 'shuffle_on' || action === 'shuffle_off') {
    state.shuffle = action === 'shuffle_on'
  } else if (action === 'audio_track' && value != null) {
    // mac 直传切音轨:core 已落状态并发来事件,这里就地启停(无缝);管线路不经这(core 重发 Play)
    if (state.current) state.current.audio_track = Math.max(0, Math.round(value) - 1)
    const el = activeEl()
    if (el) applyAudioTrackToEl(el)
  }
}

/** 音轨按钮:循环切到下一条(1 起的轨号交 core;mac 直传就地启停、其余重建管线原位续播)。 */
function cycleAudioTrack() {
  const total = state.current?.audio_tracks?.length ?? 0
  if (total < 2) return
  const next = (((state.current?.audio_track ?? 0) + 1) % total) + 1
  if (isTauri()) void api.mediaMode('audio_track', next).catch(() => {})
  else applyControl('audio_track', next) // 浏览器预览:本地镜像纯看视觉
}

/** 音轨显示名:元数据标题 > 语言码的词典名(没词条就显原码)> 「音轨 N」。 */
function audioTrackLabel(i: number): string {
  const tr = state.current?.audio_tracks?.[i]
  if (tr?.title) return tr.title
  if (tr?.lang) {
    const key = `media.lang.${tr.lang}`
    return i18n.global.te(key) ? i18n.global.t(key) : tr.lang
  }
  return i18n.global.t('media.trackN', { n: i + 1 })
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
      // 播放中低频心跳回报:core 的进度外推兜不住缓冲卡顿的漂移,15s 校准一次封顶误差
      //(暂停/空闲不报 —— 状态切换本就各报一次,静止时没有会漂的东西)
      setInterval(() => {
        if (state.status === 'playing') reportToCore()
      }, 15_000)
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
      route: 'hls_transcode', // 预览:本地不兼容片今天走转码(0.2.6 copy 切片落地后会是 'hls_copy')
      page_url: '#',
      source: 'local',
      playlist: { index: 2, total: 12, resumed: false },
      audio_tracks: [
        { codec: 'ac-3', lang: 'chi' },
        { codec: 'ac-3', lang: 'eng' },
      ],
      audio_track: 0,
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
      audio_tracks: [
        { codec: 'mp4a', lang: 'chi', title: '普通话' },
        { codec: 'mp4a', lang: 'yue' },
      ],
      audio_track: 0,
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
    setDucked,
    setRate,
    next: () => advance(1),
    prev: () => advance(-1),
    cycleLoop,
    toggleShuffle,
    cycleAudioTrack,
    audioTrackLabel,
    loginNow,
    dismissLoginHint,
  }
}
