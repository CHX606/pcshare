//! 界面与核心层之间的桥接实现（阶段 1 + 阶段 2）：
//! - 把 `CoreNode` 的能力包装成与 `app/src/api/tauri.ts` 完全对齐的命令与事件。
//! - 所有命令统一返回 `Reply<T>`（`{ ok: true, data }` 或 `{ ok: false, err: { code, msg } }`）。
//! - 任务进度事件严格限流（运行中每 100 毫秒最多推送一次，暂停与终态立即送达）。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub use crate::bridge_stage3::{
    UiBatchDeviceResult, UiCapturableWindow, UiCapturedShot, UiClipboardPreview, UiEditSyncPrompt,
    UiEditSyncResult, UiFsSearchBatchEvent, UiFsSearchHit, UiInputOutcome, UiLocalEditSession,
    UiMonitorInfo, UiPowerResult, UiQuickHit, UiShotTargets, UiShotTransform, UiSystemNotice,
    UiTermExitEvent, UiTermOutputEvent, UiTermSession, UiWakeResult,
};

use chrono::{Local, TimeZone};
use pcshare_core::error::{map_io_to_rpc, map_platform_to_rpc};
use pcshare_core::path_guard::{normalize_lexical_absolute, to_display_path, to_fs_path};
use pcshare_core::{
    CoreError, CoreEvent, CoreNode, DeviceConnState, PairingDecision, PeerRecord,
    RemoteTerminalHandle, TransferQueueOptions, TransferTaskControl, parse_invite_or_id,
};
use pcshare_proto::{
    AppLaunchArgs, AppLaunchReply, ConflictPolicy, DirEntry, EntryKind, ErrorCode,
    FIRST_LIST_BATCH_SIZE, FsCopyMoveArgs, FsDeleteArgs, FsMkdirArgs, FsRenameArgs, FsRootsReply,
    FsStatArgs, FsTaskKind, FsTaskProgress, FsTaskState, LaunchKind, Permissions, ProcKillArgs,
    RemoteEvent, RootKind, RpcError,
};
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

pub const SELF_ID: &str = "self";
const PROGRESS_THROTTLE: Duration = Duration::from_millis(100);
const ICON_CACHE_MAX_ENTRIES: usize = 512;
const PAIR_REQUEST_TTL: Duration = Duration::from_secs(300);
const EDIT_PROMPT_TTL: Duration = Duration::from_secs(1800);
const EDIT_PROMPT_MAX_ENTRIES: usize = 64;
const STAT_CONCURRENCY: usize = 8;

// ────────────────────────────── 与前端对齐的数据结构 ──────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiErrorCode {
    Unauthorized,
    Forbidden,
    NotFound,
    AlreadyExists,
    Busy,
    Invalid,
    Unsupported,
    Io,
    Cancelled,
    Internal,
    Offline,
    Timeout,
    BadCode,
    Expired,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiErrorBody {
    pub code: UiErrorCode,
    pub msg: String,
}

#[derive(Debug, Clone)]
pub enum Reply<T> {
    Ok { data: T },
    Err { err: ApiErrorBody },
}

impl<T> Reply<T> {
    pub fn ok(data: T) -> Self {
        Self::Ok { data }
    }

    pub fn err(code: UiErrorCode, msg: impl Into<String>) -> Self {
        Self::Err {
            err: ApiErrorBody {
                code,
                msg: msg.into(),
            },
        }
    }

    pub fn from_core(res: Result<T, CoreError>) -> Self {
        match res {
            Ok(data) => Self::Ok { data },
            Err(e) => Self::Err {
                err: map_core_error(&e),
            },
        }
    }

    pub fn from_rpc(res: Result<T, RpcError>) -> Self {
        match res {
            Ok(data) => Self::Ok { data },
            Err(e) => Self::Err {
                err: map_rpc_error(&e),
            },
        }
    }
}

