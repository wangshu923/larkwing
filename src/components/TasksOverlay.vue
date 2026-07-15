<script setup lang="ts">
// 任务 HUD:窗口右缘垂直堆叠的进度卡(标题 + 当前步骤 + 进度条)。
// 超过 4 条折叠成汇总胶囊;视频全屏时缩成右上角迷你胶囊,不挡画面。
// 确认卡(§7.8 确认闸)与任务卡同族同区:置顶、不随折叠收起(它在等人点头,必须可见)。
import { computed } from 'vue'
import { useI18n } from 'vue-i18n'
import { useTasks } from '../composables/useTasks'
import { useMedia } from '../composables/useMedia'
import { confirmActionPhrase, useConfirm } from '../composables/useConfirm'
import { api, type ConfirmCard, type TaskView, type TextRef } from '../lib/backend'

const { t, te } = useI18n()
const { state, dismiss } = useTasks()
const { state: media } = useMedia()
const confirm = useConfirm()

// 确认卡终态一行(短暂停留后淡出)
function confirmOutcome(c: ConfirmCard): string {
  if (c.state === 'allowed') return t('confirm.done.allowed')
  if (c.state === 'denied') return t('confirm.done.denied')
  return t('confirm.done.expired')
}

const COLLAPSE_AT = 4

// 重试失败任务:按 retry 载体(tagged)直连重放,旧失败卡撤掉(重放会冒新卡)。
// 影音=重放播放,下载=重下组件,语音模型=重下模型;未来别的可重试 job 在此加一支。
function retry(task: TaskView) {
  const r = task.retry
  if (r?.type === 'media_play') {
    void api.mediaRetry(r.data.page_url, r.data.audio_only)
  } else if (r?.type === 'download') {
    void api.retryDownload(r.data.component)
  } else if (r?.type === 'voice_model') {
    void api.retryVoiceModel(r.data.id)
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
  <div class="tasks" :class="{ mini: media.fullscreen }" v-if="state.tasks.length || confirm.state.cards.length">
    <!-- 确认卡(§7.8):等人点头的动作,置顶、永不折叠;终态短暂停留后自动淡出 -->
    <TransitionGroup name="card" tag="div" class="stack" v-if="confirm.state.cards.length">
      <div v-for="c in confirm.state.cards" :key="'cfm-' + c.id" class="card confirm" :class="c.state">
        <div class="row">
          <span class="label c-title">{{ t('confirm.title') }}</span>
          <span v-if="c.state === 'pending'" class="count">{{ confirm.remaining(c) }}s</span>
          <span v-else class="c-final" :class="c.state">{{ confirmOutcome(c) }}</span>
        </div>
        <div class="c-action">{{ confirmActionPhrase(c) }}</div>
        <div v-if="c.host" class="step">{{ t('confirm.atHost', { host: c.host }) }}</div>
        <div v-if="c.state === 'pending'" class="c-btns">
          <button class="c-go" @click="confirm.resolve(c.id, true, confirm.via)">{{ t('confirm.allow') }}</button>
          <button class="c-no" @click="confirm.resolve(c.id, false, confirm.via)">{{ t('confirm.deny') }}</button>
        </div>
      </div>
    </TransitionGroup>

    <!-- 折叠胶囊:N 项进行中(点开展开) -->
    <button v-if="collapsed && state.tasks.length" class="pill" @click="state.expanded = true">
      <span class="spin" v-if="running"></span>
      {{ t('task.progress', { n: state.tasks.length }) }}
    </button>

    <TransitionGroup v-if="!collapsed" name="card" tag="div" class="stack">
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

/* —— 确认卡(§7.8):同族观感,琥珀描边示意「等你一下」 —— */
.stack + .stack, .stack + .pill { margin-top: 8px; }
.card.confirm { border-color: rgba(var(--warn-rgb), 0.55); }
.card.confirm.allowed { border-color: rgba(var(--ok-rgb), 0.5); }
.card.confirm.denied, .card.confirm.expired { border-color: var(--line); }
.c-title { color: var(--warn); }
.count {
  font: 10.5px/1 ui-monospace, "SF Mono", monospace;
  color: var(--warn); letter-spacing: .4px;
}
.c-final { font-size: 10.5px; color: var(--text-dim); }
.c-final.allowed { color: var(--ok); }
.c-action {
  margin-top: 4px; font-size: 12px; color: var(--text);
  overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
}
.c-btns { display: flex; gap: 8px; margin-top: 8px; }
.c-go, .c-no {
  flex: 1; cursor: pointer; line-height: 1; padding: 5px 0;
  font-size: 11px; letter-spacing: .6px; border-radius: 7px;
}
.c-go {
  color: var(--accent); background: rgba(var(--accent-rgb), 0.12);
  border: 1px solid rgba(var(--accent-rgb), 0.4);
}
.c-go:hover { background: rgba(var(--accent-rgb), 0.22); border-color: var(--accent); }
.c-no {
  color: var(--text-dim); background: none; border: 1px solid var(--line);
}
.c-no:hover { color: var(--text); border-color: var(--text-dim); }

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
