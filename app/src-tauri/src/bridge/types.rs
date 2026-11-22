use super::*;

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
    pub(super) fn to_proto(self) -> ConflictPolicy {
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
