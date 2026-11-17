//! 核心节点 `CoreNode`：统一封装连接管理（任务 1.5）、配对（任务 1.6）、
//! 被控端指令分发与控制端远程调用（任务 1.7、1.8、1.9）。

use std::collections::HashMap;
use std::fs;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, UNIX_EPOCH};

use chrono::Utc;
use iroh::endpoint::{Connection, RecvStream, SendStream, presets};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode, RelayUrl, TransportAddr, Watcher};
use pcshare_platform::Platform;
use pcshare_proto::*;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::broadcast;
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
use crate::error::{CoreError, map_io_to_rpc};
use crate::events::{
    CoreEvent, DeviceConnState, PairingDecision, PairingSessionTicket, ProgressCallback,
    TransferDirection, TransferProgressEvent,
};
use crate::fs_service::{FsService, pick_unique_target, replace_file_atomically, system_time_to_ms};
use crate::fs_watch::FsWatchManager;
use crate::lan_discovery::{LanDiscovery, LanPeerAnnouncement};
use crate::pairing::{PairingManager, ParsedInvite, parse_invite_or_id};
use crate::path_guard::{normalize_lexical_absolute, to_display_path, to_fs_path};
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

pub struct CoreNode {
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
    host_paused: AtomicBool,
    direct_grants: Mutex<HashMap<String, DirectGrantRecord>>,
    pairing: Arc<PairingManager>,
    pub(crate) events_tx: broadcast::Sender<CoreEvent>,
    remote_events_tx: broadcast::Sender<(String, RemoteEvent)>,
    active_conns: RwLock<HashMap<String, Connection>>,
    dial_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    peer_states: RwLock<HashMap<String, DeviceConnState>>,
    peer_workers: Mutex<HashMap<String, CancellationToken>>,
    accept_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
    shutdown_token: CancellationToken,
    startup_warnings: Vec<String>,
}

impl CoreNode {
    pub async fn start(
        config: CoreConfig,
        platform: Arc<dyn Platform>,
    ) -> Result<Arc<Self>, CoreError> {
        Self::start_with_bind_port(config, platform, None).await
    }

    pub async fn start_with_bind_port(
        config: CoreConfig,
        platform: Arc<dyn Platform>,
        bind_port_v4: Option<u16>,
    ) -> Result<Arc<Self>, CoreError> {
        let (storage, warnings) = LocalStorage::open(&config.data_dir, platform.clone())?;
        if let Some(ref name_override) = config.device_name_override {
            let _ = storage.update_settings(|s| {
                s.device_name = name_override.clone();
            });
        }
        let storage = Arc::new(storage);
        let settings = storage.settings();

        let mut builder = match config.network_mode {
            NetworkMode::PublicOrCustom => Endpoint::builder(presets::N0),
            NetworkMode::LocalTest => Endpoint::builder(presets::Minimal),
        };
        builder = builder
            .secret_key(storage.secret_key().clone())
            .alpns(vec![
                ALPN_MAIN.to_vec(),
                ALPN_PAIR.to_vec(),
                ALPN_DIRECT.to_vec(),
            ]);

        if config.network_mode == NetworkMode::PublicOrCustom
            && !settings.custom_relay_urls.is_empty()
        {
            let mut relay_urls = Vec::new();
            for raw in &settings.custom_relay_urls {
                let parsed: RelayUrl = raw.parse().map_err(|e| {
                    CoreError::InvalidArgument(format!("无效的自定义中转服务器地址“{raw}”：{e}"))
                })?;
                relay_urls.push(parsed);
            }
            builder = builder.relay_mode(RelayMode::custom(relay_urls));
        }

        if let Some(port) = bind_port_v4 {
            builder = builder
                .clear_ip_transports()
                .bind_addr(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))
                .map_err(|e| CoreError::Internal(format!("绑定指定本地端口失败：{e}")))?;
        } else if config.network_mode == NetworkMode::LocalTest {
            builder = builder
                .clear_ip_transports()
                .bind_addr(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
                .map_err(|e| CoreError::Internal(format!("绑定本地测试端口失败：{e}")))?;
        }

        let endpoint = builder
            .bind()
            .await
            .map_err(|e| CoreError::Internal(format!("启动网络连接层失败：{e}")))?;

        let (events_tx, _) = broadcast::channel(256);
        let (remote_events_tx, _) = broadcast::channel(256);
        let fs_service = Arc::new(FsService::new(platform.clone(), remote_events_tx.clone()));
        let dir_cache = Arc::new(DirCache::load_or_new(&config.data_dir));
        let fs_watch = FsWatchManager::new(remote_events_tx.clone(), events_tx.clone());
        let iroh_port = endpoint
            .addr()
            .ip_addrs()
            .next()
            .map(|a| a.port())
            .unwrap_or(0);
        let lan_discovery = LanDiscovery::new(
            endpoint.id().to_string(),
            settings.device_name.clone(),
            iroh_port,
            events_tx.clone(),
        );
        let rate_limiter = RateLimiter::new(None);
        let edit_sync = EditSyncManager::new(&config.data_dir, events_tx.clone());
        let initial_clip_seq = platform.clipboard_sequence();
        let pairing = Arc::new(PairingManager::new());

        let node = Arc::new(Self {
            config,
            storage,
            platform,
            endpoint,
            fs_service,
            dir_cache,
            fs_watch,
            lan_discovery,
            rate_limiter,
            edit_sync,
            apps_index: Mutex::new(HashMap::new()),
            active_power_cancel: Mutex::new(None),
            dry_run_power_actions: AtomicBool::new(false),
            last_applied_clip_seq: AtomicU32::new(initial_clip_seq),
            host_paused: AtomicBool::new(false),
            direct_grants: Mutex::new(HashMap::new()),
            pairing,
            events_tx,
            remote_events_tx,
            active_conns: RwLock::new(HashMap::new()),
            dial_locks: Mutex::new(HashMap::new()),
            peer_states: RwLock::new(HashMap::new()),
            peer_workers: Mutex::new(HashMap::new()),
            accept_handle: Mutex::new(None),
            shutdown_token: CancellationToken::new(),
            startup_warnings: warnings.clone(),
        });

        node.log_event("INFO", "核心节点已启动");
        if node.config.network_mode == NetworkMode::PublicOrCustom {
            let _ = node.lan_discovery.start_default();
        }

        for w in warnings {
            let _ = node.events_tx.send(CoreEvent::StorageWarning { message: w });
        }

        // 启动入站连接接收循环
        node.spawn_accept_loop();
        node.spawn_clipboard_autosync_loop();

        // 为所有已配对设备启动持久连接与自动重连任务
        for peer in node.storage.list_peers() {
            node.set_peer_state(
                &peer.endpoint_id,
                DeviceConnState::Offline { reason: None },
            );
            if node.config.auto_connect_peers && peer.connect_outgoing {
                node.ensure_peer_worker(&peer.endpoint_id);
            }
        }

        Ok(node)
    }

    pub fn startup_warnings(&self) -> &[String] {
        &self.startup_warnings
    }

    pub fn storage(&self) -> &Arc<LocalStorage> {
        &self.storage
    }

    pub fn platform(&self) -> &Arc<dyn Platform> {
        &self.platform
    }

    pub fn fs_service(&self) -> &Arc<FsService> {
        &self.fs_service
    }

    pub fn dir_cache(&self) -> &Arc<DirCache> {
        &self.dir_cache
    }

    pub fn fs_watch(&self) -> &Arc<FsWatchManager> {
        &self.fs_watch
    }

    pub fn rate_limiter(&self) -> &Arc<RateLimiter> {
        &self.rate_limiter
    }

    pub fn edit_sync_manager(&self) -> &Arc<EditSyncManager> {
        &self.edit_sync
    }

    pub(crate) fn apps_index_mutex(&self) -> &Mutex<HashMap<String, Vec<InstalledApp>>> {
        &self.apps_index
    }

    pub fn set_dry_run_power_actions(&self, enabled: bool) {
        self.dry_run_power_actions.store(enabled, Ordering::SeqCst);
    }

    pub fn is_dry_run_power_actions(&self) -> bool {
        self.dry_run_power_actions.load(Ordering::SeqCst)
    }

    pub(crate) fn set_active_power_cancel(
        &self,
        tx: Option<tokio::sync::watch::Sender<bool>>,
    ) {
        *self
            .active_power_cancel
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = tx;
    }

    pub(crate) fn trigger_power_cancel(&self) -> bool {
        if let Some(ref tx) = *self
            .active_power_cancel
            .lock()
            .unwrap_or_else(|e| e.into_inner())
        {
            let _ = tx.send(true);
            true
        } else {
            false
        }
    }

    pub(crate) fn record_applied_clipboard_seq(&self, seq: u32) {
        self.last_applied_clip_seq.store(seq, Ordering::SeqCst);
    }

    pub(crate) fn has_active_conn(&self, peer_id: &str) -> bool {
        self.active_conns
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(peer_id)
            .is_some_and(|c| c.close_reason().is_none())
    }