impl<T: Serialize> Serialize for Reply<T> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        match self {
            Self::Ok { data } => {
                let mut st = serializer.serialize_struct("Reply", 2)?;
                st.serialize_field("ok", &true)?;
                st.serialize_field("data", data)?;
                st.end()
            }
            Self::Err { err } => {
                let mut st = serializer.serialize_struct("Reply", 2)?;
                st.serialize_field("ok", &false)?;
                st.serialize_field("err", err)?;
                st.end()
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SelfInfo {
    pub id: String,
    pub name: String,
    pub version: String,
}

fn default_ui_hotkey() -> String {
    "Alt+Space".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiSettings {
    pub device_name: String,
    #[serde(default)]
    pub autostart: bool,
    #[serde(default)]
    pub autostart_elevated: bool,
    #[serde(default)]
    pub autostart_prompted: bool,
    #[serde(default = "default_ui_hotkey")]
    pub quick_hotkey: String,
    #[serde(default)]
    pub clipboard_auto_sync: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiHostStatus {
    pub paused: bool,
    pub working_set_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiPermissions {
    pub read_files: bool,
    pub write_files: bool,
    pub apps: bool,
    pub screenshot: bool,
    pub input: bool,
    pub terminal: bool,
    pub power: bool,
    pub clipboard: bool,
}

impl UiPermissions {
    pub fn default_stage1() -> Self {
        Self {
            read_files: true,
            write_files: true,
            apps: true,
            screenshot: false,
            input: false,
            terminal: false,
            power: false,
            clipboard: false,
        }
    }

    pub fn from_proto(p: &Permissions) -> Self {
        Self {
            read_files: p.fs_read,
            write_files: p.fs_write,
            apps: p.app_launch || p.proc_manage,
            screenshot: p.screenshot,
            input: p.input_control,
            terminal: p.terminal,
            power: p.power,
            clipboard: p.clipboard,
        }
    }

    pub fn to_proto(&self, allowed_roots: Vec<String>) -> Permissions {
        Permissions {
            fs_read: self.read_files,
            fs_write: self.write_files,
            app_launch: self.apps,
            proc_manage: self.apps,
            screenshot: self.screenshot,
            input_control: self.input,
            terminal: self.terminal,
            power: self.power,
            clipboard: self.clipboard,
            allowed_roots,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UiLinkKind {
    Local,
    Direct,
    Relay,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum UiDeviceStatus {
    Offline {
        #[serde(rename = "lastSeen", skip_serializing_if = "Option::is_none")]
        last_seen: Option<u64>,
    },
    Connecting,
    Online {
        link: UiLinkKind,
        #[serde(rename = "rttMs")]
        rtt_ms: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiDevice {
    pub id: String,
    pub name: String,
    pub os: String,
    pub status: UiDeviceStatus,
    pub features: Vec<String>,
    #[serde(default, rename = "hasMac")]
    pub has_mac: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permissions: Option<UiPermissions>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiPairOffer {
    pub code: String,
    pub expires_at: u64,
    pub invite: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiPairRequest {
    pub request_id: String,
    pub device_id: String,
    pub device_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiLanPeer {
    pub endpoint_id: String,
    pub device_name: String,
    pub os: String,
    pub addrs: Vec<String>,
    pub seen_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiFsRoot {
    pub name: String,
    pub path: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub free: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiFsEntry {
    pub name: String,
    pub dir: bool,
    pub size: u64,
    pub mtime: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiFsListing {
    pub path: String,
    pub entries: Vec<UiFsEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiFsCacheHit {
    pub path: String,
    pub entries: Vec<UiFsEntry>,
    pub cached_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiFsListBatchEvent {
    pub device: String,
    pub path: String,
    pub entries: Vec<UiFsEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_hint: Option<usize>,
    pub done: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiFsChangedEvent {
    pub device: String,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiAppInfo {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiDesktopState {
    pub place: String,
    pub elevated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiLaunchResult {
    pub new_windows: Vec<String>,
    pub desktop: UiDesktopState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elevation_prompt: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiProcInfo {
    pub pid: u32,
    pub name: String,
    pub title: String,
    pub memory: u64,
    pub cpu: f32,
    pub critical: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UiTaskKind {
    Upload,
    Download,
    Relay,
    Copy,
    Move,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UiTaskState {
    Running,
    Paused,
    Done,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiTaskProgress {
    pub id: String,
    pub kind: UiTaskKind,
    pub title: String,
    pub src_device: String,
    pub dst_device: String,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub speed: f64,
    pub state: UiTaskState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ApiErrorBody>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UiConflictPolicy {
    Overwrite,
    Skip,
    Rename,
}

impl UiConflictPolicy {
    fn to_proto(self) -> ConflictPolicy {
        match self {
            Self::Overwrite => ConflictPolicy::Overwrite,
            Self::Skip => ConflictPolicy::Skip,
            Self::Rename => ConflictPolicy::RenameKeepBoth,
        }
    }
}

// ────────────────────────────── 诊断与网络体检结构 ──────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiDiagPeerRoute {
    pub peer_id: String,
    pub name: String,
    pub state: String,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rtt_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiDiagSummary {
    pub app_version: String,
    pub endpoint_id: String,
    pub device_name: String,
    pub os: String,
    pub working_set_bytes: u64,
    pub host_paused: bool,
    pub speed_limit_bps: Option<u64>,
    pub netcheck_targets: Vec<String>,
    pub peer_routes: Vec<UiDiagPeerRoute>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiRelayLatencyItem {
    pub url: String,
    pub rtt_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiNetcheckReport {
    pub checked_at_ms: u64,
    pub udp_ipv4: bool,
    pub udp_ipv6: bool,
    pub mapping_varies_by_dest_ip: Option<bool>,
    pub upnp_available: Option<bool>,
    pub pcp_available: Option<bool>,
    pub nat_pmp_available: Option<bool>,
    pub preferred_relay: Option<String>,
    pub relay_latencies: Vec<UiRelayLatencyItem>,
    pub summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiAuditEntry {
    pub ts: String,
    pub peer_id: String,
    pub peer_name: String,
    pub cmd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub ok: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiDiagLogs {
    pub runtime_logs: Vec<String>,
    pub audits: Vec<UiAuditEntry>,
}

pub trait EventSink: Send + Sync + 'static {
    fn emit_device_status(&self, device: UiDevice);
    fn emit_device_removed(&self, id: String);
    fn emit_pair_request(&self, req: UiPairRequest);
    fn emit_task_progress(&self, progress: UiTaskProgress);
    fn emit_fs_list_batch(&self, _batch: UiFsListBatchEvent) {}
    fn emit_fs_changed(&self, _ev: UiFsChangedEvent) {}
    fn emit_lan_changed(&self, _peers: Vec<UiLanPeer>) {}
    fn emit_host_paused(&self, _paused: bool) {}
    fn emit_launcher_toggle(&self) {}
    fn emit_fs_search_batch(&self, _batch: UiFsSearchBatchEvent) {}
    fn emit_term_output(&self, _ev: UiTermOutputEvent) {}
    fn emit_term_exit(&self, _ev: UiTermExitEvent) {}
    fn emit_edit_sync_prompt(&self, _ev: UiEditSyncPrompt) {}
    fn emit_batch_item(&self, _ev: UiBatchDeviceResult) {}
    fn emit_system_notice(&self, _ev: UiSystemNotice) {}
}

#[derive(Clone)]
struct TransferMeta {
    seq: u64,
    kind: UiTaskKind,
    title: String,
    src_device: String,
    dst_device: String,
    bytes_done: Arc<AtomicU64>,
    bytes_total: Arc<AtomicU64>,
    speed_bits: Arc<AtomicU64>,
}

#[derive(Clone)]
struct FsTaskMeta {
    seq: u64,
    kind: UiTaskKind,
    title: String,
    device_id: String,
    bytes_done: Arc<AtomicU64>,
    bytes_total: Arc<AtomicU64>,
    speed_bits: Arc<AtomicU64>,
}

enum CancellableTask {
    Transfer {
        control: TransferTaskControl,
        meta: TransferMeta,
    },
    LocalFsTask {
        raw_task_id: String,
        meta: FsTaskMeta,
    },
    RemoteFsTask {
        peer_id: String,
        remote_task_id: String,
        meta: FsTaskMeta,
    },
}

/// 带上限（LRU 近似）的软件图标缓存：仅缓存成功的 data URL，不缓存瞬态 `None`。
struct IconCache {
    max_entries: usize,
    seq: u64,
    entries: HashMap<String, (String, u64)>,
}

impl IconCache {
    fn new(max_entries: usize) -> Self {
        Self {
            max_entries: max_entries.max(1),
            seq: 0,
            entries: HashMap::new(),
        }
    }

    fn get(&mut self, key: &str) -> Option<String> {
        let next_seq = self.seq.wrapping_add(1);
        if let Some((val, last_used)) = self.entries.get_mut(key) {
            self.seq = next_seq;
            *last_used = next_seq;
            Some(val.clone())
        } else {
            None
        }
    }

    fn insert(&mut self, key: String, value: String) {
        use std::collections::hash_map::Entry;
        self.seq = self.seq.wrapping_add(1);
        let stamp = self.seq;
        let is_full = self.entries.len() >= self.max_entries;
        let key = match self.entries.entry(key) {
            Entry::Occupied(mut occ) => {
                occ.insert((value, stamp));
                return;
            }
            Entry::Vacant(vac) => {
                if !is_full {
                    vac.insert((value, stamp));
                    return;
                }
                vac.into_key()
            }
        };
        if let Some(oldest_key) = self
            .entries
            .iter()
            .min_by_key(|(_, (_, ts))| *ts)
            .map(|(k, _)| k.clone())
        {
            self.entries.remove(&oldest_key);
        }
        self.entries.insert(key, (value, stamp));
    }

    fn remove_device(&mut self, device: &str) {
        let prefix = format!("{device}|");
        self.entries.retain(|k, _| !k.starts_with(&prefix));
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }
}

pub struct BridgeState {
    pub(crate) node: Arc<CoreNode>,
    pub(crate) sink: Arc<dyn EventSink>,
    task_seq: AtomicU64,
    active_tasks: Mutex<HashMap<String, CancellableTask>>,
    pub(crate) active_searches: Mutex<HashMap<String, CancellationToken>>,
    pub(crate) active_terminals: Mutex<HashMap<String, Arc<RemoteTerminalHandle>>>,
    active_watches: Mutex<HashSet<(String, String)>>,
    pending_pair_requests: Mutex<Vec<(UiPairRequest, Instant)>>,
    pending_edit_prompts: Mutex<Vec<(UiEditSyncPrompt, Instant)>>,
    pub(crate) pending_launcher_open: AtomicBool,
    hotkey_trigger: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    last_online_map: Mutex<HashMap<String, bool>>,
    icon_cache: Mutex<IconCache>,
    pub(crate) shutdown: CancellationToken,
}

impl BridgeState {
    pub fn new(node: Arc<CoreNode>, sink: Arc<dyn EventSink>) -> Arc<Self> {
        let data_dir = node.storage().data_dir().to_path_buf();
        let state = Arc::new(Self {
            node,
            sink,
            task_seq: AtomicU64::new(1),
            active_tasks: Mutex::new(HashMap::new()),
            active_searches: Mutex::new(HashMap::new()),
            active_terminals: Mutex::new(HashMap::new()),
            active_watches: Mutex::new(HashSet::new()),
            pending_pair_requests: Mutex::new(Vec::new()),
            pending_edit_prompts: Mutex::new(Vec::new()),
            pending_launcher_open: AtomicBool::new(false),
            hotkey_trigger: Mutex::new(None),
            last_online_map: Mutex::new(HashMap::new()),
            icon_cache: Mutex::new(IconCache::new(ICON_CACHE_MAX_ENTRIES)),
            shutdown: CancellationToken::new(),
        });
        state.spawn_core_event_forwarder();
        // 启动时在阻塞线程池清理上次异常退出遗留的拖出临时目录
        tokio::task::spawn_blocking(move || {
            crate::bridge_stage3::cleanup_drag_out_staging(&data_dir, None);
        });
        // Warm local app list in background so Alt+Space quick search includes本机 apps immediately
        let node_bg = state.node.clone();
        tokio::spawn(async move {
            let _ = node_bg.local_apps(false).await;
        });
        state
    }

    pub fn bind_hotkey_handler(
        &self,
        on_trigger: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<(), String> {
        *self
            .hotkey_trigger
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(on_trigger.clone());
        let hk = self.node.settings().quick_hotkey;
        let opt = Some(hk.as_str()).filter(|s| !s.trim().is_empty());
        self.node
            .platform()
            .set_global_hotkey(opt, on_trigger)
            .map_err(|e| e.to_string())
    }

    fn restore_global_hotkey(&self, prev_hotkey: &str) {
        if let Some(pcb) = self
            .hotkey_trigger
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            let prev_opt = Some(prev_hotkey).filter(|s| !s.trim().is_empty());
            let _ = self.node.platform().set_global_hotkey(prev_opt, pcb);
        }
    }

    pub(crate) fn next_seq(&self) -> u64 {
        self.task_seq.fetch_add(1, Ordering::Relaxed)
    }

    pub fn node(&self) -> &Arc<CoreNode> {
        &self.node
    }

    pub fn self_id(&self) -> String {
        self.node.endpoint_id().to_string()
    }

    /// 主窗口销毁（关闭到托盘）时清理窗口级会话：活跃终端、文件搜索与目录监听，
    /// 保留后台传输任务、被控服务与全局热键。
    pub async fn on_main_window_destroyed(self: &Arc<Self>) {
        self.pending_launcher_open.store(false, Ordering::SeqCst);

        let searches: Vec<CancellationToken> = self
            .active_searches
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain()
            .map(|(_, tok)| tok)
            .collect();
        for tok in searches {
            tok.cancel();
        }

        let terms: Vec<Arc<RemoteTerminalHandle>> = self
            .active_terminals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain()
            .map(|(_, h)| h)
            .collect();
        for t in terms {
            let _ = t.close().await;
        }

        let watches: Vec<(String, String)> = self
            .active_watches
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain()
            .collect();
        self.node.fs_watch().unwatch_all_for_peer(SELF_ID);
        for (device, path) in watches {
            if device != SELF_ID {
                let _ = tokio::time::timeout(
                    Duration::from_secs(2),
                    self.node.remote_unwatch(&device, &path),
                )
                .await;
            }
        }
    }

    pub async fn shutdown(&self) {
        self.shutdown.cancel();
        let _ = self.node.platform().set_global_hotkey(None, Arc::new(|| {}));

        let searches: Vec<CancellationToken> = self
            .active_searches
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain()
            .map(|(_, tok)| tok)
            .collect();
        for tok in searches {
            tok.cancel();
        }

        let tasks: Vec<CancellableTask> = self
            .active_tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain()
            .map(|(_, t)| t)
            .collect();
        for task in tasks {
            match task {
                CancellableTask::Transfer { control, .. } => {
                    control.cancel();
                }
                CancellableTask::LocalFsTask { raw_task_id, .. } => {
                    let _ = self.node.fs_service().task_cancel(&raw_task_id);
                }
                CancellableTask::RemoteFsTask {
                    peer_id,
                    remote_task_id,
                    ..
                } => {
                    let _ = tokio::time::timeout(
                        Duration::from_millis(500),
                        self.node.remote_task_cancel(&peer_id, &remote_task_id),
                    )
                    .await;
                }
            }
        }

        let terms: Vec<Arc<RemoteTerminalHandle>> = self
            .active_terminals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain()
            .map(|(_, h)| h)
            .collect();
        for t in terms {
            let _ = t.close().await;
        }

        self.active_watches
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();

        let data_dir = self.node.storage().data_dir().to_path_buf();
        let _ = tokio::task::spawn_blocking(move || {
            crate::bridge_stage3::cleanup_drag_out_staging(&data_dir, None);
        })
        .await;

        self.node.shutdown().await;
    }

    pub(crate) fn record_pending_pair_request(&self, req: UiPairRequest) {
        let now = Instant::now();
        let mut guard = self
            .pending_pair_requests
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        guard.retain(|(_, ts)| now.duration_since(*ts) < PAIR_REQUEST_TTL);
        if let Some((existing, ts)) = guard
            .iter_mut()
            .find(|(r, _)| r.request_id == req.request_id)
        {
            *existing = req;
            *ts = now;
        } else {
            guard.push((req, now));
        }
    }

    pub(crate) fn remove_pending_pair_request(&self, request_id: &str) {
        self.pending_pair_requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(r, _)| r.request_id != request_id);
    }

    pub fn pair_pending_list(&self) -> Reply<Vec<UiPairRequest>> {
        let now = Instant::now();
        let mut guard = self
            .pending_pair_requests
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        guard.retain(|(_, ts)| now.duration_since(*ts) < PAIR_REQUEST_TTL);
        Reply::ok(guard.iter().map(|(r, _)| r.clone()).collect())
    }

    pub(crate) fn record_pending_edit_prompt(&self, ev: UiEditSyncPrompt) {
        let now = Instant::now();
        let mut guard = self
            .pending_edit_prompts
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        guard.retain(|(_, ts)| now.duration_since(*ts) < EDIT_PROMPT_TTL);
        if let Some((existing, ts)) = guard
            .iter_mut()
            .find(|(p, _)| p.session_id == ev.session_id)
        {
            *existing = ev;
            *ts = now;
        } else {
            if guard.len() >= EDIT_PROMPT_MAX_ENTRIES {
                guard.remove(0);
            }
            guard.push((ev, now));
        }
    }

    pub(crate) fn remove_pending_edit_prompt(&self, session_id: &str) {
        self.pending_edit_prompts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(p, _)| p.session_id != session_id);
    }

    pub(crate) fn mark_pending_edit_conflict(&self, session_id: &str) {
        let now = Instant::now();
        let mut guard = self
            .pending_edit_prompts
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some((existing, ts)) = guard
            .iter_mut()
            .find(|(p, _)| p.session_id == session_id)
        {
            existing.conflict = true;
            *ts = now;
        }
    }

    pub fn edit_pending_list(&self) -> Reply<Vec<UiEditSyncPrompt>> {
        let now = Instant::now();
        let mut guard = self
            .pending_edit_prompts
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        guard.retain(|(_, ts)| now.duration_since(*ts) < EDIT_PROMPT_TTL);
        Reply::ok(guard.iter().map(|(p, _)| p.clone()).collect())
    }

    fn spawn_core_event_forwarder(self: &Arc<Self>) {
        let mut rx = self.node.subscribe_events();
        let weak = Arc::downgrade(self);
        let cancel = self.shutdown.clone();
        tokio::spawn(async move {
            loop {
                let ev = tokio::select! {
                    _ = cancel.cancelled() => break,
                    res = rx.recv() => match res {
                        Ok(ev) => ev,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                };
                let Some(bridge) = weak.upgrade() else {
                    break;
                };
                match ev {
                    CoreEvent::DeviceStateChanged { peer_id, state } => {
                        if let Some(peer) = bridge.node.storage().get_peer(&peer_id) {
                            let is_now_online = matches!(
                                state,
                                DeviceConnState::OnlineDirect { .. }
                                    | DeviceConnState::OnlineRelay { .. }
                            );
                            let is_now_offline = matches!(state, DeviceConnState::Offline { .. });
                            if is_now_online || is_now_offline {
                                let prev_opt = bridge
                                    .last_online_map
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .insert(peer_id.clone(), is_now_online);
                                if prev_opt == Some(false) && is_now_online {
                                    bridge.sink.emit_system_notice(UiSystemNotice {
                                        title: "设备已上线".to_string(),
                                        body: format!("「{}」已连接，现在可以随时互传与操作", peer.name),
                                        kind: "info".to_string(),
                                    });
                                } else if prev_opt == Some(true) && is_now_offline {
                                    bridge.sink.emit_system_notice(UiSystemNotice {
                                        title: "设备已离线".to_string(),
                                        body: format!("「{}」当前连接已断开", peer.name),
                                        kind: "warn".to_string(),
                                    });
                                }
                            }
                            let ui_dev = to_ui_device(&peer, Some(&state));
                            bridge.sink.emit_device_status(ui_dev);
                        }
                    }
                    CoreEvent::PeersUpdated => {
                        for peer in bridge.node.list_peers() {
                            let state = bridge.node.peer_state(&peer.endpoint_id);
                            bridge
                                .sink
                                .emit_device_status(to_ui_device(&peer, state.as_ref()));
                        }
                        bridge
                            .sink
                            .emit_lan_changed(bridge.compute_unpaired_lan_peers());
                    }
                    CoreEvent::PairingApprovalNeeded(prompt) => {
                        let req = UiPairRequest {
                            request_id: prompt.request_id,
                            device_id: prompt.peer_id,
                            device_name: prompt.peer_name.clone(),
                        };
                        bridge.record_pending_pair_request(req.clone());
                        bridge.sink.emit_system_notice(UiSystemNotice {
                            title: "收到配对请求".to_string(),
                            body: format!("「{}」请求与这台电脑配对，请确认或拒绝", prompt.peer_name),
                            kind: "info".to_string(),
                        });
                        bridge.sink.emit_pair_request(req);
                    }
                    CoreEvent::EditSyncPromptNeeded(p) => {
                        let ev = UiEditSyncPrompt {
                            session_id: p.session_id,
                            peer_id: p.peer_id,
                            peer_name: p.peer_name,
                            remote_path: p.remote_path,
                            local_path: p.local_path,
                            file_name: p.file_name,
                            conflict: p.conflict,
                        };
                        bridge.record_pending_edit_prompt(ev.clone());
                        bridge.sink.emit_edit_sync_prompt(ev);
                    }
                    CoreEvent::RemoteDirChanged { peer_id, path } => {
                        bridge.node.dir_cache().invalidate(&peer_id, &path);
                        bridge.sink.emit_fs_changed(UiFsChangedEvent {
                            device: peer_id,
                            path,
                        });
                    }
                    CoreEvent::LanPeersChanged => {
                        bridge
                            .sink
                            .emit_lan_changed(bridge.compute_unpaired_lan_peers());
                    }
                    CoreEvent::HostPausedChanged { paused } => {
                        bridge.sink.emit_host_paused(paused);
                    }
                    _ => {}
                }
            }
        });
    }

    fn compute_unpaired_lan_peers(&self) -> Vec<UiLanPeer> {
        let paired_ids: HashSet<String> = self
            .node
            .list_peers()
            .into_iter()
            .map(|p| p.endpoint_id)
            .collect();
        self.node
            .discovered_lan_peers()
            .into_iter()
            .filter(|ann| !paired_ids.contains(&ann.endpoint_id))
            .map(|ann| UiLanPeer {
                endpoint_id: ann.endpoint_id,
                device_name: ann.device_name,
                os: ann.os,
                addrs: vec![ann.addr.to_string()],
                seen_at_ms: ann.discovered_at_ms,
            })
            .collect()
    }

    // ────────────────────────── 本机、后台常驻与设置 ──────────────────────────

    pub fn self_info(&self) -> Reply<SelfInfo> {
        Reply::ok(SelfInfo {
            id: self.node.endpoint_id().to_string(),
            name: self.node.settings().device_name,
            version: env!("CARGO_PKG_VERSION").to_string(),
        })
    }

    pub fn settings_get(&self) -> Reply<UiSettings> {
        let s = self.node.settings();
        let (autostart, autostart_elevated) = match self.node.platform().get_autostart() {
            Ok(os) => (os.enabled && !os.elevated, os.enabled && os.elevated),
            Err(_) => (s.autostart, s.autostart_elevated),
        };
        Reply::ok(UiSettings {
            device_name: s.device_name,
            autostart,
            autostart_elevated,
            autostart_prompted: s.autostart_prompted,
            quick_hotkey: s.quick_hotkey,
            clipboard_auto_sync: s.clipboard_auto_sync,
        })
    }

    pub fn settings_set(&self, next: UiSettings) -> Reply<()> {
        let trimmed = next.device_name.trim().to_string();
        if trimmed.is_empty() {
            return Reply::err(UiErrorCode::Invalid, "本机名称不能为空");
        }
        let prev = self.node.settings();
        let next_hotkey = next.quick_hotkey.trim().to_string();
        let mut hotkey_changed = false;
        if next_hotkey != prev.quick_hotkey
            && let Some(cb) = self
                .hotkey_trigger
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        {
            let opt = Some(next_hotkey.as_str()).filter(|s| !s.is_empty());
            if let Err(pe) = self.node.platform().set_global_hotkey(opt, cb) {
                self.restore_global_hotkey(&prev.quick_hotkey);
                return Reply::from_rpc(Err(map_platform_to_rpc(&pe)));
            }
            hotkey_changed = true;
        }
        let (cur_enabled, cur_elevated) = match self.node.platform().get_autostart() {
            Ok(a) => (a.enabled, a.elevated),
            Err(_) => (
                prev.autostart || prev.autostart_elevated,
                prev.autostart_elevated,
            ),
        };

        let want_elevated = next.autostart_elevated;
        let want_enabled = next.autostart_elevated || next.autostart;
        let mut autostart_prev: Option<(PathBuf, bool, bool)> = None;

        if want_enabled != cur_enabled || (want_enabled && want_elevated != cur_elevated) {
            let exe = match std::env::current_exe() {
                Ok(p) => p,
                Err(e) => {
                    if hotkey_changed {
                        self.restore_global_hotkey(&prev.quick_hotkey);
                    }
                    return Reply::err(UiErrorCode::Io, format!("无法定位当前程序路径：{e}"));
                }
            };
            if let Err(pe) = self
                .node
                .platform()
                .set_autostart(&exe, want_enabled, want_elevated)
            {
                if hotkey_changed {
                    self.restore_global_hotkey(&prev.quick_hotkey);
                }
                return Reply::from_rpc(Err(map_platform_to_rpc(&pe)));
            }
            autostart_prev = Some((exe, cur_enabled, cur_elevated));
        }

        let res = self.node.update_settings(|s| {
            s.device_name = trimmed;
            s.autostart = want_enabled && !want_elevated;
            s.autostart_elevated = want_elevated;
            s.autostart_prompted = next.autostart_prompted || s.autostart_prompted;
            s.quick_hotkey = next_hotkey;
            s.clipboard_auto_sync = next.clipboard_auto_sync;
        });
        if let Err(err) = res {
            if let Some((exe, prev_enabled, prev_elevated)) = autostart_prev {
                let _ = self
                    .node
                    .platform()
                    .set_autostart(&exe, prev_enabled, prev_elevated);
            }
            if hotkey_changed {
                self.restore_global_hotkey(&prev.quick_hotkey);
            }
            return Reply::from_core(Err(err));
        }
        Reply::ok(())
    }

    pub fn host_status(&self) -> Reply<UiHostStatus> {
        Reply::ok(UiHostStatus {
            paused: self.node.is_host_paused(),
            working_set_bytes: self.node.platform().current_process_working_set(),
        })
    }

    pub fn host_set_paused(&self, paused: bool) -> Reply<()> {
        self.node.set_host_paused(paused);
        Reply::ok(())
    }

    // ────────────────────────── 设备管理与局域网发现 ──────────────────────────

    pub fn device_list(&self) -> Reply<Vec<UiDevice>> {
        let states = self.node.all_peer_states();
        let list = self
            .node
            .list_peers()
            .into_iter()
            .map(|p| {
                let st = states.get(&p.endpoint_id);
                to_ui_device(&p, st)
            })
            .collect();
        Reply::ok(list)
    }

    pub fn lan_list(&self) -> Reply<Vec<UiLanPeer>> {
        Reply::ok(self.compute_unpaired_lan_peers())
    }

    pub fn device_rename(&self, id: String, name: String) -> Reply<()> {
        let trimmed = name.trim().to_string();
        if trimmed.is_empty() {
            return Reply::err(UiErrorCode::Invalid, "设备名称不能为空");
        }
        match self.node.rename_peer(&id, trimmed) {
            Ok(true) => Reply::ok(()),
            Ok(false) => Reply::err(UiErrorCode::NotFound, "找不到这台设备"),
            Err(e) => Reply::from_core(Err(e)),
        }
    }

    pub fn device_remove(&self, id: String) -> Reply<()> {
        match self.node.remove_peer(&id) {
            Ok(_) => {
                self.icon_cache
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove_device(&id);
                self.last_online_map
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
                self.active_watches
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .retain(|(dev, _)| dev != &id);
                self.pending_pair_requests
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .retain(|(r, _)| r.device_id != id);
                self.pending_edit_prompts
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .retain(|(p, _)| p.peer_id != id);
                self.sink.emit_device_removed(id);
                Reply::ok(())
            }
            Err(e) => Reply::from_core(Err(e)),
        }
    }

    pub fn device_set_permissions(&self, id: String, permissions: UiPermissions) -> Reply<()> {
        let Some(existing) = self.node.storage().get_peer(&id) else {
            return Reply::err(UiErrorCode::NotFound, "找不到这台设备");
        };
        let proto_perms = permissions.to_proto(existing.permissions.allowed_roots);
        match self.node.update_peer_permissions(&id, proto_perms) {
            Ok(true) => Reply::ok(()),
            Ok(false) => Reply::err(UiErrorCode::NotFound, "找不到这台设备"),
            Err(e) => Reply::from_core(Err(e)),
        }
    }

    pub async fn device_reconnect(self: &Arc<Self>, id: String) -> Reply<()> {
        Reply::from_core(self.node.reconnect_peer(&id).await)
    }

    // ────────────────────────── 配对 ──────────────────────────

    pub fn pair_begin(&self) -> Reply<UiPairOffer> {
        let ticket = self.node.start_pairing_session();
        Reply::ok(UiPairOffer {
            code: ticket.code,
            expires_at: ticket.expires_at_ms,
            invite: ticket.invite_code,
        })
    }

    pub fn pair_cancel(&self) -> Reply<()> {
        self.node.cancel_pairing_session();
        self.pending_pair_requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        Reply::ok(())
    }

    pub async fn pair_connect(self: &Arc<Self>, target: String, code: String) -> Reply<UiDevice> {
        let mut parsed = match parse_invite_or_id(&target) {
            Ok(p) => p,
            Err(e) => return Reply::from_core(Err(e)),
        };
        let final_code = parsed
            .code
            .filter(|c| !c.trim().is_empty())
            .unwrap_or_else(|| code.trim().to_string());
        if final_code.len() != 6 || !final_code.chars().all(|c| c.is_ascii_digit()) {
            return Reply::err(UiErrorCode::Invalid, "配对码是 6 位数字");
        }
        if parsed.endpoint_id == self.node.endpoint_id() {
            return Reply::err(UiErrorCode::Invalid, "不能和本机自己配对");
        }

        // 若该身份码刚在局域网自动发现里出现过，补齐其直连地址实现毫秒级同网配对
        if parsed.direct_addrs.is_empty() {
            let target_str = parsed.endpoint_id.to_string();
            for ann in self.node.discovered_lan_peers() {
                if ann.endpoint_id == target_str {
                    parsed.direct_addrs.push(ann.addr);
                }
            }
        }

        let res = self
            .node
            .pair_with_peer(
                parsed.endpoint_id,
                parsed.direct_addrs,
                parsed.relay_url,
                &final_code,
                true,
                Permissions::default(),
            )
            .await;

        match res {
            Ok(record) => {
                let st = self.node.peer_state(&record.endpoint_id);
                let ui_dev = to_ui_device(&record, st.as_ref());
                self.sink.emit_device_status(ui_dev.clone());
                self.sink.emit_lan_changed(self.compute_unpaired_lan_peers());
                Reply::ok(ui_dev)
            }
            Err(e) => Reply::from_core(Err(e)),
        }
    }

    pub fn pair_respond(
        &self,
        request_id: String,
        allow: bool,
        permissions: UiPermissions,
    ) -> Reply<()> {
        let decision = if allow {
            PairingDecision::Approve {
                granted_permissions: permissions.to_proto(Vec::new()),
                accept_mutual: true,
            }
        } else {
            PairingDecision::Reject
        };
        let res = self.node.respond_pairing(&request_id, decision);
        match &res {
            Ok(()) => self.remove_pending_pair_request(&request_id),
            Err(CoreError::Rpc(rpc))
                if matches!(rpc.code, ErrorCode::NotFound | ErrorCode::Unauthorized) =>
            {
                self.remove_pending_pair_request(&request_id);
            }
            _ => {}
        }
        Reply::from_core(res)
    }

    // ────────────────────────── 文件缓存、监听、渐进浏览与操作 ──────────────────────────

    pub async fn fs_roots(self: &Arc<Self>, device: String) -> Reply<Vec<UiFsRoot>> {
        let reply_res: Result<FsRootsReply, CoreError> = if device == SELF_ID {
            self.node
                .fs_service()
                .roots(&Permissions::default())
                .map_err(CoreError::Rpc)
        } else {
            self.node.remote_roots(&device).await
        };

        Reply::from_core(reply_res.map(convert_roots))
    }

    pub fn fs_cache_get(&self, device: String, path: String) -> Reply<Option<UiFsCacheHit>> {
        let hit = self.node.dir_cache().get(&device, &path).map(|c| UiFsCacheHit {
            path: c.path,
            entries: c.entries.into_iter().map(convert_entry).collect(),
            cached_at: c.cached_at_ms,
        });
        Reply::ok(hit)
    }

    pub async fn fs_list(self: &Arc<Self>, device: String, path: String) -> Reply<UiFsListing> {
        if device == SELF_ID {
            match self
                .node
                .fs_service()
                .list_local(&Permissions::default(), &path)
                .await
            {
                Ok(batch) => {
                    self.node
                        .dir_cache()
                        .put_silent(SELF_ID, &batch.path, batch.entries.clone());
                    let self_name = self.node.settings().device_name;
                    self.node.record_recent_folder(SELF_ID, &self_name, &batch.path);
                    let total = batch.entries.len();
                    let ui_entries: Vec<UiFsEntry> =
                        batch.entries.into_iter().map(convert_entry).collect();
                    if total > FIRST_LIST_BATCH_SIZE {
                        self.sink.emit_fs_list_batch(UiFsListBatchEvent {
                            device: SELF_ID.to_string(),
                            path: batch.path.clone(),
                            entries: ui_entries[..FIRST_LIST_BATCH_SIZE].to_vec(),
                            total_hint: Some(total),
                            done: false,
                        });
                    }
                    Reply::ok(UiFsListing {
                        path: batch.path,
                        entries: ui_entries,
                    })
                }
                Err(e) => Reply::from_rpc(Err(e)),
            }
        } else {
            let mut canonical_path = path.clone();
            let sink = self.sink.clone();
            let dev_clone = device.clone();
            let res = self
                .node
                .remote_list_progressive(&device, &path, |batch| {
                    canonical_path = batch.path.clone();
                    if !batch.done {
                        sink.emit_fs_list_batch(UiFsListBatchEvent {
                            device: dev_clone.clone(),
                            path: batch.path,
                            entries: batch.entries.into_iter().map(convert_entry).collect(),
                            total_hint: batch.total_hint,
                            done: false,
                        });
                    }
                })
                .await;
            Reply::from_core(res.map(|entries| {
                let peer_name = self
                    .node
                    .storage()
                    .get_peer(&device)
                    .map(|p| p.name)
                    .unwrap_or_else(|| "远端电脑".to_string());
                self.node
                    .record_recent_folder(&device, &peer_name, &canonical_path);
                UiFsListing {
                    path: canonical_path,
                    entries: entries.into_iter().map(convert_entry).collect(),
                }
            }))
        }
    }

    pub async fn fs_watch(self: &Arc<Self>, device: String, path: String) -> Reply<usize> {
        let res = if device == SELF_ID {
            self.node
                .fs_watch()
                .watch(SELF_ID, &Permissions::default(), &path)
                .map(|r| r.watched_count)
                .map_err(CoreError::Rpc)
        } else {
            self.node
                .remote_watch(&device, &path)
                .await
                .map(|r| r.watched_count)
        };
        if res.is_ok() {
            self.active_watches
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert((device, path));
        }
        Reply::from_core(res)
    }

    pub async fn fs_unwatch(self: &Arc<Self>, device: String, path: String) -> Reply<usize> {
        let res = if device == SELF_ID {
            self.node
                .fs_watch()
                .unwatch(SELF_ID, &Permissions::default(), &path)
                .map(|r| r.watched_count)
                .map_err(CoreError::Rpc)
        } else {
            self.node
                .remote_unwatch(&device, &path)
                .await
                .map(|r| r.watched_count)
        };
        if res.is_ok() {
            let lower = path.to_lowercase();
            self.active_watches
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|(d, p)| !(d == &device && (p == &path || p.to_lowercase() == lower)));
        }
        Reply::from_core(res)
    }

    pub async fn fs_check_conflicts(
        self: &Arc<Self>,
        device: String,
        dest_dir: String,
        names: Vec<String>,
    ) -> Reply<Vec<String>> {
        if names.is_empty() {
            return Reply::ok(Vec::new());
        }
        let entries = match self.node.fs_list_readonly(&device, &dest_dir).await {
            Ok(entries) => entries,
            Err(e) => return Reply::from_core(Err(e)),
        };
        let existing: HashSet<String> = entries
            .into_iter()
            .map(|e| e.name.to_lowercase())
            .collect();
        let conflicts = names
            .into_iter()
            .filter(|n| existing.contains(&n.to_lowercase()))
            .collect();
        Reply::ok(conflicts)
    }

    pub async fn fs_mkdir(self: &Arc<Self>, device: String, path: String) -> Reply<()> {
        let args = FsMkdirArgs {
            path,
            parents: true,
        };
        if device == SELF_ID {
            Reply::from_rpc(
                self.node
                    .fs_service()
                    .mkdir(&Permissions::default(), args)
                    .await
                    .map(|_| ()),
            )
        } else {
            Reply::from_core(self.node.remote_mkdir(&device, args).await.map(|_| ()))
        }
    }

    pub async fn fs_rename(
        self: &Arc<Self>,
        device: String,
        path: String,
        new_name: String,
    ) -> Reply<()> {
        let args = FsRenameArgs { path, new_name };
        if device == SELF_ID {
            Reply::from_rpc(
                self.node
                    .fs_service()
                    .rename(&Permissions::default(), args)
                    .await
                    .map(|_| ()),
            )
        } else {
            Reply::from_core(self.node.remote_rename(&device, args).await.map(|_| ()))
        }
    }

    pub async fn fs_copy(
        self: &Arc<Self>,
        device: String,
        sources: Vec<String>,
        dest_dir: String,
    ) -> Reply<String> {
        self.start_device_copy_or_move(device, sources, dest_dir, false)
            .await
    }

    pub async fn fs_move(
        self: &Arc<Self>,
        device: String,
        sources: Vec<String>,
        dest_dir: String,
    ) -> Reply<String> {
        self.start_device_copy_or_move(device, sources, dest_dir, true)
            .await
    }

    async fn start_device_copy_or_move(
        self: &Arc<Self>,
        device: String,
        sources: Vec<String>,
        dest_dir: String,
        is_move: bool,
    ) -> Reply<String> {
        if sources.is_empty() {
            return Reply::err(UiErrorCode::Invalid, "请先选择要操作的文件或文件夹");
        }
        let kind = if is_move {
            UiTaskKind::Move
        } else {
            UiTaskKind::Copy
        };
        let title = build_copy_move_title(&sources, &dest_dir, is_move);
        let args = FsCopyMoveArgs {
            sources,
            dest_dir,
            conflict: ConflictPolicy::RenameKeepBoth,
        };

        if device == SELF_ID {
            let fs_svc = self.node.fs_service().clone();
            let fs_kind = if is_move {
                FsTaskKind::Move
            } else {
                FsTaskKind::Copy
            };
            let task_reply =
                match fs_svc.start_copy_or_move(SELF_ID, fs_kind, &Permissions::default(), args) {
                    Ok(r) => r,
                    Err(e) => return Reply::from_rpc(Err(e)),
                };
            let (seq, ui_task_id) = self.next_task_id();
            let meta = FsTaskMeta {
                seq,
                kind,
                title: title.clone(),
                device_id: SELF_ID.to_string(),
                bytes_done: Arc::new(AtomicU64::new(0)),
                bytes_total: Arc::new(AtomicU64::new(1)),
                speed_bits: Arc::new(AtomicU64::new(0)),
            };
            self.active_tasks
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(
                    ui_task_id.clone(),
                    CancellableTask::LocalFsTask {
                        raw_task_id: task_reply.task_id.clone(),
                        meta: meta.clone(),
                    },
                );

            let bridge = self.clone();
            let tid = ui_task_id.clone();
            let raw_tid = task_reply.task_id;
            tokio::spawn(async move {
                bridge.poll_local_fs_task(tid, raw_tid, meta).await;
            });
            Reply::ok(ui_task_id)
        } else {
            let start_res = if is_move {
                self.node.remote_move(&device, args).await
            } else {
                self.node.remote_copy(&device, args).await
            };
            let task_reply = match start_res {
                Ok(r) => r,
                Err(e) => return Reply::from_core(Err(e)),
            };
            let (seq, ui_task_id) = self.next_task_id();
            let meta = FsTaskMeta {
                seq,
                kind,
                title: title.clone(),
                device_id: device.clone(),
                bytes_done: Arc::new(AtomicU64::new(0)),
                bytes_total: Arc::new(AtomicU64::new(1)),
                speed_bits: Arc::new(AtomicU64::new(0)),
            };
            self.active_tasks
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(
                    ui_task_id.clone(),
                    CancellableTask::RemoteFsTask {
                        peer_id: device.clone(),
                        remote_task_id: task_reply.task_id.clone(),
                        meta: meta.clone(),
                    },
                );

            let bridge = self.clone();
            let tid = ui_task_id.clone();
            let raw_tid = task_reply.task_id;
            tokio::spawn(async move {
                bridge
                    .poll_remote_fs_task(tid, device, raw_tid, meta)
                    .await;
            });
            Reply::ok(ui_task_id)
        }
    }

    pub async fn fs_delete(
        self: &Arc<Self>,
        device: String,
        paths: Vec<String>,
        permanent: bool,
    ) -> Reply<()> {
        let args = FsDeleteArgs { paths, permanent };
        if device == SELF_ID {
            Reply::from_rpc(
                self.node
                    .fs_service()
                    .delete(&Permissions::default(), args)
                    .await
                    .map(|_| ()),
            )
        } else {
            Reply::from_core(self.node.remote_delete(&device, args).await.map(|_| ()))
        }
    }

    // ────────────────────────── 跨设备增强传输（任务 2.4） ──────────────────────────

    pub async fn transfer_start(
        self: &Arc<Self>,
        src: String,
        sources: Vec<String>,
        dst: String,
        dest_dir: String,
        conflict: UiConflictPolicy,
    ) -> Reply<String> {
        if sources.is_empty() {
            return Reply::err(UiErrorCode::Invalid, "请先选择要传输的文件或文件夹");
        }
        if src == dst {
            return Reply::err(
                UiErrorCode::Invalid,
                "源设备和目标设备相同，请使用复制或移动",
            );
        }

        // 并发核验源路径存在并生成任务标题（阶段 2 原生支持文件与递归文件夹）
        let sem = Arc::new(Semaphore::new(STAT_CONCURRENCY));
        let mut join_set: JoinSet<(usize, Result<DirEntry, CoreError>)> = JoinSet::new();
        for (idx, s) in sources.iter().cloned().enumerate() {
            let node = self.node.clone();
            let src_c = src.clone();
            let sem_c = sem.clone();
            join_set.spawn(async move {
                let Ok(_permit) = sem_c.acquire_owned().await else {
                    return (idx, Err(CoreError::Cancelled));
                };
                let res = if src_c == SELF_ID {
                    node.fs_service()
                        .stat(&Permissions::default(), FsStatArgs { path: s })
                        .await
                        .map(|r| r.entry)
                        .map_err(CoreError::Rpc)
                } else {
                    node.remote_stat(&src_c, &s).await.map(|r| r.entry)
                };
                (idx, res)
            });
        }
        let mut ordered: Vec<Option<DirEntry>> = (0..sources.len()).map(|_| None).collect();
        while let Some(joined) = join_set.join_next().await {
            match joined {
                Ok((idx, Ok(entry))) => ordered[idx] = Some(entry),
                Ok((_, Err(err))) => {
                    join_set.abort_all();
                    return Reply::from_core(Err(err));
                }
                Err(err) => {
                    join_set.abort_all();
                    return Reply::err(UiErrorCode::Internal, format!("核验源文件任务异常：{err}"));
                }
            }
        }
        let mut names = Vec::with_capacity(sources.len());
        let mut initial_bytes = 0u64;
        for entry in ordered.into_iter().flatten() {
            initial_bytes = initial_bytes.saturating_add(entry.size);
            names.push(entry.name);
        }

        let kind = if src == SELF_ID {
            UiTaskKind::Upload
        } else if dst == SELF_ID {
            UiTaskKind::Download
        } else {
            UiTaskKind::Relay
        };
        let title = if names.len() == 1 {
            names[0].clone()
        } else {
            format!("{} 等 {} 项", names[0], names.len())
        };

        let (seq, ui_task_id) = self.next_task_id();
        let control = TransferTaskControl::new();
        let bytes_done = Arc::new(AtomicU64::new(0));
        let bytes_total = Arc::new(AtomicU64::new(initial_bytes.max(1)));
        let speed_bits = Arc::new(AtomicU64::new(0));
        let meta = TransferMeta {
            seq,
            kind,
            title: title.clone(),
            src_device: src.clone(),
            dst_device: dst.clone(),
            bytes_done: bytes_done.clone(),
            bytes_total: bytes_total.clone(),
            speed_bits,
        };

        self.active_tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                ui_task_id.clone(),
                CancellableTask::Transfer {
                    control: control.clone(),
                    meta: meta.clone(),
                },
            );

        self.sink.emit_task_progress(UiTaskProgress {
            id: ui_task_id.clone(),
            kind,
            title: title.clone(),
            src_device: src.clone(),
            dst_device: dst.clone(),
            bytes_done: 0,
            bytes_total: initial_bytes.max(1),
            speed: 0.0,
            state: UiTaskState::Running,
            error: None,
        });

        let bridge = self.clone();
        let tid = ui_task_id.clone();
        let proto_conflict = conflict.to_proto();
        tokio::spawn(async move {
            bridge
                .run_stage2_transfer_worker(
                    tid,
                    meta,
                    sources,
                    dest_dir,
                    proto_conflict,
                    control,
                )
                .await;
        });

        Reply::ok(ui_task_id)
    }

    pub fn task_list(&self) -> Reply<Vec<UiTaskProgress>> {
        let guard = self.active_tasks.lock().unwrap_or_else(|e| e.into_inner());
        let mut items: Vec<(u64, UiTaskProgress)> = guard
            .iter()
            .map(|(id, task)| match task {
                CancellableTask::Transfer { control, meta } => {
                    let paused = control.is_paused();
                    let bytes_total = meta.bytes_total.load(Ordering::Relaxed).max(1);
                    let bytes_done = meta.bytes_done.load(Ordering::Relaxed).min(bytes_total);
                    let speed = if paused {
                        0.0
                    } else {
                        f64::from_bits(meta.speed_bits.load(Ordering::Relaxed))
                    };
                    (
                        meta.seq,
                        UiTaskProgress {
                            id: id.clone(),
                            kind: meta.kind,
                            title: meta.title.clone(),
                            src_device: meta.src_device.clone(),
                            dst_device: meta.dst_device.clone(),
                            bytes_done,
                            bytes_total,
                            speed,
                            state: if paused {
                                UiTaskState::Paused
                            } else {
                                UiTaskState::Running
                            },
                            error: None,
                        },
                    )
                }
                CancellableTask::LocalFsTask { meta, .. }
                | CancellableTask::RemoteFsTask { meta, .. } => {
                    let bytes_total = meta.bytes_total.load(Ordering::Relaxed).max(1);
                    let bytes_done = meta.bytes_done.load(Ordering::Relaxed).min(bytes_total);
                    let speed = f64::from_bits(meta.speed_bits.load(Ordering::Relaxed));
                    (
                        meta.seq,
                        UiTaskProgress {
                            id: id.clone(),
                            kind: meta.kind,
                            title: meta.title.clone(),
                            src_device: meta.device_id.clone(),
                            dst_device: meta.device_id.clone(),
                            bytes_done,
                            bytes_total,
                            speed,
                            state: UiTaskState::Running,
                            error: None,
                        },
                    )
                }
            })
            .collect();
        items.sort_by_key(|(seq, _)| *seq);
        Reply::ok(items.into_iter().map(|(_, p)| p).collect())
    }

    pub fn task_pause(&self, task_id: String) -> Reply<()> {
        let guard = self.active_tasks.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(CancellableTask::Transfer { control, meta }) = guard.get(&task_id) {
            control.pause();
            meta.speed_bits.store(0, Ordering::Relaxed);
            let p = UiTaskProgress {
                id: task_id,
                kind: meta.kind,
                title: meta.title.clone(),
                src_device: meta.src_device.clone(),
                dst_device: meta.dst_device.clone(),
                bytes_done: meta.bytes_done.load(Ordering::Relaxed),
                bytes_total: meta.bytes_total.load(Ordering::Relaxed).max(1),
                speed: 0.0,
                state: UiTaskState::Paused,
                error: None,
            };
            drop(guard);
            self.sink.emit_task_progress(p);
        }
        Reply::ok(())
    }

    pub fn task_resume(&self, task_id: String) -> Reply<()> {
        let guard = self.active_tasks.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(CancellableTask::Transfer { control, meta }) = guard.get(&task_id) {
            control.resume();
            let p = UiTaskProgress {
                id: task_id,
                kind: meta.kind,
                title: meta.title.clone(),
                src_device: meta.src_device.clone(),
                dst_device: meta.dst_device.clone(),
                bytes_done: meta.bytes_done.load(Ordering::Relaxed),
                bytes_total: meta.bytes_total.load(Ordering::Relaxed).max(1),
                speed: 0.0,
                state: UiTaskState::Running,
                error: None,
            };
            drop(guard);
            self.sink.emit_task_progress(p);
        }
        Reply::ok(())
    }

    pub async fn task_cancel(self: &Arc<Self>, task_id: String) -> Reply<()> {
        let entry = self
            .active_tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&task_id);
        match entry {
            Some(CancellableTask::Transfer { control, .. }) => {
                control.cancel();
                Reply::ok(())
            }
            Some(CancellableTask::LocalFsTask { raw_task_id, .. }) => {
                let _ = self.node.fs_service().task_cancel(&raw_task_id);
                Reply::ok(())
            }
            Some(CancellableTask::RemoteFsTask {
                peer_id,
                remote_task_id,
                ..
            }) => {
                let _ = self.node.remote_task_cancel(&peer_id, &remote_task_id).await;
                Reply::ok(())
            }
            None => Reply::ok(()),
        }
    }

    pub fn transfer_get_speed_limit(&self) -> Reply<Option<u64>> {
        Reply::ok(self.node.rate_limiter().limit_bps())
    }

    pub fn transfer_set_speed_limit(&self, bytes_per_sec: Option<u64>) -> Reply<()> {
        self.node.rate_limiter().set_limit_bps(bytes_per_sec);
        Reply::ok(())
    }

    pub async fn pick_local_files(&self) -> Reply<Vec<String>> {
        match self.node.platform().pick_files().await {
            Ok(paths) => Reply::ok(paths.into_iter().map(|p| to_display_path(&p)).collect()),
            Err(pe) => Reply::from_rpc(Err(map_platform_to_rpc(&pe))),
        }
    }

    pub async fn pick_local_dir(&self) -> Reply<Option<String>> {
        match self.node.platform().pick_folder().await {
            Ok(opt) => Reply::ok(opt.map(|p| to_display_path(&p))),
            Err(pe) => Reply::from_rpc(Err(map_platform_to_rpc(&pe))),
        }
    }

    // ────────────────────────── 软件、图标与进程 ──────────────────────────

    pub async fn app_list(
        self: &Arc<Self>,
        device: String,
        force: bool,
    ) -> Reply<Vec<UiAppInfo>> {
        let res = if device == SELF_ID {
            self.node.local_apps(force).await
        } else {
            self.node.remote_apps(&device, force).await
        };
        Reply::from_core(res.map(|r| {
            r.apps
                .into_iter()
                .map(|a| UiAppInfo {
                    id: a.app_id,
                    name: a.name,
                })
                .collect()
        }))
    }

    pub async fn app_icon(
        self: &Arc<Self>,
        device: String,
        app_id: String,
    ) -> Reply<Option<String>> {
        let cache_key = format!("{device}|{app_id}");
        if let Some(hit) = self
            .icon_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&cache_key)
        {
            return Reply::ok(Some(hit));
        }

        let res = if device == SELF_ID {
            self.node.local_app_icon(&app_id).await
        } else {
            self.node.remote_app_icon(&device, &app_id).await
        };
        match res {
            Ok(rep) => {
                let data_url = rep
                    .png_base64
                    .filter(|s| !s.is_empty())
                    .map(|b64| format!("data:image/png;base64,{b64}"));
                // 仅缓存成功提取到的图标，不缓存瞬态 None
                if let Some(ref url) = data_url {
                    self.icon_cache
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(cache_key, url.clone());
                }
                Reply::ok(data_url)
            }
            Err(e) => Reply::from_core(Err(e)),
        }
    }

    pub async fn app_launch(
        self: &Arc<Self>,
        device: String,
        target: String,
        args: Option<String>,
    ) -> Reply<UiLaunchResult> {
        let kind = classify_launch_target(&target, device == SELF_ID).await;
        let req = AppLaunchArgs {
            target,
            kind,
            args,
            skip_window_wait: false,
        };
        let res = if device == SELF_ID {
            self.node.local_launch(req).await
        } else {
            self.node.remote_launch(&device, req).await
        };
        Reply::from_core(res.map(convert_launch_reply))
    }

    pub async fn proc_list(self: &Arc<Self>, device: String) -> Reply<Vec<UiProcInfo>> {
        let res = if device == SELF_ID {
            self.node.local_processes().await
        } else {
            self.node.remote_processes(&device).await
        };
        Reply::from_core(res.map(|r| {
            r.processes
                .into_iter()
                .map(|p| UiProcInfo {
                    pid: p.pid,
                    name: p.name,
                    title: p.window_title.unwrap_or_default(),
                    memory: p.memory_bytes,
                    cpu: p.cpu_percent,
                    critical: p.is_critical,
                })
                .collect()
        }))
    }

    pub async fn proc_kill(self: &Arc<Self>, device: String, pid: u32) -> Reply<()> {
        let req = ProcKillArgs {
            pid,
            confirmed: true,
        };
        let res = if device == SELF_ID {
            self.node.local_kill_process(req).await
        } else {
            self.node.remote_kill_process(&device, req).await
        };
        Reply::from_core(res.map(|_| ()))
    }

    // ────────────────────────── 诊断页与网络体检（任务 2.11） ──────────────────────────

    pub fn diag_summary(&self) -> Reply<UiDiagSummary> {
        let settings = self.node.settings();
        let states = self.node.all_peer_states();
        let peer_routes = self
            .node
            .list_peers()
            .into_iter()
            .map(|p| {
                let (state_key, detail, rtt_ms) = match states.get(&p.endpoint_id) {
                    Some(DeviceConnState::OnlineDirect { addr, rtt_ms }) => (
                        "online_direct".to_string(),
                        format!("直连 ({addr}) · {} ms", (*rtt_ms).max(1)),
                        Some((*rtt_ms).max(1)),
                    ),
                    Some(DeviceConnState::OnlineRelay { relay_url, rtt_ms }) => (
                        "online_relay".to_string(),
                        format!("中转 ({relay_url}) · {} ms", (*rtt_ms).max(1)),
                        Some((*rtt_ms).max(1)),
                    ),
                    Some(DeviceConnState::Connecting) => {
                        ("connecting".to_string(), "正在重连…".to_string(), None)
                    }
                    Some(DeviceConnState::Offline { .. }) | None => {
                        ("offline".to_string(), "未连接".to_string(), None)
                    }
                };
                UiDiagPeerRoute {
                    peer_id: p.endpoint_id,
                    name: p.name,
                    state: state_key,
                    detail,
                    rtt_ms,
                }
            })
            .collect();

        let netcheck_targets = if settings.custom_relay_urls.is_empty() {
            vec![
                "https://use1-1.relay.n0.iroh.link".to_string(),
                "https://euw1-1.relay.n0.iroh.link".to_string(),
                "https://aps1-1.relay.n0.iroh.link".to_string(),
            ]
        } else {
            settings.custom_relay_urls
        };

        Reply::ok(UiDiagSummary {
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            endpoint_id: self.node.endpoint_id().to_string(),
            device_name: settings.device_name,
            os: std::env::consts::OS.to_string(),
            working_set_bytes: self.node.platform().current_process_working_set(),
            host_paused: self.node.is_host_paused(),
            speed_limit_bps: self.node.rate_limiter().limit_bps(),
            netcheck_targets,
            peer_routes,
        })
    }

    pub async fn diag_netcheck(self: &Arc<Self>) -> Reply<UiNetcheckReport> {
        Reply::from_core(self.node.run_netcheck().await.map(|rep| {
            let preferred_relay = rep
                .relays
                .iter()
                .find(|r| r.preferred)
                .map(|r| r.relay_host.clone());
            let relay_latencies = rep
                .relays
                .into_iter()
                .map(|r| UiRelayLatencyItem {
                    url: r.relay_host,
                    rtt_ms: r.ipv4_ms.or(r.ipv6_ms).or(r.https_ms),
                })
                .collect();
            UiNetcheckReport {
                checked_at_ms: rep.checked_at_ms,
                udp_ipv4: rep.udp_v4,
                udp_ipv6: rep.udp_v6,
                mapping_varies_by_dest_ip: rep.mapping_varies_by_dest_ip,
                upnp_available: None,
                pcp_available: None,
                nat_pmp_available: None,
                preferred_relay,
                relay_latencies,
                summary: rep.summary,
            }
        }))
    }

    pub fn diag_logs(&self, limit: usize) -> Reply<UiDiagLogs> {
        let cap = limit.clamp(10, 1000);
        let runtime_logs = self.node.recent_logs(cap);
        let audits = self
            .node
            .storage()
            .read_recent_audits(cap)
            .unwrap_or_default()
            .into_iter()
            .map(|a| {
                let ts = Local
                    .timestamp_millis_opt(a.timestamp_ms as i64)
                    .single()
                    .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
                    .unwrap_or_else(|| a.timestamp_ms.to_string());
                UiAuditEntry {
                    ts,
                    peer_id: a.peer_id,
                    peer_name: a.peer_name,
                    cmd: a.action,
                    detail: if a.detail.is_empty() {
                        None
                    } else {
                        Some(a.detail)
                    },
                    ok: a.ok,
                }
            })
            .collect();
        Reply::ok(UiDiagLogs {
            runtime_logs,
            audits,
        })
    }

    pub async fn diag_export(self: &Arc<Self>, dest_dir: String) -> Reply<String> {
        Reply::from_core(
            self.node
                .export_diagnostics(Path::new(&dest_dir))
                .await
                .map(|p| to_display_path(&p)),
        )
    }

    // ────────────────────────── 后台任务进度与限流辅助 ──────────────────────────

    fn next_task_id(&self) -> (u64, String) {
        let n = self.task_seq.fetch_add(1, Ordering::Relaxed);
        (n, format!("ui-task-{n}"))
    }

    async fn run_stage2_transfer_worker(
        self: Arc<Self>,
        ui_task_id: String,
        meta: TransferMeta,
        sources: Vec<String>,
        dest_dir: String,
        conflict: ConflictPolicy,
        control: TransferTaskControl,
    ) {
        let last_emit = Arc::new(Mutex::new(Instant::now()));
        let started = Instant::now();
        // 留空 speed_limit_bps，使在途任务动态跟随 CoreNode::rate_limiter() 实时全局限速而非静态冻结
        let opts = TransferQueueOptions {
            conflict,
            concurrency: 4,
            speed_limit_bps: None,
        };

        let make_cb =
            |base_done: u64, total_scale: u64, base_total: u64| -> pcshare_core::ProgressCallback {
                let sink = self.sink.clone();
                let last_c = last_emit.clone();
                let tid = ui_task_id.clone();
                let meta_c = meta.clone();
                let ctrl_c = control.clone();
                Arc::new(move |done: u64, tot: u64, speed: f64| {
                    let scaled_done = base_done.saturating_add(done);
                    let scaled_tot = base_total
                        .saturating_add(tot.saturating_mul(total_scale))
                        .max(scaled_done)
                        .max(1);
                    meta_c.bytes_done.store(scaled_done, Ordering::Relaxed);
                    meta_c.bytes_total.store(scaled_tot, Ordering::Relaxed);
                    meta_c.speed_bits.store(speed.to_bits(), Ordering::Relaxed);
                    if ctrl_c.is_paused() {
                        return;
                    }
                    emit_throttled_running(
                        &sink,
                        &last_c,
                        UiTaskProgress {
                            id: tid.clone(),
                            kind: meta_c.kind,
                            title: meta_c.title.clone(),
                            src_device: meta_c.src_device.clone(),
                            dst_device: meta_c.dst_device.clone(),
                            bytes_done: scaled_done.min(scaled_tot),
                            bytes_total: scaled_tot,
                            speed,
                            state: UiTaskState::Running,
                            error: None,
                        },
                    );
                })
            };

        let outcome: Result<u64, CoreError> = match meta.kind {
            UiTaskKind::Upload => {
                let local_paths: Vec<PathBuf> = sources.iter().map(PathBuf::from).collect();
                self.node
                    .transfer_paths_to_peer(
                        &meta.dst_device,
                        &local_paths,
                        &dest_dir,
                        opts,
                        control.clone(),
                        Some(make_cb(0, 1, 0)),
                    )
                    .await
            }
            UiTaskKind::Download => {
                self.node
                    .download_paths_from_peer(
                        &meta.src_device,
                        &sources,
                        Path::new(&dest_dir),
                        opts,
                        control.clone(),
                        Some(make_cb(0, 1, 0)),
                    )
                    .await
            }
            UiTaskKind::Relay => {
                // 任务 2.4：两台受控电脑之间优先建立一次性授权直传（数据不经过本机），
                // 仅在连接/网络/不支持类错误时退回本机中转，确定性权限/路径/取消错误直接返回。
                let direct_res = self
                    .node
                    .transfer_between_peers_direct(
                        &meta.src_device,
                        &sources,
                        &meta.dst_device,
                        &dest_dir,
                        conflict,
                        control.cancel_token(),
                        Some(make_cb(0, 1, 0)),
                    )
                    .await;
                match direct_res {
                    Ok(()) => Ok(meta.bytes_total.load(Ordering::Relaxed)),
                    Err(err) if !should_fallback_direct_to_relay(&err) => Err(err),
                    Err(_) => {
                        let relay_tmp = std::env::temp_dir().join(format!(
                            "pcshare_relay_dir_{}_{}",
                            ui_task_id,
                            fastrand::u64(..)
                        ));
                        if let Err(e) = tokio::fs::create_dir_all(&relay_tmp).await {
                            Err(CoreError::Io(e))
                        } else {
                            let dl_res = self
                                .node
                                .download_paths_from_peer(
                                    &meta.src_device,
                                    &sources,
                                    &relay_tmp,
                                    TransferQueueOptions {
                                        conflict: ConflictPolicy::Overwrite,
                                        concurrency: 4,
                                        speed_limit_bps: None,
                                    },
                                    control.clone(),
                                    Some(make_cb(0, 2, 0)),
                                )
                                .await;
                            let up_res = match dl_res {
                                Ok(dl_bytes) => {
                                    let dl_base =
                                        dl_bytes.max(meta.bytes_done.load(Ordering::Relaxed));
                                    let mut staged = Vec::new();
                                    if let Ok(mut rd) = tokio::fs::read_dir(&relay_tmp).await {
                                        while let Ok(Some(item)) = rd.next_entry().await {
                                            staged.push(item.path());
                                        }
                                    }
                                    self.node
                                        .transfer_paths_to_peer(
                                            &meta.dst_device,
                                            &staged,
                                            &dest_dir,
                                            opts,
                                            control.clone(),
                                            Some(make_cb(dl_base, 1, dl_base)),
                                        )
                                        .await
                                        .map(|up_bytes| dl_base.saturating_add(up_bytes))
                                }
                                Err(e) => Err(e),
                            };
                            let _ = tokio::fs::remove_dir_all(&relay_tmp).await;
                            up_res
                        }
                    }
                }
            }
            _ => unreachable!(),
        };

        self.active_tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&ui_task_id);

        let cur_done = meta.bytes_done.load(Ordering::Relaxed);
        let cur_tot = meta.bytes_total.load(Ordering::Relaxed).max(cur_done).max(1);
        match outcome {
            Ok(written) => {
                let final_tot = written.max(cur_tot).max(1);
                let avg_speed = final_tot as f64 / started.elapsed().as_secs_f64().max(0.001);
                let done_title = meta.title.clone();
                self.sink.emit_task_progress(UiTaskProgress {
                    id: ui_task_id,
                    kind: meta.kind,
                    title: meta.title,
                    src_device: meta.src_device,
                    dst_device: meta.dst_device,
                    bytes_done: final_tot,
                    bytes_total: final_tot,
                    speed: avg_speed,
                    state: UiTaskState::Done,
                    error: None,
                });
                self.sink.emit_system_notice(UiSystemNotice {
                    title: "传输已完成".to_string(),
                    body: done_title,
                    kind: "success".to_string(),
                });
            }
            Err(CoreError::Cancelled) => {
                self.sink.emit_task_progress(UiTaskProgress {
                    id: ui_task_id,
                    kind: meta.kind,
                    title: meta.title,
                    src_device: meta.src_device,
                    dst_device: meta.dst_device,
                    bytes_done: cur_done,
                    bytes_total: cur_tot,
                    speed: 0.0,
                    state: UiTaskState::Cancelled,
                    error: None,
                });
            }
            Err(err) => {
                self.sink.emit_task_progress(UiTaskProgress {
                    id: ui_task_id,
                    kind: meta.kind,
                    title: meta.title,
                    src_device: meta.src_device,
                    dst_device: meta.dst_device,
                    bytes_done: cur_done,
                    bytes_total: cur_tot,
                    speed: 0.0,
                    state: UiTaskState::Failed,
                    error: Some(map_core_error(&err)),
                });
            }
        }
    }

    async fn poll_local_fs_task(
        self: Arc<Self>,
        ui_task_id: String,
        raw_task_id: String,
        meta: FsTaskMeta,
    ) {
        let mut rx = self.node.fs_service().subscribe_task_events();
        let started = Instant::now();
        let last_emit = Mutex::new(Instant::now() - PROGRESS_THROTTLE);

        loop {
            tokio::select! {
                _ = self.shutdown.cancelled() => break,
                _ = tokio::time::sleep(Duration::from_millis(100)) => {
                    match self.node.fs_service().task_status(&raw_task_id) {
                        Ok(p) => {
                            if emit_fs_task_progress(
                                &self.sink,
                                &last_emit,
                                &ui_task_id,
                                &meta,
                                &p,
                                started,
                            ) {
                                break;
                            }
                        }
                        Err(rpc) => {
                            self.sink.emit_task_progress(UiTaskProgress {
                                id: ui_task_id.clone(),
                                kind: meta.kind,
                                title: meta.title.clone(),
                                src_device: meta.device_id.clone(),
                                dst_device: meta.device_id.clone(),
                                bytes_done: meta.bytes_done.load(Ordering::Relaxed),
                                bytes_total: meta.bytes_total.load(Ordering::Relaxed).max(1),
                                speed: 0.0,
                                state: UiTaskState::Failed,
                                error: Some(map_rpc_error(&rpc)),
                            });
                            break;
                        }
                    }
                }
                ev = rx.recv() => {
                    match ev {
                        Ok((_peer, RemoteEvent::TaskProgress(p))) if p.task_id == raw_task_id => {
                            if emit_fs_task_progress(
                                &self.sink,
                                &last_emit,
                                &ui_task_id,
                                &meta,
                                &p,
                                started,
                            ) {
                                break;
                            }
                        }
                        Ok(_) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            match self.node.fs_service().task_status(&raw_task_id) {
                                Ok(p) => {
                                    if emit_fs_task_progress(
                                        &self.sink,
                                        &last_emit,
                                        &ui_task_id,
                                        &meta,
                                        &p,
                                        started,
                                    ) {
                                        break;
                                    }
                                }
                                Err(rpc) => {
                                    self.sink.emit_task_progress(UiTaskProgress {
                                        id: ui_task_id.clone(),
                                        kind: meta.kind,
                                        title: meta.title.clone(),
                                        src_device: meta.device_id.clone(),
                                        dst_device: meta.device_id.clone(),
                                        bytes_done: meta.bytes_done.load(Ordering::Relaxed),
                                        bytes_total: meta.bytes_total.load(Ordering::Relaxed).max(1),
                                        speed: 0.0,
                                        state: UiTaskState::Failed,
                                        error: Some(map_rpc_error(&rpc)),
                                    });
                                    break;
                                }
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            if let Ok(p) = self.node.fs_service().task_status(&raw_task_id) {
                                let _ = emit_fs_task_progress(
                                    &self.sink,
                                    &last_emit,
                                    &ui_task_id,
                                    &meta,
                                    &p,
                                    started,
                                );
                            }
                            break;
                        }
                    }
                }
            }
        }
        self.active_tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&ui_task_id);
    }

    async fn poll_remote_fs_task(
        self: Arc<Self>,
        ui_task_id: String,
        peer_id: String,
        raw_task_id: String,
        meta: FsTaskMeta,
    ) {
        let started = Instant::now();
        let last_emit = Mutex::new(Instant::now() - PROGRESS_THROTTLE);

        loop {
            let status_res = tokio::select! {
                _ = self.shutdown.cancelled() => break,
                res = self.node.remote_task_status(&peer_id, &raw_task_id) => res,
            };
            match status_res {
                Ok(p) => {
                    if emit_fs_task_progress(
                        &self.sink,
                        &last_emit,
                        &ui_task_id,
                        &meta,
                        &p,
                        started,
                    ) {
                        break;
                    }
                }
                Err(err) => {
                    self.sink.emit_task_progress(UiTaskProgress {
                        id: ui_task_id.clone(),
                        kind: meta.kind,
                        title: meta.title.clone(),
                        src_device: meta.device_id.clone(),
                        dst_device: meta.device_id.clone(),
                        bytes_done: meta.bytes_done.load(Ordering::Relaxed),
                        bytes_total: meta.bytes_total.load(Ordering::Relaxed).max(1),
                        speed: 0.0,
                        state: UiTaskState::Failed,
                        error: Some(map_core_error(&err)),
                    });
                    break;
                }
            }
            tokio::select! {
                _ = self.shutdown.cancelled() => break,
                _ = tokio::time::sleep(PROGRESS_THROTTLE) => {}
            }
        }
        self.active_tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&ui_task_id);
    }
}

// ────────────────────────────── 映射与工具函数 ──────────────────────────────

/// 仅对连接、超时、协议帧或不支持直传等网络/能力错误回退本机中转；
/// 权限不足、文件不存在、参数非法、已取消、被控暂停等确定性错误直接报错，避免无效下载。
pub fn should_fallback_direct_to_relay(err: &CoreError) -> bool {
    match err {
        CoreError::NotConnected(_) | CoreError::Timeout | CoreError::Frame(_) => true,
        CoreError::Rpc(rpc) => matches!(
            rpc.code,
            ErrorCode::Unsupported | ErrorCode::Io | ErrorCode::Internal
        ),
        CoreError::Internal(_) => true,
        CoreError::Cancelled
        | CoreError::InvalidArgument(_)
        | CoreError::Platform(_)
        | CoreError::Io(_) => false,
    }
}

fn emit_throttled_running(
    sink: &Arc<dyn EventSink>,
    last_emit: &Mutex<Instant>,
    progress: UiTaskProgress,
) {
    let mut guard = last_emit.lock().unwrap_or_else(|e| e.into_inner());
    if guard.elapsed() >= PROGRESS_THROTTLE {
        *guard = Instant::now();
        drop(guard);
        sink.emit_task_progress(progress);
    }
}

fn emit_fs_task_progress(
    sink: &Arc<dyn EventSink>,
    last_emit: &Mutex<Instant>,
    ui_task_id: &str,
    meta: &FsTaskMeta,
    p: &FsTaskProgress,
    started: Instant,
) -> bool {
    let speed = p.processed_bytes as f64 / started.elapsed().as_secs_f64().max(0.001);
    let total = p.total_bytes.max(p.processed_bytes).max(1);
    let (state, terminal) = match p.state {
        FsTaskState::Running => (UiTaskState::Running, false),
        FsTaskState::Completed => (UiTaskState::Done, true),
        FsTaskState::Failed => (UiTaskState::Failed, true),
        FsTaskState::Cancelled => (UiTaskState::Cancelled, true),
    };
    let bytes_done = if state == UiTaskState::Done {
        total
    } else {
        p.processed_bytes.min(total)
    };
    meta.bytes_done.store(bytes_done, Ordering::Relaxed);
    meta.bytes_total.store(total, Ordering::Relaxed);
    meta.speed_bits.store(speed.to_bits(), Ordering::Relaxed);

    let error = p.error.as_ref().map(map_rpc_error);
    let payload = UiTaskProgress {
        id: ui_task_id.to_string(),
        kind: meta.kind,
        title: meta.title.clone(),
        src_device: meta.device_id.clone(),
        dst_device: meta.device_id.clone(),
        bytes_done,
        bytes_total: total,
        speed,
        state,
        error,
    };

    if terminal {
        sink.emit_task_progress(payload);
        true
    } else {
        emit_throttled_running(sink, last_emit, payload);
        false
    }
}

fn to_ui_device(peer: &PeerRecord, state: Option<&DeviceConnState>) -> UiDevice {
    let os = match peer
        .os
        .as_deref()
        .unwrap_or("unknown")
        .to_ascii_lowercase()
        .as_str()
    {
        "windows" => "windows",
        "macos" | "darwin" => "macos",
        "linux" => "linux",
        _ => "unknown",
    }
    .to_string();

    let status = match state {
        Some(DeviceConnState::Connecting) => UiDeviceStatus::Connecting,
        Some(DeviceConnState::OnlineDirect { rtt_ms, .. }) => UiDeviceStatus::Online {
            link: UiLinkKind::Direct,
            rtt_ms: (*rtt_ms).max(1),
        },
        Some(DeviceConnState::OnlineRelay { rtt_ms, .. }) => UiDeviceStatus::Online {
            link: UiLinkKind::Relay,
            rtt_ms: (*rtt_ms).max(1),
        },
        Some(DeviceConnState::Offline { .. }) | None => UiDeviceStatus::Offline {
            last_seen: peer.last_seen_ms,
        },
    };

    let mut features = vec![
        "fs".to_string(),
        "transfer".to_string(),
        "apps".to_string(),
        "procs".to_string(),
    ];
    let caps = &peer.capabilities;
    let has_cap = |tag: &str| caps.is_empty() || caps.iter().any(|c| c == tag);
    if has_cap("shot.v1") {
        features.push("shot".to_string());
    }
    if has_cap("input.v1") {
        features.push("input".to_string());
    }
    if has_cap("power.v1") {
        features.push("power".to_string());
    }
    if has_cap("clip.v1") {
        features.push("clip".to_string());
    }
    if has_cap("term.v1") {
        features.push("term".to_string());
    }
    if has_cap("search.v1") {
        features.push("search".to_string());
    }

    UiDevice {
        id: peer.endpoint_id.clone(),
        name: peer.name.clone(),
        os,
        status,
        features,
        has_mac: !peer.mac_addresses.is_empty(),
        permissions: Some(UiPermissions::from_proto(&peer.permissions)),
    }
}

fn convert_roots(r: FsRootsReply) -> Vec<UiFsRoot> {
    let mut out = Vec::with_capacity(r.drives.len() + r.special_folders.len());
    for d in r.drives {
        let name = if d.name == d.path {
            format!("本地磁盘 ({})", d.path.trim_end_matches('\\'))
        } else {
            d.name
        };
        out.push(UiFsRoot {
            name,
            path: d.path,
            kind: "drive".to_string(),
            total: d.total_bytes,
            free: d.available_bytes,
        });
    }
    for f in r.special_folders {
        let kind = match f.kind {
            RootKind::Desktop => "desktop",
            RootKind::Downloads => "downloads",
            RootKind::Documents => "documents",
            RootKind::Home | RootKind::AllowedFolder => "home",
            RootKind::Drive => "drive",
        }
        .to_string();
        out.push(UiFsRoot {
            name: f.name,
            path: f.path,
            kind,
            total: f.total_bytes,
            free: f.available_bytes,
        });
    }
    out
}

fn convert_entry(e: DirEntry) -> UiFsEntry {
    UiFsEntry {
        name: e.name,
        dir: e.kind == EntryKind::Dir,
        size: e.size,
        mtime: e.modified_ms.unwrap_or(0),
    }
}

pub async fn classify_launch_target(target: &str, is_local: bool) -> LaunchKind {
    let t = target.trim();
    let bytes = t.as_bytes();
    let is_drive_path = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/');
    if is_drive_path
        || t.starts_with(r"\\")
        || t.starts_with("shell:")
        || t.starts_with("http://")
        || t.starts_with("https://")
    {
        LaunchKind::PathOrTarget
    } else if is_local
        && let Ok(clean) = normalize_lexical_absolute(t)
        && tokio::fs::try_exists(to_fs_path(&clean))
            .await
            .unwrap_or(false)
    {
        LaunchKind::PathOrTarget
    } else {
        LaunchKind::AppId
    }
}

pub(crate) fn convert_desktop_status(d: &pcshare_proto::DesktopStatus) -> UiDesktopState {
    let place = if d.session_id == 0 {
        "service"
    } else if d.input_desktop.is_none() {
        "locked"
    } else if d.session_id != d.console_session_id {
        "remote"
    } else {
        "normal"
    };
    UiDesktopState {
        place: place.to_string(),
        elevated: d.elevated,
    }
}

pub(crate) fn convert_launch_reply(r: AppLaunchReply) -> UiLaunchResult {
    UiLaunchResult {
        new_windows: r.new_windows,
        desktop: convert_desktop_status(&r.desktop),
        elevation_prompt: if r.uac_prompt_suspected {
            Some(true)
        } else {
            None
        },
    }
}

fn build_copy_move_title(sources: &[String], dest_dir: &str, is_move: bool) -> String {
    let verb = if is_move { "移动" } else { "复制" };
    let first_name = PathBuf::from(&sources[0])
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| sources[0].clone());
    let dest_name = PathBuf::from(dest_dir)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| dest_dir.to_string());
    if sources.len() == 1 {
        format!("{verb} {first_name} → {dest_name}")
    } else {
        format!("{verb} {} 项 → {dest_name}", sources.len())
    }
}

fn map_rpc_error(err: &RpcError) -> ApiErrorBody {
    let code = match err.code {
        ErrorCode::Unauthorized => {
            if err.msg.contains("配对码不正确") {
                UiErrorCode::BadCode
            } else if err.msg.contains("有效期") || err.msg.contains("作废") {
                UiErrorCode::Expired
            } else {
                UiErrorCode::Unauthorized
            }
        }
        ErrorCode::Forbidden => UiErrorCode::Forbidden,
        ErrorCode::NotFound => UiErrorCode::NotFound,
        ErrorCode::AlreadyExists => UiErrorCode::AlreadyExists,
        ErrorCode::Busy => UiErrorCode::Busy,
        ErrorCode::Invalid => UiErrorCode::Invalid,
        ErrorCode::Unsupported => UiErrorCode::Unsupported,
        ErrorCode::Io => UiErrorCode::Io,
        ErrorCode::Cancelled => UiErrorCode::Cancelled,
        ErrorCode::Internal => UiErrorCode::Internal,
    };
    ApiErrorBody {
        code,
        msg: err.msg.clone(),
    }
}

fn map_core_error(err: &CoreError) -> ApiErrorBody {
    match err {
        CoreError::Rpc(rpc) => map_rpc_error(rpc),
        CoreError::Platform(pe) => map_rpc_error(&map_platform_to_rpc(pe)),
        CoreError::NotConnected(msg) => ApiErrorBody {
            code: UiErrorCode::Offline,
            msg: msg.clone(),
        },
        CoreError::Timeout => ApiErrorBody {
            code: UiErrorCode::Timeout,
            msg: "对方迟迟没有回应，请检查网络是否通畅后再试".to_string(),
        },
        CoreError::Cancelled => ApiErrorBody {
            code: UiErrorCode::Cancelled,
            msg: "已取消".to_string(),
        },
        CoreError::InvalidArgument(msg) => ApiErrorBody {
            code: UiErrorCode::Invalid,
            msg: msg.clone(),
        },
        CoreError::Io(io_err) => map_rpc_error(&map_io_to_rpc(io_err, None)),
        CoreError::Frame(fe) => ApiErrorBody {
            code: UiErrorCode::Internal,
            msg: format!("协议传输错误：{fe}"),
        },
        CoreError::Internal(msg) => ApiErrorBody {
            code: UiErrorCode::Internal,
            msg: msg.clone(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_cache_enforces_limit_and_clears_removed_device() {
        let mut cache = IconCache::new(3);
        cache.insert("devA|app1".to_string(), "data:1".to_string());
        cache.insert("devA|app2".to_string(), "data:2".to_string());
        cache.insert("devB|app1".to_string(), "data:3".to_string());
        // 命中 devA|app1 使其成为最近使用，下一次插入应淘汰 devA|app2
        assert_eq!(cache.get("devA|app1").as_deref(), Some("data:1"));
        cache.insert("devB|app2".to_string(), "data:4".to_string());
        assert_eq!(cache.len(), 3);
        assert!(cache.get("devA|app2").is_none());
        assert_eq!(cache.get("devA|app1").as_deref(), Some("data:1"));

        cache.remove_device("devB");
        assert_eq!(cache.len(), 1);
        assert!(cache.get("devB|app1").is_none());
        assert!(cache.get("devB|app2").is_none());
        assert_eq!(cache.get("devA|app1").as_deref(), Some("data:1"));
    }

    #[test]
    fn direct_transfer_fallback_distinguishes_deterministic_vs_network_errors() {
        assert!(!should_fallback_direct_to_relay(&CoreError::Cancelled));
        assert!(!should_fallback_direct_to_relay(&CoreError::InvalidArgument(
            "bad".into()
        )));
        for code in [
            ErrorCode::Forbidden,
            ErrorCode::Unauthorized,
            ErrorCode::NotFound,
            ErrorCode::AlreadyExists,
            ErrorCode::Invalid,
            ErrorCode::Cancelled,
            ErrorCode::Busy,
        ] {
            let err = CoreError::Rpc(RpcError::new(code, "deterministic"));
            assert!(
                !should_fallback_direct_to_relay(&err),
                "code {code:?} must not fallback to relay"
            );
        }

        assert!(should_fallback_direct_to_relay(&CoreError::NotConnected(
            "offline".into()
        )));
        assert!(should_fallback_direct_to_relay(&CoreError::Timeout));
        assert!(should_fallback_direct_to_relay(&CoreError::Rpc(
            RpcError::new(ErrorCode::Unsupported, "old peer")
        )));
    }

    #[tokio::test]
    async fn classify_launch_target_only_checks_local_filesystem_for_self() {
        assert_eq!(
            classify_launch_target(r"C:\Windows\notepad.exe", false).await,
            LaunchKind::PathOrTarget
        );
        assert_eq!(
            classify_launch_target("Microsoft.WindowsCalculator_8wekyb3d8bbwe!App", false).await,
            LaunchKind::AppId
        );
        // 远端设备对非盘符/非协议格式绝不查本机文件系统
        let cwd = std::env::current_dir().unwrap();
        let rel_marker = cwd.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(
            classify_launch_target(&rel_marker, false).await,
            LaunchKind::AppId
        );
    }
}
