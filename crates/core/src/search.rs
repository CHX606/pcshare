//! 任务 3.9：被控端多线程工作窃取并行文件搜索（边搜边回、支持中途取消与拼音首字母匹配）。

use std::collections::VecDeque;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use iroh::endpoint::SendStream;
use pcshare_proto::*;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::error::CoreError;
use crate::fs_service::{FsService, system_time_to_ms};
use crate::node::CoreNode;
use crate::path_guard::{normalize_lexical_absolute, resolve_authorized_path, to_display_path, to_fs_path};
use crate::pinyin::CompiledQuery;

const SEARCH_BATCH_SIZE: usize = 80;
const DEFAULT_MAX_RESULTS: usize = 500;
const PROGRESS_REPORT_INTERVAL: Duration = Duration::from_millis(250);

impl FsService {
    /// 处理 `fs.search` 请求流：在被控端多线程工作窃取遍历目录，每凑满一批或超时即推回一帧，客户端停流或取消即刻中止。
    pub async fn search_to_stream(
        &self,
        perms: &Permissions,
        args: FsSearchArgs,
        send: &mut SendStream,
    ) -> Result<bool, CoreError> {
        self.search_to_stream_opts(perms, args, send, false).await
    }

    pub async fn search_to_stream_opts(
        &self,
        perms: &Permissions,
        args: FsSearchArgs,
        send: &mut SendStream,
        compress: bool,
    ) -> Result<bool, CoreError> {
        if !perms.fs_read {
            write_frame(
                send,
                &ResponseEnvelope::<FsSearchBatch>::err(
                    ErrorCode::Forbidden,
                    "对方没有开放读取文件的权限",
                ),
            )
            .await?;
            return Ok(false);
        }

        let (root_clean, _root_fs) = match resolve_authorized_path(&args.path, perms) {
            Ok(p) => p,
            Err(rpc) => {
                write_frame(send, &ResponseEnvelope::<FsSearchBatch>::Err(rpc)).await?;
                return Ok(false);
            }
        };

        let query = args.query.trim().to_string();
        if query.is_empty() {
            write_frame(
                send,
                &ResponseEnvelope::ok(FsSearchBatch {
                    entries: Vec::new(),
                    scanned_dirs: 0,
                    done: true,
                }),
            )
            .await?;
            return Ok(true);
        }

        let max_results = args.max_results.unwrap_or(DEFAULT_MAX_RESULTS).clamp(1, 5000);
        let cancel = CancellationToken::new();
        let cancel_worker = cancel.clone();
        let (tx, mut rx) = mpsc::channel::<FsSearchBatch>(16);

        tokio::task::spawn_blocking(move || {
            run_search_traversal(root_clean, query, max_results, cancel_worker, tx);
        });

        loop {
            tokio::select! {
                _ = send.stopped() => {
                    cancel.cancel();
                    break;
                }
                batch_opt = rx.recv() => {
                    let Some(batch) = batch_opt else {
                        break;
                    };
                    let is_done = batch.done;
                    if write_frame_maybe_compressed(send, &ResponseEnvelope::ok(batch), compress)
                        .await
                        .is_err()
                    {
                        cancel.cancel();
                        break;
                    }
                    if is_done {
                        break;
                    }
                }
            }
        }
        cancel.cancel();
        Ok(true)
    }
}

struct SharedWorkQueue {
    dirs: VecDeque<PathBuf>,
    active_workers: usize,
    done: bool,
}

