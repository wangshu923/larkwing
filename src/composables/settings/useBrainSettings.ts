// 设置·大脑:钥匙 / 模型下拉(已接入卡与「自己接一个大脑」草稿两套)/ 高级覆盖 / 加接入点。
// 从 SettingsView 抽出(2026-09-08)。

import { computed, onUnmounted, reactive, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import { api, isTauri, type AppError, type ErrorKind, type ModelChoice, type ModelMeta, type ModelOverride, type ModelTier, type Protocol, type ProviderPreset, type ProviderView } from '../../lib/backend'
import { useToast } from '../useToast'
import { useSettings } from '../useSettings'

export function useBrainSettings() {
  const { t } = useI18n()
  const settings = useSettings()

  // 供应商卡片:预设全预填,用户按需改。钥匙草稿按卡隔离,回车/失焦保存,空草稿不动钥匙
  const keyDrafts = reactive<Record<string, string>>({})
  function saveKey(p: ProviderView) {
    const k = (keyDrafts[p.id] ?? '').trim()
    if (!k) return
    settings.saveProvider({ id: p.id, apiKey: k })
    keyDrafts[p.id] = ''
  }
  function saveField(p: ProviderView, field: 'baseUrl' | 'model', ev: Event) {
    const v = (ev.target as HTMLInputElement).value.trim()
    if (field === 'model' && pickDismissed === p.id) {
      // 下拉开着时敲的字是筛选词,点到别处 = 放弃这次筛选(与 Esc 同义),回显已存模型——
      // 否则「dee」这种半截筛选词会被 change 事件当模型存上、供应商静默坏掉(复审实锤)。
      // 想手填自由文本:不开下拉直接改字,或回车确认。
      pickDismissed = null
      delete modelDraft[p.id]
      return
    }
    if (!v || v === p[field]) {
      if (field === 'model') delete modelDraft[p.id] // 空 / 没变 → 草稿退场,回显已存的值
      return
    }
    const saved = settings.saveProvider({ id: p.id, [field]: v })
    // 模型框:存成了草稿才退场(显示的正是存的那个);没存成留着草稿让人看见自己填的
    if (field === 'model') saved.then((ok) => { if (ok) delete modelDraft[p.id] })
  }

  // 模型下拉(combobox):输入框仍是自由文本(中转 / 自架的名字随便填),行尾 ▾ 向该供应商的
  // 接入点拉「这把钥匙能用的模型」—— 真相源在服务商、不写死名单(DeepSeek 两个月换三次名单的教训);
  // 目录只负责贴档位 / 能看图 / 牌价标签并把认识的排前面。同时只开一个;清单按供应商缓存,重开秒出。
  const pickOpen = ref<string | null>(null)
  const pickLoading = ref(false)
  // 拉清单的错:后端 kind 之外多一档 need_endpoint(草稿卡接入点还没填就点了 ▾)
  const pickErr = ref<{ kind: ErrorKind | 'need_endpoint'; message: string } | null>(null)
  const pickLists = reactive<Record<string, ModelChoice[]>>({})
  const CUSTOM_PICK = 'custom' // 「自己接一个大脑」草稿卡在 pickOpen / pickLists 里的键
  const pickActive = ref(-1) // 键盘高亮行
  // 模型框显示 = 草稿 ?? 已存值。⚠️ 不能像接入点框那样直接 :value="p.model":每敲一字清单要重算 →
  // 组件重渲染 → Vue 把 DOM 值钉回 p.model,打的字被覆盖(首测实锤)。草稿同时就是筛选词。
  const modelDraft = reactive<Record<string, string>>({})
  const modelInputs: Record<string, HTMLInputElement | null> = {} // 打开下拉时聚焦 + 全选用
  const pickFiltered = computed(() => {
    const id = pickOpen.value
    const list = id ? pickLists[id] ?? [] : []
    // 筛选词:已接入的卡 = 该卡的草稿;草稿卡 = 它的模型输入框本身(v-model,重渲染不会钉回旧值)
    const q = (id === CUSTOM_PICK ? custom.model : id ? modelDraft[id] ?? '' : '').trim().toLowerCase()
    return q ? list.filter((c) => c.id.toLowerCase().includes(q)) : list
  })
  // 浏览器预览没有后端:一小份假清单看交互(与 FAKE_META 同性质的预览夹具,不是产品数据)
  function fakeModels(p: ProviderView): ModelChoice[] {
    const row = (id: string, known: boolean, tier: ModelTier, vision: boolean, i: number | null, o: number | null): ModelChoice =>
      ({ id, known, tier, vision, inUsdPerM: i, outUsdPerM: o })
    return p.protocol === 'anthropic_compat'
      ? [row('claude-opus-4-8', true, 'smart', true, 5, 25), row('claude-sonnet-4-6', true, 'smart', true, 3, 15), row('claude-haiku-4-5', true, 'light', true, 1, 5)]
      : [row('deepseek-v4-pro', true, 'balanced', false, 1.32, 3.96), row('deepseek-v4-flash', true, 'light', false, 0.44, 1.32), row('deepseek-v4-flash-vision-exp', true, 'light', true, 0.44, 1.32), row('some-relay-only-model', false, 'balanced', false, null, null)]
  }
  async function loadPick(p: ProviderView, force = false) {
    if (!force && pickLists[p.id]) return
    if (!p.keySet) {
      pickErr.value = { kind: 'no_api_key', message: '' } // 不白打一次网络:没钥匙必 401
      return
    }
    pickLoading.value = true
    pickErr.value = null
    try {
      pickLists[p.id] = isTauri() ? await api.listModels(p.id) : fakeModels(p)
    } catch (e) {
      pickErr.value = e && typeof e === 'object' && 'kind' in e ? (e as AppError) : { kind: 'internal', message: String(e) }
    } finally {
      pickLoading.value = false
    }
  }
  // 点到下拉外面关掉的那张卡:紧随的 change 事件(pointerdown 之后同一轮里 blur 触发)按「撤销筛选」
  // 处理;下一个宏任务就清掉,免得残留到以后某次真正的手填改动上
  let pickDismissed: string | null = null
  function onPickDocDown(e: PointerEvent) {
    if (!(e.target as Element | null)?.closest?.('.model-pick')) {
      pickDismissed = pickOpen.value
      setTimeout(() => (pickDismissed = null), 0)
      closePick()
    }
  }
  function closePick() {
    pickOpen.value = null
    document.removeEventListener('pointerdown', onPickDocDown, true)
  }
  async function togglePick(p: ProviderView) {
    if (pickOpen.value === p.id) {
      closePick()
      return
    }
    pickOpen.value = p.id
    pickActive.value = -1
    pickErr.value = null
    document.addEventListener('pointerdown', onPickDocDown, true)
    // 打开即聚焦 + 全选:一敲字就是「筛清单」而不是接在旧模型名后面;不敲字点走 = 值没变、不存
    const el = modelInputs[p.id]
    el?.focus()
    el?.select()
    await loadPick(p)
  }
  function pickModel(p: ProviderView, id: string) {
    closePick()
    if (id === p.model) {
      delete modelDraft[p.id]
      return
    }
    modelDraft[p.id] = id // 立刻显示选中的;存好后草稿让位给 p.model(同一个值,无闪动)
    settings.saveProvider({ id: p.id, model: id }).then((ok) => { if (ok) delete modelDraft[p.id] })
    if (advOpen[p.id] && !modelMeta[id]) fetchMeta(id) // 高级区展开着 → 跟着换模型
  }
  function onPickKey(p: ProviderView, e: KeyboardEvent) {
    if (pickOpen.value !== p.id) {
      if (e.key === 'ArrowDown') {
        e.preventDefault()
        togglePick(p)
      }
      return
    }
    const rows = pickFiltered.value
    if (e.key === 'Escape') {
      // Esc = 撤销这次筛选:草稿作废、回显当前模型(点走 / 回车仍按自由文本原语义存)。
      // stopPropagation:设置页在 window 上听 Esc 关整页(onKeydown),下拉开着时 Esc 只关下拉。
      e.preventDefault()
      e.stopPropagation()
      delete modelDraft[p.id]
      closePick()
    } else if (e.key === 'ArrowDown') {
      e.preventDefault()
      pickActive.value = Math.min(rows.length - 1, pickActive.value + 1)
    } else if (e.key === 'ArrowUp') {
      e.preventDefault()
      pickActive.value = Math.max(0, pickActive.value - 1)
    } else if (e.key === 'Enter' && pickActive.value >= 0 && rows[pickActive.value]) {
      e.preventDefault()
      pickModel(p, rows[pickActive.value].id)
    }
    // 没高亮行的回车不拦:原生 change 照常把手填的值存上(自由文本路不受下拉影响)
  }
  onUnmounted(() => document.removeEventListener('pointerdown', onPickDocDown, true))
  /** 卡片头的协议徽章(全大写 mono 风格)。 */
  function protoLabel(p: ProviderView) {
    return protoName(p.protocol, true)
  }
  /** 协议方言的人话名:兼容方言「X 兼容」、原生方言「X 原生」(Gemini / OpenAI Responses 为保真下楼)。 */
  function protoName(proto: string, upper = false): string {
    const [vendor, native]: [string, boolean] =
      proto === 'anthropic_compat' ? ['Anthropic', false]
      : proto === 'gemini' ? ['Gemini', true]
      : proto === 'openai_responses' ? ['OpenAI Responses', true]
      : ['OpenAI', false]
    return t(native ? 'settings.brain.native' : 'settings.brain.compat', { vendor: upper ? vendor.toUpperCase() : vendor })
  }

  // 「高级」:按模型纠正档位/价格/上下文窗口(空 = 用目录猜测)。按 provider 折叠,展开时懒取 meta。
  const advOpen = reactive<Record<string, boolean>>({})
  const modelMeta = reactive<Record<string, ModelMeta>>({}) // 键 = model id
  // 浏览器预览没有后端:给个保守假 meta(纯目录未知态),让面板照样能渲染/点
  const FAKE_META: ModelMeta = { guess: { tier: 'balanced', inUsdPerM: null, outUsdPerM: null, ctxWindowTokens: null, billing: 'cached', vision: false }, over: null }
  async function fetchMeta(model: string) {
    if (!model) return
    modelMeta[model] = isTauri() ? await api.modelMeta(model) : { ...FAKE_META }
  }
  function toggleAdv(p: ProviderView) {
    advOpen[p.id] = !advOpen[p.id]
    if (advOpen[p.id] && !modelMeta[p.model]) fetchMeta(p.model)
  }
  // 占位提示:目录猜测值(null = 目录也不知道)
  function autoHint(v: number | null): string {
    return t('settings.brain.auto', { v: v == null ? '—' : String(v) })
  }
  // 窗口以 K 为单位展示(存的是原始 token);占位猜测也折成 K
  function guessWinHint(v: number | null): string {
    return t('settings.brain.auto', { v: v == null ? '—' : `${Math.round(v / 1000)}K` })
  }
  function ovWinK(over: ModelOverride | null): number | '' {
    return over?.ctxWindowTokens ? Math.round(over.ctxWindowTokens / 1000) : ''
  }
  function tierLabel(tier: ModelTier): string {
    return t(`settings.brain.tier_${tier}`)
  }
  /** 档位下拉项(SkinSelect):首项「自动(=目录猜测)」带猜测档标签,其余三档。 */
  function tierOpts(meta: ModelMeta) {
    return [
      { value: '', label: t('settings.brain.tierAuto', { tier: tierLabel(meta.guess.tier) }) },
      { value: 'light', label: t('settings.brain.tier_light') },
      { value: 'balanced', label: t('settings.brain.tier_balanced') },
      { value: 'smart', label: t('settings.brain.tier_smart') },
    ]
  }
  /** 图片理解下拉项(SkinSelect):自动(带目录猜测)/能看图/只看文字。 */
  function visionOpts(meta: ModelMeta) {
    const guess = t(meta.guess.vision ? 'settings.brain.visionYes' : 'settings.brain.visionNo')
    return [
      { value: '', label: t('settings.brain.auto', { v: guess }) },
      { value: 'true', label: t('settings.brain.visionYes') },
      { value: 'false', label: t('settings.brain.visionNo') },
    ]
  }
  /** 计价方式下拉项(SkinSelect)。 */
  const billingOpts = computed(() => [
    { value: '', label: t('settings.brain.billingAuto') },
    { value: 'cached', label: t('settings.brain.billing_cached') },
    { value: 'uncached', label: t('settings.brain.billing_uncached') },
    { value: 'percall', label: t('settings.brain.billing_percall') },
  ])
  // 改一格 → 合并进该模型的覆盖,空值删该格;后端空壳自动删整条
  // 数字输入(上下文/价格)走 Event;两个下拉(档位/计价)走 SkinSelect 直接给值 → 共用核心。
  async function saveOv(p: ProviderView, field: keyof ModelOverride, ev: Event) {
    await saveOvRaw(p, field, (ev.target as HTMLInputElement).value)
  }
  async function saveOvRaw(p: ProviderView, field: keyof ModelOverride, rawIn: string) {
    const meta = modelMeta[p.model]
    if (!meta) return
    const cur: ModelOverride = { model: p.model, ...(meta.over ?? {}) }
    const raw = rawIn.trim()
    if (field === 'tier' || field === 'billing') {
      if (raw) (cur as unknown as Record<string, unknown>)[field] = raw
      else delete cur[field]
    } else if (field === 'vision') {
      // 三态:'' = 自动(删格回落目录),'true'/'false' = 显式标注
      if (raw) cur.vision = raw === 'true'
      else delete cur.vision
    } else if (raw === '') {
      delete cur[field]
    } else if (field === 'ctxWindowTokens') {
      const k = parseInt(raw, 10) // 输入是 K,存原始 token
      if (Number.isFinite(k) && k > 0) cur.ctxWindowTokens = k * 1000
      else return
    } else {
      const n = parseFloat(raw)
      if (Number.isFinite(n) && n >= 0) (cur as unknown as Record<string, unknown>)[field] = n
      else return
    }
    if (isTauri()) {
      try {
        await api.setModelOverride(cur)
      } catch {
        useToast().error(t('toast.actionFailed'))
        return
      }
    }
    await fetchMeta(p.model) // 回读(后端可能把空壳删了)
  }

  // 自定义卡:「自己接一个大脑」
  const adding = ref(false)
  const custom = reactive({ name: '', protocol: 'openai_compat', baseUrl: '', model: '', key: '' })
  const customReady = computed(() => custom.name.trim() && custom.baseUrl.trim() && custom.model.trim())
  async function addCustom() {
    if (!customReady.value) return
    const ok = await settings.saveProvider({
      id: `${customPreset.value || 'custom'}-${Date.now().toString(36)}`,
      name: custom.name.trim(),
      protocol: custom.protocol,
      baseUrl: custom.baseUrl.trim(),
      model: custom.model.trim(),
      apiKey: custom.key.trim() || undefined,
    })
    if (ok) {
      adding.value = false
      customPreset.value = ''
      Object.assign(custom, { name: '', protocol: 'openai_compat', baseUrl: '', model: '', key: '' })
    }
  }

  // 「从预设开始」:厂商预设表来自后端(registry::presets,数据单源);选一家 → 名字 / 协议 / 接入点
  // 自动填,**刻意不填模型**(用户拍板不写死:厂商换代频繁、名单必陈)→ 贴钥匙后用模型框 ▾ 现查、选好再接入。
  const presets = ref<ProviderPreset[]>([])
  const customPreset = ref('')
  // 浏览器预览没有后端:两条假预设看交互(预览夹具,产品表在 Rust)
  const FAKE_PRESETS: ProviderPreset[] = [
    { id: 'qwen', name: '千问 Qwen', protocol: 'openai_compat', baseUrl: 'https://dashscope.aliyuncs.com/compatible-mode/v1', keyPlaceholder: null },
    { id: 'ollama', name: 'Ollama', protocol: 'openai_compat', baseUrl: 'http://localhost:11434/v1', keyPlaceholder: 'ollama' },
  ]
  async function openCustom() {
    adding.value = true
    if (presets.value.length) return
    try {
      presets.value = isTauri() ? await api.providerPresets() : FAKE_PRESETS
    } catch (e) {
      console.error('预设表加载失败', e) // 拿不到就没下拉,手填照旧
    }
  }
  function applyProviderPreset(id: string) {
    customPreset.value = id
    const p = presets.value.find((x) => x.id === id)
    if (!p) return
    custom.name = p.name
    custom.protocol = p.protocol
    custom.baseUrl = p.baseUrl
    custom.key = p.keyPlaceholder ?? ''
    custom.model = '' // 不预填:靠 ▾ 现查
  }
  // 手改了接入点就不再算「那家预设」(✓ 与 id 前缀都摘掉);改回预设地址不自动认回,重新点选即可
  watch(() => custom.baseUrl, (v) => {
    const p = presets.value.find((x) => x.id === customPreset.value)
    if (p && p.baseUrl !== v.trim()) customPreset.value = ''
  })

  // 接入点框的 ▾ = 预设清单(入口放在框本身:人到这个框上找快捷选择,用户实锤;首版单独一行「预设」下拉已砍)。
  // 与模型 ▾ 共用 pickOpen / pickActive / closePick 与全局 .pick-* 样式;边打边筛(厂商名或地址子串),Esc 撤销。
  const ENDPOINT_PICK = 'endpoint'
  const endpointInput = ref<HTMLInputElement | null>(null)
  const endpointQuery = ref('') // 打开即清空:不用框里已有的地址去筛(否则只剩当前那家)
  let endpointPrev = ''
  const presetRows = computed(() => {
    const q = endpointQuery.value.trim().toLowerCase()
    return q
      ? presets.value.filter((p) => p.name.toLowerCase().includes(q) || p.baseUrl.toLowerCase().includes(q))
      : presets.value
  })
  function toggleEndpointPick() {
    if (pickOpen.value === ENDPOINT_PICK) {
      closePick()
      return
    }
    pickOpen.value = ENDPOINT_PICK
    pickActive.value = -1
    endpointQuery.value = ''
    endpointPrev = custom.baseUrl
    document.addEventListener('pointerdown', onPickDocDown, true)
    endpointInput.value?.focus()
    endpointInput.value?.select()
  }
  function pickEndpoint(id: string) {
    applyProviderPreset(id)
    closePick()
  }
  function onEndpointKey(e: KeyboardEvent) {
    if (pickOpen.value !== ENDPOINT_PICK) {
      if (e.key === 'ArrowDown') {
        e.preventDefault()
        toggleEndpointPick()
      }
      return
    }
    const rows = presetRows.value
    if (e.key === 'Escape') {
      e.preventDefault()
      e.stopPropagation() // 设置页在 window 上听 Esc 关整页;下拉开着时只关下拉
      custom.baseUrl = endpointPrev
      closePick()
    } else if (e.key === 'ArrowDown') {
      e.preventDefault()
      pickActive.value = Math.min(rows.length - 1, pickActive.value + 1)
    } else if (e.key === 'ArrowUp') {
      e.preventDefault()
      pickActive.value = Math.max(0, pickActive.value - 1)
    } else if (e.key === 'Enter' && pickActive.value >= 0 && rows[pickActive.value]) {
      e.preventDefault()
      pickEndpoint(rows[pickActive.value].id)
    }
  }
  const protoOpts = computed(() =>
    (['openai_compat', 'anthropic_compat', 'gemini', 'openai_responses'] as Protocol[]).map((v) => ({ value: v, label: protoName(v) })),
  )

  // 草稿卡的模型 ▾:对着草稿配置(协议 / 接入点 / 钥匙)现拉,选好再接入。与已接入的卡共用弹层组件与
  // pickOpen / pickLists / pickActive 状态;筛选词就是输入框本身。
  let customPickPrev = '' // Esc 撤销用
  const customInput = ref<HTMLInputElement | null>(null)
  async function loadPickCustom(force = false) {
    if (!force && pickLists[CUSTOM_PICK]) return
    if (!custom.baseUrl.trim()) {
      pickErr.value = { kind: 'need_endpoint', message: '' }
      return
    }
    if (!custom.key.trim()) {
      pickErr.value = { kind: 'no_api_key', message: '' }
      return
    }
    pickLoading.value = true
    pickErr.value = null
    try {
      pickLists[CUSTOM_PICK] = isTauri()
        ? await api.listModelsDraft(custom.protocol, custom.baseUrl.trim(), custom.key.trim())
        : fakeModels({ protocol: custom.protocol } as ProviderView)
    } catch (e) {
      pickErr.value = e && typeof e === 'object' && 'kind' in e ? (e as AppError) : { kind: 'internal', message: String(e) }
    } finally {
      pickLoading.value = false
    }
  }
  function togglePickCustom() {
    if (pickOpen.value === CUSTOM_PICK) {
      closePick()
      return
    }
    pickOpen.value = CUSTOM_PICK
    pickActive.value = -1
    pickErr.value = null
    customPickPrev = custom.model
    document.addEventListener('pointerdown', onPickDocDown, true)
    customInput.value?.focus()
    customInput.value?.select()
    void loadPickCustom()
  }
  function pickModelCustom(id: string) {
    custom.model = id
    closePick()
  }
  function onPickKeyCustom(e: KeyboardEvent) {
    if (pickOpen.value !== CUSTOM_PICK) {
      if (e.key === 'ArrowDown') {
        e.preventDefault()
        togglePickCustom()
      }
      return
    }
    const rows = pickFiltered.value
    if (e.key === 'Escape') {
      e.preventDefault()
      e.stopPropagation() // 设置页在 window 上听 Esc 关整页;下拉开着时只关下拉
      custom.model = customPickPrev
      closePick()
    } else if (e.key === 'ArrowDown') {
      e.preventDefault()
      pickActive.value = Math.min(rows.length - 1, pickActive.value + 1)
    } else if (e.key === 'ArrowUp') {
      e.preventDefault()
      pickActive.value = Math.max(0, pickActive.value - 1)
    } else if (e.key === 'Enter' && pickActive.value >= 0 && rows[pickActive.value]) {
      e.preventDefault()
      pickModelCustom(rows[pickActive.value].id)
    }
  }
  // 草稿的协议 / 接入点 / 钥匙一变,缓存的清单作废(下次点 ▾ 重拉)
  watch(() => [custom.protocol, custom.baseUrl, custom.key], () => { delete pickLists[CUSTOM_PICK] })

  return {
    CUSTOM_PICK,
    ENDPOINT_PICK,
    addCustom,
    adding,
    advOpen,
    autoHint,
    billingOpts,
    custom,
    customInput,
    customPreset,
    customReady,
    endpointInput,
    endpointQuery,
    guessWinHint,
    keyDrafts,
    loadPick,
    loadPickCustom,
    modelDraft,
    modelInputs,
    modelMeta,
    onEndpointKey,
    onPickKey,
    onPickKeyCustom,
    openCustom,
    ovWinK,
    pickActive,
    pickEndpoint,
    pickErr,
    pickFiltered,
    pickLists,
    pickLoading,
    pickModel,
    pickModelCustom,
    pickOpen,
    presetRows,
    protoLabel,
    protoOpts,
    saveField,
    saveKey,
    saveOv,
    saveOvRaw,
    tierOpts,
    toggleAdv,
    toggleEndpointPick,
    togglePick,
    togglePickCustom,
    visionOpts,
  }
}
