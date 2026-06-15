// ViewModel:设置快照 + 乐观写(失败回滚)。浏览器预览降级为内存假数据(同 useChat 约定)。
// key 与 Rust 白名单一一对应(engine::set_setting);新设置项 = 两边各加一行。

import { reactive } from 'vue'
import {
  api,
  emitSettingChanged,
  isTauri,
  onSettingChanged,
  type ProviderPatch,
  type ProviderView,
} from '../lib/backend'

const DEFAULTS: Record<string, string> = {
  'ui.pet_name': '', // 空 = 用字典里的默认名(pet.name)
  // 一句话性格设定;与 Rust 侧 context::DEFAULT_PERSONA_STYLE 手工同步(改要一起改)。
  // 没动过 = 这句默认;清空保存 = 纯出厂人设
  'persona.style': '暖心又好奇的小机灵,偶尔「滴——」一声卖萌,永远向着这个家',
  'ui.character': 'titan',
  'ui.bubble_shape': 'round',
  'ui.text_scale': 'standard',
  'ui.locale': 'zh-CN', // 界面语言;'ui.' 前缀自动过 Rust 白名单。对话语言由模型跟随用户,与此解耦
  'llm.strategy': 'balanced',
  'llm.thinking': 'off',
  // 声音(PLAN §11):与 Rust 白名单逐键对应(user 级 speaker/auto_speak/rate/patience/volume,
  // app 级 input_device);档位值是契约,改要两边一起改
  'voice.speaker': 'zh-CN-XiaoxiaoNeural',
  'voice.auto_speak': 'follow',
  'voice.rate': 'standard',
  'voice.patience': 'standard',
  'voice.volume': '100',
  'voice.input_device': '',
  'voice.wake.sensitivity': '50', // 唤醒灵敏度 0~100(global)→ KWS 阈值;'50' = 经验折中

  'voice.tts_backend': 'online', // 在线 edge / 离线 vits(断网兜底,需下大模型)
  // 天气(PLAN 天气块):key 是秘密 → 后端回掩码(····xxxx),空 = 用免 key Open-Meteo;host 非秘密专属接口地址
  'weather.qweather.key': '',
  'weather.qweather.host': '',
  // 桌面悬浮窗(PLAN §12);ui.* 走 engine set_setting 的 ui. 分支自动放行(无需改 Rust 白名单)
  'ui.float.enabled': '1', // '1' 开 / '0' 关
  'ui.float.opacity': '0.8', // 0.4–1.0
  'ui.float.pos': '', // 拖动后记住的位置 "x,y"(物理像素);空 = 默认右下角
  'ui.float.show_usage': '0', // 待机轮播是否带"今日花费/余额"(opt-in;默认家庭脸不显)
}

// 浏览器预览的供应商假数据:与后端 effective_specs 同构(预设漏出、预填、钥匙空)
const FAKE_PROVIDERS: ProviderView[] = [
  { id: 'deepseek', name: 'DeepSeek', protocol: 'openai_compat', baseUrl: 'https://api.deepseek.com', model: 'deepseek-v4-pro', enabled: true, builtin: true, keyMasked: '${DEEPSEEK_API_KEY}', keySet: true },
  { id: 'anthropic', name: 'Anthropic', protocol: 'anthropic_compat', baseUrl: 'https://api.anthropic.com', model: 'claude-sonnet-4-6', enabled: true, builtin: true, keyMasked: '', keySet: false },
]

const state = reactive({
  ready: false,
  userName: '我',
  values: { ...DEFAULTS } as Record<string, string>,
  providers: [] as ProviderView[],
})

/** 设置侧贴好钥匙后通知 useChat 翻转 hasApiKey(回调注入,避免模块互相 import)。 */
let providersUsableCb: (() => void) | null = null
export function onProvidersUsable(cb: () => void) {
  providersUsableCb = cb
}

let loadStarted = false
async function load() {
  if (loadStarted) return
  loadStarted = true
  if (!isTauri()) {
    state.providers = FAKE_PROVIDERS.map((p) => ({ ...p }))
    state.ready = true
    return
  }
  // 跨窗口设置同步:别的窗口改了设置 → 跟随(主窗换形象 / 透明度,悬浮窗实时跟上)
  onSettingChanged((key, value) => {
    if (key in DEFAULTS) state.values[key] = value
  })
  try {
    for (const e of await api.listSettings()) {
      if (e.key in DEFAULTS) state.values[e.key] = e.value
    }
    state.providers = await api.listProviders()
  } catch (e) {
    console.error('设置加载失败', e) // 拿不到就用默认值,不挡 UI
  }
  state.ready = true
}

function afterProvidersChanged(views: ProviderView[]) {
  state.providers = views
  if (views.some((p) => p.enabled && p.keySet)) providersUsableCb?.()
}

async function saveProvider(patch: ProviderPatch) {
  if (!isTauri()) {
    // 预览降级:本地合成同样的 upsert 语义,纯看交互
    const p = state.providers.find((x) => x.id === patch.id)
    const masked = patch.apiKey?.trim()
      ? patch.apiKey.includes('${') ? patch.apiKey : `····${patch.apiKey.slice(-4)}`
      : undefined
    if (p) Object.assign(p, { ...patch, apiKey: undefined, keyMasked: masked ?? p.keyMasked, keySet: masked ? true : p.keySet })
    else state.providers.push({
      id: patch.id, name: patch.name || patch.id, protocol: (patch.protocol as ProviderView['protocol']) || 'openai_compat',
      baseUrl: patch.baseUrl || '', model: patch.model || '', enabled: patch.enabled ?? true,
      builtin: false, keyMasked: masked ?? '', keySet: !!masked,
    })
    return true
  }
  try {
    afterProvidersChanged(await api.saveProvider(patch))
    return true
  } catch (e) {
    console.error('保存供应商失败', e)
    return false
  }
}

async function removeProvider(id: string) {
  if (!isTauri()) {
    state.providers = state.providers.filter((p) => p.id !== id || p.builtin)
    return
  }
  try {
    afterProvidersChanged(await api.removeProvider(id))
  } catch (e) {
    console.error('删除供应商失败', e)
  }
}

function get(key: string): string {
  return state.values[key] ?? DEFAULTS[key] ?? ''
}

async function set(key: string, value: string) {
  const prev = state.values[key]
  state.values[key] = value // 乐观更新
  if (!isTauri()) return
  try {
    await api.setSetting(key, value)
    emitSettingChanged(key, value) // 跨窗口同步(悬浮窗 / 设置页 / 主窗对齐)
  } catch (e) {
    console.error('设置保存失败', key, e)
    state.values[key] = prev // 回滚,UI 与库不分叉
  }
}

async function rename(name: string) {
  const v = name.trim()
  if (!v) return
  const prev = state.userName
  state.userName = v
  if (!isTauri()) return
  try {
    state.userName = (await api.renameUser(v)).name
  } catch (e) {
    console.error('改名失败', e)
    state.userName = prev
  }
}

/** boot 数据过桥(useChat 单向调用,避免循环依赖)。 */
export function hydrateUserName(name: string) {
  state.userName = name
}

export function useSettings() {
  void load()
  return { state, get, set, rename, saveProvider, removeProvider }
}
