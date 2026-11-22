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


mod app_diag;
mod fs_ops;
mod transfer_ops;
mod types;

use app_diag::*;
pub use app_diag::{classify_launch_target, should_fallback_direct_to_relay};
pub(crate) use app_diag::{convert_desktop_status, convert_launch_reply};
pub use types::*;

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
}
