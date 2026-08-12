// 歌词滚动(播放条上方当前句):解析 NowPlaying 捎来的 .lrc 原文,按播放位置给出当前句。
// 只吃带时间轴的行([mm:ss.xx],同一行多标签 = 重复句);纯文本 .lrc 解析出 0 行 = 不显示
// ——一行 UI 放不下整段,而编造时间轴让它乱滚比不显示更糟(lyrics_fetch 同一条纪律)。
// [offset:±ms] 标签按「正值 = 歌词提前」惯例整体平移。
import { computed, type Ref } from 'vue'

export interface LrcLine {
  /** 出现时刻(秒,已含 offset)。 */
  t: number
  text: string
}

export function parseLrc(raw: string): LrcLine[] {
  let offsetMs = 0
  for (const row of raw.split(/\r?\n/)) {
    const om = /^\s*\[offset:\s*([+-]?\d+)\s*\]/i.exec(row)
    if (om) offsetMs = Number(om[1]) || 0
  }
  const out: LrcLine[] = []
  for (const row of raw.split(/\r?\n/)) {
    const tags = [...row.matchAll(/\[(\d{1,3}):(\d{1,2})(?:[.:](\d{1,3}))?\]/g)]
    if (!tags.length) continue
    const text = row.replace(/\[[^\]]*\]/g, '').trim()
    if (!text) continue
    for (const m of tags) {
      // 小数段 2 位是百分秒、3 位是毫秒 → 补齐 3 位统一按毫秒算
      const frac = m[3] ? Number(m[3].padEnd(3, '0').slice(0, 3)) / 1000 : 0
      const t = Number(m[1]) * 60 + Number(m[2]) + frac - offsetMs / 1000
      if (t >= 0) out.push({ t, text })
    }
  }
  return out.sort((a, b) => a.t - b.t)
}

/** 当前句 = 最后一条 t ≤ 位置(+0.2s 提前量:口型对不齐时宁早勿晚)。还没到第一句 = 空。 */
export function lineAt(lines: LrcLine[], position: number): string {
  const pos = position + 0.2
  let cur = ''
  for (const l of lines) {
    if (l.t <= pos) cur = l.text
    else break
  }
  return cur
}

export function useLyrics(lyricsRaw: Ref<string | undefined>, position: Ref<number>) {
  const lines = computed(() => (lyricsRaw.value ? parseLrc(lyricsRaw.value) : []))
  /** 有带时间轴的词才算「有歌词」(按钮与显示都看它)。 */
  const available = computed(() => lines.value.length > 0)
  const current = computed(() => (available.value ? lineAt(lines.value, position.value) : ''))
  return { available, current }
}
