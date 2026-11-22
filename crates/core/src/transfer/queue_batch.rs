use super::*;

impl CoreNode {
    // ────────────────── 任务 2.4：文件夹递归、4 路并发队列与小文件批传 ──────────────────

    pub async fn transfer_paths_to_peer(
        self: &Arc<Self>,
        peer_id: &str,
        local_sources: &[PathBuf],
        remote_dest_dir: &str,
        opts: TransferQueueOptions,
        ctrl: TransferTaskControl,
        on_progress: Option<ProgressCallback>,
    ) -> Result<u64, CoreError> {
        ctrl.wait_ready().await?;
        let src_strs: Vec<String> = local_sources
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();
        let walked = self
            .fs_service()
            .walk_local(&Permissions::default(), &src_strs)
            .await
            .map_err(CoreError::Rpc)?;

        let total_bytes: u64 = walked.iter().filter(|e| !e.dir).map(|e| e.size).sum();
        let done_bytes = Arc::new(AtomicU64::new(0));
        let started = Instant::now();
        let concurrency = opts.concurrency.clamp(1, 16);

        let mut small_items = Vec::new();
        let mut large_files = Vec::new();
        for entry in walked {
            if entry.dir || entry.size <= SMALL_FILE_BATCH_THRESHOLD {
                small_items.push(entry);
            } else {
                large_files.push(entry);
            }
        }

        // 使用派生子控制器，任何子任务报错立即取消其余并发任务并等待全部回收
        let sub_ctrl = ctrl.child();

        // 1. 先按并发度均匀分片并发发送所有目录骨架与小文件（既充分利用并发槽位，又保持单流批传低开销）
        let batches = chunk_walk_entries(small_items, concurrency);
        if !batches.is_empty() {
            let sem = Arc::new(Semaphore::new(concurrency));
            let mut set: JoinSet<Result<(), CoreError>> = JoinSet::new();
            let mut first_err: Option<CoreError> = None;

            for batch in batches {
                if first_err.is_some() || sub_ctrl.is_cancelled() {
                    break;
                }
                let cancel_tok = sub_ctrl.cancel_token();
                let permit = tokio::select! {
                    _ = cancel_tok.cancelled() => break,
                    p = sem.clone().acquire_owned() => match p {
                        Ok(p) => p,
                        Err(_) => break,
                    },
                };
                let node = self.clone();
                let pid = peer_id.to_string();
                let dst = remote_dest_dir.to_string();
                let conflict = opts.conflict;
                let limit_bps = opts.speed_limit_bps;
                let ctrl_c = sub_ctrl.clone();
                let done_c = done_bytes.clone();
                let cb_c = on_progress.clone();
                set.spawn(async move {
                    let _permit = permit;
                    let res = node
                        .upload_small_batch(
                            &pid,
                            &dst,
                            conflict,
                            limit_bps,
                            batch,
                            &ctrl_c,
                            &done_c,
                            total_bytes,
                            started,
                            cb_c,
                        )
                        .await;
                    if res.is_err() {
                        ctrl_c.cancel();
                    }
                    res
                });
            }

            while let Some(joined) = set.join_next().await {
                match joined {
                    Ok(Ok(())) => {}
                    Ok(Err(err)) => {
                        if first_err.is_none() {
                            first_err = Some(err);
                            sub_ctrl.cancel();
                        }
                    }
                    Err(join_err) => {
                        if first_err.is_none() {
                            first_err = Some(CoreError::Internal(format!(
                                "小文件批量传输异常：{join_err}"
                            )));
                            sub_ctrl.cancel();
                        }
                    }
                }
            }
            if let Some(err) = first_err {
                return Err(err);
            }
            if ctrl.is_cancelled() {
                return Err(CoreError::Cancelled);
            }
        }

        // 2. 再用 4 路并发队列传输大文件（支持断点续传）
        if !large_files.is_empty() {
            let sem = Arc::new(Semaphore::new(concurrency));
            let mut set: JoinSet<Result<(), CoreError>> = JoinSet::new();
            let mut first_err: Option<CoreError> = None;

            for lf in large_files {
                if first_err.is_some() || sub_ctrl.is_cancelled() {
                    break;
                }
                let cancel_tok = sub_ctrl.cancel_token();
                let permit = tokio::select! {
                    _ = cancel_tok.cancelled() => break,
                    p = sem.clone().acquire_owned() => match p {
                        Ok(p) => p,
                        Err(_) => break,
                    },
                };
                let node = self.clone();
                let pid = peer_id.to_string();
                let remote_file_path = join_remote_subpath(remote_dest_dir, &lf.rel_path);
                let local_p = PathBuf::from(&lf.abs_path);
                let conflict = opts.conflict;
                let limit_bps = opts.speed_limit_bps;
                let ctrl_c = sub_ctrl.clone();
                let done_c = done_bytes.clone();
                let cb_c = on_progress.clone();
                set.spawn(async move {
                    let _permit = permit;
                    let prev_reported = Arc::new(AtomicU64::new(0));
                    let prev_c = prev_reported.clone();
                    let done_inner = done_c.clone();
                    let progress_cb: Option<ProgressCallback> = cb_c.map(|outer| {
                        Arc::new(move |cur_file_done: u64, _file_tot: u64, _spd: f64| {
                            let prev = prev_c.swap(cur_file_done, Ordering::Relaxed);
                            let delta = cur_file_done.saturating_sub(prev);
                            let agg = done_inner.fetch_add(delta, Ordering::Relaxed) + delta;
                            let spd = agg as f64 / started.elapsed().as_secs_f64().max(0.001);
                            outer(agg, total_bytes.max(1), spd);
                        }) as ProgressCallback
                    });
                    let res = node
                        .upload_file_with_ctrl(
                            &pid,
                            &local_p,
                            &remote_file_path,
                            conflict,
                            &ctrl_c,
                            limit_bps,
                            progress_cb,
                            None,
                        )
                        .await;
                    if res.is_err() {
                        ctrl_c.cancel();
                    }
                    res.map(|_| ())
                });
            }

            while let Some(joined) = set.join_next().await {
                match joined {
                    Ok(Ok(())) => {}
                    Ok(Err(err)) => {
                        if first_err.is_none() {
                            first_err = Some(err);
                            sub_ctrl.cancel();
                        }
                    }
                    Err(join_err) => {
                        if first_err.is_none() {
                            first_err = Some(CoreError::Internal(format!(
                                "大文件并发上传异常：{join_err}"
                            )));
                            sub_ctrl.cancel();
                        }
                    }
                }
            }
            if let Some(err) = first_err {
                return Err(err);
            }
            if ctrl.is_cancelled() {
                return Err(CoreError::Cancelled);
            }
        }

        Ok(done_bytes.load(Ordering::Relaxed).max(total_bytes))
    }

