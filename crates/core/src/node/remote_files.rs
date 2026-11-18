use super::*;

impl CoreNode {
    pub async fn remote_ping(
        self: &Arc<Self>,
        peer_id: &str,
    ) -> Result<(PingReply, Duration), CoreError> {
        let conn = self.get_or_connect(peer_id).await?;
        let start = Instant::now();
        let now_ms = Utc::now().timestamp_millis() as u64;
        let reply: PingReply = rpc_once(
            &conn,
            &Command::SysPing(PingArgs {
                seq: fastrand::u64(..),
                client_ts_ms: now_ms,
            }),
            DEFAULT_RPC_TIMEOUT,
        )
        .await?;
        Ok((reply, start.elapsed()))
    }

    pub(crate) async fn call_rpc<R: DeserializeOwned>(
        self: &Arc<Self>,
        peer_id: &str,
        cmd: Command,
    ) -> Result<R, CoreError> {
        let can_retry = cmd.is_idempotent_read();
        let conn = self.get_or_connect(peer_id).await?;
        match rpc_once(&conn, &cmd, DEFAULT_RPC_TIMEOUT).await {
            Ok(res) => Ok(res),
            Err(CoreError::NotConnected(_) | CoreError::Frame(_) | CoreError::Timeout)
                if can_retry =>
            {
                self.invalidate_active_conn(peer_id);
                let conn2 = self.get_or_connect(peer_id).await?;
                rpc_once(&conn2, &cmd, DEFAULT_RPC_TIMEOUT).await
            }
            Err(err) => Err(err),
        }
    }

    pub async fn remote_info(self: &Arc<Self>, peer_id: &str) -> Result<SysInfoReply, CoreError> {
        self.call_rpc(peer_id, Command::SysInfo).await
    }

    pub async fn remote_roots(self: &Arc<Self>, peer_id: &str) -> Result<FsRootsReply, CoreError> {
        self.call_rpc(peer_id, Command::FsRoots).await
    }

    /// 列出远端目录（自动合并多批返回的数据帧）。
    pub async fn remote_list(
        self: &Arc<Self>,
        peer_id: &str,
        path: &str,
    ) -> Result<Vec<DirEntry>, CoreError> {
        let mut all = Vec::new();
        self.remote_list_batches(peer_id, path, None, |batch| {
            all.extend(batch.entries);
        })
        .await?;
        Ok(all)
    }

    pub async fn remote_list_batches<F>(
        self: &Arc<Self>,
        peer_id: &str,
        path: &str,
        batch_size: Option<usize>,
        mut on_batch: F,
    ) -> Result<(), CoreError>
    where
        F: FnMut(FsListBatch),
    {
        let conn = self.get_or_connect(peer_id).await?;
        let cmd = Command::FsList(FsListArgs {
            path: path.to_string(),
            batch_size,
        });
        let env = cmd.to_envelope()?;

        timeout(DEFAULT_RPC_TIMEOUT, async {
            let (mut send, mut recv) = conn
                .open_bi()
                .await
                .map_err(|e| CoreError::NotConnected(format!("打开数据流失败：{e}")))?;
            write_frame(&mut send, &env).await?;
            send.finish()
                .map_err(|e| CoreError::Internal(format!("结束发送流失败：{e}")))?;

            loop {
                let resp: ResponseEnvelope<FsListBatch> = read_frame(&mut recv).await?;
                let batch = resp.into_result()?;
                let more = batch.has_more;
                on_batch(batch);
                if !more {
                    break;
                }
            }
            Ok::<(), CoreError>(())
        })
        .await
        .map_err(|_| CoreError::Timeout)?
    }

    pub async fn remote_stat(
        self: &Arc<Self>,
        peer_id: &str,
        path: &str,
    ) -> Result<FsStatReply, CoreError> {
        self.call_rpc(
            peer_id,
            Command::FsStat(FsStatArgs {
                path: path.to_string(),
            }),
        )
        .await
    }

    pub async fn remote_watch(
        self: &Arc<Self>,
        peer_id: &str,
        path: &str,
    ) -> Result<FsWatchReply, CoreError> {
        self.call_rpc(
            peer_id,
            Command::FsWatch(FsWatchArgs {
                path: path.to_string(),
            }),
        )
        .await
    }

    pub async fn remote_unwatch(
        self: &Arc<Self>,
        peer_id: &str,
        path: &str,
    ) -> Result<FsUnwatchReply, CoreError> {
        self.call_rpc(
            peer_id,
            Command::FsUnwatch(FsUnwatchArgs {
                path: path.to_string(),
            }),
        )
        .await
    }

    pub async fn remote_mkdir(
        self: &Arc<Self>,
        peer_id: &str,
        args: FsMkdirArgs,
    ) -> Result<FsMkdirReply, CoreError> {
        self.call_rpc(peer_id, Command::FsMkdir(args)).await
    }

    pub async fn remote_rename(
        self: &Arc<Self>,
        peer_id: &str,
        args: FsRenameArgs,
    ) -> Result<FsRenameReply, CoreError> {
        self.call_rpc(peer_id, Command::FsRename(args)).await
    }

    pub async fn remote_copy(
        self: &Arc<Self>,
        peer_id: &str,
        args: FsCopyMoveArgs,
    ) -> Result<FsTaskReply, CoreError> {
        self.call_rpc(peer_id, Command::FsCopy(args)).await
    }

    pub async fn remote_move(
        self: &Arc<Self>,
        peer_id: &str,
        args: FsCopyMoveArgs,
    ) -> Result<FsTaskReply, CoreError> {
        self.call_rpc(peer_id, Command::FsMove(args)).await
    }

    pub async fn remote_task_status(
        self: &Arc<Self>,
        peer_id: &str,
        task_id: &str,
    ) -> Result<FsTaskProgress, CoreError> {
        self.call_rpc(
            peer_id,
            Command::FsTaskStatus(FsTaskIdArgs {
                task_id: task_id.to_string(),
            }),
        )
        .await
    }

    pub async fn remote_task_cancel(
        self: &Arc<Self>,
        peer_id: &str,
        task_id: &str,
    ) -> Result<FsTaskProgress, CoreError> {
        self.call_rpc(
            peer_id,
            Command::FsTaskCancel(FsTaskIdArgs {
                task_id: task_id.to_string(),
            }),
        )
        .await
    }

    /// 等待远端复制/移动任务完成。
    pub async fn wait_remote_task(
        self: &Arc<Self>,
        peer_id: &str,
        task_id: &str,
        max_wait: Duration,
    ) -> Result<FsTaskProgress, CoreError> {
        let deadline = Instant::now() + max_wait;
        loop {
            let status = self.remote_task_status(peer_id, task_id).await?;
            match status.state {
                FsTaskState::Completed | FsTaskState::Cancelled => return Ok(status),
                FsTaskState::Failed => {
                    if let Some(err) = status.error {
                        return Err(CoreError::Rpc(err));
                    }
                    return Err(CoreError::Internal("远端文件任务失败".to_string()));
                }
                FsTaskState::Running => {
                    if Instant::now() >= deadline {
                        return Err(CoreError::Timeout);
                    }
                    tokio::time::sleep(Duration::from_millis(60)).await;
                }
            }
        }
    }

    pub async fn remote_delete(
        self: &Arc<Self>,
        peer_id: &str,
        args: FsDeleteArgs,
    ) -> Result<FsDeleteReply, CoreError> {
        self.call_rpc(peer_id, Command::FsDelete(args)).await
    }
}
