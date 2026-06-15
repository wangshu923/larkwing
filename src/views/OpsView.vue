<script setup lang="ts">
// 操作记录页(PLAN §9 文件能力):看 7274 动过哪些文件,按批次列 + 一键撤销/重做。
// 定位 = 普通文件管理器的历史功能(功能性,**非安全承诺**);气质仿回忆页(MemoryView)。
// 「审批」分区预留不显示(将来若启用执行前确认,审批历史落这里)。
// 纯浏览器预览:假数据看视觉。
import { computed, onMounted, onUnmounted, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import { api, isTauri, type FsOp } from '../lib/backend'

const emit = defineEmits<{ (e: 'close'): void }>()
const { t, te } = useI18n()

const ops = ref<FsOp[]>([])
const loaded = ref(false)
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
    loaded.value = true
    return
  }
  try {
    ops.value = await api.listFsops()
  } catch (e) {
    console.error('加载操作记录失败', e)
  }
  loaded.value = true
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
  <section class="ops">
    <header class="o-head" data-tauri-drag-region>
      <div class="o-title">
        <b>{{ t('ops.title') }}</b>
        <span class="o-mono">7274 · FILES</span>
        <small>{{ t('ops.tagline') }}</small>
      </div>
      <button class="o-back" @click="emit('close')">{{ t('ops.back') }}</button>
    </header>

    <div class="o-body">
      <p v-if="loaded && total" class="o-count">{{ t('ops.count', { n: total }) }}</p>

      <TransitionGroup name="op" tag="div">
        <div v-for="o in ops" :key="o.id" class="op-card" :class="{ undone: o.state === 'undone' }">
          <span class="op-dot" :class="dotClass(o)"></span>
          <span class="op-text">{{ summary(o) }}</span>
          <span v-if="o.state === 'undone'" class="op-badge">{{ t('ops.undone') }}</span>
          <span class="op-date">{{ fmtDate(o.created_at) }}</span>
          <button
            v-if="o.state === 'applied'"
            class="op-act"
            :disabled="busy === o.id"
            @click="undo(o)"
          >
            {{ t('ops.undo') }}
          </button>
          <button v-else class="op-act redo" :disabled="busy === o.id" @click="redo(o)">
            {{ t('ops.redo') }}
          </button>
        </div>
      </TransitionGroup>

      <div v-if="loaded && !total" class="o-empty">
        <span class="o-empty-icon">🗂</span>
        <p>{{ t('ops.empty') }}</p>
      </div>
    </div>
  </section>
</template>

<style scoped>
.ops { flex: 1; display: flex; flex-direction: column; min-width: 0; padding: 18px 26px; overflow-y: auto; }
.o-head { display: flex; align-items: flex-start; justify-content: space-between; margin-bottom: 14px; padding-right: 70px; }
.o-title b { font-size: 16px; color: var(--txt); }
.o-title small { display: block; margin-top: 3px; font-size: 12px; color: var(--txt2); }
.o-mono { font-family: ui-monospace, "SF Mono", monospace; font-size: 10px; letter-spacing: 2px; color: var(--txt2); margin-left: 8px; }
.o-back { background: none; border: 1px solid var(--line); border-radius: 9px; color: var(--txt2); cursor: pointer; padding: 5px 10px; font-size: 12px; }
.o-back:hover { color: var(--cy); border-color: var(--cy); }

.o-body { max-width: 640px; }
.o-count { margin: 0 0 10px; font-size: 11.5px; letter-spacing: 2px; color: var(--txt2); }

.op-card {
  display: flex; align-items: center; gap: 10px;
  border: 1px solid var(--line); border-radius: 12px; padding: 11px 14px; margin-bottom: 8px;
  background: rgba(95, 200, 255, 0.03); font-size: 13.5px;
  transition: border-color .15s, opacity .15s;
}
.op-card:hover { border-color: rgba(95, 200, 255, 0.4); }
.op-card.undone { opacity: 0.6; }
.op-dot { width: 6px; height: 6px; border-radius: 50%; background: var(--cy); box-shadow: 0 0 6px var(--cy); flex: none; opacity: .85; }
.op-dot.warn { background: #ffb86b; box-shadow: 0 0 6px #ffb86b; }
.op-text { flex: 1; min-width: 0; color: var(--txt); line-height: 1.5; word-break: break-word; }
.op-badge {
  flex: none; font: 10px/1 ui-monospace, "SF Mono", monospace; letter-spacing: 1px;
  color: var(--txt2); border: 1px solid var(--line); border-radius: 6px; padding: 3px 7px;
}
.op-date { flex: none; font: 10.5px/1 ui-monospace, "SF Mono", monospace; letter-spacing: .5px; color: var(--txt2); }
.op-act {
  flex: none; background: none; border: 1px solid transparent; border-radius: 8px;
  color: var(--txt2); cursor: pointer; font-size: 11.5px; padding: 4px 10px;
  transition: opacity .15s, color .15s, border-color .15s;
}
.op-card:hover .op-act { color: #ffb86b; border-color: rgba(255, 184, 107, 0.45); }
.op-act.redo { }
.op-card:hover .op-act.redo { color: var(--cy); border-color: rgba(95, 200, 255, 0.45); }
.op-act:disabled { opacity: 0.4; cursor: default; }

.o-empty { padding: 42px 0; text-align: center; color: var(--txt2); font-size: 13.5px; line-height: 1.8; }
.o-empty-icon { font-size: 26px; display: block; margin-bottom: 8px; opacity: .7; }

.op-leave-to { opacity: 0; transform: translateX(12px); }
.op-leave-active { transition: all .22s ease; }
</style>
