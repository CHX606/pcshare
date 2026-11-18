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
}
