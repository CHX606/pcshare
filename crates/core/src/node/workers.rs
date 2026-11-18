use super::*;

impl CoreNode {
    pub(super) fn ensure_peer_worker(self: &Arc<Self>, peer_id: &str) {
        let mut guard = self.peer_workers.lock().unwrap_or_else(|e| e.into_inner());
        if guard.contains_key(peer_id) {
            return;
        }
        let token = CancellationToken::new();
        guard.insert(peer_id.to_string(), token.clone());
        drop(guard);

        let weak = Arc::downgrade(self);
        let peer = peer_id.to_string();
        tokio::spawn(async move {
            let mut backoff_secs = 1;
            while !token.is_cancelled() {
                let Some(node) = weak.upgrade() else {
                    break;
                };
                node.set_peer_state(&peer, DeviceConnState::Connecting);
                backoff_secs = node.reconnect_cycle(&peer, &token, backoff_secs).await;
                drop(node);
                tokio::select! {
                    _ = token.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_secs(backoff_secs)) => {}
                }
                backoff_secs = (backoff_secs * 2).min(30);
            }
        });
    }

    async fn reconnect_cycle(
        self: &Arc<Self>,
        peer: &str,
        token: &CancellationToken,
        backoff: u64,
    ) -> u64 {
        match self.dial_and_handshake(peer).await {
            Ok(conn) => {
                self.watch_peer(peer, conn, token).await;
                1
            }
            Err(e) => {
                let delay = if e.error_code() == Some(ErrorCode::Unauthorized) {
                    30
                } else {
                    backoff
                };
                self.set_peer_state(
                    peer,
                    DeviceConnState::Offline {
                        reason: Some(e.to_user_message()),
                    },
                );
                delay
            }
        }
    }

    async fn watch_peer(self: &Arc<Self>, peer: &str, conn: Connection, token: &CancellationToken) {
        self.set_peer_state(peer, inspect_conn_state(&conn));
        self.spawn_event_subscriber(peer.to_owned(), conn.clone(), token.clone());
        self.monitor_connection(peer, &conn, token).await;
        self.active_conns
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(peer);
        if !token.is_cancelled() {
            self.set_peer_state(
                peer,
                DeviceConnState::Offline {
                    reason: Some("连接已断开，正在重连".to_string()),
                },
            );
        }
    }

    pub(super) fn spawn_event_subscriber(
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
            while let Some(event) = tokio::select! {
                _ = cancel.cancelled() => None,
                frame = read_optional_frame::<_, RemoteEvent>(&mut recv) => match frame {
                    Ok(event) => event,
                    Err(error) => { tracing::debug!(%error, "设备事件订阅已结束"); None }
                },
            } {
                let _ = events_tx.send(core_event(&peer_id, event));
            }
        });
    }

    pub(super) async fn monitor_connection(
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
}

fn core_event(peer: &str, event: RemoteEvent) -> CoreEvent {
    let peer_id = peer.to_owned();
    match event {
        RemoteEvent::TaskProgress(progress) => CoreEvent::RemoteTaskProgress { peer_id, progress },
        RemoteEvent::DirChanged { path } => CoreEvent::RemoteDirChanged { peer_id, path },
        RemoteEvent::DirectTransferProgress(progress) => {
            CoreEvent::RemoteDirectTransferProgress { peer_id, progress }
        }
    }
}
