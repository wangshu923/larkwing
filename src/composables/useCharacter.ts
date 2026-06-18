// 形象(角色)状态唯一源:聊天页头像、桌宠 PetRoamer、桌宠右键「换形象」都读这一份。
// 形象 = 设置项 ui.character(每用户持久化),与皮肤(观感)正交 —— skin 切观感、character 切谁出镜。
// 角色包规范见 scripts/make-roamer-frames.py:每角色 1 张 idle + N 张 run(192 见方、体量归一、朝右),
// 文件名顺序即步态环播放顺序;px 是体量对齐后的显示尺寸;idle 多帧 = 停驻时慢速循环。
import { computed } from 'vue'
import { useSettings } from './useSettings'
import dogIdle from '../assets/dog-idle.png'
import dogRun1 from '../assets/dog-run-1.png'
import dogRun2 from '../assets/dog-run-2.png'
import dogRun3 from '../assets/dog-run-3.png'
import dogRun4 from '../assets/dog-run-4.png'
import dogRun5 from '../assets/dog-run-5.png'
import catIdle from '../assets/cat-idle.png'
import catRun1 from '../assets/cat-run-1.png'
import catRun2 from '../assets/cat-run-2.png'
import catRun3 from '../assets/cat-run-3.png'
import catRun4 from '../assets/cat-run-4.png'
import catRun5 from '../assets/cat-run-5.png'
import titanIdle1 from '../assets/titan-idle-1.png'
import titanIdle2 from '../assets/titan-idle-2.png'
import titanRun1 from '../assets/titan-run-1.png'
import titanRun2 from '../assets/titan-run-2.png'
import titanRun3 from '../assets/titan-run-3.png'
import titanRun4 from '../assets/titan-run-4.png'

export interface CharPack {
  /** idle 帧:单帧 = 静止蹲坐;多帧 = 停驻时慢速循环(如机器人光学眼呼吸)。 */
  idle: string[]
  /** run 帧:朝右,文件名顺序即步态环;朝左时整体镜像。 */
  run: string[]
  /** 体量对齐后的显示宽度(px)。 */
  px: number
  /** 飞行型(按航段选帧,而非匀速轮播,免抽搐);当前都为 false,留给悬浮机器人。 */
  fly: boolean
  /** 换帧节拍(帧/步),越大步子越沉(fly 角色忽略)。 */
  tick: number
}

// 泰坦(默认):四帧双足循环;idle 两帧 = 光学眼/散热栅呼吸;步频沉走吨位感。
const characters: CharPack[] = [
  { idle: [titanIdle1, titanIdle2], run: [titanRun1, titanRun2, titanRun3, titanRun4], px: 63, fly: false, tick: 12 },
  { idle: [dogIdle], run: [dogRun1, dogRun2, dogRun3, dogRun4, dogRun5], px: 52, fly: false, tick: 4 },
  { idle: [catIdle], run: [catRun1, catRun2, catRun3, catRun4, catRun5], px: 66, fly: false, tick: 4 },
]
const charIds = ['titan', 'dog', 'cat'] as const
export type CharId = (typeof charIds)[number]

// 预解码:避免换帧/换角色时盒子沿用旧图宽高比闪一下
characters.forEach((c) => [...c.idle, ...c.run].forEach((u) => { const im = new Image(); im.src = u }))

export function useCharacter() {
  const settings = useSettings()
  const charIdx = computed(() => Math.max(0, charIds.indexOf(settings.get('ui.character') as CharId)))
  const pack = computed(() => characters[charIdx.value])
  /** 轮换到下一个形象(点头像 / 桌宠右键「换形象」共用)。 */
  function switchCharacter() {
    void settings.set('ui.character', charIds[(charIdx.value + 1) % charIds.length])
  }
  return { characters, charIds, charIdx, pack, switchCharacter }
}
