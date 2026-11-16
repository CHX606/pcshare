# 电脑共享 · 界面（app/）

React + TypeScript + Vite 写的控制台界面，对应策划书第二册任务 1.12、1.13。
`src-tauri/`（Tauri 壳和桥接层，任务 1.1 / 1.11）还没建，这里只有界面。

## 运行

```powershell
npm install
npm run dev        # http://localhost:1420 ，在浏览器里用模拟数据
npm run build      # 类型检查 + 打包到 dist/
npm test           # 纯函数单元测试（路径、排序、打字跳转、模糊匹配）
npm run lint
```

在普通浏览器里打开时自动使用**模拟后端**（`src/api/mock/`），数据都是假的，方便单独开发界面：

- 书房电脑（直连）、公司电脑（中转、锁屏状态）、老笔记本（离线）
- `D:\五千个文件` 有 5000 项，用来验证虚拟滚动
- 配对时模拟环境的正确配对码是 `123456`；“让别的电脑连这台”打开 6 秒后会模拟一个配对请求
- 控制台里可用 `__pcshareMock.setOnline('<身份码>', false)` 让设备掉线

在 Tauri 窗口里运行时（`isTauri()` 为真）自动改走真实命令（`src/api/tauri.ts`）。

## 目录

| 路径 | 内容 |
|---|---|
| `src/api/types.ts` | 界面和桥接层交换的数据类型 |
| `src/api/backend.ts` | 界面能调用的全部功能（接口） |
| `src/api/tauri.ts` | 真实实现：Tauri 命令和事件 |
| `src/api/mock/` | 模拟实现 |
| `src/store/` | 界面状态（Zustand）：设备、文件、任务、软件和进程、提示、对话框 |
| `src/pages/` | 文件页、软件页、进程页、更多页 |
| `src/components/`、`src/dialogs/` | 侧栏、状态条、传输面板、右键菜单、各种对话框 |
| `src/lib/` | 路径、排序、格式化等纯函数 |

## 与桥接层的约定（给 src-tauri 实现用）

每条命令返回统一结构：`{ "ok": true, "data": ... }` 或 `{ "ok": false, "err": { "code": "...", "msg": "给人看的一句话" } }`。
错误码见第一册 6.3 节，另加 `offline`、`timeout`、`bad_code`、`expired`。
参数名按 Tauri 默认规则，界面传 camelCase，Rust 一侧写 snake_case。`device` 为 `"self"` 时表示本机。

| 命令 | 参数 | 返回 |
|---|---|---|
| `self_info` | — | `SelfInfo` |
| `settings_get` / `settings_set` | — / `settings` | `Settings` / — |
| `device_list` | — | `Device[]`（不含本机） |
| `device_rename` | `id, name` | — |
| `device_remove` | `id` | — |
| `device_set_permissions` | `id, permissions` | — |
| `device_reconnect` | `id` | — |
| `pair_begin` / `pair_cancel` | — | `PairOffer` / — |
| `pair_connect` | `target`（身份码或邀请码）, `code` | `Device` |
| `pair_respond` | `requestId, allow, permissions` | — |
| `fs_roots` | `device` | `FsRoot[]` |
| `fs_list` | `device, path` | `FsListing`（桥接层把分批结果合并后返回） |
| `fs_mkdir` | `device, path` | — |
| `fs_rename` | `device, path, newName` | — |
| `fs_copy` / `fs_move` | `device, sources, destDir` | 任务号 |
| `fs_delete` | `device, paths, permanent` | — |
| `transfer_start` | `src, sources, dst, destDir, conflict` | 任务号 |
| `task_cancel` | `taskId` | — |
| `pick_local_files` / `pick_local_dir` | — | `string[]` / `string \| null` |
| `app_list` | `device` | `AppInfo[]` |
| `app_launch` | `device, target, args` | `LaunchResult` |
| `proc_list` / `proc_kill` | `device` / `device, pid` | `ProcInfo[]` / — |

| 事件 | 载荷 |
|---|---|
| `device://status` | `Device`（状态、线路、延迟变化时推送） |
| `device://removed` | `{ id }` |
| `pair://request` | `PairRequest`（有设备请求配对） |
| `task://progress` | `TaskProgress`（每 100 毫秒最多一次） |

类型的字段定义以 `src/api/types.ts` 为准。

## 还没做

- 第 2 阶段的目录缓存存盘、变化推送、分批显示、拖拽、双栏多标签（界面目前在内存里保留最近读过的目录，离线时显示）
- 第 3 阶段功能，“更多”页里置灰显示
- 软件图标（`app.icon` 在第 2 阶段），现在用首字头像代替
