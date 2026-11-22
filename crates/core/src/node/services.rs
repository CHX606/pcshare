use super::*;

impl CoreNode {
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

    pub(crate) fn set_active_power_cancel(&self, tx: Option<tokio::sync::watch::Sender<bool>>) {
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

    pub(super) fn spawn_clipboard_autosync_loop(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        let shutdown = self.shutdown_token.clone();
        let notify = self.clipboard_auto_sync_notify.clone();
        tokio::spawn(async move {
            loop {
                let Some(node) = weak.upgrade() else { break };
                // 关闭自动同步时彻底挂起等待通知，零轮询唤醒且不再深拷贝 Settings
                if !node.clipboard_auto_sync_enabled.load(Ordering::SeqCst) {
                    drop(node);
                    tokio::select! {
                        _ = shutdown.cancelled() => break,
                        _ = notify.notified() => continue,
                    }
                }
                drop(node);

                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = notify.notified() => continue,
                    _ = tokio::time::sleep(Duration::from_millis(900)) => {
                        let Some(node) = weak.upgrade() else { break; };
                        if !node.clipboard_auto_sync_enabled.load(Ordering::SeqCst) {
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
                            .active_conns
                            .read()
                            .unwrap_or_else(|e| e.into_inner())
                            .iter()
                            .filter(|(_, c)| c.close_reason().is_none())
                            .map(|(id, _)| id.clone())
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
        let _ = self.events_tx.send(CoreEvent::HostPausedChanged { paused });
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

    pub(super) fn register_incoming_conn(&self, peer_id: &str, conn: Connection) {
        let mut guard = self
            .incoming_conns
            .write()
            .unwrap_or_else(|e| e.into_inner());
        let list = guard.entry(peer_id.to_string()).or_default();
        list.retain(|c| c.close_reason().is_none());
        list.push(conn);
    }

    pub(super) fn unregister_incoming_conn(&self, peer_id: &str, stable_id: usize) -> bool {
        let mut guard = self
            .incoming_conns
            .write()
            .unwrap_or_else(|e| e.into_inner());
        let Some(list) = guard.get_mut(peer_id) else {
            return false;
        };
        list.retain(|c| c.stable_id() != stable_id && c.close_reason().is_none());
        if list.is_empty() {
            guard.remove(peer_id);
            false
        } else {
            true
        }
    }

    pub(crate) fn insert_direct_grant(&self, token: String, rec: DirectGrantRecord) {
        let mut guard = self.direct_grants.lock().unwrap_or_else(|e| e.into_inner());
        guard.retain(|_, v| Instant::now() < v.expires_at);
        guard.insert(token, rec);
    }

    pub(crate) fn direct_grants_mutex(&self) -> &Mutex<HashMap<String, DirectGrantRecord>> {
        &self.direct_grants
    }

    pub(crate) fn register_direct_transfer(&self, transfer_id: String, cancel: CancellationToken) {
        self.active_direct_transfers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(transfer_id, cancel);
    }

    pub(crate) fn unregister_direct_transfer(&self, transfer_id: &str) {
        self.active_direct_transfers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(transfer_id);
    }

    pub(crate) fn cancel_direct_transfer(&self, transfer_id: &str) -> bool {
        if let Some(cancel) = self
            .active_direct_transfers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(transfer_id)
        {
            cancel.cancel();
            true
        } else {
            false
        }
    }

    pub(crate) fn emit_remote_event(&self, peer_id: &str, event: RemoteEvent) {
        let _ = self.remote_events_tx.send((peer_id.to_string(), event));
    }

    /// 确保目标源设备的 `sub.events` 订阅已就绪（无 50ms 盲等），并在连接断开时自动清理占位条目。
    pub(crate) async fn ensure_peer_event_subscription(
        self: &Arc<Self>,
        peer_id: &str,
        conn: Connection,
    ) {
        let (worker_id, tok) = {
            let mut guard = self.peer_workers.lock().unwrap_or_else(|e| e.into_inner());
            if guard.contains_key(peer_id) {
                return;
            }
            let wid = self.next_worker_id.fetch_add(1, Ordering::Relaxed);
            let tok = CancellationToken::new();
            guard.insert(
                peer_id.to_string(),
                PeerWorkerEntry {
                    id: wid,
                    cancel: tok.clone(),
                },
            );
            (wid, tok)
        };
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        self.spawn_event_subscriber_inner(
            peer_id.to_string(),
            conn,
            tok,
            Some(ready_tx),
            Some(worker_id),
        );
        let _ = timeout(Duration::from_secs(5), ready_rx).await;
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
        let mut bulk = self.bulk_conns.write().unwrap_or_else(|e| e.into_inner());
        bulk.retain(|(pid, _), c| {
            if pid == peer_id {
                c.close(1u32.into(), b"stale_retry");
                false
            } else {
                true
            }
        });
    }

    #[doc(hidden)]
    pub fn force_drop_active_conn_for_test(&self, peer_id: &str) {
        if let Some(conn) = self
            .active_conns
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(peer_id)
        {
            conn.close(99u32.into(), b"simulated_drop");
        }
        let bulk = self.bulk_conns.read().unwrap_or_else(|e| e.into_inner());
        for ((pid, _), c) in bulk.iter() {
            if pid == peer_id {
                c.close(99u32.into(), b"simulated_drop");
            }
        }
    }
}
