<script setup lang="ts">
// 足迹页(PLAN §9 文件能力):看 7274 动过哪些文件,按批次列 + 一键撤销/重做。
// 定位 = 普通文件管理器的历史功能(功能性,**非安全承诺**);气质仿回忆页(MemoryView)。
// 「确认过的操作」分组(§7.8 确认闸的审计半边,2026-07-15 兑现当年预留):一次确认一行
// (动作 + 站点 + 结果 + 谁点的),只看不改;不叫「审批」,零新概念(§3)。
// 纯浏览器预览:假数据看视觉。
import { computed, onMounted, onUnmounted, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import { api, isTauri, type ConfirmLog, type FsOp } from '../lib/backend'
import { confirmActionPhrase } from '../composables/useConfirm'
import { useToast } from '../composables/useToast'
import { useSettings } from '../composables/useSettings'

const emit = defineEmits<{ (e: 'close'): void }>()
const { t, te } = useI18n()
const toast = useToast()
const settings = useSettings()
// 名字跟随用户设置(ui.pet_name 空 = 默认名 pet.name);徽章绝不硬编 7274/旺财(§6.6 名字准则)。
const petName = computed(() => settings.get('ui.pet_name') || t('pet.name'))

const ops = ref<FsOp[]>([])
const confirms = ref<ConfirmLog[]>([])
const loaded = ref(false)
/** 加载是否出错:与「空着」分开 —— 出错显「没加载出来 + 重试」,而非误导成「还没有记录」。 */
const error = ref(false)
/** 正在撤销/重做的行 id(按钮转圈、防连点)。 */
const busy = ref<number | null>(null)

const total = computed(() => ops.value.length)

async function load() {
  if (!isTauri()) {
    const now = Date.now()
    ops.value = [
      { id: 3, user_id: 1, kind: 'move', ops: '[]', n: 42, state: 'applied', created_at: now - 3600_000, updated_at: 0 },
      { id: 2, user_id: 1, kind: 'trash', ops: '[]', n: 3, state: 'applied', created_at: now - 7200_000, updated_at: 0 },
      { id: 1, user_id: 1, kind: 'append', ops: '[]', n: 1, state: 'undone', created_at: now - 86400_000, updated_at: 0 },
    ]
    confirms.value = [
      { id: 2, user_id: 1, conv_id: 1, origin: 'ui', host: 'pay.example.com', action: '确认支付 ¥128.00', kind: 'click', decision: 'allowed', via: 'desktop', created_at: now - 1800_000 },
      { id: 1, user_id: 1, conv_id: 1, origin: 'weixin', host: 'shop.example.com', action: 'Delete', kind: 'click', decision: 'denied', via: 'timeout', created_at: now - 90000_000 },
    ]
    loaded.value = true
    return
  }
  error.value = false
  try {
    ops.value = await api.listFsops()
    confirms.value = await api.listConfirms()
  } catch (e) {
    console.error('加载操作记录失败', e)
    error.value = true
  }
  loaded.value = true
}

/** 确认行结果:允许(谁点的)/ 没执行(超时/拒/没送到,细节进 via 徽章)。 */
function confirmBadge(c: ConfirmLog): string {
  const key = `ops.confirm.via.${c.via}`
  return te(key) ? t(key) : c.via
}

async function undo(o: FsOp) {
  if (busy.value != null) return
  busy.value = o.id
  const prev = o.state
  o.state = 'undone' // 乐观更新
  if (isTauri()) {
    try {
      await api.fsopsUndo(o.id)
    } catch (e) {
      console.error('撤销失败', e)
      o.state = prev
      toast.error(t('toast.actionFailed'))
    }
  }
  busy.value = null
}

async function redo(o: FsOp) {
  if (busy.value != null) return
  busy.value = o.id
  const prev = o.state
  o.state = 'applied'
  if (isTauri()) {
    try {
      await api.fsopsRedo(o.id)
    } catch (e) {
      console.error('重做失败', e)
      o.state = prev
      toast.error(t('toast.actionFailed'))
    }
  }
  busy.value = null
}

/** 一批的人话摘要:按 kind 套 i18n 模板(未知 kind 兜底)。文案全在前端字典(§6)。 */
function summary(o: FsOp): string {
  const key = `ops.kind.${o.kind}`
  return te(key) ? t(key, { n: o.n }) : t('ops.kind.unknown', { n: o.n })
}

/** 删除走回收站那批用暖色点;其余用青色(纯视觉区分,不是危险警告)。 */
function dotClass(o: FsOp): string {
  return o.kind === 'trash' ? 'warn' : ''
}

function fmtDate(ts: number): string {
  const d = new Date(ts)
  const pad = (n: number) => String(n).padStart(2, '0')
  return `${d.getFullYear()}/${d.getMonth() + 1}/${d.getDate()} ${pad(d.getHours())}:${pad(d.getMinutes())}`
}

function onKeydown(e: KeyboardEvent) {
  if (e.key === 'Escape') emit('close')
}
onMounted(() => {
  void load()
  window.addEventListener('keydown', onKeydown)
})
onUnmounted(() => window.removeEventListener('keydown', onKeydown))
</script>

<template>
  <section class="ops view-shell">
    <header class="view-head sep" data-tauri-drag-region>
      <div class="view-title">
        <b>{{ t('ops.title') }}</b>
        <span class="view-mono">{{ petName }} · FILES</span>
        <small>{{ t('ops.tagline') }}</small>
      </div>
      <button class="view-back" @click="emit('close')">{{ t('ops.back') }}</button>
    </header>

    <div class="view-scroll">
      <p v-if="loaded && total" class="lp-count">{{ t('ops.count', { n: total }) }}</p>

      <TransitionGroup name="lp" tag="div">
        <div v-for="o in ops" :key="o.id" class="lp-card" :class="{ muted: o.state === 'undone' }">
          <span class="lp-dot" :class="dotClass(o)"></span>
          <span class="lp-text">{{ summary(o) }}</span>
          <span v-if="o.state === 'undone'" class="lp-badge">{{ t('ops.undone') }}</span>
          <span class="lp-date">{{ fmtDate(o.created_at) }}</span>
          <button
            v-if="o.state === 'applied'"
            class="lp-act attn"
            :disabled="busy === o.id"
            @click="undo(o)"
          >
            {{ t('ops.undo') }}
          </button>
          <button v-else class="lp-act cy" :disabled="busy === o.id" @click="redo(o)">
            {{ t('ops.redo') }}
          </button>
        </div>
      </TransitionGroup>

      <!-- 确认过的操作(§7.8 审计):有记录才显分组;只看不改 -->
      <template v-if="confirms.length">
        <p class="lp-count">{{ t('ops.confirm.title', { n: confirms.length }) }}</p>
        <TransitionGroup name="lp" tag="div">
          <div v-for="c in confirms" :key="'cfm-' + c.id" class="lp-card" :class="{ muted: c.decision !== 'allowed' }">
            <span class="lp-dot" :class="c.decision === 'allowed' ? '' : 'warn'"></span>
            <span class="lp-text">{{ confirmActionPhrase(c) }}<small v-if="c.host" class="cfm-host"> · {{ c.host }}</small></span>
            <span class="lp-badge">{{ c.decision === 'allowed' ? t('ops.confirm.allowed') : t('ops.confirm.denied') }} · {{ confirmBadge(c) }}</span>
            <span class="lp-date">{{ fmtDate(c.created_at) }}</span>
          </div>
        </TransitionGroup>
      </template>

      <div v-if="loaded && error" class="lp-error">
        <p>{{ t('common.loadError') }}</p>
        <button class="lp-retry" @click="load">{{ t('common.retry') }}</button>
      </div>
      <div v-else-if="loaded && !total && !confirms.length" class="lp-empty">
        <span class="lp-empty-icon"><svg viewBox="0 0 24 24"><g transform="translate(8 13.5) rotate(-16)"><ellipse cx="0" cy="-1.9" rx="2.1" ry="2.6" /><ellipse cx="-0.1" cy="2.5" rx="1.2" ry="1.5" /></g><g transform="translate(15.6 9) rotate(-16)"><ellipse cx="0" cy="-1.9" rx="2.1" ry="2.6" /><ellipse cx="-0.1" cy="2.5" rx="1.2" ry="1.5" /></g></svg></span>
        <p>{{ t('ops.empty') }}</p>
      </div>
    </div>
  </section>
</template>

<!-- 外壳 / 卡片 / 空态样式全在 style.css 的 .view-* / .lp-* 共用类(回忆·记录·提醒同款) -->
<style scoped>
/* 确认行的站点小注(唯一页面私有样式;其余全走 .lp-* 共用类) */
.cfm-host { color: var(--text-dim); font-size: 11px; }
</style>
