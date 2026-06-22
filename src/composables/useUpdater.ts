// 一键更新(清单 ⑤·A):app 内查新版 → 点「更新」后台下载(进任务 HUD,**不阻塞操作**)→
// 下完弹「立即更新?」→ 装(Windows 装前自动退出 app)+ 重启。
// 决策(2026-06-22 用户拍板):依赖用户代理(不自建镜像)· 每日查 · 仅 Windows 本期。
//
// 代理:updater 自带 HTTP,**不走 §4.6 的 net::Client**,故读 app 的 net.proxy_* 传给 check({proxy})
// (同一把用户代理 + 尊重「总开关关=直连」)。
// 下载做成**前端自驱任务**(useTasks.startLocal):进度进 HUD、非阻塞;下载完成的「回调」=
// download() 这个 await 返回之后的代码(无需在任务系统里加通用回调总线)。
// 静默失败:自动查失败只 console(被动 §3.5);下载/安装失败给任务红条 / toast。
import { reactive } from 'vue'
import { check, type Update, type DownloadEvent } from '@tauri-apps/plugin-updater'
import { isTauri, api } from '../lib/backend'
import { i18n } from '../i18n'
import { useSettings } from './useSettings'
import { useTasks } from './useTasks'
import { useToast } from './useToast'

const t = i18n.global.t
const DAY = 86_400_000
const LASTCHECK_KEY = 'lw.update.lastCheck'

const state = reactive({
  /** 发现新版、待下载(弹「发现新版本」卡)。 */
  available: null as { version: string; notes: string } | null,
  /** 已下载完、待安装(弹「立即更新?」卡)。 */
  downloaded: false,
})
let pending: Update | null = null
let checking = false
let downloading = false

/** 生效代理:总开关开 + 地址非空才用,否则直连。与 §4.6 引擎侧 resolve 同向。 */
function effectiveProxy(): string | undefined {
  const s = useSettings()
  const enabled = s.get('net.proxy_enabled') === '1'
  const addr = (s.get('net.proxy') || '').trim()
  return enabled && addr ? addr : undefined
}

/** 查一次。true=有新版 / false=已最新 / null=查失败(调用方区分)。 */
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

/** 「更新」按钮:起后台下载任务(进 HUD、不阻塞),下完置 downloaded → 弹「立即更新?」。 */
async function download() {
  if (!pending || downloading) return
  downloading = true
  state.available = null // 收起「发现新版」卡,进度去任务 HUD
  const task = useTasks().startLocal({ kind: 'download', label: { key: 'task.update' } })
  let total = 0
  let got = 0
  try {
    await pending.download((ev: DownloadEvent) => {
      if (ev.event === 'Started') {
        total = ev.data.contentLength ?? 0
        task.progress(total ? 0 : undefined)
      } else if (ev.event === 'Progress') {
        got += ev.data.chunkLength ?? 0
        task.progress(
          total ? Math.min(1, got / total) : undefined,
          total
            ? { key: 'step.download', params: { done: +(got / 1e6).toFixed(1), total: +(total / 1e6).toFixed(1) } }
            : undefined,
        )
      } else if (ev.event === 'Finished') {
        task.progress(1)
      }
    })
    task.done()
    state.downloaded = true
  } catch (e) {
    console.error('下载更新失败', e)
    task.fail({ key: 'task.err.download' })
    downloading = false
  }
}

/** 「立即更新」:装(Windows 装前自动杀进程)+ 重启(Win 多由安装器拉起,此路给 mac/兜底)。 */
async function install() {
  if (!pending) return
  try {
    await pending.install()
    await api.relaunchApp()
  } catch (e) {
    console.error('安装更新失败', e)
    useToast().error(t('update.installFailed'))
  }
}

function dismiss() {
  state.available = null
  state.downloaded = false
}

let autoStarted = false

export function useUpdater() {
  return {
    state,
    /** 手动检查(返回 true=有新版 / false=已最新 / null=失败;调用方按需 toast)。 */
    check: () => runCheck(),
    download,
    install,
    dismiss,
    /** 启动调一次:每日节流(>24h 才查)+ 每 6h 复查(常驻自启也按日触达)。失败静默。 */
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
