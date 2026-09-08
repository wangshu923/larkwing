<script setup lang="ts">
// 气泡富文本(markdown)一条 = 一个组件实例。**存在的唯一理由是性能**:
// 打字机每 16 帧改一次在飞那条的 text(useChat 的 tw loop),父组件 MainLayout 随之整体
// 重渲染;而 markdown 渲染 = marked 解析 + DOMPurify 消毒,是这条流水线上最重的一步。
// 从前正文写在 MainLayout 模板里(`v-html="renderMarkdown(m.text)"`),于是**每一帧**都对
// 当前会话已加载的全部 wang 气泡(上限 200 行)重跑一遍解析 —— 与真正变了的只有一条。
// 抽成子组件后:① html 是按 props.text 记的 computed,text 没变就不重算;② props 没变时
// Vue 直接跳过这个子组件的 patch,连内部 vnode 都不重建。在飞那条照旧每帧重算(它真的变了)。
//
// ⚠️ 刻意**不带 style**:`.md`(以及 `.bubble.wang .md:not(:first-child)`)那套规则仍留在
// MainLayout 的 scoped 块里 —— 子组件的根元素会同时带上父组件的 scope id,所以父那边的
// `.md[data-v-父]` 照旧命中,样式行为与内联写法逐字一致(v-html 产出的后代元素本就不带
// scope id,这一点改动前后同样)。别在这里补 style,否则两处样式会打架。
import { computed } from 'vue'
import { renderMarkdown } from '../lib/md'

const props = defineProps<{ text: string }>()
const html = computed(() => renderMarkdown(props.text))
</script>

<template>
  <div class="md" v-html="html"></div>
</template>

<!-- 富文本内部的排版规则。**故意不 scoped**:这些元素是 `v-html` 现产的,不带任何
     scope 属性 —— scoped 编译出的 `.md p[data-v-…]` 对它们一条都命中不了。
     (从前这批规则写在 MainLayout 的 scoped 块里,于是全部静静失效:行内 code 没底色、
      代码块没框、链接还是浏览器默认蓝、表格没边框。2026-09-08 抽子组件时实测发现并搬来这儿。)
     `:deep()` 也能达到目的,但这套规则本就只服务 `.md` 一处,非 scoped 更直白;
     选择器一律以 `.md` 起头,不外溢。`.md` 元素**自身**的规则仍在 MainLayout(它要看
     `.bubble.wang` 的上下文)。 -->
<style>
.md { white-space: normal; }
.md > :first-child { margin-top: 0; }
.md > :last-child { margin-bottom: 0; }
.md p { margin: 0 0 8px; }
.md ul, .md ol { margin: 6px 0; padding-left: 20px; }
.md li { margin: 2px 0; }
.md h1, .md h2, .md h3, .md h4 { margin: 10px 0 6px; font-weight: 600; line-height: 1.3; }
.md h1 { font-size: 1.3em; } .md h2 { font-size: 1.18em; } .md h3 { font-size: 1.06em; } .md h4 { font-size: 1em; }
.md code { font-family: ui-monospace, "SF Mono", monospace; font-size: .9em; background: rgba(var(--accent-rgb), 0.12); padding: 1px 5px; border-radius: 5px; }
.md pre { background: var(--surface-deep); border: 1px solid var(--line); border-radius: 9px; padding: 10px 12px; overflow-x: auto; margin: 8px 0; }
.md pre code { background: none; padding: 0; font-size: 12.5px; line-height: 1.5; }
.md blockquote { margin: 8px 0; padding: 2px 0 2px 12px; border-left: 2px solid var(--line); color: var(--text-dim); }
.md a { color: var(--accent); text-decoration: underline; text-underline-offset: 2px; cursor: pointer; }
.md strong, .md b { font-weight: 600; color: var(--text); }
.md hr { border: none; border-top: 1px solid var(--line); margin: 10px 0; }
/* 宽表格自己横滚,别把气泡撑破(§ 响应式:页面本体永不横滚) */
.md table { border-collapse: collapse; margin: 8px 0; font-size: .94em; display: block; overflow-x: auto; max-width: 100%; }
.md th, .md td { border: 1px solid var(--line); padding: 4px 8px; text-align: left; }
</style>
