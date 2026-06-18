import { createApp } from 'vue'
import './style.css'
import App from './App.vue'
import { i18n } from './i18n'
import { isTauri } from './lib/backend'

// 收口 WebView 外壳(§3「不暴露/强默认」):消费产品不该露出浏览器自带的右键菜单与刷新。
// 分两档(用户 2026-06-18 拍板):
//  · 右键菜单:只要跑在 Tauri app 内(含 tauri dev)就干掉——纯浏览器预览(web 调试)不动。
//    代价:app 内右键 Inspect Element 没了,Mac 调试改用 Safari「开发」菜单、Win 用 F12。
//  · 刷新/另存为/打印/前进后退 等浏览器快捷键:只在正式版拦——dev 保留 Ctrl+R 方便强制刷新。
//    刷新会重跑开机动画、丢掉所有瞬态(进行中任务、滚动位、未发消息),正式版必须挡。
// ⚠️ WebView2 把 F5/Ctrl+R 当「加速键」处理,JS preventDefault 不一定拦得住 → 若 Windows 正式版真机仍能刷新,
//    下楼用 WebView2 设置 AreBrowserAcceleratorKeysEnabled=false 兜底(§8.1 真机 watch-item)。
if (isTauri()) {
  // 1) 右键菜单选择性放行(2026-06-18 加自绘右键菜单后改):
  //    · 输入框 / textarea / 可编辑区 → 放行走原生(恢复剪切/复制/粘贴,桌面肌肉记忆)
  //    · 自绘菜单的目标已在 openMenu 里 stopPropagation,事件根本到不了这里,无需特判
  //    · 其余空白 / 装饰区 → 仍干掉浏览器默认菜单(保持非网页感;文本选中 + Ctrl+C 照常)
  window.addEventListener('contextmenu', (e) => {
    const el = e.target as HTMLElement | null
    if (el?.closest('input, textarea, [contenteditable="true"]')) return // 原生菜单
    e.preventDefault()
  })
  // 2) 正式版再拦掉会破坏单页外壳的浏览器快捷键
  if (import.meta.env.PROD) {
    window.addEventListener('keydown', (e) => {
      const k = e.key
      const mod = e.ctrlKey || e.metaKey // Win=Ctrl / mac=⌘
      // 刷新:Ctrl/⌘+R(含 Shift 硬刷)、F5
      if ((mod && (k === 'r' || k === 'R')) || k === 'F5') return e.preventDefault()
      // 浏览器 chrome:另存为 Ctrl/⌘+S、打印 Ctrl/⌘+P
      if (mod && (k === 's' || k === 'S' || k === 'p' || k === 'P')) return e.preventDefault()
      // 前进/后退导航:Alt+←/→(SPA 里会把整个 app 顶走)
      if (e.altKey && (k === 'ArrowLeft' || k === 'ArrowRight')) return e.preventDefault()
    })
  }
}

createApp(App).use(i18n).mount('#app')
