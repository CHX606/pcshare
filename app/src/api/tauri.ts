import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import type { Backend, BackendEvents, Unlisten } from './backend'
import { toApiError } from './errors'
import { parseDesktopPacket } from '../lib/desktop'
import type { Reply } from './types'

/**
 * 真实实现：每个方法对应 src-tauri 里的一条命令。
 * 每条命令返回 Reply<T>：{ ok: true, data } 或 { ok: false, err: { code, msg } }。
 */
async function call<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  let reply: Reply<T>
  try {
    reply = await invoke<Reply<T>>(cmd, args)
  } catch (e) {
    throw toApiError(e)
  }
  if (reply.ok) return reply.data
  throw toApiError(reply.err)
}

const EVENT_NAMES: Record<keyof BackendEvents, string> = {
  'device-status': 'device://status',
  'device-removed': 'device://removed',
  'pair-request': 'pair://request',
  'task-progress': 'task://progress',
  'fs-list-batch': 'fs://list-batch',
  'fs-changed': 'fs://changed',
  'lan-changed': 'lan://changed',
  'host-paused': 'host://paused',
  'launcher-toggle': 'launcher://toggle',
  'fs-search-batch': 'fs://search-batch',
  'term-output': 'term://output',
  'term-exit': 'term://exit',
  'edit-prompt': 'edit://prompt',
  'batch-item': 'batch://item',
  'app-notice': 'app://notice',
}

