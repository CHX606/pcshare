use super::*;

impl CoreNode {
    pub(super) fn ensure_peer_worker(self: &Arc<Self>, peer_id: &str) {
        let mut guard = self.peer_workers.lock().unwrap_or_else(|e| e.into_inner());
        if guard.contains_key(peer_id) {
            return;
        }
        let id = self.next_worker_id.fetch_add(1, Ordering::Relaxed);
        let token = CancellationToken::new();
        guard.insert(
            peer_id.to_string(),
            PeerWorkerEntry {
                id,
                cancel: token.clone(),
            },
        );
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
            if let Some(node) = weak.upgrade() {
                let mut g = node.peer_workers.lock().unwrap_or_else(|e| e.into_inner());
                if g.get(&peer).is_some_and(|e| e.id == id) {
                    g.remove(&peer);
                }
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
        // 仅当 active_conns 中仍是当前断开的这条连接（stable_id 匹配）时才移除，防止误删读重试刚建好的健康新连接
        let removed_current = {
            let mut guard = self
                .active_conns
                .write()
                .unwrap_or_else(|e| e.into_inner());
            if guard
                .get(peer)
                .is_some_and(|cur| cur.stable_id() == conn.stable_id())
            {
                guard.remove(peer);
                true
            } else {
                !guard.contains_key(peer)
            }
        };
        if !token.is_cancelled() && removed_current {
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
        self.spawn_event_subscriber_inner(peer_id, conn, cancel, None, None);
    }

    pub(super) fn spawn_event_subscriber_inner(
        self: &Arc<Self>,
        peer_id: String,
        conn: Connection,
        cancel: CancellationToken,
        ready_tx: Option<tokio::sync::oneshot::Sender<bool>>,
        placeholder_worker_id: Option<u64>,
    ) {
        let events_tx = self.events_tx.clone();
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut ready_tx = ready_tx;
            let subscribed = async {
                let (mut send, mut recv) = conn.open_bi().await.ok()?;
                let env = Command::SubEvents.to_envelope().ok()?;
                write_frame(&mut send, &env).await.ok()?;
                match timeout(
                    Duration::from_secs(5),
                    read_frame::<_, ResponseEnvelope<()>>(&mut recv),
                )
                .await
                {
                    Ok(Ok(ResponseEnvelope::Ok(()))) => Some((send, recv)),
                    _ => None,
                }
            }
            .await;

            let Some((_send, mut recv)) = subscribed else {
                if let Some(tx) = ready_tx.take() {
                    let _ = tx.send(false);
                }
                if let (Some(wid), Some(node)) = (placeholder_worker_id, weak.upgrade()) {
                    let mut g = node.peer_workers.lock().unwrap_or_else(|e| e.into_inner());
                    if g.get(&peer_id).is_some_and(|e| e.id == wid) {
                        g.remove(&peer_id);
                    }
                }
                return;
            };

            if let Some(tx) = ready_tx.take() {
                let _ = tx.send(true);
            }

            while let Some(event) = tokio::select! {
                _ = cancel.cancelled() => None,
                _ = conn.closed() => None,
                frame = read_optional_frame::<_, RemoteEvent>(&mut recv) => match frame {
                    Ok(event) => event,
                    Err(error) => { tracing::debug!(%error, "设备事件订阅已结束"); None }
                },
            } {
                let _ = events_tx.send(core_event(&peer_id, event));
            }

            // 占位订阅协程在断线/退出时自动清理自身条目，确保重连后可重新订阅
            if let (Some(wid), Some(node)) = (placeholder_worker_id, weak.upgrade()) {
                let mut g = node.peer_workers.lock().unwrap_or_else(|e| e.into_inner());
                if g.get(&peer_id).is_some_and(|e| e.id == wid) {
                    g.remove(&peer_id);
                }
            }
        });
    }

    pub(super) async fn monitor_connection(
        &self,
        peer_id: &str,
        conn: &Connection,
        cancel: &CancellationToken,
    ) {
        let (ping_interval, ping_timeout, max_consecutive_failures) = match self.config.network_mode
        {
            NetworkMode::LocalTest => (Duration::from_millis(800), Duration::from_secs(2), 1u8),
            NetworkMode::PublicOrCustom => (Duration::from_secs(5), Duration::from_secs(3), 2u8),
        };
        let mut ticker = tokio::time::interval(ping_interval);
        ticker.tick().await;

        let mut consecutive_failures = 0u8;
        let mut last_peers_flush = Instant::now();
        const PEERS_FLUSH_INTERVAL: Duration = Duration::from_secs(60);

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
                        ping_timeout,
                    ).await;
                    if ping_res.is_err() {
                        consecutive_failures = consecutive_failures.saturating_add(1);
                        if consecutive_failures >= max_consecutive_failures || conn.close_reason().is_some() {
                            conn.close(1u32.into(), b"ping_timeout");
                            break;
                        }
                        continue;
                    }
                    consecutive_failures = 0;
                    self.set_peer_state(peer_id, inspect_conn_state(conn));
                    let (_, relay_changed) = self.storage.touch_peer_seen_in_memory(
                        peer_id,
                        Utc::now().timestamp_millis() as u64,
                        None,
                    );
                    // 节流异步落盘 peers.json（每 60 秒或中继变化时），避免每 5 秒同步 sync_all 磨损磁盘
                    if relay_changed || last_peers_flush.elapsed() >= PEERS_FLUSH_INTERVAL {
                        last_peers_flush = Instant::now();
                        let st = self.storage.clone();
                        tokio::task::spawn_blocking(move || {
                            let _ = st.flush_peers();
                        });
                    }
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