    #[allow(clippy::too_many_arguments)]
    async fn upload_small_batch(
        self: &Arc<Self>,
        peer_id: &str,
        remote_dest_dir: &str,
        conflict: ConflictPolicy,
        speed_limit_bps: Option<u64>,
        batch: Vec<WalkEntry>,
        ctrl: &TransferTaskControl,
        done_bytes: &Arc<AtomicU64>,
        total_bytes: u64,
        started: Instant,
        on_progress: Option<ProgressCallback>,
    ) -> Result<(), CoreError> {
        ctrl.wait_ready().await?;
        let conn = self.get_or_connect(peer_id).await?;
        let (mut send, mut recv) = conn
            .open_bi()
            .await
            .map_err(|e| CoreError::NotConnected(format!("打开批量上传流失败：{e}")))?;
        let _ = send.set_priority(-16);

        let cmd = Command::FsWriteBatch(FsWriteBatchArgs {
            dest_dir: remote_dest_dir.to_string(),
            conflict,
        });
        write_frame(&mut send, &cmd.to_envelope()?).await?;
        let ready: ResponseEnvelope<()> = timeout(Duration::from_secs(15), read_frame(&mut recv))
            .await
            .map_err(|_| CoreError::Timeout)??;
        ready.into_result()?;

        // 用 2 个后台阻塞线程并行预读本批小文件并计算 BLAKE3；读取失败直接报错，绝不静默跳过
        let (rd_tx, mut rd_rx) = tokio::sync::mpsc::channel::<
            Result<(WalkEntry, Option<(Vec<u8>, String)>), CoreError>,
        >(256);
        let shared_batch = Arc::new(Mutex::new(batch.into_iter()));
        let read_abort = ctrl.cancel_token().child_token();
        for _ in 0..2 {
            let tx_c = rd_tx.clone();
            let iter_c = shared_batch.clone();
            let abort_c = read_abort.clone();
            tokio::task::spawn_blocking(move || {
                loop {
                    if abort_c.is_cancelled() {
                        break;
                    }
                    let next_item = iter_c.lock().unwrap_or_else(|e| e.into_inner()).next();
                    let Some(item) = next_item else {
                        break;
                    };
                    if item.dir {
                        if tx_c.blocking_send(Ok((item, None))).is_err() {
                            break;
                        }
                    } else {
                        let fs_p = to_fs_path(Path::new(&item.abs_path));
                        let read_res = (|| -> Result<(Vec<u8>, String), CoreError> {
                            let meta = fs::metadata(&fs_p).map_err(|e| {
                                CoreError::Rpc(map_io_to_rpc(&e, Some(&item.abs_path)))
                            })?;
                            if meta.len() > MAX_BATCH_FILE_BYTES {
                                return Err(CoreError::Rpc(RpcError::new(
                                    ErrorCode::Invalid,
                                    format!(
                                        "小文件大小已超出批传上限（{} 字节）：{}",
                                        meta.len(),
                                        item.abs_path
                                    ),
                                )));
                            }
                            let bytes = fs::read(&fs_p).map_err(|e| {
                                CoreError::Rpc(map_io_to_rpc(&e, Some(&item.abs_path)))
                            })?;
                            if bytes.len() as u64 > MAX_BATCH_FILE_BYTES {
                                return Err(CoreError::Rpc(RpcError::new(
                                    ErrorCode::Invalid,
                                    format!("小文件大小已超出批传上限：{}", item.abs_path),
                                )));
                            }
                            let hex = blake3::hash(&bytes).to_hex().to_string();
                            Ok((bytes, hex))
                        })();
                        match read_res {
                            Ok(pair) => {
                                if tx_c.blocking_send(Ok((item, Some(pair)))).is_err() {
                                    break;
                                }
                            }
                            Err(err) => {
                                abort_c.cancel();
                                let _ = tx_c.blocking_send(Err(err));
                                break;
                            }
                        }
                    }
                }
            });
        }
        drop(rd_tx);

        const COALESCE_BYTES: usize = 256 * 1024;
        let mut wire_buf = Vec::with_capacity(COALESCE_BYTES + 16 * 1024);
        let append_frame = |buf: &mut Vec<u8>, hdr: &BatchFileItem| -> Result<(), CoreError> {
            let pos = buf.len();
            buf.extend_from_slice(&[0u8; 4]);
            serde_json::to_writer(&mut *buf, hdr)
                .map_err(|e| CoreError::Frame(pcshare_proto::FrameError::Json(e)))?;
            let frame_len = (buf.len() - pos - 4) as u32;
            buf[pos..pos + 4].copy_from_slice(&frame_len.to_be_bytes());
            Ok(())
        };

        while let Some(res_item) = rd_rx.recv().await {
            if let Err(err) = ctrl.wait_ready().await {
                read_abort.cancel();
                let _ = send.reset(CLOSE_CANCELLED.into());
                return Err(err);
            }
            let (item, data_opt) = match res_item {
                Ok(v) => v,
                Err(err) => {
                    read_abort.cancel();
                    let _ = send.reset(CLOSE_CANCELLED.into());
                    return Err(err);
                }
            };
            match data_opt {
                None => {
                    let hdr = BatchFileItem {
                        done: false,
                        rel_path: item.rel_path,
                        dir: true,
                        size: 0,
                        modified_ms: item.modified_ms,
                        blake3_hex: None,
                    };
                    append_frame(&mut wire_buf, &hdr)?;
                }
                Some((bytes, hex)) => {
                    let len = bytes.len();
                    if let Err(err) = self
                        .rate_limiter()
                        .acquire_with_ctrl(len, ctrl, speed_limit_bps)
                        .await
                    {
                        read_abort.cancel();
                        let _ = send.reset(CLOSE_CANCELLED.into());
                        return Err(err);
                    }
                    let hdr = BatchFileItem {
                        done: false,
                        rel_path: item.rel_path,
                        dir: false,
                        size: len as u64,
                        modified_ms: item.modified_ms,
                        blake3_hex: Some(hex),
                    };
                    append_frame(&mut wire_buf, &hdr)?;
                    wire_buf.extend_from_slice(&bytes);
                    let agg = done_bytes.fetch_add(len as u64, Ordering::Relaxed) + len as u64;
                    if let Some(ref cb) = on_progress {
                        let spd = agg as f64 / started.elapsed().as_secs_f64().max(0.001);
                        cb(agg, total_bytes.max(1), spd);
                    }
                }
            }
            if wire_buf.len() >= COALESCE_BYTES {
                if let Err(e) = send.write_all(&wire_buf).await {
                    read_abort.cancel();
                    return Err(CoreError::NotConnected(format!("批量上传数据中断：{e}")));
                }
                wire_buf.clear();
            }
        }

        append_frame(
            &mut wire_buf,
            &BatchFileItem {
                done: true,
                ..Default::default()
            },
        )?;
        send.write_all(&wire_buf)
            .await
            .map_err(|e| CoreError::NotConnected(format!("批量上传结束帧失败：{e}")))?;
        send.flush()
            .await
            .map_err(|e| CoreError::NotConnected(format!("刷新批量上传流失败：{e}")))?;
        send.finish()
            .map_err(|e| CoreError::Internal(format!("结束批量上传流失败：{e}")))?;

        let done_resp: ResponseEnvelope<FsWriteBatchReply> =
            timeout(Duration::from_secs(60), read_frame(&mut recv))
                .await
                .map_err(|_| CoreError::Timeout)??;
        done_resp.into_result()?;
        Ok(())
    }

