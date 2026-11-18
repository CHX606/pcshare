use super::*;

impl CoreNode {
    // ────────────────────────── 文件上传与下载（任务 1.8）──────────────────────────

    /// 上传本机单个文件到对方电脑（64 KB 分块、`.part` 临时文件、保留修改时间、支持取消和冲突策略）。
    pub async fn upload_file(
        self: &Arc<Self>,
        peer_id: &str,
        local_path: &Path,
        remote_path: &str,
        conflict: ConflictPolicy,
        cancel: CancellationToken,
        on_progress: Option<ProgressCallback>,
    ) -> Result<FsWriteDone, CoreError> {
        let (file, local_disp, total, modified) = upload_source(local_path).await?;
        let conn = self.get_or_connect(peer_id).await?;
        let (mut send, mut recv, ready) =
            open_upload(&conn, remote_path, total, modified, conflict).await?;
        if ready.skipped {
            let _ = send.finish();
            return Ok(FsWriteDone {
                target_path: ready.target_path,
                bytes_written: 0,
            });
        }

        let paths = (local_disp, ready.target_path);
        let mut progress = super::file_progress::FileProgress::new(
            self,
            peer_id,
            TransferDirection::Upload,
            paths,
            total,
            on_progress,
        );
        send_file(file, &mut send, total, &cancel, &mut progress).await?;
        check_cancel(&mut send, &cancel)?;
        send.finish()
            .map_err(|e| CoreError::Internal(format!("结束上传流失败：{e}")))?;
        let done: ResponseEnvelope<FsWriteDone> =
            timeout(Duration::from_secs(30), read_frame(&mut recv))
                .await
                .map_err(|_| CoreError::Timeout)??;
        Ok(done.into_result()?)
    }
}

async fn upload_source(
    local_path: &Path,
) -> Result<(tokio::fs::File, String, u64, Option<u64>), CoreError> {
    let local_clean =
        normalize_lexical_absolute(&local_path.to_string_lossy()).map_err(CoreError::Rpc)?;
    let local_fs = to_fs_path(&local_clean);
    let file = tokio::fs::File::open(&local_fs).await?;
    let meta = file.metadata().await?;
    if !meta.is_file() {
        return Err(CoreError::InvalidArgument(format!(
            "“{}”不是单个普通文件",
            local_path.display()
        )));
    }
    let total_size = meta.len();
    let modified_ms = meta.modified().ok().and_then(system_time_to_ms);

    Ok((file, to_display_path(&local_clean), total_size, modified_ms))
}

async fn open_upload(
    conn: &Connection,
    remote_path: &str,
    total_size: u64,
    modified_ms: Option<u64>,
    conflict: ConflictPolicy,
) -> Result<(SendStream, RecvStream, FsWriteReady), CoreError> {
    let (mut send, mut recv) = conn
        .open_bi()
        .await
        .map_err(|e| CoreError::NotConnected(format!("打开上传通道失败：{e}")))?;

    let cmd = Command::FsWrite(FsWriteArgs {
        path: remote_path.to_string(),
        size: total_size,
        offset: 0,
        modified_ms,
        conflict,
    });
    write_frame(&mut send, &cmd.to_envelope()?).await?;

    let ready_resp: ResponseEnvelope<FsWriteReady> =
        timeout(DEFAULT_RPC_TIMEOUT, read_frame(&mut recv))
            .await
            .map_err(|_| CoreError::Timeout)??;
    let ready = ready_resp.into_result()?;
    Ok((send, recv, ready))
}

fn check_cancel(send: &mut SendStream, cancel: &CancellationToken) -> Result<(), CoreError> {
    if cancel.is_cancelled() {
        let _ = send.reset(1u32.into());
        return Err(CoreError::Cancelled);
    }
    Ok(())
}
async fn send_file(
    mut file: tokio::fs::File,
    send: &mut SendStream,
    total_size: u64,
    cancel: &CancellationToken,
    progress: &mut super::file_progress::FileProgress<'_>,
) -> Result<(), CoreError> {
    let mut sent = 0;
    let mut buf = vec![0u8; TRANSFER_CHUNK_SIZE];
    while sent < total_size {
        check_cancel(send, cancel)?;
        let want = ((total_size - sent) as usize).min(TRANSFER_CHUNK_SIZE);
        let n = tokio::select! {
            _ = cancel.cancelled() => {
                let _ = send.reset(1u32.into());
                return Err(CoreError::Cancelled);
            }
            r = file.read(&mut buf[..want]) => r?,
        };
        if n == 0 {
            let _ = send.reset(1u32.into());
            return Err(CoreError::Internal(
                "读取本地文件时意外提前结束".to_string(),
            ));
        }
        tokio::select! {
            _ = cancel.cancelled() => {
                let _ = send.reset(1u32.into());
                return Err(CoreError::Cancelled);
            }
            w = send.write_all(&buf[..n]) => {
                w.map_err(|e| CoreError::NotConnected(format!("上传数据中断：{e}")))?;
            }
        }
        sent += n as u64;

        progress.report(sent);
    }
    Ok(())
}
