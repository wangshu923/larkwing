// 一键更新(清单 ⑤ · 目标 A):app 内查新版 → 点一下后台下载 → 原地装 → 重启。
// 决策(2026-06-22 用户拍板):**依赖用户代理**(不自建镜像)· **每日**查 · **仅 Windows** 本期。
//
// 代理:updater 自带 HTTP,**不走 §4.6 的 net::Client**,所以这里读 app 的 net.proxy_* 设置、
// 运行时传进 check({ proxy })——用同一把用户代理,且尊重「总开关关 = 直连」。
// 静默失败:自动查失败只 console(被动,§3.5 不弹);手动查 / 下载装失败才给反馈(UI 调用方接 toast)。
import { reactive } from 'vue'
import { check, type Update, type DownloadEvent } from '@tauri-apps/plugin-updater'
import { isTauri, api } from '../lib/backend'
import { useSettings } from './useSettings'

const DAY = 86_400_000
const LASTCHECK_KEY = 'lw.update.lastCheck'

const state = reactive({
  /** 有新版 = 弹更新卡;null = 没有 / 已忽略。 */
  available: null as { version: string; notes: string } | null,
  downloading: false,
  /** 0..100;-1 = 不确定(拿不到总长)。 */
  progress: 0,
})
let pending: Update | null = null
let checking = false

/** 生效代理:总开关开 + 地址非空才用,否则直连(undefined)。与 §4.6 引擎侧 resolve 同向。 */
function effectiveProxy(): string | undefined {
  const s = useSettings()
  const enabled = s.get('net.proxy_enabled') === '1'
  const addr = (s.get('net.proxy') || '').trim()
  return enabled && addr ? addr : undefined
}

/** 查一次。返回是否有新版;失败返回 null(调用方区分「没有」与「查失败」)。 */
async function runCheck(): Promise<boolean | null> {
  if (!isTauri() || checking) return false
  checking = true
  try {
    const update = await check({ proxy: effectiveProxy(), timeout: 30_000 })
    if (update) {
      pending = update
      state.available = { version: update.version, notes: update.body ?? '' }
      return true
    }
    return false
  } catch (e) {
    console.error('检查更新失败', e)
    return null
  } finally {
    checking = false
  }
}

/** 下载 + 原地装,带进度;装完重启(Windows 由安装器拉起,此处主要走 mac/兜底)。 */
async function install(): Promise<boolean> {
  if (!pending || state.downloading) return false
  state.downloading = true
  state.progress = -1
  let total = 0
  let got = 0
  try {
    await pending.downloadAndInstall((ev: DownloadEvent) => {
      if (ev.event === 'Started') {
        total = ev.data.contentLength ?? 0
        state.progress = total ? 0 : -1
      } else if (ev.event === 'Progress') {
        got += ev.data.chunkLength ?? 0
        if (total) state.progress = Math.min(100, Math.round((got / total) * 100))
      } else if (ev.event === 'Finished') {
        state.progress = 100
      }
    })
    await api.relaunchApp() // 走到这 = 非 Windows(Win 已被安装器杀进程并装完拉起)
    return true
  } catch (e) {
    console.error('下载/安装更新失败', e)
    state.downloading = false
    return false
  }
}

function dismiss() {
  state.available = null
  pending = null
}

let autoStarted = false

export function useUpdater() {
  return {
    state,
    /** 手动「检查更新」:返回 true=有新版 / false=已最新 / null=查失败(调用方按需 toast)。 */
    check: () => runCheck(),
    install,
    dismiss,
    /** 启动调一次:每日节流(距上次 >24h 才查)+ 每 6h 复查一次(常驻自启也能按日触达)。失败静默。 */
    startAutoCheck() {
      if (!isTauri() || autoStarted) return
      autoStarted = true
      const tick = () => {
        const last = Number(localStorage.getItem(LASTCHECK_KEY) || 0)
        if (Date.now() - last < DAY) return
        localStorage.setItem(LASTCHECK_KEY, String(Date.now()))
        void runCheck()
      }
      tick()
      window.setInterval(tick, 6 * 3600_000)
    },
  }
}
