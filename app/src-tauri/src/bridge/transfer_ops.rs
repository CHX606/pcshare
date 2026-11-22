use super::*;

impl BridgeState {
    // ────────────────────────── 跨设备增强传输（任务 2.4） ──────────────────────────

    pub async fn transfer_start(
        self: &Arc<Self>,
        src: String,
        sources: Vec<String>,
        dst: String,
        dest_dir: String,
        conflict: UiConflictPolicy,
    ) -> Reply<String> {
        if sources.is_empty() {
            return Reply::err(UiErrorCode::Invalid, "请先选择要传输的文件或文件夹");
        }
        if src == dst {
            return Reply::err(
                UiErrorCode::Invalid,
                "源设备和目标设备相同，请使用复制或移动",
            );
        }

        // 并发核验源路径存在并生成任务标题（阶段 2 原生支持文件与递归文件夹）
        let sem = Arc::new(Semaphore::new(STAT_CONCURRENCY));
        let mut join_set: JoinSet<(usize, Result<DirEntry, CoreError>)> = JoinSet::new();
        for (idx, s) in sources.iter().cloned().enumerate() {
            let node = self.node.clone();
            let src_c = src.clone();
            let sem_c = sem.clone();
            join_set.spawn(async move {
                let Ok(_permit) = sem_c.acquire_owned().await else {
                    return (idx, Err(CoreError::Cancelled));
                };
                let res = if src_c == SELF_ID {
                    node.fs_service()
                        .stat(&Permissions::default(), FsStatArgs { path: s })
                        .await
                        .map(|r| r.entry)
                        .map_err(CoreError::Rpc)
                } else {
                    node.remote_stat(&src_c, &s).await.map(|r| r.entry)
                };
                (idx, res)
            });
        }
        let mut ordered: Vec<Option<DirEntry>> = (0..sources.len()).map(|_| None).collect();
        while let Some(joined) = join_set.join_next().await {
            match joined {
                Ok((idx, Ok(entry))) => ordered[idx] = Some(entry),
                Ok((_, Err(err))) => {
                    join_set.abort_all();
                    return Reply::from_core(Err(err));
                }
                Err(err) => {
                    join_set.abort_all();
                    return Reply::err(UiErrorCode::Internal, format!("核验源文件任务异常：{err}"));
                }
            }
        }
        let mut names = Vec::with_capacity(sources.len());
        let mut initial_bytes = 0u64;
        for entry in ordered.into_iter().flatten() {
            initial_bytes = initial_bytes.saturating_add(entry.size);
            names.push(entry.name);
        }

        let kind = if src == SELF_ID {
            UiTaskKind::Upload
        } else if dst == SELF_ID {
            UiTaskKind::Download
        } else {
            UiTaskKind::Relay
        };
        let title = if names.len() == 1 {
            names[0].clone()
        } else {
            format!("{} 等 {} 项", names[0], names.len())
        };

        let (seq, ui_task_id) = self.next_task_id();
        let control = TransferTaskControl::new();
        let bytes_done = Arc::new(AtomicU64::new(0));
        let bytes_total = Arc::new(AtomicU64::new(initial_bytes.max(1)));
        let speed_bits = Arc::new(AtomicU64::new(0));
        let meta = TransferMeta {
            seq,
            kind,
            title: title.clone(),
            src_device: src.clone(),
            dst_device: dst.clone(),
            bytes_done: bytes_done.clone(),
            bytes_total: bytes_total.clone(),
            speed_bits,
        };

        self.active_tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                ui_task_id.clone(),
                CancellableTask::Transfer {
                    control: control.clone(),
                    meta: meta.clone(),
                },
            );

        self.sink.emit_task_progress(UiTaskProgress {
            id: ui_task_id.clone(),
            kind,
            title: title.clone(),
            src_device: src.clone(),
            dst_device: dst.clone(),
            bytes_done: 0,
            bytes_total: initial_bytes.max(1),
            speed: 0.0,
            state: UiTaskState::Running,
            error: None,
        });

        let bridge = self.clone();
        let tid = ui_task_id.clone();
        let proto_conflict = conflict.to_proto();
        tokio::spawn(async move {
            bridge
                .run_stage2_transfer_worker(
                    tid,
                    meta,
                    sources,
                    dest_dir,
                    proto_conflict,
                    control,
                )
                .await;
        });

