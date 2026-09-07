// 播放快捷键表:视频浮层与音频播放条**共用一张表**(§4.11 用户拍板 2026-09-07 键位;音频面 2026-09-07
// 二批加入)。一张表当数据 —— 按键分发、帮助浮层(H)、按钮 tooltip 都从它生成,加键 = 加一行。
// 每行标明给哪个面:PageUp/Down、S、F 只给视频(音频面不占聊天翻页键,换曲用 N/P);R 只给音频
//(视频没有模式钮);C 在视频面是字幕、音频面是歌词开关。全部动作汇到与按钮 / 嘴控同一执行口
//(useMedia),这里不长第二套语义。
//
// 接管条件(两个面同一套):焦点不在文本输入框 / 可编辑区(打字优先,空格就是空格)、Alt 组合留给系统、
// 调用方 `blocked()` 没说让位(菜单开着 / 这个面不在前)。
import { computed } from 'vue'
import { useI18n } from 'vue-i18n'
import { SEEK_STEP_LONG_S, SEEK_STEP_S, type PlayStatus } from './useMedia'

export type KeyRow = {
  /** 显示用键名(帮助浮层 / tooltip)。 */
  keys: string[]
  /** 帮助浮层里的说明(i18n key)。 */
  label: string
  /** 命中判定:返回 true = 这条接管本次按键。 */
  match: (e: KeyboardEvent) => boolean
  run: (e: KeyboardEvent) => void
  /** 只在有对应能力时显示 / 生效(多集才有上下集,双语片才有音轨…)。 */
  when?: () => boolean
  /** 给哪个面(缺省 = 两个面都有)。 */
  surface?: 'video' | 'audio'
}

/** 各面提供的动作与读数;可选项缺省 = 这个面没有那一行。 */
export type MediaKeyActions = {
  surface: 'video' | 'audio'
  /** 本次按键该不该让位(菜单开着 / 面不在前 / 没在放)。 */
  blocked: () => boolean
  /** 按键后闪读数(各面自己决定 OSD 长哪、放哪)。 */
  osd: (text: string) => void
  status: () => PlayStatus
  playPause: () => void
  seekBy: (delta: number) => void
  volumePct: () => number
  volumeStep: (dir: 1 | -1) => void
  muted: () => boolean
  toggleMute: () => void
  rate: () => number
  stepRate: (dir: 1 | -1) => void
  hasPlaylist: () => boolean
  next: () => void
  prev: () => void
  /** 播放模式轮转 + 轮转后的模式名(音频面)。 */
  cycleMode?: () => string
  toggleList?: () => void
  /** 视频:跳过片头 / 打开标记菜单。 */
  skip?: () => void
  audioTrackCount: () => number
  cycleAudioTrack: () => void
  audioTrackLabel: () => string
  /** C 键:视频 = 字幕、音频 = 歌词开关;count 为 0 时这一行不出现。 */
  captionCount: () => number
  cycleCaption: () => void
  captionLabel: () => string
  toggleFullscreen?: () => void
  /** Esc:各面自己决定关什么(帮助 / 列表 / 全屏 / 把焦点还给输入框)。 */
  escape: () => void
  toggleHelp: () => void
}

/** ↑↓ 一步的音量(0–1 制)。 */
export const VOL_STEP = 0.1
const key = (k: string) => (e: KeyboardEvent) =>
  !e.shiftKey && !e.ctrlKey && !e.metaKey && (e.key === k || e.key === k.toUpperCase())
const shift = (k: string) => (e: KeyboardEvent) => e.shiftKey && e.key === k
const ctrl = (k: string) => (e: KeyboardEvent) => (e.ctrlKey || e.metaKey) && e.key === k

/** 正在真文本输入(输入框 / 可编辑区)→ 让位。滑杆(range)不算:拖完进度条焦点留在滑杆上,快捷键照常。 */
export function isTextTarget(e: KeyboardEvent): boolean {
  const tg = e.target as HTMLElement | null
  if (!tg) return false
  return (
    tg.isContentEditable ||
    (/^(INPUT|TEXTAREA|SELECT)$/.test(tg.tagName) && (tg as HTMLInputElement).type !== 'range')
  )
}

