// 富文本渲染:LLM 回复支持 markdown(段落/换行/列表/代码/加粗),
// 修掉"逐字 span 吞换行"的老毛病(见 MainLayout 旧 .ch 渲染)。
// LLM 输出按不可信处理:marked 解析后过 DOMPurify 消毒,挡 <script>/onerror/javascript: 等。
// 单换行→<br>(聊天里人/模型就这么断行),空行→段落。
import { marked } from 'marked'
import DOMPurify from 'dompurify'

marked.use({ gfm: true, breaks: true })

// 链接在 WebView 里直接导航会把整个 app 顶走 —— 统一加 target=_blank + rel,
// 真正的"点开"由 MainLayout 的委托点击拦截兜底(preventDefault + 外部打开)。
DOMPurify.addHook('afterSanitizeAttributes', (node) => {
  if (node.tagName === 'A') {
    node.setAttribute('target', '_blank')
    node.setAttribute('rel', 'noopener noreferrer')
  }
})

// 白名单:只放排版需要的标签。图片走 Phase 2 的真附件,不从 markdown ![]() 进(防远程加载/追踪)。
const ALLOWED_TAGS = [
  'p', 'br', 'strong', 'em', 'b', 'i', 'del', 's', 'code', 'pre',
  'ul', 'ol', 'li', 'blockquote', 'h1', 'h2', 'h3', 'h4', 'h5', 'h6',
  'a', 'hr', 'table', 'thead', 'tbody', 'tr', 'th', 'td', 'span',
]

export function renderMarkdown(src: string): string {
  if (!src) return ''
  const raw = marked.parse(src) as string
  return DOMPurify.sanitize(raw, { ALLOWED_TAGS, ALLOWED_ATTR: ['href', 'title'] })
}
