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
    TRANSFER_CHUNK_SIZE, WalkEntry, read_frame, write_frame,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWrite, AsyncWriteExt};
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

pub struct FsService {
    platform: Arc<dyn Platform>,
    tasks: Arc<Mutex<HashMap<String, ManagedTask>>>,
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
                        write_frame(send, &ResponseEnvelope::ok(batch)).await?;
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
            write_frame(send, &ResponseEnvelope::ok(batch)).await?;
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

    /// 处理 `fs.read`（下载）：先回 `FsReadHeader` 控制帧，紧接着写原始文件字节（64 KB 分块）。
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

        let mut file = match tokio::fs::File::open(&fs_path).await {
            Ok(f) => f,
            Err(err) => {
                let rpc = map_io_to_rpc(&err, Some(&disp));
                write_frame(send, &ResponseEnvelope::<FsReadHeader>::Err(rpc)).await?;
                return Ok(false);
            }
        };

        let meta = match file.metadata().await {
            Ok(m) => m,
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

        if args.offset > 0
            && let Err(err) = file.seek(std::io::SeekFrom::Start(args.offset)).await
        {
            let rpc = map_io_to_rpc(&err, Some(&disp));
            write_frame(send, &ResponseEnvelope::<FsReadHeader>::Err(rpc)).await?;
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

        let mut buf = vec![0u8; TRANSFER_CHUNK_SIZE];
        let mut left = transfer_length;
        while left > 0 {
            let to_read = (left as usize).min(TRANSFER_CHUNK_SIZE);
            let n = file.read(&mut buf[..to_read]).await?;
            if n == 0 {
                break;
            }
            send.write_all(&buf[..n]).await?;
            left -= n as u64;
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

        let mut part_file = if args.offset > 0 {
            match tokio::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&part_fs)
                .await
            {
                Ok(mut f) => {
                    let cur_len = f.metadata().await.map(|m| m.len()).unwrap_or(0);
                    if cur_len < args.offset {
                        write_frame(
                            send,
                            &ResponseEnvelope::<FsWriteReady>::err(
                                ErrorCode::Invalid,
                                "临时续传文件长度不足，请从头重传",
                            ),
                        )
                        .await?;
                        return Ok(false);
                    }
                    let _ = f.set_len(args.offset).await;
                    f.seek(std::io::SeekFrom::Start(args.offset)).await?;
                    f
                }
                Err(err) => {
                    let rpc = map_io_to_rpc(&err, Some(&to_display_path(&part_clean)));
                    write_frame(send, &ResponseEnvelope::<FsWriteReady>::Err(rpc)).await?;
                    return Ok(false);
                }
            }
        } else {
            match tokio::fs::File::create(&part_fs).await {
                Ok(f) => f,
                Err(err) => {
                    let rpc = map_io_to_rpc(&err, Some(&to_display_path(&final_clean)));
                    write_frame(send, &ResponseEnvelope::<FsWriteReady>::Err(rpc)).await?;
                    return Ok(false);
                }
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

        let mut buf = vec![0u8; TRANSFER_CHUNK_SIZE];
        let mut received = args.offset;
        while received < args.size {
            let want = ((args.size - received) as usize).min(TRANSFER_CHUNK_SIZE);
            match recv.read(&mut buf[..want]).await {
                Ok(0) => {
                    // 连接意外断开（非主动 reset 取消）：刷盘保留 .pcshare.part 供断点续传
                    let _ = part_file.flush().await;
                    guard.commit();
                    return Err(crate::error::CoreError::NotConnected(
                        "传输中途连接断开，已保留断点数据".to_string(),
                    ));
                }
                Ok(n) => {
                    part_file.write_all(&buf[..n]).await?;
                    received += n as u64;
                }
                Err(io_err) => {
                    let is_explicit_cancel = is_explicit_cancel_error(&io_err);
                    if !is_explicit_cancel {
                        let _ = part_file.flush().await;
                        guard.commit();
                    }
                    return Err(if is_explicit_cancel {
                        crate::error::CoreError::Cancelled
                    } else {
                        crate::error::CoreError::Io(io_err)
                    });
                }
            }
        }
        part_file.flush().await?;
        part_file.sync_all().await?;

        let std_file = part_file.into_std().await;
        if let Some(ms) = args.modified_ms {
            let mtime = UNIX_EPOCH + Duration::from_millis(ms);
            let _ = std_file.set_modified(mtime);
        }
        drop(std_file);

        // 原子替换为最终文件
        let part_fs_for_rename = part_fs.clone();
        let final_fs_for_rename = final_fs.clone();
        let final_disp_for_err = final_disp.clone();
        let rename_res = tokio::task::spawn_blocking(move || {
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
            write_frame(send, &ResponseEnvelope::ok(batch)).await?;
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

        // 握手前先核验所有请求项存在、为普通文件且未超出批传单文件上限
        let check_list: Vec<(String, PathBuf)> = resolved
            .iter()
            .map(|(_, disp, fs_p)| (disp.clone(), fs_p.clone()))
            .collect();
        let precheck = tokio::task::spawn_blocking(move || -> Result<(), RpcError> {
            for (disp, fs_p) in check_list {
                let meta = fs::metadata(&fs_p).map_err(|e| map_io_to_rpc(&e, Some(&disp)))?;
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
            }
            Ok(())
        })
        .await
        .map_err(|e| crate::error::CoreError::Internal(format!("批量读取校验异常：{e}")))?;
        if let Err(rpc) = precheck {
            write_frame(send, &ResponseEnvelope::<()>::Err(rpc)).await?;
            return Ok(false);
        }

        write_frame(send, &ResponseEnvelope::ok(())).await?;

        const READ_BATCH_CONCURRENCY: usize = 4;
        const SINGLE_FILE_READ_TIMEOUT: Duration = Duration::from_secs(30);
        let mut inflight = std::collections::VecDeque::with_capacity(READ_BATCH_CONCURRENCY);
        let mut iter = resolved.into_iter();

        let spawn_reader = |src_str: String, disp: String, fs_p: PathBuf| {
            tokio::task::spawn_blocking(
                move || -> Result<(String, Vec<u8>, Option<u64>, String), RpcError> {
                    let meta = fs::metadata(&fs_p).map_err(|e| map_io_to_rpc(&e, Some(&disp)))?;
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
                    let bytes = fs::read(&fs_p).map_err(|e| map_io_to_rpc(&e, Some(&disp)))?;
                    if bytes.len() as u64 > MAX_BATCH_FILE_BYTES {
                        return Err(RpcError::new(
                            ErrorCode::Invalid,
                            format!("文件大小已超过批传上限：{disp}"),
                        ));
                    }
                    let hex = blake3::hash(&bytes).to_hex().to_string();
                    Ok((src_str, bytes, mtime, hex))
                },
            )
        };

        for _ in 0..READ_BATCH_CONCURRENCY {
            if let Some((src_str, disp, fs_p)) = iter.next() {
                inflight.push_back(spawn_reader(src_str, disp, fs_p));
            }
        }

        while let Some(handle) = inflight.pop_front() {
            if let Some((src_str, disp, fs_p)) = iter.next() {
                inflight.push_back(spawn_reader(src_str, disp, fs_p));
            }
            let joined = match tokio::time::timeout(SINGLE_FILE_READ_TIMEOUT, handle).await {
                Ok(Ok(res)) => res,
                Ok(Err(e)) => {
                    for h in inflight {
                        h.abort();
                    }
                    return Err(crate::error::CoreError::Internal(format!(
                        "批量读取线程异常：{e}"
                    )));
                }
                Err(_) => {
                    for h in inflight {
                        h.abort();
                    }
                    return Err(crate::error::CoreError::Timeout);
                }
            };
            let (src_str, bytes, modified_ms, blake3_hex) = match joined {
                Ok(item) => item,
                Err(rpc) => {
                    for h in inflight {
                        h.abort();
                    }
                    return Err(crate::error::CoreError::Rpc(rpc));
                }
            };
            let hdr = BatchReadItemHeader {
                done: false,
                source_path: src_str,
                size: bytes.len() as u64,
                modified_ms,
                blake3_hex,
            };
            if let Err(e) = write_frame(send, &hdr).await {
                for h in inflight {
                    h.abort();
                }
                return Err(e.into());
            }
            if !bytes.is_empty()
                && let Err(e) = send.write_all(&bytes).await
            {
                for h in inflight {
                    h.abort();
                }
                return Err(e.into());
            }
        }

        write_frame(
            send,
            &BatchReadItemHeader {
                done: true,
                ..Default::default()
            },
        )
        .await?;
        send.flush().await?;
        Ok(true)
    }
}

pub fn deterministic_part_name(file_name: &str) -> String {
    format!(".{file_name}.pcshare.part")
}

pub fn sanitize_rel_subpath(rel: &str) -> Result<PathBuf, RpcError> {
    let trimmed = rel.trim().replace('/', "\\");
    if trimmed.is_empty() {
        return Err(RpcError::new(ErrorCode::Invalid, "相对路径不能为空"));
    }
    let mut out = PathBuf::new();
    for part in trimmed.split('\\').filter(|s| !s.is_empty()) {
        validate_single_name(part)?;
        out.push(part);
    }
    if out.as_os_str().is_empty() {
        return Err(RpcError::new(ErrorCode::Invalid, "无效的相对子路径"));
    }
    Ok(out)
}

pub fn hash_file_prefix_sync(fs_path: &Path, limit: u64) -> std::io::Result<String> {
    use std::io::Read;
    let mut f = File::open(fs_path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 128 * 1024];
    let mut left = limit;
    while left > 0 {
        let want = (left as usize).min(buf.len());
        let n = f.read(&mut buf[..want])?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        left -= n as u64;
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn collect_walk_entries_sync(
    root_clean: &Path,
    root_fs: &Path,
    out: &mut Vec<WalkEntry>,
) -> Result<(), RpcError> {
    let root_disp = to_display_path(root_clean);
    let meta = fs::symlink_metadata(root_fs).map_err(|e| map_io_to_rpc(&e, Some(&root_disp)))?;
    let base_name = root_clean
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "root".to_string());

    if is_symlink_or_junction(root_fs, &meta) {
        return Ok(());
    }
    if !meta.is_dir() {
        out.push(WalkEntry {
            rel_path: base_name,
            abs_path: root_disp,
            dir: false,
            size: meta.len(),
            modified_ms: meta.modified().ok().and_then(system_time_to_ms),
        });
        return Ok(());
    }

    let mut stack: Vec<(PathBuf, PathBuf, String)> =
        vec![(root_clean.to_path_buf(), root_fs.to_path_buf(), base_name)];
    while let Some((cur_clean, cur_fs, cur_rel)) = stack.pop() {
        let cur_disp = to_display_path(&cur_clean);
        let cur_meta = fs::symlink_metadata(&cur_fs).ok();
        out.push(WalkEntry {
            rel_path: cur_rel.clone(),
            abs_path: cur_disp.clone(),
            dir: true,
            size: 0,
            modified_ms: cur_meta
                .and_then(|m| m.modified().ok())
                .and_then(system_time_to_ms),
        });

        let rd = fs::read_dir(&cur_fs).map_err(|e| map_io_to_rpc(&e, Some(&cur_disp)))?;
        let mut sub_dirs = Vec::new();
        for item_res in rd {
            let Ok(item) = item_res else { continue };
            let Ok(ft) = item.file_type() else { continue };
            if ft.is_symlink() {
                continue;
            }
            let item_path = item.path();
            let Ok(m) = fs::symlink_metadata(&item_path) else {
                continue;
            };
            if is_symlink_or_junction(&item_path, &m) {
                continue;
            }
            let name = item.file_name().to_string_lossy().into_owned();
            let child_clean = cur_clean.join(&name);
            let child_rel = format!(r"{cur_rel}\{name}");
            if m.is_dir() {
                sub_dirs.push((child_clean, item_path, child_rel));
            } else {
                out.push(WalkEntry {
                    rel_path: child_rel,
                    abs_path: to_display_path(&child_clean),
                    dir: false,
                    size: m.len(),
                    modified_ms: m.modified().ok().and_then(system_time_to_ms),
                });
            }
        }
        sub_dirs.reverse();
        stack.extend(sub_dirs);
    }
    Ok(())
}

pub fn pick_unique_target(clean_dest: &Path) -> (PathBuf, PathBuf) {
    let fs_initial = to_fs_path(clean_dest);
    if !fs_initial.exists() {
        return (clean_dest.to_path_buf(), fs_initial);
    }
    let parent = clean_dest.parent().unwrap_or_else(|| Path::new(""));
    let stem = clean_dest
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    let ext = clean_dest
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();

    for idx in 1..100_000u32 {
        let candidate = parent.join(format!("{stem} ({idx}){ext}"));
        let candidate_fs = to_fs_path(&candidate);
        if !candidate_fs.exists() {
            return (candidate, candidate_fs);
        }
    }
    let fallback = parent.join(format!("{stem} ({}){ext}", fastrand::u64(..)));
    let fallback_fs = to_fs_path(&fallback);
    (fallback, fallback_fs)
}

pub fn replace_file_atomically(src_fs: &Path, dst_fs: &Path) -> std::io::Result<()> {
    match fs::rename(src_fs, dst_fs) {
        Ok(()) => Ok(()),
        Err(_err) if dst_fs.exists() => {
            fs::remove_file(dst_fs)?;
            fs::rename(src_fs, dst_fs)
        }
        Err(err) => Err(err),
    }
}

pub fn system_time_to_ms(t: SystemTime) -> Option<u64> {
    t.duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as u64)
}

fn make_dir_prefix(disp_dir: &str) -> String {
    if disp_dir.ends_with('\\') || disp_dir.ends_with('/') {
        disp_dir.to_string()
    } else {
        let mut p = String::with_capacity(disp_dir.len() + 1);
        p.push_str(disp_dir);
        p.push('\\');
        p
    }
}

fn convert_os_dir_entry(prefix: &str, item: fs::DirEntry) -> DirEntry {
    let name = item.file_name().to_string_lossy().into_owned();
    let mut full_path = String::with_capacity(prefix.len() + name.len());
    full_path.push_str(prefix);
    full_path.push_str(&name);
    // Windows 上 DirEntry::metadata() 直接使用 FindNextFileW 缓存的数据，不产生额外系统调用
    let (kind, size, modified_ms, created_ms, readonly) = match item.metadata() {
        Ok(meta) => {
            let ft = meta.file_type();
            let kind = if ft.is_symlink() {
                EntryKind::Symlink
            } else if ft.is_dir() {
                EntryKind::Dir
            } else {
                EntryKind::File
            };
            (
                kind,
                if ft.is_dir() { 0 } else { meta.len() },
                meta.modified().ok().and_then(system_time_to_ms),
                meta.created().ok().and_then(system_time_to_ms),
                meta.permissions().readonly(),
            )
        }
        Err(_) => (EntryKind::File, 0, None, None, false),
    };
    DirEntry {
        name,
        path: full_path,
        kind,
        size,
        modified_ms,
        created_ms,
        readonly,
    }
}

fn stream_dir_batches(
    disp_dir: &str,
    fs_dir: &Path,
    first_batch_cap: usize,
    tx: tokio::sync::mpsc::Sender<Result<FsListBatch, RpcError>>,
) {
    let reader = match fs::read_dir(fs_dir) {
        Ok(r) => r,
        Err(e) => {
            let _ = tx.blocking_send(Err(map_io_to_rpc(&e, Some(disp_dir))));
            return;
        }
    };
    let prefix = make_dir_prefix(disp_dir);
    let mut iter = reader.flatten();
    let mut current_cap = first_batch_cap.max(1);
    let next_cap = DEFAULT_LIST_BATCH_SIZE.max(current_cap);
    let mut chunk = Vec::with_capacity(current_cap);
    let mut peeked = iter.next();

    if peeked.is_none() {
        let _ = tx.blocking_send(Ok(FsListBatch {
            path: disp_dir.to_string(),
            entries: Vec::new(),
            has_more: false,
            total_hint: Some(0),
        }));
        return;
    }

    while let Some(item) = peeked {
        chunk.push(convert_os_dir_entry(&prefix, item));
        peeked = iter.next();
        if chunk.len() >= current_cap || peeked.is_none() {
            let has_more = peeked.is_some();
            let outgoing = std::mem::replace(
                &mut chunk,
                if has_more {
                    Vec::with_capacity(next_cap)
                } else {
                    Vec::new()
                },
            );
            if tx
                .blocking_send(Ok(FsListBatch {
                    path: disp_dir.to_string(),
                    entries: outgoing,
                    has_more,
                    total_hint: None,
                }))
                .is_err()
            {
                return;
            }
            current_cap = next_cap;
        }
    }
}

fn read_dir_fast(clean_dir: &Path, fs_dir: &Path) -> Result<Vec<DirEntry>, RpcError> {
    let disp_dir = to_display_path(clean_dir);
    let reader = fs::read_dir(fs_dir).map_err(|e| map_io_to_rpc(&e, Some(&disp_dir)))?;
    let prefix = make_dir_prefix(&disp_dir);
    let mut entries = Vec::new();
    for item in reader.flatten() {
        entries.push(convert_os_dir_entry(&prefix, item));
    }

    // 文件夹优先，再按名称不区分大小写排序（零额外堆分配比较）
    entries.sort_by(|a, b| {
        let a_is_dir = a.kind == EntryKind::Dir;
        let b_is_dir = b.kind == EntryKind::Dir;
        b_is_dir
            .cmp(&a_is_dir)
            .then_with(|| cmp_case_insensitive(&a.name, &b.name))
    });
    Ok(entries)
}

fn cmp_case_insensitive(a: &str, b: &str) -> std::cmp::Ordering {
    if a.is_ascii() && b.is_ascii() {
        return a
            .bytes()
            .map(|c| c.to_ascii_lowercase())
            .cmp(b.bytes().map(|c| c.to_ascii_lowercase()))
            .then_with(|| a.cmp(b));
    }
    let mut ia = a.chars().flat_map(char::to_lowercase);
    let mut ib = b.chars().flat_map(char::to_lowercase);
    loop {
        match (ia.next(), ib.next()) {
            (Some(ca), Some(cb)) => match ca.cmp(&cb) {
                std::cmp::Ordering::Equal => continue,
                non_eq => return non_eq,
            },
            (None, Some(_)) => return std::cmp::Ordering::Less,
            (Some(_), None) => return std::cmp::Ordering::Greater,
            (None, None) => return a.cmp(b),
        }
    }
}

fn stat_one(clean: &Path, fs_path: &Path) -> Result<DirEntry, RpcError> {
    let disp = to_display_path(clean);
    let meta = fs::symlink_metadata(fs_path).map_err(|e| map_io_to_rpc(&e, Some(&disp)))?;
    let ft = meta.file_type();
    let kind = if ft.is_symlink() {
        EntryKind::Symlink
    } else if ft.is_dir() {
        EntryKind::Dir
    } else {
        EntryKind::File
    };
    let name = clean
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| disp.clone());
    Ok(DirEntry {
        name,
        path: disp,
        kind,
        size: if ft.is_dir() { 0 } else { meta.len() },
        modified_ms: meta.modified().ok().and_then(system_time_to_ms),
        created_ms: meta.created().ok().and_then(system_time_to_ms),
        readonly: meta.permissions().readonly(),
    })
}

#[allow(clippy::too_many_arguments)]
fn run_copy_or_move_blocking(
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

fn estimate_size(fs_path: &Path) -> u64 {
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

fn copy_recursive(
    src_fs: &Path,
    dst_clean: &Path,
    cancel: &CancellationToken,
    processed_bytes: &mut u64,
    on_progress: &dyn Fn(u64),
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

    if meta.is_dir() {
        fs::create_dir_all(&dst_fs).map_err(|e| map_io_to_rpc(&e, Some(&disp)))?;
        for entry_res in fs::read_dir(src_fs).map_err(|e| map_io_to_rpc(&e, Some(&disp)))? {
            let entry = entry_res.map_err(|e| map_io_to_rpc(&e, Some(&disp)))?;
            let child_clean = dst_clean.join(entry.file_name());
            copy_recursive(
                &entry.path(),
                &child_clean,
                cancel,
                processed_bytes,
                on_progress,
            )?;
        }
        Ok(())
    } else {
        use std::io::{Read, Write};
        let mut reader = File::open(src_fs).map_err(|e| map_io_to_rpc(&e, Some(&disp)))?;
        let mut writer = File::create(&dst_fs).map_err(|e| map_io_to_rpc(&e, Some(&disp)))?;
        let mut buf = vec![0u8; TRANSFER_CHUNK_SIZE];
        let mut last_emit = Instant::now();
        loop {
            if cancel.is_cancelled() {
                drop(writer);
                let _ = fs::remove_file(&dst_fs);
                return Err(RpcError::new(ErrorCode::Cancelled, "任务已取消"));
            }
            let n = reader
                .read(&mut buf)
                .map_err(|e| map_io_to_rpc(&e, Some(&disp)))?;
            if n == 0 {
                break;
            }
            writer
                .write_all(&buf[..n])
                .map_err(|e| map_io_to_rpc(&e, Some(&disp)))?;
            *processed_bytes = processed_bytes.saturating_add(n as u64);
            if last_emit.elapsed() >= Duration::from_millis(100) {
                on_progress(*processed_bytes);
                last_emit = Instant::now();
            }
        }
        if let Ok(mtime) = meta.modified() {
            let _ = writer.set_modified(mtime);
        }
        Ok(())
    }
}

/// 判断目标是否为符号链接或 Windows 联接点（排除 OneDrive 等云占位重解析点）。
fn is_symlink_or_junction(path: &Path, meta: &fs::Metadata) -> bool {
    meta.file_type().is_symlink() || (is_windows_reparse_point(meta) && fs::read_link(path).is_ok())
}

fn is_windows_reparse_point(meta: &fs::Metadata) -> bool {
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

fn remove_path_permanent(fs_p: &Path) -> std::io::Result<()> {
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

fn is_same_path(a_clean: &Path, a_fs: &Path, b_clean: &Path, b_fs: &Path) -> bool {
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

fn is_same_or_descendant(
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

fn is_explicit_cancel_error(io_err: &std::io::Error) -> bool {
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

fn prune_finished_tasks(tasks: &mut HashMap<String, ManagedTask>) {
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
