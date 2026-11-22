use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) fn run_copy_or_move_blocking(
    peer_id: String,
    task_id: String,
    kind: FsTaskKind,
    sources: Vec<(PathBuf, PathBuf)>,
    dest_dir_clean: PathBuf,
    conflict: ConflictPolicy,
    cancel: CancellationToken,
    tasks: Arc<Mutex<HashMap<String, ManagedTask>>>,
    events_tx: broadcast::Sender<(String, RemoteEvent)>,
) {
    let emit = |state: FsTaskState,
                processed_bytes: u64,
                total_bytes: u64,
                processed_items: usize,
                total_items: usize,
                current_item: Option<String>,
                error: Option<RpcError>| {
        let snap = FsTaskProgress {
            task_id: task_id.clone(),
            kind,
            state,
            processed_bytes,
            total_bytes,
            processed_items,
            total_items,
            current_item,
            error,
        };
        if let Ok(mut guard) = tasks.lock()
            && let Some(t) = guard.get_mut(&task_id)
        {
            t.progress = snap.clone();
            t.updated_at = Instant::now();
        }
        let _ = events_tx.send((peer_id.clone(), RemoteEvent::TaskProgress(snap)));
    };

    // 预估总字节数
    let mut total_bytes = 0u64;
    for (_, src_fs) in &sources {
        total_bytes = total_bytes.saturating_add(estimate_size(src_fs));
    }
    let total_items = sources.len();
    let mut processed_bytes = 0u64;

    for (idx, (src_clean, src_fs)) in sources.iter().enumerate() {
        if cancel.is_cancelled() {
            emit(
                FsTaskState::Cancelled,
                processed_bytes,
                total_bytes,
                idx,
                total_items,
                None,
                None,
            );
            return;
        }

        let Some(name) = src_clean.file_name() else {
            continue;
        };
        let initial_target_clean = dest_dir_clean.join(name);
        let initial_target_fs = to_fs_path(&initial_target_clean);
        let (target_clean, target_fs) = if initial_target_fs.exists() {
            match conflict {
                ConflictPolicy::Fail => {
                    let err = RpcError::new(
                        ErrorCode::AlreadyExists,
                        format!("目标已存在：{}", to_display_path(&initial_target_clean)),
                    );
                    emit(
                        FsTaskState::Failed,
                        processed_bytes,
                        total_bytes,
                        idx,
                        total_items,
                        None,
                        Some(err),
                    );
                    return;
                }
                ConflictPolicy::Skip => {
                    continue;
                }
                ConflictPolicy::Overwrite => (initial_target_clean, initial_target_fs),
                ConflictPolicy::RenameKeepBoth => pick_unique_target(&initial_target_clean),
            }
        } else {
            (initial_target_clean, initial_target_fs)
        };

        // 拦截同路径覆盖（防止覆盖移动先删自身或覆盖复制截断为 0 字节）以及将目录复制/移动到自身子目录
        if is_same_path(src_clean, src_fs, &target_clean, &target_fs) {
            let err = RpcError::new(
                ErrorCode::Invalid,
                format!("源路径与目标路径相同，无法覆盖自身：{}", to_display_path(src_clean)),
            );
            emit(
                FsTaskState::Failed,
                processed_bytes,
                total_bytes,
                idx,
                total_items,
                None,
                Some(err),
            );
            return;
        }
        let src_is_real_dir = fs::symlink_metadata(src_fs)
            .map(|m| m.is_dir() && !is_symlink_or_junction(src_fs, &m))
            .unwrap_or(false);
        if src_is_real_dir && is_same_or_descendant(&target_clean, &target_fs, src_clean, src_fs) {
            let err = RpcError::new(
                ErrorCode::Invalid,
                format!(
                    "不能将文件夹“{}”复制或移动到自身或其子文件夹中",
                    to_display_path(src_clean)
                ),
            );
            emit(
                FsTaskState::Failed,
                processed_bytes,
                total_bytes,
                idx,
                total_items,
                None,
                Some(err),
            );
            return;
        }

        let cur_disp = Some(to_display_path(src_clean));
        emit(
            FsTaskState::Running,
            processed_bytes,
            total_bytes,
            idx,
            total_items,
            cur_disp.clone(),
            None,
        );

        let step_res = match kind {
            FsTaskKind::Move => {
                // 同盘符优先直接 rename；不先盲删已有目标，避免跨盘符或失败时双丢数据
                let mut backup_target: Option<PathBuf> = None;
                let direct_rename = match fs::rename(src_fs, &target_fs) {
                    Ok(()) => Ok(()),
                    Err(first_err) if target_fs.exists() && conflict == ConflictPolicy::Overwrite => {
                        // 将已有目标先移至同目录临时备份名，失败可恢复
                        let bak_name = format!(
                            ".pcshare.movebak.{}.tmp",
                            fastrand::u64(..)
                        );
                        let bak_clean = target_clean
                            .parent()
                            .unwrap_or_else(|| Path::new(""))
                            .join(&bak_name);
                        let bak_fs = to_fs_path(&bak_clean);
                        if fs::rename(&target_fs, &bak_fs).is_ok() {
                            backup_target = Some(bak_fs);
                            fs::rename(src_fs, &target_fs)
                        } else {
                            Err(first_err)
                        }
                    }
                    Err(err) => Err(err),
                };

                match direct_rename {
                    Ok(()) => {
                        if let Some(bak) = backup_target {
                            let _ = remove_path_permanent(&bak);
                        }
                        processed_bytes = processed_bytes.saturating_add(estimate_size(&target_fs));
                        Ok(())
                    }
                    Err(_) => {
                        // 跨盘符移动：退化为递归复制，成功后再删除源和旧备份；失败时还原备份
                        let res = copy_recursive(
                            src_fs,
                            &target_clean,
                            &cancel,
                            &mut processed_bytes,
                            &|pb| {
                                emit(
                                    FsTaskState::Running,
                                    pb,
                                    total_bytes,
                                    idx,
                                    total_items,
                                    cur_disp.clone(),
                                    None,
                                );
                            },
                        );
                        if res.is_ok() {
                            if let Some(bak) = backup_target {
                                let _ = remove_path_permanent(&bak);
                            }
                            let _ = remove_path_permanent(src_fs);
                        } else if let Some(bak) = backup_target {
                            let _ = remove_path_permanent(&target_fs);
                            let _ = fs::rename(&bak, &target_fs);
                        }
                        res
                    }
                }
            }
            FsTaskKind::Copy => copy_recursive(
                src_fs,
                &target_clean,
                &cancel,
                &mut processed_bytes,
                &|pb| {
                    emit(
                        FsTaskState::Running,
                        pb,
                        total_bytes,
                        idx,
                        total_items,
                        cur_disp.clone(),
                        None,
                    );
                },
            ),
        };

        if let Err(rpc_err) = step_res {
            let final_state = if rpc_err.code == ErrorCode::Cancelled {
                FsTaskState::Cancelled
            } else {
                FsTaskState::Failed
            };
            emit(
                final_state,
                processed_bytes,
                total_bytes,
                idx,
                total_items,
                None,
                if final_state == FsTaskState::Cancelled {
                    None
                } else {
                    Some(rpc_err)
                },
            );
            return;
        }
    }

    emit(
        FsTaskState::Completed,
        processed_bytes,
        total_bytes.max(processed_bytes),
        total_items,
        total_items,
        None,
        None,
    );
}

