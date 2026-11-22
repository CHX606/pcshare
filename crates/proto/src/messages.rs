use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::PROTOCOL_VERSION;
use crate::error::{ErrorCode, FrameError, RpcError};

fn default_true() -> bool {
    true
}

/// 权限类别（用于指令执行前检查）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionScope {
    /// 仅需已配对
    PairedOnly,
    /// 读文件
    FsRead,
    /// 写文件
    FsWrite,
    /// 开软件
    AppLaunch,
    /// 管进程
    ProcManage,
    /// 看屏幕截图
    Screenshot,
    /// 模拟鼠标键盘点击输入
    InputControl,
    /// 打开远端命令行终端
    Terminal,
    /// 电源操作（锁屏、睡眠、重启、关机）
    Power,
    /// 读写剪贴板
    Clipboard,
}

/// 单台已配对设备的权限表（见总体方案 7.3 节）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Permissions {
    #[serde(default = "default_true")]
    pub fs_read: bool,
    #[serde(default = "default_true")]
    pub fs_write: bool,
    #[serde(default = "default_true")]
    pub app_launch: bool,
    #[serde(default = "default_true")]
    pub proc_manage: bool,
    #[serde(default)]
    pub screenshot: bool,
    #[serde(default)]
    pub input_control: bool,
    #[serde(default)]
    pub terminal: bool,
    #[serde(default)]
    pub power: bool,
    #[serde(default)]
    pub clipboard: bool,
    /// 只开放这几个文件夹；为空表示不限制目录范围。
    #[serde(default)]
    pub allowed_roots: Vec<String>,
}

impl Default for Permissions {
    fn default() -> Self {
        Self {
            fs_read: true,
            fs_write: true,
            app_launch: true,
            proc_manage: true,
            screenshot: false,
            input_control: false,
            terminal: false,
            power: false,
            clipboard: false,
            allowed_roots: Vec::new(),
        }
    }
}

impl Permissions {
    pub fn allows(&self, scope: PermissionScope) -> bool {
        match scope {
            PermissionScope::PairedOnly => true,
            PermissionScope::FsRead => self.fs_read,
            PermissionScope::FsWrite => self.fs_write,
            PermissionScope::AppLaunch => self.app_launch,
            PermissionScope::ProcManage => self.proc_manage,
            PermissionScope::Screenshot => self.screenshot,
            PermissionScope::InputControl => self.input_control,
            PermissionScope::Terminal => self.terminal,
            PermissionScope::Power => self.power,
            PermissionScope::Clipboard => self.clipboard,
        }
    }
}

/// 请求外层结构：`{"v":1,"cmd":"fs.list","args":{...}}`。
/// 不设置 `deny_unknown_fields`，自动忽略未知字段以保持向后兼容。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestEnvelope {
    pub v: u32,
    pub cmd: String,
    #[serde(default = "empty_object")]
    pub args: serde_json::Value,
}

fn empty_object() -> serde_json::Value {
    serde_json::Value::Object(serde_json::Map::new())
}

/// 回复外层结构：
/// - 成功：`{"ok":true,"data":{...}}`
/// - 失败：`{"ok":false,"err":{"code":"not_found","msg":"给人看的一句话"}}`
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResponseEnvelope<T> {
    Ok(T),
    Err(RpcError),
}

impl<T> ResponseEnvelope<T> {
    pub fn ok(data: T) -> Self {
        Self::Ok(data)
    }

    pub fn err(code: ErrorCode, msg: impl Into<String>) -> Self {
        Self::Err(RpcError::new(code, msg))
    }

    pub fn into_result(self) -> Result<T, RpcError> {
        match self {
            Self::Ok(data) => Ok(data),
            Self::Err(err) => Err(err),
        }
    }
}

impl<T: Serialize> Serialize for ResponseEnvelope<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct OkWire<'a, T> {
            ok: bool,
            data: &'a T,
        }
        #[derive(Serialize)]
        struct ErrWire<'a> {
            ok: bool,
            err: &'a RpcError,
        }
        match self {
            Self::Ok(data) => OkWire { ok: true, data }.serialize(serializer),
            Self::Err(err) => ErrWire { ok: false, err }.serialize(serializer),
        }
    }
}

