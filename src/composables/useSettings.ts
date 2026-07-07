// ViewModel:设置快照 + 乐观写(失败回滚)。浏览器预览降级为内存假数据(同 useChat 约定)。
// key 与 Rust 白名单一一对应(engine::set_setting);新设置项 = 两边各加一行。

import { reactive } from 'vue'
import {
  api,
  emitSettingChanged,
  emitSkinChanged,
  isTauri,
  onSettingChanged,
  onSkinChanged,
  type ProviderPatch,
  type ProviderView,
} from '../lib/backend'

const DEFAULTS: Record<string, string> = {
  'ui.pet_name': '', // 空 = 用字典里的默认名(pet.name)
  // 一句话性格设定;与 Rust 侧 context::DEFAULT_PERSONA_STYLE 手工同步(改要一起改)。
  // 默认留空 = 中性人设(不预设性格倾向,适配最多用户);用户想要性格自己写,占位符给了示例
  'persona.style': '',
  'ui.character': 'titan',
  'ui.pet.hidden': '0', // 桌宠遛弯显隐('0' 显 / '1' 隐);右键「隐藏桌宠」置 1,设置页可恢复。ui.* 自动过白名单
  'ui.bubble_shape': 'round',
  'ui.text_scale': 'standard',
  'ui.locale': 'zh-CN', // 界面语言;'ui.' 前缀自动过 Rust 白名单。对话语言由模型跟随用户,与此解耦
  'llm.strategy': 'balanced',
  'llm.thinking': 'medium', // 默认反应模式=中度思考;与 Rust engine 缺省(→Medium)同步
  // 记忆自动提炼(PLAN §13 Phase 3):后台把聊天里值得记的事蒸馏成长期记忆。'1' 开 / '0' 关(默认开);
  // app 级,Rust APP_SETTING_KEYS + set_setting 0/1 校验逐键对应(§6.8 两边各加一行)。宁缺毋滥见 consolidate.rs
  'memory.auto_consolidate': '1',
  // 主动关怀(情境主动,PLAN ★主动关怀里程碑):贴着你近况的轻提醒(切片1 = 悬浮窗待机轮播 L0)。
  // '1' 开 / '0' 关(默认开);app 级,Rust APP_SETTING_KEYS + set_setting 0/1 校验逐键对应(§6.8)。
  // 露名字的文案一律 {name} 占位、petName 注入(§6.6),绝不硬编「旺财」。
  'care.enabled': '1',
  // 声音(PLAN §11):与 Rust 白名单逐键对应(user 级 speaker/auto_speak/rate/patience/volume,
  // app 级 input_device);档位值是契约,改要两边一起改
  'voice.speaker': '', // 默认音色单源在后端 tts::DEFAULT_SPEAKER(§4.11,前端不写死副本);空 = 未设,设置页用 voiceStatus.defaultSpeaker 高亮默认项
  'voice.auto_speak': 'follow',
  'voice.rate': 'standard',
  'voice.patience': 'standard',
  'voice.volume': '100',
  'voice.input_device': '',
  'voice.wake.sensitivity': '100', // 唤醒灵敏度 0~100(global)→ KWS 阈值;'100' = 最灵敏(默认偏召回,保障叫得应,见 AGENT.md §8.2)
  'voice.asr.model': 'sense-voice', // 中文识别模型档(global):sense-voice(快,默认)/ firered-ctc(更准,听不清/孩子选它);值与 Rust 校验同源,模型用时下载
  'voice.capture.source': 'browser', // 采集源(层1 AEC 采集端):browser(默认,2026-07-06 转正——getUserMedia 消完回声推流,治自我唤醒的根)/ cpal(回落);与 Rust capture_source 默认镜像(§6.8/§4.11)
  'voice.input_device_web': '', // 浏览器采集的麦克风 deviceId(空=系统默认);与 cpal 的 voice.input_device 分键(两套命名空间)



  'voice.tts_backend': 'online', // 在线 edge / 离线 vits(断网兜底,需下大模型)
  // 天气(PLAN 天气块):和风 JWT 接入三件套(host + 项目 ID + 凭据 ID);齐备 + 全局公钥已生成 → 切和风,
  // 否则免 key Open-Meteo。密钥对是全局的(crypto.ed25519.*,后端管),不在此前端默认表里。
  'weather.qweather.host': '',
  'weather.qweather.project_id': '',
  'weather.qweather.credential_id': '',
  // 全局代理(传输层):开关 net.proxy_enabled 控总闸,地址 net.proxy 单独保存(始终保留、给默认值免空)。
  // 关 = 一律直连;开 = 直连优先、连不通才兜底走该地址(墙内源永不被代理);地址支持 http(s):// / socks5(h):// / ${ENV}。
  // Rust 白名单 net.proxy / net.proxy_enabled 逐键对应(§6.8 两边各加一行)。
  'net.proxy_enabled': '0', // '1' 开 / '0' 关(默认关 = 直连)
  'net.proxy': 'http://127.0.0.1:7890', // 代理地址;预填常见本地端口,开关一开即用
  // 桌面悬浮窗(PLAN §12);ui.* 走 engine set_setting 的 ui. 分支自动放行(无需改 Rust 白名单)
  'ui.float.enabled': '1', // '1' 开 / '0' 关
  'ui.float.opacity': '0.8', // 0.4–1.0
  'ui.float.pos': '', // 拖动后记住的位置 "x,y"(物理像素);空 = 默认右下角
  'ui.float.show_usage': '0', // 待机轮播是否带"今日花费/余额"(opt-in;默认家庭脸不显)
  // 响度均衡 / 夜间模式(app 级;客户端 Web Audio 消费,见 useAudioGraph.ts)。Rust APP_SETTING_KEYS
  // + set_setting 逐键对应(§6.8 两边各加一行)。leveling 关 = 不接管播放(Web Audio 兜底关);夜间自动时段可跨零点。
  'audio.leveling': '1', // '1' 开 / '0' 关(默认开:电影/音乐音量稳、不炸)
  'audio.night_mode': 'auto', // off / on / auto(默认自动:到点自动压低大动态,不吵人)
  'audio.night_start': '22:00', // auto 起(HH:MM,24h)
  'audio.night_end': '07:00', // auto 止(可跨零点)
}

