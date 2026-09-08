// 设置·系统:开机自启 / 悬浮窗 / 主动关怀 / 下载认证 / 应用公钥 / 天气 / 代理。
// 从 SettingsView 抽出(2026-09-08)。

import { computed, onMounted, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import { api, isTauri, openExternal, setFloatVisible } from '../../lib/backend'
import { useToast } from '../useToast'
import { useSettings } from '../useSettings'

export function useSystemSettings() {
  const { t } = useI18n()
  const settings = useSettings()

  // —— 系统 tab:开机自启(OS 真相源,独立命令)+ 桌面悬浮窗(ui.float.* 设置,PLAN §12) ——
  // dev(tauri dev)下前端走 Vite devUrl、二进制在 target/debug —— 此时设自启会写一条指向 debug exe 的注册表项,
  // 开机却连不上 1420 → 白屏。故 dev build 禁用开关(import.meta.env.DEV 精确反映"前端是否走 dev server")。
  const isDev = import.meta.env.DEV
  const autostart = ref(false)
  const autostartBusy = ref(false)
  async function loadAutostart() {
    if (!isTauri()) return
    try {
      autostart.value = await api.autostartEnabled()
    } catch (e) {
      console.error('读取开机自启失败', e)
    }
  }
  async function toggleAutostart() {
    if (autostartBusy.value || isDev) return // dev 下禁用(见上);UI 已 disabled,这里再挡一道
    const target = !autostart.value
    if (!isTauri()) {
      autostart.value = target
      return
    }
    autostartBusy.value = true
    try {
      await api.setAutostart(target)
      autostart.value = await api.autostartEnabled() // 回读 OS 真值
    } catch (e) {
      console.error('设置开机自启失败', e)
    } finally {
      autostartBusy.value = false
    }
  }
  const floatEnabled = computed(() => settings.get('ui.float.enabled') !== '0')
  function toggleFloat() {
    const target = !floatEnabled.value
    settings.set('ui.float.enabled', target ? '1' : '0')
    void setFloatVisible(target) // 立即反映;主窗聚焦联动留 E 期
  }
  function setFloatOpacity(pct: number) {
    settings.set('ui.float.opacity', (pct / 100).toFixed(2))
  }
  // 待机轮播是否带"今日花费 / 余额"(opt-in;默认家庭脸不显,看板控自己开)
  const floatShowUsage = computed(() => settings.get('ui.float.show_usage') === '1')
  function toggleFloatUsage() {
    settings.set('ui.float.show_usage', floatShowUsage.value ? '0' : '1')
  }
  // 主动关怀总开关(PLAN ★主动关怀里程碑):关 = 悬浮窗不投关怀候选、in-chat 场景续接 chips 也收起。
  const careEnabled = computed(() => settings.get('care.enabled') !== '0')
  function toggleCare() {
    settings.set('care.enabled', careEnabled.value ? '0' : '1')
  }

  // 下载认证(WebDAV / 自家 NAS / 网盘挂载):web_download 按 host 自动带上账号。
  // **密码只进不出** —— 列表只回 host,所以「加一条」永远是整条重填(改密码 = 同 host 再填一次)。
  const credHosts = ref<string[]>([])
  const credBusy = ref(false)
  const credAdding = ref(false)
  const credForm = ref({ host: '', user: '', password: '' })
  async function loadCreds() {
    if (!isTauri()) return
    try {
      credHosts.value = await api.httpCredsHosts()
    } catch (e) {
      console.error('读下载认证失败', e)
    }
  }
  async function saveCred() {
    const f = credForm.value
    if (credBusy.value || !f.host.trim()) return
    credBusy.value = true
    try {
      await api.setHttpCred(f.host, f.user, f.password)
      credForm.value = { host: '', user: '', password: '' }
      credAdding.value = false
      await loadCreds()
    } catch (e) {
      console.error('存下载认证失败', e)
      useToast().error(t('settings.system.credSaveFailed'))
    } finally {
      credBusy.value = false
    }
  }
  async function dropCred(host: string) {
    if (credBusy.value) return
    credBusy.value = true
    try {
      await api.removeHttpCred(host)
      await loadCreds()
    } catch (e) {
      console.error('删下载认证失败', e)
      useToast().error(t('settings.system.credSaveFailed'))
    } finally {
      credBusy.value = false
    }
  }

  // 天气服务(PLAN 天气块):默认免 key Open-Meteo;接和风走 JWT —— 复制全局应用公钥到和风控制台,
  // 把项目 ID / 凭据 ID / API Host 三件套填回来。后端三件套齐 + 全局私钥已生成即切和风源。
  const appPublicKey = ref('')
  const pubKeyCopied = ref(false)
  onMounted(async () => {
    // 自动备份状态的进页即拉挪去了 useDataSettings 自己的 onMounted(同样挂载时机)。
    // 进设置即 ensure(幂等):服务页一直有公钥可复制。非 Tauri(纯浏览器预览)拿不到后端,留空。
    if (isTauri()) appPublicKey.value = await api.ensureAppKeypair()
  })
  function copyPublicKey() {
    const text = appPublicKey.value
    if (!text || !navigator.clipboard?.writeText) return
    navigator.clipboard.writeText(text).then(() => {
      pubKeyCopied.value = true
      window.setTimeout(() => (pubKeyCopied.value = false), 1500)
    })
  }
  // 三件套都是非秘密标识符,直存(后端校验 host 要 http(s));齐备才视作"已接和风"。
  const weatherConfigured = computed(
    () =>
      !!settings.get('weather.qweather.host') &&
      !!settings.get('weather.qweather.project_id') &&
      !!settings.get('weather.qweather.credential_id'),
  )
  function setQWeather(key: string, ev: Event) {
    settings.set(key, (ev.target as HTMLInputElement).value.trim())
  }
  // 全局代理:开关 net.proxy_enabled 控总闸,地址 net.proxy 始终保留(默认已填,免空)。
  // 下载/LLM 现读即生效,无需重启。关掉只停用、不丢地址(铁律:地址可保存下来)。
  const proxyEnabled = computed(() => settings.get('net.proxy_enabled') === '1')
  function toggleProxy() {
    const target = !proxyEnabled.value
    // 开启时把当前地址(可能是默认值)一并落库,确保后端选路与界面显示一致。
    if (target) {
      const addr = settings.get('net.proxy').trim()
      if (addr) void settings.set('net.proxy', addr)
    }
    void settings.set('net.proxy_enabled', target ? '1' : '0')
  }
  function setProxy(ev: Event) {
    settings.set('net.proxy', (ev.target as HTMLInputElement).value.trim())
  }
  function openQWeatherSite() {
    void openExternal('https://dev.qweather.com/')
  }

  return {
    appPublicKey,
    autostart,
    autostartBusy,
    careEnabled,
    copyPublicKey,
    credAdding,
    credBusy,
    credForm,
    credHosts,
    dropCred,
    floatEnabled,
    floatShowUsage,
    isDev,
    loadAutostart,
    loadCreds,
    openQWeatherSite,
    proxyEnabled,
    pubKeyCopied,
    saveCred,
    setFloatOpacity,
    setProxy,
    setQWeather,
    toggleAutostart,
    toggleCare,
    toggleFloat,
    toggleFloatUsage,
    toggleProxy,
    weatherConfigured,
  }
}