impl<'de, T: DeserializeOwned> Deserialize<'de> for ResponseEnvelope<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        enum FieldSlot<V> {
            Missing,
            Present(V),
        }
        #[allow(clippy::derivable_impls)]
        impl<V> Default for FieldSlot<V> {
            fn default() -> Self {
                Self::Missing
            }
        }
        impl<'de, V: Deserialize<'de>> Deserialize<'de> for FieldSlot<V> {
            fn deserialize<D2: Deserializer<'de>>(d: D2) -> Result<Self, D2::Error> {
                V::deserialize(d).map(Self::Present)
            }
        }

        #[derive(Deserialize)]
        #[serde(bound(deserialize = "V: Deserialize<'de>"))]
        struct RawWire<V> {
            ok: bool,
            #[serde(default)]
            data: FieldSlot<V>,
            #[serde(default)]
            err: Option<RpcError>,
        }

        let raw = RawWire::<T>::deserialize(deserializer)?;
        if raw.ok {
            let value = match raw.data {
                FieldSlot::Present(v) => v,
                FieldSlot::Missing => {
                    serde_json::from_value(serde_json::Value::Null).map_err(serde::de::Error::custom)?
                }
            };
            Ok(Self::Ok(value))
        } else {
            let err = raw.err.ok_or_else(|| {
                serde::de::Error::custom("失败回复缺少 err 字段")
            })?;
            Ok(Self::Err(err))
        }
    }
}

// ──────────────────────────── 阶段 1 全部指令 ────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    SysHello(HelloArgs),
    SysPing(PingArgs),
    SysInfo,
    FsRoots,
    FsList(FsListArgs),
    FsStat(FsStatArgs),
    FsMkdir(FsMkdirArgs),
    FsRename(FsRenameArgs),
    FsCopy(FsCopyMoveArgs),
    FsMove(FsCopyMoveArgs),
    FsDelete(FsDeleteArgs),
    FsTaskStatus(FsTaskIdArgs),
    FsTaskCancel(FsTaskIdArgs),
    FsRead(FsReadArgs),
    FsWrite(FsWriteArgs),
    FsHash(FsHashArgs),
    FsWatch(FsWatchArgs),
    FsUnwatch(FsUnwatchArgs),
    FsProbeResume(FsProbeResumeArgs),
    FsWalk(FsWalkArgs),
    FsWriteBatch(FsWriteBatchArgs),
    FsReadBatch(FsReadBatchArgs),
    PeerGrantDirect(PeerGrantDirectArgs),
    PeerDirectTransfer(PeerDirectTransferArgs),
    AppList(AppListArgs),
    AppIcon(AppIconArgs),
    AppLaunch(AppLaunchArgs),
    ProcList,
    ProcKill(ProcKillArgs),
    SubEvents,
    // 阶段 3 指令
    FsSearch(FsSearchArgs),
    ShotMonitors,
    ShotScreen(ShotScreenArgs),
    ShotWindow(ShotWindowArgs),
    InputClick(InputClickArgs),
    InputKey(InputKeyArgs),
    PowerExec(PowerExecArgs),
    PowerCancel,
    PowerWakeWol(PowerWakeWolArgs),
    ClipGet,
    ClipSet(ClipSetArgs),
    TermOpen(TermOpenArgs),
}

fn parse_args<T: DeserializeOwned>(cmd: &str, value: serde_json::Value) -> Result<T, RpcError> {
    serde_json::from_value(value).map_err(|err| {
        RpcError::new(
            ErrorCode::Invalid,
            format!("指令 {cmd} 的参数格式不正确：{err}"),
        )
    })
}

