// 本地「音视频分离」自适应播放的手写 MSE 播放器(0.2.6 治本)。
//
// 背景:本地不兼容片(mp4+AC3、HEVC…)原走 muxed HLS —— 逐段独立转 AAC,每段带编码器 priming,
// 被钉到固定时间网格 → MSE 丢重叠 → **音频越放越快(漂移)**(实测每 6s 快 ~80ms)。治法 = 音视频
// 分离:视频按需分段(兼容→ `-c:v copy` 省 CPU,否则转码)、音频**一整条连续编码**(无逐段 priming
// → 结构性无漂移)。shaka 只吃「段」、喂不了「一条连续流」,故这条路绕开 shaka、自己开两条 SourceBuffer:
//   · 视频 SB:按播放进度**按需 append 段**(领先 AHEAD 秒),落后 BEHIND 秒的段驱逐,控住 MSE 配额。
//   · 音频 SB:一条渐进流(`/la/{t}/audio`)边下边 append;seek 跨出已缓冲区就带 `?t=` 重起 + timestampOffset。
// 已在预览浏览器(= WebView2 同 Chromium/MSE 内核)裸 MSE 验通:两轨拼播 + seek + 无漂移。
//
// 任何致命错误(setUp/append/解析)→ 调 `onError`,useMedia 据此回落(后端注册时也已做前提兜底)。

import { api, isTauri } from '../lib/backend'

/** 诊断日志 → larkwing.log(`media_log` 桥;正式版无 JS console,0.2.6 定位黑屏就靠它)。 */
function dlog(msg: string) {
  if (isTauri()) void api.mediaLog('[adaptive] ' + msg).catch(() => {})
}

/** 后端 `/la/{token}/desc` 的 JSON。 */
interface AdaptiveDesc {
  videoMime: string
  audioMime: string
  duration: number
  copyVideo: boolean
  audioSeg: number // 音频段时长(固定网格,秒)
  audioPreroll: number // 音频段左预卷(秒),前端 appendWindow 裁掉它连同 priming
  segments: { start: number; dur: number }[]
}

/** manifest_url 是不是本地自适应路(据 relay 路径判,和 shaka 的 /dash//hls/ 区分)。 */
export function isAdaptiveUrl(url?: string): boolean {
  return !!url && url.includes('/la/')
}

/** 手写 MSE 播放的控制器句柄:seek 交给原生 <video>.currentTime;stop() 拆干净。 */
export interface AdaptiveController {
  stop: () => void
}

const AHEAD = 24 // 视频缓冲领先秒数(够顺 + 不撑爆配额)
const BEHIND = 12 // 落后多少秒开始驱逐旧段
const AUDIO_AHEAD = 30 // 音频段缓冲领先秒数(领先够了就先不喂下一段)
const AUDIO_BEHIND = 12 // 音频落后多少秒开始驱逐(控 MSE 配额)

