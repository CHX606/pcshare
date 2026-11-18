use super::file_progress::FileProgress;
use super::*;

impl CoreNode {
    /// 从对方电脑下载单个文件到本机（64 KB 分块、同目录 `.part` 临时文件、恢复修改时间、取消自动清理）。
    pub async fn download_file(
        self: &Arc<Self>,
        peer_id: &str,
        remote_path: &str,
        local_dest: &Path,
        conflict: ConflictPolicy,
        cancel: CancellationToken,
        on_progress: Option<ProgressCallback>,
    ) -> Result<FsWriteDone, CoreError> {
        let initial =
            normalize_lexical_absolute(&local_dest.to_string_lossy()).map_err(CoreError::Rpc)?;
        let display = to_display_path(&initial);
        let Some((final_clean, final_fs)) = destination(initial, conflict)? else {
            return Ok(FsWriteDone {
                target_path: display,
                bytes_written: 0,
            });
        };
        let parent = final_clean
            .parent()
            .ok_or_else(|| CoreError::InvalidArgument("无效的本地保存路径".into()))?;
        tokio::fs::create_dir_all(to_fs_path(parent)).await?;
        let conn = self.get_or_connect(peer_id).await?;
        let (mut recv, header) = open_download(&conn, remote_path).await?;
        let (mut file, mut guard) = part_file(&final_clean, parent).await?;
        let display = to_display_path(&final_clean);
        let paths = (display.clone(), remote_path.to_string());
        let total = header.transfer_length;
        let direction = TransferDirection::Download;
        let mut progress = FileProgress::new(self, peer_id, direction, paths, total, on_progress);
        receive_file(&mut recv, &mut file, total, &cancel, &mut progress).await?;
        commit_download(file, &guard.path, final_fs, &display, header.modified_ms).await?;
        guard.committed = true;
        Ok(FsWriteDone {
            target_path: display,
            bytes_written: header.transfer_length,
        })
    }
}

fn destination(
    initial_clean: PathBuf,
    conflict: ConflictPolicy,
) -> Result<Option<(PathBuf, PathBuf)>, CoreError> {
    let initial_fs = to_fs_path(&initial_clean);
    if !initial_fs.exists() {
        return Ok(Some((initial_clean, initial_fs)));
    }
    match conflict {
        ConflictPolicy::Skip => Ok(None),
        ConflictPolicy::Overwrite => Ok(Some((initial_clean, initial_fs))),
        ConflictPolicy::RenameKeepBoth => Ok(Some(pick_unique_target(&initial_clean))),
        ConflictPolicy::Fail => Err(RpcError::new(
            ErrorCode::AlreadyExists,
            format!("本地目标文件已存在：{}", to_display_path(&initial_clean)),
        )
        .into()),
    }
}

async fn open_download(
    conn: &Connection,
    remote_path: &str,
) -> Result<(RecvStream, FsReadHeader), CoreError> {
    let (mut send, mut recv) = conn
        .open_bi()
        .await
        .map_err(|e| CoreError::NotConnected(format!("打开下载通道失败：{e}")))?;

    let cmd = Command::FsRead(FsReadArgs {
        path: remote_path.to_string(),
        offset: 0,
        length: None,
    });
    write_frame(&mut send, &cmd.to_envelope()?).await?;
    send.finish()
        .map_err(|e| CoreError::Internal(format!("结束下载请求流失败：{e}")))?;

    let header_resp: ResponseEnvelope<FsReadHeader> =
        timeout(DEFAULT_RPC_TIMEOUT, read_frame(&mut recv))
            .await
            .map_err(|_| CoreError::Timeout)??;
    let header = header_resp.into_result()?;

    Ok((recv, header))
}

struct LocalPartGuard {
    path: PathBuf,
    committed: bool,
}
impl Drop for LocalPartGuard {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

async fn part_file(
    final_clean: &Path,
    parent_clean: &Path,
) -> Result<(tokio::fs::File, LocalPartGuard), CoreError> {
    let file_name = final_clean
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    let part_clean = parent_clean.join(format!(
        ".{file_name}.pcshare-{:08x}.part",
        fastrand::u32(..)
    ));
    let part_fs = to_fs_path(&part_clean);

    let part_file = tokio::fs::File::create(&part_fs).await?;
    Ok((
        part_file,
        LocalPartGuard {
            path: part_fs,
            committed: false,
        },
    ))
}

async fn receive_file(
    recv: &mut RecvStream,
    part_file: &mut tokio::fs::File,
    total: u64,
    cancel: &CancellationToken,
    progress: &mut super::file_progress::FileProgress<'_>,
) -> Result<(), CoreError> {
    let mut received = 0;
    let mut buf = vec![0; TRANSFER_CHUNK_SIZE];
    while received < total {
        if cancel.is_cancelled() {
            let _ = recv.stop(1u32.into());
            return Err(CoreError::Cancelled);
        }
        let want = ((total - received) as usize).min(TRANSFER_CHUNK_SIZE);
        let chunk_opt = tokio::select! {
            _ = cancel.cancelled() => {
                let _ = recv.stop(1u32.into());
                return Err(CoreError::Cancelled);
            }
            r = recv.read(&mut buf[..want]) => {
                r.map_err(|e| CoreError::NotConnected(format!("下载数据流中断：{e}")))?
            }
        };
        let Some(n) = chunk_opt else {
            return Err(CoreError::NotConnected(
                "下载尚未完成但对方提前结束了连接".to_string(),
            ));
        };
        part_file.write_all(&buf[..n]).await?;
        received += n as u64;

        progress.report(received);
    }
    Ok(())
}
async fn commit_download(
    mut part_file: tokio::fs::File,
    part_fs: &Path,
    final_fs: PathBuf,
    final_disp: &str,
    modified_ms: Option<u64>,
) -> Result<(), CoreError> {
    part_file.flush().await?;
    part_file.sync_all().await?;
    let std_file = part_file.into_std().await;
    if let Some(ms) = modified_ms {
        let _ = std_file.set_modified(UNIX_EPOCH + Duration::from_millis(ms));
    }
    drop(std_file);

    let part_fs_c = part_fs.to_owned();
    let final_fs_c = final_fs;
    let final_disp_c = final_disp.to_owned();
    tokio::task::spawn_blocking(move || {
        replace_file_atomically(&part_fs_c, &final_fs_c)
            .map_err(|e| map_io_to_rpc(&e, Some(&final_disp_c)))
    })
    .await
    .map_err(|e| CoreError::Internal(format!("下载改名异常：{e}")))??;

    Ok(())
}