    pub async fn download_paths_from_peer(
        self: &Arc<Self>,
        peer_id: &str,
        remote_sources: &[String],
        local_dest_dir: &Path,
        opts: TransferQueueOptions,
        ctrl: TransferTaskControl,
        on_progress: Option<ProgressCallback>,
    ) -> Result<u64, CoreError> {
        ctrl.wait_ready().await?;
        let dest_clean = normalize_lexical_absolute(&local_dest_dir.to_string_lossy())
            .map_err(CoreError::Rpc)?;
        tokio::fs::create_dir_all(to_fs_path(&dest_clean)).await?;

        let walked = self.remote_walk(peer_id, remote_sources).await?;
        let total_bytes: u64 = walked.iter().filter(|e| !e.dir).map(|e| e.size).sum();
        let done_bytes = Arc::new(AtomicU64::new(0));
        let started = Instant::now();
        let concurrency = opts.concurrency.clamp(1, 16);

        let mut small_files = Vec::new();
        let mut large_files = Vec::new();
        for entry in walked {
            let rel_clean = sanitize_rel_subpath(&entry.rel_path).map_err(CoreError::Rpc)?;
            let target_clean = dest_clean.join(&rel_clean);
            if entry.dir {
                tokio::fs::create_dir_all(to_fs_path(&target_clean)).await?;
            } else if entry.size <= SMALL_FILE_BATCH_THRESHOLD {
                small_files.push((entry, target_clean));
            } else {
                large_files.push((entry, target_clean));
            }
        }

        let sub_ctrl = ctrl.child();

        // 1. 小文件并发批量下载（按并发槽位数均匀切分批次，避免单批垄断全部小文件导致其余并发槽空转）
        if !small_files.is_empty() {
            let sem = Arc::new(Semaphore::new(concurrency));
            let item_cap = small_files
                .len()
                .div_ceil(concurrency.max(1))
                .clamp(128, BATCH_MAX_ITEMS);
            let mut chunks = Vec::new();
            let mut cur = Vec::new();
            let mut cur_bytes = 0u64;
            for pair in small_files {
                cur_bytes += pair.0.size;
                cur.push(pair);
                if cur.len() >= item_cap || cur_bytes >= BATCH_MAX_BYTES {
                    chunks.push(std::mem::take(&mut cur));
                    cur_bytes = 0;
                }
            }
            if !cur.is_empty() {
                chunks.push(cur);
            }

            let mut set: JoinSet<Result<(), CoreError>> = JoinSet::new();
            let mut first_err: Option<CoreError> = None;

            for chunk in chunks {
                if first_err.is_some() || sub_ctrl.is_cancelled() {
                    break;
                }
                let cancel_tok = sub_ctrl.cancel_token();
                let permit = tokio::select! {
                    _ = cancel_tok.cancelled() => break,
                    p = sem.clone().acquire_owned() => match p {
                        Ok(p) => p,
                        Err(_) => break,
                    },
                };
                let node = self.clone();
                let pid = peer_id.to_string();
                let conflict = opts.conflict;
                let limit_bps = opts.speed_limit_bps;
                let ctrl_c = sub_ctrl.clone();
                let done_c = done_bytes.clone();
                let cb_c = on_progress.clone();
                set.spawn(async move {
                    let _permit = permit;
                    let res = node
                        .download_small_batch(
                            &pid,
                            conflict,
                            limit_bps,
                            chunk,
                            &ctrl_c,
                            &done_c,
                            total_bytes,
                            started,
                            cb_c,
                        )
                        .await;
                    if res.is_err() {
                        ctrl_c.cancel();
                    }
                    res
                });
            }
            while let Some(joined) = set.join_next().await {
                match joined {
                    Ok(Ok(())) => {}
                    Ok(Err(err)) => {
                        if first_err.is_none() {
                            first_err = Some(err);
                            sub_ctrl.cancel();
                        }
                    }
                    Err(join_err) => {
                        if first_err.is_none() {
                            first_err = Some(CoreError::Internal(format!(
                                "小文件批量下载异常：{join_err}"
                            )));
                            sub_ctrl.cancel();
                        }
                    }
                }
            }
            if let Some(err) = first_err {
                return Err(err);
            }
            if ctrl.is_cancelled() {
                return Err(CoreError::Cancelled);
            }
        }

        // 2. 大文件 4 路并发断点续传下载
        if !large_files.is_empty() {
            let sem = Arc::new(Semaphore::new(concurrency));
            let mut set: JoinSet<Result<(), CoreError>> = JoinSet::new();
            let mut first_err: Option<CoreError> = None;

            for (entry, target_clean) in large_files {
                if first_err.is_some() || sub_ctrl.is_cancelled() {
                    break;
                }
                let cancel_tok = sub_ctrl.cancel_token();
                let permit = tokio::select! {
                    _ = cancel_tok.cancelled() => break,
                    p = sem.clone().acquire_owned() => match p {
                        Ok(p) => p,
                        Err(_) => break,
                    },
                };
                let node = self.clone();
                let pid = peer_id.to_string();
                let conflict = opts.conflict;
                let limit_bps = opts.speed_limit_bps;
                let ctrl_c = sub_ctrl.clone();
                let done_c = done_bytes.clone();
                let cb_c = on_progress.clone();
                set.spawn(async move {
                    let _permit = permit;
                    let prev_reported = Arc::new(AtomicU64::new(0));
                    let prev_c = prev_reported.clone();
                    let done_inner = done_c.clone();
                    let progress_cb: Option<ProgressCallback> = cb_c.map(|outer| {
                        Arc::new(move |cur_done: u64, _tot: u64, _spd: f64| {
                            let prev = prev_c.swap(cur_done, Ordering::Relaxed);
                            let delta = cur_done.saturating_sub(prev);
                            let agg = done_inner.fetch_add(delta, Ordering::Relaxed) + delta;
                            let spd = agg as f64 / started.elapsed().as_secs_f64().max(0.001);
                            outer(agg, total_bytes.max(1), spd);
                        }) as ProgressCallback
                    });
                    let res = node
                        .download_file_with_ctrl(
                            &pid,
                            &entry.abs_path,
                            &target_clean,
                            conflict,
                            &ctrl_c,
                            limit_bps,
                            progress_cb,
                            None,
                        )
                        .await;
                    if res.is_err() {
                        ctrl_c.cancel();
                    }
                    res.map(|_| ())
                });
            }
            while let Some(joined) = set.join_next().await {
                match joined {
                    Ok(Ok(())) => {}
                    Ok(Err(err)) => {
                        if first_err.is_none() {
                            first_err = Some(err);
                            sub_ctrl.cancel();
                        }
                    }
                    Err(join_err) => {
                        if first_err.is_none() {
                            first_err = Some(CoreError::Internal(format!(
                                "大文件并发下载异常：{join_err}"
                            )));
                            sub_ctrl.cancel();
                        }
                    }
                }
            }
            if let Some(err) = first_err {
                return Err(err);
            }
            if ctrl.is_cancelled() {
                return Err(CoreError::Cancelled);
            }
        }

        Ok(done_bytes.load(Ordering::Relaxed).max(total_bytes))
    }

