// 模拟后端：没有 Tauri 时（npm run dev 在浏览器里开发界面）使用。
// 行为尽量贴近策划书描述的核心层，但数据都是假的。
import type { Backend, BackendEvents, Unlisten } from '../backend'
import { ApiError } from '../errors'
import {
  DEFAULT_PERMISSIONS,
  SELF_ID,
  type ConflictPolicy,
  type DesktopInputEvent,
  type Device,
  type DeviceId,
  type EditSyncPrompt,
  type FsCacheHit,
  type FsEntry,
  type LanPeer,
  type LinkKind,
  type PairRequest,
  type ProcInfo,
  type Settings,
  type TaskKind,
  type TaskProgress,
} from '../types'
import { baseName, isInside } from '../../lib/path'
import { ALREADY_OPEN, ELEVATED_APPS, MOCK_SELF_ID, PHASE1_FEATURES, initialDevices, mockApps, mockProcs, type MockDevice } from './data'
import { MockFs } from './fs'

type Listener = (payload: never) => void
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms))

interface MockDesktopSession {
  wake: (() => void) | null
  closed: boolean
  dirty: boolean
  frames: number
  lastInput: string
}

/** 模拟后端的桌面画面：画一张 1280×720 的 JPEG，写上帧号和最近一次输入。 */
async function mockDesktopJpeg(frame: number, lastInput: string): Promise<Uint8Array<ArrayBuffer>> {
  const c = document.createElement('canvas')
  c.width = 1280
  c.height = 720
  const g = c.getContext('2d')
  if (g) {
    g.fillStyle = '#1e293b'
    g.fillRect(0, 0, c.width, c.height)
    g.fillStyle = '#e2e8f0'
    g.textAlign = 'center'
    g.font = '28px sans-serif'
    g.fillText(`模拟远程桌面 · 逐步模式 · 第 ${frame} 帧`, 640, 340)
    g.font = '18px sans-serif'
    g.fillText(lastInput || '点击画面或发送文字后会回传新画面', 640, 380)
  }
  const blob = await new Promise<Blob | null>((r) => c.toBlob(r, 'image/jpeg', 0.8))
  return new Uint8Array(blob ? await blob.arrayBuffer() : new ArrayBuffer(0))
}

function describeInput(ev: DesktopInputEvent): string {
  switch (ev.kind) {
    case 'text':
      return `最近输入：文字「${ev.text}」`
    case 'shortcut':
      return `最近输入：组合键 ${ev.combo}`
    case 'key':
      return `最近输入：按键 0x${ev.vk.toString(16)} ${ev.down ? '按下' : '松开'}`
    case 'release':
      return '最近输入：释放全部按键'
    case 'move':
      return `最近输入：移动到 (${ev.x}, ${ev.y})`
    default:
      return `最近输入：${ev.kind} (${ev.x}, ${ev.y})`
  }
}
const SPEED: Record<LinkKind, number> = { local: 420e6, direct: 92e6, relay: 2.8e6 }

interface RunningTask {
  p: TaskProgress
  finish: () => void
}

