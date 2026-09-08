// 设置·家人:家人增删改名 / 渠道对话指认 / 声纹注册与遗忘。
// 从 SettingsView 抽出(2026-09-08)。

import { computed, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import { api, isTauri, type ChannelChat, type FamilyMember } from '../../lib/backend'
import { hydrateUser, useSettings } from '../useSettings'
import { useToast } from '../useToast'
import { onEnrollDone, useVoice } from '../useVoice'

export function useFamilySettings(restartWakeIfRunning: () => Promise<void>) {
  const { t } = useI18n()
  const settings = useSettings()

  /** 渠道对话「指认给谁」下拉项:还没指认 + 家人们。 */
  const famOpts = computed(() => [
    { value: '', label: t('settings.family.unassigned') },
    ...family.value.map((m) => ({ value: String(m.id), label: m.name })),
  ])
  // 家人页(渠道归人 = 多用户第一步,声纹后置):家人列表 CRUD + 手机对话指认给家人。
  // 指认后 TA 在手机上说的「提醒我 / 我喜欢…」归 TA 自己(speaker_user 缝,记忆归人 §6)。
  const family = ref<FamilyMember[]>([])
  const chats = ref<ChannelChat[]>([])
  const famError = ref(false)
  const famEditing = ref(0) // 行内改名中的家人 id;0 = 没有
  const famDraft = ref('')
  const famNew = ref('')
  const famArm = ref(0) // 删除二次确认中的家人 id(clone 删除同款手势)
  async function loadFamily() {
    if (!isTauri()) {
      // 浏览器预览假数据(UI 摸手感;与后端 FamilyMember/ChannelChat 同构)。
      // 预览没有 boot 过桥 → 补上当前用户 id,让「(你)/防删自己」渲染与真机一致
      if (!settings.state.userId) hydrateUser(1, settings.state.userName)
      family.value = [
        { id: 1, name: settings.state.userName, skin_id: 'scifi', created_at: 0, last_active_at: 0, enrolled: false },
        { id: 2, name: '豆豆', skin_id: 'scifi', created_at: 0, last_active_at: 0, enrolled: false },
      ]
      chats.value = [
        { id: 1, channel: 'telegram', ext_id: '12345678', conv_id: 9, user_id: 2, label: 'Doudou' },
        { id: 2, channel: 'dingtalk', ext_id: 'cidXXXX', conv_id: 10, user_id: null, label: '妈妈' },
      ]
      return
    }
    famError.value = false
    try {
      const [f, c] = await Promise.all([api.listFamily(), api.listChannelChats()])
      family.value = f
      chats.value = c
    } catch {
      famError.value = true // 初载失败别装空(§6.6):错误态 + 重试
    }
  }
  function startFamRename(m: FamilyMember) {
    famDraft.value = m.name
    famEditing.value = m.id
  }
  async function saveFamRename(m: FamilyMember) {
    const name = famDraft.value.trim()
    famEditing.value = 0
    if (!name || name === m.name) return
    try {
      // 自己走既有 rename 链(同步顶栏「现在陪着」名);家人按 id 改
      if (m.id === settings.state.userId) await settings.rename(name)
      else await api.renameFamily(m.id, name)
      await loadFamily()
    } catch {
      useToast().error(t('toast.actionFailed'))
    }
  }
  async function addFam() {
    const name = famNew.value.trim()
    if (!name) return
    try {
      await api.addFamily(name)
      famNew.value = ''
      await loadFamily()
    } catch {
      useToast().error(t('toast.actionFailed'))
    }
  }
  async function removeFam(m: FamilyMember) {
    if (famArm.value !== m.id) {
      famArm.value = m.id
      return
    }
    famArm.value = 0
    try {
      await api.removeFamily(m.id)
      await loadFamily()
    } catch {
      useToast().error(t('toast.deleteFailed'))
    }
  }
  async function bindChat(c: ChannelChat, v: string) {
    const uid = v ? Number(v) : null
    try {
      await api.bindChannelChat(c.id, uid)
      c.user_id = uid
    } catch {
      useToast().error(t('toast.actionFailed'))
      await loadFamily() // 失败把 select 拉回真值
    }
  }
  function channelName(id: string): string {
    return id === 'telegram' ? 'Telegram' : id === 'dingtalk' ? t('settings.family.dingtalk') : id
  }

  // 声纹注册(多用户第二步):让旺财凭声音认出家人 → TA 说的话记忆/提醒归 TA(§渠道归人第二步)。
  // 录 3 段取平均(core);进度/终态走 voice 的 enroll 事件。owner 也可录(便于与家人区分,§4.2)。
  const voice = useVoice()
  /** 某家人此刻是否正在录(preparing/recording)——决定卡片显进度还是显按钮。 */
  function enrollBusy(id: number): boolean {
    const e = voice.state.enroll
    return e.userId === id && (e.stage === 'preparing' || e.stage === 'recording')
  }
  function enrollLabel(id: number): string {
    const e = voice.state.enroll
    if (e.userId !== id) return ''
    return e.stage === 'preparing'
      ? t('settings.family.enrollPreparing')
      : t('settings.family.enrollRecording', { n: e.done + 1, total: e.total })
  }
  function startEnrollFam(m: FamilyMember) {
    voice.startEnroll(m.id)
  }
  async function forgetVoice(m: FamilyMember) {
    try {
      await voice.unenroll(m.id)
      if (!isTauri()) m.enrolled = false
      else {
        await loadFamily()
        await restartWakeIfRunning() // 唤醒循环要重载声纹库(少一个候选)才生效
      }
      useToast().ok(t('settings.family.forgetDone'))
    } catch {
      useToast().error(t('toast.actionFailed'))
    }
  }
  // 注册终态:成功 = toast + 刷新「已录」+ 唤醒在跑就重启让新声纹生效;失败 = toast 请重试(§3.5)
  onEnrollDone(async (userId, ok) => {
    if (!ok) {
      useToast().error(t('settings.family.enrollFailed'))
      return
    }
    useToast().ok(t('settings.family.enrollDone'))
    if (!isTauri()) {
      const m = family.value.find((x) => x.id === userId)
      if (m) m.enrolled = true // 预览:本地标已录看视觉
    } else {
      await loadFamily()
      await restartWakeIfRunning() // 首次注册后唤醒循环要重载声纹库才认得出
    }
  })

  return {
    addFam,
    bindChat,
    channelName,
    chats,
    enrollBusy,
    enrollLabel,
    famArm,
    famDraft,
    famEditing,
    famError,
    famNew,
    famOpts,
    family,
    forgetVoice,
    loadFamily,
    removeFam,
    saveFamRename,
    startEnrollFam,
    startFamRename,
    voice,
  }
}