pub(super) fn estimate_size(fs_path: &Path) -> u64 {
    let Ok(meta) = fs::symlink_metadata(fs_path) else {
        return 0;
    };
    if is_symlink_or_junction(fs_path, &meta) {
        return 0;
    }
    if meta.is_file() {
        return meta.len();
    }
    if meta.is_dir() {
        let mut sum = 0u64;
        if let Ok(rd) = fs::read_dir(fs_path) {
            for e in rd.flatten() {
                sum = sum.saturating_add(estimate_size(&e.path()));
            }
        }
        return sum;
    }
    0
}

pub(super) fn copy_single_file_fast(
    src_fs: &Path,
    dst_fs: &Path,
    disp: &str,
    meta: &fs::Metadata,
    cancel: &CancellationToken,
    shared_buf: &mut Vec<u8>,
    mut on_chunk: impl FnMut(u64),
) -> Result<(), RpcError> {
    const KERNEL_COPY_MAX_BYTES: u64 = 8 * 1024 * 1024;
    if cancel.is_cancelled() {
        return Err(RpcError::new(ErrorCode::Cancelled, "任务已取消"));
    }
    if meta.len() <= KERNEL_COPY_MAX_BYTES {
        let copied = fs::copy(src_fs, dst_fs).map_err(|e| map_io_to_rpc(&e, Some(disp)))?;
        on_chunk(copied);
        return Ok(());
    }

    use std::io::{Read, Write};
    if shared_buf.len() < TRANSFER_CHUNK_SIZE {
        shared_buf.resize(TRANSFER_CHUNK_SIZE, 0);
    }
    let mut reader = open_sequential_read(src_fs).map_err(|e| map_io_to_rpc(&e, Some(disp)))?;
    let mut writer = create_sequential_write(dst_fs).map_err(|e| map_io_to_rpc(&e, Some(disp)))?;
    loop {
        if cancel.is_cancelled() {
            drop(writer);
            let _ = fs::remove_file(dst_fs);
            return Err(RpcError::new(ErrorCode::Cancelled, "任务已取消"));
        }
        let n = reader
            .read(shared_buf)
            .map_err(|e| map_io_to_rpc(&e, Some(disp)))?;
        if n == 0 {
            break;
        }
        writer
            .write_all(&shared_buf[..n])
            .map_err(|e| map_io_to_rpc(&e, Some(disp)))?;
        on_chunk(n as u64);
    }
    if let Ok(mtime) = meta.modified() {
        let _ = writer.set_modified(mtime);
    }
    Ok(())
}

