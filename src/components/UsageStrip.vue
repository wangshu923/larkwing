<script setup lang="ts">
import { computed } from 'vue'
import { useI18n } from 'vue-i18n'
import { useChat } from '../composables/useChat'
import { fmtTokens, fmtUsd } from '../lib/fmt'

// 输入框下的记账灯带:↑↓token / 缓存命中 / 今日费用 / 余额。
// 灯带只管"量";时间(在飞跳秒 + 完成档案)归气泡那行 turn-meta 管,这里不重复。
// 数据全来自 VM(TurnEvent::Usage + usage_today/llm_balance);没数据的段不点灯。
const { t } = useI18n()
const { state: chat } = useChat()

const live = computed(() => chat.mood !== 'idle') // 流中:灯带光线流动

// 左段 = 当前话题累计(库聚合,重启不丢、切话题跟着切);单条回复的读数在气泡 hover 档案
const conv = computed(() => {
  const c = chat.usage.conv
  return c && c.input_tokens + c.output_tokens > 0 ? c : null // 没花过 = 熄灯
})
const today = computed(() => chat.usage.today)

/** 话题缓存命中率:命中 ⊆ 输入,前缀缓存吃得越满越省。 */
const hitPct = computed(() => {
  const c = conv.value
  if (!c || c.input_tokens <= 0) return null
  return Math.round((c.cache_hit_tokens / c.input_tokens) * 100)
})

const todayText = computed(() => {
  const d = today.value
  if (!d) return null
  // 今日有估不出价的轮次:钱不是全貌,退回只报 token(不装懂)
  if (d.unpriced && d.cost_usd === 0) return fmtTokens(d.input_tokens + d.output_tokens)
  const cost = '≈' + fmtUsd(d.cost_usd)
  return d.unpriced ? cost + '+' : cost
})

const balanceText = computed(() => {
  const b = chat.usage.balance
  if (!b) return null
  const sign = b.currency === 'CNY' ? '¥' : b.currency === 'USD' ? '$' : b.currency + ' '
  return sign + b.amount
})
</script>

<template>
  <div class="strip" :class="{ live }" :title="t('strip.tooltip')">
    <span class="lamp"></span>
    <!-- 当前话题的累计;没花过的话题整段熄灯,不摆破折号装故障 -->
    <span class="seg" v-if="conv">
      {{ t('strip.conv') }}
      <i class="arr">↑</i><b>{{ fmtTokens(conv.input_tokens) }}</b>
      <i class="arr">↓</i><b>{{ fmtTokens(conv.output_tokens) }}</b>
    </span>
    <span class="seg" v-if="hitPct !== null">{{ t('strip.cache') }} <b>{{ hitPct }}%</b></span>
    <span class="gap"></span>
    <span class="seg" v-if="todayText">{{ t('strip.today') }} <b>{{ todayText }}</b></span>
    <span class="seg bal" v-if="balanceText">{{ t('strip.balance') }} <b>{{ balanceText }}</b></span>
  </div>
</template>

<style scoped>
/* 窄窄一条:上沿 1px 光带 + 一行等宽小字读数;颜色全部继承 .layout 的主题变量 */
.strip {
  position: relative;
  display: flex;
  align-items: center;
  gap: 12px;
  padding-top: 7px;
  font: 10.5px/1 ui-monospace, 'SF Mono', monospace;
  letter-spacing: 0.8px;
  color: var(--txt2);
  user-select: none;
}
.strip::before {
  content: '';
  position: absolute;
  top: 0;
  left: 0;
  right: 0;
  height: 1px;
  background: linear-gradient(
    90deg,
    transparent,
    rgba(95, 200, 255, 0.32) 18%,
    rgba(95, 200, 255, 0.55) 50%,
    rgba(95, 200, 255, 0.32) 82%,
    transparent
  );
  background-size: 200% 100%;
  opacity: 0.55;
}
/* 流中:光带扫光 + 灯珠提速,一眼可读"正在花" */
.strip.live::before {
  opacity: 1;
  animation: stripFlow 1.6s linear infinite;
}
@keyframes stripFlow {
  from { background-position: 200% 0; }
  to { background-position: -200% 0; }
}

.lamp {
  width: 5px;
  height: 5px;
  border-radius: 50%;
  background: var(--cy);
  box-shadow: 0 0 7px var(--cy);
  opacity: 0.7;
  animation: lampIdle 3.2s ease-in-out infinite;
  flex-shrink: 0;
}
.strip.live .lamp { animation: lampIdle 0.7s ease-in-out infinite; }
@keyframes lampIdle {
  0%, 100% { opacity: 0.75; }
  50% { opacity: 0.25; }
}

.seg { display: inline-flex; align-items: center; gap: 4px; white-space: nowrap; }
.seg b {
  font-weight: 500;
  color: var(--cy);
  text-shadow: 0 0 8px rgba(95, 200, 255, 0.35);
}
.arr { font-style: normal; opacity: 0.75; }
.arr + b { margin-right: 6px; }
.gap { flex: 1; }
.bal b { color: #5fe0b0; text-shadow: 0 0 8px rgba(95, 224, 176, 0.35); }
</style>