pub(crate) fn run_search_traversal(
    root_clean: PathBuf,
    query: String,
    max_results: usize,
    cancel: CancellationToken,
    tx: mpsc::Sender<FsSearchBatch>,
) {
    let compiled = CompiledQuery::new(&query);
    if compiled.is_empty() {
        let _ = tx.blocking_send(FsSearchBatch {
            entries: Vec::new(),
            scanned_dirs: 0,
            done: true,
        });
        return;
    }

    let worker_count = std::thread::available_parallelism()
        .map(|n| n.get().clamp(2, 8))
        .unwrap_or(4);

    let mut init_dirs = VecDeque::with_capacity(128);
    init_dirs.push_back(root_clean);

    let queue = Mutex::new(SharedWorkQueue {
        dirs: init_dirs,
        active_workers: 0,
        done: false,
    });
    let cv = Condvar::new();
    let scanned_dirs = AtomicU64::new(0);
    let last_reported_dirs = AtomicU64::new(0);
    let total_found = AtomicUsize::new(0);
    let last_progress_ms = AtomicU64::new(0);
    let epoch = Instant::now();

    std::thread::scope(|scope| {
        for _ in 0..worker_count {
            let queue_ref = &queue;
            let cv_ref = &cv;
            let compiled_ref = &compiled;
            let cancel_ref = &cancel;
            let scanned_ref = &scanned_dirs;
            let reported_ref = &last_reported_dirs;
            let found_ref = &total_found;
            let progress_ms_ref = &last_progress_ms;
            let tx_c = tx.clone();

            scope.spawn(move || {
                let mut local_stack: Vec<PathBuf> = Vec::with_capacity(32);
                let mut pending: Vec<DirEntry> = Vec::with_capacity(SEARCH_BATCH_SIZE);
                let mut local_last_flush = Instant::now();

                loop {
                    if cancel_ref.is_cancelled()
                        || found_ref.load(Ordering::Relaxed) >= max_results
                    {
                        signal_all_done(queue_ref, cv_ref);
                        break;
                    }

                    // 优先取线程本地小栈减少锁竞争；本地空时去共享双端队列窃取
                    let next_dir = if let Some(d) = local_stack.pop() {
                        d
                    } else {
                        let mut guard = queue_ref.lock().unwrap_or_else(|e| e.into_inner());
                        loop {
                            if guard.done
                                || cancel_ref.is_cancelled()
                                || found_ref.load(Ordering::Relaxed) >= max_results
                            {
                                guard.done = true;
                                cv_ref.notify_all();
                                drop(guard);
                                flush_worker_pending(
                                    &mut pending,
                                    scanned_ref,
                                    cancel_ref,
                                    &tx_c,
                                );
                                return;
                            }
                            if let Some(d) = guard.dirs.pop_front() {
                                guard.active_workers += 1;
                                break d;
                            }
                            if guard.active_workers == 0 {
                                guard.done = true;
                                cv_ref.notify_all();
                                drop(guard);
                                flush_worker_pending(
                                    &mut pending,
                                    scanned_ref,
                                    cancel_ref,
                                    &tx_c,
                                );
                                return;
                            }
                            // 等待其他工作线程产生新子目录前先把已积攒命中推给前端，保证边搜边回零积压
                            if !pending.is_empty() {
                                drop(guard);
                                if !flush_worker_pending(
                                    &mut pending,
                                    scanned_ref,
                                    cancel_ref,
                                    &tx_c,
                                ) {
                                    signal_all_done(queue_ref, cv_ref);
                                    return;
                                }
                                local_last_flush = Instant::now();
                                guard = queue_ref.lock().unwrap_or_else(|e| e.into_inner());
                                continue;
                            }
                            guard = cv_ref
                                .wait_timeout(guard, Duration::from_millis(60))
                                .unwrap_or_else(|e| e.into_inner())
                                .0;
                        }
                    };

                    let cur_scanned = scanned_ref.fetch_add(1, Ordering::Relaxed) + 1;
                    let fs_dir = to_fs_path(&next_dir);
                    if let Ok(rd) = fs::read_dir(&fs_dir) {
                        let disp_dir = to_display_path(&next_dir);
                        let mut prefix = String::with_capacity(disp_dir.len() + 1);
                        prefix.push_str(&disp_dir);
                        if !disp_dir.ends_with('\\') && !disp_dir.ends_with('/') {
                            prefix.push('\\');
                        }

                        let mut sub_dirs: Vec<PathBuf> = Vec::new();
                        for item in rd.flatten() {
                            if cancel_ref.is_cancelled()
                                || found_ref.load(Ordering::Relaxed) >= max_results
                            {
                                break;
                            }
                            let Ok(ft) = item.file_type() else {
                                continue;
                            };
                            let file_name_os = item.file_name();
                            let name_cow = file_name_os.to_string_lossy();

                            let is_dir = ft.is_dir();
                            let is_sym = ft.is_symlink();
                            if is_dir && !is_sym {
                                sub_dirs.push(next_dir.join(file_name_os.as_os_str()));
                            }

                            if compiled_ref.match_rank(&name_cow).is_some() {
                                // 原子预留配额，严格不超过 max_results
                                let prev = found_ref.fetch_add(1, Ordering::Relaxed);
                                if prev >= max_results {
                                    found_ref.store(max_results, Ordering::Relaxed);
                                    break;
                                }

                                let meta_opt = item.metadata().ok();
                                let kind = if is_sym {
                                    EntryKind::Symlink
                                } else if is_dir {
                                    EntryKind::Dir
                                } else {
                                    EntryKind::File
                                };
                                let name = name_cow.into_owned();
                                let mut full_path = String::with_capacity(prefix.len() + name.len());
                                full_path.push_str(&prefix);
                                full_path.push_str(&name);

                                pending.push(DirEntry {
                                    name,
                                    path: full_path,
                                    kind,
                                    size: meta_opt.as_ref().map_or(0, |m| {
                                        if kind == EntryKind::File { m.len() } else { 0 }
                                    }),
                                    modified_ms: meta_opt
                                        .as_ref()
                                        .and_then(|m| m.modified().ok())
                                        .and_then(system_time_to_ms),
                                    created_ms: meta_opt
                                        .as_ref()
                                        .and_then(|m| m.created().ok())
                                        .and_then(system_time_to_ms),
                                    readonly: meta_opt
                                        .as_ref()
                                        .is_some_and(|m| m.permissions().readonly()),
                                });

                                if pending.len() >= SEARCH_BATCH_SIZE
                                    || local_last_flush.elapsed() >= Duration::from_millis(80)
                                {
                                    if !flush_worker_pending(
                                        &mut pending,
                                        scanned_ref,
                                        cancel_ref,
                                        &tx_c,
                                    ) {
                                        complete_step_and_abort(queue_ref, cv_ref, &mut local_stack);
                                        return;
                                    }
                                    reported_ref.store(cur_scanned, Ordering::Relaxed);
                                    local_last_flush = Instant::now();
                                }
                            }
                        }

                        // 将新发现的子目录分流到共享队列以唤醒空闲线程并行扫描，自己只留至多 1 个直接下钻
                        if !sub_dirs.is_empty() {
                            if sub_dirs.len() == 1 && local_stack.len() < 8 {
                                // 保持当前工作线程为 active 状态，直接继续扫唯一子目录，省一次加解锁！
                                local_stack.push(sub_dirs.pop().unwrap());
                            } else {
                                let keep_one = if local_stack.is_empty() {
                                    sub_dirs.pop()
                                } else {
                                    None
                                };
                                {
                                    let mut guard =
                                        queue_ref.lock().unwrap_or_else(|e| e.into_inner());
                                    let pushed = sub_dirs.len();
                                    guard.dirs.extend(sub_dirs);
                                    if keep_one.is_none() {
                                        guard.active_workers =
                                            guard.active_workers.saturating_sub(1);
                                    }
                                    if pushed == 1 {
                                        cv_ref.notify_one();
                                    } else if pushed > 1 {
                                        cv_ref.notify_all();
                                    }
                                }
                                if let Some(d) = keep_one {
                                    local_stack.push(d);
                                }
                            }
                        } else if local_stack.is_empty() {
                            let mut guard = queue_ref.lock().unwrap_or_else(|e| e.into_inner());
                            guard.active_workers = guard.active_workers.saturating_sub(1);
                            if guard.dirs.is_empty() && guard.active_workers == 0 {
                                guard.done = true;
                                cv_ref.notify_all();
                            }
                        }
                    } else if local_stack.is_empty() {
                        let mut guard = queue_ref.lock().unwrap_or_else(|e| e.into_inner());
                        guard.active_workers = guard.active_workers.saturating_sub(1);
                        if guard.dirs.is_empty() && guard.active_workers == 0 {
                            guard.done = true;
                            cv_ref.notify_all();
                        }
                    }

                    // 无命中期间定期回报全局扫描目录进度
                    let now_ms = epoch.elapsed().as_millis() as u64;
                    let prev_ms = progress_ms_ref.load(Ordering::Relaxed);
                    if now_ms.saturating_sub(prev_ms) >= PROGRESS_REPORT_INTERVAL.as_millis() as u64
                        && (!pending.is_empty()
                            || cur_scanned > reported_ref.load(Ordering::Relaxed))
                        && progress_ms_ref
                            .compare_exchange(
                                prev_ms,
                                now_ms,
                                Ordering::Relaxed,
                                Ordering::Relaxed,
                            )
                            .is_ok()
                    {
                        let batch = FsSearchBatch {
                            entries: std::mem::take(&mut pending),
                            scanned_dirs: scanned_ref.load(Ordering::Relaxed),
                            done: false,
                        };
                        if tx_c.blocking_send(batch).is_err() {
                            complete_step_and_abort(queue_ref, cv_ref, &mut local_stack);
                            return;
                        }
                        reported_ref.store(cur_scanned, Ordering::Relaxed);
                        local_last_flush = Instant::now();
                    }
                }

                complete_step_and_abort(queue_ref, cv_ref, &mut local_stack);
                flush_worker_pending(&mut pending, scanned_ref, cancel_ref, &tx_c);
            });
        }
    });

    if !cancel.is_cancelled() {
        let _ = tx.blocking_send(FsSearchBatch {
            entries: Vec::new(),
            scanned_dirs: scanned_dirs.load(Ordering::Relaxed),
            done: true,
        });
    }
}