pub(super) fn copy_recursive(
    src_fs: &Path,
    dst_clean: &Path,
    cancel: &CancellationToken,
    processed_bytes: &mut u64,
    on_progress: &(dyn Fn(u64) + Sync),
) -> Result<(), RpcError> {
    if cancel.is_cancelled() {
        return Err(RpcError::new(ErrorCode::Cancelled, "任务已取消"));
    }
    let disp = to_display_path(dst_clean);
    let meta = fs::symlink_metadata(src_fs).map_err(|e| map_io_to_rpc(&e, Some(&disp)))?;
    // 显式跳过符号链接与 Windows 联接点，防止越出开放目录复制或遇目录软链报错
    if is_symlink_or_junction(src_fs, &meta) {
        return Ok(());
    }
    let dst_fs = to_fs_path(dst_clean);

    if !meta.is_dir() {
        let mut single_buf = Vec::new();
        let mut last_emit = Instant::now();
        return copy_single_file_fast(
            src_fs,
            &dst_fs,
            &disp,
            &meta,
            cancel,
            &mut single_buf,
            |delta| {
                *processed_bytes = processed_bytes.saturating_add(delta);
                if last_emit.elapsed() >= Duration::from_millis(100) {
                    on_progress(*processed_bytes);
                    last_emit = Instant::now();
                }
            },
        );
    }

    // 目录递归复制：主线程遍历目录骨架，4 个工作线程并行调用内核拷贝 / 复用单缓冲流式拷贝
    struct FileCopyJob {
        src_fs: PathBuf,
        dst_fs: PathBuf,
        disp: String,
        meta: fs::Metadata,
    }

    let base_bytes = *processed_bytes;
    let added_bytes = std::sync::atomic::AtomicU64::new(0);
    let last_emit_ms = std::sync::atomic::AtomicU64::new(0);
    let started = Instant::now();
    let abort = std::sync::atomic::AtomicBool::new(false);
    let first_err: Mutex<Option<RpcError>> = Mutex::new(None);
    let (job_tx, job_rx) = std::sync::mpsc::sync_channel::<FileCopyJob>(512);
    let shared_rx = Mutex::new(job_rx);

    let set_err = |slot: &Mutex<Option<RpcError>>, flag: &std::sync::atomic::AtomicBool, e: RpcError| {
        flag.store(true, std::sync::atomic::Ordering::Release);
        let mut g = slot.lock().unwrap_or_else(|p| p.into_inner());
        if g.is_none() {
            *g = Some(e);
        }
    };

    std::thread::scope(|s| {
        for _ in 0..4 {
            let rx_ref = &shared_rx;
            let added_ref = &added_bytes;
            let emit_ref = &last_emit_ms;
            let abort_ref = &abort;
            let err_ref = &first_err;
            s.spawn(move || {
                let mut local_buf = Vec::new();
                loop {
                    if abort_ref.load(std::sync::atomic::Ordering::Acquire) {
                        break;
                    }
                    if cancel.is_cancelled() {
                        set_err(
                            err_ref,
                            abort_ref,
                            RpcError::new(ErrorCode::Cancelled, "任务已取消"),
                        );
                        break;
                    }
                    let job = {
                        let g = rx_ref.lock().unwrap_or_else(|p| p.into_inner());
                        g.recv()
                    };
                    let Ok(job) = job else {
                        break;
                    };
                    let res = copy_single_file_fast(
                        &job.src_fs,
                        &job.dst_fs,
                        &job.disp,
                        &job.meta,
                        cancel,
                        &mut local_buf,
                        |delta| {
                            let cur = base_bytes.saturating_add(
                                added_ref
                                    .fetch_add(delta, std::sync::atomic::Ordering::Relaxed)
                                    .saturating_add(delta),
                            );
                            let now_ms = started.elapsed().as_millis() as u64;
                            let prev_ms = emit_ref.load(std::sync::atomic::Ordering::Relaxed);
                            if now_ms.saturating_sub(prev_ms) >= 100
                                && emit_ref
                                    .compare_exchange(
                                        prev_ms,
                                        now_ms,
                                        std::sync::atomic::Ordering::Relaxed,
                                        std::sync::atomic::Ordering::Relaxed,
                                    )
                                    .is_ok()
                            {
                                on_progress(cur);
                            }
                        },
                    );
                    if let Err(e) = res {
                        set_err(err_ref, abort_ref, e);
                        break;
                    }
                }
            });
        }

        let mut stack = vec![(src_fs.to_path_buf(), dst_clean.to_path_buf())];
        while let Some((cur_src, cur_dst_clean)) = stack.pop() {
            if abort.load(std::sync::atomic::Ordering::Acquire) {
                break;
            }
            if cancel.is_cancelled() {
                set_err(
                    &first_err,
                    &abort,
                    RpcError::new(ErrorCode::Cancelled, "任务已取消"),
                );
                break;
            }
            let cur_disp = to_display_path(&cur_dst_clean);
            let cur_dst_fs = to_fs_path(&cur_dst_clean);
            if let Err(e) = fs::create_dir_all(&cur_dst_fs) {
                set_err(&first_err, &abort, map_io_to_rpc(&e, Some(&cur_disp)));
                break;
            }
            let rd = match fs::read_dir(&cur_src) {
                Ok(r) => r,
                Err(e) => {
                    set_err(&first_err, &abort, map_io_to_rpc(&e, Some(&cur_disp)));
                    break;
                }
            };
            for entry_res in rd {
                if abort.load(std::sync::atomic::Ordering::Acquire) {
                    break;
                }
                let entry = match entry_res {
                    Ok(e) => e,
                    Err(e) => {
                        set_err(&first_err, &abort, map_io_to_rpc(&e, Some(&cur_disp)));
                        break;
                    }
                };
                let child_src = entry.path();
                let child_meta = match fs::symlink_metadata(&child_src) {
                    Ok(m) => m,
                    Err(e) => {
                        set_err(&first_err, &abort, map_io_to_rpc(&e, Some(&cur_disp)));
                        break;
                    }
                };
                if is_symlink_or_junction(&child_src, &child_meta) {
                    continue;
                }
                let child_clean = cur_dst_clean.join(entry.file_name());
                if child_meta.is_dir() {
                    stack.push((child_src, child_clean));
                } else {
                    let job = FileCopyJob {
                        src_fs: child_src,
                        dst_fs: to_fs_path(&child_clean),
                        disp: to_display_path(&child_clean),
                        meta: child_meta,
                    };
                    if job_tx.send(job).is_err() {
                        break;
                    }
                }
            }
        }
        drop(job_tx);
    });

    *processed_bytes = base_bytes.saturating_add(
        added_bytes.load(std::sync::atomic::Ordering::Relaxed),
    );
    match first_err.into_inner().unwrap_or_else(|e| e.into_inner()) {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// 判断目标是否为符号链接或 Windows 联接点（排除 OneDrive 等云占位重解析点）。
pub(super) fn is_symlink_or_junction(path: &Path, meta: &fs::Metadata) -> bool {
    meta.file_type().is_symlink() || (is_windows_reparse_point(meta) && fs::read_link(path).is_ok())
}

pub(super) fn is_windows_reparse_point(meta: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        (meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) != 0
    }
    #[cfg(not(windows))]
    {
        let _ = meta;
        false
    }
}

pub(super) fn remove_path_permanent(fs_p: &Path) -> std::io::Result<()> {
    let meta = fs::symlink_metadata(fs_p)?;
    if is_symlink_or_junction(fs_p, &meta) {
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
            if (meta.file_attributes() & FILE_ATTRIBUTE_DIRECTORY) != 0 {
                return fs::remove_dir(fs_p);
            }
        }
        return fs::remove_file(fs_p);
    }
    if meta.is_dir() {
        fs::remove_dir_all(fs_p)
    } else {
        fs::remove_file(fs_p)
    }
}

