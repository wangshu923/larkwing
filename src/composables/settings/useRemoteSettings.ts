// 设置·远程渠道:Telegram / 钉钉凭证 + 微信扫码绑定(多绑定「一人一 bot」)。
// 从 SettingsView 抽出(2026-09-08)。

import { computed, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import { api, isTauri, type RemoteChannelView } from '../../lib/backend'
import { useToast } from '../useToast'

export function useRemoteSettings() {
  const { t } = useI18n()

  // —— 远程渠道(Telegram/钉钉 bot,PLAN 远程渠道):自包含 tab(同 providers,不走 useSettings) ——
  // 凭证写得进读不回(同 provider key 的「空串视同不改」);状态读 remote_status,保存后 reload_channels。
  const remoteChannels = ref<RemoteChannelView[]>([])
  const tgToken = ref('') // 本地写入框,提交即清空(凭证永不回显)
  const dtKey = ref('')
  const dtSecret = ref('')
  const fallback = (id: string): RemoteChannelView => ({
    id, enabled: false, configured: false, allowed_chats: '', running: false, last_error: null,
  })
  const tg = computed<RemoteChannelView>(
    () => remoteChannels.value.find((c) => c.id === 'telegram') ?? fallback('telegram'),
  )
  const dt = computed<RemoteChannelView>(
    () => remoteChannels.value.find((c) => c.id === 'dingtalk') ?? fallback('dingtalk'),
  )
  const wx = computed<RemoteChannelView>(
    () => remoteChannels.value.find((c) => c.id === 'weixin') ?? fallback('weixin'),
  )
  // 微信扫码登录状态机(QR-based,区别于 TG/钉钉粘贴 token):起手拿二维码 → 轮询状态 → confirmed 落库。
  const wxQrSvg = ref('') // 二维码 SVG(v-html 直接展示,免前端二维码依赖)
  const wxQrUrl = ref('') // 备用链接(扫不了时点开)
  const wxLoginStatus = ref('') // '' | wait | scaned | need_verifycode | verify_blocked | expired | confirmed | already | error
  const wxVerifyCode = ref('') // 手机上显示的配对码(need_verifycode 时输入,下次轮询自动带上)
  let wxQrcode = '' // 轮询标识(非响应式)
  let wxBaseUrl: string | null = null // IDC 重定向后的轮询地址
  let wxLoginSeq = 0 // 代次:重新扫码 / 切走 tab 时作废旧轮询循环
  async function loadRemote() {
    if (!isTauri()) return
    remoteChannels.value = await api.remoteStatus().catch(() => [])
    wxAccounts.value = await api.weixinAccounts().catch(() => [])
  }
  // 微信多绑定(一人一 bot):绑定者 user_id 列表;空串 = 旧版迁移的无身份绑定
  const wxAccounts = ref<string[]>([])
  async function unbindWeixin(userId: string) {
    try {
      await api.weixinUnbind(userId)
      await api.reloadChannels()
      await loadRemote()
    } catch (e) {
      console.error('解绑微信失败', e)
      useToast().error(t('settings.remote.weixin.unbindFailed'))
    }
  }
  async function toggleRemote(id: string, on: boolean) {
    await api.setSetting(`remote.${id}.enabled`, on ? '1' : '0')
    await api.reloadChannels()
    await loadRemote()
  }
  /** 写凭证:空 = 不改(读不回);写完清空输入框,再重启渠道。 */
  async function saveRemoteCred(key: string, val: string) {
    const v = val.trim()
    if (!v) return
    await api.setSetting(key, v)
    if (key.endsWith('.token')) tgToken.value = ''
    if (key.endsWith('.app_key')) dtKey.value = ''
    if (key.endsWith('.app_secret')) dtSecret.value = ''
    await api.reloadChannels()
    await loadRemote()
  }
  async function saveRemote(key: string, ev: Event) {
    await api.setSetting(key, (ev.target as HTMLInputElement).value.trim())
    await api.reloadChannels()
    await loadRemote()
  }
  function remoteStatusText(c: RemoteChannelView): string {
    if (c.last_error) return t('settings.remote.statusError')
    if (c.running) return t('settings.remote.statusOn')
    if (!c.enabled) return t('settings.remote.statusOff')
    if (!c.configured) return t('settings.remote.statusUnconfigured')
    return t('settings.remote.statusStarting')
  }

  /** 微信扫码登录起手:拿二维码并展示,开轮询循环。 */
  async function startWeixinLogin() {
    const seq = ++wxLoginSeq // 作废上一次(重复点/重扫)
    wxVerifyCode.value = ''
    wxBaseUrl = null
    wxLoginStatus.value = 'wait'
    wxQrSvg.value = ''
    wxQrUrl.value = ''
    try {
      const s = await api.weixinLoginStart()
      if (seq !== wxLoginSeq) return
      wxQrcode = s.qrcode
      wxQrUrl.value = s.qr_url
      wxQrSvg.value = s.qr_svg
    } catch {
      wxLoginStatus.value = 'error'
      useToast().error(t('settings.remote.weixin.startFailed'))
      return
    }
    void pollWeixinLogin(seq)
  }

  /** 轮询扫码状态:每次 poll 命令本身会长挂到服务端事件;小憩防打爆。confirmed 即连(自动开渠道)。 */
  async function pollWeixinLogin(seq: number) {
    const nap = (ms: number) => new Promise((r) => setTimeout(r, ms))
    while (seq === wxLoginSeq) {
      let r
      try {
        r = await api.weixinLoginPoll(wxQrcode, wxBaseUrl, wxVerifyCode.value || null)
      } catch {
        await nap(1500)
        continue
      }
      if (seq !== wxLoginSeq) return
      if (r.status === 'redirect') {
        wxBaseUrl = r.base_url // IDC 重定向:下次轮询换地址
        continue
      }
      wxLoginStatus.value = r.status
      if (r.status === 'confirmed' || r.status === 'already') {
        wxQrSvg.value = ''
        // 扫码即连(§3 强默认):自动开启渠道并刷新状态(token 已由 core 落库)
        await api.setSetting('remote.weixin.enabled', '1')
        await api.reloadChannels()
        await loadRemote()
        return
      }
      if (r.status === 'expired' || r.status === 'verify_blocked') {
        wxQrSvg.value = '' // 收起过期码,让用户重扫
        return
      }
      await nap(1200) // wait / scaned / need_verifycode:继续
    }
  }

  /** 离开远程 tab:作废在跑的扫码轮询 + 收起二维码。
   *  `wxLoginSeq` 是模块内的 `let` 代次,不能过解构 → 由本函数代劳(抽 composable 时新增的唯一一处)。 */
  function leaveRemoteTab() {
    wxLoginSeq++
    wxQrSvg.value = ''
  }

  return {
    dt,
    dtKey,
    dtSecret,
    leaveRemoteTab,
    loadRemote,
    remoteStatusText,
    saveRemote,
    saveRemoteCred,
    startWeixinLogin,
    tg,
    tgToken,
    toggleRemote,
    unbindWeixin,
    wx,
    wxAccounts,
    wxLoginStatus,
    wxQrSvg,
    wxQrUrl,
    wxVerifyCode,
  }
}
