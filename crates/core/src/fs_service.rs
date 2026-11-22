//! 被控端文件操作与流式读写服务（任务 1.7、1.8）。

use std::collections::HashMap;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use pcshare_platform::{Platform, SpecialFolderKind};
use pcshare_proto::{
    BatchFileItem, BatchReadItemHeader, ConflictPolicy, DEFAULT_LIST_BATCH_SIZE, DirEntry,
    EntryKind, ErrorCode, FsCopyMoveArgs, FsDeleteArgs, FsDeleteReply,
    FsHashArgs, FsHashReply, FsListArgs, FsListBatch, FsMkdirArgs, FsMkdirReply, FsProbeResumeArgs,
    FsProbeResumeReply, FsReadArgs, FsReadBatchArgs, FsReadHeader, FsRenameArgs, FsRenameReply,
    FsRootsReply, FsStatArgs, FsStatReply, FsTaskKind, FsTaskProgress, FsTaskReply, FsTaskState,
    FsWalkArgs, FsWalkBatch, FsWriteArgs, FsWriteBatchArgs, FsWriteBatchReply, FsWriteDone,
    FsWriteReady, Permissions, RemoteEvent, ResponseEnvelope, RootEntry, RootKind, RpcError,
    TRANSFER_CHUNK_SIZE, WalkEntry, read_frame, write_frame, write_frame_maybe_compressed,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::error::{map_io_to_rpc, map_platform_to_rpc};
use crate::path_guard::{
    is_subpath_of, normalize_lexical_absolute, paths_equal, resolve_authorized_path,
    to_display_path, to_fs_path, validate_single_name,
};

/// 传输流被暂停时对端发送的 QUIC reset 错误码（保留 `.pcshare.part` 供断点续传）。
pub const STREAM_STOP_CODE_PAUSE: u32 = 498;
/// 传输流被主动取消时对端发送的 QUIC reset 错误码（删除 `.pcshare.part`）。
pub const STREAM_STOP_CODE_CANCEL: u32 = 499;
/// 单文件批量读写通道允许的单个文件最大字节数（4 MB），超限应改走分块流式通道，防止整文件读入内存 OOM。
pub const MAX_BATCH_FILE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_TRACKED_TASKS: usize = 256;
const FINISHED_TASK_TTL: Duration = Duration::from_secs(30 * 60);

struct ManagedTask {
    progress: FsTaskProgress,
    cancel: CancellationToken,
    updated_at: Instant,
}


struct StripedPartState {
    active_writers: usize,
    base_offset: u64,
    segments: Vec<(u64, u64)>,
    explicit_cancel: bool,
    had_error: bool,
}

type StripedStateMap = Arc<Mutex<HashMap<PathBuf, StripedPartState>>>;


pub struct FsService {
    platform: Arc<dyn Platform>,
    tasks: Arc<Mutex<HashMap<String, ManagedTask>>>,
    striped_states: StripedStateMap,
    remote_events_tx: broadcast::Sender<(String, RemoteEvent)>,
}

impl FsService {
    pub fn new(
        platform: Arc<dyn Platform>,
        remote_events_tx: broadcast::Sender<(String, RemoteEvent)>,
    ) -> Self {
        Self {
            platform,
            tasks: Arc::new(Mutex::new(HashMap::new())),
            striped_states: Arc::new(Mutex::new(HashMap::new())),
            remote_events_tx,
        }
    }

    pub fn roots(&self, perms: &Permissions) -> Result<FsRootsReply, RpcError> {
        if !perms.fs_read {
            return Err(RpcError::new(ErrorCode::Forbidden, "对方没有开放读取文件的权限"));
        }

        // 如果设置了只开放特定目录，直接返回这些受限目录作为可访问入口
        if !perms.allowed_roots.is_empty() {
            let mut special_folders = Vec::new();
            for r in &perms.allowed_roots {
                if let Ok(clean) = normalize_lexical_absolute(r) {
                    let name = clean
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| to_display_path(&clean));
                    special_folders.push(RootEntry {
                        name,
                        path: to_display_path(&clean),
                        kind: RootKind::AllowedFolder,
                        total_bytes: None,
                        available_bytes: None,
                    });
                }
            }
            return Ok(FsRootsReply {
                drives: Vec::new(),
                special_folders,
            });
        }

        let drives = self
            .platform
            .list_drives()
            .map_err(|e| map_platform_to_rpc(&e))?
            .into_iter()
            .map(|d| RootEntry {
                name: if d.label.is_empty() {
                    d.path.clone()
                } else {
                    format!("{} ({})", d.label, d.path.trim_end_matches('\\'))
                },
                path: d.path,
                kind: RootKind::Drive,
                total_bytes: Some(d.total_bytes),
                available_bytes: Some(d.available_bytes),
            })
            .collect();

        let special_folders = self
            .platform
            .special_folders()
            .unwrap_or_default()
            .into_iter()
            .map(|f| RootEntry {
                name: f.name,
                path: to_display_path(&f.path),
                kind: match f.kind {
                    SpecialFolderKind::Desktop => RootKind::Desktop,
                    SpecialFolderKind::Downloads => RootKind::Downloads,
                    SpecialFolderKind::Documents => RootKind::Documents,
                    SpecialFolderKind::Home => RootKind::Home,
                },
                total_bytes: None,
                available_bytes: None,
            })
            .collect();

        Ok(FsRootsReply {
            drives,
            special_folders,
        })
    }

    /// 在阻塞线程池中读取目录，并按最多 2000 项一帧分批发送。
    /// 返回 `Ok(true)` 表示业务成功，`Ok(false)` 表示已向对端返回业务错误帧。
    pub async fn list_to_stream<W>(
        &self,
        perms: &Permissions,
        args: FsListArgs,
        send: &mut W,
    ) -> Result<bool, crate::error::CoreError>
    where
        W: AsyncWrite + Unpin + ?Sized,
    {
        self.list_to_stream_opts(perms, args, send, false).await
    }

    pub async fn list_to_stream_opts<W>(
        &self,
        perms: &Permissions,
        args: FsListArgs,
        send: &mut W,
        compress: bool,
    ) -> Result<bool, crate::error::CoreError>
    where
        W: AsyncWrite + Unpin + ?Sized,
    {
        if !perms.fs_read {
            let resp: ResponseEnvelope<FsListBatch> =
                ResponseEnvelope::err(ErrorCode::Forbidden, "对方没有开放读取文件的权限");
            write_frame(send, &resp).await?;
            return Ok(false);
        }

        let (clean_dir, fs_dir) = match resolve_authorized_path(&args.path, perms) {
            Ok(pair) => pair,
            Err(rpc_err) => {
                let resp: ResponseEnvelope<FsListBatch> = ResponseEnvelope::Err(rpc_err);
                write_frame(send, &resp).await?;
                return Ok(false);
            }
        };

        let batch_cap = args
            .batch_size
            .unwrap_or(DEFAULT_LIST_BATCH_SIZE)
            .clamp(1, DEFAULT_LIST_BATCH_SIZE);
        let display_dir = to_display_path(&clean_dir);

        // 任务 2.3：渐进式流式列目录——读满前 batch_cap（500）项立即推上网络流，与后续磁盘遍历流水线并行
        if args.batch_size.is_some() {
            let (tx, mut rx) =
                tokio::sync::mpsc::channel::<Result<FsListBatch, RpcError>>(8);
            let disp_for_worker = display_dir.clone();
            tokio::task::spawn_blocking(move || {
                stream_dir_batches(&disp_for_worker, &fs_dir, batch_cap, tx);
            });
            while let Some(item) = rx.recv().await {
                match item {
                    Ok(batch) => {
                        let done = !batch.has_more;
                        write_frame_maybe_compressed(
                            send,
                            &ResponseEnvelope::ok(batch),
                            compress,
                        )
                        .await?;
                        if done {
                            break;
                        }
                    }
                    Err(rpc_err) => {
                        let resp: ResponseEnvelope<FsListBatch> = ResponseEnvelope::Err(rpc_err);
                        write_frame(send, &resp).await?;
                        return Ok(false);
                    }
                }
            }
            return Ok(true);
        }

        let clean_dir_clone = clean_dir.clone();
        let entries_res =
            tokio::task::spawn_blocking(move || read_dir_fast(&clean_dir_clone, &fs_dir))
                .await
                .map_err(|e| crate::error::CoreError::Internal(format!("列目录任务异常：{e}")))?;

        let entries = match entries_res {
            Ok(list) => list,
            Err(rpc_err) => {
                let resp: ResponseEnvelope<FsListBatch> = ResponseEnvelope::Err(rpc_err);
                write_frame(send, &resp).await?;
                return Ok(false);
            }
        };

        let total = entries.len();
        if entries.is_empty() {
            let batch = FsListBatch {
                path: display_dir,
                entries: Vec::new(),
                has_more: false,
                total_hint: Some(0),
            };
            write_frame(send, &ResponseEnvelope::ok(batch)).await?;
            return Ok(true);
        }

        let mut cursor = 0usize;
        while cursor < total {
            let end = (cursor + batch_cap).min(total);
            let batch = FsListBatch {
                path: display_dir.clone(),
                entries: entries[cursor..end].to_vec(),
                has_more: end < total,
                total_hint: Some(total),
            };
            write_frame_maybe_compressed(send, &ResponseEnvelope::ok(batch), compress).await?;
            cursor = end;
        }
        Ok(true)
    }

    pub async fn bench_list_local(&self, dir: &Path) -> Result<usize, RpcError> {
        let clean = normalize_lexical_absolute(&dir.to_string_lossy())?;
        let fs_dir = to_fs_path(&clean);
        let list = tokio::task::spawn_blocking(move || read_dir_fast(&clean, &fs_dir))
            .await
            .map_err(|e| RpcError::new(ErrorCode::Internal, format!("列目录异常：{e}")))??;
        Ok(list.len())
    }

    /// 本机直接列目录（供桌面界面操作 `self` 时使用）。
    pub async fn list_local(&self, perms: &Permissions, path: &str) -> Result<FsListBatch, RpcError> {
        if !perms.fs_read {
            return Err(RpcError::new(ErrorCode::Forbidden, "没有读取文件的权限"));
        }
        let (clean_dir, fs_dir) = resolve_authorized_path(path, perms)?;
        let display_dir = to_display_path(&clean_dir);
        let entries = tokio::task::spawn_blocking(move || read_dir_fast(&clean_dir, &fs_dir))
            .await
            .map_err(|e| RpcError::new(ErrorCode::Internal, format!("列目录异常：{e}")))??;
        let total = entries.len();
        Ok(FsListBatch {
            path: display_dir,
            entries,
            has_more: false,
            total_hint: Some(total),
        })
    }

    pub fn subscribe_task_events(&self) -> broadcast::Receiver<(String, RemoteEvent)> {
        self.remote_events_tx.subscribe()
    }

    pub async fn stat(&self, perms: &Permissions, args: FsStatArgs) -> Result<FsStatReply, RpcError> {
        if !perms.fs_read {
            return Err(RpcError::new(ErrorCode::Forbidden, "对方没有开放读取文件的权限"));
        }
        let (clean, fs_path) = resolve_authorized_path(&args.path, perms)?;
        let entry = tokio::task::spawn_blocking(move || stat_one(&clean, &fs_path))
            .await
            .map_err(|e| RpcError::new(ErrorCode::Internal, format!("查询文件信息异常：{e}")))??;
        Ok(FsStatReply { entry })
    }

    pub async fn mkdir(
        &self,
        perms: &Permissions,
        args: FsMkdirArgs,
    ) -> Result<FsMkdirReply, RpcError> {
        if !perms.fs_write {
            return Err(RpcError::new(ErrorCode::Forbidden, "对方没有开放修改文件的权限"));
        }
        let (clean, fs_path) = resolve_authorized_path(&args.path, perms)?;
        let display = to_display_path(&clean);
        let display_clone = display.clone();
        tokio::task::spawn_blocking(move || {
            if args.parents {
                fs::create_dir_all(&fs_path).map_err(|e| map_io_to_rpc(&e, Some(&display_clone)))
            } else {
                fs::create_dir(&fs_path).map_err(|e| map_io_to_rpc(&e, Some(&display_clone)))
            }
        })
        .await
        .map_err(|e| RpcError::new(ErrorCode::Internal, format!("创建文件夹异常：{e}")))??;

        Ok(FsMkdirReply { path: display })
    }

    pub async fn rename(
        &self,
        perms: &Permissions,
        args: FsRenameArgs,
    ) -> Result<FsRenameReply, RpcError> {
        if !perms.fs_write {
            return Err(RpcError::new(ErrorCode::Forbidden, "对方没有开放修改文件的权限"));
        }
        let valid_name = validate_single_name(&args.new_name)?.to_string();
        let (old_clean, old_fs) = resolve_authorized_path(&args.path, perms)?;
        let parent = old_clean.parent().ok_or_else(|| {
            RpcError::new(ErrorCode::Invalid, "根目录或盘符不能重命名")
        })?;
        let new_clean = parent.join(&valid_name);
        let (_, new_fs) = resolve_authorized_path(&to_display_path(&new_clean), perms)?;

        let old_disp = to_display_path(&old_clean);
        let new_disp = to_display_path(&new_clean);
        let old_disp_c = old_disp.clone();
        let new_disp_c = new_disp.clone();

        tokio::task::spawn_blocking(move || {
            if !old_fs.exists() {
                return Err(RpcError::new(
                    ErrorCode::NotFound,
                    format!("找不到要重命名的目标：{old_disp_c}"),
                ));
            }
            if new_fs.exists()
                && !old_disp_c.eq_ignore_ascii_case(&new_disp_c)
            {
                return Err(RpcError::new(
                    ErrorCode::AlreadyExists,
                    format!("已存在同名文件或文件夹：{new_disp_c}"),
                ));
            }
            fs::rename(&old_fs, &new_fs).map_err(|e| map_io_to_rpc(&e, Some(&old_disp_c)))
        })
        .await
        .map_err(|e| RpcError::new(ErrorCode::Internal, format!("重命名任务异常：{e}")))??;

        Ok(FsRenameReply {
            old_path: old_disp,
            new_path: new_disp,
        })
    }

    pub async fn delete(
        &self,
        perms: &Permissions,
        args: FsDeleteArgs,
    ) -> Result<FsDeleteReply, RpcError> {
        if !perms.fs_write {
            return Err(RpcError::new(ErrorCode::Forbidden, "对方没有开放删除文件的权限"));
        }
        if args.paths.is_empty() {
            return Ok(FsDeleteReply { deleted_count: 0 });
        }

        let mut clean_paths = Vec::with_capacity(args.paths.len());
        let mut fs_paths = Vec::with_capacity(args.paths.len());
        for raw in &args.paths {
            let (clean, fs_p) = resolve_authorized_path(raw, perms)?;
            clean_paths.push(clean);
            fs_paths.push(fs_p);
        }

        let count = clean_paths.len();
        let platform = self.platform.clone();
        let permanent = args.permanent;

        tokio::task::spawn_blocking(move || {
            if permanent {
                for (clean, fs_p) in clean_paths.iter().zip(fs_paths.iter()) {
                    let disp = to_display_path(clean);
                    remove_path_permanent(fs_p).map_err(|e| map_io_to_rpc(&e, Some(&disp)))?;
                }
                Ok(())
            } else {
                platform
                    .move_to_trash(&clean_paths)
                    .map_err(|e| map_platform_to_rpc(&e))
            }
        })
        .await
        .map_err(|e| RpcError::new(ErrorCode::Internal, format!("删除任务异常：{e}")))??;

        Ok(FsDeleteReply {
            deleted_count: count,
        })
    }

    pub fn start_copy_or_move(
        &self,
        peer_id: &str,
        kind: FsTaskKind,
        perms: &Permissions,
        args: FsCopyMoveArgs,
    ) -> Result<FsTaskReply, RpcError> {
        if !perms.fs_read || !perms.fs_write {
            return Err(RpcError::new(
                ErrorCode::Forbidden,
                "复制或移动需要同时具备文件读取和写入权限",
            ));
        }
        if args.sources.is_empty() {
            return Err(RpcError::new(ErrorCode::Invalid, "没有指定要复制或移动的源文件"));
        }

        let (dest_clean, dest_fs) = resolve_authorized_path(&args.dest_dir, perms)?;
        if !dest_fs.is_dir() {
            return Err(RpcError::new(
                ErrorCode::NotFound,
                format!("目标文件夹不存在：{}", to_display_path(&dest_clean)),
            ));
        }

        let mut sources = Vec::with_capacity(args.sources.len());
        for s in &args.sources {
            let (sc, sf) = resolve_authorized_path(s, perms)?;
            let src_meta = fs::symlink_metadata(&sf).map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    RpcError::new(
                        ErrorCode::NotFound,
                        format!("找不到源路径：{}", to_display_path(&sc)),
                    )
                } else {
                    map_io_to_rpc(&e, Some(&to_display_path(&sc)))
                }
            })?;
            // 提前拦截将文件夹复制/移动到自身或其子文件夹
            if src_meta.is_dir()
                && !is_symlink_or_junction(&sf, &src_meta)
                && is_same_or_descendant(&dest_clean, &dest_fs, &sc, &sf)
            {
                return Err(RpcError::new(
                    ErrorCode::Invalid,
                    format!(
                        "不能将文件夹“{}”复制或移动到自身或其子文件夹中",
                        to_display_path(&sc)
                    ),
                ));
            }
            sources.push((sc, sf));
        }

        let task_id = format!("fstask-{:016x}", fastrand::u64(..));
        let cancel = CancellationToken::new();
        let initial = FsTaskProgress {
            task_id: task_id.clone(),
            kind,
            state: FsTaskState::Running,
            processed_bytes: 0,
            total_bytes: 0,
            processed_items: 0,
            total_items: sources.len(),
            current_item: None,
            error: None,
        };

        {
            let mut guard = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
            prune_finished_tasks(&mut guard);
            if guard.len() >= MAX_TRACKED_TASKS {
                return Err(RpcError::new(
                    ErrorCode::Busy,
                    "当前同时进行的文件复制/移动任务过多，请稍后再试",
                ));
            }
            guard.insert(
                task_id.clone(),
                ManagedTask {
                    progress: initial.clone(),
                    cancel: cancel.clone(),
                    updated_at: Instant::now(),
                },
            );
        }

        let tasks_ref = self.tasks.clone();
        let events_tx = self.remote_events_tx.clone();
        let peer_owned = peer_id.to_string();
        let task_id_clone = task_id.clone();
        let conflict = args.conflict;

        tokio::task::spawn_blocking(move || {
            run_copy_or_move_blocking(
                peer_owned,
                task_id_clone,
                kind,
                sources,
                dest_clean,
                conflict,
                cancel,
                tasks_ref,
                events_tx,
            );
        });

        Ok(FsTaskReply { task_id })
    }

    pub fn task_status(&self, task_id: &str) -> Result<FsTaskProgress, RpcError> {
        let guard = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .get(task_id)
            .map(|t| t.progress.clone())
            .ok_or_else(|| RpcError::new(ErrorCode::NotFound, format!("找不到文件任务：{task_id}")))
    }

    pub fn task_cancel(&self, task_id: &str) -> Result<FsTaskProgress, RpcError> {
        let mut guard = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
        let Some(task) = guard.get_mut(task_id) else {
            return Err(RpcError::new(
                ErrorCode::NotFound,
                format!("找不到文件任务：{task_id}"),
            ));
        };
        task.cancel.cancel();
        if task.progress.state == FsTaskState::Running {
            task.progress.state = FsTaskState::Cancelled;
            task.updated_at = Instant::now();
        }
        Ok(task.progress.clone())
    }
}

mod batch_rw;
mod copy_move;
mod io_helpers;
mod stream_rw;

use copy_move::*;
use io_helpers::*;
pub use io_helpers::{
    create_sequential_write, deterministic_part_name, hash_file_prefix_sync,
    open_or_create_sequential_rw, open_sequential_read, open_sequential_rw, pick_unique_target,
    replace_file_atomically, sanitize_rel_subpath, system_time_to_ms,
};
pub(crate) use io_helpers::write_all_at_offset;
