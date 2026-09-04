// 桌宠行为层(B1,2026-08-31):在 A 层「戏份」(道具叠加)之上给它**生物感**——
// 空闲久了睡觉、任务完工蹦一下/失败蔫掉、手机来消息送信、你打字它凑过来围观、
// 想事情来回踱步。信号全是现成的(activity / tasks 边沿 / app_event / 输入框打字),
// 纯前端零后端;全部计时走 setTimeout(不开 interval 轮询,藏托盘也不空烧)。
//
// 行为(behavior)与戏份(activity)是两个维度:戏份管「手上拿什么/头上飘什么」,
// 行为管「身体在干嘛」(去哪/停驻姿态/一次性表演)。运动执行在 PetRoamer,这里只做
// 「信号 → 该演什么」的决策(usePetActivity 同款分工:解析单源,别在组件里再长一张表)。
import { computed, onBeforeUnmount, ref, watch, type Ref } from 'vue'
import { onAppEvent } from '../lib/backend'
import { useTasks } from './useTasks'
import type { PetActivity } from './usePetActivity'

export type PetBehavior =
  | 'sleep' // 空闲太久:走到角落趴下打盹(Zzz + 呼吸),有动静就醒
  | 'pace' // 思考:原地来回踱步(替「站着发呆」)
  | 'watch' // 用户在打字:凑到输入框上方蹲着围观
  | 'deliver' // 手机渠道有动静:叼信封往会话列表方向跑一趟
  | 'celebrate' // 后台任务完工:蹦两下 + 星星(一次性)
  | 'dazed' // 后台任务失败:头顶问号 + 蔫一下(一次性)
  | null // 纯遛弯

/** 安静多久算「该睡了」(无任务/无播放/无思考/没人打字/没人戳)。 */
const SLEEP_AFTER_MS = 3 * 60_000
/** 一次性表演时长(庆祝/蔫)与送信跑动窗口。 */
const ONESHOT_MS = 1_800
const DELIVER_MS = 4_500

export function usePetBehavior(
  activity: Ref<PetActivity | null>,
  typing: Ref<boolean>,
): { behavior: Ref<PetBehavior>; markInteraction: () => void } {
  // —— 睡意:安静 SLEEP_AFTER_MS 后自动成立;任何信号变化/交互重置(醒来)。
  const sleepy = ref(false)
  let sleepTimer = 0
  function armSleep() {
    sleepy.value = false
    clearTimeout(sleepTimer)
    sleepTimer = window.setTimeout(() => (sleepy.value = true), SLEEP_AFTER_MS)
  }
  armSleep()
  watch([activity, typing], armSleep)

  // —— 一次性表演:tasks 的 running→done/failed 边沿(按 task_id 记上一拍状态,
  //    只对「见过在跑」的任务发,防挂载首拍把历史终态全当新闻)。
  const celebrating = ref(false)
  const dazedNow = ref(false)
  let onceTimer = 0
  function fireOnce(kind: 'celebrate' | 'dazed') {
    celebrating.value = kind === 'celebrate'
    dazedNow.value = kind === 'dazed'
    clearTimeout(onceTimer)
    onceTimer = window.setTimeout(() => {
      celebrating.value = false
      dazedNow.value = false
    }, ONESHOT_MS)
  }
  const tasks = useTasks()
  const lastStates = new Map<number, string>()
  watch(
    () => tasks.state.tasks.map((t) => `${t.task_id}:${t.state}`).join('|'),
    () => {
      for (const t of tasks.state.tasks) {
        const prev = lastStates.get(t.task_id)
        if (prev === 'running' && t.state === 'done') fireOnce('celebrate')
        else if (prev === 'running' && t.state === 'failed') fireOnce('dazed')
        lastStates.set(t.task_id, t.state)
      }
    },
  )

  // —— 送信:会话有动静(提醒到点/后台汇报/渠道回合收尾)= 有话捎来 → 跑一趟。
  //    旁听仲裁(overheard / overheard_dismissed)是零痕迹的内部事,不算「来消息」——高频唤醒名
  //    被电视里的词误触时,它不该叼着信封满屏跑(复审实锤)。
  const DELIVER_KINDS = new Set(['channel', 'reminder', 'report'])
  const delivering = ref(false)
  let deliverTimer = 0
  onAppEvent((ev) => {
    if (ev.type !== 'conversation' || !DELIVER_KINDS.has(ev.data.kind)) return
    delivering.value = true
    clearTimeout(deliverTimer)
    deliverTimer = window.setTimeout(() => (delivering.value = false), DELIVER_MS)
  })

  onBeforeUnmount(() => {
    clearTimeout(sleepTimer)
    clearTimeout(onceTimer)
    clearTimeout(deliverTimer)
  })

  const behavior = computed<PetBehavior>(() => {
    if (celebrating.value) return 'celebrate'
    if (dazedNow.value) return 'dazed'
    if (delivering.value) return 'deliver'
    if (typing.value) return 'watch'
    if (activity.value === 'think') return 'pace'
    if (sleepy.value && activity.value === null) return 'sleep'
    return null
  })

  return { behavior, markInteraction: armSleep }
}
