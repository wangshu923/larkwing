<script setup lang="ts">
// 任务 HUD:窗口右缘垂直堆叠的进度卡(标题 + 当前步骤 + 进度条)。
// 超过 4 条折叠成汇总胶囊;视频全屏时缩成右上角迷你胶囊,不挡画面。
import { computed } from 'vue'
import { useI18n } from 'vue-i18n'
import { useTasks } from '../composables/useTasks'
import { useMedia } from '../composables/useMedia'
import { api, type TaskView, type TextRef } from '../lib/backend'

const { t, te } = useI18n()
const { state, dismiss } = useTasks()
const { state: media } = useMedia()

const COLLAPSE_AT = 4

// 重试失败任务:按 retry 载体直连重放(目前仅影音),旧失败卡撤掉(重放会冒新卡)。
function retry(task: TaskView) {
  if (task.retry?.type === 'media_play') {
    void api.mediaRetry(task.retry.data.page_url, task.retry.data.audio_only)
  }
  dismiss(task.task_id)
}

const running = computed(() => state.tasks.filter(x => x.state === 'running').length)
const collapsed = computed(
  () => !state.expanded && (media.fullscreen || state.tasks.length > COLLAPSE_AT),
)

// key 不在字典(新 core 配旧前端)= 兜底文案,同 tool.unknown 的增量演化约定
function txt(ref?: TextRef, fallback = 'task.unknown'): string {
  if (!ref) return ''
  const params = (ref.params ?? {}) as Record<string, unknown>
  return te(ref.key) ? t(ref.key, params) : t(fallback)
}
</script>

<template>
  <div class="tasks" :class="{ mini: media.fullscreen }" v-if="state.tasks.length">
    <!-- 折叠胶囊:N 项进行中(点开展开) -->
    <button v-if="collapsed" class="pill" @click="state.expanded = true">
      <span class="spin" v-if="running"></span>
      {{ t('task.progress', { n: state.tasks.length }) }}
    </button>

    <TransitionGroup v-else name="card" tag="div" class="stack">
      <div v-for="task in state.tasks" :key="task.task_id" class="card" :class="task.state">
        <div class="row">
          <span class="label">{{ txt(task.label) }}</span>
          <button
            v-if="task.state === 'failed' && task.retry"
            class="retry"
            @click="retry(task)"
          >{{ t('task.retry') }}</button>
          <button
            v-if="task.state === 'failed'"
            class="x"
            @click="dismiss(task.task_id)"
            aria-label="dismiss"
          >✕</button>
          <span v-else-if="task.state === 'done'" class="ok">✓</span>
        </div>
        <div v-if="task.state === 'failed'" class="step err">{{ txt(task.error) }}</div>
        <div v-else-if="task.step" class="step">{{ txt(task.step) }}</div>
        <div class="bar" v-if="task.state === 'running'">
          <div
            v-if="task.progress != null"
            class="fill"
            :style="{ width: (task.progress * 100).toFixed(1) + '%' }"
          ></div>
          <div v-else class="fill indeterminate"></div>
        </div>
      </div>
    </TransitionGroup>
  </div>
</template>

<style scoped>
.tasks {
  /* 固定定位浮层,从 :root 继承科幻 token(原先自带一份 --t-* 副本,已删) */
  position: fixed; top: 74px; right: 14px; z-index: 40;
  font-family: -apple-system, "PingFang SC", "Segoe UI", sans-serif;
  pointer-events: none;
}
.tasks.mini { top: 12px; }
.tasks > * { pointer-events: auto; }

.stack { display: flex; flex-direction: column; gap: 8px; width: 236px; }

.card {
  padding: 9px 11px 10px; border-radius: 10px;
  background: var(--surface); border: 1px solid var(--line);
  backdrop-filter: blur(10px); -webkit-backdrop-filter: blur(10px);
  box-shadow: 0 6px 18px rgba(0, 0, 0, 0.3);
}
.card.failed { border-color: rgba(var(--attn-rgb), 0.5); }
.card.done { border-color: rgba(var(--ok-rgb), 0.5); }

.row { display: flex; align-items: center; gap: 8px; }
.label { flex: 1; font-size: 12px; color: var(--text); letter-spacing: .5px; }
.ok { color: var(--ok); font-size: 12px; text-shadow: 0 0 8px rgba(var(--ok-rgb), .6); }
.x {
  background: none; border: none; cursor: pointer; color: var(--text-dim);
  font-size: 11px; padding: 0 2px; line-height: 1;
}
.x:hover { color: var(--attn); }
.retry {
  flex: 0 0 auto; cursor: pointer; line-height: 1;
  font-size: 10.5px; letter-spacing: .4px; color: var(--accent);
  background: rgba(var(--accent-rgb), 0.1); border: 1px solid rgba(var(--accent-rgb), 0.3);
  border-radius: 6px; padding: 2px 7px;
}
.retry:hover { background: rgba(var(--accent-rgb), 0.2); border-color: var(--accent); }

.step {
  margin-top: 3px; font: 10.5px/1.4 ui-monospace, "SF Mono", monospace;
  color: var(--text-dim); letter-spacing: .4px;
  overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
}
.step.err { color: var(--attn); }

.bar {
  margin-top: 7px; height: 3px; border-radius: 2px; overflow: hidden;
  background: rgba(var(--accent-rgb), 0.12);
}
.fill {
  height: 100%; border-radius: 2px; background: var(--accent);
  box-shadow: 0 0 8px rgba(var(--accent-rgb), 0.7);
  transition: width .4s ease;
}
.fill.indeterminate { width: 36%; animation: slide 1.3s ease-in-out infinite; }
@keyframes slide {
  0% { transform: translateX(-110%); }
  100% { transform: translateX(360%); }
}

.pill {
  display: flex; align-items: center; gap: 7px;
  padding: 7px 13px; border-radius: 999px; cursor: pointer;
  font-size: 11.5px; letter-spacing: .8px; color: var(--text);
  background: var(--surface); border: 1px solid var(--line);
  backdrop-filter: blur(10px); -webkit-backdrop-filter: blur(10px);
}
.pill:hover { border-color: var(--accent); }
.spin {
  width: 9px; height: 9px; border-radius: 50%;
  border: 1.5px solid rgba(var(--accent-rgb), 0.25); border-top-color: var(--accent);
  animation: rot 0.9s linear infinite;
}
@keyframes rot { to { transform: rotate(360deg); } }

.card-enter-from { opacity: 0; transform: translateX(14px); }
.card-leave-to { opacity: 0; transform: translateX(14px); }
.card-enter-active, .card-leave-active { transition: all .28s ease; }
</style>
