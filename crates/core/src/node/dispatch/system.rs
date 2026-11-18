use super::*;
impl RpcStream<'_> {
    pub(super) async fn dispatch_system(&mut self, cmd: Command) -> Result<(), CoreError> {
        match cmd {
            Command::SysHello(args) => self.on_sys_hello(args).await,
            Command::SysPing(args) => self.on_sys_ping(args).await,
            Command::SysInfo => self.on_sys_info().await,
            Command::SubEvents => self.on_sub_events().await,
            other => self.dispatch_files(other).await,
        }
    }
    async fn on_sys_hello(&mut self, args: HelloArgs) -> Result<(), CoreError> {
        let _ = self.node.storage.update_peer_hello_meta(
            self.peer,
            args.capabilities,
            args.mac_addresses,
        );
        let reply = HelloReply {
            protocol_version: PROTOCOL_VERSION,
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            device_name: self.node.storage.settings().device_name,
            os: std::env::consts::OS.to_string(),
            capabilities: node_capabilities(),
            mac_addresses: self.node.platform.local_mac_addresses(),
            granted_permissions: self.record.permissions.clone(),
        };
        write_frame(self.send, &ResponseEnvelope::ok(reply)).await?;
        Ok(())
    }
    async fn on_sys_ping(&mut self, args: PingArgs) -> Result<(), CoreError> {
        let reply = PingReply {
            seq: args.seq,
            client_ts_ms: args.client_ts_ms,
            server_ts_ms: Utc::now().timestamp_millis() as u64,
        };
        write_frame(self.send, &ResponseEnvelope::ok(reply)).await?;
        Ok(())
    }
    async fn on_sys_info(&mut self) -> Result<(), CoreError> {
        let (computer_name, user_name) = self.node.platform.host_and_user();
        let desktop = to_proto_desktop(self.node.platform.desktop_environment());
        let disks = self
            .node
            .platform
            .list_drives()
            .unwrap_or_default()
            .into_iter()
            .map(|d| DiskSpaceInfo {
                mount_point: d.path,
                label: d.label,
                total_bytes: d.total_bytes,
                available_bytes: d.available_bytes,
            })
            .collect();
        let mem = self
            .node
            .platform
            .memory_info()
            .unwrap_or(pcshare_platform::SystemMemory {
                total_bytes: 0,
                available_bytes: 0,
            });
        let reply = SysInfoReply {
            computer_name,
            user_name,
            os: std::env::consts::OS.to_string(),
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            desktop,
            disks,
            memory: MemoryInfo {
                total_bytes: mem.total_bytes,
                available_bytes: mem.available_bytes,
            },
        };
        write_frame(self.send, &ResponseEnvelope::ok(reply)).await?;
        Ok(())
    }
    async fn on_sub_events(&mut self) -> Result<(), CoreError> {
        // 长期订阅流：先回复就绪，再持续推送属于该连接对端的事件
        write_frame(self.send, &ResponseEnvelope::ok(())).await?;
        let mut rx = self.node.remote_events_tx.subscribe();
        loop {
            tokio::select! {
                _ = self.send.stopped() => break,
                msg = rx.recv() => {
                    match msg {
                        Ok((target_peer, ev)) if target_peer == self.peer => {
                            write_frame(self.send, &ev).await?;
                        }
                        Ok(_) => {}
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        }
        Ok(())
    }
}