// 浏览器预览的供应商假数据:与后端 effective_specs 同构(预设漏出、预填、钥匙空)
const FAKE_PROVIDERS: ProviderView[] = [
  { id: 'deepseek', name: 'DeepSeek', protocol: 'openai_compat', baseUrl: 'https://api.deepseek.com', model: 'deepseek-v4-pro', enabled: true, builtin: true, keyMasked: '${DEEPSEEK_API_KEY}', keySet: true },
  { id: 'anthropic', name: 'Anthropic', protocol: 'anthropic_compat', baseUrl: 'https://api.anthropic.com', model: 'claude-sonnet-4-6', enabled: true, builtin: true, keyMasked: '', keySet: false },
]

const state = reactive({
  ready: false,
  userId: 0, // 当前用户 id(boot 过桥;家人页据此标「你」、防删自己)
  userName: '我',
  skin: 'scifi', // 用户级皮肤(users.skin_id);默认/兜底 = 科幻。boot 过桥应用,设置页可切
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
  onSkinChanged(applySkin) // 别的窗口换肤 → 跟随(主窗 ↔ 悬浮窗实时同步)
  try {
    applySkin(await api.skin()) // 用户级皮肤 → <html data-skin>(主窗与悬浮窗都经此拉初值)
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
export function hydrateUser(id: number, name: string) {
  state.userId = id
  state.userName = name
}

// 皮肤 = 用户级偏好(users.skin_id)。语义 token 在 style.css 按 <html data-skin> 生效;
// 换皮只换观感、不改组件(宪法 §3.6/§5)。未知值一律回科幻,脏数据不黑屏。
const SKINS = ['scifi', 'warm', 'green', 'night']
function applySkin(id: string) {
  const skin = SKINS.includes(id) ? id : 'scifi'
  state.skin = skin
  document.documentElement.dataset.skin = skin
}

/** 设置页换肤入口:乐观换观感 + 持久化(users.skin_id);失败回滚。 */
async function setSkin(id: string) {
  const prev = state.skin
  applySkin(id)
  if (!isTauri()) return
  try {
    await api.setSkin(id)
    emitSkinChanged(id) // 实时同步给悬浮窗(另一个 WebView)
  } catch (e) {
    console.error('换肤保存失败', e)
    applySkin(prev) // 回滚,UI 与库不分叉
  }
}

export function useSettings() {
  void load()
  return { state, get, set, rename, saveProvider, removeProvider, setSkin }
}
