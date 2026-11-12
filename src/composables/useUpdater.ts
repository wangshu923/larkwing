// 一键更新(清单 ⑤·A):app 内查新版 → 点「更新」后台下载(进任务 HUD,**不阻塞操作**)→
// 下完弹「立即更新?」→ 装(Windows 装前自动退出 app)+ 重启。
// 决策(2026-06-22)· 每日查 · 仅 Windows 本期。
//
// 选路(2026-06-30 用户拍板,§6.9 镜像同理):endpoint 列表 = **gh 镜像优先 + 官方兜底**
// (tauri.conf;顺序故障转移,第一个给出明确答案的用)。check 两阶段:**先直连**这串 endpoint;
// **全够不到才看代理开关**,开了带代理再试一次(代理那趟覆盖检查+下载)。
// ⚠️ 检查走壳层自己的 `update_check`(2026-11-12),不用插件的 `check()`:插件遇到「回了 200 但正文
// 不是清单」(镜像的拦截页)会直接整体失败、不试后面的地址 —— 镜像排最前,它一出问题所有人静默
// 收不到更新。壳层逐个地址单独查,怎么失败都换下一个;查到的东西仍是插件的 Update(下载 / 安装照旧)。
// ⚠️ ① updater 自带 HTTP,**不走 §4.6 的 net::Client**;② 关代理时只是「不传我们的地址」,插件底层
// reqwest 仍可能读系统代理 env(check API 无 no_proxy 旋钮),做不到 §4.6「连 env 都不读」那么严。
// ⚠️ 镜像加速**检查**(拉 manifest)的同时,**下载**也走镜像了(2026-06-30):CI 的 finalize-manifest
// job 把 latest.json 的资产 url 前缀成 ghfast.top(release.yml);tauri updater 每平台只认一个 url,
// 故 download() 失败时先**换入口**(endpoints 里其余的镜像 / 官方各试一次,壳层 `update_retarget`;
// 包照旧过签名校验),都不行再「开了代理就带代理重下一次」(check 那趟同款)。
// 下载做成**前端自驱任务**(useTasks.startLocal):进度进 HUD、非阻塞;下载完成的「回调」=
// download() 这个 await 返回之后的代码(无需在任务系统里加通用回调总线)。
// 静默失败:自动查失败只 console(被动 §3.5);下载/安装失败给任务红条 / toast。
import { reactive } from 'vue'
import { Update, type DownloadEvent } from '@tauri-apps/plugin-updater'
import { emitUpdateState, isTauri, api } from '../lib/backend'
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
/** 最初那次查到的资源号与可换的下载入口数(换入口恒按它数,序号才稳定;见 download())。 */
let pendingRid = 0
let pendingAlts = 0
let checking = false
let downloading = false

/** 生效代理:总开关开 + 地址非空 → 用它;否则 undefined = 不传我们的代理(插件用默认,可能含系统 env)。
 *  ⚠️ 这不是 §4.6 的「纯直连」(见文件头注释),只是「不主动加我们配置的代理」。 */
function effectiveProxy(): string | undefined {
  const s = useSettings()
  const enabled = s.get('net.proxy_enabled') === '1'
  const addr = (s.get('net.proxy') || '').trim()
  return enabled && addr ? addr : undefined
}

/** 把 check() 结果落到 state;有新版返 true,够得到但无更新返 false。
 *  同时把绝对态广播给悬浮窗(待机轮播出「发现新版本」可点条;主窗仍是唯一执行位)。 */
/** 查一次(壳层逐个地址试);null = 已是最新。 */
async function fetchUpdate(proxy?: string): Promise<{ update: Update; alts: number } | null> {
  const meta = await api.updateCheck(proxy, 12_000)
  return meta ? { update: new Update(meta), alts: meta.downloadAlts } : null
}

function adopt(found: { update: Update; alts: number } | null): boolean {
  if (found) {
    const update = found.update
    pending = update
    pendingRid = update.rid
    pendingAlts = found.alts
    state.available = { version: update.version, notes: update.body ?? '' }
    emitUpdateState('available', update.version)
    return true
  }
  return false
}

/** 查一次。true=有新版 / false=已最新 / null=查失败(调用方区分)。
 *  策略(2026-06-30 用户拍板):**先直连**——走 tauri.conf 的 endpoint 列表(gh 镜像优先 + 官方兜底,
 *  顺序故障转移、第一个通的用;tauri 不支持并发竞速,short timeout 让失败快速翻篇)。直连**全够不到**
 *  (throw)且**代理开关开** → 带代理再试一次(代理那趟同时覆盖检查 + 后续下载)。代理关 = 不兜,如实失败。 */
async function runCheck(): Promise<boolean | null> {
  if (!isTauri() || checking) return false
  checking = true
  try {
    // 1) 直连:镜像 + 官方逐个试(不传我们的代理)
    try {
      return adopt(await fetchUpdate())
    } catch (eDirect) {
      // 2) 直连全失败 → 代理开关开就带代理重试(否则放弃,§3.5 如实失败)
      const proxy = effectiveProxy()
      if (!proxy) {
        console.error('检查更新失败(直连,未开代理)', eDirect)
        return null
      }
      return adopt(await fetchUpdate(proxy))
    }
  } catch (e) {
    console.error('检查更新失败(代理回退也失败)', e)
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
  emitUpdateState('none') // 悬浮窗那条同步收起(下载中,进度看主窗 HUD)
  const task = useTasks().startLocal({ kind: 'download', label: { key: 'task.update' } })
  let total = 0
  let got = 0
  const onEvent = (ev: DownloadEvent) => {
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
  }
  try {
    // 1) 直下:url 已是镜像(CI 改写,见文件头)→ 国内无代理也能下
    try {
      await pending.download(onEvent)
    } catch (eDirect) {
      // 2) 清单里那个地址下不动 → 换入口各试一次(另一个镜像 / 官方;序号恒按最初查到的那份数)
      let done = false
      for (let i = 0; i < pendingAlts && !done; i++) {
        try {
          const u = new Update(await api.updateRetarget(pendingRid, i))
          total = 0
          got = 0 // 重置进度,重下从头算
          await u.download(onEvent)
          pending = u // 装的时候要用「下成了的那一份」(下好的字节挂在它身上)
          done = true
        } catch (eAlt) {
          console.warn('更新下载:换入口也没成', i, eAlt)
        }
      }
      if (!done) {
        // 3) 都不行 → 开了代理就带代理重查一次、拿到绑代理的 Update 重下(否则如实失败 §3.5)
        const proxy = effectiveProxy()
        if (!proxy) throw eDirect
        const found = await fetchUpdate(proxy)
        if (!found) throw eDirect
        pending = found.update
        pendingRid = found.update.rid
        pendingAlts = found.alts
        total = 0
        got = 0
        await found.update.download(onEvent)
      }
    }
    task.done()
    state.downloaded = true
    emitUpdateState('downloaded', pending?.version) // 悬浮窗改显「点击重启生效」
  } catch (e) {
    console.error('下载更新失败', e)
    task.fail({ key: 'task.err.download' })
    // 失败不静默吞条目(§3.5):恢复「发现新版」卡 + 悬浮窗条(adopt 幂等),留给用户重试
    if (pending) adopt({ update: pending, alts: pendingAlts })
  } finally {
    // 成功 / 失败都复位:否则下载成功后 downloading 永为 true,长驻进程里第二次更新点「更新」
    // 会在入口 `if (!pending || downloading) return` 静默早退、按钮无反应(破「不静默失败」§3.5)。
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
  emitUpdateState('none') // 主窗按了「稍后」→ 悬浮窗那条也收(别两处各唠各的)
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
