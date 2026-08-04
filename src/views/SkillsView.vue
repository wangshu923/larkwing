<script setup lang="ts">
// 技能页:agent 的工作手册,恒全局(与用户无关;记忆才归人)。列表 = 内置 + 用户教的,
// 每条带触发统计三数字(总/近7天/最近;触发 = 模型 skill_lookup 命中一次)。
// 管理 = 停用(内置/用户教的都可)+ 删除(仅用户教的;内置可关不可删)。
// 纯浏览器预览:假数据看视觉。
import { computed, onMounted, onUnmounted, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import { api, isTauri, type SkillItem } from '../lib/backend'
import { useContextMenu } from '../composables/useContextMenu'
import { useSettings } from '../composables/useSettings'
import { useToast } from '../composables/useToast'
import { copyText } from '../lib/clipboard'

const emit = defineEmits<{ (e: 'close'): void }>()
const { t } = useI18n()
const { openMenu } = useContextMenu()
const settings = useSettings()
const toast = useToast()
// 名字跟随用户设置(§6.6 名字准则:徽章绝不硬编)。
const petName = computed(() => settings.get('ui.pet_name') || t('pet.name'))

const skills = ref<SkillItem[]>([])
const loaded = ref(false)
/** 出错 ≠ 空:错显「没加载出来 + 重试」,别误导成「还没有技能」(§6.6)。 */
const error = ref(false)
/** 两步删除(仅用户教的条目)。 */
const arming = ref<number | null>(null)
/** 点开看正文(内联展开,一次一条)。 */
const expanded = ref<number | null>(null)

async function load() {
  if (!isTauri()) {
    loadFake()
    loaded.value = true
    return
  }
  error.value = false
  try {
    skills.value = await api.listSkills()
  } catch (e) {
    console.error('加载技能页失败', e)
    error.value = true
  }
  loaded.value = true
}

// 浏览器预览:内置几条 + 一条用户教的,顺带演示统计三态(常用/没用过/停用)
function loadFake() {
  const now = Date.now()
  skills.value = [
    { id: 1, slug: 'media-playback', name: '放歌放视频', when_to_use: '用户点播任何歌曲、儿歌、电影、剧集时', content: '先本地、后网络,找到就直接放:\n1. 任务需知里登记过本地媒体目录的,先去那里找;\n2. 本地没有再上网搜,挑最合适的一条直接放;\n3. 需要登录会自动弹扫码,不算失败。', enabled: true, source: 'builtin', created_at: now - 30 * 86400_000, updated_at: now - 86400_000, total_hits: 42, recent_hits: 6, last_hit_at: now - 3600_000, sections: [] },
    { id: 2, slug: 'web-tasks', name: '网页上办事', when_to_use: '要打开网页下载文件、查信息、点按钮填表单时', content: '从轻到重:先静态读页面,不行再开真浏览器按编号操作;登录验证码交给用户;下载落盘后按需转图、发手机。', enabled: true, source: 'builtin', created_at: now - 30 * 86400_000, updated_at: now - 86400_000, total_hits: 3, recent_hits: 0, last_hit_at: now - 12 * 86400_000, sections: ['登录墙处理'] },
    { id: 3, slug: 'torrent-video', name: 'BT下载', when_to_use: '用户给出磁力链或种子文件要下载时', content: '种子文件比磁力链可靠;下载转后台,下完自动汇报;失败按原因如实说。', enabled: false, source: 'builtin', created_at: now - 30 * 86400_000, updated_at: now - 86400_000, total_hits: 0, recent_hits: 0, last_hit_at: null, sections: [] },
    { id: 4, slug: null, name: '备份照片', when_to_use: '用户说「备份照片」或要把相册拷去备份盘时', content: '1. 到相册文件夹里找本月的图片;\n2. 在备份盘按月份建文件夹;\n3. 拷过去(原图留在相册);\n4. 完成后汇报拷了几张。', enabled: true, source: 'user', created_at: now - 5 * 86400_000, updated_at: now - 5 * 86400_000, total_hits: 2, recent_hits: 2, last_hit_at: now - 2 * 86400_000, sections: [] },
  ]
}

/** 启停(乐观更新,失败翻回)。停用即从模型的索引消失,下一句话生效。 */
async function toggle(s: SkillItem) {
  s.enabled = !s.enabled
  if (!isTauri()) return
  try {
    await api.setSkillEnabled(s.id, s.enabled)
  } catch (e) {
    console.error('切换技能失败', e)
    s.enabled = !s.enabled
    toast.error(t('toast.actionFailed'))
  }
}

/** 真删(仅用户教的;乐观更新,失败补回)。 */
async function doRemove(s: SkillItem) {
  const idx = skills.value.findIndex((x) => x.id === s.id)
  if (idx >= 0) skills.value.splice(idx, 1)
  if (!isTauri()) return
  try {
    await api.deleteSkill(s.id)
  } catch (e) {
    console.error('删除技能失败', e)
    if (idx >= 0) skills.value.splice(idx, 0, s)
    toast.error(t('toast.deleteFailed'))
  }
}

async function remove(s: SkillItem) {
  if (arming.value !== s.id) {
    arming.value = s.id
    return
  }
  arming.value = null
  await doRemove(s)
}

function openSkillMenu(e: MouseEvent, s: SkillItem) {
  const items = [
    { label: t('ctx.copy'), action: () => copyText(`${s.name}\n${s.when_to_use}\n\n${s.content}`) },
    { separator: true as const },
    {
      label: s.enabled ? t('skills.turnOff') : t('skills.turnOn'),
      action: () => void toggle(s),
    },
  ]
  if (s.source === 'user') {
    items.push({ label: t('ctx.delete'), danger: true, action: () => void doRemove(s) } as never)
  }
  openMenu(e, items)
}

function fmtAgo(ts: number): string {
  const sec = Math.max(0, (Date.now() - ts) / 1000)
  if (sec < 60) return t('skills.agoJustNow')
  if (sec < 3600) return t('skills.agoMin', { n: Math.floor(sec / 60) })
  if (sec < 86400) return t('skills.agoHour', { n: Math.floor(sec / 3600) })
  return t('skills.agoDay', { n: Math.floor(sec / 86400) })
}

/** 统计一行:「触发 42 次 · 近7天 6 · 最近 1 小时前」;从没触发过 = 「还没用过」。 */
function statLine(s: SkillItem): string {
  if (!s.total_hits || !s.last_hit_at) return t('skills.hitsNone')
  return t('skills.hits', { total: s.total_hits, recent: s.recent_hits, ago: fmtAgo(s.last_hit_at) })
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
  <section class="skills view-shell" @click.self="arming = null">
    <header class="view-head sep" data-tauri-drag-region>
      <div class="view-title">
        <b>{{ t('skills.title') }}</b>
        <span class="view-mono">{{ petName }} · SKILLS</span>
        <small>{{ t('skills.tagline') }}</small>
      </div>
      <button class="view-back" @click="emit('close')">{{ t('skills.back') }}</button>
    </header>

    <div class="view-scroll">
      <p v-if="loaded && skills.length" class="lp-count">{{ t('skills.count', { n: skills.length }) }}</p>

      <TransitionGroup name="lp" tag="div">
        <div
          v-for="s in skills"
          :key="s.id"
          class="lp-card top sk-card"
          :class="{ off: !s.enabled }"
          @contextmenu="openSkillMenu($event, s)"
          @click="expanded = expanded === s.id ? null : s.id"
        >
          <span class="lp-dot sm" :class="{ warn: !s.enabled }"></span>
          <span class="lp-text multiline">
            <span class="sk-head">
              <b class="sk-name">{{ s.name }}</b>
              <span class="lp-chip sk-src">{{ s.source === 'builtin' ? t('skills.builtin') : t('skills.taught') }}</span>
            </span>
            <span class="sk-when">{{ s.when_to_use }}</span>
            <span class="sk-stats">{{ statLine(s) }}</span>
            <span v-if="expanded === s.id" class="sk-body" @click.stop>
              {{ s.content }}
              <span v-if="s.sections.length" class="sk-sections">{{ t('skills.sections', { list: s.sections.join('、') }) }}</span>
            </span>
          </span>
          <span class="sk-acts" @click.stop>
            <button class="sk-toggle" :class="{ on: s.enabled }" @click="toggle(s)">
              {{ s.enabled ? t('skills.on') : t('skills.off') }}
            </button>
            <button
              v-if="s.source === 'user'"
              class="lp-act hoveronly"
              :class="{ armed: arming === s.id }"
              @click="remove(s)"
            >
              {{ arming === s.id ? t('skills.confirm') : '✕' }}
            </button>
          </span>
        </div>
      </TransitionGroup>

      <div v-if="loaded && error" class="lp-error">
        <p>{{ t('common.loadError') }}</p>
        <button class="lp-retry" @click="load">{{ t('common.retry') }}</button>
      </div>
      <div v-else-if="loaded && !skills.length" class="lp-empty">
        <span class="lp-empty-icon"><svg viewBox="0 0 24 24"><path d="M12 3l2.2 4.9L19 9l-4 3.4 1.2 5.1L12 14.8 7.8 17.5 9 12.4 5 9l4.8-1.1z" /></svg></span>
        <p>{{ t('skills.empty') }}</p>
      </div>
    </div>
  </section>
</template>

<style scoped>
/* 只放技能页私有的小样式;列表骨架全用全局 view-* 与 lp-* 类(§6.7 列表页共用类) */
.sk-card { cursor: pointer; }
.sk-card.off { opacity: 0.55; }
.sk-head { display: inline-flex; align-items: center; gap: 8px; }
.sk-name { color: var(--text); }
.sk-src { flex: none; }
.sk-when { display: block; color: var(--text-dim); margin-top: 2px; }
.sk-stats { display: block; color: var(--text-faint); font-size: 12px; margin-top: 4px; }
.sk-body {
  display: block;
  margin-top: 8px;
  padding: 8px 10px;
  white-space: pre-wrap;
  color: var(--text-dim);
  font-size: 13px;
  line-height: 1.6;
  background: rgba(var(--surface-rgb), 0.5);
  border-left: 2px solid rgba(var(--accent-rgb), 0.4);
  cursor: auto;
  user-select: text;
}
.sk-sections { display: block; margin-top: 6px; color: var(--text-faint); font-size: 12px; }
.sk-acts { display: inline-flex; align-items: center; gap: 8px; flex: none; }
.sk-toggle {
  border: 1px solid rgba(var(--line-rgb), 0.6);
  background: transparent;
  color: var(--text-faint);
  font-size: 12px;
  padding: 3px 10px;
  border-radius: 999px;
  cursor: pointer;
}
.sk-toggle.on {
  color: var(--ok);
  border-color: rgba(var(--ok-rgb), 0.5);
  background: rgba(var(--ok-rgb), 0.08);
}
.sk-toggle:hover { border-color: rgba(var(--accent-rgb), 0.6); }
</style>
