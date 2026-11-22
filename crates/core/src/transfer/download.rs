use super::*;

impl CoreNode {
    #[allow(clippy::too_many_arguments)]
    pub async fn download_file_resumable(
        self: &Arc<Self>,
        peer_id: &str,
        remote_path: &str,
        local_dest: &Path,
        conflict: ConflictPolicy,
        cancel: CancellationToken,
        on_progress: Option<ProgressCallback>,
        on_wire_delta: Option<WireDeltaCallback>,
    ) -> Result<FsWriteDone, CoreError> {
        let ctrl = TransferTaskControl::from_token(cancel);
        self.download_file_with_ctrl(
            peer_id,
            remote_path,
            local_dest,
            conflict,
            &ctrl,
            None,
            on_progress,
            on_wire_delta,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn download_file_with_ctrl(
        self: &Arc<Self>,
        peer_id: &str,
        remote_path: &str,
        local_dest: &Path,
        conflict: ConflictPolicy,
        ctrl: &TransferTaskControl,
        speed_limit_bps: Option<u64>,
        on_progress: Option<ProgressCallback>,
        on_wire_delta: Option<WireDeltaCallback>,
    ) -> Result<FsWriteDone, CoreError> {
        ctrl.wait_ready().await?;
        let initial_clean = normalize_lexical_absolute(&local_dest.to_string_lossy())
            .map_err(CoreError::Rpc)?;
        let initial_fs = to_fs_path(&initial_clean);

        let (final_clean, final_fs) = if initial_fs.exists() {
            match conflict {
                ConflictPolicy::Fail => {
                    return Err(CoreError::Rpc(RpcError::new(
                        ErrorCode::AlreadyExists,
                        format!("本地目标文件已存在：{}", to_display_path(&initial_clean)),
                    )));
                }
                ConflictPolicy::Skip => {
                    return Ok(FsWriteDone {
                        target_path: to_display_path(&initial_clean),
                        bytes_written: 0,
                    });
                }
                ConflictPolicy::Overwrite => (initial_clean, initial_fs),
                ConflictPolicy::RenameKeepBoth => pick_unique_target(&initial_clean),
            }
        } else {
            (initial_clean, initial_fs)
        };

        let parent_clean = final_clean
            .parent()
            .ok_or_else(|| CoreError::InvalidArgument("无效的本地保存路径".to_string()))?;
        tokio::fs::create_dir_all(to_fs_path(parent_clean)).await?;

        let file_name = final_clean
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let part_clean = parent_clean.join(deterministic_part_name(&file_name));
        let part_fs = to_fs_path(&part_clean);

        // 检查本地是否已有 `.pcshare.part` 并与远端前 N 字节校验值核对
        let mut resume_offset = 0u64;
        if conflict != ConflictPolicy::RenameKeepBoth
            && let Ok(p_meta) = tokio::fs::metadata(&part_fs).await
            && p_meta.is_file()
            && p_meta.len() > 0
        {
            let part_len = p_meta.len();
            let part_fs_c = part_fs.clone();
            let local_hex_opt =
                tokio::task::spawn_blocking(move || hash_file_prefix_sync(&part_fs_c, part_len).ok())
                    .await
                    .ok()
                    .flatten();
            if let Some(local_hex) = local_hex_opt
                && let Ok(remote_h) = self.remote_hash(peer_id, remote_path, Some(part_len)).await
                && remote_h.size >= part_len
                && remote_h.blake3_hex == local_hex
            {
                resume_offset = part_len;
            }
        }

        let conn = self.get_or_connect(peer_id).await?;
        let (mut send, mut recv) = conn
            .open_bi()
            .await
            .map_err(|e| CoreError::NotConnected(format!("打开下载通道失败：{e}")))?;

        let cmd = Command::FsRead(FsReadArgs {
            path: remote_path.to_string(),
            offset: resume_offset,
            length: None,
        });
        write_frame(&mut send, &cmd.to_envelope()?).await?;
        send.finish()
            .map_err(|e| CoreError::Internal(format!("结束下载请求流失败：{e}")))?;

        let header_resp: ResponseEnvelope<FsReadHeader> =
            timeout(Duration::from_secs(15), read_frame(&mut recv))
                .await
                .map_err(|_| CoreError::Timeout)??;
        let header = header_resp.into_result()?;

        let total = header.file_size;
        let final_disp = to_display_path(&final_clean);
        let remaining_total = total.saturating_sub(resume_offset);

        // 对 >= 32 MiB 且未开启限速的大文件启用 4 流并发区间拉取 + 单写线程随机偏移落盘（支持连续前缀断点续传）
        const PARALLEL_RANGE_MIN_BYTES: u64 = 32 * 1024 * 1024;
        if remaining_total >= PARALLEL_RANGE_MIN_BYTES
            && self
                .rate_limiter()
                .effective_limit_bps(ctrl.speed_limit_bps(), speed_limit_bps)
                == 0
        {
            return self
                .download_parallel_ranges(
                    peer_id,
                    &conn,
                    recv,
                    remote_path,
                    &part_fs,
                    &final_fs,
                    &final_disp,
                    resume_offset,
                    total,
                    header.modified_ms,
                    ctrl,
                    on_progress,
                    on_wire_delta,
                )
                .await;
        }

        let part_fs_open = part_fs.clone();
        let part_file = tokio::task::spawn_blocking(move || -> std::io::Result<File> {
            use std::io::Seek;
            if resume_offset > 0 {
                let mut f = open_sequential_rw(&part_fs_open)?;
                f.set_len(resume_offset)?;
                f.seek(std::io::SeekFrom::Start(resume_offset))?;
                Ok(f)
            } else {
                create_sequential_write(&part_fs_open)
            }
        })
        .await
        .map_err(|e| CoreError::Internal(format!("打开本地下载文件异常：{e}")))??;

        const DL_POOL_CAP: usize = 6;
        let (write_tx, mut write_rx) = tokio::sync::mpsc::channel::<(Vec<u8>, usize)>(DL_POOL_CAP);
        let (recycle_tx, mut recycle_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(DL_POOL_CAP);
        for _ in 0..DL_POOL_CAP {
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

        let started = Instant::now();
        let mut last_report = Instant::now();
        let mut received = resume_offset;
        let mut dl_err: Option<CoreError> = None;
        let mut remove_part = false;

        while received < total {
            if ctrl.is_cancelled() {
                let _ = recv.stop(CLOSE_CANCELLED.into());
                dl_err = Some(CoreError::Cancelled);
                remove_part = true;
                break;
            }
            if ctrl.is_paused()
                && let Err(e) = ctrl.wait_ready().await
            {
                let _ = recv.stop(CLOSE_CANCELLED.into());
                dl_err = Some(e);
                remove_part = true;
                break;
            }

            let Some(mut buf) = recycle_rx.recv().await else {
                dl_err = Some(CoreError::Internal("下载写盘缓冲池提前关闭".to_string()));
                break;
            };
            if buf.len() < TRANSFER_CHUNK_SIZE {
                buf.resize(TRANSFER_CHUNK_SIZE, 0);
            }

            let want = ((total - received) as usize).min(TRANSFER_CHUNK_SIZE);
            let cancel_tok = ctrl.cancel_token();
            let mut filled = 0usize;
            while filled < want {
                let step = tokio::select! {
                    _ = cancel_tok.cancelled() => {
                        let _ = recv.stop(CLOSE_CANCELLED.into());
                        dl_err = Some(CoreError::Cancelled);
                        remove_part = true;
                        break;
                    }
                    r = recv.read(&mut buf[filled..want]) => r,
                };
                match step {
                    Ok(Some(n)) => {
                        filled += n;
                    }
                    Ok(None) => {
                        dl_err = Some(CoreError::NotConnected(
                            "下载尚未完成但连接已中断，已保留断点数据".to_string(),
                        ));
                        break;
                    }
                    Err(e) => {
                        dl_err = Some(CoreError::NotConnected(format!("下载数据流中断：{e}")));
                        break;
                    }
                }
            }

            if filled > 0 && !remove_part {
                if let Err(err) = self
                    .rate_limiter()
                    .acquire_with_ctrl(filled, ctrl, speed_limit_bps)
                    .await
                {
                    let _ = recv.stop(CLOSE_CANCELLED.into());
                    dl_err = Some(err);
                    remove_part = true;
                    break;
                }
                received += filled as u64;
                if let Some(ref wcb) = on_wire_delta {
                    wcb(filled as u64);
                }
                if write_tx.send((buf, filled)).await.is_err() {
                    dl_err = Some(CoreError::Internal("下载写盘通道意外断开".to_string()));
                    break;
                }

                if last_report.elapsed() >= Duration::from_millis(100) || received == total {
                    let wire_done = received.saturating_sub(resume_offset);
                    let speed = wire_done as f64 / started.elapsed().as_secs_f64().max(0.001);
                    if let Some(ref cb) = on_progress {
                        cb(received, total, speed);
                    }
                    let _ = self.events_tx.send(CoreEvent::TransferProgress(
                        TransferProgressEvent {
                            peer_id: peer_id.to_string(),
                            direction: TransferDirection::Download,
                            local_path: final_disp.clone(),
                            remote_path: remote_path.to_string(),
                            transferred_bytes: received,
                            total_bytes: total,
                            bytes_per_sec: speed,
                        },
                    ));
                    last_report = Instant::now();
                }
            }
            if dl_err.is_some() {
                break;
            }
        }
        drop(write_tx);

        let writer_res = writer_handle
            .await
            .map_err(|e| CoreError::Internal(format!("下载写盘线程异常：{e}")))?;

        if let Some(err) = dl_err {
            drop(writer_res);
            if remove_part {
                let _ = fs::remove_file(&part_fs);
            }
            return Err(err);
        }
        if ctrl.is_cancelled() {
            let _ = recv.stop(CLOSE_CANCELLED.into());
            drop(writer_res);
            let _ = fs::remove_file(&part_fs);
            return Err(CoreError::Cancelled);
        }

        let std_file = writer_res?;
        let modified_ms = header.modified_ms;
        let part_fs_c = part_fs.clone();
        let final_fs_c = final_fs.clone();
        let final_disp_c = final_disp.clone();
        tokio::task::spawn_blocking(move || -> Result<(), RpcError> {
            std_file
                .sync_all()
                .map_err(|e| map_io_to_rpc(&e, Some(&final_disp_c)))?;
            if let Some(ms) = modified_ms {
                let _ = std_file.set_modified(UNIX_EPOCH + Duration::from_millis(ms));
            }
            drop(std_file);
            replace_file_atomically(&part_fs_c, &final_fs_c)
                .map_err(|e| map_io_to_rpc(&e, Some(&final_disp_c)))
        })
        .await
        .map_err(|e| CoreError::Internal(format!("下载改名异常：{e}")))??;

        Ok(FsWriteDone {
            target_path: final_disp,
            bytes_written: received,
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn download_parallel_ranges(
        self: &Arc<Self>,
        peer_id: &str,
        conn: &Connection,
        first_recv: iroh::endpoint::RecvStream,
        remote_path: &str,
        part_fs: &Path,
        final_fs: &Path,
        final_disp: &str,
        resume_offset: u64,
        total: u64,
        modified_ms: Option<u64>,
        ctrl: &TransferTaskControl,
        on_progress: Option<ProgressCallback>,
        on_wire_delta: Option<WireDeltaCallback>,
    ) -> Result<FsWriteDone, CoreError> {
        const STREAMS: usize = 2;
        const POOL_CAP: usize = 12;

        let remaining = total - resume_offset;
        let align = TRANSFER_CHUNK_SIZE as u64;
        let base_span = ((remaining / STREAMS as u64) / align).max(1) * align;
        let mut ranges: Vec<(u64, u64)> = Vec::with_capacity(STREAMS);
        let mut cur = resume_offset;
        for idx in 0..STREAMS {
            if cur >= total {
                break;
            }
            let end = if idx + 1 == STREAMS {
                total
            } else {
                (cur + base_span).min(total)
            };
            ranges.push((cur, end));
            cur = end;
        }
        let seg_count = ranges.len();
        let seg_cursors: Arc<Vec<AtomicU64>> = Arc::new(
            ranges
                .iter()
                .map(|&(start, _)| AtomicU64::new(start))
                .collect(),
        );

        let part_fs_open = part_fs.to_path_buf();
        let part_file = tokio::task::spawn_blocking(move || -> std::io::Result<File> {
            let mut opts = fs::OpenOptions::new();
            opts.read(true).write(true).create(true);
            if resume_offset == 0 {
                opts.truncate(true);
            }
            let f = opts.open(&part_fs_open)?;
            if resume_offset > 0 {
                f.set_len(resume_offset)?;
            }
            Ok(f)
        })
        .await
        .map_err(|e| CoreError::Internal(format!("打开多流下载临时文件异常：{e}")))??;

        let (write_tx, mut write_rx) =
            tokio::sync::mpsc::channel::<(usize, u64, Vec<u8>, usize)>(POOL_CAP);
        let (recycle_tx, recycle_rx) =
            tokio::sync::mpsc::channel::<Vec<u8>>(POOL_CAP);
        for _ in 0..POOL_CAP {
            let _ = recycle_tx.try_send(vec![0u8; TRANSFER_CHUNK_SIZE]);
        }
        let shared_recycle_rx = Arc::new(tokio::sync::Mutex::new(recycle_rx));

        let cursors_for_writer = seg_cursors.clone();
        let writer_handle = tokio::task::spawn_blocking(move || -> std::io::Result<File> {
            let mut f = part_file;
            while let Some((seg_idx, offset, buf, n)) = write_rx.blocking_recv() {
                write_all_at_offset(&mut f, &buf[..n], offset)?;
                cursors_for_writer[seg_idx].store(offset + n as u64, Ordering::Release);
                let _ = recycle_tx.try_send(buf);
            }
            Ok(f)
        });

        let started = Instant::now();
        let last_report_ms = Arc::new(AtomicU64::new(0));
        let wire_done = Arc::new(AtomicU64::new(0));
        let sub_ctrl = ctrl.child();
        let mut set: JoinSet<Result<(), CoreError>> = JoinSet::new();
        let mut first_recv_opt = Some(first_recv);

        for (seg_idx, &(seg_start, seg_end)) in ranges.iter().enumerate() {
            let node_c = self.clone();
            let fallback_conn = conn.clone();
            let rpath = remote_path.to_string();
            let sub_c = sub_ctrl.clone();
            let tx_c = write_tx.clone();
            let pool_rx = shared_recycle_rx.clone();
            let wire_c = wire_done.clone();
            let last_ms_c = last_report_ms.clone();
            let cb_c = on_progress.clone();
            let wcb_c = on_wire_delta.clone();
            let ev_tx = self.events_tx.clone();
            let pid_str = peer_id.to_string();
            let fdisp = final_disp.to_string();
            let pre_opened = if seg_idx == 0 {
                first_recv_opt.take()
            } else {
                None
            };

            set.spawn(async move {
                let (mut recv_stream, stop_at_end) = if let Some(r0) = pre_opened {
                    (r0, seg_count > 1)
                } else {
                    let lane_conn = node_c
                        .get_or_connect_bulk_lane(&pid_str, seg_idx)
                        .await
                        .unwrap_or(fallback_conn);
                    let (mut s, mut r) = lane_conn
                        .open_bi()
                        .await
                        .map_err(|e| CoreError::NotConnected(format!("打开分片下载流失败：{e}")))?;
                    let _ = s.set_priority(-16);
                    let cmd = Command::FsRead(FsReadArgs {
                        path: rpath.clone(),
                        offset: seg_start,
                        length: Some(seg_end - seg_start),
                    });
                    write_frame(&mut s, &cmd.to_envelope()?).await?;
                    s.finish()
                        .map_err(|e| CoreError::Internal(format!("结束分片请求失败：{e}")))?;
                    let hdr_resp: ResponseEnvelope<FsReadHeader> =
                        timeout(Duration::from_secs(15), read_frame(&mut r))
                            .await
                            .map_err(|_| CoreError::Timeout)??;
                    let _ = hdr_resp.into_result()?;
                    (r, false)
                };

                let mut pos = seg_start;
                while pos < seg_end {
                    if sub_c.is_cancelled() {
                        let _ = recv_stream.stop(CLOSE_CANCELLED.into());
                        return Err(CoreError::Cancelled);
                    }
                    if sub_c.is_paused() {
                        sub_c.wait_ready().await.inspect_err(|_| {
                            let _ = recv_stream.stop(CLOSE_CANCELLED.into());
                        })?;
                    }

                    let cancel_tok = sub_c.cancel_token();
                    let buf_opt = {
                        let mut guard = pool_rx.lock().await;
                        tokio::select! {
                            _ = cancel_tok.cancelled() => {
                                let _ = recv_stream.stop(CLOSE_CANCELLED.into());
                                return Err(CoreError::Cancelled);
                            }
                            b = guard.recv() => b,
                        }
                    };
                    let Some(mut buf) = buf_opt else {
                        return Err(CoreError::Internal("多流下载缓冲池提前关闭".to_string()));
                    };
                    if buf.len() < TRANSFER_CHUNK_SIZE {
                        buf.resize(TRANSFER_CHUNK_SIZE, 0);
                    }

                    let want = ((seg_end - pos) as usize).min(TRANSFER_CHUNK_SIZE);
                    let mut filled = 0usize;
                    let cancel_tok = sub_c.cancel_token();
                    while filled < want {
                        let r = tokio::select! {
                            _ = cancel_tok.cancelled() => {
                                let _ = recv_stream.stop(CLOSE_CANCELLED.into());
                                return Err(CoreError::Cancelled);
                            }
                            step = recv_stream.read(&mut buf[filled..want]) => step,
                        };
                        match r {
                            Ok(Some(n)) => filled += n,
                            Ok(None) => {
                                if filled > 0 {
                                    let _ = tx_c.send((seg_idx, pos, buf, filled)).await;
                                }
                                return Err(CoreError::NotConnected(
                                    "分片下载尚未完成但连接已断开，已保留连续前缀断点".to_string(),
                                ));
                            }
                            Err(e) => {
                                if filled > 0 {
                                    let _ = tx_c.send((seg_idx, pos, buf, filled)).await;
                                }
                                return Err(CoreError::NotConnected(format!("分片下载中断：{e}")));
                            }
                        }
                    }

                    let write_offset = pos;
                    pos += filled as u64;
                    tx_c.send((seg_idx, write_offset, buf, filled))
                        .await
                        .map_err(|_| CoreError::Internal("分片写盘通道断开".to_string()))?;

                    if let Some(ref wcb) = wcb_c {
                        wcb(filled as u64);
                    }
                    let done_now = wire_c.fetch_add(filled as u64, Ordering::Relaxed) + filled as u64;
                    let transferred = resume_offset + done_now;
                    let elapsed_ms = started.elapsed().as_millis() as u64;
                    let prev_ms = last_ms_c.load(Ordering::Relaxed);
                    if (elapsed_ms.saturating_sub(prev_ms) >= 100 || transferred == total)
                        && last_ms_c
                            .compare_exchange(
                                prev_ms,
                                elapsed_ms,
                                Ordering::Relaxed,
                                Ordering::Relaxed,
                            )
                            .is_ok()
                    {
                        let speed = done_now as f64 / started.elapsed().as_secs_f64().max(0.001);
                        if let Some(ref cb) = cb_c {
                            cb(transferred, total, speed);
                        }
                        let _ = ev_tx.send(CoreEvent::TransferProgress(TransferProgressEvent {
                            peer_id: pid_str.clone(),
                            direction: TransferDirection::Download,
                            local_path: fdisp.clone(),
                            remote_path: rpath.clone(),
                            transferred_bytes: transferred,
                            total_bytes: total,
                            bytes_per_sec: speed,
                        }));
                    }
                }

                if stop_at_end {
                    let _ = recv_stream.stop(0u32.into());
                }
                Ok(())
            });
        }
        drop(write_tx);

        let mut first_err: Option<CoreError> = None;
        while let Some(res) = set.join_next().await {
            match res {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    if first_err.is_none() {
                        first_err = Some(e);
                        sub_ctrl.cancel();
                    }
                }
                Err(join_err) => {
                    if first_err.is_none() {
                        first_err = Some(CoreError::Internal(format!("分片任务异常：{join_err}")));
                        sub_ctrl.cancel();
                    }
                }
            }
        }

        let writer_res = writer_handle
            .await
            .map_err(|e| CoreError::Internal(format!("多流写盘线程异常：{e}")))?;

        if ctrl.is_cancelled() || matches!(first_err, Some(CoreError::Cancelled)) {
            drop(writer_res);
            let _ = fs::remove_file(part_fs);
            return Err(CoreError::Cancelled);
        }

        if let Some(err) = first_err {
            // 截断到从头连续写完的最高偏移，使 .pcshare.part 始终保持合法连续前缀以供下次续传
            if let Ok(std_file) = writer_res {
                let mut contiguous = resume_offset;
                for (idx, &(_, seg_end)) in ranges.iter().enumerate() {
                    let cur_end = seg_cursors[idx].load(Ordering::Acquire);
                    contiguous = cur_end;
                    if cur_end < seg_end {
                        break;
                    }
                }
                let _ = tokio::task::spawn_blocking(move || {
                    use std::io::Write;
                    let _ = std_file.set_len(contiguous);
                    let _ = (&std_file).flush();
                })
                .await;
            }
            return Err(err);
        }

        let std_file = writer_res?;
        let part_fs_c = part_fs.to_path_buf();
        let final_fs_c = final_fs.to_path_buf();
        let final_disp_c = final_disp.to_string();
        tokio::task::spawn_blocking(move || -> Result<(), RpcError> {
            std_file
                .set_len(total)
                .map_err(|e| map_io_to_rpc(&e, Some(&final_disp_c)))?;
            std_file
                .sync_all()
                .map_err(|e| map_io_to_rpc(&e, Some(&final_disp_c)))?;
            if let Some(ms) = modified_ms {
                let _ = std_file.set_modified(UNIX_EPOCH + Duration::from_millis(ms));
            }
            drop(std_file);
            replace_file_atomically(&part_fs_c, &final_fs_c)
                .map_err(|e| map_io_to_rpc(&e, Some(&final_disp_c)))
        })
        .await
        .map_err(|e| CoreError::Internal(format!("多流下载改名异常：{e}")))??;

        Ok(FsWriteDone {
            target_path: final_disp.to_string(),
            bytes_written: total,
        })
    }
}
