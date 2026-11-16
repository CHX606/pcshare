//! 文件搜索服务（任务 3.9：被控端遍历、边搜边回、可中途取消）。

use std::collections::VecDeque;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use iroh::endpoint::SendStream;
use pcshare_proto::{
    Command, DirEntry, EntryKind, ErrorCode, FsSearchArgs, FsSearchBatch, Permissions,
    ResponseEnvelope, read_frame, write_frame,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::CoreNode;
use crate::error::CoreError;
use crate::fs_service::{FsService, system_time_to_ms};
use crate::path_guard::{normalize_lexical_absolute, resolve_authorized_path, to_display_path, to_fs_path};
use crate::pinyin::match_rank;

const SEARCH_BATCH_SIZE: usize = 32;
const DEFAULT_MAX_RESULTS: usize = 500;

impl FsService {
    pub async fn search_to_stream(
        &self,
        perms: &Permissions,
        args: FsSearchArgs,
        send: &mut SendStream,
    ) -> Result<(), CoreError> {
        if !perms.fs_read {
            write_frame(
                send,
                &ResponseEnvelope::<FsSearchBatch>::err(
                    ErrorCode::Forbidden,
                    "对方没有开放读取文件的权限",
                ),
            )
            .await?;
            return Ok(());
        }

        let (root_clean, _root_fs) = match resolve_authorized_path(&args.path, perms) {
            Ok(p) => p,
            Err(rpc_err) => {
                write_frame(send, &ResponseEnvelope::<FsSearchBatch>::Err(rpc_err)).await?;
                return Ok(());
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
            return Ok(());
        }

        let max_results = args.max_results.unwrap_or(DEFAULT_MAX_RESULTS).clamp(1, 5000);
        let cancel = CancellationToken::new();
        let cancel_worker = cancel.clone();
        let (tx, mut rx) = mpsc::channel::<FsSearchBatch>(8);

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
                    if write_frame(send, &ResponseEnvelope::ok(batch)).await.is_err() {
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
        Ok(())
    }
}

pub(crate) fn run_search_traversal(
    root_clean: PathBuf,
    query: String,
    max_results: usize,
    cancel: CancellationToken,
    tx: mpsc::Sender<FsSearchBatch>,
) {
    let mut queue = VecDeque::new();
    queue.push_back(root_clean);
    let mut scanned_dirs = 0u64;
    let mut total_found = 0usize;
    let mut pending = Vec::with_capacity(SEARCH_BATCH_SIZE);
    let mut last_flush = Instant::now();

    while let Some(dir_clean) = queue.pop_front() {
        if cancel.is_cancelled() || total_found >= max_results {
            break;
        }
        scanned_dirs += 1;
        let Ok(rd) = fs::read_dir(to_fs_path(&dir_clean)) else {
            continue;
        };

        for item in rd.flatten() {
            if cancel.is_cancelled() || total_found >= max_results {
                break;
            }
            let name = item.file_name().to_string_lossy().into_owned();
            let Ok(ft) = item.file_type() else {
                continue;
            };
            let entry_clean = dir_clean.join(&name);
            if ft.is_dir() && !ft.is_symlink() {
                queue.push_back(entry_clean.clone());
            }

            if match_rank(&name, &query).is_some() {
                let meta_opt = item.metadata().ok();
                let kind = if ft.is_symlink() {
                    EntryKind::Symlink
                } else if ft.is_dir() {
                    EntryKind::Dir
                } else {
                    EntryKind::File
                };
                pending.push(DirEntry {
                    name,
                    path: to_display_path(&entry_clean),
                    kind,
                    size: meta_opt.as_ref().map_or(0, |m| if kind == EntryKind::File { m.len() } else { 0 }),
                    modified_ms: meta_opt.as_ref().and_then(|m| m.modified().ok()).and_then(system_time_to_ms),
                    created_ms: meta_opt.as_ref().and_then(|m| m.created().ok()).and_then(system_time_to_ms),
                    readonly: meta_opt.as_ref().is_some_and(|m| m.permissions().readonly()),
                });
                total_found += 1;

                if pending.len() >= SEARCH_BATCH_SIZE
                    || last_flush.elapsed() >= Duration::from_millis(80)
                {
                    let batch = FsSearchBatch {
                        entries: std::mem::take(&mut pending),
                        scanned_dirs,
                        done: false,
                    };
                    if tx.blocking_send(batch).is_err() {
                        return;
                    }
                    last_flush = Instant::now();
                }
            }
        }
    }

    if !cancel.is_cancelled() {
        let _ = tx.blocking_send(FsSearchBatch {
            entries: pending,
            scanned_dirs,
            done: true,
        });
    }
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
        let (tx, mut rx) = mpsc::channel::<FsSearchBatch>(8);
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
