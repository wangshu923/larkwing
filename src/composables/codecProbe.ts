// 解码能力探测(P1):boot 时问这台机器的 WebView「你到底解得动什么」,把答案告诉 core。
//
// **为什么要探**:core 原先靠两张硬编码白名单猜哪些编码不行(还带一套 `cfg!(target_os)` 的
// mac/Windows 分叉),猜错两个方向都疼 —— 猜「不行」就白重编一遍(掉画质、烧 CPU),猜「行」
// 就黑屏/无声。浏览器自己知道答案,问它即可,顺带白拿三样:装了 HEVC 扩展的机器直接 copy 原轨、
// WebView 升级带来的新解码能力自动吃到、mac 与 Windows 不再需要两套编译期矩阵。
//
// **两路分开探**:直传(`<video src>`)问 `canPlayType`,管线里能不能 copy 问
// `MediaSource.isTypeSupported` —— 同一台机器上两者真的会不一样(mac 的 WKWebView 直传能放
// AC3/HEVC〔系统解码器〕,MSE 未必)。core 那边也按两路存。

import { api, type MediaCodecs } from '../lib/backend'

/** 探测矩阵:归一编码名 → 一个有代表性的 mime 串。
 *  管线产出的永远是 fMP4,故一律用 mp4 容器问(mkv 里的 H.264 到了我们手上也是 fMP4)。
 *  **加一行 = 多探一个编码**,core 侧 `normalize_codec` 认得的名字都可以加。 */
const MATRIX: ReadonlyArray<readonly [string, string]> = [
  // 视频
  ['h264', 'video/mp4; codecs="avc1.640028"'],
  ['hevc', 'video/mp4; codecs="hvc1.1.6.L93.B0"'],
  ['av1', 'video/mp4; codecs="av01.0.05M.08"'],
  ['vp9', 'video/mp4; codecs="vp09.00.10.08"'],
  ['dolbyvision', 'video/mp4; codecs="dvh1.05.06"'],
  // 音频
  ['aac', 'audio/mp4; codecs="mp4a.40.2"'],
  ['ac3', 'audio/mp4; codecs="ac-3"'],
  ['eac3', 'audio/mp4; codecs="ec-3"'],
  ['dts', 'audio/mp4; codecs="dtsc"'],
  ['truehd', 'audio/mp4; codecs="mlpa"'],
  ['alac', 'audio/mp4; codecs="alac"'],
  ['flac', 'audio/mp4; codecs="flac"'],
  ['opus', 'audio/mp4; codecs="opus"'],
  ['mp3', 'audio/mpeg'],
]

/** 探一遍矩阵。纯函数(只读浏览器 API、不发 IPC),可在预览里直接跑着看。 */
export function probeCodecs(): MediaCodecs {
  const probed: string[] = []
  const direct: string[] = []
  const mse: string[] = []
  // 探测用的 <video> 只创建一次、不入 DOM(canPlayType 不需要它挂载)。
  const el = document.createElement('video')
  const mseOk = typeof MediaSource !== 'undefined' && typeof MediaSource.isTypeSupported === 'function'
  for (const [name, mime] of MATRIX) {
    // 任一路问出了结果就算「探过」;两路都问不了(极老 WebView)= 不记,core 那边照旧回落白名单。
    let asked = false
    try {
      // canPlayType 回 '' | 'maybe' | 'probably';非空即认为放得了(maybe 也照收——
      // 宁可试着直传,失败还有前端兜底重放,总好过白转一遍码)。
      if (el.canPlayType(mime) !== '') direct.push(name)
      asked = true
    } catch {
      /* 老 WebView 可能抛,当没探过 */
    }
    try {
      if (mseOk) {
        if (MediaSource.isTypeSupported(mime)) mse.push(name)
        asked = true
      }
    } catch {
      /* 同上 */
    }
    if (asked) probed.push(name)
  }
  return { probed, direct, mse }
}

/** 探完报给 core。失败静默(core 没收到 = 回落白名单 = 从前的行为,不该因此挡住任何播放)。 */
export function reportCodecs(): void {
  try {
    api.setMediaCodecs(probeCodecs())
  } catch {
    /* 探测本身炸了也不能影响 boot */
  }
}
