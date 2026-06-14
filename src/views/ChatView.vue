<script setup lang="ts">
import { ref, computed, nextTick } from 'vue'

type Mood = 'idle' | 'thinking' | 'speaking'
interface Message { id: number; role: 'user' | 'wang'; text: string }

const mood = ref<Mood>('idle')
const input = ref('')
const messages = ref<Message[]>([
  { id: 0, role: 'wang', text: '嗨,我是旺财!🐾 今天过得怎么样呀?' },
  { id: 1, role: 'user', text: '今天上班好累啊' },
  { id: 2, role: 'wang', text: '今天辛苦你啦……来,跟我一起深呼吸一下。我陪着你呢,想说什么我都在听。🐾' },
  { id: 3, role: 'user', text: '老板又临时让我加班' },
  { id: 4, role: 'wang', text: '哎呀又加班…… 你已经很努力啦,别太逼自己。对了,晚饭吃了没?' },
  { id: 5, role: 'user', text: '还没呢，没胃口' },
  { id: 6, role: 'wang', text: '那也得垫一口呀~ 先去吃点热乎的,我在这儿等你回来接着唠。🍚' },
])
const suggestions = ['今天有点累…', '随便聊聊', '给我讲个笑话']

let nextId = 7
let streamTimer: ReturnType<typeof setInterval> | undefined
const listEl = ref<HTMLElement | null>(null)

const statusText = computed(() =>
  mood.value === 'thinking' ? '让我想想…'
  : mood.value === 'speaking' ? '正在说…'
  : '在这儿陪你 🐾'
)

// —— 假回应库:以后整段换成 Anthropic 流式 ——
function pickReply(text: string): string {
  if (/笑话|讲个|搞笑/.test(text))
    return '嘿嘿,听好咯~ 为什么小狗从不玩捉迷藏?因为它一藏起来,尾巴就先摇出来露馅啦!🐶'
  if (/累|烦|难过|不开心|压力|emo/.test(text))
    return '今天辛苦你啦……来,跟我一起深呼吸一下。我陪着你呢,想说什么我都在听。🐾'
  if (/你好|嗨|hi|hello|在吗/i.test(text))
    return '嗨嗨!我在我在~ 今天想跟我聊点什么呀?'
  const pool = [
    '嗯嗯,我在听~ 再多跟我说一点嘛。',
    '哇,这个有意思!然后呢然后呢?',
    '我懂我懂~ 那你现在心情还好吗?',
    '陪你唠唠这个~ 你是怎么想的呀?',
  ]
  return pool[Math.floor(Math.random() * pool.length)]
}

function scrollToBottom() {
  nextTick(() => {
    const el = listEl.value
    if (el) el.scrollTop = el.scrollHeight
  })
}

function send(text?: string) {
  const content = (text ?? input.value).trim()
  if (!content || mood.value !== 'idle') return

  messages.value.push({ id: nextId++, role: 'user', text: content })
  input.value = ''
  scrollToBottom()

  mood.value = 'thinking'
  setTimeout(() => {
    const reply = pickReply(content)
    const msgId = nextId++
    messages.value.push({ id: msgId, role: 'wang', text: '' })
    streamReply(msgId, reply)
  }, 650)
}

// 假流式:逐字吐 —— 模拟以后 SSE 的体感
function streamReply(msgId: number, full: string) {
  mood.value = 'speaking'
  let i = 0
  const msg = messages.value.find(m => m.id === msgId)
  streamTimer = setInterval(() => {
    if (!msg) return
    msg.text = full.slice(0, ++i)
    scrollToBottom()
    if (i >= full.length) {
      clearInterval(streamTimer)
      mood.value = 'idle'
    }
  }, 38)
}
</script>

<template>
  <div class="window">
    <!-- 旺财舞台 -->
    <div class="stage">
      <div class="dog-wrap" :class="mood">
        <svg class="dog" viewBox="0 0 140 140" aria-label="旺财">
          <g class="breathe">
            <!-- 耳朵 -->
            <path class="ear" d="M34 40 Q26 14 50 26 Q48 42 40 50 Z" />
            <path class="ear" d="M106 40 Q114 14 90 26 Q92 42 100 50 Z" />
            <path class="ear-in" d="M38 38 Q34 24 46 30 Z" />
            <path class="ear-in" d="M102 38 Q106 24 94 30 Z" />
            <!-- 脸 -->
            <ellipse class="face" cx="70" cy="76" rx="46" ry="42" />
            <!-- 吻部 -->
            <ellipse class="muzzle" cx="70" cy="92" rx="31" ry="25" />
            <!-- 腮红 -->
            <ellipse class="blush" cx="42" cy="86" rx="9" ry="6" />
            <ellipse class="blush" cx="98" cy="86" rx="9" ry="6" />
            <!-- 眼睛 -->
            <g class="eyes">
              <ellipse class="eye" cx="54" cy="72" rx="5.5" ry="7" />
              <ellipse class="eye" cx="86" cy="72" rx="5.5" ry="7" />
              <circle class="glint" cx="56" cy="69" r="1.8" />
              <circle class="glint" cx="88" cy="69" r="1.8" />
            </g>
            <!-- 鼻子 + 嘴 -->
            <ellipse class="nose" cx="70" cy="86" rx="5" ry="3.6" />
            <path class="mouth" d="M70 90 Q70 99 62 99 M70 90 Q70 99 78 99" />
          </g>
        </svg>
        <!-- 思考气泡点 -->
        <div class="think-dots" v-show="mood === 'thinking'">
          <span></span><span></span><span></span>
        </div>
      </div>
      <div class="name">旺财</div>
      <div class="status">{{ statusText }}</div>
    </div>

    <!-- 消息 -->
    <div class="messages" ref="listEl">
      <div
        v-for="m in messages"
        :key="m.id"
        class="bubble"
        :class="m.role"
      >{{ m.text }}<span v-if="m.role === 'wang' && m.text === ''" class="typing">…</span></div>
    </div>

    <!-- 输入区 -->
    <div class="composer">
      <div class="suggestions">
        <button
          v-for="s in suggestions"
          :key="s"
          class="chip"
          :disabled="mood !== 'idle'"
          @click="send(s)"
        >{{ s }}</button>
      </div>
      <div class="input-row">
        <input
          v-model="input"
          class="field"
          type="text"
          placeholder="跟旺财说点什么…"
          @keyup.enter="send()"
        />
        <button class="send" :disabled="mood !== 'idle' || !input.trim()" @click="send()">
          发送
        </button>
      </div>
    </div>
  </div>