export function createTauriBackend(): Backend {
  return {
    kind: 'tauri',
    selfInfo: () => call('self_info'),
    getSettings: () => call('settings_get'),
    setSettings: (settings) => call('settings_set', { settings }),
    hostStatus: () => call('host_status'),
    hostSetPaused: (paused) => call('host_set_paused', { paused }),

    listDevices: () => call('device_list'),
    renameDevice: (id, name) => call('device_rename', { id, name }),
    removeDevice: (id) => call('device_remove', { id }),
    setPermissions: (id, permissions) => call('device_set_permissions', { id, permissions }),
    reconnect: (id) => call('device_reconnect', { id }),
    lanList: () => call('lan_list'),

    pairBegin: () => call('pair_begin'),
    pairCancel: () => call('pair_cancel'),
    pairConnect: (target, code) => call('pair_connect', { target, code }),
    pairRespond: (requestId, allow, permissions) =>
      call('pair_respond', { requestId, allow, permissions }),
    pairPendingList: () => call('pair_pending_list'),

    fsRoots: (device) => call('fs_roots', { device }),
    fsCacheGet: (device, path) => call('fs_cache_get', { device, path }),
    fsList: (device, path) => call('fs_list', { device, path }),
    fsWatch: (device, path) => call('fs_watch', { device, path }).then(() => undefined),
    fsUnwatch: (device, path) => call('fs_unwatch', { device, path }).then(() => undefined),
    fsCheckConflicts: (device, destDir, names) =>
      call('fs_check_conflicts', { device, destDir, names }),
    fsMkdir: (device, path) => call('fs_mkdir', { device, path }),
    fsRename: (device, path, newName) => call('fs_rename', { device, path, newName }),
    fsCopy: (device, sources, destDir) => call('fs_copy', { device, sources, destDir }),
    fsMove: (device, sources, destDir) => call('fs_move', { device, sources, destDir }),
    fsDelete: (device, paths, permanent) => call('fs_delete', { device, paths, permanent }),

    transfer: (src, sources, dst, destDir, conflict) =>
      call('transfer_start', { src, sources, dst, destDir, conflict }),
    pauseTask: (taskId) => call('task_pause', { taskId }),
    resumeTask: (taskId) => call('task_resume', { taskId }),
    cancelTask: (taskId) => call('task_cancel', { taskId }),
    taskList: () => call('task_list'),
    getSpeedLimit: () => call('transfer_get_speed_limit'),
    setSpeedLimit: (bytesPerSec) => call('transfer_set_speed_limit', { bytesPerSec }),
    pickLocalFiles: () => call('pick_local_files'),
    pickLocalDir: () => call('pick_local_dir'),

    appList: (device, force = false) => call('app_list', { device, force: Boolean(force) }),
    appIcon: (device, appId) => call('app_icon', { device, appId }),
    appLaunch: (device, target, args) => call('app_launch', { device, target, args: args ?? null }),
    procList: (device) => call('proc_list', { device }),
    procKill: (device, pid) => call('proc_kill', { device, pid }),

    diagSummary: () => call('diag_summary'),
    diagNetcheck: () => call('diag_netcheck'),
    diagLogs: (limit = 200) => call('diag_logs', { limit }),
    diagExport: (destDir) => call('diag_export', { destDir }),

    launcherConsumePending: () => call('launcher_consume_pending'),
    quickSearch: (query, limit = 12) => call('quick_search', { query, limit }),
    quickLaunch: (device, target, kind) => call('quick_launch', { device, target, kind }),

    fsSearchStart: (device, path, query) => call('fs_search_start', { device, path, query }),
    fsSearchCancel: (searchId) => call('fs_search_cancel', { searchId }),
    fsOpenRemote: (device, path) => call('fs_open_remote', { device, path }),
    fsOpenLocalEdit: (device, path, launchApp = true) =>
      call('fs_open_local_edit', { device, path, launchApp }),
    fsEditSyncBack: (sessionId, autoSync, forceOverwrite) =>
      call('fs_edit_sync_back', { sessionId, autoSync, forceOverwrite }),
    editPendingList: () => call('edit_pending_list'),
    fsDragOut: (device, paths) => call('fs_drag_out', { device, paths }),

    shotMonitors: (device) => call('shot_monitors', { device }),
    shotCapture: (opts) =>
      call('shot_capture', {
        device: opts.device,
        monitorIndex: opts.monitorIndex ?? null,
        hwnd: opts.hwnd ?? null,
        maxWidth: opts.maxWidth ?? null,
        quality: opts.quality ?? null,
      }),
    inputClick: (opts) =>
      call('input_click', {
        device: opts.device,
        screenX: opts.screenX,
        screenY: opts.screenY,
        button: opts.button,
        double: opts.double,
        targetHwnd: opts.targetHwnd ?? null,
        refreshMonitor: opts.refreshMonitor ?? null,
        refreshHwnd: opts.refreshHwnd ?? null,
        maxWidth: opts.maxWidth ?? null,
      }),
    inputKey: (opts) =>
      call('input_key', {
        device: opts.device,
        text: opts.text ?? null,
        keyCombo: opts.keyCombo ?? null,
        targetHwnd: opts.targetHwnd ?? null,
        refreshMonitor: opts.refreshMonitor ?? null,
        refreshHwnd: opts.refreshHwnd ?? null,
        maxWidth: opts.maxWidth ?? null,
      }),

    desktopOpen: (opts) =>
      call('desktop_open', {
        device: opts.device,
        monitorIndex: opts.monitorIndex ?? null,
        hwnd: opts.hwnd ?? null,
        fitWidth: opts.fitWidth ?? null,
        fitHeight: opts.fitHeight ?? null,
      }),
    desktopNext: async (sessionId) => {
      // 成功时直接返回二进制，失败时以 { code, msg } 拒绝，不走 Reply 包装
      let buf: ArrayBuffer
      try {
        buf = await invoke<ArrayBuffer>('desktop_next', { sessionId })
      } catch (e) {
        throw toApiError(e)
      }
      return parseDesktopPacket(buf)
    },
    desktopInput: (sessionId, events) => call('desktop_input', { sessionId, events }),
    desktopRefresh: (sessionId) => call('desktop_refresh', { sessionId }),
    desktopClose: (sessionId) => call('desktop_close', { sessionId }),

    powerExec: (device, action, countdownSecs) =>
      call('power_exec', { device, action, countdownSecs: countdownSecs ?? null }),
    powerCancel: (device) => call('power_cancel', { device }),
    powerWake: (device) => call('power_wake', { device }),

    termOpen: (device, shell, cols, rows, cwd) =>
      call('term_open', { device, shell, cols, rows, cwd: cwd ?? null }),
    termWrite: (sessionId, dataB64) => call('term_write', { sessionId, dataB64 }),
    termResize: (sessionId, cols, rows) => call('term_resize', { sessionId, cols, rows }),
    termClose: (sessionId) => call('term_close', { sessionId }),

    clipPush: (device) => call('clip_push', { device }),
    clipPull: (device) => call('clip_pull', { device }),
    clipPeek: (device) => call('clip_peek', { device }),

    batchExec: (opts) =>
      call('batch_exec', {
        devices: opts.devices,
        op: opts.op,
        target: opts.target ?? null,
        powerAction: opts.powerAction ?? null,
        command: opts.command ?? null,
        shell: opts.shell ?? null,
      }),

    on<K extends keyof BackendEvents>(event: K, cb: (payload: BackendEvents[K]) => void): Unlisten {
      let off: Unlisten | null = null
      let cancelled = false
      void listen<BackendEvents[K]>(EVENT_NAMES[event], (e) => cb(e.payload)).then((fn) => {
        if (cancelled) fn()
        else off = fn
      })
      return () => {
        cancelled = true
        off?.()
      }
    },
  }
}