export function useMediaKeys(a: MediaKeyActions) {
  const { t } = useI18n()
  const video = a.surface === 'video'
  const sign = (d: number) => `${d > 0 ? '+' : '−'}${Math.abs(d)}`

  const all: KeyRow[] = [
    {
      keys: ['Space', 'K'],
      label: 'media.keys.playPause',
      match: (e) => key('k')(e) || e.key === ' ' || e.key === 'Spacebar',
      run: () => {
        a.playPause()
        a.osd(a.status() === 'playing' ? t('media.osd.pause') : t('media.osd.play'))
      },
    },
    {
      keys: ['←', '→'],
      label: 'media.keys.seek',
      match: (e) => !e.shiftKey && !e.ctrlKey && !e.metaKey && (e.key === 'ArrowLeft' || e.key === 'ArrowRight'),
      run: (e) => {
        const d = e.key === 'ArrowLeft' ? -SEEK_STEP_S : SEEK_STEP_S
        a.seekBy(d)
        a.osd(t('media.osd.seek', { s: sign(d) }))
      },
    },
    {
      keys: ['Shift+←', 'Shift+→'],
      label: 'media.keys.seekLong',
      match: (e) => shift('ArrowLeft')(e) || shift('ArrowRight')(e),
      run: (e) => {
        const d = e.key === 'ArrowLeft' ? -SEEK_STEP_LONG_S : SEEK_STEP_LONG_S
        a.seekBy(d)
        a.osd(t('media.osd.seek', { s: sign(d) }))
      },
    },
    {
      keys: ['↑', '↓'],
      label: 'media.keys.volume',
      match: (e) => !e.ctrlKey && !e.metaKey && (e.key === 'ArrowUp' || e.key === 'ArrowDown'),
      run: (e) => {
        a.volumeStep(e.key === 'ArrowUp' ? 1 : -1)
        a.osd(t('media.osd.volume', { pct: a.volumePct() }))
      },
    },
    {
      keys: ['M'],
      label: 'media.keys.mute',
      match: key('m'),
      run: () => {
        a.toggleMute()
        a.osd(a.muted() ? t('media.osd.muted') : t('media.osd.volume', { pct: a.volumePct() }))
      },
    },
    {
      keys: ['Ctrl+←', 'Ctrl+→'],
      label: 'media.keys.speed',
      match: (e) => ctrl('ArrowLeft')(e) || ctrl('ArrowRight')(e),
      run: (e) => {
        a.stepRate(e.key === 'ArrowRight' ? 1 : -1)
        a.osd(`${a.rate()}x`)
      },
    },
    {
      // 视频面:PageUp/Down 上下集(用户拍板)。音频面不占它们 —— 那是翻聊天记录的键,换曲是破坏性动作。
      keys: ['PageUp', 'PageDown'],
      label: 'media.keys.episode',
      surface: 'video',
      match: (e) => e.key === 'PageUp' || e.key === 'PageDown',
      when: () => a.hasPlaylist(),
      run: (e) => stepTrack(e.key === 'PageDown'),
    },
    {
      // 两个面都有:N / P 上下曲(VLC 惯例;与 Space/K 一样的双绑定)。
      keys: ['N', 'P'],
      label: video ? 'media.keys.episode' : 'media.keys.track',
      match: (e) => key('n')(e) || key('p')(e),
      when: () => a.hasPlaylist(),
      run: (e) => stepTrack(e.key.toLowerCase() === 'n'),
    },
    {
      keys: ['R'],
      label: 'media.keys.mode',
      surface: 'audio',
      match: key('r'),
      when: () => !!a.cycleMode,
      run: () => {
        const name = a.cycleMode?.()
        if (name) a.osd(name)
      },
    },
    {
      keys: ['L'],
      label: video ? 'media.keys.list' : 'media.keys.trackList',
      match: key('l'),
      when: () => a.hasPlaylist() && !!a.toggleList,
      run: () => a.toggleList?.(),
    },
    {
      keys: ['S'],
      label: 'media.keys.skip',
      surface: 'video',
      match: key('s'),
      when: () => a.hasPlaylist() && !!a.skip,
      run: () => a.skip?.(),
    },
    {
      keys: ['A'],
      label: 'media.keys.audioTrack',
      match: key('a'),
      when: () => a.audioTrackCount() >= 2,
      run: () => {
        a.cycleAudioTrack()
        a.osd(t('media.audioTrack', { label: a.audioTrackLabel() }))
      },
    },
    {
      keys: ['C'],
      label: video ? 'media.keys.subtitle' : 'media.keys.lyrics',
      match: key('c'),
      when: () => a.captionCount() >= 1,
      run: () => {
        a.cycleCaption()
        a.osd(a.captionLabel())
      },
    },
    {
      keys: ['F'],
      label: 'media.keys.fullscreen',
      surface: 'video',
      match: key('f'),
      when: () => !!a.toggleFullscreen,
      run: () => a.toggleFullscreen?.(),
    },
    {
      keys: ['Esc'],
      label: video ? 'media.keys.esc' : 'media.keys.escBar',
      match: (e) => e.key === 'Escape',
      run: () => a.escape(),
    },
    {
      keys: ['H'],
      label: 'media.keys.help',
      match: key('h'),
      run: () => a.toggleHelp(),
    },
  ]
  const rows = all.filter((r) => !r.surface || r.surface === a.surface)

  function stepTrack(forward: boolean) {
    if (forward) {
      a.next()
      a.osd(t(video ? 'media.osd.nextEp' : 'media.osd.nextTrack'))
    } else {
      a.prev()
      a.osd(t(video ? 'media.osd.prevEp' : 'media.osd.prevTrack'))
    }
  }

  /** 帮助浮层的行:只列当前有意义的(没多集不列上下集)。 */
  const help = computed(() =>
    rows.filter((k) => !k.when || k.when()).map((k) => ({ keys: k.keys, label: t(k.label) })),
  )
  /** 按钮 tooltip 里附带的键名提示,如「下一集 (PageDown)」。 */
  const kb = (label: string, k: string) => `${label} (${k})`

  /** window keydown 处理;返回 true = 这次按键被接管(调用方可据此让控制条浮现一下)。 */
  function onKey(e: KeyboardEvent): boolean {
    if (a.blocked()) return false
    if (isTextTarget(e)) return false
    if (e.altKey) return false // Alt 组合留给系统;Ctrl/Cmd 只放行表里点名的
    const hit = rows.find((k) => (!k.when || k.when()) && k.match(e))
    if (!hit) return false
    e.preventDefault() // 空格/方向键防页面滚动;Esc 自己接管
    e.stopPropagation()
    hit.run(e)
    return true
  }

  return { help, kb, onKey }
}
