use super::*;

/// 临时 `.part` 文件守护：除非显式 `commit()`，否则无论正常报错、断流还是任务取消都会自动清理临时文件。
struct PartFileGuard {
    fs_path: PathBuf,
    committed: bool,
}

impl PartFileGuard {
    fn new(fs_path: PathBuf) -> Self {
        Self {
            fs_path,
            committed: false,
        }
    }

    fn commit(&mut self) {
        self.committed = true;
    }
}

impl Drop for PartFileGuard {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_file(&self.fs_path);
        }
    }
}

struct StripedWriterGuard {
    map: StripedStateMap,
    part_fs: PathBuf,
    seg_start: u64,
    seg_end: u64,
    explicit_cancel: bool,
    succeeded: bool,
    settled: bool,
}

impl StripedWriterGuard {
    fn new(map: StripedStateMap, part_fs: PathBuf, seg_start: u64) -> Self {
        Self {
            map,
            part_fs,
            seg_start,
            seg_end: seg_start,
            explicit_cancel: false,
            succeeded: false,
            settled: false,
        }
    }

    fn settle(&mut self, seg_end: u64, explicit_cancel: bool, succeeded: bool) {
        if self.settled {
            return;
        }
        self.settled = true;
        self.seg_end = seg_end;
        self.explicit_cancel = explicit_cancel;
        self.succeeded = succeeded;
        let finished = {
            let mut guard = self.map.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(st) = guard.get_mut(&self.part_fs) {
                st.segments.push((self.seg_start, self.seg_end));
                if self.explicit_cancel {
                    st.explicit_cancel = true;
                }
                if !self.succeeded {
                    st.had_error = true;
                }
                st.active_writers = st.active_writers.saturating_sub(1);
                if st.active_writers == 0 {
                    guard.remove(&self.part_fs)
                } else {
                    None
                }
            } else {
                None
            }
        };

        if let Some(mut st) = finished {
            if st.explicit_cancel {
                let _ = fs::remove_file(&self.part_fs);
            } else if st.had_error {
                st.segments.sort_unstable_by_key(|&(s, _)| s);
                let mut contig = st.base_offset;
                for (s, e) in st.segments {
                    if s <= contig && e > contig {
                        contig = e;
                    }
                }
                if let Ok(f) = open_sequential_rw(&self.part_fs) {
                    let _ = f.set_len(contig);
                }
            }
        }
    }
}

impl Drop for StripedWriterGuard {
    fn drop(&mut self) {
        if !self.settled {
            self.settle(self.seg_start, false, false);
        }
    }
}

