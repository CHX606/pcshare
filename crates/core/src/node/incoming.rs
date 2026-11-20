use super::*;

impl CoreNode {
    // ────────────────────────── 内部连接管理与服务端循环 ──────────────────────────

    pub(super) fn set_peer_state(&self, peer_id: &str, state: DeviceConnState) {
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

    pub(super) fn spawn_accept_loop(self: &Arc<Self>) {
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
        *self.accept_handle.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
    }

    async fn handle_incoming_pair(self: Arc<Self>, conn: Connection) {
        let peer_id = conn.remote_id();
        let Ok(Ok((mut send, mut recv))) = timeout(Duration::from_secs(10), conn.accept_bi()).await
        else {
            conn.close(CLOSE_UNAUTHORIZED.into(), b"pair_timeout");
            return;
        };

        let Ok(Ok(req)) = timeout(
            Duration::from_secs(10),
            read_frame_limited::<_, PairRequest>(&mut recv, MAX_HANDSHAKE_FRAME_SIZE),
        )
        .await
        else {
            conn.close(CLOSE_UNAUTHORIZED.into(), b"bad_pair_req");
            return;
        };

        let decision = match self.pair_decision(peer_id, &req).await {
            Ok(decision) => decision,
            Err((err, reason)) => {
                reject_pair(&conn, &mut send, err, reason).await;
                return;
            }
        };
        match decision {
            PairingDecision::Reject => {
                let reason = "对方拒绝了本次配对请求（或等待确认超时）";
                reject_pair(
                    &conn,
                    &mut send,
                    RpcError::new(ErrorCode::Forbidden, reason),
                    b"rejected",
                )
                .await
            }
            PairingDecision::Approve {
                granted_permissions,
                accept_mutual,
            } => {
                self.approve_pair(&conn, &mut send, req, granted_permissions, accept_mutual)
                    .await;
            }
        }
    }

    async fn pair_decision(
        &self,
        peer_id: EndpointId,
        req: &PairRequest,
    ) -> Result<PairingDecision, (RpcError, &'static [u8])> {
        if req.v != PROTOCOL_VERSION {
            let msg = format!(
                "配对协议版本不兼容（对方 v{}，本机 v{}）",
                req.v, PROTOCOL_VERSION
            );
            return Err((RpcError::new(ErrorCode::Unsupported, msg), b"unsupported"));
        }
        let (prompt, decision_rx) = self
            .pairing
            .verify_code_and_prepare_approval(
                peer_id,
                req.requester_name.clone(),
                req.requester_os.clone(),
                req.mutual,
                &req.code,
            )
            .map_err(|e| (e, b"invalid_code" as &'static [u8]))?;
        let request_id = prompt.request_id.clone();
        let _ = self
            .events_tx
            .send(CoreEvent::PairingApprovalNeeded(prompt));
        let decision = match timeout(Duration::from_secs(120), decision_rx).await {
            Ok(Ok(d)) => d,
            _ => {
                self.pairing.remove_pending(&request_id);
                PairingDecision::Reject
            }
        };
        Ok(decision)
    }

    async fn approve_pair(
        self: &Arc<Self>,
        conn: &Connection,
        send: &mut SendStream,
        req: PairRequest,
        granted_permissions: Permissions,
        accept_mutual: bool,
    ) {
        self.pairing.consume_session_on_approved();
        let connect_outgoing = req.mutual && accept_mutual;
        let peer_record = incoming_peer(conn, req, granted_permissions.clone(), connect_outgoing);
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
        let _ = write_frame(send, &resp).await;
        let _ = send.finish();
        let _ = timeout(Duration::from_secs(2), send.stopped()).await;
        if connect_outgoing && self.config.auto_connect_peers {
            self.ensure_peer_worker(&peer_record.endpoint_id);
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

        self.register_incoming_conn(&peer_id_str, conn.clone());
        let _ = self.storage.touch_peer_seen_in_memory(
            &peer_id_str,
            Utc::now().timestamp_millis() as u64,
            None,
        );

        while let Ok((send, recv)) = conn.accept_bi().await {
            let node = self.clone();
            let pid = peer_id_str.clone();
            let path = conn.clone();
            tokio::spawn(async move {
                if let Err(err) = node.handle_command_stream(&pid, &path, send, recv).await {
                    tracing::debug!(peer=%pid,error=%err,"设备指令流已结束");
                }
            });
        }
        let still_has_incoming = self.unregister_incoming_conn(&peer_id_str, conn.stable_id());
        if !still_has_incoming {
            self.fs_watch.unwatch_all_for_peer(&peer_id_str);
        }
    }
}

fn incoming_peer(
    conn: &Connection,
    req: PairRequest,
    granted_permissions: Permissions,
    connect_outgoing: bool,
) -> PeerRecord {
    let now_ms = Utc::now().timestamp_millis() as u64;
    PeerRecord {
        endpoint_id: conn.remote_id().to_string(),
        name: req.requester_name,
        os: Some(req.requester_os),
        permissions: granted_permissions,
        allow_incoming: true,
        connect_outgoing,
        relay_url: req.requester_relay_url,
        direct_addrs: super::wire::observed_addrs(conn),
        mac_addresses: req.requester_macs,
        capabilities: node_capabilities(),
        paired_at_ms: now_ms,
        last_seen_ms: Some(now_ms),
    }
}
async fn reject_pair(conn: &Connection, send: &mut SendStream, err: RpcError, reason: &[u8]) {
    let _ = write_frame(
        send,
        &PairResponse::Rejected {
            code: err.code,
            msg: err.msg,
        },
    )
    .await;
    let _ = send.finish();
    let _ = timeout(Duration::from_secs(2), send.stopped()).await;
    conn.close(CLOSE_UNAUTHORIZED.into(), reason);
}
