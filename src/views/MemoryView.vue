<script setup lang="ts">
// 回忆页:两本账两个分组 —— 「关于你」(小本本,归人)/「家里的事」(家庭备忘,
// 归任务域;宪法 §6 + PLAN §9:机制同构、数据分账)。删除都走两步确认,防误触。
// 纯浏览器预览:假数据看视觉。
import { computed, onMounted, onUnmounted, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import { api, isTauri, type Briefing, type FamilyMember, type Memory } from '../lib/backend'
import { useContextMenu } from '../composables/useContextMenu'
import { hydrateUser, useSettings } from '../composables/useSettings'
import { useToast } from '../composables/useToast'
import { copyText } from '../lib/clipboard'
import SkinSelect from '../components/SkinSelect.vue'

const emit = defineEmits<{ (e: 'close'): void }>()
const { t } = useI18n()
const { openMenu } = useContextMenu()
const settings = useSettings()
const toast = useToast()

/** 自动记住开关(memory.auto_consolidate,默认开):回忆页是记忆的唯一用户触点(§7.3),
 *  开关安在它产出的记忆上方,因果一目了然。关掉只停后台自动提炼,手动/对话里记不受影响。 */
const autoRemember = computed(() => settings.get('memory.auto_consolidate') !== '0')
function toggleAuto() {
  void settings.set('memory.auto_consolidate', autoRemember.value ? '0' : '1')
}

const memories = ref<Memory[]>([])
const briefings = ref<Briefing[]>([])
const loaded = ref(false)
/** 加载是否出错:与「空着」分开 —— 出错显「没加载出来 + 重试」,而非误导成「还没有记忆」。 */
const error = ref(false)
/** 两步删除:第一次点变"确定删?",再点才真删;键带前缀区分两组(m-1 / b-1)。 */
const arming = ref<string | null>(null)

const total = computed(() => memories.value.length + briefings.value.length)

// 看谁的记忆(§渠道归人第二步「主人查看家人记忆」):主人换视角看家人的小本本 —— **不是切换用户**,
// 只是过滤视图。共享的「家里的事」(home-scope 需知)对谁都在,只有归人的小本本随人切。
const family = ref<FamilyMember[]>([])
/** 0 = 当前主人(默认);>0 = 指认查看某家人。 */
const viewUser = ref(0)
const showPicker = computed(() => family.value.length > 1)
const viewingSelf = computed(() => !viewUser.value || viewUser.value === settings.state.userId)
/** 传给后端的目标 user_id:主人视角传 undefined(走 ensure_default_user),家人传其 id。 */
const targetId = computed(() => (viewingSelf.value ? undefined : viewUser.value))
const viewingName = computed(() => family.value.find((m) => m.id === viewUser.value)?.name ?? '')
const famOptions = computed(() =>
  family.value.map((m) => ({
    // settings.family.you 本身已含括号「(你)」,直接拼、别再套一层
    value: String(m.id),
    label: m.id === settings.state.userId ? `${m.name}${t('settings.family.you')}` : m.name,
  })),
)

/** 只重拉记忆/备忘(切人时调;家人列表不用重拉)。 */
async function loadEntries() {
  if (!isTauri()) {
    loadFakeEntries()
    loaded.value = true
    return
  }
  error.value = false
  try {
    const [m, b] = await Promise.all([api.listMemories(targetId.value), api.listBriefings(targetId.value)])
    memories.value = m
    briefings.value = b
  } catch (e) {
    console.error('加载回忆页失败', e)
    error.value = true
  }
  loaded.value = true
}

function onPickUser(v: string) {
  viewUser.value = Number(v) || 0
  void loadEntries()
}

async function load() {
  if (isTauri()) {
    family.value = await api.listFamily().catch(() => [])
    viewUser.value = settings.state.userId
  } else {
    if (!settings.state.userId) hydrateUser(1, settings.state.userName)
    family.value = [
      { id: 1, name: settings.state.userName, skin_id: 'scifi', created_at: 0, last_active_at: 0, enrolled: true },
      { id: 2, name: '豆豆', skin_id: 'scifi', created_at: 0, last_active_at: 0, enrolled: true },
    ]
    viewUser.value = 1
  }
  await loadEntries()
}

// 浏览器预览:小本本随「看谁」变(演示归人切换),家里的事共享不变(演示共同记忆)
function loadFakeEntries() {
  memories.value =
    viewUser.value === 2
      ? [
          { id: 10, user_id: 2, kind: 'fact', content: '喜欢恐龙,怕打雷', resident: true, salience: 2, source: 'distilled', last_used_at: Date.now() - 3600_000, created_at: Date.now() - 3 * 86400_000, updated_at: 0 },
          { id: 11, user_id: 2, kind: 'fact', content: '睡前要听一个故事', resident: true, salience: 1, source: 'explicit', last_used_at: null, created_at: Date.now() - 86400_000, updated_at: 0 },
        ]
      : [
          { id: 1, user_id: 1, kind: 'fact', content: '不吃香菜', resident: true, salience: 3, source: 'explicit', last_used_at: Date.now() - 12 * 60_000, created_at: Date.now() - 5 * 86400_000, updated_at: 0 },
          { id: 2, user_id: 1, kind: 'fact', content: '对花生过敏', resident: true, salience: 1, source: 'explicit', last_used_at: null, created_at: Date.now() - 2 * 86400_000, updated_at: 0 },
        ]
  briefings.value = [
    { id: 1, domain: 'media', content: '电影在 \\\\nas\\film;动画片在 \\\\nas\\kids', scope: 'home', resident: true, created_at: Date.now() - 86400_000, updated_at: 0 },
    { id: 2, domain: 'appliance', content: '路由器在客厅电视柜后面', scope: 'home', resident: true, created_at: Date.now() - 3600_000, updated_at: 0 },
  ]
}

/** 真删记忆(乐观更新,失败补回)。hover ✕ 走两步确认后调它;右键菜单直接调(右键本身已是明确动作)。 */
async function doRemoveMemory(m: Memory) {
  const idx = memories.value.findIndex((x) => x.id === m.id)
  if (idx >= 0) memories.value.splice(idx, 1)
  if (!isTauri()) return
  try {
    await api.deleteMemory(m.id, targetId.value) // 看家人时删的是 TA 的记忆(主人管理面)
  } catch (e) {
    console.error('删除记忆失败', e)
    if (idx >= 0) memories.value.splice(idx, 0, m)
    toast.error(t('toast.deleteFailed'))
  }
}

async function removeMemory(m: Memory) {
  if (arming.value !== `m-${m.id}`) {
    arming.value = `m-${m.id}`
    return
  }
  arming.value = null
  await doRemoveMemory(m)
}

async function doRemoveBriefing(b: Briefing) {
  const idx = briefings.value.findIndex((x) => x.id === b.id)
  if (idx >= 0) briefings.value.splice(idx, 1)
  if (!isTauri()) return
  try {
    await api.deleteBriefing(b.id)
  } catch (e) {
    console.error('删除备忘失败', e)
    if (idx >= 0) briefings.value.splice(idx, 0, b)
    toast.error(t('toast.deleteFailed'))
  }
}

async function removeBriefing(b: Briefing) {
  if (arming.value !== `b-${b.id}`) {
    arming.value = `b-${b.id}`
    return
  }
  arming.value = null
  await doRemoveBriefing(b)
}

// 右键菜单:复制内容 / 删除(右键的删除直达,不走 hover 那套两步确认)
function openMemoryMenu(e: MouseEvent, m: Memory) {
  openMenu(e, [
    { label: t('ctx.copy'), action: () => copyText(m.content) },
    { separator: true },
    { label: t('ctx.delete'), danger: true, action: () => doRemoveMemory(m) },
  ])
}
function openBriefingMenu(e: MouseEvent, b: Briefing) {
  openMenu(e, [
    { label: t('ctx.copy'), action: () => copyText(b.content) },
    { separator: true },
    { label: t('ctx.delete'), danger: true, action: () => doRemoveBriefing(b) },
  ])
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
      <!-- 看谁的记忆(§渠道归人第二步):主人换视角看家人小本本;不是切换用户,只是过滤视图。
           家里的事(home 共享)对谁都在,只有归人的小本本随人切。 -->
      <div v-if="showPicker" class="mem-who">
        <b class="mem-who-title">{{ t('memory.whoTitle') }}</b>
        <SkinSelect
          class="mem-who-sel"
          :model-value="String(viewUser)"
          :options="famOptions"
          :aria-label="t('memory.whoTitle')"
          @update:model-value="onPickUser"
        />
      </div>

      <!-- 自动记住开关(§13 Phase 3):只在看自己时显(是主人自己的设置);看家人时改显一句说明 -->
      <div v-if="viewingSelf" class="mem-auto">
        <b class="mem-auto-title">{{ t('memory.autoTitle') }}</b>
        <span class="key-state">
          <span class="chip" :class="{ on: autoRemember }">{{ autoRemember ? t('memory.autoOn') : t('memory.autoOff') }}</span>
          <button class="link" type="button" @click="toggleAuto">
            {{ autoRemember ? t('memory.autoTurnOff') : t('memory.autoTurnOn') }}
          </button>
        </span>
      </div>
      <p v-else class="hint mem-viewing">{{ t('memory.viewingFamily', { name: viewingName }) }}</p>

      <p v-if="loaded && total" class="lp-count">{{ t('memory.count', { n: total }) }}</p>

      <!-- 关于你(小本本) -->
      <p v-if="memories.length" class="lp-group">{{ t('memory.groupYou') }}</p>
      <TransitionGroup name="lp" tag="div">
        <div v-for="m in memories" :key="`m-${m.id}`" class="lp-card top" :title="recallHint(m)" @contextmenu="openMemoryMenu($event, m)">
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
        <div v-for="b in briefings" :key="`b-${b.id}`" class="lp-card top" @contextmenu="openBriefingMenu($event, b)">
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

      <div v-if="loaded && error" class="lp-error">
        <p>{{ t('common.loadError') }}</p>
        <button class="lp-retry" @click="load">{{ t('common.retry') }}</button>
      </div>
      <div v-else-if="loaded && !total" class="lp-empty">
        <span class="lp-empty-icon"><svg viewBox="0 0 24 24"><path d="M7 4h10v16l-5-3-5 3z" /></svg></span>
        <p>{{ t('memory.empty') }}</p>
      </div>
    </div>
  </section>
</template>

<!-- 外壳 / 卡片 / 空态样式全在 style.css 的 .view-* / .lp-* 共用类(回忆·记录·提醒同款) -->

<!-- 自动记住开关:回忆页专属、自包含;只用语义 token(§6.7 绝不写死颜色),换肤自动跟随 -->
<style scoped>
/* 看谁的记忆:主人切视角(§渠道归人第二步)。SkinSelect 皮肤化下拉,全语义 token */
.mem-who {
  display: flex;
  align-items: center;
  gap: 12px;
  margin-bottom: 14px;
}
.mem-who-title {
  font-size: 13.5px;
  font-weight: 600;
  color: var(--text);
  flex: 0 0 auto;
}
.mem-who-sel {
  min-width: 160px;
}
.mem-viewing {
  margin-top: 0;
  margin-bottom: 14px;
}
.mem-auto {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 14px;
  padding: 9px 15px;
  margin-bottom: 14px;
  background: var(--surface);
  border: 1px solid var(--line);
  border-radius: 11px;
}
.mem-auto-title {
  font-size: 13.5px;
  font-weight: 600;
  color: var(--text);
  min-width: 0;
}
/* 与设置页开关一致(开机自启 / 悬浮窗):状态徽章 + 文字链接,纯语义 token */
.key-state {
  display: inline-flex;
  align-items: center;
  gap: 10px;
}
.chip {
  border: 1px solid var(--line);
  border-radius: 9px;
  padding: 4px 11px;
  font-size: 12.5px;
  color: var(--text);
}
.chip.on {
  border-color: rgba(var(--accent-rgb), 0.45);
  color: var(--accent);
}
.link {
  background: none;
  border: none;
  color: var(--accent);
  cursor: pointer;
  font-size: 12.5px;
  padding: 0;
}
</style>