        Reply::ok(ui_task_id)
    }

    pub fn task_list(&self) -> Reply<Vec<UiTaskProgress>> {
        let guard = self.active_tasks.lock().unwrap_or_else(|e| e.into_inner());
        let mut items: Vec<(u64, UiTaskProgress)> = guard
            .iter()
            .map(|(id, task)| match task {
                CancellableTask::Transfer { control, meta } => {
                    let paused = control.is_paused();
                    let bytes_total = meta.bytes_total.load(Ordering::Relaxed).max(1);
                    let bytes_done = meta.bytes_done.load(Ordering::Relaxed).min(bytes_total);
                    let speed = if paused {
                        0.0
                    } else {
                        f64::from_bits(meta.speed_bits.load(Ordering::Relaxed))
                    };
                    (
                        meta.seq,
                        UiTaskProgress {
                            id: id.clone(),
                            kind: meta.kind,
                            title: meta.title.clone(),
                            src_device: meta.src_device.clone(),
                            dst_device: meta.dst_device.clone(),
                            bytes_done,
                            bytes_total,
                            speed,
                            state: if paused {
                                UiTaskState::Paused
                            } else {
                                UiTaskState::Running
                            },
                            error: None,
                        },
                    )
                }
                CancellableTask::LocalFsTask { meta, .. }
                | CancellableTask::RemoteFsTask { meta, .. } => {
                    let bytes_total = meta.bytes_total.load(Ordering::Relaxed).max(1);
                    let bytes_done = meta.bytes_done.load(Ordering::Relaxed).min(bytes_total);
                    let speed = f64::from_bits(meta.speed_bits.load(Ordering::Relaxed));
                    (
                        meta.seq,
                        UiTaskProgress {
                            id: id.clone(),
                            kind: meta.kind,
                            title: meta.title.clone(),
                            src_device: meta.device_id.clone(),
                            dst_device: meta.device_id.clone(),
                            bytes_done,
                            bytes_total,
                            speed,
                            state: UiTaskState::Running,
                            error: None,
                        },
                    )
                }
            })
            .collect();
        items.sort_by_key(|(seq, _)| *seq);
        Reply::ok(items.into_iter().map(|(_, p)| p).collect())
    }

    pub fn task_pause(&self, task_id: String) -> Reply<()> {
        let guard = self.active_tasks.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(CancellableTask::Transfer { control, meta }) = guard.get(&task_id) {
            control.pause();
            meta.speed_bits.store(0, Ordering::Relaxed);
            let p = UiTaskProgress {
                id: task_id,
                kind: meta.kind,
                title: meta.title.clone(),
                src_device: meta.src_device.clone(),
                dst_device: meta.dst_device.clone(),
                bytes_done: meta.bytes_done.load(Ordering::Relaxed),
                bytes_total: meta.bytes_total.load(Ordering::Relaxed).max(1),
                speed: 0.0,
                state: UiTaskState::Paused,
                error: None,
            };
            drop(guard);
            self.sink.emit_task_progress(p);
        }
        Reply::ok(())
    }

    pub fn task_resume(&self, task_id: String) -> Reply<()> {
        let guard = self.active_tasks.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(CancellableTask::Transfer { control, meta }) = guard.get(&task_id) {
            control.resume();
            let p = UiTaskProgress {
                id: task_id,
                kind: meta.kind,
                title: meta.title.clone(),
                src_device: meta.src_device.clone(),
                dst_device: meta.dst_device.clone(),
                bytes_done: meta.bytes_done.load(Ordering::Relaxed),
                bytes_total: meta.bytes_total.load(Ordering::Relaxed).max(1),
                speed: 0.0,
                state: UiTaskState::Running,
                error: None,
            };
            drop(guard);
            self.sink.emit_task_progress(p);
        }
        Reply::ok(())
    }

    pub async fn task_cancel(self: &Arc<Self>, task_id: String) -> Reply<()> {
        let entry = self
            .active_tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&task_id);
        match entry {
            Some(CancellableTask::Transfer { control, .. }) => {
                control.cancel();
                Reply::ok(())
            }
            Some(CancellableTask::LocalFsTask { raw_task_id, .. }) => {
                let _ = self.node.fs_service().task_cancel(&raw_task_id);
                Reply::ok(())
            }
            Some(CancellableTask::RemoteFsTask {
                peer_id,
                remote_task_id,
                ..
            }) => {
                let _ = self.node.remote_task_cancel(&peer_id, &remote_task_id).await;
                Reply::ok(())
            }
            None => Reply::ok(()),
        }
    }

    pub fn transfer_get_speed_limit(&self) -> Reply<Option<u64>> {
        Reply::ok(self.node.rate_limiter().limit_bps())
    }

    pub fn transfer_set_speed_limit(&self, bytes_per_sec: Option<u64>) -> Reply<()> {
        self.node.rate_limiter().set_limit_bps(bytes_per_sec);
        Reply::ok(())
    }

    pub async fn pick_local_files(&self) -> Reply<Vec<String>> {
        match self.node.platform().pick_files().await {
            Ok(paths) => Reply::ok(paths.into_iter().map(|p| to_display_path(&p)).collect()),
            Err(pe) => Reply::from_rpc(Err(map_platform_to_rpc(&pe))),
        }
    }

    pub async fn pick_local_dir(&self) -> Reply<Option<String>> {
        match self.node.platform().pick_folder().await {
            Ok(opt) => Reply::ok(opt.map(|p| to_display_path(&p))),
            Err(pe) => Reply::from_rpc(Err(map_platform_to_rpc(&pe))),
        }
    }

    // ────────────────────────── 后台任务进度与限流辅助 ──────────────────────────

    pub(super) fn next_task_id(&self) -> (u64, String) {
        let n = self.task_seq.fetch_add(1, Ordering::Relaxed);
        (n, format!("ui-task-{n}"))
    }

    async fn run_stage2_transfer_worker(
        self: Arc<Self>,
        ui_task_id: String,
        meta: TransferMeta,
        sources: Vec<String>,
        dest_dir: String,
        conflict: ConflictPolicy,
        control: TransferTaskControl,
    ) {
        let last_emit = Arc::new(Mutex::new(Instant::now()));
        let started = Instant::now();
        // 留空 speed_limit_bps，使在途任务动态跟随 CoreNode::rate_limiter() 实时全局限速而非静态冻结
        let opts = TransferQueueOptions {
            conflict,
            concurrency: 4,
            speed_limit_bps: None,
        };

        let make_cb =
            |base_done: u64, total_scale: u64, base_total: u64| -> pcshare_core::ProgressCallback {
                let sink = self.sink.clone();
                let last_c = last_emit.clone();
                let tid = ui_task_id.clone();
                let meta_c = meta.clone();
                let ctrl_c = control.clone();
                Arc::new(move |done: u64, tot: u64, speed: f64| {
                    let scaled_done = base_done.saturating_add(done);
                    let scaled_tot = base_total
                        .saturating_add(tot.saturating_mul(total_scale))
                        .max(scaled_done)
                        .max(1);
                    meta_c.bytes_done.store(scaled_done, Ordering::Relaxed);
                    meta_c.bytes_total.store(scaled_tot, Ordering::Relaxed);
                    meta_c.speed_bits.store(speed.to_bits(), Ordering::Relaxed);
                    if ctrl_c.is_paused() {
                        return;
                    }
                    emit_throttled_running(
                        &sink,
                        &last_c,
                        UiTaskProgress {
                            id: tid.clone(),
                            kind: meta_c.kind,
                            title: meta_c.title.clone(),
                            src_device: meta_c.src_device.clone(),
                            dst_device: meta_c.dst_device.clone(),
                            bytes_done: scaled_done.min(scaled_tot),
                            bytes_total: scaled_tot,
                            speed,
                            state: UiTaskState::Running,
                            error: None,
                        },
                    );
                })
            };

        let outcome: Result<u64, CoreError> = match meta.kind {
            UiTaskKind::Upload => {
                let local_paths: Vec<PathBuf> = sources.iter().map(PathBuf::from).collect();
                self.node
                    .transfer_paths_to_peer(
                        &meta.dst_device,
                        &local_paths,
                        &dest_dir,
                        opts,
                        control.clone(),
                        Some(make_cb(0, 1, 0)),
                    )
                    .await
            }
            UiTaskKind::Download => {
                self.node
                    .download_paths_from_peer(
                        &meta.src_device,
                        &sources,
                        Path::new(&dest_dir),
                        opts,
                        control.clone(),
                        Some(make_cb(0, 1, 0)),
                    )
                    .await
            }
            UiTaskKind::Relay => {
                // 任务 2.4：两台受控电脑之间优先建立一次性授权直传（数据不经过本机），
                // 仅在连接/网络/不支持类错误时退回本机中转，确定性权限/路径/取消错误直接返回。
                let direct_res = self
                    .node
                    .transfer_between_peers_direct(
                        &meta.src_device,
                        &sources,
                        &meta.dst_device,
                        &dest_dir,
                        conflict,
                        control.cancel_token(),
                        Some(make_cb(0, 1, 0)),
                    )
                    .await;
                match direct_res {
                    Ok(()) => Ok(meta.bytes_total.load(Ordering::Relaxed)),
                    Err(err) if !should_fallback_direct_to_relay(&err) => Err(err),
                    Err(_) => {
                        let relay_tmp = std::env::temp_dir().join(format!(
                            "pcshare_relay_dir_{}_{}",
                            ui_task_id,
                            fastrand::u64(..)
                        ));
                        if let Err(e) = tokio::fs::create_dir_all(&relay_tmp).await {
                            Err(CoreError::Io(e))
                        } else {
                            let dl_res = self
                                .node
                                .download_paths_from_peer(
                                    &meta.src_device,
                                    &sources,
                                    &relay_tmp,
                                    TransferQueueOptions {
                                        conflict: ConflictPolicy::Overwrite,
                                        concurrency: 4,
                                        speed_limit_bps: None,
                                    },
                                    control.clone(),
                                    Some(make_cb(0, 2, 0)),
                                )
                                .await;
                            let up_res = match dl_res {
                                Ok(dl_bytes) => {
                                    let dl_base =
                                        dl_bytes.max(meta.bytes_done.load(Ordering::Relaxed));
                                    let mut staged = Vec::new();
                                    if let Ok(mut rd) = tokio::fs::read_dir(&relay_tmp).await {
                                        while let Ok(Some(item)) = rd.next_entry().await {
                                            staged.push(item.path());
                                        }
                                    }
                                    self.node
                                        .transfer_paths_to_peer(
                                            &meta.dst_device,
                                            &staged,
                                            &dest_dir,
                                            opts,
                                            control.clone(),
                                            Some(make_cb(dl_base, 1, dl_base)),
                                        )
                                        .await
                                        .map(|up_bytes| dl_base.saturating_add(up_bytes))
                                }
                                Err(e) => Err(e),
                            };
                            let _ = tokio::fs::remove_dir_all(&relay_tmp).await;
                            up_res
                        }
                    }
                }
            }
            _ => unreachable!(),
        };

        self.active_tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&ui_task_id);

        let cur_done = meta.bytes_done.load(Ordering::Relaxed);
        let cur_tot = meta.bytes_total.load(Ordering::Relaxed).max(cur_done).max(1);
        match outcome {
            Ok(written) => {
                let final_tot = written.max(cur_tot).max(1);
                let avg_speed = final_tot as f64 / started.elapsed().as_secs_f64().max(0.001);
                let done_title = meta.title.clone();
                self.sink.emit_task_progress(UiTaskProgress {
                    id: ui_task_id,
                    kind: meta.kind,
                    title: meta.title,
                    src_device: meta.src_device,
                    dst_device: meta.dst_device,
                    bytes_done: final_tot,
                    bytes_total: final_tot,
                    speed: avg_speed,
                    state: UiTaskState::Done,
                    error: None,
                });
                self.sink.emit_system_notice(UiSystemNotice {
                    title: "传输已完成".to_string(),
                    body: done_title,
                    kind: "success".to_string(),
                });
            }
            Err(CoreError::Cancelled) => {
                self.sink.emit_task_progress(UiTaskProgress {
                    id: ui_task_id,
                    kind: meta.kind,
                    title: meta.title,
                    src_device: meta.src_device,
                    dst_device: meta.dst_device,
                    bytes_done: cur_done,
                    bytes_total: cur_tot,
                    speed: 0.0,
                    state: UiTaskState::Cancelled,
                    error: None,
                });
            }
            Err(err) => {
                self.sink.emit_task_progress(UiTaskProgress {
                    id: ui_task_id,
                    kind: meta.kind,
                    title: meta.title,
                    src_device: meta.src_device,
                    dst_device: meta.dst_device,
                    bytes_done: cur_done,
                    bytes_total: cur_tot,
                    speed: 0.0,
                    state: UiTaskState::Failed,
                    error: Some(map_core_error(&err)),
                });
            }
        }
    }

    pub(super) async fn poll_local_fs_task(
        self: Arc<Self>,
        ui_task_id: String,
        raw_task_id: String,
        meta: FsTaskMeta,
    ) {
        let mut rx = self.node.fs_service().subscribe_task_events();
        let started = Instant::now();
        let last_emit = Mutex::new(Instant::now() - PROGRESS_THROTTLE);

        loop {
            tokio::select! {
                _ = self.shutdown.cancelled() => break,
                _ = tokio::time::sleep(Duration::from_millis(100)) => {
                    match self.node.fs_service().task_status(&raw_task_id) {
                        Ok(p) => {
                            if emit_fs_task_progress(
                                &self.sink,
                                &last_emit,
                                &ui_task_id,
                                &meta,
                                &p,
                                started,
                            ) {
                                break;
                            }
                        }
                        Err(rpc) => {
                            self.sink.emit_task_progress(UiTaskProgress {
                                id: ui_task_id.clone(),
                                kind: meta.kind,
                                title: meta.title.clone(),
                                src_device: meta.device_id.clone(),
                                dst_device: meta.device_id.clone(),
                                bytes_done: meta.bytes_done.load(Ordering::Relaxed),
                                bytes_total: meta.bytes_total.load(Ordering::Relaxed).max(1),
                                speed: 0.0,
                                state: UiTaskState::Failed,
                                error: Some(map_rpc_error(&rpc)),
                            });
                            break;
                        }
                    }
                }
                ev = rx.recv() => {
                    match ev {
                        Ok((_peer, RemoteEvent::TaskProgress(p))) if p.task_id == raw_task_id => {
                            if emit_fs_task_progress(
                                &self.sink,
                                &last_emit,
                                &ui_task_id,
                                &meta,
                                &p,
                                started,
                            ) {
                                break;
                            }
                        }
                        Ok(_) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            match self.node.fs_service().task_status(&raw_task_id) {
                                Ok(p) => {
                                    if emit_fs_task_progress(
                                        &self.sink,
                                        &last_emit,
                                        &ui_task_id,
                                        &meta,
                                        &p,
                                        started,
                                    ) {
                                        break;
                                    }
                                }
                                Err(rpc) => {
                                    self.sink.emit_task_progress(UiTaskProgress {
                                        id: ui_task_id.clone(),
                                        kind: meta.kind,
                                        title: meta.title.clone(),
                                        src_device: meta.device_id.clone(),
                                        dst_device: meta.device_id.clone(),
                                        bytes_done: meta.bytes_done.load(Ordering::Relaxed),
                                        bytes_total: meta.bytes_total.load(Ordering::Relaxed).max(1),
                                        speed: 0.0,
                                        state: UiTaskState::Failed,
                                        error: Some(map_rpc_error(&rpc)),
                                    });
                                    break;
                                }
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            if let Ok(p) = self.node.fs_service().task_status(&raw_task_id) {
                                let _ = emit_fs_task_progress(
                                    &self.sink,
                                    &last_emit,
                                    &ui_task_id,
                                    &meta,
                                    &p,
                                    started,
                                );
                            }
                            break;
                        }
                    }
                }
            }
        }
        self.active_tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&ui_task_id);
    }

    pub(super) async fn poll_remote_fs_task(
        self: Arc<Self>,
        ui_task_id: String,
        peer_id: String,
        raw_task_id: String,
        meta: FsTaskMeta,
    ) {
        let started = Instant::now();
        let last_emit = Mutex::new(Instant::now() - PROGRESS_THROTTLE);

        loop {
            let status_res = tokio::select! {
                _ = self.shutdown.cancelled() => break,
                res = self.node.remote_task_status(&peer_id, &raw_task_id) => res,
            };
            match status_res {
                Ok(p) => {
                    if emit_fs_task_progress(
                        &self.sink,
                        &last_emit,
                        &ui_task_id,
                        &meta,
                        &p,
                        started,
                    ) {
                        break;
                    }
                }
                Err(err) => {
                    self.sink.emit_task_progress(UiTaskProgress {
                        id: ui_task_id.clone(),
                        kind: meta.kind,
                        title: meta.title.clone(),
                        src_device: meta.device_id.clone(),
                        dst_device: meta.device_id.clone(),
                        bytes_done: meta.bytes_done.load(Ordering::Relaxed),
                        bytes_total: meta.bytes_total.load(Ordering::Relaxed).max(1),
                        speed: 0.0,
                        state: UiTaskState::Failed,
                        error: Some(map_core_error(&err)),
                    });
                    break;
                }
            }
            tokio::select! {
                _ = self.shutdown.cancelled() => break,
                _ = tokio::time::sleep(PROGRESS_THROTTLE) => {}
            }
        }
        self.active_tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&ui_task_id);
    }
}
