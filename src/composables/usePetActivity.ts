// 桌宠/悬浮窗头像的「戏份」解析(2026-08-13,桌宠戏份 A 层):从三路**现成**信号
// (后台任务 kind / mood / 播放态)推导此刻该演什么。纯映射、零新状态;
// 主窗 PetRoamer(全套道具+摇摆)与悬浮窗 orb(迷你角标)共用这一份,别各写一张表。
import { onBeforeUnmount, ref, type Ref } from 'vue'

export type PetActivity = 'carry' | 'inspect' | 'think' | 'groove'

/** 查看类任务 → 拿放大镜;其余 running 任务一律当搬运(扛箱子)。新任务 kind 不用登记,
 *  缺省进箱子桶(干活总归在搬东西,错也错得无害)。 */
const INSPECT_KINDS = new Set(['usage', 'webrender', 'resolve', 'lyrics'])

/** 优先级:任务(具体)> 思考 > 放歌;都没有 = null 纯遛弯。 */
export function resolveActivity(
  taskKinds: string[],
  thinking: boolean,
  playing: boolean,
): PetActivity | null {
  if (taskKinds.length) {
    return taskKinds.some((k) => !INSPECT_KINDS.has(k)) ? 'carry' : 'inspect'
  }
  if (thinking) return 'think'
  if (playing) return 'groove'
  return null
}

/** ?demo 预览:浏览器里没有真状态 → 每 3.2s 轮换一种戏份(含「没戏份」)纯看观感;
 *  非 demo 恒 null(调用方 `demo ?? resolveActivity(...)` 兜真值)。 */
export function usePetDemoActivity(): Ref<PetActivity | null> {
  const act = ref<PetActivity | null>(null)
  if (new URLSearchParams(location.search).has('demo')) {
    const cycle: (PetActivity | null)[] = ['carry', 'inspect', 'think', 'groove', null]
    let i = 0
    act.value = cycle[0]
    const timer = setInterval(() => {
      act.value = cycle[++i % cycle.length]
    }, 3200)
    onBeforeUnmount(() => clearInterval(timer))
  }
  return act
}
