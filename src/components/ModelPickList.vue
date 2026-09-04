<script setup lang="ts">
// 模型下拉的弹层 —— 设置·大脑两处共用:已接入的供应商卡 / 「自己接一个大脑」草稿卡。
// 只管渲染清单与各态文案;拉取、筛选词、键盘高亮归调用方(两处的存法不同:已接入的卡按 provider id
// 存草稿,草稿卡的输入框本身就是草稿)。弹层锚在调用方 `.model-pick`(position:relative)的左右缘。
// 状态文案全部经 t() 选好(core 只给 kind,§6.6);样式只用语义 token,与 SkinSelect 同款玻璃。
import { useI18n } from 'vue-i18n'
import type { ErrorKind, ModelChoice, ModelTier } from '../lib/backend'

/** 拉清单前置检查 / 后端错误:后端 kind 之外多一档 need_endpoint(草稿卡接入点还没填)。 */
type PickErr = { kind: ErrorKind | 'need_endpoint'; message: string }

defineProps<{
  loading: boolean
  err: PickErr | null
  /** 拉到的总条数(0 = 接入点没返回可用模型) */
  total: number
  /** 筛选后的行 */
  rows: ModelChoice[]
  /** 当前模型 id(打 ✓) */
  current: string
  /** 键盘高亮行下标 */
  active: number
}>()
const emit = defineEmits<{ pick: [string]; refresh: []; hover: [number] }>()
const { t } = useI18n()

function errText(err: PickErr): string {
  switch (err.kind) {
    case 'no_api_key': return t('settings.brain.modelsNeedKey')
    case 'need_endpoint': return t('settings.brain.modelsNeedEndpoint')
    case 'bad_api_key': return t('settings.brain.modelsBadKey')
    case 'network': return t('settings.brain.modelsNetwork')
    default: return t('settings.brain.modelsFailed', { err: err.message })
  }
}
// 前置检查类的错(没钥匙 / 没接入点)重拉也白拉,不给「重新拉取」
function retryable(err: PickErr): boolean {
  return err.kind !== 'no_api_key' && err.kind !== 'need_endpoint'
}
function tierLabel(tier: ModelTier): string {
  return t(`settings.brain.tier_${tier}`)
}
function priceTag(c: ModelChoice): string {
  return c.inUsdPerM != null && c.outUsdPerM != null ? `$${c.inUsdPerM} / $${c.outUsdPerM}` : ''
}
</script>

<template>
  <!-- pointerdown.prevent:点行时不让输入框失焦,免得 blur 先把半截文字当模型存上 -->
  <div class="pick-list" role="listbox" @pointerdown.prevent>
    <p v-if="loading" class="pick-msg">{{ t('settings.brain.modelsLoading') }}</p>
    <p v-else-if="err" class="pick-msg warn">
      {{ errText(err) }}
      <button v-if="retryable(err)" class="link" @click="emit('refresh')">{{ t('settings.brain.modelsRefresh') }}</button>
    </p>
    <p v-else-if="!total" class="pick-msg">{{ t('settings.brain.modelsEmpty') }}</p>
    <p v-else-if="!rows.length" class="pick-msg">{{ t('settings.brain.modelsNoMatch') }}</p>
    <template v-else>
      <div
        v-for="(c, i) in rows"
        :key="c.id"
        class="pick-row"
        role="option"
        :aria-selected="c.id === current"
        :class="{ sel: c.id === current, active: i === active, dim: !c.known }"
        @click="emit('pick', c.id)"
        @mousemove="emit('hover', i)"
      >
        <span class="pick-id">{{ c.id }}</span>
        <span v-if="c.known" class="pick-tags">
          <span class="pick-tag">{{ tierLabel(c.tier) }}</span>
          <span v-if="c.vision" class="pick-tag vis">{{ t('settings.brain.visionYes') }}</span>
          <span v-if="priceTag(c)" class="pick-tag price">{{ priceTag(c) }}</span>
        </span>
      </div>
      <p class="pick-msg foot">
        <button class="link" @click="emit('refresh')">{{ t('settings.brain.modelsRefresh') }}</button>
      </p>
    </template>
  </div>
</template>

<style scoped>
/* 弹层样式(.pick-list / .pick-row / .pick-tag / .pick-msg)在全局 style.css —— 接入点预设 ▾(SettingsView
   内联)也用同一套。SettingsView 的 .link 是它的 scoped 样式,进不了本组件(§6.7 scoped 隔离)→ 本地复刻一份 */
.link { background: none; border: none; color: var(--accent); cursor: pointer; font-size: 12.5px; padding: 0; }
</style>
