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

import { isTauri } from '../lib/backend'

/** 后端 `/la/{token}/desc` 的 JSON。 */
interface AdaptiveDesc {
  videoMime: string
  audioMime: string
  duration: number
  copyVideo: boolean
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

/** 起播:把 `el` 接管为手写 MSE 播放 descUrl 的自适应流。异步返回控制器;失败 reject(调用方回落)。 */
export async function playAdaptive(
  el: HTMLVideoElement,
  descUrl: string,
  onError: (why: string) => void,
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
  let audioAbort: AbortController | null = null
  let dead = false
  const fail = (why: string) => {
    if (dead) return
    dead = true
    console.error('[lw][adaptive]', why, '| video.error=', el.error?.message)
    onError(why)
  }

  // 每条 SB 一条串行队列:MSE 不允许并发 append/remove(updating 时再操作会抛)。
  const mkQueue = (sb: SourceBuffer) => {
    let chain: Promise<void> = Promise.resolve()
    return (op: () => void) => {
      chain = chain.then(
        () =>
          new Promise<void>((res) => {
            if (stopped || ms.readyState !== 'open') return res()
            const done = () => {
              sb.removeEventListener('updateend', done)
              sb.removeEventListener('error', done)
              res()
            }
            sb.addEventListener('updateend', done, { once: true })
            sb.addEventListener('error', done, { once: true })
            try {
              op()
            } catch (e) {
              done()
              fail('sb op: ' + e)
            }
          }),
      )
      return chain
    }
  }
  let vq: (op: () => void) => Promise<void>
  const bufEnd = (sb: SourceBuffer, t: number): number => {
    // 覆盖 t 的已缓冲区间的末端(没覆盖到 → t 本身,表示这里之后没数据)。
    const b = sb.buffered
    for (let i = 0; i < b.length; i++) if (b.start(i) <= t + 0.1 && b.end(i) > t) return b.end(i)
    return t
  }
  const fetchBuf = (u: string, signal?: AbortSignal) =>
    fetch(u, signal ? { signal } : undefined).then((r) => {
      if (!r.ok) throw new Error(u + ' ' + r.status)
      return r.arrayBuffer()
    })

  // 视频按需泵:保证 [now-BEHIND, now+AHEAD] 有段;落后的驱逐。串行、无并发 append。
  let nextSeg = 0 // 下一个待 append 的段号(顺序推进;seek 会重设)
  let pumping = false
  const pump = async () => {
    if (pumping || stopped || dead || !vsb || ms.readyState !== 'open') return
    pumping = true
    try {
      const now = el.currentTime
      // 驱逐:落后 BEHIND 秒以上的已缓冲头部(降配额压力,避免 QuotaExceeded)。
      const b = vsb.buffered
      if (b.length && b.start(0) < now - BEHIND && !vsb.updating) {
        await vq(() => vsb!.remove(0, now - BEHIND))
      }
      // 领先不足 AHEAD 且还有段没喂 → 喂下一个(每次一个,timeupdate 再来接着喂)。
      while (
        !stopped &&
        !dead &&
        nextSeg < desc.segments.length &&
        desc.segments[nextSeg].start < now + AHEAD
      ) {
        const i = nextSeg
        nextSeg++
        const buf = await fetchBuf(base + '/v' + i)
        if (stopped || dead || ms.readyState !== 'open') break
        await vq(() => vsb!.appendBuffer(new Uint8Array(buf)))
      }
    } catch (e) {
      if (!stopped) fail('video pump: ' + e)
    } finally {
      pumping = false
    }
  }

  // 音频:一条渐进流边下边喂(可从 fromSec 起,配 timestampOffset 落到正确时间轴)。
  const startAudio = (fromSec: number) => {
    if (!asb || stopped) return
    audioAbort?.abort()
    audioAbort = new AbortController()
    const signal = audioAbort.signal
    const aq = mkQueue(asb)
    aq(() => {
      if (asb!.timestampOffset !== fromSec) asb!.timestampOffset = fromSec
    })
    ;(async () => {
      try {
        const resp = await fetch(base + '/audio' + (fromSec > 0 ? '?t=' + fromSec.toFixed(3) : ''), {
          signal,
        })
        if (!resp.ok || !resp.body) throw new Error('audio ' + resp.status)
        const reader = resp.body.getReader()
        for (;;) {
          const { done, value } = await reader.read()
          if (done || stopped || signal.aborted || ms.readyState !== 'open') break
          if (value && value.byteLength) await aq(() => asb!.appendBuffer(value))
        }
      } catch (e) {
        if (!stopped && !signal.aborted) fail('audio: ' + e)
      }
    })()
  }

  // seek:目标已缓冲(视频段 + 音频)→ 原生跳,啥都不做;否则重设视频段指针 + 重起音频。
  const onSeeking = () => {
    if (stopped || dead || !vsb || !asb) return
    const t = el.currentTime
    const vOk = bufEnd(vsb, t) > t + 0.3
    const aOk = bufEnd(asb, t) > t + 0.3
    if (vOk && aOk) return // 命中缓冲,原生 seek 即可
    // 视频:清空重来,指针定位到覆盖 t 的段(线性找,段数不多)。
    let seg = desc.segments.findIndex((s) => s.start <= t && s.start + s.dur > t)
    if (seg < 0) seg = Math.max(0, desc.segments.length - 1)
    nextSeg = seg
    const clearAndPump = async () => {
      try {
        if (vsb && vsb.buffered.length && ms.readyState === 'open') {
          await vq(() => vsb!.remove(0, Infinity))
        }
      } catch {
        /* ignore */
      }
      void pump()
    }
    void clearAndPump()
    if (!aOk) startAudio(desc.segments[seg]?.start ?? t) // 音频从段首连续起(与视频对齐)
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
      // 先喂视频 init,再泵段 + 起音频。
      try {
        const vinit = await fetchBuf(base + '/vinit')
        await vq(() => vsb!.appendBuffer(new Uint8Array(vinit)))
      } catch (e) {
        reject(new Error('vinit: ' + e))
        return
      }
      el.addEventListener('timeupdate', pump)
      el.addEventListener('seeking', onSeeking)
      startAudio(0)
      await pump()
      void el.play().catch(() => {})
      // 停滞看门狗:MSE 有时既不报错也不出画(静默黑屏)。12s 后若仍卡在起点(且非用户暂停)→
      // 判失败,触发 onError → useMedia 兜底回落 muxed HLS(能放的老路)。宽松阈值防弱机首段慢误杀。
      const watchdog = window.setTimeout(() => {
        if (!stopped && !dead && el.currentTime < 0.3 && !el.paused) fail('stall: no progress in 12s')
      }, 12000)
      resolve({
        stop: () => {
          stopped = true
          clearTimeout(watchdog)
          el.removeEventListener('timeupdate', pump)
          el.removeEventListener('seeking', onSeeking)
          audioAbort?.abort()
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
