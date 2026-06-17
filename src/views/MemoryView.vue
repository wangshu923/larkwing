<script setup lang="ts">
// 回忆页:两本账两个分组 —— 「关于你」(小本本,归人)/「家里的事」(家庭备忘,
// 归任务域;宪法 §6 + PLAN §9:机制同构、数据分账)。删除都走两步确认,防误触。
// 纯浏览器预览:假数据看视觉。
import { computed, onMounted, onUnmounted, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import { api, isTauri, type Briefing, type Memory } from '../lib/backend'

const emit = defineEmits<{ (e: 'close'): void }>()
const { t } = useI18n()

const memories = ref<Memory[]>([])
const briefings = ref<Briefing[]>([])
const loaded = ref(false)
/** 两步删除:第一次点变"确定删?",再点才真删;键带前缀区分两组(m-1 / b-1)。 */
const arming = ref<string | null>(null)

const total = computed(() => memories.value.length + briefings.value.length)

async function load() {
  if (!isTauri()) {
    memories.value = [
      { id: 1, user_id: 1, kind: 'fact', content: '不吃香菜', resident: true, salience: 3, source: 'explicit', last_used_at: Date.now() - 12 * 60_000, created_at: Date.now() - 5 * 86400_000, updated_at: 0 },
      { id: 2, user_id: 1, kind: 'fact', content: '对花生过敏', resident: true, salience: 1, source: 'explicit', last_used_at: null, created_at: Date.now() - 2 * 86400_000, updated_at: 0 },
    ]
    briefings.value = [
      { id: 1, domain: 'media', content: '电影在 \\\\nas\\film;动画片在 \\\\nas\\kids', scope: 'home', resident: true, created_at: Date.now() - 86400_000, updated_at: 0 },
      { id: 2, domain: 'appliance', content: '路由器在客厅电视柜后面', scope: 'home', resident: true, created_at: Date.now() - 3600_000, updated_at: 0 },
    ]
    loaded.value = true
    return
  }
  try {
    const [m, b] = await Promise.all([api.listMemories(), api.listBriefings()])
    memories.value = m
    briefings.value = b
  } catch (e) {
    console.error('加载回忆页失败', e)
  }
  loaded.value = true
}

async function removeMemory(m: Memory) {
  if (arming.value !== `m-${m.id}`) {
    arming.value = `m-${m.id}`
    return
  }
  arming.value = null
  const idx = memories.value.findIndex((x) => x.id === m.id)
  if (idx >= 0) memories.value.splice(idx, 1) // 乐观更新,失败补回
  if (!isTauri()) return
  try {
    await api.deleteMemory(m.id)
  } catch (e) {
    console.error('删除记忆失败', e)
    if (idx >= 0) memories.value.splice(idx, 0, m)
  }
}

async function removeBriefing(b: Briefing) {
  if (arming.value !== `b-${b.id}`) {
    arming.value = `b-${b.id}`
    return
  }
  arming.value = null
  const idx = briefings.value.findIndex((x) => x.id === b.id)
  if (idx >= 0) briefings.value.splice(idx, 1)
  if (!isTauri()) return
  try {
    await api.deleteBriefing(b.id)
  } catch (e) {
    console.error('删除备忘失败', e)
    if (idx >= 0) briefings.value.splice(idx, 0, b)
  }
}

/** 记忆/备忘是长期事实,绝对日期比"几小时前"更称职。 */
function fmtDate(ts: number): string {
  const d = new Date(ts)
  return `${d.getFullYear()}/${d.getMonth() + 1}/${d.getDate()}`
}

/** 「上次想起」用相对时间(召回新鲜度才是重点,与创建日的绝对显示不同)。 */
function fmtAgo(ts: number): string {
  const sec = Math.max(0, (Date.now() - ts) / 1000)
  if (sec < 60) return t('memory.agoJustNow')
  if (sec < 3600) return t('memory.agoMin', { n: Math.floor(sec / 60) })
  if (sec < 86400) return t('memory.agoHour', { n: Math.floor(sec / 3600) })
  return t('memory.agoDay', { n: Math.floor(sec / 86400) })
}

/** 隐身观测(§4.4 hover 浮现):被 recall 工具想起的次数 + 上次几时。
 *  salience = 1.0 + 召回次数(只有 recall 会 +1);last_used_at 在召回时刷新。 */
function recallHint(m: Memory): string {
  const n = Math.round(m.salience) - 1
  return n >= 1 && m.last_used_at
    ? t('memory.recalled', { n, ago: fmtAgo(m.last_used_at) })
    : t('memory.neverRecalled')
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
  <section class="memory view-shell" @click.self="arming = null">
    <header class="view-head sep" data-tauri-drag-region>
      <div class="view-title">
        <b>{{ t('memory.title') }}</b>
        <span class="view-mono">7274 · MEMORY</span>
        <small>{{ t('memory.tagline') }}</small>
      </div>
      <button class="view-back" @click="emit('close')">{{ t('memory.back') }}</button>
    </header>

    <div class="view-scroll">
      <p v-if="loaded && total" class="lp-count">{{ t('memory.count', { n: total }) }}</p>

      <!-- 关于你(小本本) -->
      <p v-if="memories.length" class="lp-group">{{ t('memory.groupYou') }}</p>
      <TransitionGroup name="lp" tag="div">
        <div v-for="m in memories" :key="`m-${m.id}`" class="lp-card top" :title="recallHint(m)">
          <span class="lp-dot sm"></span>
          <span class="lp-text multiline">{{ m.content }}</span>
          <span class="lp-date top">{{ fmtDate(m.created_at) }}</span>
          <button
            class="lp-act hoveronly"
            :class="{ armed: arming === `m-${m.id}` }"
            @click.stop="removeMemory(m)"
          >
            {{ arming === `m-${m.id}` ? t('memory.confirm') : '✕' }}
          </button>
        </div>
      </TransitionGroup>

      <!-- 家里的事(家庭备忘) -->
      <p v-if="briefings.length" class="lp-group">{{ t('memory.groupHome') }}</p>
      <TransitionGroup name="lp" tag="div">
        <div v-for="b in briefings" :key="`b-${b.id}`" class="lp-card top">
          <span class="lp-chip">{{ b.domain }}</span>
          <span class="lp-text multiline">{{ b.content }}</span>
          <span class="lp-date top">{{ fmtDate(b.updated_at || b.created_at) }}</span>
          <button
            class="lp-act hoveronly"
            :class="{ armed: arming === `b-${b.id}` }"
            @click.stop="removeBriefing(b)"
          >
            {{ arming === `b-${b.id}` ? t('memory.confirm') : '✕' }}
          </button>
        </div>
      </TransitionGroup>

      <div v-if="loaded && !total" class="lp-empty">
        <span class="lp-empty-icon"><svg viewBox="0 0 24 24"><path d="M7 4h10v16l-5-3-5 3z" /></svg></span>
        <p>{{ t('memory.empty') }}</p>
      </div>
    </div>
  </section>
</template>

<!-- 外壳 / 卡片 / 空态样式全在 style.css 的 .view-* / .lp-* 共用类(回忆·记录·提醒同款) -->