impl Command {
    pub fn cmd_name(&self) -> &'static str {
        match self {
            Self::SysHello(_) => "sys.hello",
            Self::SysPing(_) => "sys.ping",
            Self::SysInfo => "sys.info",
            Self::FsRoots => "fs.roots",
            Self::FsList(_) => "fs.list",
            Self::FsStat(_) => "fs.stat",
            Self::FsMkdir(_) => "fs.mkdir",
            Self::FsRename(_) => "fs.rename",
            Self::FsCopy(_) => "fs.copy",
            Self::FsMove(_) => "fs.move",
            Self::FsDelete(_) => "fs.delete",
            Self::FsTaskStatus(_) => "fs.task_status",
            Self::FsTaskCancel(_) => "fs.task_cancel",
            Self::FsRead(_) => "fs.read",
            Self::FsWrite(_) => "fs.write",
            Self::FsHash(_) => "fs.hash",
            Self::FsWatch(_) => "fs.watch",
            Self::FsUnwatch(_) => "fs.unwatch",
            Self::FsProbeResume(_) => "fs.probe_resume",
            Self::FsWalk(_) => "fs.walk",
            Self::FsWriteBatch(_) => "fs.write_batch",
            Self::FsReadBatch(_) => "fs.read_batch",
            Self::PeerGrantDirect(_) => "peer.grant_direct",
            Self::PeerDirectTransfer(_) => "peer.direct_transfer",
            Self::AppList(_) => "app.list",
            Self::AppIcon(_) => "app.icon",
            Self::AppLaunch(_) => "app.launch",
            Self::ProcList => "proc.list",
            Self::ProcKill(_) => "proc.kill",
            Self::SubEvents => "sub.events",
            Self::FsSearch(_) => "fs.search",
            Self::ShotMonitors => "shot.monitors",
            Self::ShotScreen(_) => "shot.screen",
            Self::ShotWindow(_) => "shot.window",
            Self::InputClick(_) => "input.click",
            Self::InputKey(_) => "input.key",
            Self::PowerExec(_) => "power.exec",
            Self::PowerCancel => "power.cancel",
            Self::PowerWakeWol(_) => "power.wake_wol",
            Self::ClipGet => "clip.get",
            Self::ClipSet(_) => "clip.set",
            Self::TermOpen(_) => "term.open",
        }
    }

    /// 任务 2.8：仅幂等读类指令允许在瞬断重连后自动重试一次，写类与产生副作用的指令绝不自动重试。
    pub fn is_idempotent_read(&self) -> bool {
        matches!(
            self,
            Self::SysPing(_)
                | Self::SysInfo
                | Self::FsRoots
                | Self::FsList(_)
                | Self::FsStat(_)
                | Self::FsTaskStatus(_)
                | Self::FsHash(_)
                | Self::FsProbeResume(_)
                | Self::FsWalk(_)
                | Self::FsSearch(_)
                | Self::AppList(_)
                | Self::AppIcon(_)
                | Self::ProcList
                | Self::ShotMonitors
                | Self::ShotScreen(_)
                | Self::ShotWindow(_)
                | Self::ClipGet
        )
    }

    pub fn required_permission(&self) -> PermissionScope {
        match self {
            Self::SysHello(_)
            | Self::SysPing(_)
            | Self::SysInfo
            | Self::SubEvents
            | Self::PowerWakeWol(_) => PermissionScope::PairedOnly,
            Self::FsRoots
            | Self::FsList(_)
            | Self::FsStat(_)
            | Self::FsRead(_)
            | Self::FsHash(_)
            | Self::FsWatch(_)
            | Self::FsUnwatch(_)
            | Self::FsWalk(_)
            | Self::FsReadBatch(_)
            | Self::FsSearch(_)
            | Self::PeerDirectTransfer(_) => PermissionScope::FsRead,
            Self::FsMkdir(_)
            | Self::FsRename(_)
            | Self::FsCopy(_)
            | Self::FsMove(_)
            | Self::FsDelete(_)
            | Self::FsTaskStatus(_)
            | Self::FsTaskCancel(_)
            | Self::FsWrite(_)
            | Self::FsProbeResume(_)
            | Self::FsWriteBatch(_)
            | Self::PeerGrantDirect(_) => PermissionScope::FsWrite,
            Self::AppList(_) | Self::AppIcon(_) | Self::AppLaunch(_) => PermissionScope::AppLaunch,
            Self::ProcList | Self::ProcKill(_) => PermissionScope::ProcManage,
            Self::ShotMonitors | Self::ShotScreen(_) | Self::ShotWindow(_) => {
                PermissionScope::Screenshot
            }
            Self::InputClick(_) | Self::InputKey(_) => PermissionScope::InputControl,
            Self::PowerExec(_) | Self::PowerCancel => PermissionScope::Power,
            Self::ClipGet | Self::ClipSet(_) => PermissionScope::Clipboard,
            Self::TermOpen(_) => PermissionScope::Terminal,
        }
    }

    pub fn to_envelope(&self) -> Result<RequestEnvelope, FrameError> {
        let args = match self {
            Self::SysHello(a) => serde_json::to_value(a)?,
            Self::SysPing(a) => serde_json::to_value(a)?,
            Self::SysInfo
            | Self::FsRoots
            | Self::ProcList
            | Self::SubEvents
            | Self::ShotMonitors
            | Self::PowerCancel
            | Self::ClipGet => empty_object(),
            Self::FsList(a) => serde_json::to_value(a)?,
            Self::FsStat(a) => serde_json::to_value(a)?,
            Self::FsMkdir(a) => serde_json::to_value(a)?,
            Self::FsRename(a) => serde_json::to_value(a)?,
            Self::FsCopy(a) | Self::FsMove(a) => serde_json::to_value(a)?,
            Self::FsDelete(a) => serde_json::to_value(a)?,
            Self::FsTaskStatus(a) | Self::FsTaskCancel(a) => serde_json::to_value(a)?,
            Self::FsRead(a) => serde_json::to_value(a)?,
            Self::FsWrite(a) => serde_json::to_value(a)?,
            Self::FsHash(a) => serde_json::to_value(a)?,
            Self::FsWatch(a) => serde_json::to_value(a)?,
            Self::FsUnwatch(a) => serde_json::to_value(a)?,
            Self::FsProbeResume(a) => serde_json::to_value(a)?,
            Self::FsWalk(a) => serde_json::to_value(a)?,
            Self::FsWriteBatch(a) => serde_json::to_value(a)?,
            Self::FsReadBatch(a) => serde_json::to_value(a)?,
            Self::PeerGrantDirect(a) => serde_json::to_value(a)?,
            Self::PeerDirectTransfer(a) => serde_json::to_value(a)?,
            Self::AppList(a) => serde_json::to_value(a)?,
            Self::AppIcon(a) => serde_json::to_value(a)?,
            Self::AppLaunch(a) => serde_json::to_value(a)?,
            Self::ProcKill(a) => serde_json::to_value(a)?,
            Self::FsSearch(a) => serde_json::to_value(a)?,
            Self::ShotScreen(a) => serde_json::to_value(a)?,
            Self::ShotWindow(a) => serde_json::to_value(a)?,
            Self::InputClick(a) => serde_json::to_value(a)?,
            Self::InputKey(a) => serde_json::to_value(a)?,
            Self::PowerExec(a) => serde_json::to_value(a)?,
            Self::PowerWakeWol(a) => serde_json::to_value(a)?,
            Self::ClipSet(a) => serde_json::to_value(a)?,
            Self::TermOpen(a) => serde_json::to_value(a)?,
        };
        Ok(RequestEnvelope {
            v: PROTOCOL_VERSION,
            cmd: self.cmd_name().to_string(),
            args,
        })
    }

    pub fn from_envelope(env: RequestEnvelope) -> Result<Self, RpcError> {
        if env.v != PROTOCOL_VERSION {
            return Err(RpcError::new(
                ErrorCode::Unsupported,
                format!(
                    "不支持对方的协议版本 v{}（本机版本为 v{}），请升级两端程序",
                    env.v, PROTOCOL_VERSION
                ),
            ));
        }
        let cmd = env.cmd.as_str();
        match cmd {
            "sys.hello" => Ok(Self::SysHello(parse_args(cmd, env.args)?)),
            "sys.ping" => Ok(Self::SysPing(parse_args(cmd, env.args)?)),
            "sys.info" => Ok(Self::SysInfo),
            "fs.roots" => Ok(Self::FsRoots),
            "fs.list" => Ok(Self::FsList(parse_args(cmd, env.args)?)),
            "fs.stat" => Ok(Self::FsStat(parse_args(cmd, env.args)?)),
            "fs.mkdir" => Ok(Self::FsMkdir(parse_args(cmd, env.args)?)),
            "fs.rename" => Ok(Self::FsRename(parse_args(cmd, env.args)?)),
            "fs.copy" => Ok(Self::FsCopy(parse_args(cmd, env.args)?)),
            "fs.move" => Ok(Self::FsMove(parse_args(cmd, env.args)?)),
            "fs.delete" => Ok(Self::FsDelete(parse_args(cmd, env.args)?)),
            "fs.task_status" => Ok(Self::FsTaskStatus(parse_args(cmd, env.args)?)),
            "fs.task_cancel" => Ok(Self::FsTaskCancel(parse_args(cmd, env.args)?)),
            "fs.read" => Ok(Self::FsRead(parse_args(cmd, env.args)?)),
            "fs.write" => Ok(Self::FsWrite(parse_args(cmd, env.args)?)),
            "fs.hash" => Ok(Self::FsHash(parse_args(cmd, env.args)?)),
            "fs.watch" => Ok(Self::FsWatch(parse_args(cmd, env.args)?)),
            "fs.unwatch" => Ok(Self::FsUnwatch(parse_args(cmd, env.args)?)),
            "fs.probe_resume" => Ok(Self::FsProbeResume(parse_args(cmd, env.args)?)),
            "fs.walk" => Ok(Self::FsWalk(parse_args(cmd, env.args)?)),
            "fs.write_batch" => Ok(Self::FsWriteBatch(parse_args(cmd, env.args)?)),
            "fs.read_batch" => Ok(Self::FsReadBatch(parse_args(cmd, env.args)?)),
            "peer.grant_direct" => Ok(Self::PeerGrantDirect(parse_args(cmd, env.args)?)),
            "peer.direct_transfer" => Ok(Self::PeerDirectTransfer(parse_args(cmd, env.args)?)),
            "app.list" => Ok(Self::AppList(parse_args(cmd, env.args)?)),
            "app.icon" => Ok(Self::AppIcon(parse_args(cmd, env.args)?)),
            "app.launch" => Ok(Self::AppLaunch(parse_args(cmd, env.args)?)),
            "proc.list" => Ok(Self::ProcList),
            "proc.kill" => Ok(Self::ProcKill(parse_args(cmd, env.args)?)),
            "sub.events" => Ok(Self::SubEvents),
            "fs.search" => Ok(Self::FsSearch(parse_args(cmd, env.args)?)),
            "shot.monitors" => Ok(Self::ShotMonitors),
            "shot.screen" => Ok(Self::ShotScreen(parse_args(cmd, env.args)?)),
            "shot.window" => Ok(Self::ShotWindow(parse_args(cmd, env.args)?)),
            "input.click" => Ok(Self::InputClick(parse_args(cmd, env.args)?)),
            "input.key" => Ok(Self::InputKey(parse_args(cmd, env.args)?)),
            "power.exec" => Ok(Self::PowerExec(parse_args(cmd, env.args)?)),
            "power.cancel" => Ok(Self::PowerCancel),
            "power.wake_wol" => Ok(Self::PowerWakeWol(parse_args(cmd, env.args)?)),
            "clip.get" => Ok(Self::ClipGet),
            "clip.set" => Ok(Self::ClipSet(parse_args(cmd, env.args)?)),
            "term.open" => Ok(Self::TermOpen(parse_args(cmd, env.args)?)),
            other => Err(RpcError::new(
                ErrorCode::Unsupported,
                format!("对方电脑的版本暂不支持这项功能（{other}）"),
            )),
        }
    }
}

