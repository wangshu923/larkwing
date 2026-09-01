// 桌宠/悬浮窗头像的「戏份」解析(2026-08-13,桌宠戏份 A 层):从三路**现成**信号
// (后台任务 kind / mood / 播放态)推导此刻该演什么。纯映射、零新状态;
// 主窗 PetRoamer(全套道具+摇摆)与悬浮窗 orb(迷你角标)共用这一份,别各写一张表。
// B1 行为层(2026-08-31)加了 PropGlyph:道具组件按「图形」渲染,戏份→图形的映射在此单源
// (行为层的 zzz/信封等图形不对应任何 activity,分开两个词汇才不搅)。
import { onBeforeUnmount, ref, type Ref } from 'vue'

export type PetActivity = 'carry' | 'inspect' | 'think' | 'groove'

/** 道具图形词汇(PetProp 的 prop):戏份四样 + 行为层的浮标(睡觉 Zzz / 惊叹 / 问号 / 信封)。 */
export type PropGlyph = 'box' | 'lens' | 'dots' | 'note' | 'zzz' | 'bang' | 'question' | 'mail'

/** 戏份 → 道具图形(单源映射;PetRoamer 与悬浮窗 orb 角标共用)。 */
export function glyphOf(a: PetActivity): PropGlyph {
  switch (a) {
    case 'carry':
      return 'box'
    case 'inspect':
      return 'lens'
    case 'groove':
      return 'note'
    default:
      return 'dots'
  }
}

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

/** ?demo 总轮播(B1;主窗桌宠与悬浮窗 orb 共用):戏份与行为一张表按序演(先 A 层四样,
 *  再行为层五样,末尾空档纯遛弯)。行为词汇见 usePetBehavior 的 PetBehavior;这里用字符串
 *  免循环依赖。非 demo 恒 null(调用方 `demo ?? 真值` 兜底)。 */
export function usePetDemoShow(): { act: Ref<PetActivity | null>; behavior: Ref<string | null> } {
  const act = ref<PetActivity | null>(null)
  const behavior = ref<string | null>(null)
  if (new URLSearchParams(location.search).has('demo')) {
    const cycle: { a?: PetActivity; b?: string }[] = [
      { a: 'carry' },
      { a: 'inspect' },
      { a: 'think' }, // think 自带踱步(pace 由 activity 推导)
      { a: 'groove' },
      { b: 'sleep' },
      { b: 'celebrate' },
      { b: 'dazed' },
      { b: 'deliver' },
      { b: 'watch' },
      { b: 'watch' }, // watch 连两格:要走到输入框上方才蹲下,一格不够看到歪头
      {}, // 纯遛弯
    ]
    let i = 0
    const apply = () => {
      const c = cycle[i % cycle.length]
      act.value = c.a ?? null
      behavior.value = c.b ?? null
    }
    apply()
    const timer = setInterval(() => {
      i++
      apply()
    }, 3200)
    onBeforeUnmount(() => clearInterval(timer))
  }
  return { act, behavior }
}
