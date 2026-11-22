use super::*;

impl CoreNode {
    // ────────────────────────── 任务 2.4：单文件断点续传 ──────────────────────────

    #[allow(clippy::too_many_arguments)]
    pub async fn upload_file_resumable(
        self: &Arc<Self>,
        peer_id: &str,
        local_src: &Path,
        remote_dest: &str,
        conflict: ConflictPolicy,
        cancel: CancellationToken,
        on_progress: Option<ProgressCallback>,
        on_wire_delta: Option<WireDeltaCallback>,
    ) -> Result<FsWriteDone, CoreError> {
        let ctrl = TransferTaskControl::from_token(cancel);
        self.upload_file_with_ctrl(
            peer_id,
            local_src,
            remote_dest,
            conflict,
            &ctrl,
            None,
            on_progress,
            on_wire_delta,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn upload_file_with_ctrl(
        self: &Arc<Self>,
        peer_id: &str,
        local_src: &Path,
        remote_dest: &str,
        conflict: ConflictPolicy,
        ctrl: &TransferTaskControl,
        speed_limit_bps: Option<u64>,
        on_progress: Option<ProgressCallback>,
        on_wire_delta: Option<WireDeltaCallback>,
    ) -> Result<FsWriteDone, CoreError> {
        ctrl.wait_ready().await?;
        let local_clean = normalize_lexical_absolute(&local_src.to_string_lossy())
            .map_err(CoreError::Rpc)?;
        let local_fs = to_fs_path(&local_clean);
        let local_disp = to_display_path(&local_clean);

        let meta = tokio::fs::metadata(&local_fs)
            .await
            .map_err(|e| CoreError::Rpc(map_io_to_rpc(&e, Some(&local_disp))))?;
        if meta.is_dir() {
            return Err(CoreError::InvalidArgument(format!(
                "单文件上传接口收到文件夹，请改用队列传输：{local_disp}"
            )));
        }
        let total_size = meta.len();
        let modified_ms = meta.modified().ok().and_then(system_time_to_ms);

        // 先探测远端冲突与是否已有同名 `.pcshare.part` 文件可断点续传
        let mut resume_offset = 0u64;
        let remote_clean_opt = normalize_lexical_absolute(remote_dest).ok();
        if let Some(ref r_clean) = remote_clean_opt
            && let (Some(r_parent), Some(r_name)) = (r_clean.parent(), r_clean.file_name())
            && conflict != ConflictPolicy::RenameKeepBoth
            && let Ok(probe) = self
                .remote_probe_resume(
                    peer_id,
                    &to_display_path(r_parent),
                    &r_name.to_string_lossy(),
                    conflict,
                )
                .await
        {
            // 先按 dest_exists 短路 Fail/Skip，避免远端同时存在正式文件与残留 .part 时白算整段本地哈希
            if probe.dest_exists {
                match conflict {
                    ConflictPolicy::Skip => {
                        return Ok(FsWriteDone {
                            target_path: probe.target_path,
                            bytes_written: 0,
                        });
                    }
                    ConflictPolicy::Fail => {
                        return Err(CoreError::Rpc(RpcError::new(
                            ErrorCode::AlreadyExists,
                            format!("目标文件已存在：{}", probe.target_path),
                        )));
                    }
                    ConflictPolicy::Overwrite | ConflictPolicy::RenameKeepBoth => {}
                }
            }
            if probe.part_bytes > 0
                && probe.part_bytes <= total_size
                && let Some(remote_part_hex) = probe.part_blake3_hex
            {
                let fs_c = local_fs.clone();
                let part_len = probe.part_bytes;
                let local_hex_opt = tokio::task::spawn_blocking(move || {
                    hash_file_prefix_sync(&fs_c, part_len).ok()
                })
                .await
                .ok()
                .flatten();
                if local_hex_opt.as_deref() == Some(remote_part_hex.as_str()) {
                    resume_offset = part_len;
                }
            }
        }

        let conn = self.get_or_connect(peer_id).await?;
        let unthrottled = self
            .rate_limiter()
            .effective_limit_bps(ctrl.speed_limit_bps(), speed_limit_bps)
            == 0;
        let peer_supports_striped = self
            .storage
            .get_peer(peer_id)
            .is_some_and(|p| p.capabilities.iter().any(|c| c == CAP_TRANSFER_STRIPED));
        if unthrottled
            && peer_supports_striped
            && total_size.saturating_sub(resume_offset) >= 32 * 1024 * 1024
        {
            return self
                .upload_parallel_stripes(
                    peer_id,
                    &conn,
                    &local_fs,
                    &local_disp,
                    remote_dest,
                    total_size,
                    resume_offset,
                    modified_ms,
                    conflict,
                    ctrl,
                    on_progress,
                    on_wire_delta,
                )
                .await;
        }

        let (mut send, mut recv) = conn
            .open_bi()
            .await
            .map_err(|e| CoreError::NotConnected(format!("打开上传通道失败：{e}")))?;
        let _ = send.set_priority(-16);

        let cmd = Command::FsWrite(FsWriteArgs {
            path: remote_dest.to_string(),
            size: total_size,
            offset: resume_offset,
            range_length: None,
            defer_finalize: false,
            modified_ms,
            conflict,
        });
        write_frame(&mut send, &cmd.to_envelope()?).await?;

        let ready_resp: ResponseEnvelope<FsWriteReady> =
            timeout(Duration::from_secs(15), read_frame(&mut recv))
                .await
                .map_err(|_| CoreError::Timeout)??;
        let ready = ready_resp.into_result()?;
        if ready.skipped {
            let _ = send.finish();
            return Ok(FsWriteDone {
                target_path: ready.target_path,
                bytes_written: 0,
            });
        }

        let fs_for_open = local_fs.clone();
        let disp_for_open = local_disp.clone();
        let mut std_file = tokio::task::spawn_blocking(move || -> Result<File, RpcError> {
            use std::io::Seek;
            let mut f =
                open_sequential_read(&fs_for_open).map_err(|e| map_io_to_rpc(&e, Some(&disp_for_open)))?;
            if resume_offset > 0 {
                f.seek(std::io::SeekFrom::Start(resume_offset))
                    .map_err(|e| map_io_to_rpc(&e, Some(&disp_for_open)))?;
            }
            Ok(f)
        })
        .await
        .map_err(|e| CoreError::Internal(format!("打开上传源文件异常：{e}")))?
        .map_err(CoreError::Rpc)?;

        let started = Instant::now();
        let mut last_report = Instant::now();
        let mut sent = resume_offset;

        if let Some(ref cb) = on_progress
            && resume_offset > 0
        {
            cb(resume_offset, total_size, 0.0);
        }

        const UPLOAD_POOL_CAP: usize = 6;
        let (data_tx, mut data_rx) =
            tokio::sync::mpsc::channel::<std::io::Result<(Vec<u8>, usize)>>(UPLOAD_POOL_CAP);
        let (recycle_tx, mut recycle_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(UPLOAD_POOL_CAP);
        for _ in 0..UPLOAD_POOL_CAP {
            let _ = recycle_tx.try_send(vec![0u8; TRANSFER_CHUNK_SIZE]);
        }

        let to_read_total = total_size.saturating_sub(resume_offset);
        let reader_cancel = ctrl.cancel_token();
        tokio::task::spawn_blocking(move || {
            use std::io::Read;
            let mut left = to_read_total;
            while left > 0 {
                if reader_cancel.is_cancelled() {
                    break;
                }
                let Some(mut buf) = recycle_rx.blocking_recv() else {
                    break;
                };
                if buf.len() < TRANSFER_CHUNK_SIZE {
                    buf.resize(TRANSFER_CHUNK_SIZE, 0);
                }
                let want = (left as usize).min(TRANSFER_CHUNK_SIZE);
                match std_file.read(&mut buf[..want]) {
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

        while sent < total_size {
            if ctrl.is_cancelled() {
                let _ = send.reset(CLOSE_CANCELLED.into());
                return Err(CoreError::Cancelled);
            }
            if ctrl.is_paused() {
                ctrl.wait_ready().await.inspect_err(|_| {
                    let _ = send.reset(CLOSE_CANCELLED.into());
                })?;
            }

            let cancel_tok = ctrl.cancel_token();
            let next_chunk = tokio::select! {
                _ = cancel_tok.cancelled() => {
                    let _ = send.reset(CLOSE_CANCELLED.into());
                    return Err(CoreError::Cancelled);
                }
                item = data_rx.recv() => item,
            };
            let Some(chunk_res) = next_chunk else {
                let _ = send.reset(CLOSE_PAUSED.into());
                return Err(CoreError::Internal("读取本地文件时意外提前结束".to_string()));
            };
            let (buf, n) = chunk_res?;

            if let Err(err) = self
                .rate_limiter()
                .acquire_with_ctrl(n, ctrl, speed_limit_bps)
                .await
            {
                let _ = send.reset(CLOSE_CANCELLED.into());
                return Err(err);
            }

            tokio::select! {
                _ = cancel_tok.cancelled() => {
                    let _ = send.reset(CLOSE_CANCELLED.into());
                    return Err(CoreError::Cancelled);
                }
                w = send.write_all(&buf[..n]) => {
                    w.map_err(|e| CoreError::NotConnected(format!("上传数据中断：{e}")))?;
                }
            }
            let _ = recycle_tx.try_send(buf);
            sent += n as u64;
            if let Some(ref wcb) = on_wire_delta {
                wcb(n as u64);
            }

            if last_report.elapsed() >= Duration::from_millis(100) || sent == total_size {
                let wire_done = sent.saturating_sub(resume_offset);
                let speed = wire_done as f64 / started.elapsed().as_secs_f64().max(0.001);
                if let Some(ref cb) = on_progress {
                    cb(sent, total_size, speed);
                }
                let _ = self.events_tx.send(CoreEvent::TransferProgress(
                    TransferProgressEvent {
                        peer_id: peer_id.to_string(),
                        direction: TransferDirection::Upload,
                        local_path: local_disp.clone(),
                        remote_path: ready.target_path.clone(),
                        transferred_bytes: sent,
                        total_bytes: total_size,
                        bytes_per_sec: speed,
                    },
                ));
                last_report = Instant::now();
            }
        }

        if ctrl.is_cancelled() {
            let _ = send.reset(CLOSE_CANCELLED.into());
            return Err(CoreError::Cancelled);
        }

        send.finish()
            .map_err(|e| CoreError::Internal(format!("结束上传流失败：{e}")))?;

        let done_resp: ResponseEnvelope<FsWriteDone> =
            timeout(Duration::from_secs(30), read_frame(&mut recv))
                .await
                .map_err(|_| CoreError::Timeout)??;
        Ok(done_resp.into_result()?)
    }

    #[allow(clippy::too_many_arguments)]
    async fn upload_parallel_stripes(
        self: &Arc<Self>,
        peer_id: &str,
        conn0: &Connection,
        local_fs: &Path,
        local_disp: &str,
        remote_dest: &str,
        total_size: u64,
        resume_offset: u64,
        modified_ms: Option<u64>,
        conflict: ConflictPolicy,
        ctrl: &TransferTaskControl,
        on_progress: Option<ProgressCallback>,
        on_wire_delta: Option<WireDeltaCallback>,
    ) -> Result<FsWriteDone, CoreError> {
        let rem = total_size.saturating_sub(resume_offset);
        let chunk_u64 = TRANSFER_CHUNK_SIZE as u64;
        let seg0_len = ((rem / 2) / chunk_u64) * chunk_u64;
        let seg1_start = resume_offset + seg0_len;
        let seg1_len = rem - seg0_len;

        // 1. 先在主通道握手首分片，决议冲突路径并初始化 `.pcshare.part`
        let (mut send0, mut recv0) = conn0
            .open_bi()
            .await
            .map_err(|e| CoreError::NotConnected(format!("打开主分片上传通道失败：{e}")))?;
        let _ = send0.set_priority(-16);
        let cmd0 = Command::FsWrite(FsWriteArgs {
            path: remote_dest.to_string(),
            size: total_size,
            offset: resume_offset,
            range_length: Some(seg0_len),
            defer_finalize: true,
            modified_ms,
            conflict,
        });
        write_frame(&mut send0, &cmd0.to_envelope()?).await?;
        let ready0_resp: ResponseEnvelope<FsWriteReady> =
            timeout(Duration::from_secs(15), read_frame(&mut recv0))
                .await
                .map_err(|_| CoreError::Timeout)??;
        let ready0 = ready0_resp.into_result()?;
        if ready0.skipped {
            let _ = send0.finish();
            return Ok(FsWriteDone {
                target_path: ready0.target_path,
                bytes_written: 0,
            });
        }
        let target_path = ready0.target_path;

        // 2. 在第二条 QUIC 连接握手尾分片
        let conn1 = match self.get_or_connect_bulk_lane(peer_id, 1).await {
            Ok(c) => c,
            Err(e) => {
                let _ = send0.reset(CLOSE_PAUSED.into());
                return Err(e);
            }
        };
        let (mut send1, mut recv1) = match conn1.open_bi().await {
            Ok(pair) => pair,
            Err(e) => {
                let _ = send0.reset(CLOSE_PAUSED.into());
                return Err(CoreError::NotConnected(format!(
                    "打开辅助分片上传通道失败：{e}"
                )));
            }
        };
        let _ = send1.set_priority(-16);
        let cmd1 = Command::FsWrite(FsWriteArgs {
            path: target_path.clone(),
            size: total_size,
            offset: seg1_start,
            range_length: Some(seg1_len),
            defer_finalize: true,
            modified_ms,
            conflict: ConflictPolicy::Overwrite,
        });
        if let Err(e) = write_frame(&mut send1, &cmd1.to_envelope()?).await {
            let _ = send0.reset(CLOSE_PAUSED.into());
            return Err(CoreError::Frame(e));
        }
        let ready1_res: Result<ResponseEnvelope<FsWriteReady>, CoreError> = async {
            let r = timeout(Duration::from_secs(15), read_frame(&mut recv1))
                .await
                .map_err(|_| CoreError::Timeout)??;
            Ok(r)
        }
        .await;
        match ready1_res.and_then(|r| r.into_result().map_err(CoreError::Rpc)) {
            Ok(_) => {}
            Err(e) => {
                let _ = send0.reset(CLOSE_PAUSED.into());
                let _ = send1.reset(CLOSE_PAUSED.into());
                return Err(e);
            }
        }

        if let Some(ref cb) = on_progress
            && resume_offset > 0
        {
            cb(resume_offset, total_size, 0.0);
        }

        let started = Instant::now();
        let total_sent = Arc::new(AtomicU64::new(resume_offset));
        let last_report_ms = Arc::new(AtomicU64::new(0));
        let abort = CancellationToken::new();
        let mut join_set = JoinSet::new();

        for (mut send, mut recv, seg_start, seg_len) in [
            (send0, recv0, resume_offset, seg0_len),
            (send1, recv1, seg1_start, seg1_len),
        ] {
            let node_c = self.clone();
            let peer_owned = peer_id.to_string();
            let local_fs_c = local_fs.to_path_buf();
            let local_disp_c = local_disp.to_string();
            let target_path_c = target_path.clone();
            let ctrl_c = ctrl.clone();
            let abort_c = abort.clone();
            let sent_c = total_sent.clone();
            let report_c = last_report_ms.clone();
            let prog_c = on_progress.clone();
            let wire_c = on_wire_delta.clone();

            join_set.spawn(async move {
                let fs_open = local_fs_c.clone();
                let disp_open = local_disp_c.clone();
                let mut std_file =
                    tokio::task::spawn_blocking(move || -> Result<File, RpcError> {
                        use std::io::Seek;
                        let mut f = open_sequential_read(&fs_open)
                            .map_err(|e| map_io_to_rpc(&e, Some(&disp_open)))?;
                        if seg_start > 0 {
                            f.seek(std::io::SeekFrom::Start(seg_start))
                                .map_err(|e| map_io_to_rpc(&e, Some(&disp_open)))?;
                        }
                        Ok(f)
                    })
                    .await
                    .map_err(|e| CoreError::Internal(format!("打开分片源文件异常：{e}")))?
                    .map_err(CoreError::Rpc)?;

                const POOL_CAP: usize = 6;
                let (data_tx, mut data_rx) =
                    tokio::sync::mpsc::channel::<std::io::Result<(Vec<u8>, usize)>>(POOL_CAP);
                let (recycle_tx, mut recycle_rx) =
                    tokio::sync::mpsc::channel::<Vec<u8>>(POOL_CAP);
                for _ in 0..POOL_CAP {
                    let _ = recycle_tx.try_send(vec![0u8; TRANSFER_CHUNK_SIZE]);
                }

                let reader_cancel = ctrl_c.cancel_token();
                let reader_abort = abort_c.clone();
                tokio::task::spawn_blocking(move || {
                    use std::io::Read;
                    let mut left = seg_len;
                    while left > 0 {
                        if reader_cancel.is_cancelled() || reader_abort.is_cancelled() {
                            break;
                        }
                        let Some(mut buf) = recycle_rx.blocking_recv() else {
                            break;
                        };
                        if buf.len() < TRANSFER_CHUNK_SIZE {
                            buf.resize(TRANSFER_CHUNK_SIZE, 0);
                        }
                        let want = (left as usize).min(TRANSFER_CHUNK_SIZE);
                        match std_file.read(&mut buf[..want]) {
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

                let mut seg_done = 0u64;
                while seg_done < seg_len {
                    if ctrl_c.is_cancelled() {
                        let _ = send.reset(CLOSE_CANCELLED.into());
                        return Err(CoreError::Cancelled);
                    }
                    if abort_c.is_cancelled() {
                        let code = if ctrl_c.is_cancelled() {
                            CLOSE_CANCELLED
                        } else {
                            CLOSE_PAUSED
                        };
                        let _ = send.reset(code.into());
                        return Err(CoreError::NotConnected("并行分片已联动中止".to_string()));
                    }
                    if ctrl_c.is_paused() {
                        ctrl_c.wait_ready().await.inspect_err(|_| {
                            let _ = send.reset(CLOSE_CANCELLED.into());
                        })?;
                    }

                    let cancel_tok = ctrl_c.cancel_token();
                    let next_chunk = tokio::select! {
                        _ = cancel_tok.cancelled() => {
                            let _ = send.reset(CLOSE_CANCELLED.into());
                            return Err(CoreError::Cancelled);
                        }
                        _ = abort_c.cancelled() => {
                            let _ = send.reset(CLOSE_PAUSED.into());
                            return Err(CoreError::NotConnected("并行分片已联动中止".to_string()));
                        }
                        item = data_rx.recv() => item,
                    };
                    let Some(chunk_res) = next_chunk else {
                        let _ = send.reset(CLOSE_PAUSED.into());
                        return Err(CoreError::Internal(
                            "读取分片本地文件时意外提前结束".to_string(),
                        ));
                    };
                    let (buf, n) = chunk_res?;

                    tokio::select! {
                        _ = cancel_tok.cancelled() => {
                            let _ = send.reset(CLOSE_CANCELLED.into());
                            return Err(CoreError::Cancelled);
                        }
                        _ = abort_c.cancelled() => {
                            let _ = send.reset(CLOSE_PAUSED.into());
                            return Err(CoreError::NotConnected("并行分片已联动中止".to_string()));
                        }
                        w = send.write_all(&buf[..n]) => {
                            w.map_err(|e| CoreError::NotConnected(format!("分片上传中断：{e}")))?;
                        }
                    }
                    let _ = recycle_tx.try_send(buf);
                    seg_done += n as u64;
                    let cur_total = sent_c.fetch_add(n as u64, Ordering::Relaxed) + n as u64;
                    if let Some(ref wcb) = wire_c {
                        wcb(n as u64);
                    }

                    let now_ms = started.elapsed().as_millis() as u64;
                    let prev_ms = report_c.load(Ordering::Relaxed);
                    if now_ms.saturating_sub(prev_ms) >= 100
                        && report_c
                            .compare_exchange(
                                prev_ms,
                                now_ms,
                                Ordering::Relaxed,
                                Ordering::Relaxed,
                            )
                            .is_ok()
                    {
                        let wire_done = cur_total.saturating_sub(resume_offset);
                        let speed =
                            wire_done as f64 / started.elapsed().as_secs_f64().max(0.001);
                        if let Some(ref cb) = prog_c {
                            cb(cur_total, total_size, speed);
                        }
                        let _ = node_c.events_tx.send(CoreEvent::TransferProgress(
                            TransferProgressEvent {
                                peer_id: peer_owned.clone(),
                                direction: TransferDirection::Upload,
                                local_path: local_disp_c.clone(),
                                remote_path: target_path_c.clone(),
                                transferred_bytes: cur_total,
                                total_bytes: total_size,
                                bytes_per_sec: speed,
                            },
                        ));
                    }
                }

                send.finish()
                    .map_err(|e| CoreError::Internal(format!("结束分片上传流失败：{e}")))?;
                let done_resp: ResponseEnvelope<FsWriteDone> =
                    timeout(Duration::from_secs(30), read_frame(&mut recv))
                        .await
                        .map_err(|_| CoreError::Timeout)??;
                let _ = done_resp.into_result()?;
                Ok::<(), CoreError>(())
            });
        }

        let mut first_err: Option<CoreError> = None;
        while let Some(joined) = join_set.join_next().await {
            match joined {
                Ok(Ok(())) => {}
                Ok(Err(err)) => {
                    abort.cancel();
                    if first_err.is_none() || matches!(err, CoreError::Cancelled) {
                        first_err = Some(err);
                    }
                }
                Err(join_err) => {
                    abort.cancel();
                    if first_err.is_none() {
                        first_err = Some(CoreError::Internal(format!(
                            "并行分片上传任务异常：{join_err}"
                        )));
                    }
                }
            }
        }

        if let Some(err) = first_err {
            return Err(err);
        }

        // 3. 发送 0 字节收尾请求触发远端落盘校验与原子改名
        let (mut fsend, mut frecv) = conn0
            .open_bi()
            .await
            .map_err(|e| CoreError::NotConnected(format!("打开分片收尾通道失败：{e}")))?;
        let _ = fsend.set_priority(8);
        let fin_cmd = Command::FsWrite(FsWriteArgs {
            path: target_path.clone(),
            size: total_size,
            offset: total_size,
            range_length: Some(0),
            defer_finalize: false,
            modified_ms,
            conflict: ConflictPolicy::Overwrite,
        });
        write_frame(&mut fsend, &fin_cmd.to_envelope()?).await?;
        let fin_ready: ResponseEnvelope<FsWriteReady> =
            timeout(Duration::from_secs(15), read_frame(&mut frecv))
                .await
                .map_err(|_| CoreError::Timeout)??;
        let _ = fin_ready.into_result()?;
        fsend
            .finish()
            .map_err(|e| CoreError::Internal(format!("结束分片收尾流失败：{e}")))?;
        let fin_done: ResponseEnvelope<FsWriteDone> =
            timeout(Duration::from_secs(30), read_frame(&mut frecv))
                .await
                .map_err(|_| CoreError::Timeout)??;
        let done = fin_done.into_result()?;

        let final_speed = rem as f64 / started.elapsed().as_secs_f64().max(0.001);
        if let Some(ref cb) = on_progress {
            cb(total_size, total_size, final_speed);
        }
        let _ = self.events_tx.send(CoreEvent::TransferProgress(
            TransferProgressEvent {
                peer_id: peer_id.to_string(),
                direction: TransferDirection::Upload,
                local_path: local_disp.to_string(),
                remote_path: done.target_path.clone(),
                transferred_bytes: total_size,
                total_bytes: total_size,
                bytes_per_sec: final_speed,
            },
        ));
        Ok(done)
    }
}
