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

    /// 获取或建立第 `lane`（`0..4`）条数据面 QUIC 连接：
    /// `lane == 0` 复用主连接；`lane >= 1` 建立独立 QUIC 连接使多核并行执行 AEAD 加解密与 UDP 收发，
    /// 若辅助连接拨号受阻则平滑回退主连接。
    pub(crate) async fn get_or_connect_bulk_lane(
        self: &Arc<Self>,
        peer_id: &str,
        lane: usize,
    ) -> Result<Connection, CoreError> {
        let main_conn = self.get_or_connect(peer_id).await?;
        let slot = lane % 4;
        if slot == 0 {
            return Ok(main_conn);
        }
        let key = (peer_id.to_string(), slot);
        {
            let guard = self.bulk_conns.read().unwrap_or_else(|e| e.into_inner());
            if let Some(c) = guard.get(&key)
                && c.close_reason().is_none()
            {
                return Ok(c.clone());
            }
        }
        match self.dial_bulk_lane(peer_id, &main_conn).await {
            Ok(aux_conn) => {
                let mut guard = self.bulk_conns.write().unwrap_or_else(|e| e.into_inner());
                if let Some(existing) = guard.get(&key)
                    && existing.close_reason().is_none()
                {
                    let keep = existing.clone();
                    drop(guard);
                    aux_conn.close(CLOSE_OK.into(), b"dup_lane");
                    return Ok(keep);
                }
                guard.insert(key, aux_conn.clone());
                Ok(aux_conn)
            }
            Err(_) => Ok(main_conn),
        }
    }

    async fn dial_bulk_lane(
        &self,
        peer_id: &str,
        main_conn: &Connection,
    ) -> Result<Connection, CoreError> {
        let peer = self
            .storage
            .get_peer(peer_id)
            .ok_or_else(|| CoreError::InvalidArgument(format!("设备尚未配对：{peer_id}")))?;
        let target_id: EndpointId = peer
            .endpoint_id
            .parse()
            .map_err(|_| CoreError::InvalidArgument(format!("身份码无效：{peer_id}")))?;

        let mut direct_addrs = super::wire::observed_addrs(main_conn);
        for &a in &peer.direct_addrs {
            if !direct_addrs.contains(&a) {
                direct_addrs.push(a);
            }
        }
        let addr = build_endpoint_addr(target_id, peer.relay_url.as_deref(), &direct_addrs);
        timeout(Duration::from_secs(3), self.endpoint.connect(addr, ALPN_MAIN))
            .await
            .map_err(|_| CoreError::Timeout)?
            .map_err(|e| CoreError::NotConnected(format!("建立辅助传输连接失败：{e}")))
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
