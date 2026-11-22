use super::*;

impl FsService {
    // ────────────────────────── 阶段 2 文件增强接口 ──────────────────────────

    pub async fn hash(&self, perms: &Permissions, args: FsHashArgs) -> Result<FsHashReply, RpcError> {
        if !perms.fs_read {
            return Err(RpcError::new(ErrorCode::Forbidden, "对方没有开放读取文件的权限"));
        }
        let (clean, fs_path) = resolve_authorized_path(&args.path, perms)?;
        let disp = to_display_path(&clean);
        tokio::task::spawn_blocking(move || {
            let meta = fs::metadata(&fs_path).map_err(|e| map_io_to_rpc(&e, Some(&disp)))?;
            if meta.is_dir() {
                return Err(RpcError::new(
                    ErrorCode::Invalid,
                    format!("不能对文件夹计算单文件校验值：{disp}"),
                ));
            }
            let size = meta.len();
            let modified_ms = meta.modified().ok().and_then(system_time_to_ms);
            let limit = args.length.unwrap_or(size).min(size);
            let blake3_hex = hash_file_prefix_sync(&fs_path, limit)
                .map_err(|e| map_io_to_rpc(&e, Some(&disp)))?;
            Ok(FsHashReply {
                size,
                blake3_hex,
                modified_ms,
            })
        })
        .await
        .map_err(|e| RpcError::new(ErrorCode::Internal, format!("计算文件校验值异常：{e}")))?
    }

    pub async fn probe_resume(
        &self,
        perms: &Permissions,
        args: FsProbeResumeArgs,
    ) -> Result<FsProbeResumeReply, RpcError> {
        if !perms.fs_write {
            return Err(RpcError::new(ErrorCode::Forbidden, "对方没有开放写入文件的权限"));
        }
        validate_single_name(&args.file_name)?;
        let (dir_clean, _) = resolve_authorized_path(&args.dest_dir, perms)?;
        let initial_clean = dir_clean.join(&args.file_name);
        let initial_fs = to_fs_path(&initial_clean);
        let dest_exists = initial_fs.exists();

        let final_clean = if dest_exists && args.conflict == ConflictPolicy::RenameKeepBoth {
            pick_unique_target(&initial_clean).0
        } else {
            initial_clean
        };
        let target_disp = to_display_path(&final_clean);
        let file_name = final_clean
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let part_clean = dir_clean.join(deterministic_part_name(&file_name));
        let part_fs = to_fs_path(&part_clean);

        tokio::task::spawn_blocking(move || {
            if let Ok(meta) = fs::metadata(&part_fs)
                && meta.is_file()
                && meta.len() > 0
            {
                let len = meta.len();
                if let Ok(hex) = hash_file_prefix_sync(&part_fs, len) {
                    return Ok(FsProbeResumeReply {
                        dest_exists,
                        target_path: target_disp,
                        part_bytes: len,
                        part_blake3_hex: Some(hex),
                    });
                }
            }
            Ok(FsProbeResumeReply {
                dest_exists,
                target_path: target_disp,
                part_bytes: 0,
                part_blake3_hex: None,
            })
        })
        .await
        .map_err(|e| RpcError::new(ErrorCode::Internal, format!("查询续传状态异常：{e}")))?
    }

    pub async fn walk_local(
        &self,
        perms: &Permissions,
        sources: &[String],
    ) -> Result<Vec<WalkEntry>, RpcError> {
        if !perms.fs_read {
            return Err(RpcError::new(ErrorCode::Forbidden, "没有读取文件的权限"));
        }
        let mut roots = Vec::with_capacity(sources.len());
        for s in sources {
            let (clean, fs_p) = resolve_authorized_path(s, perms)?;
            if !fs_p.exists() {
                return Err(RpcError::new(
                    ErrorCode::NotFound,
                    format!("找不到要传输的文件或文件夹：{}", to_display_path(&clean)),
                ));
            }
            roots.push((clean, fs_p));
        }

        tokio::task::spawn_blocking(move || {
            let mut out = Vec::new();
            for (clean, fs_p) in roots {
                collect_walk_entries_sync(&clean, &fs_p, &mut out)?;
            }
            Ok(out)
        })
        .await
        .map_err(|e| RpcError::new(ErrorCode::Internal, format!("遍历文件夹异常：{e}")))?
    }

    pub async fn walk_to_stream<W: AsyncWrite + Unpin + ?Sized>(
        &self,
        perms: &Permissions,
        args: FsWalkArgs,
        send: &mut W,
    ) -> Result<bool, crate::error::CoreError> {
        self.walk_to_stream_opts(perms, args, send, false).await
    }