fn flush_worker_pending(
    pending: &mut Vec<DirEntry>,
    scanned_ref: &AtomicU64,
    cancel_ref: &CancellationToken,
    tx: &mpsc::Sender<FsSearchBatch>,
) -> bool {
    if pending.is_empty() || cancel_ref.is_cancelled() {
        pending.clear();
        return !cancel_ref.is_cancelled();
    }
    let batch = FsSearchBatch {
        entries: std::mem::take(pending),
        scanned_dirs: scanned_ref.load(Ordering::Relaxed),
        done: false,
    };
    tx.blocking_send(batch).is_ok()
}

fn signal_all_done(queue: &Mutex<SharedWorkQueue>, cv: &Condvar) {
    let mut guard = queue.lock().unwrap_or_else(|e| e.into_inner());
    guard.done = true;
    cv.notify_all();
}

fn complete_step_and_abort(
    queue: &Mutex<SharedWorkQueue>,
    cv: &Condvar,
    local_stack: &mut Vec<PathBuf>,
) {
    local_stack.clear();
    let mut guard = queue.lock().unwrap_or_else(|e| e.into_inner());
    guard.done = true;
    guard.active_workers = guard.active_workers.saturating_sub(1);
    cv.notify_all();
}

impl CoreNode {
    pub async fn remote_search_files<F>(
        self: &Arc<Self>,
        peer_id: &str,
        path: &str,
        query: &str,
        max_results: Option<usize>,
        cancel: CancellationToken,
        mut on_batch: F,
    ) -> Result<u64, CoreError>
    where
        F: FnMut(FsSearchBatch),
    {
        let conn = self.get_or_connect(peer_id).await?;
        let (mut send, mut recv) = conn
            .open_bi()
            .await
            .map_err(|e| CoreError::NotConnected(format!("打开搜索通道失败：{e}")))?;

        let cmd = Command::FsSearch(FsSearchArgs {
            path: path.to_string(),
            query: query.to_string(),
            max_results,
        });
        write_frame(&mut send, &cmd.to_envelope()?).await?;
        send.finish()
            .map_err(|e| CoreError::Internal(format!("结束搜索请求流失败：{e}")))?;

        loop {
            if cancel.is_cancelled() {
                let _ = recv.stop(1u32.into());
                return Err(CoreError::Cancelled);
            }
            let next = tokio::select! {
                _ = cancel.cancelled() => {
                    let _ = recv.stop(1u32.into());
                    return Err(CoreError::Cancelled);
                }
                f = read_frame::<_, ResponseEnvelope<FsSearchBatch>>(&mut recv) => f,
            };
            let batch = next?.into_result()?;
            let scanned = batch.scanned_dirs;
            let done = batch.done;
            on_batch(batch);
            if done {
                return Ok(scanned);
            }
        }
    }

