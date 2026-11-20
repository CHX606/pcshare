use super::*;

impl CoreNode {
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
        self.clipboard_auto_sync_enabled
            .store(updated.clipboard_auto_sync, Ordering::SeqCst);
        self.clipboard_auto_sync_notify.notify_one();
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
        if let Some(entry) = self
            .peer_workers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(peer_id)
        {
            entry.cancel.cancel();
        }
        if let Some(conn) = self
            .active_conns
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(peer_id)
        {
            conn.close(CLOSE_UNAUTHORIZED.into(), b"removed");
        }
        // 强制断开该对端所有已建立的入站连接，立即终止终端、事件流、实时桌面与目录监听
        if let Some(in_list) = self
            .incoming_conns
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(peer_id)
        {
            for conn in in_list {
                conn.close(CLOSE_UNAUTHORIZED.into(), b"removed");
            }
        }
        self.fs_watch.unwatch_all_for_peer(peer_id);
        self.edit_sync.remove_peer_sessions(peer_id);
        self.dial_locks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(peer_id);
        self.apps_index
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(peer_id);
        self.direct_grants
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|_, g| g.allowed_peer_id != peer_id);
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
        let _ = self.storage.flush_peers();
        self.edit_sync.shutdown();
        let _ = self.trigger_power_cancel();
        self.fs_watch.shutdown();
        self.lan_discovery.stop();
        self.shutdown_token.cancel();
        self.clipboard_auto_sync_notify.notify_waiters();
        let direct_cancels: Vec<CancellationToken> = self
            .active_direct_transfers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain()
            .map(|(_, c)| c)
            .collect();
        for c in direct_cancels {
            c.cancel();
        }
        let workers: Vec<PeerWorkerEntry> = self
            .peer_workers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain()
            .map(|(_, e)| e)
            .collect();
        for w in workers {
            w.cancel.cancel();
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
        let in_conns: Vec<Connection> = self
            .incoming_conns
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .drain()
            .flat_map(|(_, v)| v)
            .collect();
        for c in in_conns {
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
}
