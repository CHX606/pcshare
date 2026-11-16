// 模拟数据：设备、软件、进程。只用于浏览器里开发界面。
import { DEFAULT_PERMISSIONS, type AppInfo, type Device, type DesktopState, type Feature, type ProcInfo } from '../types'
import { rng } from './fs'

export const PHASE1_FEATURES: Feature[] = [
  'fs',
  'transfer',
  'apps',
  'procs',
  'shot',
  'input',
  'power',
  'clip',
  'term',
  'search',
]

export const MOCK_SELF_ID = 'b7e3a1c94f0d2e8857a6c3b1d09f4e2a7c5b8d1e3f6a9c2b4d7e0f1a3c5b7d9e'

export interface MockDevice extends Device {
  user: string
  drives: string[]
  desktop: DesktopState
  /** 模拟时永远连不上 */
  unreachable?: boolean
}

export function initialDevices(): MockDevice[] {
  return [
    {
      id: '3f9a6c2e1b8d4f7a0c5e9b2d6a1f8c3e7b0d4a9f2c6e1b5d8a3f7c0e4b9d2a6f',
      name: '书房电脑',
      os: 'windows',
      status: { state: 'online', link: 'direct', rttMs: 3 },
      features: PHASE1_FEATURES,
      permissions: { ...DEFAULT_PERMISSIONS },
      user: 'Lenovo',
      drives: ['C:', 'D:'],
      desktop: { place: 'normal', elevated: false },
    },
    {
      id: 'a1c5e9b3d7f2a6c0e4b8d2f6a0c4e8b2d6f0a4c8e2b6d0f4a8c2e6b0d4f8a2c6',
      name: '公司电脑',
      os: 'windows',
      status: { state: 'online', link: 'relay', rttMs: 46 },
      features: PHASE1_FEATURES,
      permissions: { ...DEFAULT_PERMISSIONS },
      user: 'zhang.wei',
      drives: ['C:'],
      desktop: { place: 'locked', elevated: false },
    },
    {
      id: '9d2b6f0a4c8e2b6d0f4a8c2e6b0d4f8a2c6e0b4d8f2a6c0e4b8d2f6a0c4e8b2d',
      name: '老笔记本',
      os: 'windows',
      status: { state: 'offline', lastSeen: Date.now() - 3 * 86400_000 },
      features: PHASE1_FEATURES,
      permissions: { ...DEFAULT_PERMISSIONS },
      user: 'admin',
      drives: ['C:'],
      desktop: { place: 'normal', elevated: true },
      unreachable: true,
    },
  ]
}

const APP_NAMES = [
  '微信', 'QQ', '钉钉', '企业微信', '飞书', 'Google Chrome', 'Microsoft Edge', 'Firefox',
  'Visual Studio Code', 'Notepad++', '记事本', '画图', '计算器', '命令提示符', 'Windows PowerShell',
  '任务管理器', '注册表编辑器', '控制面板', '设置', '文件资源管理器', 'Word', 'Excel', 'PowerPoint',
  'Outlook', 'OneNote', 'WPS Office', '网易云音乐', 'QQ音乐', '哔哩哔哩', '腾讯会议', 'Zoom',
  'Steam', 'Photoshop', 'Premiere Pro', 'OBS Studio', '7-Zip', 'Everything', 'PotPlayer',
  '百度网盘', '迅雷', 'Git Bash', 'Docker Desktop', 'Postman', 'Xshell', 'Typora', '截图工具',
]

export function mockApps(): AppInfo[] {
  return APP_NAMES.map((name, i) => ({
    name,
    id: name === '记事本' ? 'Microsoft.WindowsNotepad_8wekyb3d8bbwe!App' : `{6D809377-${String(i).padStart(4, '0')}}\\${name}.exe`,
  }))
}

/** 需要管理员权限才能打开的软件 */
export const ELEVATED_APPS = new Set(['注册表编辑器', '任务管理器', '命令提示符'])
/** 模拟“本来就开着”的软件 */
export const ALREADY_OPEN = new Set(['微信', '钉钉'])

const PROC_BASE: Array<[string, string, number, boolean]> = [
  ['System', '', 0.2, true],
  ['csrss.exe', '', 3, true],
  ['winlogon.exe', '', 4, true],
  ['lsass.exe', '', 9, true],
  ['services.exe', '', 7, true],
  ['explorer.exe', '文件资源管理器', 95, false],
  ['WeChat.exe', '微信', 320, false],
  ['chrome.exe', '新标签页 - Google Chrome', 610, false],
  ['Code.exe', 'main.rs - 电脑共享 - Visual Studio Code', 540, false],
  ['DingTalk.exe', '钉钉', 280, false],
  ['cloudmusic.exe', '网易云音乐', 190, false],
  ['svchost.exe', '', 24, false],
  ['svchost.exe', '', 18, false],
  ['RuntimeBroker.exe', '', 12, false],
  ['SearchHost.exe', '', 88, false],
  ['pcshare.exe', '电脑共享', 38, false],
  ['WINWORD.EXE', '季度报告.docx - Word', 210, false],
  ['Steam.exe', 'Steam', 150, false],
  ['dwm.exe', '', 70, true],
  ['audiodg.exe', '', 14, false],
]

export function mockProcs(seed: string): ProcInfo[] {
  const r = rng(seed.length * 977)
  return PROC_BASE.map(([name, title, mb, critical], i) => ({
    pid: [4, 612, 708, 820, 760][i] ?? 1000 + Math.floor(r() * 30000),
    name,
    title,
    memory: Math.round(mb * 1024 * 1024 * (0.9 + r() * 0.2)),
    cpu: 0,
    critical,
  }))
}
