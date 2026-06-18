// 通用右键菜单 VM(模块单例):任意组件调 openMenu(事件, 菜单项数组),
// 由 App.vue 顶层唯一一个 <ContextMenu/> 宿主渲染。菜单项数据化 —— label 由调用方
// 用 t() 译好直接传(宿主不碰 i18n key);danger 标红、disabled 置灰、separator 分隔。
// 桌面 app 右键的统一接缝:加新菜单 = 在某处调 openMenu,组件零改动。
import { reactive } from 'vue'

export interface MenuItem {
  /** 已译好的显示文案(调用方传 t('...'));separator 项可省。 */
  label?: string
  /** 危险动作(删除)标红。 */
  danger?: boolean
  /** 置灰不可点。 */
  disabled?: boolean
  /** 分隔线(其余字段忽略)。 */
  separator?: boolean
  /** 点击执行;宿主会先关菜单再调。 */
  action?: () => void
}

const state = reactive({
  open: false,
  x: 0,
  y: 0,
  items: [] as MenuItem[],
})

/** 在光标处弹出菜单。prevent + stop:压掉原生菜单,并阻止冒泡到 main.ts 的全局抑制。 */
function openMenu(e: MouseEvent, items: MenuItem[]) {
  e.preventDefault()
  e.stopPropagation()
  state.items = items
  state.x = e.clientX
  state.y = e.clientY
  state.open = true
}

function closeMenu() {
  state.open = false
  state.items = []
}

export function useContextMenu() {
  return { state, openMenu, closeMenu }
}
