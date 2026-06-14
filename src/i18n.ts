import { createI18n } from 'vue-i18n'

import zhCN from './locales/zh-CN'

// 单语言起步(PLAN §6:只做 i18n 化的姿势);加语言 = locales/ 加文件 + 这里注册一行。
export const i18n = createI18n({
  legacy: false,
  locale: 'zh-CN',
  fallbackLocale: 'zh-CN',
  messages: { 'zh-CN': zhCN },
})

/** boot 带回的用户级 locale;字典里还没有的语言先留在 zh-CN。 */
export function applyLocale(locale: string) {
  const known = i18n.global.availableLocales as string[]
  if (known.includes(locale)) {
    i18n.global.locale.value = locale as typeof i18n.global.locale.value
  }
}
