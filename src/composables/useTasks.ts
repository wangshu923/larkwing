// 任务 HUD 的 VM:订阅全局事件车道,按 task_id upsert(事件是全量快照,错过即追平)。
// 完成的淡出移除;失败的留着等用户点掉;带 retry 载体的失败(影音解析/组件下载)显「重试」钮,
// 点击直连重放(TasksOverlay.retry → api.mediaRetry,不绕 LLM)。
// 浏览器预览:?demo=tasks 注入假任务,纯看视觉(UI 优先工作流)。

import { reactive } from 'vue'
import { isTauri, onAppEvent, type TaskView, type TextRef } from '../lib/backend'

const state = reactive({
  tasks: [] as TaskView[],
  /** >N 条折叠成汇总胶囊;用户点开后展开(下次再超额重新折叠)。 */
  expanded: false,
})

const DONE_LINGER_MS = 1600
let wired = false
// 前端自建任务用负 id,与 core 的正 task_id 永不撞(同 useChat localId 套路)。
let localSeq = 0

/** 前端自驱任务的句柄(progress/done/fail);完成时机即调用方代码路径(无需通用回调总线)。 */
export interface LocalTaskHandle {
  progress(fraction?: number, step?: TextRef): void
  done(): void
  fail(error?: TextRef): void
}

function upsert(view: TaskView) {
  const i = state.tasks.findIndex((t) => t.task_id === view.task_id)
  if (i >= 0) state.tasks[i] = view
  else state.tasks.push(view)
  if (view.state === 'done') {
    setTimeout(() => dismiss(view.task_id), DONE_LINGER_MS)
  }
}

function dismiss(taskId: number) {
  const i = state.tasks.findIndex((t) => t.task_id === taskId)
  if (i >= 0) state.tasks.splice(i, 1)
  if (state.tasks.length <= 1) state.expanded = false
}

/** 前端自驱一条任务(core 的 task 是后端推、这条由前端跑的活儿驱动,如更新下载)。
 *  和 core 任务同一份 state.tasks + 同一渲染(TaskView 形状一致),HUD 零改。done 后照常淡出。 */
function startLocal(init: { kind: string; label: TextRef }): LocalTaskHandle {
  let view: TaskView = { task_id: --localSeq, kind: init.kind, label: init.label, state: 'running', progress: 0 }
  upsert(view)
  const sync = (patch: Partial<TaskView>) => {
    view = { ...view, ...patch }
    upsert(view)
  }
  return {
    progress: (fraction, step) => sync({ progress: fraction, step }),
    done: () => sync({ state: 'done', progress: 1, step: undefined }),
    fail: (error) => sync({ state: 'failed', error, step: undefined }),
  }
}

function wire() {
  if (wired) return
  wired = true
  if (isTauri()) {
    onAppEvent((ev) => {
      if (ev.type === 'task') upsert(ev.data)
    })
    return
  }
  // 浏览器预览的假任务(看视觉/调样式)
  if (new URLSearchParams(location.search).get('demo')?.includes('tasks')) {
    upsert({
      task_id: 1,
      kind: 'download',
      label: { key: 'task.download.ytdlp' },
      state: 'running',
      progress: 0.34,
      step: { key: 'step.download', params: { done: 5.8, total: 17.1 } },
    })
    upsert({
      task_id: 2,
      kind: 'download',
      label: { key: 'task.download.ffmpeg' },
      state: 'running',
      step: { key: 'step.connect', params: { host: 'ghproxy.net' } },
    })
    upsert({
      task_id: 3,
      kind: 'resolve',
      label: { key: 'task.resolve' },
      state: 'failed',
      error: { key: 'task.err.resolve' },
      retry: { type: 'media_play', data: { page_url: 'https://www.bilibili.com/video/BV1xx', audio_only: false } },
    })
    let p = 0.34
    const timer = setInterval(() => {
      p = Math.min(1, p + 0.02)
      upsert({
        task_id: 1,
        kind: 'download',
        label: { key: 'task.download.ytdlp' },
        state: p >= 1 ? 'done' : 'running',
        progress: p,
        step: p >= 1 ? undefined : { key: 'step.download', params: { done: (17.1 * p).toFixed(1), total: 17.1 } },
      })
      if (p >= 1) clearInterval(timer)
    }, 600)
  }
}

export function useTasks() {
  wire()
  return { state, dismiss, startLocal }
}
