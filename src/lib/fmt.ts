// 读数格式化(灯带 / 气泡 meta 共用):token 千分位缩写、小额美元、毫秒时长。

export function fmtTokens(n: number): string {
  if (n >= 1_000_000) return (n / 1_000_000).toFixed(n < 10_000_000 ? 1 : 0) + 'M'
  if (n >= 1_000) return (n / 1_000).toFixed(n < 10_000 ? 1 : 0) + 'K'
  return String(n)
}

/** 小额美元:量级自适应(牌价估算,调用方自己决定带不带 ≈)。 */
export function fmtUsd(v: number): string {
  if (v === 0) return '$0'
  if (v >= 1) return '$' + v.toFixed(2)
  if (v >= 0.01) return '$' + v.toFixed(3)
  return '$' + v.toFixed(4)
}

/** 时长:亚秒给毫秒,1 分钟内给一位小数秒,再往上给分+秒。 */
export function fmtMs(ms: number): string {
  if (ms < 1000) return Math.round(ms) + 'ms'
  if (ms < 60_000) return (ms / 1000).toFixed(1) + 's'
  return Math.floor(ms / 60_000) + 'm' + Math.round((ms % 60_000) / 1000) + 's'
}

/** 播放时钟:67 → "1:07",3723 → "1:02:03"。 */
export function fmtClock(seconds: number): string {
  const s = Math.max(0, Math.floor(seconds))
  const [h, m, sec] = [Math.floor(s / 3600), Math.floor((s % 3600) / 60), s % 60]
  const mm = h > 0 ? String(m).padStart(2, '0') : String(m)
  return (h > 0 ? h + ':' : '') + mm + ':' + String(sec).padStart(2, '0')
}