impl FsService {
    /// 处理 `fs.read`（下载）：先回 `FsReadHeader` 控制帧，紧接着通过专职顺序预读线程 + 6 块复用缓冲池写原始文件字节（512 KB 分块）。
    pub async fn handle_read_stream<W>(
        &self,
        perms: &Permissions,
        args: FsReadArgs,
        send: &mut W,
    ) -> Result<bool, crate::error::CoreError>
    where
        W: AsyncWrite + Unpin + ?Sized,
    {
        if !perms.fs_read {
            let resp: ResponseEnvelope<FsReadHeader> =
                ResponseEnvelope::err(ErrorCode::Forbidden, "对方没有开放读取文件的权限");
            write_frame(send, &resp).await?;
            return Ok(false);
        }

        let (clean, fs_path) = match resolve_authorized_path(&args.path, perms) {
            Ok(p) => p,
            Err(err) => {
                write_frame(send, &ResponseEnvelope::<FsReadHeader>::Err(err)).await?;
                return Ok(false);
            }
        };
        let disp = to_display_path(&clean);

        let fs_path_open = fs_path.clone();
        let offset = args.offset;
        let open_res = tokio::task::spawn_blocking(move || -> std::io::Result<(File, fs::Metadata)> {
            use std::io::Seek;
            let mut f = open_sequential_read(&fs_path_open)?;
            let meta = f.metadata()?;
            if meta.is_file() && offset > 0 && offset <= meta.len() {
                f.seek(std::io::SeekFrom::Start(offset))?;
            }
            Ok((f, meta))
        })
        .await
        .map_err(|e| crate::error::CoreError::Internal(format!("打开读取文件任务异常：{e}")))?;

        let (mut file, meta) = match open_res {
            Ok(pair) => pair,
            Err(err) => {
                let rpc = map_io_to_rpc(&err, Some(&disp));
                write_frame(send, &ResponseEnvelope::<FsReadHeader>::Err(rpc)).await?;
                return Ok(false);
            }
        };

        if !meta.is_file() {
            let resp: ResponseEnvelope<FsReadHeader> =
                ResponseEnvelope::err(ErrorCode::Invalid, format!("“{disp}”不是普通文件，无法直接下载"));
            write_frame(send, &resp).await?;
            return Ok(false);
        }

        let file_size = meta.len();
        if args.offset > file_size {
            let resp: ResponseEnvelope<FsReadHeader> =
                ResponseEnvelope::err(ErrorCode::Invalid, "读取起始位置超过文件总长度");
            write_frame(send, &resp).await?;
            return Ok(false);
        }

        let remaining = file_size - args.offset;
        let transfer_length = args.length.map_or(remaining, |l| l.min(remaining));
        let modified_ms = meta.modified().ok().and_then(system_time_to_ms);

        let header = FsReadHeader {
            file_size,
            transfer_length,
            modified_ms,
        };
        write_frame(send, &ResponseEnvelope::ok(header)).await?;

        if transfer_length == 0 {
            send.flush().await?;
            return Ok(true);
        }

        // 小读取（<= 512 KB）单次同步读完即发，免去通道创建开销；大文件走 6 缓冲池全双工预读流水线
        if transfer_length <= TRANSFER_CHUNK_SIZE as u64 {
            let len_usize = transfer_length as usize;
            let buf = tokio::task::spawn_blocking(move || -> std::io::Result<Vec<u8>> {
                use std::io::Read;
                let mut b = vec![0u8; len_usize];
                let mut read_so_far = 0usize;
                while read_so_far < len_usize {
                    let n = file.read(&mut b[read_so_far..])?;
                    if n == 0 {
                        b.truncate(read_so_far);
                        break;
                    }
                    read_so_far += n;
                }
                Ok(b)
            })
            .await
            .map_err(|e| crate::error::CoreError::Internal(format!("读取小块异常：{e}")))??;
            if !buf.is_empty() {
                send.write_all(&buf).await?;
            }
            send.flush().await?;
            return Ok(true);
        }

        const POOL_CAP: usize = 6;
        let (data_tx, mut data_rx) =
            tokio::sync::mpsc::channel::<std::io::Result<(Vec<u8>, usize)>>(POOL_CAP);
        let (recycle_tx, mut recycle_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(POOL_CAP);
        for _ in 0..POOL_CAP {
            let _ = recycle_tx.try_send(vec![0u8; TRANSFER_CHUNK_SIZE]);
        }

        tokio::task::spawn_blocking(move || {
            use std::io::Read;
            let mut left = transfer_length;
            while left > 0 {
                let Some(mut buf) = recycle_rx.blocking_recv() else {
                    break;
                };
                if buf.len() < TRANSFER_CHUNK_SIZE {
                    buf.resize(TRANSFER_CHUNK_SIZE, 0);
                }
                let to_read = (left as usize).min(TRANSFER_CHUNK_SIZE);
                match file.read(&mut buf[..to_read]) {
                    Ok(0) => break,
                    Ok(n) => {
                        left -= n as u64;
                        if data_tx.blocking_send(Ok((buf, n))).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = data_tx.blocking_send(Err(e));
                        break;
                    }
                }
            }
        });

        while let Some(chunk_res) = data_rx.recv().await {
            let (buf, n) = chunk_res?;
            send.write_all(&buf[..n]).await?;
            let _ = recycle_tx.try_send(buf);
        }
        send.flush().await?;
        Ok(true)
    }

    /// 处理 `fs.write`（上传）：
    /// 1. 校验权限与冲突策略，回复 `FsWriteReady`
    /// 2. 接收原始字节流写入同目录 `.part` 临时文件（若主动取消，`PartFileGuard` 自动清除临时文件；若断网/暂停则保留）
    /// 3. 完整收齐后恢复修改时间、原子改名为目标文件，最后回复 `FsWriteDone`
    pub async fn handle_write_stream<R, W>(
        &self,
        perms: &Permissions,
        args: FsWriteArgs,
        recv: &mut R,
        send: &mut W,
    ) -> Result<bool, crate::error::CoreError>
    where
        R: AsyncRead + Unpin + ?Sized,
        W: AsyncWrite + Unpin + ?Sized,
    {
        if !perms.fs_write {
            write_frame(
                send,
                &ResponseEnvelope::<FsWriteReady>::err(
                    ErrorCode::Forbidden,
                    "对方没有开放写入文件的权限",
                ),
            )
            .await?;
            return Ok(false);
        }

        if args.offset > args.size {
            write_frame(
                send,
                &ResponseEnvelope::<FsWriteReady>::err(
                    ErrorCode::Invalid,
                    "续传偏移量超出了文件总大小",
                ),
            )
            .await?;
            return Ok(false);
        }

        let (initial_clean, initial_fs) = match resolve_authorized_path(&args.path, perms) {
            Ok(p) => p,
            Err(err) => {
                write_frame(send, &ResponseEnvelope::<FsWriteReady>::Err(err)).await?;
                return Ok(false);
            }
        };

        // 处理目标冲突策略
        let (final_clean, final_fs) = if initial_fs.exists() {
            match args.conflict {
                ConflictPolicy::Fail => {
                    write_frame(
                        send,
                        &ResponseEnvelope::<FsWriteReady>::err(
                            ErrorCode::AlreadyExists,
                            format!("目标文件已存在：{}", to_display_path(&initial_clean)),
                        ),
                    )
                    .await?;
                    return Ok(false);
                }
                ConflictPolicy::Skip => {
                    let disp = to_display_path(&initial_clean);
                    write_frame(
                        send,
                        &ResponseEnvelope::ok(FsWriteReady {
                            skipped: true,
                            target_path: disp,
                        }),
                    )
                    .await?;
                    return Ok(true);
                }
                ConflictPolicy::Overwrite => (initial_clean, initial_fs),
                ConflictPolicy::RenameKeepBoth => pick_unique_target(&initial_clean),
            }
        } else {
            (initial_clean, initial_fs)
        };

        let Some(parent_clean) = final_clean.parent() else {
            write_frame(
                send,
                &ResponseEnvelope::<FsWriteReady>::err(ErrorCode::Invalid, "无效的目标文件路径"),
            )
            .await?;
            return Ok(false);
        };
        let parent_fs = to_fs_path(parent_clean);
        if let Err(err) = tokio::fs::create_dir_all(&parent_fs).await {
            let rpc = map_io_to_rpc(&err, Some(&to_display_path(parent_clean)));
            write_frame(send, &ResponseEnvelope::<FsWriteReady>::Err(rpc)).await?;
            return Ok(false);
        }

        let file_name = final_clean
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        let part_clean = parent_clean.join(deterministic_part_name(&file_name));
        let part_fs = to_fs_path(&part_clean);

        if args.range_length.is_some() || args.defer_finalize {
            return self
                .handle_striped_write_stream(
                    args,
                    final_clean,
                    final_fs,
                    part_clean,
                    part_fs,
                    recv,
                    send,
                )
                .await;
        }

        let part_fs_open = part_fs.clone();
        let offset = args.offset;
        let open_part_res = tokio::task::spawn_blocking(move || -> Result<File, Result<RpcError, std::io::Error>> {
            use std::io::Seek;
            if offset > 0 {
                let mut f = open_sequential_rw(&part_fs_open).map_err(Err)?;
                let cur_len = f.metadata().map(|m| m.len()).unwrap_or(0);
                if cur_len < offset {
                    return Err(Ok(RpcError::new(
                        ErrorCode::Invalid,
                        "临时续传文件长度不足，请从头重传",
                    )));
                }
                let _ = f.set_len(offset);
                f.seek(std::io::SeekFrom::Start(offset)).map_err(Err)?;
                Ok(f)
            } else {
                create_sequential_write(&part_fs_open).map_err(Err)
            }
        })
        .await
        .map_err(|e| crate::error::CoreError::Internal(format!("打开临时文件任务异常：{e}")))?;

        let part_file = match open_part_res {
            Ok(f) => f,
            Err(Ok(rpc_err)) => {
                write_frame(send, &ResponseEnvelope::<FsWriteReady>::Err(rpc_err)).await?;
                return Ok(false);
            }
            Err(Err(io_err)) => {
                let disp_target = if args.offset > 0 {
                    to_display_path(&part_clean)
                } else {
                    to_display_path(&final_clean)
                };
                let rpc = map_io_to_rpc(&io_err, Some(&disp_target));
                write_frame(send, &ResponseEnvelope::<FsWriteReady>::Err(rpc)).await?;
                return Ok(false);
            }
        };
        let mut guard = PartFileGuard::new(part_fs.clone());

        let final_disp = to_display_path(&final_clean);
        write_frame(
            send,
            &ResponseEnvelope::ok(FsWriteReady {
                skipped: false,
                target_path: final_disp.clone(),
            }),
        )
        .await?;

        const POOL_CAP: usize = 6;
        let (write_tx, mut write_rx) = tokio::sync::mpsc::channel::<(Vec<u8>, usize)>(POOL_CAP);
        let (recycle_tx, mut recycle_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(POOL_CAP);
        for _ in 0..POOL_CAP {
            let _ = recycle_tx.try_send(vec![0u8; TRANSFER_CHUNK_SIZE]);
        }

        let writer_handle = tokio::task::spawn_blocking(move || -> std::io::Result<File> {
            use std::io::Write;
            let mut f = part_file;
            while let Some((buf, n)) = write_rx.blocking_recv() {
                f.write_all(&buf[..n])?;
                let _ = recycle_tx.try_send(buf);
            }
            f.flush()?;
            Ok(f)
        });

        let mut received = args.offset;
        let mut recv_err: Option<crate::error::CoreError> = None;
        let mut explicit_cancel = false;

        while received < args.size {
            let Some(mut buf) = recycle_rx.recv().await else {
                break;
            };
            if buf.len() < TRANSFER_CHUNK_SIZE {
                buf.resize(TRANSFER_CHUNK_SIZE, 0);
            }
            let want = ((args.size - received) as usize).min(TRANSFER_CHUNK_SIZE);
            let mut filled = 0usize;
            while filled < want {
                match recv.read(&mut buf[filled..want]).await {
                    Ok(0) => {
                        recv_err = Some(crate::error::CoreError::NotConnected(
                            "传输中途连接断开，已保留断点数据".to_string(),
                        ));
                        break;
                    }
                    Ok(n) => {
                        filled += n;
                    }
                    Err(io_err) => {
                        explicit_cancel = is_explicit_cancel_error(&io_err);
                        recv_err = Some(if explicit_cancel {
                            crate::error::CoreError::Cancelled
                        } else {
                            crate::error::CoreError::Io(io_err)
                        });
                        break;
                    }
                }
            }

            if filled > 0 {
                received += filled as u64;
                if write_tx.send((buf, filled)).await.is_err() {
                    break;
                }
            }
            if recv_err.is_some() {
                break;
            }
        }
        drop(write_tx);

        let writer_outcome = writer_handle
            .await
            .map_err(|e| crate::error::CoreError::Internal(format!("写入临时文件线程异常：{e}")))?;

        if let Some(err) = recv_err {
            if !explicit_cancel {
                guard.commit();
            }
            return Err(err);
        }

        let std_file = writer_outcome?;
        let modified_ms = args.modified_ms;
        let part_fs_for_rename = part_fs.clone();
        let final_fs_for_rename = final_fs.clone();
        let final_disp_for_err = final_disp.clone();
        let rename_res = tokio::task::spawn_blocking(move || -> Result<(), RpcError> {
            std_file
                .sync_all()
                .map_err(|e| map_io_to_rpc(&e, Some(&final_disp_for_err)))?;
            if let Some(ms) = modified_ms {
                let mtime = UNIX_EPOCH + Duration::from_millis(ms);
                let _ = std_file.set_modified(mtime);
            }
            drop(std_file);
            replace_file_atomically(&part_fs_for_rename, &final_fs_for_rename)
                .map_err(|e| map_io_to_rpc(&e, Some(&final_disp_for_err)))
        })
        .await
        .map_err(|e| crate::error::CoreError::Internal(format!("完成文件写入改名异常：{e}")))?;

        match rename_res {
            Ok(()) => {
                guard.commit();
                write_frame(
                    send,
                    &ResponseEnvelope::ok(FsWriteDone {
                        target_path: final_disp,
                        bytes_written: received,
                    }),
                )
                .await?;
                Ok(true)
            }
            Err(rpc_err) => {
                // 目标可能被短暂占用，保留已完整接收的 .pcshare.part 便于续传秒完成
                guard.commit();
                write_frame(send, &ResponseEnvelope::<FsWriteDone>::Err(rpc_err)).await?;
                Ok(false)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_striped_write_stream<R, W>(
        &self,
        args: FsWriteArgs,
        final_clean: PathBuf,
        final_fs: PathBuf,
        part_clean: PathBuf,
        part_fs: PathBuf,
        recv: &mut R,
        send: &mut W,
    ) -> Result<bool, crate::error::CoreError>
    where
        R: AsyncRead + Unpin + ?Sized,
        W: AsyncWrite + Unpin + ?Sized,
    {
        let write_len = match args.range_length {
            Some(len) => {
                let Some(end) = args.offset.checked_add(len) else {
                    write_frame(
                        send,
                        &ResponseEnvelope::<FsWriteReady>::err(
                            ErrorCode::Invalid,
                            "分片写入长度溢出",
                        ),
                    )
                    .await?;
                    return Ok(false);
                };
                if end > args.size {
                    write_frame(
                        send,
                        &ResponseEnvelope::<FsWriteReady>::err(
                            ErrorCode::Invalid,
                            "分片写入范围超出文件总大小",
                        ),
                    )
                    .await?;
                    return Ok(false);
                }
                len
            }
            None => args.size - args.offset,
        };
        let target_end = args.offset + write_len;
        let final_disp = to_display_path(&final_clean);

        // 收尾阶段：所有分片已写完，仅校验 `.pcshare.part` 总长度后落盘并原子改名
        if !args.defer_finalize && write_len == 0 && args.offset == args.size {
            let part_fs_fin = part_fs.clone();
            let final_fs_fin = final_fs.clone();
            let disp_fin = final_disp.clone();
            let expected_size = args.size;
            let modified_ms = args.modified_ms;
            let check_res = tokio::task::spawn_blocking(move || -> Result<File, RpcError> {
                let f = if expected_size == 0 && !part_fs_fin.exists() {
                    create_sequential_write(&part_fs_fin)
                        .map_err(|e| map_io_to_rpc(&e, Some(&disp_fin)))?
                } else {
                    open_sequential_rw(&part_fs_fin)
                        .map_err(|e| map_io_to_rpc(&e, Some(&disp_fin)))?
                };
                let cur_len = f
                    .metadata()
                    .map_err(|e| map_io_to_rpc(&e, Some(&disp_fin)))?
                    .len();
                if cur_len != expected_size {
                    return Err(RpcError::new(
                        ErrorCode::Invalid,
                        format!("分片临时文件长度不匹配（当前 {cur_len}，期望 {expected_size}）"),
                    ));
                }
                Ok(f)
            })
            .await
            .map_err(|e| crate::error::CoreError::Internal(format!("检查分片临时文件异常：{e}")))?;

            let std_file = match check_res {
                Ok(f) => f,
                Err(rpc_err) => {
                    write_frame(send, &ResponseEnvelope::<FsWriteReady>::Err(rpc_err)).await?;
                    return Ok(false);
                }
            };

            write_frame(
                send,
                &ResponseEnvelope::ok(FsWriteReady {
                    skipped: false,
                    target_path: final_disp.clone(),
                }),
            )
            .await?;

            let part_fs_ren = part_fs.clone();
            let disp_ren = final_disp.clone();
            let rename_res = tokio::task::spawn_blocking(move || -> Result<(), RpcError> {
                std_file
                    .sync_all()
                    .map_err(|e| map_io_to_rpc(&e, Some(&disp_ren)))?;
                if let Some(ms) = modified_ms {
                    let _ = std_file.set_modified(UNIX_EPOCH + Duration::from_millis(ms));
                }
                drop(std_file);
                replace_file_atomically(&part_fs_ren, &final_fs_fin)
                    .map_err(|e| map_io_to_rpc(&e, Some(&disp_ren)))
            })
            .await
            .map_err(|e| crate::error::CoreError::Internal(format!("分片收尾改名异常：{e}")))?;

            return match rename_res {
                Ok(()) => {
                    write_frame(
                        send,
                        &ResponseEnvelope::ok(FsWriteDone {
                            target_path: final_disp,
                            bytes_written: args.size,
                        }),
                    )
                    .await?;
                    Ok(true)
                }
                Err(rpc_err) => {
                    write_frame(send, &ResponseEnvelope::<FsWriteDone>::Err(rpc_err)).await?;
                    Ok(false)
                }
            };
        }

        let is_first_writer = {
            let mut map = self.striped_states.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(st) = map.get_mut(&part_fs) {
                st.active_writers += 1;
                false
            } else {
                map.insert(
                    part_fs.clone(),
                    StripedPartState {
                        active_writers: 1,
                        base_offset: args.offset,
                        segments: Vec::with_capacity(4),
                        explicit_cancel: false,
                        had_error: false,
                    },
                );
                true
            }
        };
        let mut stripe_guard =
            StripedWriterGuard::new(self.striped_states.clone(), part_fs.clone(), args.offset);

        let part_fs_open = part_fs.clone();
        let offset = args.offset;
        let open_res = tokio::task::spawn_blocking(
            move || -> Result<File, Result<RpcError, std::io::Error>> {
                if is_first_writer {
                    if offset > 0 {
                        let f = open_sequential_rw(&part_fs_open).map_err(Err)?;
                        let cur_len = f.metadata().map(|m| m.len()).unwrap_or(0);
                        if cur_len < offset {
                            return Err(Ok(RpcError::new(
                                ErrorCode::Invalid,
                                "临时续传文件长度不足，请从头重传",
                            )));
                        }
                        let _ = f.set_len(offset);
                        Ok(f)
                    } else {
                        create_sequential_write(&part_fs_open).map_err(Err)
                    }
                } else {
                    open_or_create_sequential_rw(&part_fs_open).map_err(Err)
                }
            },
        )
        .await
        .map_err(|e| crate::error::CoreError::Internal(format!("打开分片临时文件异常：{e}")))?;

        let part_file = match open_res {
            Ok(f) => f,
            Err(Ok(rpc_err)) => {
                stripe_guard.settle(args.offset, false, false);
                write_frame(send, &ResponseEnvelope::<FsWriteReady>::Err(rpc_err)).await?;
                return Ok(false);
            }
            Err(Err(io_err)) => {
                stripe_guard.settle(args.offset, false, false);
                let disp_target = if args.offset > 0 {
                    to_display_path(&part_clean)
                } else {
                    final_disp
                };
                let rpc = map_io_to_rpc(&io_err, Some(&disp_target));
                write_frame(send, &ResponseEnvelope::<FsWriteReady>::Err(rpc)).await?;
                return Ok(false);
            }
        };

        write_frame(
            send,
            &ResponseEnvelope::ok(FsWriteReady {
                skipped: false,
                target_path: final_disp.clone(),
            }),
        )
        .await?;

        const POOL_CAP: usize = 6;
        let (write_tx, mut write_rx) =
            tokio::sync::mpsc::channel::<(Vec<u8>, usize, u64)>(POOL_CAP);
        let (recycle_tx, mut recycle_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(POOL_CAP);
        for _ in 0..POOL_CAP {
            let _ = recycle_tx.try_send(vec![0u8; TRANSFER_CHUNK_SIZE]);
        }

        let writer_handle = tokio::task::spawn_blocking(move || -> std::io::Result<File> {
            use std::io::Write;
            let mut f = part_file;
            while let Some((buf, n, write_off)) = write_rx.blocking_recv() {
                write_all_at_offset(&mut f, &buf[..n], write_off)?;
                let _ = recycle_tx.try_send(buf);
            }
            f.flush()?;
            Ok(f)
        });

        let mut received = args.offset;
        let mut recv_err: Option<crate::error::CoreError> = None;
        let mut explicit_cancel = false;

        while received < target_end {
            let Some(mut buf) = recycle_rx.recv().await else {
                break;
            };
            if buf.len() < TRANSFER_CHUNK_SIZE {
                buf.resize(TRANSFER_CHUNK_SIZE, 0);
            }
            let want = ((target_end - received) as usize).min(TRANSFER_CHUNK_SIZE);
            let mut filled = 0usize;
            while filled < want {
                match recv.read(&mut buf[filled..want]).await {
                    Ok(0) => {
                        recv_err = Some(crate::error::CoreError::NotConnected(
                            "分片传输中途连接断开，已保留连续断点数据".to_string(),
                        ));
                        break;
                    }
                    Ok(n) => {
                        filled += n;
                    }
                    Err(io_err) => {
                        explicit_cancel = is_explicit_cancel_error(&io_err);
                        recv_err = Some(if explicit_cancel {
                            crate::error::CoreError::Cancelled
                        } else {
                            crate::error::CoreError::Io(io_err)
                        });
                        break;
                    }
                }
            }

            if filled > 0 {
                let chunk_off = received;
                received += filled as u64;
                if write_tx.send((buf, filled, chunk_off)).await.is_err() {
                    break;
                }
            }
            if recv_err.is_some() {
                break;
            }
        }
        drop(write_tx);

        let writer_outcome = writer_handle
            .await
            .map_err(|e| crate::error::CoreError::Internal(format!("分片写入线程异常：{e}")))?;

        if let Some(err) = recv_err {
            drop(writer_outcome);
            stripe_guard.settle(received, explicit_cancel, false);
            return Err(err);
        }

        let std_file = match writer_outcome {
            Ok(f) => f,
            Err(io_err) => {
                stripe_guard.settle(received, false, false);
                return Err(crate::error::CoreError::Io(io_err));
            }
        };

        if args.defer_finalize {
            drop(std_file);
            stripe_guard.settle(received, false, true);
            write_frame(
                send,
                &ResponseEnvelope::ok(FsWriteDone {
                    target_path: final_disp,
                    bytes_written: received,
                }),
            )
            .await?;
            return Ok(true);
        }

        let modified_ms = args.modified_ms;
        let part_fs_for_rename = part_fs.clone();
        let final_fs_for_rename = final_fs.clone();
        let final_disp_for_err = final_disp.clone();
        let rename_res = tokio::task::spawn_blocking(move || -> Result<(), RpcError> {
            std_file
                .sync_all()
                .map_err(|e| map_io_to_rpc(&e, Some(&final_disp_for_err)))?;
            if let Some(ms) = modified_ms {
                let _ = std_file.set_modified(UNIX_EPOCH + Duration::from_millis(ms));
            }
            drop(std_file);
            replace_file_atomically(&part_fs_for_rename, &final_fs_for_rename)
                .map_err(|e| map_io_to_rpc(&e, Some(&final_disp_for_err)))
        })
        .await
        .map_err(|e| crate::error::CoreError::Internal(format!("完成文件写入改名异常：{e}")))?;

        stripe_guard.settle(received, false, true);
        match rename_res {
            Ok(()) => {
                write_frame(
                    send,
                    &ResponseEnvelope::ok(FsWriteDone {
                        target_path: final_disp,
                        bytes_written: received,
                    }),
                )
                .await?;
                Ok(true)
            }
            Err(rpc_err) => {
                write_frame(send, &ResponseEnvelope::<FsWriteDone>::Err(rpc_err)).await?;
                Ok(false)
            }
        }
    }
}
