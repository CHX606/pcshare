import { create } from 'zustand'
import { backend } from '../api'
import { humanError } from '../api/errors'
import type { DeviceId, TaskProgress } from '../api/types'
import { deviceName } from './devices'
import { toast } from './toasts'

/** 任务完成后需要刷新的目录 */
interface TaskTarget {
  device: DeviceId
  dirs: string[]
}

interface TasksState {
  tasks: TaskProgress[]
  panelOpen: boolean
  speedLimitBps: number | null
  upsert(p: TaskProgress): void
  pause(id: string): Promise<void>
  resume(id: string): Promise<void>
  cancel(id: string): Promise<void>
  setSpeedLimit(bps: number | null): Promise<void>
  clearFinished(): void
  setPanel(open: boolean): void
}

const targets = new Map<string, TaskTarget>()
type DoneHandler = (t: TaskTarget) => void
let onTaskDone: DoneHandler = () => {}

export function setTaskDoneHandler(h: DoneHandler) {
  onTaskDone = h
}

export function registerTask(id: string, target: TaskTarget) {
  targets.set(id, target)
}

const VERB = { upload: '上传', download: '下载', relay: '传输', copy: '复制', move: '移动' } as const
const isActive = (s: TaskProgress['state']) => s === 'running' || s === 'paused'

export const useTasks = create<TasksState>()((set) => ({
  tasks: [],
  panelOpen: false,
  speedLimitBps: null,
  upsert(p) {
    set((s) => {
      const i = s.tasks.findIndex((t) => t.id === p.id)
      const prev = i >= 0 ? s.tasks[i] : undefined
      if ((prev && isActive(prev.state) && !isActive(p.state)) || (!prev && !isActive(p.state))) {
        announce(p)
      }
      if (!prev) {
        const isTransfer = p.kind === 'upload' || p.kind === 'download' || p.kind === 'relay'
        return { tasks: [p, ...s.tasks], panelOpen: s.panelOpen || isTransfer }
      }
      const tasks = s.tasks.slice()
      tasks[i] = p
      return { tasks }
    })
  },
  async pause(id) {
    try {
      await backend.pauseTask(id)
    } catch (e) {
      toast.error(humanError(e))
    }
  },
  async resume(id) {
    try {
      await backend.resumeTask(id)
    } catch (e) {
      toast.error(humanError(e))
    }
  },
  async cancel(id) {
    try {
      await backend.cancelTask(id)
    } catch (e) {
      toast.error(humanError(e))
    }
  },
  async setSpeedLimit(bps) {
    try {
      await backend.setSpeedLimit(bps)
      set({ speedLimitBps: bps })
    } catch (e) {
      toast.error(humanError(e))
    }
  },
  clearFinished() {
    set((s) => ({ tasks: s.tasks.filter((t) => isActive(t.state)) }))
  },
  setPanel(open) {
    set({ panelOpen: open })
  },
}))

function announce(p: TaskProgress) {
  if (isActive(p.state)) return
  const verb = VERB[p.kind]
  const where = p.kind === 'download' ? '本机' : deviceName(p.dstDevice)
  const local = p.kind === 'copy' || p.kind === 'move'
  if (p.state === 'done') toast.success(local ? `已完成：${p.title}` : `已${verb}“${p.title}”到 ${where}`)
  if (p.state === 'failed') toast.error(`${local ? p.title : `${verb}“${p.title}”`}失败：${p.error?.msg ?? '未知原因'}`)
  const target = targets.get(p.id)
  if (target) {
    targets.delete(p.id)
    onTaskDone(target)
  }
}

export function wireTaskEvents(): () => void {
  void backend
    .getSpeedLimit()
    .then((bps) => useTasks.setState({ speedLimitBps: bps }))
    .catch(() => {})
  return backend.on('task-progress', (p) => useTasks.getState().upsert(p))
}
