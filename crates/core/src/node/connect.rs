use super::*;

impl CoreNode {
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

    pub(super) async fn dial_and_handshake(
        self: &Arc<Self>,
        peer_id: &str,
    ) -> Result<Connection, CoreError> {
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

        let conn = self.dial_peer(peer_id).await?;
        let reply = self.exchange_hello(&conn).await?;
        let _ =
            self.storage
                .update_peer_hello_meta(peer_id, reply.capabilities, reply.mac_addresses);

        let mut guard = self.active_conns.write().unwrap_or_else(|e| e.into_inner());
        guard.insert(peer_id.to_string(), conn.clone());
        drop(guard);

        if reply.granted_permissions.app_launch {
            self.warm_apps(peer_id, &conn);
        }
        Ok(conn)
    }

    async fn dial_peer(&self, peer_id: &str) -> Result<Connection, CoreError> {
        let peer = self
            .storage
            .get_peer(peer_id)
            .ok_or_else(|| CoreError::InvalidArgument(format!("设备尚未配对：{peer_id}")))?;
        let target_id: EndpointId = peer
            .endpoint_id
            .parse()
            .map_err(|_| CoreError::InvalidArgument(format!("身份码无效：{peer_id}")))?;

        let addr = build_endpoint_addr(target_id, peer.relay_url.as_deref(), &peer.direct_addrs);
        let connect_timeout = match self.config.network_mode {
            NetworkMode::LocalTest => Duration::from_secs(2),
            NetworkMode::PublicOrCustom => Duration::from_secs(15),
        };
        timeout(connect_timeout, self.endpoint.connect(addr, ALPN_MAIN))
            .await
            .map_err(|_| CoreError::Timeout)?
            .map_err(|e| CoreError::NotConnected(format!("无法连接设备“{}”：{e}", peer.name)))
    }
    async fn exchange_hello(&self, conn: &Connection) -> Result<HelloReply, CoreError> {
        // 连接建立后先交换 sys.hello（见总体方案 6.5 节）
        let hello = Command::SysHello(HelloArgs {
            protocol_version: PROTOCOL_VERSION,
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            device_name: self.storage.settings().device_name,
            os: std::env::consts::OS.to_string(),
            capabilities: node_capabilities(),
            mac_addresses: self.platform.local_mac_addresses(),
        });
        match rpc_once(conn, &hello, DEFAULT_RPC_TIMEOUT).await {
            Ok(r) => Ok(r),
            Err(err) => {
                conn.close(CLOSE_OK.into(), b"hello_failed");
                Err(err)
            }
        }
    }
    fn warm_apps(self: &Arc<Self>, peer_id: &str, conn: &Connection) {
        let cached = self
            .apps_index
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(peer_id);
        if cached {
            return;
        }
        let node = self.clone();
        let id = peer_id.to_string();
        let conn = conn.clone();
        tokio::spawn(async move {
            let cmd = Command::AppList(AppListArgs { refresh: false });
            if let Ok(rep) = rpc_once::<AppListReply>(&conn, &cmd, DEFAULT_RPC_TIMEOUT).await {
                node.cache_peer_apps(&id, rep.apps);
            }
        });
    }
}
