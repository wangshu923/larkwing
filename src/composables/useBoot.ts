import { ref, readonly, onUnmounted } from 'vue'

/**
 * 启动序列编排层(与皮肤/内容解耦的 ViewModel 状态)。
 * 对外只给 phase('boot'→'ready')和 progress(0→1);各组件订阅它自己做入场。
 * - 可跳过:任意键 / 点击立即进 ready。
 * - 只冷启播:同一 webview 会话内(含 HMR / reload)不重播。
 * 以后接 Pinia 时把内部状态平移过去即可,接口不变。
 */
export function useBoot(durationMs = 1800) {
  const phase = ref<'boot' | 'ready'>('boot')
  const progress = ref(0)
  let raf = 0
  let startTs = 0

  function cleanup() {
    cancelAnimationFrame(raf)
    window.removeEventListener('keydown', skip)
    window.removeEventListener('pointerdown', skip)
  }
  function finish() {
    progress.value = 1
    phase.value = 'ready'
    cleanup()
  }
  function skip() { finish() }

  function tick(ts: number) {
    if (!startTs) startTs = ts
    progress.value = Math.min((ts - startTs) / durationMs, 1)
    if (progress.value >= 1) { finish(); return }
    raf = requestAnimationFrame(tick)
  }

  function run() {
    if (sessionStorage.getItem('lw-booted')) { finish(); return }
    sessionStorage.setItem('lw-booted', '1')
    window.addEventListener('keydown', skip)
    window.addEventListener('pointerdown', skip)
    raf = requestAnimationFrame(tick)
  }

  onUnmounted(cleanup)
  return { phase: readonly(phase), progress: readonly(progress), run, skip }
}
