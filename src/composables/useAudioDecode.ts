// 本地音频文件 → 16k 单声道 wav 的 base64(给 voice_clone_import 用)。
// 解码/重采样全在 WebView(Web Audio API,Mac WKWebView / Win WebView2 原生支持),
// 后端不引解码依赖、不依赖 ffmpeg。

/** 选中的音频文件 → 16k 单声道 wav 的 base64(无 data: 前缀)+ 时长秒。 */
export async function audioFileToWavBase64(
  file: File,
): Promise<{ base64: string; durationSec: number }> {
  const buf = await file.arrayBuffer()
  const Ctx = window.AudioContext || (window as unknown as { webkitAudioContext: typeof AudioContext }).webkitAudioContext
  const ac = new Ctx()
  let decoded: AudioBuffer
  try {
    decoded = await ac.decodeAudioData(buf)
  } finally {
    void ac.close?.()
  }
  const mono = toMono(decoded)
  const pcm = decoded.sampleRate === 16000 ? mono : await resampleTo16k(mono, decoded.sampleRate)
  const wav = encodeWav16(pcm, 16000)
  return { base64: bytesToBase64(new Uint8Array(wav)), durationSec: decoded.duration }
}

/** 多声道 → 单声道(等权混音)。 */
function toMono(b: AudioBuffer): Float32Array {
  if (b.numberOfChannels === 1) return b.getChannelData(0).slice()
  const n = b.length
  const out = new Float32Array(n)
  for (let c = 0; c < b.numberOfChannels; c++) {
    const d = b.getChannelData(c)
    for (let i = 0; i < n; i++) out[i] += d[i] / b.numberOfChannels
  }
  return out
}

/** OfflineAudioContext 重采样到 16k(带抗混叠,优于手写线性插值)。 */
async function resampleTo16k(mono: Float32Array, srcRate: number): Promise<Float32Array> {
  const len = Math.max(1, Math.round((mono.length / srcRate) * 16000))
  const Off =
    window.OfflineAudioContext ||
    (window as unknown as { webkitOfflineAudioContext: typeof OfflineAudioContext }).webkitOfflineAudioContext
  const oac = new Off(1, len, 16000)
  const src = oac.createBuffer(1, mono.length, srcRate)
  src.getChannelData(0).set(mono)
  const node = oac.createBufferSource()
  node.buffer = src
  node.connect(oac.destination)
  node.start()
  const rendered = await oac.startRendering()
  return rendered.getChannelData(0).slice()
}

/** f32 PCM([-1,1]) → 16-bit mono WAV(44 字节头),与后端 pcm_f32_to_wav 对齐。 */
function encodeWav16(pcm: Float32Array, rate: number): ArrayBuffer {
  const dataLen = pcm.length * 2
  const buf = new ArrayBuffer(44 + dataLen)
  const v = new DataView(buf)
  const str = (o: number, s: string) => {
    for (let i = 0; i < s.length; i++) v.setUint8(o + i, s.charCodeAt(i))
  }
  str(0, 'RIFF')
  v.setUint32(4, 36 + dataLen, true)
  str(8, 'WAVE')
  str(12, 'fmt ')
  v.setUint32(16, 16, true)
  v.setUint16(20, 1, true) // PCM
  v.setUint16(22, 1, true) // mono
  v.setUint32(24, rate, true)
  v.setUint32(28, rate * 2, true) // byte rate
  v.setUint16(32, 2, true) // block align
  v.setUint16(34, 16, true) // bits/sample
  str(36, 'data')
  v.setUint32(40, dataLen, true)
  let o = 44
  for (let i = 0; i < pcm.length; i++, o += 2) {
    const s = Math.max(-1, Math.min(1, pcm[i]))
    v.setInt16(o, s < 0 ? s * 0x8000 : s * 0x7fff, true)
  }
  return buf
}

/** 大数组安全的 base64(分块,避免 String.fromCharCode(...huge) 爆栈)。 */
function bytesToBase64(bytes: Uint8Array): string {
  let bin = ''
  const CHUNK = 0x8000
  for (let i = 0; i < bytes.length; i += CHUNK) {
    bin += String.fromCharCode(...bytes.subarray(i, i + CHUNK))
  }
  return btoa(bin)
}