pub(super) fn is_same_path(a_clean: &Path, a_fs: &Path, b_clean: &Path, b_fs: &Path) -> bool {
    if paths_equal(a_clean, b_clean) {
        return true;
    }
    if let (Ok(ca), Ok(cb)) = (fs::canonicalize(a_fs), fs::canonicalize(b_fs)) {
        let ca_clean = PathBuf::from(to_display_path(&ca));
        let cb_clean = PathBuf::from(to_display_path(&cb));
        return paths_equal(&ca_clean, &cb_clean);
    }
    false
}

pub(super) fn is_same_or_descendant(
    cand_clean: &Path,
    cand_fs: &Path,
    root_clean: &Path,
    root_fs: &Path,
) -> bool {
    if is_subpath_of(cand_clean, root_clean) {
        return true;
    }
    if let (Ok(cc), Ok(rc)) = (fs::canonicalize(cand_fs), fs::canonicalize(root_fs)) {
        let cc_clean = PathBuf::from(to_display_path(&cc));
        let rc_clean = PathBuf::from(to_display_path(&rc));
        return is_subpath_of(&cc_clean, &rc_clean);
    }
    false
}

pub(super) fn is_explicit_cancel_error(io_err: &std::io::Error) -> bool {
    if let Some(read_err) = io_err
        .get_ref()
        .and_then(|e| e.downcast_ref::<iroh::endpoint::ReadError>())
    {
        return matches!(
            read_err,
            iroh::endpoint::ReadError::Reset(code)
                if code.into_inner() == u64::from(STREAM_STOP_CODE_CANCEL)
        );
    }
    false
}

pub(super) fn prune_finished_tasks(tasks: &mut HashMap<String, ManagedTask>) {
    tasks.retain(|_, t| {
        t.progress.state == FsTaskState::Running || t.updated_at.elapsed() < FINISHED_TASK_TTL
    });
    while tasks.len() >= MAX_TRACKED_TASKS {
        let oldest_finished = tasks
            .iter()
            .filter(|(_, t)| t.progress.state != FsTaskState::Running)
            .min_by_key(|(_, t)| t.updated_at)
            .map(|(k, _)| k.clone());
        match oldest_finished {
            Some(k) => {
                tasks.remove(&k);
            }
            None => break,
        }
    }
}
