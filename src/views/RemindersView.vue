<script setup lang="ts">
// 提醒页(rail 目的地,接替原「场景」死按钮):看 7274 替你记下的定时提醒。
// 数据 = jobs 域(用户口头设的提醒,模型翻成绝对时刻 + repeat 枚举,用户永不见 cron)。
// 定位 = 家用「我设了哪些提醒」一览 + 一键取消;气质仿操作记录页(OpsView)。
// 纯浏览器预览:假数据看视觉。
import { computed, onMounted, onUnmounted, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import { api, isTauri, type Reminder } from '../lib/backend'
import { useToast } from '../composables/useToast'

const emit = defineEmits<{ (e: 'close'): void }>()
const { t, te, locale } = useI18n()
const toast = useToast()

const items = ref<Reminder[]>([])
const loaded = ref(false)
/** 加载是否出错:与「空着」分开 —— 出错显「没加载出来 + 重试」,而非误导成「还没有提醒」。 */
const error = ref(false)
/** 正在取消的行 id(按钮转圈、防连点)。 */
const busy = ref<number | null>(null)

const total = computed(() => items.value.length)

async function load() {
  if (!isTauri()) {
    const now = Date.now()
    items.value = [
      mock(1, '提醒奶奶吃降压药', now + 90 * 60_000, 'daily'),
      mock(2, '接孩子放学', now + 5 * 3600_000, 'weekdays'),
      mock(3, '给阳台的花浇水', now + 26 * 3600_000, 'once'),
      mock(4, '周末给爸妈打个电话', now + 3 * 86400_000, 'weekly'),
    ]
    loaded.value = true
    return
  }
  error.value = false
  try {
    items.value = await api.listReminders()
  } catch (e) {
    console.error('加载提醒失败', e)
    error.value = true
  }
  loaded.value = true
}

function mock(id: number, content: string, due_at: number, repeat: string): Reminder {
  return { id, user_id: 1, conv_id: 1, content, due_at, repeat, status: 'pending', kind: 'time', created_at: 0, updated_at: 0 }
}

async function cancel(r: Reminder) {
  if (busy.value != null) return
  busy.value = r.id
  if (isTauri()) {
    try {
      await api.cancelReminder(r.id)
    } catch (e) {
      console.error('取消提醒失败', e)
      toast.error(t('toast.actionFailed'))
      busy.value = null
      return
    }
  }
  // 取消成功 → 移出列表(pending 清单不再含它)
  items.value = items.value.filter((x) => x.id !== r.id)
  busy.value = null
}

/** repeat 徽标:重复类才显(once 单次不挂徽标)。文案全在前端字典(§6)。 */
function repeatLabel(r: Reminder): string {
  const key = `reminders.repeat.${r.repeat}`
  return te(key) ? t(key) : ''
}

/** 友好时刻:今天/明天/本周内周几/更远的日期 + HH:MM。星期名按 locale 出(Intl,zh→周三/en→Wed)。 */
function fmtDue(ts: number): string {
  const d = new Date(ts)
  const pad = (n: number) => String(n).padStart(2, '0')
  const hm = `${pad(d.getHours())}:${pad(d.getMinutes())}`
  const startOfDay = (x: Date) => new Date(x.getFullYear(), x.getMonth(), x.getDate()).getTime()
  const dayDiff = Math.round((startOfDay(d) - startOfDay(new Date())) / 86400_000)
  if (dayDiff === 0) return `${t('reminders.today')} ${hm}`
  if (dayDiff === 1) return `${t('reminders.tomorrow')} ${hm}`
  if (dayDiff > 1 && dayDiff < 7) return `${d.toLocaleDateString(locale.value, { weekday: 'short' })} ${hm}`
  const datePart = `${d.getMonth() + 1}/${d.getDate()}`
  return `${datePart} ${hm}`
}

/** 已到点还没触发(调度器轮询有间隔)→ 暖色点提示「该响了」。 */
function isDue(r: Reminder): boolean {
  // 条件提醒的 due_at 是「下次检查时刻」,不是该响时刻 → 不挂「该响了」点
  return r.kind !== 'cond' && r.due_at <= Date.now()
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
  <section class="rem view-shell">
    <header class="view-head sep" data-tauri-drag-region>
      <div class="view-title">
        <b>{{ t('reminders.title') }}</b>
        <span class="view-mono">7274 · REMINDERS</span>
        <small>{{ t('reminders.tagline') }}</small>
      </div>
      <button class="view-back" @click="emit('close')">{{ t('reminders.back') }}</button>
    </header>

    <div class="view-scroll">
      <p v-if="loaded && total" class="lp-count">{{ t('reminders.count', { n: total }) }}</p>

      <TransitionGroup name="lp" tag="div">
        <div v-for="r in items" :key="r.id" class="lp-card">
          <span class="lp-dot" :class="{ warn: isDue(r) }"></span>
          <span class="lp-text">{{ r.content }}</span>
          <!-- 家人的提醒标归属(提醒页=主人的管理面;自己的不标) -->
          <span v-if="r.owner" class="lp-badge">{{ r.owner }}</span>
          <span v-if="repeatLabel(r)" class="lp-badge">{{ repeatLabel(r) }}</span>
          <span class="lp-date">{{ r.kind === 'cond' ? t('reminders.condition') : fmtDue(r.due_at) }}</span>
          <button class="lp-act attn" :disabled="busy === r.id" @click="cancel(r)">
            {{ t('reminders.cancel') }}
          </button>
        </div>
      </TransitionGroup>

      <div v-if="loaded && error" class="lp-error">
        <p>{{ t('common.loadError') }}</p>
        <button class="lp-retry" @click="load">{{ t('common.retry') }}</button>
      </div>
      <div v-else-if="loaded && !total" class="lp-empty">
        <span class="lp-empty-icon"><svg viewBox="0 0 24 24"><circle cx="12" cy="13" r="8" /><path d="M12 9v4l2.5 1.5" /><path d="M5 4.5 8 7M19 4.5 16 7" /></svg></span>
        <p>{{ t('reminders.empty') }}</p>
      </div>
    </div>
  </section>
</template>

<!-- 外壳 / 卡片 / 空态样式全在 style.css 的 .view-* / .lp-* 共用类(回忆·记录·提醒同款) -->