export function createMockBackend(): Backend & { debug: MockDebug } {
  const listeners = new Map<string, Set<Listener>>()
  const emit = <K extends keyof BackendEvents>(event: K, payload: BackendEvents[K]) => {
    for (const cb of listeners.get(event) ?? []) (cb as (p: BackendEvents[K]) => void)(payload)
  }

  const devices = new Map<DeviceId, MockDevice>(initialDevices().map((d) => [d.id, d]))
  const fss = new Map<DeviceId, MockFs>([[SELF_ID, new MockFs('Administrator', ['C:', 'D:'])]])
  for (const d of devices.values()) fss.set(d.id, new MockFs(d.user, d.drives))
  const procs = new Map<DeviceId, ProcInfo[]>()
  const tasks = new Map<string, RunningTask>()
  const cache = new Map<string, FsCacheHit>()
  const pendingPairs = new Map<string, PairRequest>()
  const pendingEdits = new Map<string, EditSyncPrompt>()
  const desktopSessions = new Map<number, MockDesktopSession>()
  let desktopSeq = 0
  let settings: Settings = {
    deviceName: '客厅主机',
    autostart: false,
    autostartElevated: false,
    autostartPrompted: true,
    quickHotkey: 'Alt+Space',
    clipboardAutoSync: false,
  }
  let hostPaused = false
  let speedLimitBps: number | null = null
  let offer: { code: string; expiresAt: number; timer?: number } | null = null
  let pairFailures = 0
  let seq = 0

  const pub = (d: MockDevice): Device => ({
    id: d.id,
    name: d.name,
    os: d.os,
    status: d.status,
    features: d.features,
    permissions: d.permissions,
  })

  async function reach(id: DeviceId, workMs = 2): Promise<MockFs> {
    if (id === SELF_ID) {
      await sleep(workMs)
      return fss.get(SELF_ID)!
    }
    const d = devices.get(id)
    if (!d) throw new ApiError('unauthorized', '这台设备已被移除，需要重新配对')
    if (d.status.state !== 'online') throw new ApiError('offline', `“${d.name}”现在不在线，等它上线后再试`)
    await sleep(d.status.rttMs + workMs)
    return fss.get(id)!
  }

  function linkOf(a: DeviceId, b: DeviceId): LinkKind {
    const slow = [a, b].map((id) => {
      const s = devices.get(id)?.status
      return s?.state === 'online' ? s.link : 'local'
    })
    return slow.includes('relay') ? 'relay' : slow.includes('direct') ? 'direct' : 'local'
  }

  function startTask(kind: TaskKind, title: string, src: DeviceId, dst: DeviceId, total: number, onDone: () => void): string {
    const id = `t${++seq}`
    const rawSpeed = SPEED[linkOf(src, dst)] * (0.85 + Math.random() * 0.3)
    const speed = speedLimitBps ? Math.min(rawSpeed, speedLimitBps) : rawSpeed
    const p: TaskProgress = { id, kind, title, srcDevice: src, dstDevice: dst, bytesDone: 0, bytesTotal: Math.max(total, 1), speed, state: 'running' }
    tasks.set(id, { p, finish: onDone })
    emit('task-progress', { ...p })
    return id
  }

  setInterval(() => {
    for (const [id, t] of tasks) {
      const p = t.p
      if (p.state === 'paused') continue
      const offline = [p.srcDevice, p.dstDevice].some((d) => d !== SELF_ID && devices.get(d)?.status.state !== 'online')
      if (offline) {
        p.state = 'failed'
        p.error = { code: 'offline', msg: '传输途中对方断线了，等它上线后重新开始' }
      } else {
        const cap = speedLimitBps ?? Infinity
        p.speed = Math.min(cap, p.speed * (0.95 + Math.random() * 0.1))
        p.bytesDone = Math.min(p.bytesTotal, p.bytesDone + p.speed / 10)
        if (p.bytesDone >= p.bytesTotal) {
          try {
            t.finish()
            p.state = 'done'
          } catch (e) {
            p.state = 'failed'
            p.error = e instanceof ApiError ? e.body : { code: 'internal', msg: '任务失败' }
          }
        }
      }
      emit('task-progress', { ...p })
      if (p.state !== 'running') tasks.delete(id)
    }
  }, 100)

  setInterval(() => {
    for (const d of devices.values()) {
      if (d.status.state !== 'online') continue
      const base = d.status.link === 'direct' ? 3 : 46
      d.status = { ...d.status, rttMs: Math.max(1, Math.round(base + (Math.random() - 0.5) * base * 0.4)) }
      emit('device-status', pub(d))
    }
  }, 2000)

  function entriesOf(fs: MockFs, paths: string[]): FsEntry[] {
    return paths.map((p) => {
      const e = fs.stat(p)
      if (!e) throw new ApiError('not_found', `找不到“${baseName(p)}”，可能已被移走`)
      return e
    })
  }

  function localOp(kind: 'copy' | 'move', device: DeviceId, sources: string[], destDir: string, fs: MockFs) {
    for (const s of sources) {
      if (isInside(destDir, s)) throw new ApiError('invalid', `不能把“${baseName(s)}”${kind === 'copy' ? '复制' : '移动'}到它自己里面`)
    }
    const items = entriesOf(fs, sources)
    const total = kind === 'move' ? items.length * 1_000_000 : sources.reduce((n, s) => n + fs.sizeOf(s), 0)
    const verb = kind === 'copy' ? '复制' : '移动'
    const title = `${verb} ${items.length > 1 ? `${items.length} 项` : items[0].name} → ${baseName(destDir) || destDir}`
    return startTask(kind, title, device, device, total, () => {
      items.forEach((e, i) => {
        fs.put(destDir, e)
        if (kind === 'move') fs.remove(sources[i])
      })
    })
  }

  const debug: MockDebug = {
    setOnline(id, online) {
      const d = devices.get(id)
      if (!d) return
      d.status = online ? { state: 'online', link: 'direct', rttMs: 3 } : { state: 'offline', lastSeen: Date.now() }
      emit('device-status', pub(d))
    },
    pairRequest() {
      const req: PairRequest = {
        requestId: `r${++seq}`,
        deviceId: 'c4e8b2d6f0a4c8e2b6d0f4a8c2e6b0d4f8a2c6e0b4d8f2a6c0e4b8d2f6a0c4e8',
        deviceName: '客厅电脑',
      }
      pendingPairs.set(req.requestId, req)
      emit('pair-request', req)
    },
  }

  const mockLanPeers: LanPeer[] = [
    {
      endpointId: 'e1f2a3b4c5d6e7f8a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a1b2c3d4e5f6',
      deviceName: '卧室笔记本',
      os: 'windows',
      addrs: ['192.168.1.28:49210'],
      seenAtMs: Date.now(),
    },
  ]

  return {
    kind: 'mock',
    debug,

    async selfInfo() {
      return { id: MOCK_SELF_ID, name: settings.deviceName, version: '0.4.1（界面预览）' }
    },
    async getSettings() {
      return { ...settings }
    },
    async setSettings(s) {
      await sleep(80)
      settings = { ...s }
    },
    async hostStatus() {
      return { paused: hostPaused, workingSetBytes: 26 * 1024 * 1024 }
    },
    async hostSetPaused(paused) {
      hostPaused = paused
      emit('host-paused', { paused })
    },

    async listDevices() {
      await sleep(30)
      return [...devices.values()].map(pub)
    },
    async renameDevice(id, name) {
      const d = devices.get(id)
      if (!d) throw new ApiError('not_found', '找不到这台设备')
      d.name = name
      emit('device-status', pub(d))
    },
    async removeDevice(id) {
      devices.delete(id)
      emit('device-removed', { id })
    },
    async setPermissions(id, permissions) {
      const d = devices.get(id)
      if (!d) throw new ApiError('not_found', '找不到这台设备')
      d.permissions = { ...permissions }
      emit('device-status', pub(d))
    },
    async reconnect(id) {
      const d = devices.get(id)
      if (!d) return
      d.status = { state: 'connecting' }
      emit('device-status', pub(d))
      await sleep(1500)
      d.status = d.unreachable ? { state: 'offline', lastSeen: Date.now() - 3 * 86400_000 } : { state: 'online', link: 'direct', rttMs: 3 }
      emit('device-status', pub(d))
      if (d.unreachable) throw new ApiError('offline', `还是连不上“${d.name}”。确认它已开机、程序在运行，并且能上网`)
    },
    async lanList() {
      return mockLanPeers.filter((p) => !devices.has(p.endpointId))
    },

    async pairBegin() {
      if (offer?.timer) clearTimeout(offer.timer)
      const code = String(Math.floor(100000 + Math.random() * 900000))
      offer = { code, expiresAt: Date.now() + 5 * 60_000 }
      offer.timer = window.setTimeout(() => debug.pairRequest(), 6000)
      return { code, expiresAt: offer.expiresAt, invite: `pcshare:${MOCK_SELF_ID}:${code}` }
    },
    async pairCancel() {
      if (offer?.timer) clearTimeout(offer.timer)
      offer = null
    },
    async pairConnect(target, code) {
      let id = target.trim()
      const m = /^pcshare:([0-9a-f]{64}):(\d{6})$/i.exec(id)
      if (m) {
        id = m[1]
        code = m[2]
      }
      if (!/^[0-9a-f]{64}$/i.test(id)) throw new ApiError('invalid', '身份码应该是 64 位的数字和字母（0-9、a-f），检查是否抄错')
      if (!/^\d{6}$/.test(code)) throw new ApiError('invalid', '配对码是 6 位数字')
      if (devices.has(id.toLowerCase())) throw new ApiError('already_exists', '这台设备已经配对过了')
      await sleep(900)
      if (pairFailures >= 5) throw new ApiError('expired', '配对码输错太多次，已经作废。请让对方重新生成')
      if (code !== '123456') {
        pairFailures++
        const left = 5 - pairFailures
        throw new ApiError(left > 0 ? 'bad_code' : 'expired', left > 0 ? `配对码不对，还能再试 ${left} 次（模拟环境的正确配对码是 123456）` : '配对码输错太多次，已经作废。请让对方重新生成')
      }
      pairFailures = 0
      const d: MockDevice = {
        id: id.toLowerCase(),
        name: '新电脑',
        os: 'windows',
        status: { state: 'online', link: 'direct', rttMs: 5 },
        features: PHASE1_FEATURES,
        permissions: { ...DEFAULT_PERMISSIONS },
        user: 'user',
        drives: ['C:'],
        desktop: { place: 'normal', elevated: false },
      }
      devices.set(d.id, d)
      fss.set(d.id, new MockFs(d.user, d.drives))
      emit('device-status', pub(d))
      return pub(d)
    },
    async pairRespond(requestId, allow, permissions) {
      pendingPairs.delete(requestId)
      if (offer?.timer) clearTimeout(offer.timer)
      offer = null
      if (!allow) return
      const d: MockDevice = {
        id: 'c4e8b2d6f0a4c8e2b6d0f4a8c2e6b0d4f8a2c6e0b4d8f2a6c0e4b8d2f6a0c4e8',
        name: '客厅电脑',
        os: 'windows',
        status: { state: 'online', link: 'direct', rttMs: 2 },
        features: PHASE1_FEATURES,
        permissions,
        user: 'family',
        drives: ['C:', 'D:'],
        desktop: { place: 'normal', elevated: false },
      }
      devices.set(d.id, d)
      fss.set(d.id, new MockFs(d.user, d.drives))
      emit('device-status', pub(d))
    },
    async pairPendingList() {
      return [...pendingPairs.values()]
    },

    async fsRoots(device) {
      return (await reach(device)).roots
    },
    async fsCacheGet(device, path) {
      return cache.get(`${device}|${path.toLowerCase()}`) ?? null
    },
    async fsList(device, path) {
      const fs = await reach(device, 4)
      const listing = fs.list(path)
      const entries = listing.entries.map((e) => ({ ...e }))
      if (entries.length > 500) {
        const batchSize = 500
        for (let i = 0; i < entries.length; i += batchSize) {
          const slice = entries.slice(i, i + batchSize)
          emit('fs-list-batch', {
            device,
            path: listing.path,
            entries: slice,
            totalHint: entries.length,
            done: i + batchSize >= entries.length,
          })
          await sleep(16)
        }
      } else {
        await sleep(entries.length / 400)
      }
      cache.set(`${device}|${listing.path.toLowerCase()}`, {
        path: listing.path,
        entries,
        cachedAt: Date.now(),
      })
      return { path: listing.path, entries }
    },
    async fsWatch() {},
    async fsUnwatch() {},
    async fsCheckConflicts(device, destDir, names) {
      const fs = await reach(device, 1)
      const existing = new Set(fs.list(destDir).entries.map((e) => e.name.toLowerCase()))
      return names.filter((n) => existing.has(n.toLowerCase()))
    },
    async fsMkdir(device, path) {
      ;(await reach(device)).mkdir(path)
    },
    async fsRename(device, path, newName) {
      ;(await reach(device)).rename(path, newName)
    },
    async fsCopy(device, sources, destDir) {
      return localOp('copy', device, sources, destDir, await reach(device))
    },
    async fsMove(device, sources, destDir) {
      return localOp('move', device, sources, destDir, await reach(device))
    },
    async fsDelete(device, paths) {
      const fs = await reach(device, 10)
      for (const p of paths) fs.remove(p)
    },

    async transfer(src, sources, dst, destDir, conflict: ConflictPolicy) {
      const from = await reach(src)
      const to = await reach(dst)
      to.list(destDir)
      const items = entriesOf(from, sources)
      const total = sources.reduce((n, s) => n + from.sizeOf(s), 0)
      const kind: TaskKind = src === SELF_ID ? 'upload' : dst === SELF_ID ? 'download' : 'relay'
      const title = items.length > 1 ? `${items[0].name} 等 ${items.length} 项` : items[0].name
      return startTask(kind, title, src, dst, total, () => {
        for (const e of items) to.put(destDir, { ...e, mtime: e.mtime }, conflict)
      })
    },
    async pauseTask(taskId) {
      const t = tasks.get(taskId)
      if (!t || t.p.state !== 'running') return
      t.p.state = 'paused'
      t.p.speed = 0
      emit('task-progress', { ...t.p })
    },
    async resumeTask(taskId) {
      const t = tasks.get(taskId)
      if (!t || t.p.state !== 'paused') return
      t.p.state = 'running'
      t.p.speed = SPEED[linkOf(t.p.srcDevice, t.p.dstDevice)]
      emit('task-progress', { ...t.p })
    },
    async cancelTask(taskId) {
      const t = tasks.get(taskId)
      if (!t) return
      t.p.state = 'cancelled'
      emit('task-progress', { ...t.p })
      tasks.delete(taskId)
    },
    async taskList() {
      return [...tasks.values()].map((t) => ({ ...t.p }))
    },
    async getSpeedLimit() {
      return speedLimitBps
    },
    async setSpeedLimit(bps) {
      speedLimitBps = bps
    },
    async pickLocalFiles() {
      await sleep(200)
      const firstFile = fss
        .get(SELF_ID)
        ?.list('C:\\Users\\Administrator\\Downloads')
        .entries.find((e) => !e.dir)
      return firstFile ? [`C:\\Users\\Administrator\\Downloads\\${firstFile.name}`] : []
    },
    async pickLocalDir() {
      await sleep(200)
      return 'C:\\Users\\Administrator\\Downloads'
    },

    async appList(device, _force) {
      await reach(device, 40)
      return mockApps()
    },
    async appIcon() {
      return null
    },
    async appLaunch(device, target) {
      await reach(device, 300)
      const d = devices.get(device)
      const desktop = d?.desktop ?? { place: 'normal' as const, elevated: true }
      const app = mockApps().find((a) => a.id === target)
      const name = app?.name ?? baseName(target)
      if (!app && !/^[A-Za-z]:\\/.test(target)) throw new ApiError('not_found', '找不到这个程序或文件')
      if (ELEVATED_APPS.has(name) && !desktop.elevated) return { newWindows: [], desktop, elevationPrompt: true }
      if (ALREADY_OPEN.has(name) || desktop.place !== 'normal') return { newWindows: [], desktop }
      return { newWindows: [name], desktop }
    },
    async procList(device) {
      await reach(device, 15)
      let list = procs.get(device)
      if (!list) {
        list = mockProcs(device)
        procs.set(device, list)
      }
      for (const p of list) p.cpu = Math.max(0, Math.round((p.name === 'chrome.exe' ? 8 : 1.2) * Math.random() * 10) / 10)
      return list.map((p) => ({ ...p }))
    },
    async procKill(device, pid) {
      await reach(device, 40)
      const list = procs.get(device) ?? []
      const p = list.find((x) => x.pid === pid)
      if (!p) throw new ApiError('not_found', '这个程序已经退出了')
      if (p.critical) throw new ApiError('forbidden', `“${p.name}”是系统关键进程，结束它会导致对方电脑蓝屏或注销，所以不允许`)
      procs.set(device, list.filter((x) => x.pid !== pid))
    },

    async diagSummary() {
      return {
        appVersion: '0.4.1',
        endpointId: MOCK_SELF_ID,
        deviceName: settings.deviceName,
        os: 'windows',
        workingSetBytes: 26 * 1024 * 1024,
        hostPaused,
        speedLimitBps,
        netcheckTargets: ['https://use1-1.relay.n0.iroh.link', 'https://euw1-1.relay.n0.iroh.link'],
        peerRoutes: [...devices.values()].map((d) => ({
          peerId: d.id,
          name: d.name,
          state:
            d.status.state === 'online'
              ? d.status.link === 'direct'
                ? 'online_direct'
                : 'online_relay'
              : d.status.state === 'connecting'
                ? 'connecting'
                : 'offline',
          detail:
            d.status.state === 'online'
              ? `${d.status.link === 'direct' ? '直连' : '中转'} · ${d.status.rttMs} ms`
              : '未连接',
          rttMs: d.status.state === 'online' ? d.status.rttMs : undefined,
        })),
      }
    },
    async diagNetcheck() {
      await sleep(600)
      return {
        checkedAtMs: Date.now(),
        udpIpv4: true,
        udpIpv6: false,
        mappingVariesByDestIp: false,
        upnpAvailable: true,
        pcpAvailable: false,
        natPmpAvailable: false,
        preferredRelay: 'https://use1-1.relay.n0.iroh.link/',
        relayLatencies: [{ url: 'https://use1-1.relay.n0.iroh.link/', rttMs: 42 }],
        summary: 'UDP IPv4 可用，NAT 端口映射固定，大概率支持跨网直连',
      }
    },
    async diagLogs() {
      return {
        runtimeLogs: ['[2026-11-16 11:00:00] INFO 核心节点启动完成'],
        audits: [],
      }
    },
    async diagExport(destDir) {
      return `${destDir}\\pcshare-diag-mock`
    },

    async launcherConsumePending() {
      return false
    },
    async quickSearch(query, limit = 10) {
      const q = query.trim().toLowerCase()
      return mockApps()
        .filter((a) => !q || a.name.toLowerCase().includes(q))
        .slice(0, limit)
        .map((a) => ({
          peerId: [...devices.keys()][0] ?? SELF_ID,
          peerName: '书房电脑',
          online: true,
          kind: 'app' as const,
          title: a.name,
          subtitle: '书房电脑 · 已装软件',
          target: a.id,
          launchKind: 'app_id' as const,
        }))
    },
    async quickLaunch(device, target) {
      return this.appLaunch(device, target)
    },
    async fsSearchStart(device, path, query) {
      const fs = await reach(device, 10)
      const searchId = `ms-${++seq}`
      const entries = fs
        .list(path)
        .entries.filter((e) => e.name.toLowerCase().includes(query.toLowerCase()))
        .map((e) => ({
          name: e.name,
          path: `${path.replace(/\\$/, '')}\\${e.name}`,
          dir: e.dir,
          size: e.size,
          mtime: e.mtime,
        }))
      window.setTimeout(() => {
        emit('fs-search-batch', { searchId, device, entries, scannedDirs: 1, done: true })
      }, 40)
      return searchId
    },
    async fsSearchCancel() {},
    async fsOpenRemote(device, path) {
      return this.appLaunch(device, path)
    },
    async fsOpenLocalEdit(device, path) {
      const d = devices.get(device)
      return {
        sessionId: `me-${++seq}`,
        peerId: device,
        peerName: d?.name ?? '书房电脑',
        remotePath: path,
        localPath: `C:\\Temp\\${baseName(path)}`,
        fileName: baseName(path),
      }
    },
    async fsEditSyncBack(sessionId) {
      pendingEdits.delete(sessionId)
      return { conflict: false, bytesWritten: 128 }
    },
    async editPendingList() {
      return [...pendingEdits.values()]
    },
    async fsDragOut() {
      return true
    },
    async shotMonitors(device) {
      await reach(device, 20)
      return {
        monitors: [
          {
            index: 0,
            name: '主显示器 #1（1920×1080）',
            originX: 0,
            originY: 0,
            width: 1920,
            height: 1080,
            dpiScalePercent: 100,
            isPrimary: true,
          },
        ],
        windows: [{ hwnd: '65552', pid: 1024, title: '文件资源管理器', width: 1200, height: 800 }],
        desktop: { place: 'normal', elevated: false },
      }
    },
    async shotCapture() {
      const svg = encodeURIComponent(
        '<svg xmlns="http://www.w3.org/2000/svg" width="1280" height="720"><rect width="100%" height="100%" fill="#1e293b"/><text x="50%" y="50%" fill="#e2e8f0" font-size="28" text-anchor="middle">远程屏幕截图预览 (1280×720)</text></svg>',
      )
      return {
        imageDataUrl: `data:image/svg+xml;utf8,${svg}`,
        byteLen: 512,
        capturedAtMs: Date.now(),
        transform: {
          originX: 0,
          originY: 0,
          sourceWidth: 1920,
          sourceHeight: 1080,
          imageWidth: 1280,
          imageHeight: 720,
          dpiScalePercent: 100,
          monitorIndex: 0,
        },
        desktop: { place: 'normal', elevated: false },
      }
    },
    async inputClick() {
      return { uipiBlocked: false }
    },
    async inputKey() {
      return { uipiBlocked: false }
    },
    async desktopOpen(opts) {
      if (opts.device === SELF_ID) throw new ApiError('invalid', '查看本机请用截图')
      await reach(opts.device, 20)
      const sessionId = ++desktopSeq
      desktopSessions.set(sessionId, { wake: null, closed: false, dirty: true, frames: 0, lastInput: '' })
      // 模拟对方原来是 2560×1440，按本机分辨率临时切换
      const fit =
        opts.fitWidth && opts.fitHeight
          ? { applied: true, width: opts.fitWidth, height: opts.fitHeight, previousWidth: 2560, previousHeight: 1440 }
          : undefined
      return { sessionId, rawInput: true, fit }
    },
    async desktopNext(sessionId) {
      const s = desktopSessions.get(sessionId)
      if (!s) throw new ApiError('not_found', '桌面会话已关闭')
      if (!s.dirty) await new Promise<void>((r) => (s.wake = r))
      if (s.closed) throw new ApiError('not_found', '桌面会话已关闭')
      s.dirty = false
      s.frames += 1
      const data = await mockDesktopJpeg(s.frames, s.lastInput)
      return {
        meta: {
          mode: 'step',
          format: 'jpeg',
          timestampUs: Date.now() * 1000,
          key: true,
          epoch: 0,
          encoder: 'mock',
          reason: s.frames > 1 ? '模拟后端只提供逐步截图' : null,
          transform: {
            originX: 0,
            originY: 0,
            sourceWidth: 1920,
            sourceHeight: 1080,
            imageWidth: 1280,
            imageHeight: 720,
            dpiScalePercent: 100,
            monitorIndex: 0,
          },
        },
        data,
      }
    },
    async desktopInput(sessionId, events) {
      const s = desktopSessions.get(sessionId)
      if (!s) throw new ApiError('not_found', '桌面会话已关闭')
      await sleep(15)
      const done = events.filter(
        (e) => !(e.kind === 'move' || ((e.kind === 'key' || e.kind === 'button') && e.down)),
      )
      if (done.length > 0) {
        s.lastInput = describeInput(done[done.length - 1])
        s.dirty = true
        s.wake?.()
      }
    },
    async desktopRefresh(sessionId) {
      const s = desktopSessions.get(sessionId)
      if (!s) throw new ApiError('not_found', '桌面会话已关闭')
      s.dirty = true
      s.wake?.()
    },
    async desktopClose(sessionId) {
      const s = desktopSessions.get(sessionId)
      if (!s) throw new ApiError('not_found', '桌面会话已关闭')
      s.closed = true
      s.wake?.()
      desktopSessions.delete(sessionId)
    },
    async powerExec(_device, action) {
      return { action, cancelled: false, executed: true, message: `已触发 ${action}` }
    },
    async powerCancel() {
      return true
    },
    async powerWake() {
      return { localPackets: 2, relayPeersNotified: 1, relayedPackets: 2 }
    },
    async termOpen(_device, shell) {
      const sessionId = `mt-${++seq}`
      window.setTimeout(() => {
        emit('term-output', {
          sessionId,
          dataB64: btoa(`Mock ${shell} session ready.\r\nPS C:\\Users\\Lenovo> `),
        })
      }, 30)
      return { sessionId, pid: 4321, shellName: `${shell}.exe` }
    },
    async termWrite(sessionId, dataB64) {
      emit('term-output', { sessionId, dataB64 })
    },
    async termResize() {},
    async termClose() {},
    async clipPush() {
      return 'text'
    },
    async clipPull() {
      return { kind: 'text', text: '来自模拟设备的剪贴板内容' }
    },
    async clipPeek() {
      return { kind: 'text', text: '当前剪贴板内容示例' }
    },
    async batchExec(opts) {
      return opts.devices.map((pid) => ({
        peerId: pid,
        peerName: devices.get(pid)?.name ?? '本机',
        ok: true,
        summary: '模拟执行完成',
        output: opts.command ? `Executed: ${opts.command}` : undefined,
        elapsedMs: 24,
      }))
    },

    on<K extends keyof BackendEvents>(event: K, cb: (payload: BackendEvents[K]) => void): Unlisten {
      let set = listeners.get(event)
      if (!set) listeners.set(event, (set = new Set()))
      set.add(cb as Listener)
      return () => set.delete(cb as Listener)
    },
  }
}

export interface MockDebug {
  setOnline(id: DeviceId, online: boolean): void
  pairRequest(): void
}
