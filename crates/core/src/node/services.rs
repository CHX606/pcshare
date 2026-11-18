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
}