/** 起播:把 `el` 接管为手写 MSE 播放 descUrl 的自适应流。异步返回控制器;失败 reject(调用方回落)。 */
export async function playAdaptive(
  el: HTMLVideoElement,
  descUrl: string,
  onError: (why: string) => void,
  startAt = 0,
): Promise<AdaptiveController> {
  const base = descUrl.replace(/\/desc$/, '')
  const desc: AdaptiveDesc = await fetch(descUrl).then((r) => {
    if (!r.ok) throw new Error('desc ' + r.status)
    return r.json()
  })
  if (!MediaSource.isTypeSupported(desc.videoMime) || !MediaSource.isTypeSupported(desc.audioMime)) {
    throw new Error('codec unsupported: ' + desc.videoMime + ' / ' + desc.audioMime)
  }

  const ms = new MediaSource()
  el.src = URL.createObjectURL(ms)
  let stopped = false
  let vsb: SourceBuffer | null = null
  let asb: SourceBuffer | null = null
  let dead = false
  const ranges = (sb: SourceBuffer | null): string => {
    try {
      if (!sb) return '-'
      const b = sb.buffered
      const o: string[] = []
      for (let i = 0; i < b.length; i++) o.push(`${b.start(i).toFixed(1)}-${b.end(i).toFixed(1)}`)
      return o.join(',') || '空'
    } catch {
      return '?'
    }
  }
  const fail = (why: string) => {
    if (dead) return
    dead = true
    // 富化失败现场(t/readyState/两轨缓冲/video.error)→ onError → 写进 larkwing.log,真机可定位。
    const detail =
      `${why} | t=${el.currentTime.toFixed(1)} rs=${el.readyState}` +
      ` verr=${el.error?.code ?? '-'} vbuf=[${ranges(vsb)}] abuf=[${ranges(asb)}]`
    console.error('[lw][adaptive]', detail)
    onError(detail)
  }

  // 每条 SB 一条串行队列:MSE 不允许并发 append/remove(updating 时再操作会抛)。
  const mkQueue = (sb: SourceBuffer) => {
    let chain: Promise<void> = Promise.resolve()
    return (op: () => void) => {
      chain = chain.then(
        () =>
          new Promise<void>((res) => {
            if (stopped || ms.readyState !== 'open') return res()
            const cleanup = () => {
              sb.removeEventListener('updateend', onEnd)
              sb.removeEventListener('error', onErr)
              res()
            }
            const onEnd = () => cleanup()
            // SB 'error' 事件 = 真失败(坏数据/解码不了)→ 触发 fail(回落),不再静默吞掉。
            const onErr = () => {
              fail('sb error event')
              cleanup()
            }
            sb.addEventListener('updateend', onEnd, { once: true })
            sb.addEventListener('error', onErr, { once: true })
            try {
              op()
            } catch (e) {
              cleanup()
              // 配额已由有界缓冲防住;真抛异常(含意外 Quota)→ 回落。
              fail('sb op: ' + e)
            }
          }),
      )
      return chain
    }
  }
  let vq: (op: () => void) => Promise<void>
  let aq: (op: () => void) => Promise<void>
  const bufEnd = (sb: SourceBuffer, t: number): number => {
    // 覆盖 t 的已缓冲区间的末端(没覆盖到 → t 本身,表示这里之后没数据)。
    const b = sb.buffered
    for (let i = 0; i < b.length; i++) if (b.start(i) <= t + 0.1 && b.end(i) > t) return b.end(i)
    return t
  }
  const fetchBuf = (u: string) =>
    fetch(u).then((r) => {
      if (!r.ok) throw new Error(u + ' ' + r.status)
      return r.arrayBuffer()
    })

  // seek 代次:seek 时 +1,让 seek 前发出的在途请求(resolve 时)自弃,不把过期段 append 到错位。
  let gen = 0
  // 起播位(切音轨原位续播):段指针直指目标,**绝不从 0 爬灌** —— 否则续播 seek 和初始爬灌
  // 竞态(onSeeking 的重灌被 pumping 标志挡成 no-op),音频晚于起播才就位,而 WKWebView
  // 不接入「播放开始后才补进的音频」→ 无声(2026-07-22 真机定案);顺带 t=0 画面不再闪现。
  const start = Math.max(0, Math.min(startAt, Math.max(0, desc.duration - 1)))
  const vsegAt = (t: number): number => {
    const i = desc.segments.findIndex((sg) => sg.start <= t && sg.start + sg.dur > t)
    return i < 0 ? 0 : i
  }
  let nextSeg = start > 0 ? vsegAt(start) : 0 // 下一个待喂的**视频**段号(顺序推进;seek 重设)
  let nextAudioSeg = start > 0 ? Math.max(0, Math.floor(start / desc.audioSeg)) : 0 // 音频段号(固定 6s 网格)
  const audioSegCount = Math.max(1, Math.ceil(desc.duration / desc.audioSeg))
  let pumpingV = false
  let pumpingA = false
  let aDiag = 3 // 每会话头几次音频 append 打落点诊断(排「样本被窗裁光」类哑巴故障)
  // 音频先行(2026-07-22 真机定案):用户系统的 WKWebView 会**仅凭视频完成 seek/canplay**,
  // 之后补进的音频样本被静默丢弃(append 成功、回读正确、buffered 恒空;上游 Playwright WebKit
  // 无此行为,seek 会等双轨)。治法 = 播放头处音频未落桶前,视频段暂缓喂入 —— seek 只能在
  // 双轨齐备后完成,迟到音频从结构上不存在。1.5s 超时放行防饿死(音频源故障时宁可无声别卡死)。
  let audioFirstDeadline = performance.now() + 1500
  const audioCovers = (t: number): boolean => {
    try {
      const b = asb!.buffered
      for (let i = 0; i < b.length; i++) {
        if (b.start(i) <= t + 0.3 && b.end(i) > t) return true
      }
    } catch {
      /* 当没货 */
    }
    return false
  }

  // 视频按需泵:保证 [now−BEHIND, now+AHEAD] 有段;落后的驱逐。串行、无并发 append。
  const pumpVideo = async () => {
    if (pumpingV || stopped || dead || !vsb || ms.readyState !== 'open') return
    // 音频先行:播放头处音频还没落桶 → 视频缓一拍再来(80ms 轮询,超时放行)
    if (asb && !audioCovers(el.currentTime) && performance.now() < audioFirstDeadline) {
      setTimeout(() => void pumpVideo(), 80)
      return
    }
    pumpingV = true
    const entryGenV = gen
    try {
      const now = el.currentTime
      const b = vsb.buffered
      if (b.length && b.start(0) < now - BEHIND && !vsb.updating) {
        await vq(() => vsb!.remove(0, now - BEHIND))
      }
      while (
        !stopped &&
        !dead &&
        nextSeg < desc.segments.length &&
        desc.segments[nextSeg].start < now + AHEAD
      ) {
        const i = nextSeg
        nextSeg++
        const myGen = gen
        const buf = await fetchBuf(base + '/v' + i)
        if (stopped || dead || ms.readyState !== 'open' || gen !== myGen) break
        await vq(() => vsb!.appendBuffer(new Uint8Array(buf)))
      }
    } catch (e) {
      if (!stopped) fail('video pump: ' + e)
    } finally {
      pumpingV = false
      // 本轮跑动中被 seek 换代:指针已被 onSeeking 重设,立刻按新指针再泵一轮
      //(否则 onSeeking 的 pump() 被 pumping 标志挡掉后,要等下一个 timeupdate 才有人接手)。
      if (!stopped && !dead && gen !== entryGenV) void pumpVideo()
    }
  }

  // 音频按需泵:**离散段**(不流式,WebView2 收不下流式 body),固定 6s 网格。每段带左预卷,靠
  // `appendWindow` 裁到 [grid, grid+seg] —— 连同 AAC 逐段 priming 一起裁掉 → gapless、无累计漂移。
  // `timestampOffset` 把段内(tfdt=0)内容放到真时间轴。领先 AUDIO_AHEAD 停喂、落后 AUDIO_BEHIND 驱逐。
  const pumpAudio = async () => {
    if (pumpingA || stopped || dead || !asb || ms.readyState !== 'open') return
    pumpingA = true
    const entryGenA = gen
    try {
      const now = el.currentTime
      const b = asb.buffered
      if (b.length && b.start(0) < now - AUDIO_BEHIND && !asb.updating) {
        await aq(() => asb!.remove(0, now - AUDIO_BEHIND))
      }
      while (
        !stopped &&
        !dead &&
        nextAudioSeg < audioSegCount &&
        nextAudioSeg * desc.audioSeg < now + AUDIO_AHEAD
      ) {
        const n = nextAudioSeg
        nextAudioSeg++
        const myGen = gen
        const grid = n * desc.audioSeg
        const buf = await fetchBuf(base + '/a' + n)
        if (stopped || dead || ms.readyState !== 'open' || gen !== myGen) break
        await aq(() => {
          // 顺序要紧:先设 End(更大),再设 Start(< End),否则 appendWindowStart>End 会抛。
          asb!.appendWindowEnd = Math.min(desc.duration, grid + desc.audioSeg)
          asb!.appendWindowStart = grid
          asb!.timestampOffset = n > 0 ? grid - desc.audioPreroll : 0
          asb!.appendBuffer(new Uint8Array(buf))
        })
        // 前几段的落点诊断:append 后缓冲没长 = 样本被 appendWindow 裁光(时间戳错位的指纹)
        if (aDiag > 0) {
          aDiag--
          // 回读三件套:tsOff/win 若与设置值不符(比如回读为 0)= WebKit 静默无视了 setter,
          // 样本按原始 0~6.5s 落点被窗整段裁光 —— 「append 成功缓冲为空」的最后嫌疑。
          dlog(
            `a${n} appended ${buf.byteLength}B → abuf=[${ranges(asb)}] ` +
              `tsOff=${asb!.timestampOffset.toFixed(1)} win=[${asb!.appendWindowStart.toFixed(1)},${asb!.appendWindowEnd === Infinity ? 'inf' : asb!.appendWindowEnd.toFixed(1)}]`,
          )
        }
      }
    } catch (e) {
      if (!stopped) fail('audio pump: ' + e)
    } finally {
      pumpingA = false
      if (!stopped && !dead && gen !== entryGenA) void pumpAudio()
    }
  }

  const pump = () => {
    void pumpVideo()
    void pumpAudio()
  }

  // seek:两轨目标都已缓冲 → 原生跳;否则 seek 代次 +1(弃在途)、重设两轨段指针、清缓冲、重泵。
  const onSeeking = () => {
    if (stopped || dead || !vsb || !asb) return
    const t = el.currentTime
    if (bufEnd(vsb, t) > t + 0.3 && bufEnd(asb, t) > t + 0.3) return // 命中缓冲,原生 seek 即可
    gen++
    audioFirstDeadline = performance.now() + 1500 // 新目标位:音频重新先行
    let vseg = desc.segments.findIndex((s) => s.start <= t && s.start + s.dur > t)
    if (vseg < 0) vseg = Math.max(0, desc.segments.length - 1)
    nextSeg = vseg
    nextAudioSeg = Math.max(0, Math.floor(t / desc.audioSeg))
    void (async () => {
      try {
        if (vsb!.buffered.length && ms.readyState === 'open') await vq(() => vsb!.remove(0, Infinity))
        if (asb!.buffered.length && ms.readyState === 'open') await aq(() => asb!.remove(0, Infinity))
      } catch {
        /* ignore */
      }
      pump()
    })()
  }

  return await new Promise<AdaptiveController>((resolve, reject) => {
    const onOpen = async () => {
      ms.removeEventListener('sourceopen', onOpen)
      try {
        vsb = ms.addSourceBuffer(desc.videoMime)
        asb = ms.addSourceBuffer(desc.audioMime)
        ms.duration = desc.duration // 显式设时长 → <video>.duration 正确(进度条/seek 靠它,别被 MSE 缓冲末端覆盖)
      } catch (e) {
        reject(new Error('addSourceBuffer: ' + e))
        return
      }
      vq = mkQueue(vsb)
      aq = mkQueue(asb)
      dlog(`sourceopen ok copy=${desc.copyVideo} startAt=${start.toFixed(1)} vmime=${desc.videoMime}`)
      // 先喂两轨 init,再泵段(视频 + 音频离散段)。
      try {
        const [vinit, ainit] = await Promise.all([
          fetchBuf(base + '/vinit'),
          fetchBuf(base + '/ainit'),
        ])
        await vq(() => vsb!.appendBuffer(new Uint8Array(vinit)))
        await aq(() => asb!.appendBuffer(new Uint8Array(ainit)))
        dlog(`init appended v=${vinit.byteLength}B a=${ainit.byteLength}B`)
      } catch (e) {
        reject(new Error('init: ' + e))
        return
      }
      // 启动定位放在挂 onSeeking **之前**:段指针本就初始化在起播位,这记 seek 不该换代
      // (换代会作废首轮泵的成果、多跑一轮 → 音频晚到,输给 canplay)。
      if (start > 0) {
        try {
          el.currentTime = start
        } catch {
          el.addEventListener('loadedmetadata', () => (el.currentTime = start), { once: true })
        }
      }
      el.addEventListener('timeupdate', pump)
      el.addEventListener('seeking', onSeeking)
      // 生命周期诊断(一次性;真机绿屏定位用——上回停滞时这中间一片静默,只能瞎猜)
      for (const ev of ['loadedmetadata', 'canplay', 'playing'] as const) {
        el.addEventListener(
          ev,
          () => dlog(`${ev} rs=${el.readyState} ct=${el.currentTime.toFixed(2)}`),
          { once: true },
        )
      }
      el.addEventListener(
        'error',
        () => dlog(`element error code=${el.error?.code} msg=${el.error?.message ?? ''}`),
        { once: true },
      )
      pump() // 首泵:两轨段从起播位喂起(无 startAt 时即 t=0)
      // 起播闸:**两轨在起播位都有货才 play()**。WebKit 不接入「播放开始后才补进的音频」——
      // 视频段 copy 极快,canplay 抢跑后音频再 append 就永远无声(2026-07-22 真机 5/5 会话
      // 相关性;Chromium harness 证明字节与窗口数学无罪)。4s 超时兜底照常放行(宁可无声也
      // 别卡死,看门狗还在后面)。
      const covered = (sb: SourceBuffer | null, t: number): boolean => {
        try {
          const b = sb!.buffered
          for (let i = 0; i < b.length; i++) {
            if (b.start(i) <= t + 0.3 && b.end(i) > t + 0.1) return true
          }
        } catch {
          /* buffered 读取竞态,当没货 */
        }
        return false
      }
      const doPlay = () => {
        el.play()
          .then(() => dlog(`play() ok rs=${el.readyState}`))
          .catch((e) => dlog(`play() rejected: ${e} rs=${el.readyState}`))
      }
      const gateT0 = performance.now()
      const gate = window.setInterval(() => {
        if (stopped || dead) {
          clearInterval(gate)
          return
        }
        const ok = covered(vsb, start) && covered(asb, start)
        if (ok || performance.now() - gateT0 > 4000) {
          clearInterval(gate)
          dlog(`起播闸${ok ? '开' : '超时'} v=${covered(vsb, start)} a=${covered(asb, start)}`)
          doPlay()
        }
      }, 50)
      // 最小版 gap-jump(2026-07-22 真机定案的 WebKit 起播卡点):B 帧片源首个视频样本
      // PTS≈0.1s → 缓冲从 0.1 起,播放头停在 0.0;Chromium 自己就近起播,WebKit 严格等
      // 「正好覆盖播放头」的样本 → readyState 永卡 1(绿屏/12s 看门狗回落)。shaka 内建
      // gap-jumping 治的就是它(muxed 路经 shaka 没事的原因)。低频巡逻:播放头落在某段
      // 视频缓冲起点前不到半秒且没起来 → 挪进去补一脚 play()。起播与 seek 落点都覆盖。
      const nudge = window.setInterval(() => {
        if (stopped || dead) {
          clearInterval(nudge)
          return
        }
        try {
          const b = vsb!.buffered
          for (let i = 0; i < b.length; i++) {
            const s = b.start(i)
            if (el.readyState < 3 && el.currentTime < s && s - el.currentTime < 0.5) {
              dlog(`gap-jump: ${el.currentTime.toFixed(3)} → ${s.toFixed(3)} rs=${el.readyState}`)
              el.currentTime = s + 0.001
              void el.play().catch(() => {})
              break
            }
          }
        } catch {
          /* buffered 读取竞态,忽略这一拍 */
        }
      }, 500)
      // 5s 快照:两轨缓冲区间 + readyState 一行看全(停滞时分辨「没喂进数据」还是「喂了解不动」)
      const snap = window.setTimeout(() => {
        const rng = (sb: SourceBuffer | null) => {
          try {
            const b = sb?.buffered
            return b && b.length ? `${b.start(0).toFixed(1)}~${b.end(b.length - 1).toFixed(1)}` : '空'
          } catch {
            return '?'
          }
        }
        dlog(
          `5s 快照 rs=${el.readyState} ct=${el.currentTime.toFixed(2)} paused=${el.paused} ` +
            `v=[${rng(vsb)}] a=[${rng(asb)}] err=${el.error?.message ?? '无'}`,
        )
      }, 5000)
      // 停滞看门狗:MSE 有时既不报错也不出画(静默黑屏)。12s 后若仍卡在起点 → 判失败,
      // 触发 onError → useMedia 兜底回落 muxed HLS(能放的老路)。宽松阈值防弱机首段慢误杀。
      // 两种卡法都要兜:!paused = 播着却不走;readyState<2 = 连首帧都没解出来 —— WKWebView 上
      // play() 被 MSE 拒掉会把 paused 弹回 true,原先只查 !paused 就永远不触发(2026-07-21
      // Mac 绿屏卡死实锤)。用户在首帧后主动暂停 = paused 且 readyState≥2,不会误杀。
      const watchdog = window.setTimeout(() => {
        if (!stopped && !dead && el.currentTime < 0.3 && (!el.paused || el.readyState < 2)) {
          fail('stall: no progress in 12s')
        }
      }, 12000)
      resolve({
        stop: () => {
          stopped = true
          clearTimeout(watchdog)
          clearTimeout(snap)
          clearInterval(nudge)
          clearInterval(gate)
          el.removeEventListener('timeupdate', pump)
          el.removeEventListener('seeking', onSeeking)
          try {
            if (ms.readyState === 'open') ms.endOfStream()
          } catch {
            /* ignore */
          }
          try {
            URL.revokeObjectURL(el.src)
          } catch {
            /* ignore */
          }
        },
      })
    }
    ms.addEventListener('sourceopen', onOpen)
    ms.addEventListener('error', () => fail('mediasource error'))
    // 非 Tauri(浏览器预览)不会真走到这条路;保险起见给个兜底 reject 超时。
    if (!isTauri()) setTimeout(() => reject(new Error('not tauri')), 100)
  })
}
