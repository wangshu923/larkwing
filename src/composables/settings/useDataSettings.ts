// 设置·系统·数据:数据根搬家 / 一键备份 / 自动备份 / 从备份恢复。
// 从 SettingsView 抽出(2026-09-08)。

import { computed, onMounted, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import { api, isTauri } from '../../lib/backend'
import { useToast } from '../useToast'
import { useSettings } from '../useSettings'

export function useDataSettings(petName: { value: string }) {
  const { t } = useI18n()
  const settings = useSettings()

  // 数据目录「搬家」(datadir):选目录 → 预检 → 内联确认 → 执行(HUD 进度,完后自动重启)。
  const dataRoot = ref('')
  const oldDataRoot = ref<string | null>(null)
  const relocateBusy = ref(false)
  const relocateError = ref('')
  const pendingMove = ref<{ picked: string; newRoot: string; needBytes: number } | null>(null)
  const gb = (n: number) => (n / 1073741824).toFixed(1)
  async function loadDataLocation() {
    if (!isTauri()) return
    try {
      const loc = await api.dataLocation()
      dataRoot.value = loc.root
      oldDataRoot.value = loc.oldRoot
    } catch (e) {
      console.error('读取数据位置失败', e)
    }
  }
  function revealData() {
    if (isTauri()) void api.revealDataDir()
  }
  async function relocate() {
    if (relocateBusy.value || !isTauri()) return
    relocateError.value = ''
    pendingMove.value = null
    const picked = await api.pickDataFolder()
    if (!picked) return
    try {
      const check = await api.relocatePrecheck(picked)
      if (!check.ok) {
        relocateError.value = t(`settings.system.dataErr.${check.reason ?? 'failed'}`)
        return
      }
      pendingMove.value = { picked, newRoot: check.newRoot ?? '', needBytes: check.needBytes }
    } catch (e) {
      console.error('搬家预检失败', e)
      relocateError.value = t('settings.system.relocateFailed')
    }
  }
  async function confirmRelocate() {
    const pm = pendingMove.value
    if (!pm) return
    relocateBusy.value = true
    try {
      await api.relocateData(pm.picked) // 成功 = 翻指针后自动重启,页面随之刷新,不会走到这下面
    } catch (e) {
      relocateBusy.value = false
      pendingMove.value = null
      console.error('搬家失败', e)
      relocateError.value = t('settings.system.relocateFailed')
    }
  }
  function cancelRelocate() {
    pendingMove.value = null
  }
  // 一键备份:选目录 → 导出 larkwing-backup-<时间戳>.zip(DB 快照 + 克隆音色)。不重启。
  const backupBusy = ref(false)
  const backupMsg = ref('')
  const backupErr = ref(false)
  async function backupNow() {
    if (backupBusy.value || relocateBusy.value || !isTauri()) return
    backupMsg.value = ''
    backupErr.value = false
    const dest = await api.pickDataFolder()
    if (!dest) return
    backupBusy.value = true
    try {
      const zip = await api.backupData(dest)
      backupMsg.value = t('settings.system.backupDone', { path: zip })
    } catch (e) {
      console.error('备份失败', e)
      backupErr.value = true
      backupMsg.value = t('settings.system.backupFailed')
    } finally {
      backupBusy.value = false
    }
  }
  // 自动备份(autobackup.rs 水位线):选目标目录即开(每周一份、保留最近 10 份、机器轮转);
  // 清空 = 关。选完立即出第一份(auto_backup_now),别让用户等下个节拍才见着结果。
  const autoBackup = ref<import('../../lib/backend').AutoBackupStatus>({ dir: null, lastOkMs: null, lastError: null, keep: 0, intervalDays: 0 })
  const autoBackupBusy = ref(false)
  async function refreshAutoBackup() {
    if (!isTauri()) return
    try {
      autoBackup.value = await api.autoBackupStatus()
    } catch (e) {
      console.error('自动备份状态拉取失败', e)
    }
  }
  async function autoBackupPick() {
    if (autoBackupBusy.value || !isTauri()) return
    const dest = await api.pickDataFolder()
    if (!dest) return
    autoBackupBusy.value = true
    try {
      await settings.set('backup.auto.dir', dest)
      await api.autoBackupNow() // 选完立即出第一份(成功即写水位)
    } catch (e) {
      console.error('自动备份首份失败', e)
      useToast().error(t('settings.system.autoBackupFirstFailed'))
    } finally {
      autoBackupBusy.value = false
      void refreshAutoBackup()
    }
  }
  async function autoBackupOff() {
    if (autoBackupBusy.value) return
    await settings.set('backup.auto.dir', '')
    void refreshAutoBackup()
  }
  const autoBackupLine = computed(() => {
    const s = autoBackup.value
    if (!s.dir) return ''
    if (s.lastOkMs) {
      const d = new Date(s.lastOkMs)
      // 份数 / 间隔来自 core 状态(单源常量过桥),文案里不写死数字(§4.11)
      return t('settings.system.autoBackupLast', { time: d.toLocaleString(), days: s.intervalDays, keep: s.keep })
    }
    return s.lastError ? t('settings.system.autoBackupErr', { err: s.lastError }) : t('settings.system.autoBackupPending')
  })

  // 从备份恢复:选 zip → 预检(结构/魔数/迁移版本)→ 内联确认 → 负载暂存 + 自动重启,
  // 下次启动开库前落位(现库留 pre-restore 保险副本)。backup 的另一半。
  const restoreBusy = ref(false)
  const restoreError = ref('')
  const pendingRestore = ref<{ zip: string; dbBytes: number; clones: number } | null>(null)
  const mb = (n: number) => (Math.max(n, 104858) / 1048576).toFixed(1)
  async function restorePick() {
    if (restoreBusy.value || relocateBusy.value || backupBusy.value || !isTauri()) return
    restoreError.value = ''
    pendingRestore.value = null
    const zip = await api.pickBackupFile()
    if (!zip) return
    try {
      const check = await api.restorePrecheck(zip)
      if (!check.ok) {
        restoreError.value = t(`settings.system.restoreErr.${check.reason ?? 'not_backup'}`, { name: petName.value })
        return
      }
      pendingRestore.value = { zip, dbBytes: check.dbBytes, clones: check.clones }
    } catch (e) {
      console.error('恢复预检失败', e)
      restoreError.value = t('settings.system.restoreFailed')
    }
  }
  async function confirmRestore() {
    const pr = pendingRestore.value
    if (!pr) return
    restoreBusy.value = true
    try {
      await api.restoreData(pr.zip) // 成功 = 暂存后自动重启落位,不会走到这下面
    } catch (e) {
      restoreBusy.value = false
      pendingRestore.value = null
      console.error('恢复失败', e)
      restoreError.value = t('settings.system.restoreFailed')
    }
  }
  function cancelRestore() {
    pendingRestore.value = null
  }
  async function cleanupOld() {
    try {
      await api.cleanupOldData()
    } catch (e) {
      console.error('清理旧数据失败', e)
    } finally {
      oldDataRoot.value = null
    }
  }
  async function keepOld() {
    try {
      await api.keepOldData()
    } catch (e) {
      console.error(e)
    } finally {
      oldDataRoot.value = null
    }
  }

  // 自动备份状态(目录/上次成功)进页即拉 —— 原先搭在设置页那个大 onMounted 里,
  // 抽 composable 时挪到这儿,挂载时机不变。
  onMounted(() => void refreshAutoBackup())

  return {
    autoBackup,
    autoBackupBusy,
    autoBackupLine,
    autoBackupOff,
    autoBackupPick,
    backupBusy,
    backupErr,
    backupMsg,
    backupNow,
    cancelRelocate,
    cancelRestore,
    cleanupOld,
    confirmRelocate,
    confirmRestore,
    dataRoot,
    gb,
    keepOld,
    loadDataLocation,
    mb,
    oldDataRoot,
    pendingMove,
    pendingRestore,
    relocate,
    relocateBusy,
    relocateError,
    restoreBusy,
    restoreError,
    restorePick,
    revealData,
  }
}