    pub async fn walk_to_stream_opts<W: AsyncWrite + Unpin + ?Sized>(
        &self,
        perms: &Permissions,
        args: FsWalkArgs,
        send: &mut W,
        compress: bool,
    ) -> Result<bool, crate::error::CoreError> {
        let entries = match self.walk_local(perms, &args.sources).await {
            Ok(list) => list,
            Err(err) => {
                write_frame(send, &ResponseEnvelope::<FsWalkBatch>::Err(err)).await?;
                return Ok(false);
            }
        };
        if entries.is_empty() {
            write_frame(
                send,
                &ResponseEnvelope::ok(FsWalkBatch {
                    entries: Vec::new(),
                    done: true,
                }),
            )
            .await?;
            return Ok(true);
        }
        let total = entries.len();
        let mut idx = 0usize;
        while idx < total {
            let end = (idx + DEFAULT_LIST_BATCH_SIZE).min(total);
            let batch = FsWalkBatch {
                entries: entries[idx..end].to_vec(),
                done: end == total,
            };
            write_frame_maybe_compressed(send, &ResponseEnvelope::ok(batch), compress).await?;
            idx = end;
        }
        Ok(true)
    }

    /// 接收单条数据流里连续发来的一批小文件与子目录（任务 2.4 小文件优化）。
    pub async fn handle_write_batch_stream<R, W>(
        &self,
        perms: &Permissions,
        args: FsWriteBatchArgs,
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
                &ResponseEnvelope::<()>::err(ErrorCode::Forbidden, "对方没有开放写入文件的权限"),
            )
            .await?;
            return Ok(false);
        }
        let (dest_clean, dest_fs) = match resolve_authorized_path(&args.dest_dir, perms) {
            Ok(p) => p,
            Err(err) => {
                write_frame(send, &ResponseEnvelope::<()>::Err(err)).await?;
                return Ok(false);
            }
        };
        if let Err(e) = tokio::fs::create_dir_all(&dest_fs).await {
            let rpc = map_io_to_rpc(&e, Some(&to_display_path(&dest_clean)));
            write_frame(send, &ResponseEnvelope::<()>::Err(rpc)).await?;
            return Ok(false);
        }
        // 回复握手成功
        write_frame(send, &ResponseEnvelope::ok(())).await?;

        enum WriteJob {
            Dir(PathBuf),
            File {
                rel_path: String,
                target_clean: PathBuf,
                conflict: ConflictPolicy,
                modified_ms: Option<u64>,
                expected_hex: Option<String>,
                data: Vec<u8>,
            },
        }

        let (job_tx, job_rx) = tokio::sync::mpsc::channel::<WriteJob>(256);
        let shared_rx = Arc::new(Mutex::new(job_rx));
        let abort = CancellationToken::new();
        let mut worker_handles = Vec::with_capacity(4);
        for _ in 0..4 {
            let rx_c = shared_rx.clone();
            let abort_c = abort.clone();
            worker_handles.push(tokio::task::spawn_blocking(
                move || -> Result<FsWriteBatchReply, RpcError> {
                    let mut written_files = 0u64;
                    let mut skipped_files = 0u64;
                    let mut written_bytes = 0u64;
                    let mut local_dirs = std::collections::HashSet::<PathBuf>::new();

                    loop {
                        if abort_c.is_cancelled() {
                            break;
                        }
                        let job = {
                            let mut guard = rx_c.lock().unwrap_or_else(|e| e.into_inner());
                            guard.blocking_recv()
                        };
                        let Some(job) = job else {
                            break;
                        };
                        if abort_c.is_cancelled() {
                            break;
                        }
                        let step_res: Result<(), RpcError> = match job {
                            WriteJob::Dir(dir_fs) => {
                                if local_dirs.insert(dir_fs.clone()) {
                                    fs::create_dir_all(&dir_fs).map_err(|e| map_io_to_rpc(&e, None))
                                } else {
                                    Ok(())
                                }
                            }
                            WriteJob::File {
                                rel_path,
                                target_clean,
                                conflict,
                                modified_ms,
                                expected_hex,
                                data,
                            } => (|| -> Result<(), RpcError> {
                                if let Some(ref exp_hex) = expected_hex {
                                    let got_hex = blake3::hash(&data).to_hex().to_string();
                                    if &got_hex != exp_hex {
                                        return Err(RpcError::new(
                                            ErrorCode::Io,
                                            format!("传输文件校验不一致：{rel_path}"),
                                        ));
                                    }
                                }
                                let disp = to_display_path(&target_clean);
                                let initial_fs = to_fs_path(&target_clean);
                                let final_fs = match conflict {
                                    ConflictPolicy::Overwrite => initial_fs,
                                    ConflictPolicy::Skip => {
                                        if initial_fs.exists() {
                                            skipped_files += 1;
                                            return Ok(());
                                        }
                                        initial_fs
                                    }
                                    ConflictPolicy::Fail => {
                                        if initial_fs.exists() {
                                            return Err(RpcError::new(
                                                ErrorCode::AlreadyExists,
                                                format!("目标文件已存在：{disp}"),
                                            ));
                                        }
                                        initial_fs
                                    }
                                    ConflictPolicy::RenameKeepBoth => {
                                        if initial_fs.exists() {
                                            pick_unique_target(&target_clean).1
                                        } else {
                                            initial_fs
                                        }
                                    }
                                };
                                if let Some(parent) = final_fs.parent()
                                    && local_dirs.insert(parent.to_path_buf())
                                {
                                    fs::create_dir_all(parent)
                                        .map_err(|e| map_io_to_rpc(&e, None))?;
                                }
                                if let Ok(meta) = fs::symlink_metadata(&final_fs)
                                    && meta.file_type().is_symlink()
                                {
                                    return Err(RpcError::new(
                                        ErrorCode::Forbidden,
                                        format!("拒绝覆盖符号链接目标：{disp}"),
                                    ));
                                }
                                let mut f = File::create(&final_fs)
                                    .map_err(|e| map_io_to_rpc(&e, Some(&disp)))?;
                                if !data.is_empty() {
                                    std::io::Write::write_all(&mut f, &data)
                                        .map_err(|e| map_io_to_rpc(&e, Some(&disp)))?;
                                }
                                if let Some(ms) = modified_ms {
                                    let _ = f.set_modified(UNIX_EPOCH + Duration::from_millis(ms));
                                }
                                written_files += 1;
                                written_bytes += data.len() as u64;
                                Ok(())
                            })(),
                        };
                        if let Err(err) = step_res {
                            abort_c.cancel();
                            return Err(err);
                        }
                    }
                    Ok(FsWriteBatchReply {
                        written_files,
                        skipped_files,
                        written_bytes,
                    })
                },
            ));
        }

        let mut stream_err: Option<RpcError> = None;
        loop {
            if abort.is_cancelled() {
                break;
            }
            let item: BatchFileItem = tokio::select! {
                _ = abort.cancelled() => break,
                r = read_frame::<_, BatchFileItem>(recv) => r?,
            };
            if item.done {
                break;
            }
            let rel_clean = match sanitize_rel_subpath(&item.rel_path) {
                Ok(p) => p,
                Err(rpc) => {
                    stream_err = Some(rpc);
                    abort.cancel();
                    break;
                }
            };
            let target_clean = dest_clean.join(&rel_clean);
            let target_disp = to_display_path(&target_clean);
            let (auth_clean, auth_fs) = match resolve_authorized_path(&target_disp, perms) {
                Ok(pair) => pair,
                Err(rpc) => {
                    stream_err = Some(rpc);
                    abort.cancel();
                    break;
                }
            };
            if item.dir {
                tokio::select! {
                    _ = abort.cancelled() => break,
                    res = job_tx.send(WriteJob::Dir(auth_fs)) => {
                        if res.is_err() {
                            break;
                        }
                    }
                }
                continue;
            }

            if item.size > MAX_BATCH_FILE_BYTES {
                stream_err = Some(RpcError::new(
                    ErrorCode::Invalid,
                    format!(
                        "批传单个文件过大（{} 字节，上限 {} 字节）：{}",
                        item.size, MAX_BATCH_FILE_BYTES, item.rel_path
                    ),
                ));
                abort.cancel();
                break;
            }

            let mut data = vec![0u8; item.size as usize];
            if item.size > 0 {
                tokio::select! {
                    _ = abort.cancelled() => break,
                    r = recv.read_exact(&mut data) => {
                        r?;
                    }
                }
            }
            let job = WriteJob::File {
                rel_path: item.rel_path,
                target_clean: auth_clean,
                conflict: args.conflict,
                modified_ms: item.modified_ms,
                expected_hex: item.blake3_hex,
                data,
            };
            tokio::select! {
                _ = abort.cancelled() => break,
                res = job_tx.send(job) => {
                    if res.is_err() {
                        break;
                    }
                }
            }
        }
        drop(job_tx);

        let mut agg_reply = FsWriteBatchReply {
            written_files: 0,
            skipped_files: 0,
            written_bytes: 0,
        };
        let mut writer_res: Result<FsWriteBatchReply, RpcError> = Ok(agg_reply.clone());
        for h in worker_handles {
            let sub = h
                .await
                .map_err(|e| crate::error::CoreError::Internal(format!("批量写入线程异常：{e}")))?;
            match sub {
                Ok(r) => {
                    agg_reply.written_files += r.written_files;
                    agg_reply.skipped_files += r.skipped_files;
                    agg_reply.written_bytes += r.written_bytes;
                }
                Err(err) if writer_res.is_ok() => {
                    writer_res = Err(err);
                }
                Err(_) => {}
            }
        }
        if writer_res.is_ok() {
            writer_res = Ok(agg_reply);
        }

        let final_res = match stream_err {
            Some(err) => Err(err),
            None => writer_res,
        };
        match final_res {
            Ok(rep) => {
                write_frame(send, &ResponseEnvelope::ok(rep)).await?;
                Ok(true)
            }
            Err(err) => {
                write_frame(send, &ResponseEnvelope::<FsWriteBatchReply>::Err(err)).await?;
                Ok(false)
            }
        }
    }

    /// 在单条流上连续读取一批小文件并发回（任务 2.4 下载端小文件优化）。
    /// 支持并发预读与单文件超时，读取任一文件失败或超过大小上限立即中止报错（绝不静默跳过）。
    pub async fn handle_read_batch_stream<W>(
        &self,
        perms: &Permissions,
        args: FsReadBatchArgs,
        send: &mut W,
    ) -> Result<bool, crate::error::CoreError>
    where
        W: AsyncWrite + Unpin + ?Sized,
    {
        if !perms.fs_read {
            write_frame(
                send,
                &ResponseEnvelope::<()>::err(ErrorCode::Forbidden, "对方没有开放读取文件的权限"),
            )
            .await?;
            return Ok(false);
        }
        let mut resolved = Vec::with_capacity(args.sources.len());
        for s in &args.sources {
            match resolve_authorized_path(s, perms) {
                Ok((clean, fs_p)) => resolved.push((s.clone(), to_display_path(&clean), fs_p)),
                Err(rpc) => {
                    write_frame(send, &ResponseEnvelope::<()>::Err(rpc)).await?;
                    return Ok(false);
                }
            }
        }

        // 握手前用 4 线程并行核验所有请求项存在、为普通文件且未超出批传单文件上限
        let resolved = Arc::new(resolved);
        let precheck_ref = resolved.clone();
        let precheck = tokio::task::spawn_blocking(move || -> Result<(), RpcError> {
            let next_i = std::sync::atomic::AtomicUsize::new(0);
            let first_err: Mutex<Option<RpcError>> = Mutex::new(None);
            let worker_n = precheck_ref.len().clamp(1, 4);
            std::thread::scope(|s| {
                for _ in 0..worker_n {
                    let list = &precheck_ref;
                    let idx_ref = &next_i;
                    let err_ref = &first_err;
                    s.spawn(move || {
                        loop {
                            if err_ref.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
                                break;
                            }
                            let i = idx_ref.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            let Some((_, disp, fs_p)) = list.get(i) else {
                                break;
                            };
                            let step: Result<(), RpcError> = (|| {
                                let meta = fs::metadata(fs_p)
                                    .map_err(|e| map_io_to_rpc(&e, Some(disp)))?;
                                if !meta.is_file() {
                                    return Err(RpcError::new(
                                        ErrorCode::Invalid,
                                        format!("批量读取遇到非普通文件：{disp}"),
                                    ));
                                }
                                if meta.len() > MAX_BATCH_FILE_BYTES {
                                    return Err(RpcError::new(
                                        ErrorCode::Invalid,
                                        format!(
                                            "文件超过小文件批传上限（{} 字节，上限 {} 字节）：{disp}",
                                            meta.len(),
                                            MAX_BATCH_FILE_BYTES
                                        ),
                                    ));
                                }
                                Ok(())
                            })();
                            if let Err(e) = step {
                                let mut g = err_ref.lock().unwrap_or_else(|p| p.into_inner());
                                if g.is_none() {
                                    *g = Some(e);
                                }
                                break;
                            }
                        }
                    });
                }
            });
            match first_err.into_inner().unwrap_or_else(|e| e.into_inner()) {
                Some(err) => Err(err),
                None => Ok(()),
            }
        })
        .await
        .map_err(|e| crate::error::CoreError::Internal(format!("批量读取校验异常：{e}")))?;
        if let Err(rpc) = precheck {
            write_frame(send, &ResponseEnvelope::<()>::Err(rpc)).await?;
            return Ok(false);
        }

        write_frame(send, &ResponseEnvelope::ok(())).await?;

        // 4 个常驻后台线程按原子下标拉取文件、单句柄读取内容与元数据，并通过 256 KB 聚合缓冲发送
        const READ_WORKERS: usize = 4;
        const COALESCE_BYTES: usize = 256 * 1024;
        let (item_tx, mut item_rx) = tokio::sync::mpsc::channel::<
            Result<(BatchReadItemHeader, Vec<u8>), RpcError>,
        >(128);
        let cursor = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let abort = CancellationToken::new();

        let worker_count = resolved.len().clamp(1, READ_WORKERS);
        for _ in 0..worker_count {
            let list_c = resolved.clone();
            let cur_c = cursor.clone();
            let tx_c = item_tx.clone();
            let abort_c = abort.clone();
            tokio::task::spawn_blocking(move || {
                use std::io::Read;
                loop {
                    if abort_c.is_cancelled() {
                        break;
                    }
                    let idx = cur_c.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some((src_str, disp, fs_p)) = list_c.get(idx) else {
                        break;
                    };
                    let read_one = (|| -> Result<(BatchReadItemHeader, Vec<u8>), RpcError> {
                        let mut f =
                            open_sequential_read(fs_p).map_err(|e| map_io_to_rpc(&e, Some(disp)))?;
                        let meta = f.metadata().map_err(|e| map_io_to_rpc(&e, Some(disp)))?;
                        if !meta.is_file() {
                            return Err(RpcError::new(
                                ErrorCode::Invalid,
                                format!("批量读取遇到非普通文件：{disp}"),
                            ));
                        }
                        if meta.len() > MAX_BATCH_FILE_BYTES {
                            return Err(RpcError::new(
                                ErrorCode::Invalid,
                                format!("文件大小已超过批传上限：{disp}"),
                            ));
                        }
                        let mtime = meta.modified().ok().and_then(system_time_to_ms);
                        let mut bytes = Vec::with_capacity(meta.len() as usize);
                        f.read_to_end(&mut bytes)
                            .map_err(|e| map_io_to_rpc(&e, Some(disp)))?;
                        if bytes.len() as u64 > MAX_BATCH_FILE_BYTES {
                            return Err(RpcError::new(
                                ErrorCode::Invalid,
                                format!("文件大小已超过批传上限：{disp}"),
                            ));
                        }
                        let hex = blake3::hash(&bytes).to_hex().to_string();
                        let hdr = BatchReadItemHeader {
                            done: false,
                            source_path: src_str.clone(),
                            size: bytes.len() as u64,
                            modified_ms: mtime,
                            blake3_hex: hex,
                        };
                        Ok((hdr, bytes))
                    })();

                    match read_one {
                        Ok(pair) => {
                            if tx_c.blocking_send(Ok(pair)).is_err() {
                                break;
                            }
                        }
                        Err(rpc) => {
                            abort_c.cancel();
                            let _ = tx_c.blocking_send(Err(rpc));
                            break;
                        }
                    }
                }
            });
        }
        drop(item_tx);

        let mut wire_buf = Vec::with_capacity(COALESCE_BYTES + 16 * 1024);
        let append_hdr =
            |buf: &mut Vec<u8>, hdr: &BatchReadItemHeader| -> Result<(), crate::error::CoreError> {
                let pos = buf.len();
                buf.extend_from_slice(&[0u8; 4]);
                serde_json::to_writer(&mut *buf, hdr)
                    .map_err(|e| crate::error::CoreError::Frame(pcshare_proto::FrameError::Json(e)))?;
                let frame_len = (buf.len() - pos - 4) as u32;
                buf[pos..pos + 4].copy_from_slice(&frame_len.to_be_bytes());
                Ok(())
            };

        while let Some(res) = item_rx.recv().await {
            let (hdr, bytes) = match res {
                Ok(pair) => pair,
                Err(rpc) => {
                    abort.cancel();
                    return Err(crate::error::CoreError::Rpc(rpc));
                }
            };
            if let Err(e) = append_hdr(&mut wire_buf, &hdr) {
                abort.cancel();
                return Err(e);
            }
            wire_buf.extend_from_slice(&bytes);
            if wire_buf.len() >= COALESCE_BYTES {
                if let Err(e) = send.write_all(&wire_buf).await {
                    abort.cancel();
                    return Err(e.into());
                }
                wire_buf.clear();
            }
        }

        append_hdr(
            &mut wire_buf,
            &BatchReadItemHeader {
                done: true,
                ..Default::default()
            },
        )?;
        send.write_all(&wire_buf).await?;
        send.flush().await?;
        Ok(true)
    }
}
