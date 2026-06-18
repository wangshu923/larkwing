// 复制到剪贴板:优先 async clipboard,失败(无焦点 / 旧环境 / 非安全上下文)兜底 execCommand。
// 右键「复制」、气泡复制钮、记忆卡复制共用一处。ok 回调供"复制 ✓"闪动等反馈。
export function copyText(text: string, ok?: () => void) {
  if (!text) return
  const fallback = () => {
    try {
      const ta = document.createElement('textarea')
      ta.value = text
      ta.style.cssText = 'position:fixed;top:0;left:0;opacity:0'
      document.body.appendChild(ta)
      ta.focus()
      ta.select()
      const done = document.execCommand('copy')
      document.body.removeChild(ta)
      if (done) ok?.()
    } catch (e) {
      console.error('复制失败', e)
    }
  }
  if (navigator.clipboard?.writeText) {
    navigator.clipboard.writeText(text).then(() => ok?.()).catch(fallback)
  } else {
    fallback()
  }
}