// ────────────────────────── 系统类消息（sys.*）──────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloArgs {
    pub protocol_version: u32,
    pub app_version: String,
    pub device_name: String,
    pub os: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub mac_addresses: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloReply {
    pub protocol_version: u32,
    pub app_version: String,
    pub device_name: String,
    pub os: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub mac_addresses: Vec<String>,
    pub granted_permissions: Permissions,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PingArgs {
    #[serde(default)]
    pub seq: u64,
    #[serde(default)]
    pub client_ts_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PingReply {
    pub seq: u64,
    pub client_ts_ms: u64,
    pub server_ts_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesktopStatus {
    pub session_id: u32,
    pub console_session_id: u32,
    pub elevated: bool,
    pub input_desktop: Option<String>,
    pub interactive: bool,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskSpaceInfo {
    pub mount_point: String,
    pub label: String,
    pub total_bytes: u64,
    pub available_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryInfo {
    pub total_bytes: u64,
    pub available_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SysInfoReply {
    pub computer_name: String,
    pub user_name: String,
    pub os: String,
    pub app_version: String,
    pub desktop: DesktopStatus,
    pub disks: Vec<DiskSpaceInfo>,
    pub memory: MemoryInfo,
}

// ────────────────────────── 文件类消息（fs.*）──────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RootKind {
    Drive,
    Desktop,
    Downloads,
    Documents,
    Home,
    AllowedFolder,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootEntry {
    pub name: String,
    pub path: String,
    pub kind: RootKind,
    #[serde(default)]
    pub total_bytes: Option<u64>,
    #[serde(default)]
    pub available_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsRootsReply {
    pub drives: Vec<RootEntry>,
    pub special_folders: Vec<RootEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsListArgs {
    pub path: String,
    #[serde(default)]
    pub batch_size: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirEntry {
    pub name: String,
    pub path: String,
    pub kind: EntryKind,
    pub size: u64,
    #[serde(default)]
    pub modified_ms: Option<u64>,
    #[serde(default)]
    pub created_ms: Option<u64>,
    #[serde(default)]
    pub readonly: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsListBatch {
    pub path: String,
    pub entries: Vec<DirEntry>,
    pub has_more: bool,
    #[serde(default)]
    pub total_hint: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsStatArgs {
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsStatReply {
    pub entry: DirEntry,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsMkdirArgs {
    pub path: String,
    #[serde(default)]
    pub parents: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsMkdirReply {
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsRenameArgs {
    pub path: String,
    pub new_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsRenameReply {
    pub old_path: String,
    pub new_path: String,
}

/// 目标文件已存在时的处理策略（见分阶段任务 1.8 节）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictPolicy {
    /// 报错提示已存在（默认安全策略）
    #[default]
    Fail,
    /// 覆盖已有文件
    Overwrite,
    /// 跳过不处理
    Skip,
    /// 改名保留两份（例如 `报告 (1).docx`）
    RenameKeepBoth,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsCopyMoveArgs {
    pub sources: Vec<String>,
    pub dest_dir: String,
    #[serde(default)]
    pub conflict: ConflictPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsTaskReply {
    pub task_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsTaskIdArgs {
    pub task_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FsTaskKind {
    Copy,
    Move,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FsTaskState {
    Running,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsTaskProgress {
    pub task_id: String,
    pub kind: FsTaskKind,
    pub state: FsTaskState,
    pub processed_bytes: u64,
    pub total_bytes: u64,
    pub processed_items: usize,
    pub total_items: usize,
    #[serde(default)]
    pub current_item: Option<String>,
    #[serde(default)]
    pub error: Option<RpcError>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsDeleteArgs {
    pub paths: Vec<String>,
    /// 默认 `false`（放进回收站），显式为 `true` 时才永久删除。
    #[serde(default)]
    pub permanent: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsDeleteReply {
    pub deleted_count: usize,
}

// ────────────────────── 文件读写流消息（fs.read / fs.write）──────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsReadArgs {
    pub path: String,
    #[serde(default)]
    pub offset: u64,
    #[serde(default)]
    pub length: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsReadHeader {
    pub file_size: u64,
    pub transfer_length: u64,
    #[serde(default)]
    pub modified_ms: Option<u64>,
}

fn is_false(v: &bool) -> bool {
    !*v
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsWriteArgs {
    pub path: String,
    pub size: u64,
    #[serde(default)]
    pub offset: u64,
    /// 分片并发上传模式下本条流写入的字节长度；缺省 `None` 表示顺序写至文件末尾（兼容 0.4.1）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range_length: Option<u64>,
    /// 为 `true` 时写完本分片仅落盘保留 `.pcshare.part` 而不立即原子改名，由最终收尾请求统一落盘改名。
    #[serde(default, skip_serializing_if = "is_false")]
    pub defer_finalize: bool,
    #[serde(default)]
    pub modified_ms: Option<u64>,
    #[serde(default)]
    pub conflict: ConflictPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsWriteReady {
    pub skipped: bool,
    pub target_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsWriteDone {
    pub target_path: String,
    pub bytes_written: u64,
}

// ────────────── 阶段 2 文件增强消息（校验、监听、递归、批量小文件、直传）──────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsHashArgs {
    pub path: String,
    /// 为 `Some(n)` 时仅对前 `n` 字节算校验值（用于断点续传核对已有 `.part` 文件）。
    #[serde(default)]
    pub length: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsHashReply {
    pub size: u64,
    pub blake3_hex: String,
    #[serde(default)]
    pub modified_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsWatchArgs {
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsWatchReply {
    pub watched_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsUnwatchArgs {
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsUnwatchReply {
    pub watched_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsProbeResumeArgs {
    pub dest_dir: String,
    pub file_name: String,
    #[serde(default)]
    pub conflict: ConflictPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsProbeResumeReply {
    pub dest_exists: bool,
    pub target_path: String,
    pub part_bytes: u64,
    #[serde(default)]
    pub part_blake3_hex: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsWalkArgs {
    pub sources: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalkEntry {
    /// 相对目标目录的子路径（用 `\` 分隔，例如 `photos\2026\1.jpg` 或文件本名 `1.jpg`）
    pub rel_path: String,
    /// 源端规范绝对路径
    pub abs_path: String,
    pub dir: bool,
    pub size: u64,
    #[serde(default)]
    pub modified_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsWalkBatch {
    pub entries: Vec<WalkEntry>,
    pub done: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsWriteBatchArgs {
    pub dest_dir: String,
    #[serde(default)]
    pub conflict: ConflictPolicy,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchFileItem {
    /// 为 `true` 表示本批结束，后续无文件字节
    #[serde(default)]
    pub done: bool,
    #[serde(default)]
    pub rel_path: String,
    #[serde(default)]
    pub dir: bool,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub modified_ms: Option<u64>,
    #[serde(default)]
    pub blake3_hex: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsWriteBatchReply {
    pub written_files: u64,
    pub skipped_files: u64,
    pub written_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsReadBatchArgs {
    pub sources: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchReadItemHeader {
    #[serde(default)]
    pub done: bool,
    #[serde(default)]
    pub source_path: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub modified_ms: Option<u64>,
    #[serde(default)]
    pub blake3_hex: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerGrantDirectArgs {
    pub token: String,
    pub allowed_peer_id: String,
    pub dest_dir: String,
    pub ttl_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerGrantDirectReply {
    pub accepted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerDirectTransferArgs {
    pub transfer_id: String,
    pub target_peer_id: String,
    #[serde(default)]
    pub target_addrs: Vec<String>,
    #[serde(default)]
    pub target_relay_url: Option<String>,
    pub token: String,
    pub sources: Vec<String>,
    pub dest_dir: String,
    #[serde(default)]
    pub conflict: ConflictPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerDirectTransferReply {
    pub accepted: bool,
}

/// 走 `ALPN_DIRECT`（两台受控端直传专用通道）时的首帧请求。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum DirectStreamRequest {
    ProbeResume {
        token: String,
        rel_path: String,
        conflict: ConflictPolicy,
    },
    SingleFile {
        token: String,
        rel_path: String,
        size: u64,
        #[serde(default)]
        offset: u64,
        #[serde(default)]
        modified_ms: Option<u64>,
        #[serde(default)]
        conflict: ConflictPolicy,
        #[serde(default)]
        blake3_hex: Option<String>,
    },
    Mkdir {
        token: String,
        rel_path: String,
    },
    Batch {
        token: String,
        #[serde(default)]
        conflict: ConflictPolicy,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectTransferProgress {
    pub transfer_id: String,
    pub state: FsTaskState,
    pub bytes_done: u64,
    pub bytes_total: u64,
    #[serde(default)]
    pub error: Option<RpcError>,
}

// ────────────────────── 软件与进程消息（app.* / proc.*）──────────────────────

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppListArgs {
    #[serde(default)]
    pub refresh: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppIconArgs {
    pub app_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppIconReply {
    #[serde(default)]
    pub png_base64: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledApp {
    pub name: String,
    pub app_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppListReply {
    pub apps: Vec<InstalledApp>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchKind {
    /// 按开始菜单应用标识（`shell:AppsFolder\<id>`）
    #[default]
    AppId,
    /// 按软件名称匹配开始菜单软件
    AppName,
    /// 按可执行文件路径、文档路径或系统命令打开
    PathOrTarget,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppLaunchArgs {
    pub target: String,
    #[serde(default)]
    pub kind: LaunchKind,
    #[serde(default)]
    pub args: Option<String>,
    /// 为 `true` 时在系统接受启动后立即返回，不等待新窗口标题枚举（供快捷搜索框 <=300ms 快速启动使用）。
    #[serde(default)]
    pub skip_window_wait: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppLaunchReply {
    #[serde(default)]
    pub resolved_name: Option<String>,
    pub new_windows: Vec<String>,
    pub desktop: DesktopStatus,
    #[serde(default)]
    pub uac_prompt_suspected: bool,
    #[serde(default)]
    pub hint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcessEntry {
    pub pid: u32,
    pub name: String,
    #[serde(default)]
    pub window_title: Option<String>,
    pub memory_bytes: u64,
    pub cpu_percent: f32,
    pub is_critical: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcListReply {
    pub processes: Vec<ProcessEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcKillArgs {
    pub pid: u32,
    /// 必须显式二次确认（`true`）才允许结束进程。
    #[serde(default)]
    pub confirmed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcKillReply {
    pub pid: u32,
    pub name: String,
}

// ────────────────────────── 远端推送事件与配对消息 ──────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum RemoteEvent {
    TaskProgress(FsTaskProgress),
    DirChanged { path: String },
    DirectTransferProgress(DirectTransferProgress),
}

/// 配对请求消息（走 `pcshare/pair/1` 专用协议名）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairRequest {
    pub v: u32,
    pub code: String,
    pub requester_name: String,
    pub requester_os: String,
    #[serde(default)]
    pub requester_relay_url: Option<String>,
    #[serde(default)]
    pub requester_macs: Vec<String>,
    #[serde(default = "default_true")]
    pub mutual: bool,
    #[serde(default)]
    pub granted_to_responder: Permissions,
}

/// 配对回复消息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum PairResponse {
    Approved {
        responder_name: String,
        responder_os: String,
        #[serde(default)]
        responder_relay_url: Option<String>,
        #[serde(default)]
        responder_macs: Vec<String>,
        granted_to_requester: Permissions,
        mutual_accepted: bool,
    },
    Rejected {
        code: ErrorCode,
        msg: String,
    },
}

// ────────────── 阶段 3 消息（搜索、截图与点击、电源唤醒、剪贴板、终端）──────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsSearchArgs {
    pub path: String,
    pub query: String,
    #[serde(default)]
    pub max_results: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsSearchBatch {
    pub entries: Vec<DirEntry>,
    pub scanned_dirs: u64,
    pub done: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorInfo {
    pub index: u32,
    pub name: String,
    pub origin_x: i32,
    pub origin_y: i32,
    pub width: u32,
    pub height: u32,
    pub dpi_scale_percent: u32,
    pub is_primary: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapturableWindow {
    pub hwnd: u64,
    pub pid: u32,
    pub title: String,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShotMonitorsReply {
    pub monitors: Vec<MonitorInfo>,
    pub windows: Vec<CapturableWindow>,
    pub desktop: DesktopStatus,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShotScreenArgs {
    #[serde(default)]
    pub monitor_index: Option<u32>,
    #[serde(default)]
    pub max_width: Option<u32>,
    #[serde(default)]
    pub quality: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShotWindowArgs {
    pub hwnd: u64,
    #[serde(default)]
    pub max_width: Option<u32>,
    #[serde(default)]
    pub quality: Option<u8>,
}

/// 截图坐标变换元数据（任务 3.3：用于把缩略图上的像素点精确还原到远端虚拟桌面真实坐标）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShotTransform {
    pub origin_x: i32,
    pub origin_y: i32,
    pub source_width: u32,
    pub source_height: u32,
    pub image_width: u32,
    pub image_height: u32,
    pub dpi_scale_percent: u32,
    #[serde(default)]
    pub target_hwnd: Option<u64>,
    #[serde(default)]
    pub monitor_index: Option<u32>,
}

impl ShotTransform {
    /// 将截图图片内的像素坐标 `(img_x, img_y)` 映射为远端虚拟桌面的真实屏幕坐标 `(screen_x, screen_y)`。
    pub fn map_image_point(&self, img_x: u32, img_y: u32) -> (i32, i32) {
        let clamped_x = img_x.min(self.image_width.saturating_sub(1));
        let clamped_y = img_y.min(self.image_height.saturating_sub(1));
        let iw = self.image_width.max(1) as i64;
        let ih = self.image_height.max(1) as i64;
        let sx = self.origin_x as i64
            + ((clamped_x as i64 * self.source_width as i64 + iw / 2) / iw);
        let sy = self.origin_y as i64
            + ((clamped_y as i64 * self.source_height as i64 + ih / 2) / ih);
        (sx as i32, sy as i32)
    }
}

/// 截图响应头；其后紧跟 `byte_len` 字节的原始图像字节（JPEG）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShotHeader {
    pub format: String,
    pub byte_len: u32,
    pub transform: ShotTransform,
    pub captured_at_ms: u64,
    pub desktop: DesktopStatus,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseBtn {
    #[default]
    Left,
    Right,
    Middle,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShotRefreshSpec {
    #[serde(default)]
    pub monitor_index: Option<u32>,
    #[serde(default)]
    pub hwnd: Option<u64>,
    #[serde(default)]
    pub max_width: Option<u32>,
    #[serde(default)]
    pub quality: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputClickArgs {
    pub screen_x: i32,
    pub screen_y: i32,
    #[serde(default)]
    pub button: MouseBtn,
    #[serde(default)]
    pub double: bool,
    #[serde(default)]
    pub target_hwnd: Option<u64>,
    #[serde(default)]
    pub refresh_shot: Option<ShotRefreshSpec>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputKeyArgs {
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub key_combo: Option<String>,
    #[serde(default)]
    pub target_hwnd: Option<u64>,
    #[serde(default)]
    pub refresh_shot: Option<ShotRefreshSpec>,
}

/// 点按或按键回复；若 `refreshed_shot` 为 `Some(h)`，同一流上紧跟 `h.byte_len` 字节的更新后截图。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputReply {
    #[serde(default)]
    pub uipi_blocked: bool,
    #[serde(default)]
    pub warning: Option<String>,
    #[serde(default)]
    pub refreshed_shot: Option<ShotHeader>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PowerAction {
    Lock,
    Sleep,
    Restart,
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PowerExecArgs {
    pub action: PowerAction,
    #[serde(default)]
    pub countdown_secs: Option<u32>,
    #[serde(default)]
    pub requester_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PowerExecReply {
    pub action: PowerAction,
    pub cancelled_by_host_user: bool,
    pub executed: bool,
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PowerCancelReply {
    pub cancelled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PowerWakeWolArgs {
    pub mac_addresses: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PowerWakeWolReply {
    pub packets_sent: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClipKind {
    #[default]
    Empty,
    Text,
    ImagePng,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipHeader {
    pub kind: ClipKind,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub image_width: Option<u32>,
    #[serde(default)]
    pub image_height: Option<u32>,
    #[serde(default)]
    pub png_byte_len: u32,
    #[serde(default)]
    pub sequence: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipSetArgs {
    pub kind: ClipKind,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub png_byte_len: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipSetReply {
    pub sequence: u32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TermShell {
    #[default]
    PowerShell,
    Cmd,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TermOpenArgs {
    #[serde(default)]
    pub shell: TermShell,
    pub cols: u16,
    pub rows: u16,
    #[serde(default)]
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TermOpenReply {
    pub shell_name: String,
    pub pid: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum TermClientFrame {
    Input { data_b64: String },
    Resize { cols: u16, rows: u16 },
    Close,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum TermServerFrame {
    Output { data_b64: String },
    Exited { exit_code: Option<i32> },
}