    fn spawn_clipboard_autosync_loop(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        let shutdown = self.shutdown_token.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_millis(900));
            ticker.tick().await;
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = ticker.tick() => {
                        let Some(node) = weak.upgrade() else { break; };
                        if !node.storage.settings().clipboard_auto_sync {
                            continue;
                        }
                        let seq = node.platform.clipboard_sequence();
                        if seq == 0 {
                            continue;
                        }
                        let prev = node.last_applied_clip_seq.swap(seq, Ordering::SeqCst);
                        if prev == 0 || prev == seq {
                            continue;
                        }
                        let online_ids: Vec<String> = node
                            .storage
                            .list_peers()
                            .into_iter()
                            .filter(|p| node.has_active_conn(&p.endpoint_id))
                            .map(|p| p.endpoint_id)
                            .collect();
                        for pid in online_ids {
                            let _ = node.push_local_clipboard_to_peer(&pid).await;
                        }
                    }
                }
            }
        });
    }

    pub fn set_host_paused(&self, paused: bool) {
        self.host_paused.store(paused, Ordering::SeqCst);
        let _ = self
            .events_tx
            .send(CoreEvent::HostPausedChanged { paused });
        self.log_event(
            "INFO",
            if paused {
                "已暂停被控功能"
            } else {
                "已恢复被控功能"
            },
        );
    }

    pub fn is_host_paused(&self) -> bool {
        self.host_paused.load(Ordering::SeqCst)
    }

    pub fn start_lan_discovery_on_port(&self, port: u16) -> Result<(), CoreError> {
        self.lan_discovery.start_on_port(port)
    }

    pub fn discovered_lan_peers(&self) -> Vec<LanPeerAnnouncement> {
        self.lan_discovery.discovered_peers()
    }

    pub fn log_event(&self, level: &str, msg: &str) {
        append_runtime_log(&self.storage.logs_dir(), level, msg);
    }

    pub fn recent_logs(&self, limit: usize) -> Vec<String> {
        read_recent_runtime_logs(&self.storage.logs_dir(), limit)
    }

    pub async fn run_netcheck(&self) -> Result<NetcheckReport, CoreError> {
        let relays = self.storage.settings().custom_relay_urls;
        self.log_event("INFO", "用户触发网络体检");
        let rep = run_netcheck(&relays).await?;
        self.log_event("INFO", &format!("网络体检完成：{}", rep.summary));
        Ok(rep)
    }

    pub async fn export_diagnostics(&self, dest_parent: &Path) -> Result<PathBuf, CoreError> {
        let states = self.all_peer_states();
        let peer_routes = self
            .list_peers()
            .into_iter()
            .map(|p| DiagnosticPeerRoute {
                state: states
                    .get(&p.endpoint_id)
                    .cloned()
                    .unwrap_or(DeviceConnState::Offline { reason: None }),
                peer_id: p.endpoint_id,
                name: p.name,
            })
            .collect();
        let audits = self.storage.read_recent_audits(200).unwrap_or_default();
        let runtime_logs = self.recent_logs(500);
        let settings = self.storage.settings();
        let summary = DiagnosticSummary {
            exported_at_ms: Utc::now().timestamp_millis() as u64,
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            endpoint_id: self.endpoint_id().to_string(),
            device_name: settings.device_name,
            os: std::env::consts::OS.to_string(),
            working_set_bytes: self.platform.current_process_working_set(),
            host_paused: self.is_host_paused(),
            custom_relays: settings.custom_relay_urls,
            peer_routes,
            audit_count: audits.len(),
        };
        let dest_owned = dest_parent.to_path_buf();
        tokio::task::spawn_blocking(move || {
            write_diagnostic_bundle(&dest_owned, &summary, &runtime_logs, &audits)
        })
        .await
        .map_err(|e| CoreError::Internal(format!("导出诊断包任务异常：{e}")))?
    }

    pub(crate) fn insert_direct_grant(&self, token: String, rec: DirectGrantRecord) {
        let mut guard = self.direct_grants.lock().unwrap_or_else(|e| e.into_inner());
        guard.retain(|_, v| Instant::now() < v.expires_at);
        guard.insert(token, rec);
    }

    pub(crate) fn direct_grants_mutex(&self) -> &Mutex<HashMap<String, DirectGrantRecord>> {
        &self.direct_grants
    }

    pub(crate) fn emit_remote_event(&self, peer_id: &str, event: RemoteEvent) {
        let _ = self.remote_events_tx.send((peer_id.to_string(), event));
    }

    pub(crate) fn ensure_peer_worker_public(self: &Arc<Self>, peer_id: &str, conn: Connection) {
        let mut guard = self.peer_workers.lock().unwrap_or_else(|e| e.into_inner());
        if !guard.contains_key(peer_id) {
            let tok = CancellationToken::new();
            guard.insert(peer_id.to_string(), tok.clone());
            drop(guard);
            self.spawn_event_subscriber(peer_id.to_string(), conn, tok);
        }
    }

    pub(crate) fn invalidate_active_conn(&self, peer_id: &str) {
        if let Some(old) = self
            .active_conns
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(peer_id)
        {
            old.close(1u32.into(), b"stale_retry");
        }
    }

    pub fn force_drop_active_conn_for_test(&self, peer_id: &str) {
        if let Some(conn) = self
            .active_conns
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(peer_id)
        {
            conn.close(99u32.into(), b"simulated_drop");
        }
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    pub fn endpoint_id(&self) -> EndpointId {
        self.endpoint.id()
    }

    pub fn local_socket_addrs(&self) -> Vec<SocketAddr> {
        self.endpoint.addr().ip_addrs().copied().collect()
    }

    pub fn home_relay_url(&self) -> Option<String> {
        let mut status = self.endpoint.home_relay_status();
        let list = status.get();
        list.iter()
            .find(|r| r.is_connected())
            .or_else(|| list.first())
            .map(|r| r.url().to_string())
    }

    pub fn subscribe_events(&self) -> broadcast::Receiver<CoreEvent> {
        self.events_tx.subscribe()
    }

    pub fn settings(&self) -> Settings {
        self.storage.settings()
    }

    pub fn update_settings<F>(&self, f: F) -> Result<Settings, CoreError>
    where
        F: FnOnce(&mut Settings),
    {
        let updated = self.storage.update_settings(f)?;
        self.lan_discovery
            .update_device_name(updated.device_name.clone());
        Ok(updated)
    }

    pub fn list_peers(&self) -> Vec<PeerRecord> {
        self.storage.list_peers()
    }

    pub fn peer_state(&self, peer_id: &str) -> Option<DeviceConnState> {
        self.peer_states
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(peer_id)
            .cloned()
    }

    pub fn all_peer_states(&self) -> HashMap<String, DeviceConnState> {
        self.peer_states
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn rename_peer(&self, peer_id: &str, new_name: String) -> Result<bool, CoreError> {
        let updated = self.storage.rename_peer(peer_id, new_name)?;
        if updated {
            let _ = self.events_tx.send(CoreEvent::PeersUpdated);
        }
        Ok(updated)
    }

    pub fn update_peer_permissions(
        &self,
        peer_id: &str,
        permissions: Permissions,
    ) -> Result<bool, CoreError> {
        let updated = self.storage.update_peer_permissions(peer_id, permissions)?;
        if updated {
            let _ = self.events_tx.send(CoreEvent::PeersUpdated);
        }
        Ok(updated)
    }

    pub fn remove_peer(&self, peer_id: &str) -> Result<bool, CoreError> {
        if let Some(cancel) = self
            .peer_workers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(peer_id)
        {
            cancel.cancel();
        }
        if let Some(conn) = self
            .active_conns
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(peer_id)
        {
            conn.close(CLOSE_UNAUTHORIZED.into(), b"removed");
        }
        self.peer_states
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(peer_id);
        let removed = self.storage.remove_peer(peer_id)?;
        if removed {
            let _ = self.events_tx.send(CoreEvent::PeersUpdated);
        }
        Ok(removed)
    }

    pub async fn bench_list_dir_local(&self, dir: &Path) -> Result<usize, CoreError> {
        Ok(self.fs_service.bench_list_local(dir).await?)
    }

    pub async fn shutdown(&self) {
        let _ = self.dir_cache.flush();
        self.edit_sync.shutdown();
        let _ = self.trigger_power_cancel();
        self.fs_watch.shutdown();
        self.lan_discovery.stop();
        self.shutdown_token.cancel();
        let workers: Vec<CancellationToken> = self
            .peer_workers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain()
            .map(|(_, c)| c)
            .collect();
        for w in workers {
            w.cancel();
        }
        let conns: Vec<Connection> = self
            .active_conns
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .drain()
            .map(|(_, c)| c)
            .collect();
        for c in conns {
            c.close(CLOSE_OK.into(), b"shutdown");
        }
        self.endpoint.close().await;
        let handle = self
            .accept_handle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(h) = handle {
            let _ = h.await;
        }
    }

    // ────────────────────────────── 配对接口 ──────────────────────────────

    pub fn start_pairing_session(&self) -> PairingSessionTicket {
        let relay = self.home_relay_url();
        let addrs = self.local_socket_addrs();
        self.pairing
            .start_session(self.endpoint_id(), relay.as_deref(), &addrs)
    }

    pub fn cancel_pairing_session(&self) {
        self.pairing.cancel_session();
    }

    pub fn respond_pairing(
        &self,
        request_id: &str,
        decision: PairingDecision,
    ) -> Result<(), CoreError> {
        self.pairing.respond(request_id, decision)
    }

    pub fn parse_invite(input: &str) -> Result<ParsedInvite, CoreError> {
        parse_invite_or_id(input)
    }

    /// 控制端主动向目标设备发起配对。
    pub async fn pair_with_peer(
        self: &Arc<Self>,
        target_id: EndpointId,
        direct_addrs: Vec<SocketAddr>,
        relay_url: Option<String>,
        code: &str,
        mutual: bool,
        granted_to_responder: Permissions,
    ) -> Result<PeerRecord, CoreError> {
        let addr = build_endpoint_addr(target_id, relay_url.as_deref(), &direct_addrs);
        let conn = timeout(
            Duration::from_secs(20),
            self.endpoint.connect(addr, ALPN_PAIR),
        )
        .await
        .map_err(|_| CoreError::Timeout)?
        .map_err(|e| CoreError::NotConnected(format!("连接配对目标失败：{e}")))?;

        let (mut send, mut recv) = conn
            .open_bi()
            .await
            .map_err(|e| CoreError::NotConnected(format!("打开配对数据流失败：{e}")))?;

        let req = PairRequest {
            v: PROTOCOL_VERSION,
            code: code.trim().to_string(),
            requester_name: self.storage.settings().device_name,
            requester_os: std::env::consts::OS.to_string(),
            requester_relay_url: self.home_relay_url(),
            requester_macs: self.platform.local_mac_addresses(),
            mutual,
            granted_to_responder: granted_to_responder.clone(),
        };
        write_frame(&mut send, &req).await?;
        send.finish()
            .map_err(|e| CoreError::Internal(format!("结束配对发送流失败：{e}")))?;

        // 等待被控电脑前的人点击确认（最长等待 120 秒）
        let resp: PairResponse = timeout(Duration::from_secs(120), read_frame(&mut recv))
            .await
            .map_err(|_| CoreError::Timeout)??;
        let observed_addrs: Vec<SocketAddr> = if direct_addrs.is_empty() {
            conn.paths()
                .iter()
                .filter_map(|p| match p.remote_addr() {
                    TransportAddr::Ip(ip) => Some(*ip),
                    _ => None,
                })
                .collect()
        } else {
            direct_addrs
        };
        conn.close(CLOSE_OK.into(), b"pair_done");

        match resp {
            PairResponse::Approved {
                responder_name,
                responder_os,
                responder_relay_url,
                responder_macs,
                granted_to_requester: _,
                mutual_accepted,
            } => {
                let now_ms = Utc::now().timestamp_millis() as u64;
                let record = PeerRecord {
                    endpoint_id: target_id.to_string(),
                    name: responder_name,
                    os: Some(responder_os),
                    permissions: granted_to_responder,
                    allow_incoming: mutual && mutual_accepted,
                    connect_outgoing: true,
                    relay_url: responder_relay_url.or(relay_url),
                    direct_addrs: observed_addrs,
                    mac_addresses: responder_macs,
                    capabilities: node_capabilities(),
                    paired_at_ms: now_ms,
                    last_seen_ms: Some(now_ms),
                };
                self.storage.upsert_peer(record.clone())?;
                let _ = self.events_tx.send(CoreEvent::PeersUpdated);
                if self.config.auto_connect_peers {
                    self.ensure_peer_worker(&record.endpoint_id);
                }
                Ok(record)
            }
            PairResponse::Rejected { code, msg } => Err(CoreError::Rpc(RpcError::new(code, msg))),
        }
    }

    /// 供安全测试与诊断使用：未配对时直接尝试连接发送 `sys.ping`，验证对方返回 `unauthorized`。
    pub async fn connect_and_ping_direct(
        &self,
        target_id: EndpointId,
        direct_addrs: Vec<SocketAddr>,
    ) -> Result<PingReply, CoreError> {
        let addr = build_endpoint_addr(target_id, None, &direct_addrs);
        let conn = timeout(
            Duration::from_secs(10),
            self.endpoint.connect(addr, ALPN_MAIN),
        )
        .await
        .map_err(|_| CoreError::Timeout)?
        .map_err(|e| CoreError::NotConnected(format!("连接失败：{e}")))?;

        let res = rpc_once::<PingReply>(
            &conn,
            &Command::SysPing(PingArgs {
                seq: 1,
                client_ts_ms: Utc::now().timestamp_millis() as u64,
            }),
            DEFAULT_RPC_TIMEOUT,
        )
        .await;
        conn.close(CLOSE_OK.into(), b"done");
        res
    }

    // ────────────────────────── 控制端远程指令 API ──────────────────────────

    pub async fn remote_ping(self: &Arc<Self>, peer_id: &str) -> Result<(PingReply, Duration), CoreError> {
        let conn = self.get_or_connect(peer_id).await?;
        let start = Instant::now();
        let now_ms = Utc::now().timestamp_millis() as u64;
        let reply: PingReply = rpc_once(
            &conn,
            &Command::SysPing(PingArgs {
                seq: fastrand::u64(..),
                client_ts_ms: now_ms,
            }),
            DEFAULT_RPC_TIMEOUT,
        )
        .await?;
        Ok((reply, start.elapsed()))
    }

    pub(crate) async fn call_rpc<R: DeserializeOwned>(
        self: &Arc<Self>,
        peer_id: &str,
        cmd: Command,
    ) -> Result<R, CoreError> {
        let can_retry = cmd.is_idempotent_read();
        let conn = self.get_or_connect(peer_id).await?;
        match rpc_once(&conn, &cmd, DEFAULT_RPC_TIMEOUT).await {
            Ok(res) => Ok(res),
            Err(CoreError::NotConnected(_) | CoreError::Frame(_) | CoreError::Timeout)
                if can_retry =>
            {
                self.invalidate_active_conn(peer_id);
                let conn2 = self.get_or_connect(peer_id).await?;
                rpc_once(&conn2, &cmd, DEFAULT_RPC_TIMEOUT).await
            }
            Err(err) => Err(err),
        }
    }

    pub async fn remote_info(self: &Arc<Self>, peer_id: &str) -> Result<SysInfoReply, CoreError> {
        self.call_rpc(peer_id, Command::SysInfo).await
    }

    pub async fn remote_roots(self: &Arc<Self>, peer_id: &str) -> Result<FsRootsReply, CoreError> {
        self.call_rpc(peer_id, Command::FsRoots).await
    }

    /// 列出远端目录（自动合并多批返回的数据帧）。
    pub async fn remote_list(
        self: &Arc<Self>,
        peer_id: &str,
        path: &str,
    ) -> Result<Vec<DirEntry>, CoreError> {
        let mut all = Vec::new();
        self.remote_list_batches(peer_id, path, None, |batch| {
            all.extend(batch.entries);
        })
        .await?;
        Ok(all)
    }

    pub async fn remote_list_batches<F>(
        self: &Arc<Self>,
        peer_id: &str,
        path: &str,
        batch_size: Option<usize>,
        mut on_batch: F,
    ) -> Result<(), CoreError>
    where
        F: FnMut(FsListBatch),
    {
        let conn = self.get_or_connect(peer_id).await?;
        let cmd = Command::FsList(FsListArgs {
            path: path.to_string(),
            batch_size,
        });
        let env = cmd.to_envelope()?;

        timeout(DEFAULT_RPC_TIMEOUT, async {
            let (mut send, mut recv) = conn
                .open_bi()
                .await
                .map_err(|e| CoreError::NotConnected(format!("打开数据流失败：{e}")))?;
            write_frame(&mut send, &env).await?;
            send.finish()
                .map_err(|e| CoreError::Internal(format!("结束发送流失败：{e}")))?;

            loop {
                let resp: ResponseEnvelope<FsListBatch> = read_frame(&mut recv).await?;
                let batch = resp.into_result()?;
                let more = batch.has_more;
                on_batch(batch);
                if !more {
                    break;
                }
            }
            Ok::<(), CoreError>(())
        })
        .await
        .map_err(|_| CoreError::Timeout)?
    }

    pub async fn remote_stat(
        self: &Arc<Self>,
        peer_id: &str,
        path: &str,
    ) -> Result<FsStatReply, CoreError> {
        self.call_rpc(
            peer_id,
            Command::FsStat(FsStatArgs {
                path: path.to_string(),
            }),
        )
        .await
    }

    pub async fn remote_watch(
        self: &Arc<Self>,
        peer_id: &str,
        path: &str,
    ) -> Result<FsWatchReply, CoreError> {
        self.call_rpc(
            peer_id,
            Command::FsWatch(FsWatchArgs {
                path: path.to_string(),
            }),
        )
        .await
    }

    pub async fn remote_unwatch(
        self: &Arc<Self>,
        peer_id: &str,
        path: &str,
    ) -> Result<FsUnwatchReply, CoreError> {
        self.call_rpc(
            peer_id,
            Command::FsUnwatch(FsUnwatchArgs {
                path: path.to_string(),
            }),
        )
        .await
    }

    pub async fn remote_mkdir(
        self: &Arc<Self>,
        peer_id: &str,
        args: FsMkdirArgs,
    ) -> Result<FsMkdirReply, CoreError> {
        self.call_rpc(peer_id, Command::FsMkdir(args)).await
    }

    pub async fn remote_rename(
        self: &Arc<Self>,
        peer_id: &str,
        args: FsRenameArgs,
    ) -> Result<FsRenameReply, CoreError> {
        self.call_rpc(peer_id, Command::FsRename(args)).await
    }

    pub async fn remote_copy(
        self: &Arc<Self>,
        peer_id: &str,
        args: FsCopyMoveArgs,
    ) -> Result<FsTaskReply, CoreError> {
        self.call_rpc(peer_id, Command::FsCopy(args)).await
    }

    pub async fn remote_move(
        self: &Arc<Self>,
        peer_id: &str,
        args: FsCopyMoveArgs,
    ) -> Result<FsTaskReply, CoreError> {
        self.call_rpc(peer_id, Command::FsMove(args)).await
    }

    pub async fn remote_task_status(
        self: &Arc<Self>,
        peer_id: &str,
        task_id: &str,
    ) -> Result<FsTaskProgress, CoreError> {
        self.call_rpc(
            peer_id,
            Command::FsTaskStatus(FsTaskIdArgs {
                task_id: task_id.to_string(),
            }),
        )
        .await
    }

    pub async fn remote_task_cancel(
        self: &Arc<Self>,
        peer_id: &str,
        task_id: &str,
    ) -> Result<FsTaskProgress, CoreError> {
        self.call_rpc(
            peer_id,
            Command::FsTaskCancel(FsTaskIdArgs {
                task_id: task_id.to_string(),
            }),
        )
        .await
    }

    /// 等待远端复制/移动任务完成。
    pub async fn wait_remote_task(
        self: &Arc<Self>,
        peer_id: &str,
        task_id: &str,
        max_wait: Duration,
    ) -> Result<FsTaskProgress, CoreError> {
        let deadline = Instant::now() + max_wait;
        loop {
            let status = self.remote_task_status(peer_id, task_id).await?;
            match status.state {
                FsTaskState::Completed | FsTaskState::Cancelled => return Ok(status),
                FsTaskState::Failed => {
                    if let Some(err) = status.error {
                        return Err(CoreError::Rpc(err));
                    }
                    return Err(CoreError::Internal("远端文件任务失败".to_string()));
                }
                FsTaskState::Running => {
                    if Instant::now() >= deadline {
                        return Err(CoreError::Timeout);
                    }
                    tokio::time::sleep(Duration::from_millis(60)).await;
                }
            }
        }
    }

    pub async fn remote_delete(
        self: &Arc<Self>,
        peer_id: &str,
        args: FsDeleteArgs,
    ) -> Result<FsDeleteReply, CoreError> {
        self.call_rpc(peer_id, Command::FsDelete(args)).await
    }

    pub async fn remote_apps(
        self: &Arc<Self>,
        peer_id: &str,
        refresh: bool,
    ) -> Result<AppListReply, CoreError> {
        let rep: AppListReply = self
            .call_rpc(peer_id, Command::AppList(AppListArgs { refresh }))
            .await?;
        self.cache_peer_apps(peer_id, rep.apps.clone());
        Ok(rep)
    }

    pub async fn remote_app_icon(
        self: &Arc<Self>,
        peer_id: &str,
        app_id: &str,
    ) -> Result<AppIconReply, CoreError> {
        self.call_rpc(
            peer_id,
            Command::AppIcon(AppIconArgs {
                app_id: app_id.to_string(),
            }),
        )
        .await
    }

    pub async fn remote_launch(
        self: &Arc<Self>,
        peer_id: &str,
        args: AppLaunchArgs,
    ) -> Result<AppLaunchReply, CoreError> {
        self.call_rpc(peer_id, Command::AppLaunch(args)).await
    }

    pub async fn remote_processes(
        self: &Arc<Self>,
        peer_id: &str,
    ) -> Result<ProcListReply, CoreError> {
        self.call_rpc(peer_id, Command::ProcList).await
    }

    pub async fn remote_kill_process(
        self: &Arc<Self>,
        peer_id: &str,
        args: ProcKillArgs,
    ) -> Result<ProcKillReply, CoreError> {
        self.call_rpc(peer_id, Command::ProcKill(args)).await
    }

    /// 手动立即重连指定已配对设备（供桌面界面“重连”按钮调用）。
    pub async fn reconnect_peer(self: &Arc<Self>, peer_id: &str) -> Result<(), CoreError> {
        let peer = self.storage.get_peer(peer_id).ok_or_else(|| {
            CoreError::Rpc(RpcError::new(ErrorCode::NotFound, "找不到这台已配对设备"))
        })?;
        if let Some(cancel) = self
            .peer_workers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(peer_id)
        {
            cancel.cancel();
        }
        if let Some(conn) = self
            .active_conns
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(peer_id)
        {
            conn.close(CLOSE_OK.into(), b"manual_reconnect");
        }

        self.set_peer_state(peer_id, DeviceConnState::Connecting);
        match self.dial_and_handshake(peer_id).await {
            Ok(conn) => {
                self.set_peer_state(peer_id, inspect_conn_state(&conn));
                if self.config.auto_connect_peers && peer.connect_outgoing {
                    self.ensure_peer_worker(peer_id);
                }
                Ok(())
            }
            Err(err) => {
                self.set_peer_state(
                    peer_id,
                    DeviceConnState::Offline {
                        reason: Some(err.to_user_message()),
                    },
                );
                if self.config.auto_connect_peers && peer.connect_outgoing {
                    self.ensure_peer_worker(peer_id);
                }
                Err(CoreError::NotConnected(format!(
                    "还是连不上“{}”。确认它已开机、程序在运行，并且能上网",
                    peer.name
                )))
            }
        }
    }

    // ────────────────────────── 本机直接操作辅助接口 ──────────────────────────

    pub async fn local_apps(&self, refresh: bool) -> Result<AppListReply, CoreError> {
        let perms = Permissions::default();
        let rep = handle_app_list(&self.platform, &perms, AppListArgs { refresh }).await?;
        self.cache_peer_apps("self", rep.apps.clone());
        Ok(rep)
    }

    pub async fn local_app_icon(&self, app_id: &str) -> Result<AppIconReply, CoreError> {
        let perms = Permissions::default();
        Ok(handle_app_icon(
            &self.platform,
            &perms,
            AppIconArgs {
                app_id: app_id.to_string(),
            },
        )
        .await?)
    }

    pub async fn local_launch(&self, args: AppLaunchArgs) -> Result<AppLaunchReply, CoreError> {
        // 本机操作本机，不受“命令行”权限限制（例如批量操作里带参数打开本机软件）。
        let perms = Permissions {
            terminal: true,
            ..Permissions::default()
        };
        Ok(handle_app_launch(&self.platform, &perms, args).await?)
    }

    pub async fn local_processes(&self) -> Result<ProcListReply, CoreError> {
        let perms = Permissions::default();
        Ok(handle_proc_list(&self.platform, &perms).await?)
    }

    pub async fn local_kill_process(&self, args: ProcKillArgs) -> Result<ProcKillReply, CoreError> {
        let perms = Permissions::default();
        Ok(handle_proc_kill(&self.platform, &perms, args).await?)
    }

    // ────────────────────────── 文件上传与下载（任务 1.8）──────────────────────────

    /// 上传本机单个文件到对方电脑（64 KB 分块、`.part` 临时文件、保留修改时间、支持取消和冲突策略）。
    pub async fn upload_file(
        self: &Arc<Self>,
        peer_id: &str,
        local_path: &Path,
        remote_path: &str,
        conflict: ConflictPolicy,
        cancel: CancellationToken,
        on_progress: Option<ProgressCallback>,
    ) -> Result<FsWriteDone, CoreError> {
        let local_clean = normalize_lexical_absolute(&local_path.to_string_lossy())
            .map_err(CoreError::Rpc)?;
        let local_fs = to_fs_path(&local_clean);
        let mut file = tokio::fs::File::open(&local_fs).await?;
        let meta = file.metadata().await?;
        if !meta.is_file() {
            return Err(CoreError::InvalidArgument(format!(
                "“{}”不是单个普通文件",
                local_path.display()
            )));
        }
        let total_size = meta.len();
        let modified_ms = meta.modified().ok().and_then(system_time_to_ms);

        let conn = self.get_or_connect(peer_id).await?;
        let (mut send, mut recv) = conn
            .open_bi()
            .await
            .map_err(|e| CoreError::NotConnected(format!("打开上传通道失败：{e}")))?;

        let cmd = Command::FsWrite(FsWriteArgs {
            path: remote_path.to_string(),
            size: total_size,
            offset: 0,
            modified_ms,
            conflict,
        });
        write_frame(&mut send, &cmd.to_envelope()?).await?;

        let ready_resp: ResponseEnvelope<FsWriteReady> =
            timeout(DEFAULT_RPC_TIMEOUT, read_frame(&mut recv))
                .await
                .map_err(|_| CoreError::Timeout)??;
        let ready = ready_resp.into_result()?;
        if ready.skipped {
            let _ = send.finish();
            return Ok(FsWriteDone {
                target_path: ready.target_path,
                bytes_written: 0,
            });
        }

        let started = Instant::now();
        let mut last_report = Instant::now();
        let mut sent = 0u64;
        let mut buf = vec![0u8; TRANSFER_CHUNK_SIZE];
        let local_disp = to_display_path(&local_clean);

        while sent < total_size {
            if cancel.is_cancelled() {
                let _ = send.reset(1u32.into());
                return Err(CoreError::Cancelled);
            }
            let want = ((total_size - sent) as usize).min(TRANSFER_CHUNK_SIZE);
            let n = tokio::select! {
                _ = cancel.cancelled() => {
                    let _ = send.reset(1u32.into());
                    return Err(CoreError::Cancelled);
                }
                r = file.read(&mut buf[..want]) => r?,
            };
            if n == 0 {
                let _ = send.reset(1u32.into());
                return Err(CoreError::Internal("读取本地文件时意外提前结束".to_string()));
            }
            tokio::select! {
                _ = cancel.cancelled() => {
                    let _ = send.reset(1u32.into());
                    return Err(CoreError::Cancelled);
                }
                w = send.write_all(&buf[..n]) => {
                    w.map_err(|e| CoreError::NotConnected(format!("上传数据中断：{e}")))?;
                }
            }
            sent += n as u64;

            if last_report.elapsed() >= Duration::from_millis(100) || sent == total_size {
                let speed = sent as f64 / started.elapsed().as_secs_f64().max(0.001);
                if let Some(ref cb) = on_progress {
                    cb(sent, total_size, speed);
                }
                let _ = self.events_tx.send(CoreEvent::TransferProgress(
                    TransferProgressEvent {
                        peer_id: peer_id.to_string(),
                        direction: TransferDirection::Upload,
                        local_path: local_disp.clone(),
                        remote_path: ready.target_path.clone(),
                        transferred_bytes: sent,
                        total_bytes: total_size,
                        bytes_per_sec: speed,
                    },
                ));
                last_report = Instant::now();
            }
        }

        if cancel.is_cancelled() {
            let _ = send.reset(1u32.into());
            return Err(CoreError::Cancelled);
        }

        send.finish()
            .map_err(|e| CoreError::Internal(format!("结束上传流失败：{e}")))?;

        let done_resp: ResponseEnvelope<FsWriteDone> =
            timeout(Duration::from_secs(30), read_frame(&mut recv))
                .await
                .map_err(|_| CoreError::Timeout)??;
        Ok(done_resp.into_result()?)
    }

    /// 从对方电脑下载单个文件到本机（64 KB 分块、同目录 `.part` 临时文件、恢复修改时间、取消自动清理）。
    pub async fn download_file(
        self: &Arc<Self>,
        peer_id: &str,
        remote_path: &str,
        local_dest: &Path,
        conflict: ConflictPolicy,
        cancel: CancellationToken,
        on_progress: Option<ProgressCallback>,
    ) -> Result<FsWriteDone, CoreError> {
        let initial_clean = normalize_lexical_absolute(&local_dest.to_string_lossy())
            .map_err(CoreError::Rpc)?;
        let initial_fs = to_fs_path(&initial_clean);

        let (final_clean, final_fs) = if initial_fs.exists() {
            match conflict {
                ConflictPolicy::Fail => {
                    return Err(CoreError::Rpc(RpcError::new(
                        ErrorCode::AlreadyExists,
                        format!("本地目标文件已存在：{}", to_display_path(&initial_clean)),
                    )));
                }
                ConflictPolicy::Skip => {
                    return Ok(FsWriteDone {
                        target_path: to_display_path(&initial_clean),
                        bytes_written: 0,
                    });
                }
                ConflictPolicy::Overwrite => (initial_clean, initial_fs),
                ConflictPolicy::RenameKeepBoth => pick_unique_target(&initial_clean),
            }
        } else {
            (initial_clean, initial_fs)
        };

        let parent_clean = final_clean.parent().ok_or_else(|| {
            CoreError::InvalidArgument("无效的本地保存路径".to_string())
        })?;
        tokio::fs::create_dir_all(to_fs_path(parent_clean)).await?;

        let conn = self.get_or_connect(peer_id).await?;
        let (mut send, mut recv) = conn
            .open_bi()
            .await
            .map_err(|e| CoreError::NotConnected(format!("打开下载通道失败：{e}")))?;

        let cmd = Command::FsRead(FsReadArgs {
            path: remote_path.to_string(),
            offset: 0,
            length: None,
        });
        write_frame(&mut send, &cmd.to_envelope()?).await?;
        send.finish()
            .map_err(|e| CoreError::Internal(format!("结束下载请求流失败：{e}")))?;

        let header_resp: ResponseEnvelope<FsReadHeader> =
            timeout(DEFAULT_RPC_TIMEOUT, read_frame(&mut recv))
                .await
                .map_err(|_| CoreError::Timeout)??;
        let header = header_resp.into_result()?;

        let file_name = final_clean
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        let part_clean =
            parent_clean.join(format!(".{file_name}.pcshare-{:08x}.part", fastrand::u32(..)));
        let part_fs = to_fs_path(&part_clean);

        let mut part_file = tokio::fs::File::create(&part_fs).await?;
        struct LocalPartGuard {
            path: PathBuf,
            committed: bool,
        }
        impl Drop for LocalPartGuard {
            fn drop(&mut self) {
                if !self.committed {
                    let _ = fs::remove_file(&self.path);
                }
            }
        }
        let mut part_guard = LocalPartGuard {
            path: part_fs.clone(),
            committed: false,
        };

        let started = Instant::now();
        let mut last_report = Instant::now();
        let mut received = 0u64;
        let total = header.transfer_length;
        let mut buf = vec![0u8; TRANSFER_CHUNK_SIZE];
        let final_disp = to_display_path(&final_clean);

        while received < total {
            if cancel.is_cancelled() {
                let _ = recv.stop(1u32.into());
                return Err(CoreError::Cancelled);
            }
            let want = ((total - received) as usize).min(TRANSFER_CHUNK_SIZE);
            let chunk_opt = tokio::select! {
                _ = cancel.cancelled() => {
                    let _ = recv.stop(1u32.into());
                    return Err(CoreError::Cancelled);
                }
                r = recv.read(&mut buf[..want]) => {
                    r.map_err(|e| CoreError::NotConnected(format!("下载数据流中断：{e}")))?
                }
            };
            let Some(n) = chunk_opt else {
                return Err(CoreError::NotConnected(
                    "下载尚未完成但对方提前结束了连接".to_string(),
                ));
            };
            part_file.write_all(&buf[..n]).await?;
            received += n as u64;

            if last_report.elapsed() >= Duration::from_millis(100) || received == total {
                let speed = received as f64 / started.elapsed().as_secs_f64().max(0.001);
                if let Some(ref cb) = on_progress {
                    cb(received, total, speed);
                }
                let _ = self.events_tx.send(CoreEvent::TransferProgress(
                    TransferProgressEvent {
                        peer_id: peer_id.to_string(),
                        direction: TransferDirection::Download,
                        local_path: final_disp.clone(),
                        remote_path: remote_path.to_string(),
                        transferred_bytes: received,
                        total_bytes: total,
                        bytes_per_sec: speed,
                    },
                ));
                last_report = Instant::now();
            }
        }

        part_file.flush().await?;
        part_file.sync_all().await?;
        let std_file = part_file.into_std().await;
        if let Some(ms) = header.modified_ms {
            let _ = std_file.set_modified(UNIX_EPOCH + Duration::from_millis(ms));
        }
        drop(std_file);

        let part_fs_c = part_fs.clone();
        let final_fs_c = final_fs.clone();
        let final_disp_c = final_disp.clone();
        tokio::task::spawn_blocking(move || {
            replace_file_atomically(&part_fs_c, &final_fs_c)
                .map_err(|e| map_io_to_rpc(&e, Some(&final_disp_c)))
        })
        .await
        .map_err(|e| CoreError::Internal(format!("下载改名异常：{e}")))??;

        part_guard.committed = true;
        Ok(FsWriteDone {
            target_path: final_disp,
            bytes_written: received,
        })
    }

    // ────────────────────────── 内部连接管理与服务端循环 ──────────────────────────

    fn set_peer_state(&self, peer_id: &str, state: DeviceConnState) {
        let changed = {
            let mut guard = self.peer_states.write().unwrap_or_else(|e| e.into_inner());
            let prev = guard.insert(peer_id.to_string(), state.clone());
            prev.as_ref() != Some(&state)
        };
        if changed {
            let _ = self.events_tx.send(CoreEvent::DeviceStateChanged {
                peer_id: peer_id.to_string(),
                state,
            });
        }
    }

    fn spawn_accept_loop(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        let endpoint = self.endpoint.clone();
        let shutdown = self.shutdown_token.clone();
        let handle = tokio::spawn(async move {
            loop {
                let incoming_opt = tokio::select! {
                    _ = shutdown.cancelled() => break,
                    inc = endpoint.accept() => inc,
                };
                let Some(incoming) = incoming_opt else {
                    break;
                };
                let Some(node) = weak.upgrade() else {
                    break;
                };
                tokio::spawn(async move {
                    if let Ok(conn) = incoming.await {
                        if conn.alpn() == ALPN_PAIR {
                            node.handle_incoming_pair(conn).await;
                        } else if conn.alpn() == ALPN_DIRECT {
                            node.handle_incoming_direct(conn).await;
                        } else {
                            node.handle_incoming_main(conn).await;
                        }
                    }
                });
            }
        });
        *self
            .accept_handle
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(handle);
    }

    async fn handle_incoming_pair(self: Arc<Self>, conn: Connection) {
        let peer_id = conn.remote_id();
        let Ok(Ok((mut send, mut recv))) =
            timeout(Duration::from_secs(10), conn.accept_bi()).await
        else {
            conn.close(CLOSE_UNAUTHORIZED.into(), b"pair_timeout");
            return;
        };

        let Ok(req) = read_frame::<_, PairRequest>(&mut recv).await else {
            conn.close(CLOSE_UNAUTHORIZED.into(), b"bad_pair_req");
            return;
        };

        if req.v != PROTOCOL_VERSION {
            let _ = write_frame(
                &mut send,
                &PairResponse::Rejected {
                    code: ErrorCode::Unsupported,
                    msg: format!("配对协议版本不兼容（对方 v{}，本机 v{}）", req.v, PROTOCOL_VERSION),
                },
            )
            .await;
            let _ = send.finish();
            let _ = timeout(Duration::from_secs(2), send.stopped()).await;
            conn.close(CLOSE_UNAUTHORIZED.into(), b"unsupported");
            return;
        }

        let (prompt, decision_rx) = match self.pairing.verify_code_and_prepare_approval(
            peer_id,
            req.requester_name.clone(),
            req.requester_os.clone(),
            req.mutual,
            &req.code,
        ) {
            Ok(pair) => pair,
            Err(rpc_err) => {
                let _ = write_frame(
                    &mut send,
                    &PairResponse::Rejected {
                        code: rpc_err.code,
                        msg: rpc_err.msg,
                    },
                )
                .await;
                let _ = send.finish();
                let _ = timeout(Duration::from_secs(2), send.stopped()).await;
                conn.close(CLOSE_UNAUTHORIZED.into(), b"invalid_code");
                return;
            }
        };

        let _ = self
            .events_tx
            .send(CoreEvent::PairingApprovalNeeded(prompt));

        let decision = match timeout(Duration::from_secs(120), decision_rx).await {
            Ok(Ok(d)) => d,
            _ => PairingDecision::Reject,
        };

        match decision {
            PairingDecision::Reject => {
                let _ = write_frame(
                    &mut send,
                    &PairResponse::Rejected {
                        code: ErrorCode::Forbidden,
                        msg: "对方拒绝了本次配对请求（或等待确认超时）".to_string(),
                    },
                )
                .await;
                let _ = send.finish();
                let _ = timeout(Duration::from_secs(2), send.stopped()).await;
                conn.close(CLOSE_UNAUTHORIZED.into(), b"rejected");
            }
            PairingDecision::Approve {
                granted_permissions,
                accept_mutual,
            } => {
                self.pairing.consume_session_on_approved();
                let now_ms = Utc::now().timestamp_millis() as u64;
                let direct_addrs: Vec<SocketAddr> = conn
                    .paths()
                    .iter()
                    .filter_map(|p| match p.remote_addr() {
                        TransportAddr::Ip(ip) => Some(*ip),
                        _ => None,
                    })
                    .collect();

                let connect_outgoing = req.mutual && accept_mutual;
                let peer_record = PeerRecord {
                    endpoint_id: peer_id.to_string(),
                    name: req.requester_name,
                    os: Some(req.requester_os),
                    permissions: granted_permissions.clone(),
                    allow_incoming: true,
                    connect_outgoing,
                    relay_url: req.requester_relay_url,
                    direct_addrs,
                    mac_addresses: req.requester_macs,
                    capabilities: node_capabilities(),
                    paired_at_ms: now_ms,
                    last_seen_ms: Some(now_ms),
                };
                let _ = self.storage.upsert_peer(peer_record.clone());
                let _ = self.events_tx.send(CoreEvent::PeersUpdated);

                let resp = PairResponse::Approved {
                    responder_name: self.storage.settings().device_name,
                    responder_os: std::env::consts::OS.to_string(),
                    responder_relay_url: self.home_relay_url(),
                    responder_macs: self.platform.local_mac_addresses(),
                    granted_to_requester: granted_permissions,
                    mutual_accepted: connect_outgoing,
                };
                let _ = write_frame(&mut send, &resp).await;
                let _ = send.finish();
                let _ = timeout(Duration::from_secs(2), send.stopped()).await;
                if connect_outgoing && self.config.auto_connect_peers {
                    self.ensure_peer_worker(&peer_record.endpoint_id);
                }
            }
        }
    }

    async fn handle_incoming_main(self: Arc<Self>, conn: Connection) {
        let peer_id_str = conn.remote_id().to_string();
        let authorized = self
            .storage
            .get_peer(&peer_id_str)
            .is_some_and(|p| p.allow_incoming);

        if !authorized {
            // 未配对拒绝时先回明确原因再断开（见第三册第 6 节第 9 条）
            if let Ok(Ok((mut send, _recv))) =
                timeout(Duration::from_secs(5), conn.accept_bi()).await
            {
                let resp: ResponseEnvelope<()> = ResponseEnvelope::err(
                    ErrorCode::Unauthorized,
                    "这台设备尚未与对方配对，或对方已取消授权",
                );
                let _ = write_frame(&mut send, &resp).await;
                let _ = send.finish();
                let _ = timeout(Duration::from_secs(2), send.stopped()).await;
            }
            conn.close(CLOSE_UNAUTHORIZED.into(), b"unauthorized");
            return;
        }

        let _ = self.storage.touch_peer_seen(
            &peer_id_str,
            Utc::now().timestamp_millis() as u64,
            None,
        );

        while let Ok((send, recv)) = conn.accept_bi().await {
            let node = self.clone();
            let pid = peer_id_str.clone();
            tokio::spawn(async move {
                let _ = node.handle_command_stream(&pid, send, recv).await;
            });
        }
        self.fs_watch.unwatch_all_for_peer(&peer_id_str);
    }

    async fn handle_command_stream(
        self: &Arc<Self>,
        peer_id: &str,
        mut send: SendStream,
        mut recv: RecvStream,
    ) -> Result<(), CoreError> {
        let Some(peer_rec) = self.storage.get_peer(peer_id) else {
            write_frame(
                &mut send,
                &ResponseEnvelope::<()>::err(ErrorCode::Unauthorized, "设备配对已解除"),
            )
            .await?;
            let _ = send.finish();
            return Ok(());
        };
        let perms = peer_rec.permissions;

        let env: RequestEnvelope = read_frame(&mut recv).await?;
        let raw_cmd_name = env.cmd.clone();
        let cmd = match Command::from_envelope(env) {
            Ok(c) => c,
            Err(rpc_err) => {
                write_frame(&mut send, &ResponseEnvelope::<()>::Err(rpc_err)).await?;
                let _ = send.finish();
                return Ok(());
            }
        };

        // 任务 2.7：被控端开启“暂停被控”时，保留握手、心跳与事件通道，拒绝实际受控指令
        if self.is_host_paused()
            && !matches!(
                cmd,
                Command::SysHello(_) | Command::SysPing(_) | Command::SubEvents
            )
        {
            write_frame(
                &mut send,
                &ResponseEnvelope::<()>::err(
                    ErrorCode::Busy,
                    "对方这台电脑当前开启了“暂停被控”，恢复后才能操作",
                ),
            )
            .await?;
            let _ = send.finish();
            return Ok(());
        }

        let required = cmd.required_permission();
        if !perms.allows(required) {
            self.record_audit(peer_id, &peer_rec.name, &raw_cmd_name, "权限不足", false);
            write_frame(
                &mut send,
                &ResponseEnvelope::<()>::err(ErrorCode::Forbidden, "对方没有授予执行此操作的权限"),
            )
            .await?;
            let _ = send.finish();
            return Ok(());
        }

        match cmd {
            Command::SysHello(args) => {
                let _ = self.storage.update_peer_hello_meta(
                    peer_id,
                    args.capabilities,
                    args.mac_addresses,
                );
                let reply = HelloReply {
                    protocol_version: PROTOCOL_VERSION,
                    app_version: env!("CARGO_PKG_VERSION").to_string(),
                    device_name: self.storage.settings().device_name,
                    os: std::env::consts::OS.to_string(),
                    capabilities: node_capabilities(),
                    mac_addresses: self.platform.local_mac_addresses(),
                    granted_permissions: perms,
                };
                write_frame(&mut send, &ResponseEnvelope::ok(reply)).await?;
            }
            Command::SysPing(args) => {
                let reply = PingReply {
                    seq: args.seq,
                    client_ts_ms: args.client_ts_ms,
                    server_ts_ms: Utc::now().timestamp_millis() as u64,
                };
                write_frame(&mut send, &ResponseEnvelope::ok(reply)).await?;
            }
            Command::SysInfo => {
                let (computer_name, user_name) = self.platform.host_and_user();
                let desktop = to_proto_desktop(self.platform.desktop_environment());
                let disks = self
                    .platform
                    .list_drives()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|d| DiskSpaceInfo {
                        mount_point: d.path,
                        label: d.label,
                        total_bytes: d.total_bytes,
                        available_bytes: d.available_bytes,
                    })
                    .collect();
                let mem = self
                    .platform
                    .memory_info()
                    .unwrap_or(pcshare_platform::SystemMemory {
                        total_bytes: 0,
                        available_bytes: 0,
                    });
                let reply = SysInfoReply {
                    computer_name,
                    user_name,
                    os: std::env::consts::OS.to_string(),
                    app_version: env!("CARGO_PKG_VERSION").to_string(),
                    desktop,
                    disks,
                    memory: MemoryInfo {
                        total_bytes: mem.total_bytes,
                        available_bytes: mem.available_bytes,
                    },
                };
                write_frame(&mut send, &ResponseEnvelope::ok(reply)).await?;
            }
            Command::FsRoots => {
                let res = self.fs_service.roots(&perms);
                write_rpc_result(&mut send, res).await?;
            }
            Command::FsList(args) => {
                self.record_audit(peer_id, &peer_rec.name, "fs.list", &args.path, true);
                self.fs_service
                    .list_to_stream(&perms, args, &mut send)
                    .await?;
            }
            Command::FsStat(args) => {
                let res = self.fs_service.stat(&perms, args).await;
                write_rpc_result(&mut send, res).await?;
            }
            Command::FsMkdir(args) => {
                let detail = args.path.clone();
                let res = self.fs_service.mkdir(&perms, args).await;
                self.record_audit(peer_id, &peer_rec.name, "fs.mkdir", &detail, res.is_ok());
                write_rpc_result(&mut send, res).await?;
            }
            Command::FsRename(args) => {
                let detail = format!("{} -> {}", args.path, args.new_name);
                let res = self.fs_service.rename(&perms, args).await;
                self.record_audit(peer_id, &peer_rec.name, "fs.rename", &detail, res.is_ok());
                write_rpc_result(&mut send, res).await?;
            }
            Command::FsCopy(args) => {
                let detail = args.dest_dir.clone();
                let res = self
                    .fs_service
                    .start_copy_or_move(peer_id, FsTaskKind::Copy, &perms, args);
                self.record_audit(peer_id, &peer_rec.name, "fs.copy", &detail, res.is_ok());
                write_rpc_result(&mut send, res).await?;
            }
            Command::FsMove(args) => {
                let detail = args.dest_dir.clone();
                let res = self
                    .fs_service
                    .start_copy_or_move(peer_id, FsTaskKind::Move, &perms, args);
                self.record_audit(peer_id, &peer_rec.name, "fs.move", &detail, res.is_ok());
                write_rpc_result(&mut send, res).await?;
            }
            Command::FsDelete(args) => {
                let detail = args.paths.join(", ");
                let res = self.fs_service.delete(&perms, args).await;
                self.record_audit(peer_id, &peer_rec.name, "fs.delete", &detail, res.is_ok());
                write_rpc_result(&mut send, res).await?;
            }
            Command::FsTaskStatus(args) => {
                let res = self.fs_service.task_status(&args.task_id);
                write_rpc_result(&mut send, res).await?;
            }
            Command::FsTaskCancel(args) => {
                let res = self.fs_service.task_cancel(&args.task_id);
                write_rpc_result(&mut send, res).await?;
            }
            Command::FsRead(args) => {
                let detail = args.path.clone();
                self.record_audit(peer_id, &peer_rec.name, "fs.read", &detail, true);
                self.fs_service
                    .handle_read_stream(&perms, args, &mut send)
                    .await?;
            }
            Command::FsWrite(args) => {
                let detail = args.path.clone();
                let res = self
                    .fs_service
                    .handle_write_stream(&perms, args, &mut recv, &mut send)
                    .await;
                self.record_audit(peer_id, &peer_rec.name, "fs.write", &detail, res.is_ok());
                res?;
            }
            Command::FsHash(args) => {
                let res = self.fs_service.hash(&perms, args).await;
                write_rpc_result(&mut send, res).await?;
            }
            Command::FsWatch(args) => {
                let res = self.fs_watch.watch(peer_id, &perms, &args.path);
                write_rpc_result(&mut send, res).await?;
            }
            Command::FsUnwatch(args) => {
                let res = self.fs_watch.unwatch(peer_id, &perms, &args.path);
                write_rpc_result(&mut send, res).await?;
            }
            Command::FsProbeResume(args) => {
                let res = self.fs_service.probe_resume(&perms, args).await;
                write_rpc_result(&mut send, res).await?;
            }
            Command::FsWalk(args) => {
                self.fs_service
                    .walk_to_stream(&perms, args, &mut send)
                    .await?;
            }
            Command::FsWriteBatch(args) => {
                let detail = args.dest_dir.clone();
                let res = self
                    .fs_service
                    .handle_write_batch_stream(&perms, args, &mut recv, &mut send)
                    .await;
                self.record_audit(
                    peer_id,
                    &peer_rec.name,
                    "fs.write_batch",
                    &detail,
                    res.is_ok(),
                );
                res?;
            }
            Command::FsReadBatch(args) => {
                self.fs_service
                    .handle_read_batch_stream(&perms, args, &mut send)
                    .await?;
            }
            Command::PeerGrantDirect(args) => {
                let detail = args.dest_dir.clone();
                let res = self.handle_grant_direct(&perms, args);
                self.record_audit(
                    peer_id,
                    &peer_rec.name,
                    "peer.grant_direct",
                    &detail,
                    res.is_ok(),
                );
                write_rpc_result(&mut send, res).await?;
            }
            Command::PeerDirectTransfer(args) => {
                let detail = format!("-> {}", args.target_peer_id);
                let res = self.spawn_outgoing_direct_transfer(peer_id, &perms, args);
                self.record_audit(
                    peer_id,
                    &peer_rec.name,
                    "peer.direct_transfer",
                    &detail,
                    res.is_ok(),
                );
                write_rpc_result(&mut send, res).await?;
            }
            Command::AppList(args) => {
                let res = handle_app_list(&self.platform, &perms, args).await;
                write_rpc_result(&mut send, res).await?;
            }
            Command::AppIcon(args) => {
                let res = handle_app_icon(&self.platform, &perms, args).await;
                write_rpc_result(&mut send, res).await?;
            }
            Command::AppLaunch(args) => {
                let detail = args.target.clone();
                let res = handle_app_launch(&self.platform, &perms, args).await;
                self.record_audit(
                    peer_id,
                    &peer_rec.name,
                    "app.launch",
                    &detail,
                    res.is_ok(),
                );
                write_rpc_result(&mut send, res).await?;
            }
            Command::ProcList => {
                let res = handle_proc_list(&self.platform, &perms).await;
                write_rpc_result(&mut send, res).await?;
            }
            Command::ProcKill(args) => {
                let detail = format!("PID {}", args.pid);
                let res = handle_proc_kill(&self.platform, &perms, args).await;
                self.record_audit(
                    peer_id,
                    &peer_rec.name,
                    "proc.kill",
                    &detail,
                    res.is_ok(),
                );
                write_rpc_result(&mut send, res).await?;
            }
            Command::SubEvents => {
                // 长期订阅流：先回复就绪，再持续推送属于该连接对端的事件
                write_frame(&mut send, &ResponseEnvelope::ok(())).await?;
                let mut rx = self.remote_events_tx.subscribe();
                loop {
                    tokio::select! {
                        _ = send.stopped() => break,
                        msg = rx.recv() => {
                            match msg {
                                Ok((target_peer, ev)) if target_peer == peer_id => {
                                    if write_frame(&mut send, &ev).await.is_err() {
                                        break;
                                    }
                                }
                                Ok(_) => {}
                                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                                Err(broadcast::error::RecvError::Closed) => break,
                            }
                        }
                    }
                }
            }
            Command::FsSearch(args) => {
                self.record_audit(peer_id, &peer_rec.name, "fs.search", &args.query, true);
                self.fs_service
                    .search_to_stream(&perms, args, &mut send)
                    .await?;
            }
            Command::ShotMonitors => {
                let res = handle_shot_monitors(&self.platform, &perms).await;
                write_rpc_result(&mut send, res).await?;
            }
            Command::ShotScreen(args) => {
                self.record_audit(peer_id, &peer_rec.name, "shot.screen", "全屏截图", true);
                handle_shot_screen_stream(&self.platform, &perms, args, &mut send).await?;
            }
            Command::ShotWindow(args) => {
                let detail = format!("HWND {}", args.hwnd);
                self.record_audit(peer_id, &peer_rec.name, "shot.window", &detail, true);
                handle_shot_window_stream(&self.platform, &perms, args, &mut send).await?;
            }
            Command::InputClick(args) => {
                let detail = format!("({}, {})", args.screen_x, args.screen_y);
                self.record_audit(peer_id, &peer_rec.name, "input.click", &detail, true);
                handle_input_click_stream(&self.platform, &perms, args, &mut send).await?;
            }
            Command::InputKey(args) => {
                let detail = args
                    .key_combo
                    .clone()
                    .unwrap_or_else(|| "文本输入".to_string());
                self.record_audit(peer_id, &peer_rec.name, "input.key", &detail, true);
                handle_input_key_stream(&self.platform, &perms, args, &mut send).await?;
            }
            Command::PowerExec(args) => {
                let detail = format!("{:?}", args.action);
                let res = self.handle_power_exec(&perms, args).await;
                self.record_audit(peer_id, &peer_rec.name, "power.exec", &detail, res.is_ok());
                write_rpc_result(&mut send, res).await?;
            }
            Command::PowerCancel => {
                let res = self.handle_power_cancel(&perms);
                self.record_audit(peer_id, &peer_rec.name, "power.cancel", "", res.is_ok());
                write_rpc_result(&mut send, res).await?;
            }
            Command::PowerWakeWol(args) => {
                let detail = args.mac_addresses.join(", ");
                let res = self.handle_power_wake_wol(args);
                self.record_audit(peer_id, &peer_rec.name, "power.wake_wol", &detail, res.is_ok());
                write_rpc_result(&mut send, res).await?;
            }
            Command::ClipGet => {
                self.record_audit(peer_id, &peer_rec.name, "clip.get", "", true);
                handle_clip_get_stream(&self.platform, &perms, &mut send).await?;
            }
            Command::ClipSet(args) => {
                let detail = format!("{:?}", args.kind);
                let res = handle_clip_set_stream(&self.platform, &perms, args, &mut recv, &mut send)
                    .await;
                self.record_audit(peer_id, &peer_rec.name, "clip.set", &detail, res.is_ok());
                res?;
            }
            Command::TermOpen(args) => {
                let detail = format!("{:?}", args.shell);
                self.record_audit(peer_id, &peer_rec.name, "term.open", &detail, true);
                handle_term_open_stream(&self.platform, &perms, args, &mut send, &mut recv)
                    .await?;
            }
        }

        let _ = send.finish();
        Ok(())
    }

    fn record_audit(&self, peer_id: &str, peer_name: &str, action: &str, detail: &str, ok: bool) {
        let _ = self.storage.append_audit(AuditEntry {
            timestamp_ms: Utc::now().timestamp_millis() as u64,
            peer_id: peer_id.to_string(),
            peer_name: peer_name.to_string(),
            action: action.to_string(),
            detail: detail.to_string(),
            ok,
        });
    }

    fn ensure_peer_worker(self: &Arc<Self>, peer_id: &str) {
        let mut guard = self.peer_workers.lock().unwrap_or_else(|e| e.into_inner());
        if guard.contains_key(peer_id) {
            return;
        }
        let token = CancellationToken::new();
        guard.insert(peer_id.to_string(), token.clone());
        drop(guard);

        let weak = Arc::downgrade(self);
        let peer_owned = peer_id.to_string();
        tokio::spawn(async move {
            let mut backoff_secs = 1u64;
            while !token.is_cancelled() {
                let Some(node) = weak.upgrade() else {
                    break;
                };
                node.set_peer_state(&peer_owned, DeviceConnState::Connecting);

                match node.dial_and_handshake(&peer_owned).await {
                    Ok(conn) => {
                        backoff_secs = 1;
                        node.set_peer_state(&peer_owned, inspect_conn_state(&conn));
                        node.spawn_event_subscriber(peer_owned.clone(), conn.clone(), token.clone());
                        node.monitor_connection(&peer_owned, &conn, &token).await;
                        node.active_conns
                            .write()
                            .unwrap_or_else(|e| e.into_inner())
                            .remove(&peer_owned);
                        if !token.is_cancelled() {
                            node.set_peer_state(
                                &peer_owned,
                                DeviceConnState::Offline {
                                    reason: Some("连接已断开，正在重连".to_string()),
                                },
                            );
                        }
                    }
                    Err(CoreError::Rpc(err)) if err.code == ErrorCode::Unauthorized => {
                        node.set_peer_state(
                            &peer_owned,
                            DeviceConnState::Offline {
                                reason: Some(err.msg),
                            },
                        );
                        // 未授权时拉长重试间隔，避免频繁打扰对端
                        backoff_secs = 30;
                    }
                    Err(err) => {
                        node.set_peer_state(
                            &peer_owned,
                            DeviceConnState::Offline {
                                reason: Some(err.to_user_message()),
                            },
                        );
                    }
                }
                drop(node);

                if token.is_cancelled() {
                    break;
                }
                tokio::select! {
                    _ = token.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_secs(backoff_secs)) => {}
                }
                backoff_secs = (backoff_secs * 2).min(30);
            }
        });
    }

    fn spawn_event_subscriber(
        self: &Arc<Self>,
        peer_id: String,
        conn: Connection,
        cancel: CancellationToken,
    ) {
        let events_tx = self.events_tx.clone();
        tokio::spawn(async move {
            let Ok((mut send, mut recv)) = conn.open_bi().await else {
                return;
            };
            let Ok(env) = Command::SubEvents.to_envelope() else {
                return;
            };
            if write_frame(&mut send, &env).await.is_err() {
                return;
            }
            let Ok(ResponseEnvelope::Ok(())) =
                read_frame::<_, ResponseEnvelope<()>>(&mut recv).await
            else {
                return;
            };
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    frame = read_optional_frame::<_, RemoteEvent>(&mut recv) => {
                        match frame {
                            Ok(Some(RemoteEvent::TaskProgress(progress))) => {
                                let _ = events_tx.send(CoreEvent::RemoteTaskProgress {
                                    peer_id: peer_id.clone(),
                                    progress,
                                });
                            }
                            Ok(Some(RemoteEvent::DirChanged { path })) => {
                                let _ = events_tx.send(CoreEvent::RemoteDirChanged {
                                    peer_id: peer_id.clone(),
                                    path,
                                });
                            }
                            Ok(Some(RemoteEvent::DirectTransferProgress(progress))) => {
                                let _ = events_tx.send(CoreEvent::RemoteDirectTransferProgress {
                                    peer_id: peer_id.clone(),
                                    progress,
                                });
                            }
                            Ok(None) | Err(_) => break,
                        }
                    }
                }
            }
        });
    }

    async fn monitor_connection(
        &self,
        peer_id: &str,
        conn: &Connection,
        cancel: &CancellationToken,
    ) {
        let ping_interval = match self.config.network_mode {
            NetworkMode::LocalTest => Duration::from_millis(800),
            NetworkMode::PublicOrCustom => Duration::from_secs(5),
        };
        let mut ticker = tokio::time::interval(ping_interval);
        ticker.tick().await;

        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = conn.closed() => break,
                _ = ticker.tick() => {
                    let ping_res = rpc_once::<PingReply>(
                        conn,
                        &Command::SysPing(PingArgs {
                            seq: fastrand::u64(..),
                            client_ts_ms: Utc::now().timestamp_millis() as u64,
                        }),
                        Duration::from_millis(1800),
                    ).await;
                    if ping_res.is_err() {
                        conn.close(1u32.into(), b"ping_timeout");
                        break;
                    }
                    self.set_peer_state(peer_id, inspect_conn_state(conn));
                    let _ = self.storage.touch_peer_seen(
                        peer_id,
                        Utc::now().timestamp_millis() as u64,
                        None,
                    );
                }
            }
        }
    }

    pub(crate) async fn get_or_connect(
        self: &Arc<Self>,
        peer_id: &str,
    ) -> Result<Connection, CoreError> {
        {
            let guard = self.active_conns.read().unwrap_or_else(|e| e.into_inner());
            if let Some(conn) = guard.get(peer_id)
                && conn.close_reason().is_none()
            {
                return Ok(conn.clone());
            }
        }
        self.dial_and_handshake(peer_id).await
    }

    async fn dial_and_handshake(self: &Arc<Self>, peer_id: &str) -> Result<Connection, CoreError> {
        let peer_lock = {
            let mut map = self.dial_locks.lock().unwrap_or_else(|e| e.into_inner());
            map.entry(peer_id.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        let _dial_guard = peer_lock.lock().await;
        {
            let guard = self.active_conns.read().unwrap_or_else(|e| e.into_inner());
            if let Some(conn) = guard.get(peer_id)
                && conn.close_reason().is_none()
            {
                return Ok(conn.clone());
            }
        }

        let peer = self.storage.get_peer(peer_id).ok_or_else(|| {
            CoreError::InvalidArgument(format!("设备尚未配对：{peer_id}"))
        })?;
        let target_id: EndpointId = peer
            .endpoint_id
            .parse()
            .map_err(|_| CoreError::InvalidArgument(format!("身份码无效：{peer_id}")))?;

        let addr = build_endpoint_addr(target_id, peer.relay_url.as_deref(), &peer.direct_addrs);
        let connect_timeout = match self.config.network_mode {
            NetworkMode::LocalTest => Duration::from_secs(2),
            NetworkMode::PublicOrCustom => Duration::from_secs(15),
        };
        let conn = timeout(connect_timeout, self.endpoint.connect(addr, ALPN_MAIN))
            .await
            .map_err(|_| CoreError::Timeout)?
            .map_err(|e| CoreError::NotConnected(format!("无法连接设备“{}”：{e}", peer.name)))?;

        // 连接建立后先交换 sys.hello（见总体方案 6.5 节）
        let hello = Command::SysHello(HelloArgs {
            protocol_version: PROTOCOL_VERSION,
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            device_name: self.storage.settings().device_name,
            os: std::env::consts::OS.to_string(),
            capabilities: node_capabilities(),
            mac_addresses: self.platform.local_mac_addresses(),
        });
        let reply: HelloReply = match rpc_once(&conn, &hello, DEFAULT_RPC_TIMEOUT).await {
            Ok(r) => r,
            Err(err) => {
                conn.close(CLOSE_OK.into(), b"hello_failed");
                return Err(err);
            }
        };
        let _ = self.storage.update_peer_hello_meta(
            peer_id,
            reply.capabilities,
            reply.mac_addresses,
        );

        let mut guard = self.active_conns.write().unwrap_or_else(|e| e.into_inner());
        guard.insert(peer_id.to_string(), conn.clone());
        drop(guard);

        // Pre-warm remote app index in background for <= 300ms Quick Search Launcher (Task 3.1)
        if reply.granted_permissions.app_launch {
            let need_warm = !self
                .apps_index
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key(peer_id);
            if need_warm {
                let node_bg = self.clone();
                let pid_bg = peer_id.to_string();
                let conn_bg = conn.clone();
                tokio::spawn(async move {
                    if let Ok(rep) = rpc_once::<AppListReply>(
                        &conn_bg,
                        &Command::AppList(AppListArgs { refresh: false }),
                        DEFAULT_RPC_TIMEOUT,
                    )
                    .await
                    {
                        node_bg.cache_peer_apps(&pid_bg, rep.apps);
                    }
                });
            }
        }

        Ok(conn)
    }
}

pub(crate) fn build_endpoint_addr(
    target_id: EndpointId,
    relay_url: Option<&str>,
    direct_addrs: &[SocketAddr],
) -> EndpointAddr {
    let mut addr = EndpointAddr::new(target_id);
    if let Some(raw_url) = relay_url
        && let Ok(parsed) = raw_url.parse::<RelayUrl>()
    {
        addr = addr.with_relay_url(parsed);
    }
    for &sock in direct_addrs {
        addr = addr.with_ip_addr(sock);
    }
    addr
}

fn inspect_conn_state(conn: &Connection) -> DeviceConnState {
    let paths = conn.paths();
    let selected = paths
        .iter()
        .find(|p| p.is_selected())
        .or_else(|| paths.iter().next());

    if let Some(p) = selected {
        let rtt_ms = p.rtt().as_millis() as u64;
        match p.remote_addr() {
            TransportAddr::Ip(ip) => DeviceConnState::OnlineDirect {
                addr: ip.to_string(),
                rtt_ms,
            },
            TransportAddr::Relay(url) => DeviceConnState::OnlineRelay {
                relay_url: url.to_string(),
                rtt_ms,
            },
            _ => DeviceConnState::OnlineDirect {
                addr: "unknown".to_string(),
                rtt_ms,
            },
        }
    } else {
        DeviceConnState::OnlineDirect {
            addr: "connected".to_string(),
            rtt_ms: 0,
        }
    }
}

async fn rpc_once<R: DeserializeOwned>(
    conn: &Connection,
    cmd: &Command,
    limit: Duration,
) -> Result<R, CoreError> {
    let env = cmd.to_envelope()?;
    timeout(limit, async {
        let (mut send, mut recv) = conn
            .open_bi()
            .await
            .map_err(|e| CoreError::NotConnected(format!("和对方的连接已断开：{e}")))?;
        write_frame(&mut send, &env).await?;
        send.finish()
            .map_err(|e| CoreError::Internal(format!("结束请求流失败：{e}")))?;
        let resp: ResponseEnvelope<R> = read_frame(&mut recv).await?;
        Ok(resp.into_result()?)
    })
    .await
    .map_err(|_| CoreError::Timeout)?
}

async fn write_rpc_result<T: serde::Serialize>(
    send: &mut SendStream,
    res: Result<T, RpcError>,
) -> Result<(), CoreError> {
    let env = match res {
        Ok(data) => ResponseEnvelope::Ok(data),
        Err(err) => ResponseEnvelope::Err(err),
    };
    write_frame(send, &env).await?;
    Ok(())
}

pub fn node_capabilities() -> Vec<String> {
    vec![
        "sys.v1".to_string(),
        "fs.v1".to_string(),
        "fs.v2".to_string(),
        "transfer.v1".to_string(),
        "transfer.v2".to_string(),
        "app.v1".to_string(),
        "app.v2".to_string(),
        "proc.v1".to_string(),
        "events.v1".to_string(),
        "events.v2".to_string(),
        "search.v1".to_string(),
        "shot.v1".to_string(),
        "input.v1".to_string(),
        "power.v1".to_string(),
        "clip.v1".to_string(),
        "term.v1".to_string(),
    ]
}
