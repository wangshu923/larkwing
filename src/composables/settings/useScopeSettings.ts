// 设置·系统·能碰的文件夹(文件授权圈 §7.2):三档 × 出厂基线 + 用户自己加的目录。
// 从 SettingsView 抽出(2026-09-08)。

import { computed, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import { api, isTauri, type ScopeEntry, type ScopeMode } from '../../lib/backend'
import { useToast } from '../useToast'

export function useScopeSettings() {
  const { t } = useI18n()

  // —— 文件授权圈(§7.2「能碰的文件夹」):模型只能读写这里允许的文件夹,圈外先问。 ——
  // 内置区:程序数据(恒可用,说明行)+ 下载/桌面(出厂「可存入」,升「完全访问」= 落一条
  // 表记录、降回 = 删记录);用户条目三档可调、可删。添加复用 pick_data_folder 原生选择器。
  const scopes = ref<ScopeEntry[]>([])
  const scopeDownloads = ref<string | null>(null)
  const scopeDesktop = ref<string | null>(null)
  async function loadScopes() {
    if (!isTauri()) {
      // 浏览器预览:假数据看交互
      scopeDownloads.value = '/Users/demo/Downloads'
      scopeDesktop.value = '/Users/demo/Desktop'
      scopes.value = [{ path: '/Volumes/nas/电影', mode: 'full' }]
      return
    }
    try {
      const v = await api.fsScopes()
      scopes.value = v.entries
      scopeDownloads.value = v.downloads
      scopeDesktop.value = v.desktop
    } catch (e) {
      console.error('读取文件授权失败', e)
    }
  }
  /** 内置基线行(下载/桌面)当前档:表里有升档记录显其档,否则出厂「可存入」。 */
  function baselineMode(path: string | null): ScopeMode {
    if (!path) return 'create'
    return scopes.value.find((e) => e.path === path)?.mode ?? 'create'
  }
  /** 用户条目 = 表里剔除「内置基线的升档记录」(那俩显示在固定行的档位上,不重复列)。 */
  const userScopes = computed(() =>
    scopes.value.filter((e) => e.path !== scopeDownloads.value && e.path !== scopeDesktop.value),
  )
  const scopeModeOpts = computed(() => [
    { value: 'read', label: t('settings.scopes.modeRead') },
    { value: 'create', label: t('settings.scopes.modeCreate') },
    { value: 'full', label: t('settings.scopes.modeFull') },
  ])
  /** 内置行不给「只读」——最低就是出厂基线「可存入」。 */
  const baselineModeOpts = computed(() => [
    { value: 'create', label: t('settings.scopes.modeCreate') },
    { value: 'full', label: t('settings.scopes.modeFull') },
  ])
  async function setScopeMode(path: string, mode: string) {
    if (!isTauri()) return
    try {
      scopes.value = await api.fsScopeSet(path, mode as ScopeMode)
    } catch (e) {
      console.error('改文件授权失败', e)
      useToast().error(t('toast.actionFailed'))
    }
  }
  async function setBaselineMode(path: string | null, mode: string) {
    if (!path) return
    // 降回「可存入」= 删升档记录(回落出厂基线);升档 = 普通入表
    if (mode === 'create') await removeScopeRow(path)
    else await setScopeMode(path, mode)
  }
  async function removeScopeRow(path: string) {
    if (!isTauri()) return
    try {
      scopes.value = await api.fsScopeRemove(path)
    } catch (e) {
      console.error('删文件授权失败', e)
      useToast().error(t('toast.actionFailed'))
    }
  }
  async function addScopeFolder() {
    if (!isTauri()) return
    try {
      const picked = await api.pickDataFolder()
      if (!picked) return
      scopes.value = await api.fsScopeSet(picked, 'read') // 新加默认只读,列表里再升档
    } catch (e) {
      console.error('添加文件授权失败', e)
      useToast().error(t('toast.actionFailed'))
    }
  }

  return {
    addScopeFolder,
    baselineMode,
    baselineModeOpts,
    loadScopes,
    removeScopeRow,
    scopeDesktop,
    scopeDownloads,
    scopeModeOpts,
    scopes,
    setBaselineMode,
    setScopeMode,
    userScopes,
  }
}
