use super::*;

impl CoreNode {
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

        let req = self.pair_request(code, mutual, granted_to_responder.clone());
        let resp = exchange_pair(&conn, req).await?;
        let observed_addrs = if direct_addrs.is_empty() {
            super::wire::observed_addrs(&conn)
        } else {
            direct_addrs
        };
        conn.close(CLOSE_OK.into(), b"pair_done");
        self.finish_pair(
            target_id,
            observed_addrs,
            relay_url,
            mutual,
            granted_to_responder,
            resp,
        )
    }
    fn pair_request(
        &self,
        code: &str,
        mutual: bool,
        granted_to_responder: Permissions,
    ) -> PairRequest {
        PairRequest {
            v: PROTOCOL_VERSION,
            code: code.trim().to_string(),
            requester_name: self.storage.settings().device_name,
            requester_os: std::env::consts::OS.to_string(),
            requester_relay_url: self.home_relay_url(),
            requester_macs: self.platform.local_mac_addresses(),
            mutual,
            granted_to_responder,
        }
    }
    fn finish_pair(
        self: &Arc<Self>,
        target_id: EndpointId,
        observed_addrs: Vec<SocketAddr>,
        relay_url: Option<String>,
        mutual: bool,
        granted_to_responder: Permissions,
        resp: PairResponse,
    ) -> Result<PeerRecord, CoreError> {
        let PairResponse::Approved {
            responder_name,
            responder_os,
            responder_relay_url,
            responder_macs,
            mutual_accepted,
            ..
        } = resp
        else {
            if let PairResponse::Rejected { code, msg } = resp {
                return Err(RpcError::new(code, msg).into());
            }
            unreachable!()
        };
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
        self.store_pair(record)
    }
    fn store_pair(self: &Arc<Self>, record: PeerRecord) -> Result<PeerRecord, CoreError> {
        self.storage.upsert_peer(record.clone())?;
        let _ = self.events_tx.send(CoreEvent::PeersUpdated);
        if self.config.auto_connect_peers {
            self.ensure_peer_worker(&record.endpoint_id);
        }
        Ok(record)
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
}

async fn exchange_pair(conn: &Connection, req: PairRequest) -> Result<PairResponse, CoreError> {
    let (mut send, mut recv) = conn
        .open_bi()
        .await
        .map_err(|e| CoreError::NotConnected(format!("打开配对数据流失败：{e}")))?;

    write_frame(&mut send, &req).await?;
    send.finish()
        .map_err(|e| CoreError::Internal(format!("结束配对发送流失败：{e}")))?;

    // 等待被控电脑前的人点击确认（最长等待 120 秒）
    Ok(timeout(Duration::from_secs(120), read_frame(&mut recv))
        .await
        .map_err(|_| CoreError::Timeout)??)
}
