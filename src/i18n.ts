import { createI18n } from 'vue-i18n'

import en from './locales/en'
import zhCN from './locales/zh-CN'

// 多语言(PLAN §6):加语言 = locales/ 加文件 + 这里注册一行;界面文案随 ui.locale 切,
// 对话语言由模型跟随用户(人格语言中立,见场景数据)。fallback 走 zh-CN(词条产地)。
export const i18n = createI18n({
  legacy: false,
  locale: 'zh-CN',
  fallbackLocale: 'zh-CN',
  messages: { 'zh-CN': zhCN, en },
})

/** boot 带回的用户级 locale;字典里还没有的语言先留在 zh-CN。 */
export function applyLocale(locale: string) {
  const known = i18n.global.availableLocales as string[]
  if (known.includes(locale)) {
    i18n.global.locale.value = locale as typeof i18n.global.locale.value
  }
}
