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
      { id: 1, user_id: 1, kind: 'fact', content: '不吃香菜', created_at: Date.now() - 5 * 86400_000, updated_at: 0 },
      { id: 2, user_id: 1, kind: 'fact', content: '对花生过敏', created_at: Date.now() - 2 * 86400_000, updated_at: 0 },
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
  <section class="memory" @click.self="arming = null">
    <header class="m-head" data-tauri-drag-region>
      <div class="m-title">
        <b>{{ t('memory.title') }}</b>
        <span class="m-mono">7274 · MEMORY</span>
        <small>{{ t('memory.tagline') }}</small>
      </div>
      <button class="m-back" @click="emit('close')">{{ t('memory.back') }}</button>
    </header>

    <div class="m-body">
      <p v-if="loaded && total" class="m-count">{{ t('memory.count', { n: total }) }}</p>

      <!-- 关于你(小本本) -->
      <p v-if="memories.length" class="m-group">{{ t('memory.groupYou') }}</p>
      <TransitionGroup name="mem" tag="div">
        <div v-for="m in memories" :key="`m-${m.id}`" class="mem-card">
          <span class="mem-dot"></span>
          <span class="mem-text">{{ m.content }}</span>
          <span class="mem-date">{{ fmtDate(m.created_at) }}</span>
          <button
            class="mem-del"
            :class="{ arming: arming === `m-${m.id}` }"
            @click.stop="removeMemory(m)"
          >
            {{ arming === `m-${m.id}` ? t('memory.confirm') : '✕' }}
          </button>
        </div>
      </TransitionGroup>

      <!-- 家里的事(家庭备忘) -->
      <p v-if="briefings.length" class="m-group">{{ t('memory.groupHome') }}</p>
      <TransitionGroup name="mem" tag="div">
        <div v-for="b in briefings" :key="`b-${b.id}`" class="mem-card">
          <span class="mem-chip">{{ b.domain }}</span>
          <span class="mem-text">{{ b.content }}</span>
          <span class="mem-date">{{ fmtDate(b.updated_at || b.created_at) }}</span>
          <button
            class="mem-del"
            :class="{ arming: arming === `b-${b.id}` }"
            @click.stop="removeBriefing(b)"
          >
            {{ arming === `b-${b.id}` ? t('memory.confirm') : '✕' }}
          </button>
        </div>
      </TransitionGroup>

      <div v-if="loaded && !total" class="m-empty">
        <span class="m-empty-icon">📖</span>
        <p>{{ t('memory.empty') }}</p>
      </div>
    </div>
  </section>
</template>

<style scoped>
.memory { flex: 1; display: flex; flex-direction: column; min-width: 0; padding: 18px 26px; overflow-y: auto; }
/* padding-right 让「回去聊天」避开右上角窗控三键(二轮真机修复:不再重叠) */
.m-head { display: flex; align-items: flex-start; justify-content: space-between; margin-bottom: 14px; padding-right: 70px; }
.m-title b { font-size: 16px; color: var(--txt); }
.m-title small { display: block; margin-top: 3px; font-size: 12px; color: var(--txt2); }
.m-mono { font-family: ui-monospace, "SF Mono", monospace; font-size: 10px; letter-spacing: 2px; color: var(--txt2); margin-left: 8px; }
.m-back { background: none; border: 1px solid var(--line); border-radius: 9px; color: var(--txt2); cursor: pointer; padding: 5px 10px; font-size: 12px; }
.m-back:hover { color: var(--cy); border-color: var(--cy); }

.m-body { max-width: 640px; }
.m-count { margin: 0 0 10px; font-size: 11.5px; letter-spacing: 2px; color: var(--txt2); }
.m-group { margin: 16px 0 9px; font-size: 11.5px; letter-spacing: 2px; color: var(--txt2); }
.m-group:first-of-type { margin-top: 0; }

.mem-card {
  display: flex; align-items: center; gap: 10px;
  border: 1px solid var(--line); border-radius: 12px; padding: 11px 14px; margin-bottom: 8px;
  background: rgba(95, 200, 255, 0.03); font-size: 13.5px;
  transition: border-color .15s;
}
.mem-card:hover { border-color: rgba(95, 200, 255, 0.4); }
.mem-dot { width: 5px; height: 5px; border-radius: 50%; background: var(--cy); box-shadow: 0 0 6px var(--cy); flex: none; opacity: .8; }
.mem-chip {
  flex: none; font: 10px/1 ui-monospace, "SF Mono", monospace; letter-spacing: 1px;
  color: var(--cy); border: 1px solid rgba(95, 200, 255, 0.35); border-radius: 6px;
  padding: 3px 7px; text-transform: uppercase;
}
.mem-text { flex: 1; min-width: 0; color: var(--txt); line-height: 1.5; word-break: break-word; }
.mem-date { flex: none; font: 10.5px/1 ui-monospace, "SF Mono", monospace; letter-spacing: .5px; color: var(--txt2); }
.mem-del {
  flex: none; background: none; border: 1px solid transparent; border-radius: 8px;
  color: var(--txt2); cursor: pointer; font-size: 11px; padding: 3px 8px;
  opacity: 0; transition: opacity .15s, color .15s, border-color .15s;
}
.mem-card:hover .mem-del, .mem-del.arming { opacity: 1; }
.mem-del:hover { color: #ffb86b; }
.mem-del.arming { color: #ffb86b; border-color: rgba(255, 184, 107, 0.45); }

.m-empty { padding: 42px 0; text-align: center; color: var(--txt2); font-size: 13.5px; line-height: 1.8; }
.m-empty-icon { font-size: 26px; display: block; margin-bottom: 8px; opacity: .7; }

.mem-leave-to { opacity: 0; transform: translateX(12px); }
.mem-leave-active { transition: all .22s ease; }
</style>