</template>

<style scoped>
.window {
  width: 420px;
  height: 640px;
  background: var(--cream);
  border-radius: 28px;
  box-shadow: 0 18px 50px rgba(180, 120, 60, 0.22);
  display: flex;
  flex-direction: column;
  overflow: hidden;
}

/* —— 舞台 —— */
.stage {
  padding: 22px 0 14px;
  display: flex;
  flex-direction: column;
  align-items: center;
  background: linear-gradient(180deg, var(--cream-deep), var(--cream));
  flex-shrink: 0;
}
.dog-wrap { position: relative; }
.dog { width: 132px; height: 132px; display: block; }

.ear { fill: var(--orange-deep); }
.ear-in { fill: var(--blush); }
.face { fill: var(--orange); }
.muzzle { fill: #fff7ef; }
.blush { fill: var(--blush); opacity: 0.65; }
.eye { fill: #4a3526; transform-box: fill-box; transform-origin: center; }
.glint { fill: #fff; }
.nose { fill: #4a3526; }
.mouth { fill: none; stroke: #4a3526; stroke-width: 2.2; stroke-linecap: round; }

/* 呼吸 */
.breathe { transform-box: fill-box; transform-origin: 70px 100px; animation: breathe 3.2s ease-in-out infinite; }
@keyframes breathe { 0%,100% { transform: scale(1); } 50% { transform: scale(1.04); } }

/* 眨眼 */
.eyes { animation: blink 4.5s infinite; transform-box: fill-box; transform-origin: center; }
@keyframes blink { 0%,92%,100% { transform: scaleY(1); } 96% { transform: scaleY(0.1); } }

/* 思考:眯眼 + 顶上点点 */
.dog-wrap.thinking .eyes { animation: none; transform: scaleY(0.5); }
.think-dots {
  position: absolute; top: 2px; left: 50%; transform: translateX(20px);
  display: flex; gap: 5px;
}
.think-dots span {
  width: 7px; height: 7px; border-radius: 50%; background: var(--orange);
  animation: bounce 1s infinite;
}
.think-dots span:nth-child(2) { animation-delay: 0.15s; }
.think-dots span:nth-child(3) { animation-delay: 0.3s; }
@keyframes bounce { 0%,100% { transform: translateY(0); opacity: 0.5; } 50% { transform: translateY(-6px); opacity: 1; } }

.name { margin-top: 6px; font-weight: 700; font-size: 17px; }
.status { font-size: 12.5px; color: var(--ink-soft); margin-top: 2px; }

/* —— 消息 —— */
.messages {
  flex: 1;
  overflow-y: auto;
  padding: 16px;
  display: flex;
  flex-direction: column;
  gap: 10px;
}
.bubble {
  max-width: 76%;
  padding: 10px 14px;
  border-radius: 18px;
  font-size: 14.5px;
  line-height: 1.5;
  white-space: pre-wrap;
  word-break: break-word;
}
.bubble.wang {
  align-self: flex-start;
  background: #fff;
  border: 1px solid var(--peach);
  border-bottom-left-radius: 6px;
}
.bubble.user {
  align-self: flex-end;
  background: var(--user-bubble);
  color: var(--user-ink);
  border-bottom-right-radius: 6px;
}
.typing { color: var(--ink-soft); }

/* —— 输入区 —— */
.composer { padding: 10px 14px 16px; border-top: 1px solid var(--cream-deep); flex-shrink: 0; }
.suggestions { display: flex; gap: 8px; flex-wrap: wrap; margin-bottom: 10px; }
.chip {
  border: 1px solid var(--peach);
  background: #fff;
  color: var(--ink);
  padding: 6px 12px;
  border-radius: 16px;
  font-size: 12.5px;
  cursor: pointer;
  transition: transform 0.1s, background 0.15s;
}
.chip:hover:not(:disabled) { background: var(--cream-deep); transform: translateY(-1px); }
.chip:disabled { opacity: 0.5; cursor: default; }

.input-row { display: flex; gap: 8px; }
.field {
  flex: 1;
  border: 1px solid var(--peach);
  border-radius: 20px;
  padding: 11px 16px;
  font-size: 14px;
  outline: none;
  color: var(--ink);
  background: #fff;
}
.field:focus { border-color: var(--orange); }
.field::placeholder { color: var(--ink-soft); }
.send {
  border: none;
  background: linear-gradient(135deg, var(--orange), var(--orange-deep));
  color: #fff;
  font-weight: 600;
  padding: 0 20px;
  border-radius: 20px;
  cursor: pointer;
  font-size: 14px;
  transition: opacity 0.15s;
}
.send:disabled { opacity: 0.45; cursor: default; }
</style>
