use super::*;

impl CoreNode {
    pub async fn remote_apps(
        self: &Arc<Self>,
        peer_id: &str,
        refresh: bool,
    ) -> Result<AppListReply, CoreError> {
        let rep: AppListReply = self
            .call_rpc(peer_id, Command::AppList(AppListArgs { refresh }))
            .await?;
        self.cache_peer_apps(peer_id, rep.apps.clone());
        Ok(rep)
    }

    pub async fn remote_app_icon(
        self: &Arc<Self>,
        peer_id: &str,
        app_id: &str,
    ) -> Result<AppIconReply, CoreError> {
        self.call_rpc(
            peer_id,
            Command::AppIcon(AppIconArgs {
                app_id: app_id.to_string(),
            }),
        )
        .await
    }

    pub async fn remote_launch(
        self: &Arc<Self>,
        peer_id: &str,
        args: AppLaunchArgs,
    ) -> Result<AppLaunchReply, CoreError> {
        self.call_rpc(peer_id, Command::AppLaunch(args)).await
    }

    pub async fn remote_processes(
        self: &Arc<Self>,
        peer_id: &str,
    ) -> Result<ProcListReply, CoreError> {
        self.call_rpc(peer_id, Command::ProcList).await
    }

    pub async fn remote_kill_process(
        self: &Arc<Self>,
        peer_id: &str,
        args: ProcKillArgs,
    ) -> Result<ProcKillReply, CoreError> {
        self.call_rpc(peer_id, Command::ProcKill(args)).await
    }

    /// 手动立即重连指定已配对设备（供桌面界面“重连”按钮调用）。
    pub async fn reconnect_peer(self: &Arc<Self>, peer_id: &str) -> Result<(), CoreError> {
        let peer = self.storage.get_peer(peer_id).ok_or_else(|| {
            CoreError::Rpc(RpcError::new(ErrorCode::NotFound, "找不到这台已配对设备"))
        })?;
        self.disconnect_for_retry(peer_id);
        self.set_peer_state(peer_id, DeviceConnState::Connecting);
        match self.dial_and_handshake(peer_id).await {
            Ok(conn) => {
                self.set_peer_state(peer_id, inspect_conn_state(&conn));
                if self.config.auto_connect_peers && peer.connect_outgoing {
                    self.ensure_peer_worker(peer_id);
                }
                Ok(())
            }
            Err(err) => {
                self.set_peer_state(
                    peer_id,
                    DeviceConnState::Offline {
                        reason: Some(err.to_user_message()),
                    },
                );
                if self.config.auto_connect_peers && peer.connect_outgoing {
                    self.ensure_peer_worker(peer_id);
                }
                Err(CoreError::NotConnected(format!(
                    "还是连不上“{}”。确认它已开机、程序在运行，并且能上网",
                    peer.name
                )))
            }
        }
    }

    fn disconnect_for_retry(&self, peer_id: &str) {
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
            conn.close(CLOSE_OK.into(), b"manual_reconnect");
        }
    }
    // ────────────────────────── 本机直接操作辅助接口 ──────────────────────────

    pub async fn local_apps(&self, refresh: bool) -> Result<AppListReply, CoreError> {
        let perms = Permissions::default();
        let rep = handle_app_list(&self.platform, &perms, AppListArgs { refresh }).await?;
        self.cache_peer_apps("self", rep.apps.clone());
        Ok(rep)
    }

    pub async fn local_app_icon(&self, app_id: &str) -> Result<AppIconReply, CoreError> {
        let perms = Permissions::default();
        Ok(handle_app_icon(
            &self.platform,
            &perms,
            AppIconArgs {
                app_id: app_id.to_string(),
            },
        )
        .await?)
    }

    pub async fn local_launch(&self, args: AppLaunchArgs) -> Result<AppLaunchReply, CoreError> {
        // 本机操作本机，不受“命令行”权限限制（例如批量操作里带参数打开本机软件）。
        let perms = Permissions {
            terminal: true,
            ..Permissions::default()
        };
        Ok(handle_app_launch(&self.platform, &perms, args).await?)
    }

    pub async fn local_processes(&self) -> Result<ProcListReply, CoreError> {
        let perms = Permissions::default();
        Ok(handle_proc_list(&self.platform, &perms).await?)
    }

    pub async fn local_kill_process(&self, args: ProcKillArgs) -> Result<ProcKillReply, CoreError> {
        let perms = Permissions::default();
        Ok(handle_proc_kill(&self.platform, &perms, args).await?)
    }
}