    #[allow(clippy::too_many_arguments)]
    async fn download_small_batch(
        self: &Arc<Self>,
        peer_id: &str,
        conflict: ConflictPolicy,
        speed_limit_bps: Option<u64>,
        chunk: Vec<(WalkEntry, PathBuf)>,
        ctrl: &TransferTaskControl,
        done_bytes: &Arc<AtomicU64>,
        total_bytes: u64,
        started: Instant,
        on_progress: Option<ProgressCallback>,
    ) -> Result<(), CoreError> {
        ctrl.wait_ready().await?;
        let expected_files = chunk.len();
        let mut target_map: HashMap<String, PathBuf> = HashMap::with_capacity(expected_files);
        let mut sources = Vec::with_capacity(expected_files);
        for (entry, dst_clean) in chunk {
            sources.push(entry.abs_path.clone());
            target_map.insert(entry.abs_path, dst_clean);
        }

        let conn = self.get_or_connect(peer_id).await?;
        let (mut send, mut recv) = conn
            .open_bi()
            .await
            .map_err(|e| CoreError::NotConnected(format!("打开批量下载流失败：{e}")))?;

        let cmd = Command::FsReadBatch(FsReadBatchArgs { sources });
        write_frame(&mut send, &cmd.to_envelope()?).await?;
        send.finish()
            .map_err(|e| CoreError::Internal(format!("结束批量下载请求失败：{e}")))?;

        let ready: ResponseEnvelope<()> = timeout(Duration::from_secs(15), read_frame(&mut recv))
            .await
            .map_err(|_| CoreError::Timeout)??;
        ready.into_result()?;

        struct DownloadWriteJob {
            source_path: String,
            target_clean: PathBuf,
            modified_ms: Option<u64>,
            expected_hex: String,
            data: Vec<u8>,
        }

        let (job_tx, job_rx) = tokio::sync::mpsc::channel::<DownloadWriteJob>(256);
        let shared_rx = Arc::new(Mutex::new(job_rx));
        let abort = CancellationToken::new();
        let mut worker_handles = Vec::with_capacity(4);
        for _ in 0..4 {
            let rx_c = shared_rx.clone();
            let abort_c = abort.clone();
            worker_handles.push(tokio::task::spawn_blocking(
                move || -> Result<usize, RpcError> {
                    let mut written_count = 0usize;
                    let mut local_dirs = std::collections::HashSet::<PathBuf>::new();
                    loop {
                        if abort_c.is_cancelled() {
                            break;
                        }
                        let job = {
                            let mut g = rx_c.lock().unwrap_or_else(|e| e.into_inner());
                            g.blocking_recv()
                        };
                        let Some(job) = job else {
                            break;
                        };
                        if abort_c.is_cancelled() {
                            break;
                        }
                        let step = (|| -> Result<(), RpcError> {
                            let got_hex = blake3::hash(&job.data).to_hex().to_string();
                            if got_hex != job.expected_hex {
                                return Err(RpcError::new(
                                    ErrorCode::Io,
                                    format!("下载文件校验失败：{}", job.source_path),
                                ));
                            }
                            let disp = to_display_path(&job.target_clean);
                            let initial_fs = to_fs_path(&job.target_clean);
                            let final_fs = if initial_fs.exists() {
                                match conflict {
                                    ConflictPolicy::Skip => {
                                        written_count += 1;
                                        return Ok(());
                                    }
                                    ConflictPolicy::Fail => {
                                        return Err(RpcError::new(
                                            ErrorCode::AlreadyExists,
                                            format!("本地目标已存在：{disp}"),
                                        ));
                                    }
                                    ConflictPolicy::Overwrite => initial_fs,
                                    ConflictPolicy::RenameKeepBoth => {
                                        pick_unique_target(&job.target_clean).1
                                    }
                                }
                            } else {
                                initial_fs
                            };
                            if let Some(p) = final_fs.parent()
                                && local_dirs.insert(p.to_path_buf())
                            {
                                fs::create_dir_all(p).map_err(|e| map_io_to_rpc(&e, None))?;
                            }
                            let mut f = File::create(&final_fs)
                                .map_err(|e| map_io_to_rpc(&e, Some(&disp)))?;
                            if !job.data.is_empty() {
                                std::io::Write::write_all(&mut f, &job.data)
                                    .map_err(|e| map_io_to_rpc(&e, Some(&disp)))?;
                            }
                            if let Some(ms) = job.modified_ms {
                                let _ = f.set_modified(UNIX_EPOCH + Duration::from_millis(ms));
                            }
                            written_count += 1;
                            Ok(())
                        })();
                        if let Err(rpc) = step {
                            abort_c.cancel();
                            return Err(rpc);
                        }
                    }
                    Ok(written_count)
                },
            ));
        }

        let mut stream_err: Option<CoreError> = None;
        loop {
            if abort.is_cancelled() {
                break;
            }
            if let Err(err) = ctrl.wait_ready().await {
                abort.cancel();
                let _ = recv.stop(CLOSE_CANCELLED.into());
                stream_err = Some(err);
                break;
            }
            let cancel_tok = ctrl.cancel_token();
            let hdr_res = tokio::select! {
                _ = cancel_tok.cancelled() => {
                    abort.cancel();
                    let _ = recv.stop(CLOSE_CANCELLED.into());
                    stream_err = Some(CoreError::Cancelled);
                    break;
                }
                _ = abort.cancelled() => break,
                r = read_frame::<_, BatchReadItemHeader>(&mut recv) => r,
            };
            let hdr = match hdr_res {
                Ok(h) => h,
                Err(e) => {
                    abort.cancel();
                    stream_err = Some(CoreError::Frame(e));
                    break;
                }
            };
            if hdr.done {
                break;
            }
            if hdr.size > MAX_BATCH_FILE_BYTES {
                abort.cancel();
                let _ = recv.stop(CLOSE_CANCELLED.into());
                stream_err = Some(CoreError::Rpc(RpcError::new(
                    ErrorCode::Invalid,
                    format!(
                        "批量下载收到超过单文件上限的数据（{} 字节，上限 {} 字节）：{}",
                        hdr.size, MAX_BATCH_FILE_BYTES, hdr.source_path
                    ),
                )));
                break;
            }
            let mut data = vec![0u8; hdr.size as usize];
            if hdr.size > 0
                && let Err(e) = recv.read_exact(&mut data).await
            {
                abort.cancel();
                stream_err = Some(CoreError::NotConnected(format!(
                    "读取批量下载数据中断：{e}"
                )));
                break;
            }
            if let Err(err) = self
                .rate_limiter()
                .acquire_with_ctrl(data.len(), ctrl, speed_limit_bps)
                .await
            {
                abort.cancel();
                let _ = recv.stop(CLOSE_CANCELLED.into());
                stream_err = Some(err);
                break;
            }

            if let Some(target_clean) = target_map.get(&hdr.source_path).cloned() {
                let job = DownloadWriteJob {
                    source_path: hdr.source_path,
                    target_clean,
                    modified_ms: hdr.modified_ms,
                    expected_hex: hdr.blake3_hex,
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

            let agg = done_bytes.fetch_add(hdr.size, Ordering::Relaxed) + hdr.size;
            if let Some(ref cb) = on_progress {
                let spd = agg as f64 / started.elapsed().as_secs_f64().max(0.001);
                cb(agg, total_bytes.max(1), spd);
            }
        }
        drop(job_tx);

        let mut received_files = 0usize;
        let mut worker_rpc_err: Option<RpcError> = None;
        for h in worker_handles {
            match h
                .await
                .map_err(|e| CoreError::Internal(format!("写入本地小文件线程异常：{e}")))?
            {
                Ok(n) => received_files += n,
                Err(rpc) => {
                    if worker_rpc_err.is_none() {
                        worker_rpc_err = Some(rpc);
                    }
                }
            }
        }
        if let Some(rpc) = worker_rpc_err {
            let _ = recv.stop(CLOSE_CANCELLED.into());
            return Err(CoreError::Rpc(rpc));
        }
        if let Some(err) = stream_err {
            return Err(err);
        }

        if received_files < expected_files {
            return Err(CoreError::Internal(format!(
                "批量下载未收齐全部文件（期望 {expected_files} 个，实际收到 {received_files} 个）"
            )));
        }
        Ok(())
    }
}
