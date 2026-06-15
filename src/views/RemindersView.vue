<script setup lang="ts">
// 提醒页(rail 目的地,接替原「场景」死按钮):看 7274 替你记下的定时提醒。
// 数据 = jobs 域(用户口头设的提醒,模型翻成绝对时刻 + repeat 枚举,用户永不见 cron)。
// 定位 = 家用「我设了哪些提醒」一览 + 一键取消;气质仿操作记录页(OpsView)。
// 纯浏览器预览:假数据看视觉。
import { computed, onMounted, onUnmounted, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import { api, isTauri, type Reminder } from '../lib/backend'

const emit = defineEmits<{ (e: 'close'): void }>()
const { t, te, locale } = useI18n()

const items = ref<Reminder[]>([])
const loaded = ref(false)
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
  try {
    items.value = await api.listReminders()
  } catch (e) {
    console.error('加载提醒失败', e)
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
  <section class="rem">
    <header class="r-head" data-tauri-drag-region>
      <div class="r-title">
        <b>{{ t('reminders.title') }}</b>
        <span class="r-mono">7274 · REMINDERS</span>
        <small>{{ t('reminders.tagline') }}</small>
      </div>
      <button class="r-back" @click="emit('close')">{{ t('reminders.back') }}</button>
    </header>

    <div class="r-body">
      <p v-if="loaded && total" class="r-count">{{ t('reminders.count', { n: total }) }}</p>

      <TransitionGroup name="rem" tag="div">
        <div v-for="r in items" :key="r.id" class="rem-card">
          <span class="rem-dot" :class="{ due: isDue(r) }"></span>
          <span class="rem-text">{{ r.content }}</span>
          <span v-if="repeatLabel(r)" class="rem-badge">{{ repeatLabel(r) }}</span>
          <span class="rem-when">{{ r.kind === 'cond' ? t('reminders.condition') : fmtDue(r.due_at) }}</span>
          <button class="rem-act" :disabled="busy === r.id" @click="cancel(r)">
            {{ t('reminders.cancel') }}
          </button>
        </div>
      </TransitionGroup>

      <div v-if="loaded && !total" class="r-empty">
        <span class="r-empty-icon">⏰</span>
        <p>{{ t('reminders.empty') }}</p>
      </div>
    </div>
  </section>
</template>

<style scoped>
.rem { flex: 1; display: flex; flex-direction: column; min-width: 0; padding: 18px 26px; overflow-y: auto; }
.r-head { display: flex; align-items: flex-start; justify-content: space-between; margin-bottom: 14px; padding-right: 70px; }
.r-title b { font-size: 16px; color: var(--txt); }
.r-title small { display: block; margin-top: 3px; font-size: 12px; color: var(--txt2); }
.r-mono { font-family: ui-monospace, "SF Mono", monospace; font-size: 10px; letter-spacing: 2px; color: var(--txt2); margin-left: 8px; }
.r-back { background: none; border: 1px solid var(--line); border-radius: 9px; color: var(--txt2); cursor: pointer; padding: 5px 10px; font-size: 12px; }
.r-back:hover { color: var(--cy); border-color: var(--cy); }

.r-body { max-width: 640px; }
.r-count { margin: 0 0 10px; font-size: 11.5px; letter-spacing: 2px; color: var(--txt2); }

.rem-card {
  display: flex; align-items: center; gap: 10px;
  border: 1px solid var(--line); border-radius: 12px; padding: 11px 14px; margin-bottom: 8px;
  background: rgba(95, 200, 255, 0.03); font-size: 13.5px;
  transition: border-color .15s, opacity .15s;
}
.rem-card:hover { border-color: rgba(95, 200, 255, 0.4); }
.rem-dot { width: 6px; height: 6px; border-radius: 50%; background: var(--cy); box-shadow: 0 0 6px var(--cy); flex: none; opacity: .85; }
.rem-dot.due { background: #ffb86b; box-shadow: 0 0 6px #ffb86b; }
.rem-text { flex: 1; min-width: 0; color: var(--txt); line-height: 1.5; word-break: break-word; }
.rem-badge {
  flex: none; font: 10px/1 ui-monospace, "SF Mono", monospace; letter-spacing: 1px;
  color: var(--txt2); border: 1px solid var(--line); border-radius: 6px; padding: 3px 7px;
}
.rem-when { flex: none; font: 10.5px/1 ui-monospace, "SF Mono", monospace; letter-spacing: .5px; color: var(--txt2); }
.rem-act {
  flex: none; background: none; border: 1px solid transparent; border-radius: 8px;
  color: var(--txt2); cursor: pointer; font-size: 11.5px; padding: 4px 10px;
  transition: opacity .15s, color .15s, border-color .15s;
}
.rem-card:hover .rem-act { color: #ffb86b; border-color: rgba(255, 184, 107, 0.45); }
.rem-act:disabled { opacity: 0.4; cursor: default; }

.r-empty { padding: 42px 0; text-align: center; color: var(--txt2); font-size: 13.5px; line-height: 1.8; }
.r-empty-icon { font-size: 26px; display: block; margin-bottom: 8px; opacity: .7; }

.rem-leave-to { opacity: 0; transform: translateX(12px); }
.rem-leave-active { transition: all .22s ease; }
</style>