    pub async fn local_search_files<F>(
        &self,
        path: &str,
        query: &str,
        max_results: Option<usize>,
        cancel: CancellationToken,
        mut on_batch: F,
    ) -> Result<u64, CoreError>
    where
        F: FnMut(FsSearchBatch),
    {
        let root_clean = normalize_lexical_absolute(path).map_err(CoreError::Rpc)?;
        let q = query.trim().to_string();
        if q.is_empty() {
            on_batch(FsSearchBatch {
                entries: Vec::new(),
                scanned_dirs: 0,
                done: true,
            });
            return Ok(0);
        }
        let max_res = max_results.unwrap_or(DEFAULT_MAX_RESULTS).clamp(1, 5000);
        let cancel_worker = cancel.clone();
        let (tx, mut rx) = mpsc::channel::<FsSearchBatch>(16);
        tokio::task::spawn_blocking(move || {
            run_search_traversal(root_clean, q, max_res, cancel_worker, tx);
        });

        let mut scanned = 0u64;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return Err(CoreError::Cancelled),
                b = rx.recv() => {
                    let Some(batch) = b else { break; };
                    scanned = batch.scanned_dirs;
                    let done = batch.done;
                    on_batch(batch);
                    if done {
                        break;
                    }
                }
            }
        }
        Ok(scanned)
    }
}
