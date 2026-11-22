//! 核心节点 `CoreNode`：统一封装连接管理（任务 1.5）、配对（任务 1.6）、
//! 被控端指令分发与控制端远程调用（任务 1.7、1.8、1.9）。

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use chrono::Utc;
use iroh::endpoint::{Connection, RecvStream, SendStream, presets};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode, RelayUrl, TransportAddr, Watcher};
use pcshare_platform::Platform;
use pcshare_proto::*;
use serde::de::DeserializeOwned;
use tokio::sync::{Notify, broadcast};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use crate::app_proc::{
    handle_app_icon, handle_app_launch, handle_app_list, handle_proc_kill, handle_proc_list,
    to_proto_desktop,
};
use crate::clipboard::{handle_clip_get_stream, handle_clip_set_stream};
use crate::diagnostics::{
    DiagnosticPeerRoute, DiagnosticSummary, NetcheckReport, append_runtime_log,
    read_recent_runtime_logs, run_netcheck, write_diagnostic_bundle,
};
use crate::dir_cache::DirCache;
use crate::edit_sync::EditSyncManager;
use crate::error::CoreError;
use crate::events::{
    CoreEvent, DeviceConnState, PairingDecision, PairingSessionTicket, ProgressCallback,
};
use crate::fs_service::FsService;
use crate::fs_watch::FsWatchManager;
use crate::lan_discovery::{LanDiscovery, LanPeerAnnouncement};
use crate::pairing::{PairingManager, ParsedInvite, parse_invite_or_id};
use crate::screen_input::{
    handle_input_click_stream, handle_input_key_stream, handle_shot_monitors,
    handle_shot_screen_stream, handle_shot_window_stream,
};
use crate::storage::{AuditEntry, LocalStorage, PeerRecord, Settings};
use crate::terminal::handle_term_open_stream;
use crate::transfer::{DirectGrantRecord, RateLimiter};

const DEFAULT_RPC_TIMEOUT: Duration = Duration::from_secs(15);
const CLOSE_OK: u32 = 0;
const CLOSE_UNAUTHORIZED: u32 = 401;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NetworkMode {
    /// 生产环境：默认使用 iroh 公共中转/发现，若配置了自定义中转则使用自定义服务器
    #[default]
    PublicOrCustom,
    /// 本地测试模式：不连接公网中转与 DNS，仅通过本地 UDP 地址直连
    LocalTest,
}

#[derive(Debug, Clone)]
pub struct CoreConfig {
    pub data_dir: PathBuf,
    pub network_mode: NetworkMode,
    pub device_name_override: Option<String>,
    /// 启动后是否自动为所有已配对设备维护后台长连接（桌面端与守护模式为 `true`，单次 CLI 命令可设为 `false`）
    pub auto_connect_peers: bool,
}

impl Default for CoreConfig {
    fn default() -> Self {
        Self {
            data_dir: crate::storage::default_data_dir(),
            network_mode: NetworkMode::PublicOrCustom,
            device_name_override: None,
            auto_connect_peers: true,
        }
    }
}

pub(super) struct PeerWorkerEntry {
    pub(super) id: u64,
    pub(super) cancel: CancellationToken,
}

pub struct CoreNode {
    pub(crate) desktop_input_lock: tokio::sync::Mutex<()>,
    pub(crate) power_exec_lock: tokio::sync::Mutex<()>,
    config: CoreConfig,
    pub(crate) storage: Arc<LocalStorage>,
    pub(crate) platform: Arc<dyn Platform>,
    endpoint: Endpoint,
    fs_service: Arc<FsService>,
    dir_cache: Arc<DirCache>,
    fs_watch: Arc<FsWatchManager>,
    lan_discovery: Arc<LanDiscovery>,
    rate_limiter: Arc<RateLimiter>,
    edit_sync: Arc<EditSyncManager>,
    apps_index: Mutex<HashMap<String, Vec<InstalledApp>>>,
    active_power_cancel: Mutex<Option<tokio::sync::watch::Sender<bool>>>,
    dry_run_power_actions: AtomicBool,
    last_applied_clip_seq: AtomicU32,
    clipboard_auto_sync_enabled: AtomicBool,
    clipboard_auto_sync_notify: Arc<Notify>,
    host_paused: AtomicBool,
    direct_grants: Mutex<HashMap<String, DirectGrantRecord>>,
    active_direct_transfers: Mutex<HashMap<String, CancellationToken>>,
    pairing: Arc<PairingManager>,
    pub(crate) events_tx: broadcast::Sender<CoreEvent>,
    remote_events_tx: broadcast::Sender<(String, RemoteEvent)>,
    active_conns: RwLock<HashMap<String, Connection>>,
    bulk_conns: RwLock<HashMap<(String, usize), Connection>>,
    incoming_conns: RwLock<HashMap<String, Vec<Connection>>>,
    dial_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    peer_states: RwLock<HashMap<String, DeviceConnState>>,
    peer_workers: Mutex<HashMap<String, PeerWorkerEntry>>,
    next_worker_id: AtomicU64,
    accept_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
    shutdown_token: CancellationToken,
    startup_warnings: Vec<String>,
}

mod applications;
mod connect;
mod dispatch;
mod download;
mod incoming;
mod pair_client;
mod peers;
mod remote_files;
mod services;
mod startup;
mod upload;
mod wire;
mod workers;

pub(crate) use wire::build_endpoint_addr;
pub use wire::node_capabilities;
use wire::{inspect_conn_state, rpc_once, write_rpc_result, write_rpc_result_opts};
