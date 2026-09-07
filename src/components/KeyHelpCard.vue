<script setup lang="ts">
// 快捷键速查卡:视频浮层(H / ?)与音频播放条(H)共用,行由 useMediaKeys 的同一张表生成。
// 两种观感:overlay = 盖在画面上(覆盖媒体豁免:恒黑底浅字,不随皮肤);drop = 播放条上方的
// 下拉卡(语义 token,随皮肤)。
import { useI18n } from 'vue-i18n'

defineProps<{
  rows: { keys: string[]; label: string }[]
  variant: 'overlay' | 'drop'
}>()
const { t } = useI18n()
</script>

<template>
  <div class="help-card" :class="variant" @click.stop @pointerdown.stop>
    <h3>{{ t('media.keys.title') }}</h3>
    <ul>
      <li v-for="row in rows" :key="row.label">
        <span class="kbs"><kbd v-for="k in row.keys" :key="k">{{ k }}</kbd></span>
        <span class="lbl">{{ row.label }}</span>
      </li>
    </ul>
  </div>
</template>

<style scoped>
.help-card {
  min-width: 260px; max-width: min(92%, 420px); max-height: 92%; overflow: auto;
  padding: 14px 18px 16px; border-radius: 14px;
  border: 1px solid rgba(var(--accent-rgb), 0.3);
  box-shadow: 0 18px 60px rgba(0, 0, 0, 0.55);
  scrollbar-gutter: stable;
}
.help-card.overlay { background: rgba(0, 0, 0, 0.82); color: #eaf2fb; }
.help-card.drop {
  background: var(--surface); color: var(--text);
  backdrop-filter: blur(14px); -webkit-backdrop-filter: blur(14px);
}
.help-card h3 { margin: 0 0 10px; font-size: 13px; letter-spacing: 0.6px; color: var(--accent); }
.help-card ul { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: 6px; }
.help-card li { display: flex; align-items: center; gap: 12px; font-size: 12.5px; }
.help-card .kbs { flex: none; min-width: 118px; display: flex; gap: 4px; flex-wrap: wrap; }
.help-card kbd {
  padding: 2px 7px; border-radius: 6px; font: 11px/1.5 ui-monospace, "SF Mono", monospace;
}
.help-card.overlay kbd { background: rgba(255, 255, 255, 0.1); border: 1px solid rgba(255, 255, 255, 0.22); color: #fff; }
.help-card.drop kbd { background: rgba(var(--accent-rgb), 0.1); border: 1px solid rgba(var(--accent-rgb), 0.3); color: var(--text); }
.help-card.overlay .lbl { color: rgba(234, 242, 251, 0.85); }
.help-card.drop .lbl { color: var(--text-dim); }
</style>
