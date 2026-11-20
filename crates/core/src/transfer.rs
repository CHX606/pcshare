//! 任务 2.3 & 2.4：渐进式大目录拉取、断点续传、文件夹递归传输、4 路并发队列、限速、小文件单流批传与两台受控电脑直传。

use std::collections::HashMap;
use std::fs::{self, File};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, UNIX_EPOCH};

use constant_time_eq::constant_time_eq;
use iroh::endpoint::Connection;
use iroh::{EndpointId, TransportAddr};
use pcshare_proto::*;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{Notify, Semaphore};
use tokio::task::JoinSet;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use crate::error::{CoreError, map_io_to_rpc};
use crate::events::{
    CoreEvent, DeviceConnState, ProgressCallback, TransferDirection, TransferProgressEvent,
};
use crate::fs_service::{
    MAX_BATCH_FILE_BYTES, STREAM_STOP_CODE_CANCEL, STREAM_STOP_CODE_PAUSE, deterministic_part_name,
    hash_file_prefix_sync, pick_unique_target, replace_file_atomically, sanitize_rel_subpath,
    system_time_to_ms,
};
use crate::node::{CoreNode, build_endpoint_addr};
use crate::pairing::csprng_token_hex_64;
use crate::path_guard::{
    normalize_lexical_absolute, resolve_authorized_path, to_display_path, to_fs_path,
};

const CLOSE_CANCELLED: u32 = STREAM_STOP_CODE_CANCEL;
const CLOSE_PAUSED: u32 = STREAM_STOP_CODE_PAUSE;
const BATCH_MAX_ITEMS: usize = 2600;
const BATCH_MAX_BYTES: u64 = 16 * 1024 * 1024;
const DIRECT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

pub type WireDeltaCallback = Arc<dyn Fn(u64) + Send + Sync>;

#[derive(Debug, Clone)]
pub struct ProgressiveDirBatch {
    pub path: String,
    pub entries: Vec<DirEntry>,
    pub total_hint: Option<usize>,
    pub done: bool,
}

#[derive(Debug, Clone)]
pub struct TransferQueueOptions {
    pub conflict: ConflictPolicy,
    pub concurrency: usize,
    pub speed_limit_bps: Option<u64>,
}

impl Default for TransferQueueOptions {
    fn default() -> Self {
        Self {
            conflict: ConflictPolicy::RenameKeepBoth,
            concurrency: 4,
            speed_limit_bps: None,
        }
    }
}

#[derive(Clone)]
pub struct TransferTaskControl {
    paused: Arc<AtomicBool>,
    cancel: CancellationToken,
    notify: Arc<Notify>,
    speed_limit_bps: Arc<AtomicU64>,
}

impl Default for TransferTaskControl {
    fn default() -> Self {
        Self::new()
    }
}

impl TransferTaskControl {
    pub fn new() -> Self {
        Self {
            paused: Arc::new(AtomicBool::new(false)),
            cancel: CancellationToken::new(),
            notify: Arc::new(Notify::new()),
            speed_limit_bps: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn from_token(cancel: CancellationToken) -> Self {
        Self {
            paused: Arc::new(AtomicBool::new(false)),
            cancel,
            notify: Arc::new(Notify::new()),
            speed_limit_bps: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn set_speed_limit(&self, bps: Option<u64>) {
        self.speed_limit_bps
            .store(bps.unwrap_or(0), Ordering::Relaxed);
    }

    pub fn speed_limit_bps(&self) -> Option<u64> {
        let v = self.speed_limit_bps.load(Ordering::Relaxed);
        if v == 0 { None } else { Some(v) }
    }

    /// 派生一个共享暂停/动态限速、带独立子取消令牌的控制器（父取消会级联，子取消不影响父）。
    pub fn child(&self) -> Self {
        Self {
            paused: self.paused.clone(),
            cancel: self.cancel.child_token(),
            notify: self.notify.clone(),
            speed_limit_bps: self.speed_limit_bps.clone(),
        }
    }

    pub fn pause(&self) {
        self.paused.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    pub fn resume(&self) {
        self.paused.store(false, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    pub fn cancel(&self) {
        self.cancel.cancel();
        self.notify.notify_waiters();
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    pub async fn wait_ready(&self) -> Result<(), CoreError> {
        loop {
            if self.cancel.is_cancelled() {
                return Err(CoreError::Cancelled);
            }
            if !self.paused.load(Ordering::SeqCst) {
                return Ok(());
            }
            tokio::select! {
                _ = self.cancel.cancelled() => return Err(CoreError::Cancelled),
                _ = self.notify.notified() => {}
            }
        }
    }
}

pub struct RateLimiter {
    global_bps: Arc<AtomicU64>,
    /// 单调预约时间（GCRA 虚拟调度）：多并发调用按顺序串行预约配额时间片，彻底消除并发倍率放大。
    next_available: Mutex<Instant>,
}

impl RateLimiter {
    pub fn new(initial_bps: Option<u64>) -> Arc<Self> {
        Arc::new(Self {
            global_bps: Arc::new(AtomicU64::new(initial_bps.unwrap_or(0))),
            next_available: Mutex::new(Instant::now()),
        })
    }

    pub fn set_limit_bps(&self, bps: Option<u64>) {
        self.global_bps.store(bps.unwrap_or(0), Ordering::Relaxed);
        // 调高或解除限速时立即收缩历史低速预约，让在途任务马上提速
        let now = Instant::now();
        let mut guard = self
            .next_available
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if *guard > now {
            *guard = now;
        }
    }

    pub fn limit_bps(&self) -> Option<u64> {
        let v = self.global_bps.load(Ordering::Relaxed);
        if v == 0 { None } else { Some(v) }
    }

    fn effective_limit_bps(
        &self,
        task_bps: Option<u64>,
        override_bps: Option<u64>,
    ) -> u64 {
        let per_task = task_bps.or(override_bps);
        let global = self.limit_bps();
        match (per_task, global) {
            (Some(a), Some(b)) => a.min(b),
            (Some(a), None) => a,
            (None, Some(b)) => b,
            (None, None) => 0,
        }
    }

    fn reserve_wait(&self, bytes: usize, limit: u64) -> Duration {
        if limit == 0 || bytes == 0 {
            return Duration::ZERO;
        }
        let mut guard = self
            .next_available
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        // 允许极小突发信用（100ms），既不丢平滑性，也不放大多并发下的长期速率
        let max_burst = Duration::from_millis(100);
        let earliest = now.checked_sub(max_burst).unwrap_or(now);
        let slot_start = if *guard < earliest { earliest } else { *guard };
        let cost = Duration::from_secs_f64((bytes as f64) / (limit as f64));
        let next = slot_start + cost;
        *guard = next;
        next.saturating_duration_since(now)
    }

    pub async fn acquire(&self, bytes: usize, override_bps: Option<u64>) {
        let limit = self.effective_limit_bps(None, override_bps);
        let wait_dur = self.reserve_wait(bytes, limit);
        if !wait_dur.is_zero() {
            tokio::time::sleep(wait_dur).await;
        }
    }

    pub async fn acquire_with_ctrl(
        &self,
        bytes: usize,
        ctrl: &TransferTaskControl,
        override_bps: Option<u64>,
    ) -> Result<(), CoreError> {
        if ctrl.is_cancelled() {
            return Err(CoreError::Cancelled);
        }
        let limit = self.effective_limit_bps(ctrl.speed_limit_bps(), override_bps);
        let wait_dur = self.reserve_wait(bytes, limit);
        if !wait_dur.is_zero() {
            let cancel_tok = ctrl.cancel_token();
            tokio::select! {
                _ = cancel_tok.cancelled() => return Err(CoreError::Cancelled),
                _ = tokio::time::sleep(wait_dur) => {}
            }
        }
        Ok(())
    }
}

#[derive(Clone)]
pub(crate) struct DirectGrantRecord {
    pub allowed_peer_id: String,
    pub dest_clean: PathBuf,
    pub expires_at: Instant,
}

impl CoreNode {
    // ────────────────────────── 任务 2.3：大目录渐进式分批读取 ──────────────────────────

    pub async fn remote_list_progressive<F>(
        self: &Arc<Self>,
        peer_id: &str,
        path: &str,
        mut on_batch: F,
    ) -> Result<Vec<DirEntry>, CoreError>
    where
        F: FnMut(ProgressiveDirBatch),
    {
        let cmd = Command::FsList(FsListArgs {
            path: path.to_string(),
            batch_size: Some(FIRST_LIST_BATCH_SIZE),
        });
        let env = cmd.to_envelope()?;

        let mut attempt = 0u8;
        let (mut send, mut recv) = loop {
            let conn = self.get_or_connect(peer_id).await?;
            match conn.open_bi().await {
                Ok(pair) => break pair,
                Err(e) if attempt == 0 => {
                    attempt += 1;
                    self.invalidate_active_conn(peer_id);
                    let _ = e;
                }
                Err(e) => return Err(CoreError::NotConnected(format!("打开列目录通道失败：{e}"))),
            }
        };

        write_frame(&mut send, &env).await?;
        send.finish()
            .map_err(|e| CoreError::Internal(format!("结束请求流失败：{e}")))?;

        let mut all_entries = Vec::new();
        let resolved_path;
        loop {
            let resp: ResponseEnvelope<FsListBatch> =
                timeout(Duration::from_secs(15), read_frame(&mut recv))
                    .await
                    .map_err(|_| CoreError::Timeout)??;
            let batch = resp.into_result()?;
            let batch_path = batch.path.clone();
            let done = !batch.has_more;
            all_entries.extend(batch.entries.iter().cloned());
            on_batch(ProgressiveDirBatch {
                path: batch.path,
                entries: batch.entries,
                total_hint: batch.total_hint,
                done,
            });
            if done {
                resolved_path = batch_path;
                break;
            }
        }

        self.dir_cache()
            .put_silent(peer_id, &resolved_path, all_entries.clone());
        Ok(all_entries)
    }

    pub async fn remote_hash(
        self: &Arc<Self>,
        peer_id: &str,
        path: &str,
        length: Option<u64>,
    ) -> Result<FsHashReply, CoreError> {
        self.call_rpc(
            peer_id,
            Command::FsHash(FsHashArgs {
                path: path.to_string(),
                length,
            }),
        )
        .await
    }

    pub async fn remote_probe_resume(
        self: &Arc<Self>,
        peer_id: &str,
        dest_dir: &str,
        file_name: &str,
        conflict: ConflictPolicy,
    ) -> Result<FsProbeResumeReply, CoreError> {
        self.call_rpc(
            peer_id,
            Command::FsProbeResume(FsProbeResumeArgs {
                dest_dir: dest_dir.to_string(),
                file_name: file_name.to_string(),
                conflict,
            }),
        )
        .await
    }

    pub async fn remote_walk(
        self: &Arc<Self>,
        peer_id: &str,
        sources: &[String],
    ) -> Result<Vec<WalkEntry>, CoreError> {
        let conn = self.get_or_connect(peer_id).await?;
        let (mut send, mut recv) = conn
            .open_bi()
            .await
            .map_err(|e| CoreError::NotConnected(format!("打开遍历通道失败：{e}")))?;
        let cmd = Command::FsWalk(FsWalkArgs {
            sources: sources.to_vec(),
        });
        write_frame(&mut send, &cmd.to_envelope()?).await?;
        send.finish()
            .map_err(|e| CoreError::Internal(format!("结束遍历请求流失败：{e}")))?;

        let mut all = Vec::new();
        loop {
            let resp: ResponseEnvelope<FsWalkBatch> =
                timeout(Duration::from_secs(30), read_frame(&mut recv))
                    .await
                    .map_err(|_| CoreError::Timeout)??;
            let batch = resp.into_result()?;
            all.extend(batch.entries);
            if batch.done {
                break;
            }
        }
        Ok(all)
    }

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
        let (mut send, mut recv) = conn
            .open_bi()
            .await
            .map_err(|e| CoreError::NotConnected(format!("打开上传通道失败：{e}")))?;

        let cmd = Command::FsWrite(FsWriteArgs {
            path: remote_dest.to_string(),
            size: total_size,
            offset: resume_offset,
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

        let mut file = tokio::fs::File::open(&local_fs)
            .await
            .map_err(|e| CoreError::Rpc(map_io_to_rpc(&e, Some(&local_disp))))?;
        if resume_offset > 0 {
            file.seek(std::io::SeekFrom::Start(resume_offset)).await?;
        }

        let started = Instant::now();
        let mut last_report = Instant::now();
        let mut sent = resume_offset;
        let mut buf = vec![0u8; TRANSFER_CHUNK_SIZE];

        if let Some(ref cb) = on_progress
            && resume_offset > 0
        {
            cb(resume_offset, total_size, 0.0);
        }

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

            let want = ((total_size - sent) as usize).min(TRANSFER_CHUNK_SIZE);
            let n = file.read(&mut buf[..want]).await?;
            if n == 0 {
                let _ = send.reset(CLOSE_PAUSED.into());
                return Err(CoreError::Internal("读取本地文件时意外提前结束".to_string()));
            }

            if let Err(err) = self
                .rate_limiter()
                .acquire_with_ctrl(n, ctrl, speed_limit_bps)
                .await
            {
                let _ = send.reset(CLOSE_CANCELLED.into());
                return Err(err);
            }

            let cancel_tok = ctrl.cancel_token();
            tokio::select! {
                _ = cancel_tok.cancelled() => {
                    let _ = send.reset(CLOSE_CANCELLED.into());
                    return Err(CoreError::Cancelled);
                }
                w = send.write_all(&buf[..n]) => {
                    w.map_err(|e| CoreError::NotConnected(format!("上传数据中断：{e}")))?;
                }
            }
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

        let mut part_file = if resume_offset > 0 {
            let mut f = tokio::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&part_fs)
                .await?;
            f.set_len(resume_offset).await?;
            f.seek(std::io::SeekFrom::Start(resume_offset)).await?;
            f
        } else {
            tokio::fs::File::create(&part_fs).await?
        };

        let started = Instant::now();
        let mut last_report = Instant::now();
        let mut received = resume_offset;
        let total = header.file_size;
        let mut buf = vec![0u8; TRANSFER_CHUNK_SIZE];
        let final_disp = to_display_path(&final_clean);

        while received < total {
            if ctrl.is_cancelled() {
                let _ = recv.stop(CLOSE_CANCELLED.into());
                drop(part_file);
                let _ = fs::remove_file(&part_fs);
                return Err(CoreError::Cancelled);
            }
            if ctrl.is_paused()
                && let Err(e) = ctrl.wait_ready().await
            {
                let _ = recv.stop(CLOSE_CANCELLED.into());
                drop(part_file);
                let _ = fs::remove_file(&part_fs);
                return Err(e);
            }

            let want = ((total - received) as usize).min(TRANSFER_CHUNK_SIZE);
            let cancel_tok = ctrl.cancel_token();
            let chunk_opt = tokio::select! {
                _ = cancel_tok.cancelled() => {
                    let _ = recv.stop(CLOSE_CANCELLED.into());
                    drop(part_file);
                    let _ = fs::remove_file(&part_fs);
                    return Err(CoreError::Cancelled);
                }
                r = recv.read(&mut buf[..want]) => {
                    match r {
                        Ok(opt) => opt,
                        Err(e) => {
                            // 网络中断：刷盘并保留 `.pcshare.part` 以便重连后断点续传
                            let _ = part_file.flush().await;
                            return Err(CoreError::NotConnected(format!("下载数据流中断：{e}")));
                        }
                    }
                }
            };
            let Some(n) = chunk_opt else {
                let _ = part_file.flush().await;
                return Err(CoreError::NotConnected(
                    "下载尚未完成但连接已中断，已保留断点数据".to_string(),
                ));
            };

            if let Err(err) = self
                .rate_limiter()
                .acquire_with_ctrl(n, ctrl, speed_limit_bps)
                .await
            {
                let _ = recv.stop(CLOSE_CANCELLED.into());
                drop(part_file);
                let _ = fs::remove_file(&part_fs);
                return Err(err);
            }
            part_file.write_all(&buf[..n]).await?;
            received += n as u64;
            if let Some(ref wcb) = on_wire_delta {
                wcb(n as u64);
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

        if ctrl.is_cancelled() {
            let _ = recv.stop(CLOSE_CANCELLED.into());
            drop(part_file);
            let _ = fs::remove_file(&part_fs);
            return Err(CoreError::Cancelled);
        }

        part_file.flush().await?;
        part_file.sync_all().await?;
        let std_file = part_file.into_std().await;
        if let Some(ms) = header.modified_ms {
            let _ = std_file.set_modified(UNIX_EPOCH + Duration::from_millis(ms));
        }
        drop(std_file);

        let part_fs_c = part_fs.clone();
        let final_fs_c = final_fs.clone();
        let final_disp_c = final_disp.clone();
        tokio::task::spawn_blocking(move || {
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

        // 1. 先按批次并发发送所有目录骨架与小文件（单流连续发多文件，大幅减少开流开销）
        let batches = chunk_walk_entries(small_items);
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

        // 1. 小文件并发批量下载
        if !small_files.is_empty() {
            let sem = Arc::new(Semaphore::new(concurrency));
            let mut chunks = Vec::new();
            let mut cur = Vec::new();
            let mut cur_bytes = 0u64;
            for pair in small_files {
                cur_bytes += pair.0.size;
                cur.push(pair);
                if cur.len() >= BATCH_MAX_ITEMS || cur_bytes >= BATCH_MAX_BYTES {
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

        let mut received_files = 0usize;
        loop {
            if let Err(err) = ctrl.wait_ready().await {
                let _ = recv.stop(CLOSE_CANCELLED.into());
                return Err(err);
            }
            let cancel_tok = ctrl.cancel_token();
            let hdr: BatchReadItemHeader = tokio::select! {
                _ = cancel_tok.cancelled() => {
                    let _ = recv.stop(CLOSE_CANCELLED.into());
                    return Err(CoreError::Cancelled);
                }
                r = read_frame(&mut recv) => r?,
            };
            if hdr.done {
                break;
            }
            if hdr.size > MAX_BATCH_FILE_BYTES {
                let _ = recv.stop(CLOSE_CANCELLED.into());
                return Err(CoreError::Rpc(RpcError::new(
                    ErrorCode::Invalid,
                    format!(
                        "批量下载收到超过单文件上限的数据（{} 字节，上限 {} 字节）：{}",
                        hdr.size, MAX_BATCH_FILE_BYTES, hdr.source_path
                    ),
                )));
            }
            let mut data = vec![0u8; hdr.size as usize];
            if hdr.size > 0 {
                recv.read_exact(&mut data).await.map_err(|e| {
                    CoreError::NotConnected(format!("读取批量下载数据中断：{e}"))
                })?;
            }
            if let Err(err) = self
                .rate_limiter()
                .acquire_with_ctrl(data.len(), ctrl, speed_limit_bps)
                .await
            {
                let _ = recv.stop(CLOSE_CANCELLED.into());
                return Err(err);
            }

            let got_hex = blake3::hash(&data).to_hex().to_string();
            if got_hex != hdr.blake3_hex {
                return Err(CoreError::Rpc(RpcError::new(
                    ErrorCode::Io,
                    format!("下载文件校验失败：{}", hdr.source_path),
                )));
            }

            if let Some(target_clean) = target_map.get(&hdr.source_path).cloned() {
                let modified_ms = hdr.modified_ms;
                tokio::task::spawn_blocking(move || -> Result<(), RpcError> {
                    let initial_fs = to_fs_path(&target_clean);
                    let final_fs = if initial_fs.exists() {
                        match conflict {
                            ConflictPolicy::Skip => return Ok(()),
                            ConflictPolicy::Fail => {
                                return Err(RpcError::new(
                                    ErrorCode::AlreadyExists,
                                    format!("本地目标已存在：{}", to_display_path(&target_clean)),
                                ));
                            }
                            ConflictPolicy::Overwrite => initial_fs,
                            ConflictPolicy::RenameKeepBoth => pick_unique_target(&target_clean).1,
                        }
                    } else {
                        initial_fs
                    };
                    if let Some(p) = final_fs.parent() {
                        fs::create_dir_all(p).map_err(|e| map_io_to_rpc(&e, None))?;
                    }
                    let mut f = File::create(&final_fs)
                        .map_err(|e| map_io_to_rpc(&e, Some(&to_display_path(&target_clean))))?;
                    std::io::Write::write_all(&mut f, &data)
                        .map_err(|e| map_io_to_rpc(&e, Some(&to_display_path(&target_clean))))?;
                    if let Some(ms) = modified_ms {
                        let _ = f.set_modified(UNIX_EPOCH + Duration::from_millis(ms));
                    }
                    Ok(())
                })
                .await
                .map_err(|e| CoreError::Internal(format!("写入本地小文件异常：{e}")))??;
                received_files += 1;
            }

            let agg = done_bytes.fetch_add(hdr.size, Ordering::Relaxed) + hdr.size;
            if let Some(ref cb) = on_progress {
                let spd = agg as f64 / started.elapsed().as_secs_f64().max(0.001);
                cb(agg, total_bytes.max(1), spd);
            }
        }

        if received_files < expected_files {
            return Err(CoreError::Internal(format!(
                "批量下载未收齐全部文件（期望 {expected_files} 个，实际收到 {received_files} 个）"
            )));
        }
        Ok(())
    }

    // ────────────────── 任务 2.4：两台对方电脑之间直传（数据不经过控制端）──────────────────

    #[allow(clippy::too_many_arguments)]
    pub async fn transfer_between_peers_direct(
        self: &Arc<Self>,
        src_peer_id: &str,
        sources: &[String],
        dst_peer_id: &str,
        dest_dir: &str,
        conflict: ConflictPolicy,
        cancel: CancellationToken,
        on_progress: Option<ProgressCallback>,
    ) -> Result<(), CoreError> {
        // 确保与乙的连接就绪并获取乙的最新可达地址
        let dst_conn = self.get_or_connect(dst_peer_id).await?;
        let mut target_addrs: Vec<String> = dst_conn
            .paths()
            .iter()
            .filter_map(|p| match p.remote_addr() {
                TransportAddr::Ip(ip) => Some(ip.to_string()),
                _ => None,
            })
            .collect();
        if let Some(rec) = self.storage().get_peer(dst_peer_id) {
            for a in rec.direct_addrs {
                let s = a.to_string();
                if !target_addrs.contains(&s) {
                    target_addrs.push(s);
                }
            }
        }
        let target_relay = self.storage().get_peer(dst_peer_id).and_then(|r| r.relay_url);

        let token = csprng_token_hex_64();
        let transfer_id = format!("p2p-{}", &csprng_token_hex_64()[..16]);

        // 1. 先向乙签发一次性直传接收许可（5 分钟有效）
        let grant_rep: PeerGrantDirectReply = self
            .call_rpc(
                dst_peer_id,
                Command::PeerGrantDirect(PeerGrantDirectArgs {
                    token: token.clone(),
                    allowed_peer_id: src_peer_id.to_string(),
                    dest_dir: dest_dir.to_string(),
                    ttl_ms: 300_000,
                }),
            )
            .await?;
        if !grant_rep.accepted {
            return Err(CoreError::Internal("目标设备拒绝了直传接收许可".to_string()));
        }

        // 订阅来自甲的直传进度事件（等待 sub.events 真正就绪，去掉 50ms 盲等）
        let src_conn = self.get_or_connect(src_peer_id).await?;
        self.ensure_peer_event_subscription(src_peer_id, src_conn.clone())
            .await;
        let mut ev_rx = self.subscribe_events();

        // 2. 通知甲直接连接乙并推送数据
        let start_rep: PeerDirectTransferReply = self
            .call_rpc(
                src_peer_id,
                Command::PeerDirectTransfer(PeerDirectTransferArgs {
                    transfer_id: transfer_id.clone(),
                    target_peer_id: dst_peer_id.to_string(),
                    target_addrs,
                    target_relay_url: target_relay,
                    token,
                    sources: sources.to_vec(),
                    dest_dir: dest_dir.to_string(),
                    conflict,
                }),
            )
            .await?;
        if !start_rep.accepted {
            return Err(CoreError::Internal("源设备未能启动直传任务".to_string()));
        }

        let started = Instant::now();
        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    // 取消时通知源端中止直传，不再让源端继续向目标推送
                    let _ = timeout(
                        Duration::from_secs(2),
                        self.remote_task_cancel(src_peer_id, &transfer_id),
                    )
                    .await;
                    return Err(CoreError::Cancelled);
                }
                _ = src_conn.closed() => {
                    return Err(CoreError::NotConnected("源设备在直传途中断开连接，直传已中断".to_string()));
                }
                ev = ev_rx.recv() => {
                    match ev {
                        Ok(CoreEvent::RemoteDirectTransferProgress { peer_id, progress })
                            if peer_id == src_peer_id && progress.transfer_id == transfer_id =>
                        {
                            let spd = progress.bytes_done as f64 / started.elapsed().as_secs_f64().max(0.001);
                            if let Some(ref cb) = on_progress {
                                cb(progress.bytes_done, progress.bytes_total.max(1), spd);
                            }
                            match progress.state {
                                FsTaskState::Running => {}
                                FsTaskState::Completed => return Ok(()),
                                FsTaskState::Cancelled => return Err(CoreError::Cancelled),
                                FsTaskState::Failed => {
                                    return Err(match progress.error {
                                        Some(rpc) => CoreError::Rpc(rpc),
                                        None => CoreError::Internal("两台远端设备直传失败".to_string()),
                                    });
                                }
                            }
                        }
                        Ok(CoreEvent::DeviceStateChanged { peer_id, state: DeviceConnState::Offline { .. } })
                            if peer_id == src_peer_id =>
                        {
                            return Err(CoreError::NotConnected("源设备已离线，直传已中断".to_string()));
                        }
                        Ok(_) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            return Err(CoreError::Internal("事件通道已关闭".to_string()));
                        }
                    }
                }
            }
        }
    }

    pub(crate) fn handle_grant_direct(
        &self,
        perms: &Permissions,
        args: PeerGrantDirectArgs,
    ) -> Result<PeerGrantDirectReply, RpcError> {
        if !perms.fs_write {
            return Err(RpcError::new(ErrorCode::Forbidden, "没有写入文件的权限"));
        }
        let (dest_clean, dest_fs) = resolve_authorized_path(&args.dest_dir, perms)?;
        fs::create_dir_all(&dest_fs)
            .map_err(|e| map_io_to_rpc(&e, Some(&to_display_path(&dest_clean))))?;
        let ttl = Duration::from_millis(args.ttl_ms.clamp(5_000, 600_000));
        self.insert_direct_grant(
            args.token,
            DirectGrantRecord {
                allowed_peer_id: args.allowed_peer_id,
                dest_clean,
                expires_at: Instant::now() + ttl,
            },
        );
        Ok(PeerGrantDirectReply { accepted: true })
    }

    pub(crate) fn spawn_outgoing_direct_transfer(
        self: &Arc<Self>,
        controller_peer_id: &str,
        perms: &Permissions,
        args: PeerDirectTransferArgs,
    ) -> Result<PeerDirectTransferReply, RpcError> {
        if !perms.fs_read {
            return Err(RpcError::new(ErrorCode::Forbidden, "没有读取源文件的权限"));
        }
        for s in &args.sources {
            let _ = resolve_authorized_path(s, perms)?;
        }
        let cancel = CancellationToken::new();
        self.register_direct_transfer(args.transfer_id.clone(), cancel.clone());

        let node = self.clone();
        let ctrl_id = controller_peer_id.to_string();
        let perms_clone = perms.clone();
        tokio::spawn(async move {
            let res = node
                .run_outgoing_direct_worker(&ctrl_id, &perms_clone, &args, cancel)
                .await;
            node.unregister_direct_transfer(&args.transfer_id);
            match res {
                Ok(()) => {}
                Err(CoreError::Cancelled) => {
                    node.emit_remote_event(
                        &ctrl_id,
                        RemoteEvent::DirectTransferProgress(DirectTransferProgress {
                            transfer_id: args.transfer_id,
                            state: FsTaskState::Cancelled,
                            bytes_done: 0,
                            bytes_total: 1,
                            error: None,
                        }),
                    );
                }
                Err(err) => {
                    let rpc = match err {
                        CoreError::Rpc(r) => r,
                        other => RpcError::new(ErrorCode::Io, other.to_user_message()),
                    };
                    node.emit_remote_event(
                        &ctrl_id,
                        RemoteEvent::DirectTransferProgress(DirectTransferProgress {
                            transfer_id: args.transfer_id,
                            state: FsTaskState::Failed,
                            bytes_done: 0,
                            bytes_total: 1,
                            error: Some(rpc),
                        }),
                    );
                }
            }
        });
        Ok(PeerDirectTransferReply { accepted: true })
    }

    async fn run_outgoing_direct_worker(
        self: &Arc<Self>,
        controller_peer_id: &str,
        perms: &Permissions,
        args: &PeerDirectTransferArgs,
        cancel: CancellationToken,
    ) -> Result<(), CoreError> {
        if cancel.is_cancelled() {
            return Err(CoreError::Cancelled);
        }
        let walked = self
            .fs_service()
            .walk_local(perms, &args.sources)
            .await
            .map_err(CoreError::Rpc)?;
        let total_bytes: u64 = walked.iter().filter(|e| !e.dir).map(|e| e.size).sum();

        let target_id: EndpointId = args
            .target_peer_id
            .parse()
            .map_err(|_| CoreError::InvalidArgument("目标设备身份码格式无效".to_string()))?;
        let sock_addrs: Vec<SocketAddr> = args
            .target_addrs
            .iter()
            .filter_map(|s| s.parse().ok())
            .collect();
        let endpoint_addr =
            build_endpoint_addr(target_id, args.target_relay_url.as_deref(), &sock_addrs);

        let conn = timeout(
            Duration::from_secs(15),
            self.endpoint().connect(endpoint_addr, ALPN_DIRECT),
        )
        .await
        .map_err(|_| CoreError::Timeout)?
        .map_err(|e| CoreError::NotConnected(format!("无法直连目标设备：{e}")))?;

        let mut small_items = Vec::new();
        let mut large_files = Vec::new();
        for item in walked {
            if item.dir || item.size <= SMALL_FILE_BATCH_THRESHOLD {
                small_items.push(item);
            } else {
                large_files.push(item);
            }
        }

        let mut done_bytes = 0u64;
        let mut last_emit = Instant::now();

        // 1. 小文件与目录骨架走 Batch 流
        if !small_items.is_empty() {
            let (mut send, mut recv) = conn
                .open_bi()
                .await
                .map_err(|e| CoreError::NotConnected(format!("打开直传小文件流失败：{e}")))?;

            let req = DirectStreamRequest::Batch {
                token: args.token.clone(),
                conflict: args.conflict,
            };
            write_frame(&mut send, &req).await?;
            let ready: ResponseEnvelope<()> =
                timeout(Duration::from_secs(15), read_frame(&mut recv))
                    .await
                    .map_err(|_| CoreError::Timeout)??;
            ready.into_result()?;

            for item in small_items {
                if cancel.is_cancelled() {
                    let _ = send.reset(CLOSE_CANCELLED.into());
                    conn.close(CLOSE_CANCELLED.into(), b"direct_cancelled");
                    return Err(CoreError::Cancelled);
                }
                if item.dir {
                    write_frame(
                        &mut send,
                        &BatchFileItem {
                            done: false,
                            rel_path: item.rel_path,
                            dir: true,
                            size: 0,
                            modified_ms: item.modified_ms,
                            blake3_hex: None,
                        },
                    )
                    .await?;
                } else {
                    let fs_p = to_fs_path(Path::new(&item.abs_path));
                    let meta_len = tokio::fs::metadata(&fs_p).await?.len();
                    if meta_len > MAX_BATCH_FILE_BYTES {
                        let _ = send.reset(CLOSE_CANCELLED.into());
                        return Err(CoreError::Rpc(RpcError::new(
                            ErrorCode::Invalid,
                            format!(
                                "小文件大小变化超过直传批传上限（{meta_len} 字节）：{}",
                                item.abs_path
                            ),
                        )));
                    }
                    let bytes = tokio::fs::read(&fs_p).await?;
                    if bytes.len() as u64 > MAX_BATCH_FILE_BYTES {
                        let _ = send.reset(CLOSE_CANCELLED.into());
                        return Err(CoreError::Rpc(RpcError::new(
                            ErrorCode::Invalid,
                            format!("小文件大小变化超过直传批传上限：{}", item.abs_path),
                        )));
                    }
                    let hex = blake3::hash(&bytes).to_hex().to_string();
                    write_frame(
                        &mut send,
                        &BatchFileItem {
                            done: false,
                            rel_path: item.rel_path,
                            dir: false,
                            size: bytes.len() as u64,
                            modified_ms: item.modified_ms,
                            blake3_hex: Some(hex),
                        },
                    )
                    .await?;
                    if !bytes.is_empty() {
                        send.write_all(&bytes).await.map_err(|e| {
                            CoreError::NotConnected(format!("直传发送文件数据中断：{e}"))
                        })?;
                    }
                    done_bytes += bytes.len() as u64;
                    if last_emit.elapsed() >= Duration::from_millis(100) {
                        self.emit_remote_event(
                            controller_peer_id,
                            RemoteEvent::DirectTransferProgress(DirectTransferProgress {
                                transfer_id: args.transfer_id.clone(),
                                state: FsTaskState::Running,
                                bytes_done: done_bytes,
                                bytes_total: total_bytes.max(1),
                                error: None,
                            }),
                        );
                        last_emit = Instant::now();
                    }
                }
            }

            write_frame(
                &mut send,
                &BatchFileItem {
                    done: true,
                    ..Default::default()
                },
            )
            .await?;
            send.finish()
                .map_err(|e| CoreError::Internal(format!("结束直传流失败：{e}")))?;

            let done_resp: ResponseEnvelope<FsWriteBatchReply> =
                timeout(Duration::from_secs(60), read_frame(&mut recv))
                    .await
                    .map_err(|_| CoreError::Timeout)??;
            done_resp.into_result()?;
        }

        // 2. 大文件走 SingleFile 分块流式传输（64 KB 分块，绝不整文件读入内存防 OOM）
        let mut buf = vec![0u8; TRANSFER_CHUNK_SIZE];
        for item in large_files {
            if cancel.is_cancelled() {
                conn.close(CLOSE_CANCELLED.into(), b"direct_cancelled");
                return Err(CoreError::Cancelled);
            }
            let fs_p = to_fs_path(Path::new(&item.abs_path));
            let mut file = tokio::fs::File::open(&fs_p).await?;
            let file_size = file.metadata().await?.len();

            let (mut send, mut recv) = conn
                .open_bi()
                .await
                .map_err(|e| CoreError::NotConnected(format!("打开直传单文件流失败：{e}")))?;

            let req = DirectStreamRequest::SingleFile {
                token: args.token.clone(),
                rel_path: item.rel_path,
                size: file_size,
                offset: 0,
                modified_ms: item.modified_ms,
                conflict: args.conflict,
                blake3_hex: None,
            };
            write_frame(&mut send, &req).await?;
            let ready_resp: ResponseEnvelope<FsWriteReady> =
                timeout(Duration::from_secs(15), read_frame(&mut recv))
                    .await
                    .map_err(|_| CoreError::Timeout)??;
            let ready = ready_resp.into_result()?;
            if ready.skipped {
                let _ = send.finish();
                done_bytes += file_size;
                continue;
            }

            let mut sent = 0u64;
            while sent < file_size {
                if cancel.is_cancelled() {
                    let _ = send.reset(CLOSE_CANCELLED.into());
                    conn.close(CLOSE_CANCELLED.into(), b"direct_cancelled");
                    return Err(CoreError::Cancelled);
                }
                let want = ((file_size - sent) as usize).min(TRANSFER_CHUNK_SIZE);
                let n = file.read(&mut buf[..want]).await?;
                if n == 0 {
                    let _ = send.reset(CLOSE_PAUSED.into());
                    return Err(CoreError::Internal("直传读取本地文件意外提前结束".to_string()));
                }
                tokio::select! {
                    _ = cancel.cancelled() => {
                        let _ = send.reset(CLOSE_CANCELLED.into());
                        conn.close(CLOSE_CANCELLED.into(), b"direct_cancelled");
                        return Err(CoreError::Cancelled);
                    }
                    w = send.write_all(&buf[..n]) => {
                        w.map_err(|e| CoreError::NotConnected(format!("直传发送大文件数据中断：{e}")))?;
                    }
                }
                sent += n as u64;
                done_bytes += n as u64;
                if last_emit.elapsed() >= Duration::from_millis(100) {
                    self.emit_remote_event(
                        controller_peer_id,
                        RemoteEvent::DirectTransferProgress(DirectTransferProgress {
                            transfer_id: args.transfer_id.clone(),
                            state: FsTaskState::Running,
                            bytes_done: done_bytes,
                            bytes_total: total_bytes.max(1),
                            error: None,
                        }),
                    );
                    last_emit = Instant::now();
                }
            }
            send.finish()
                .map_err(|e| CoreError::Internal(format!("结束直传单文件流失败：{e}")))?;

            let done_resp: ResponseEnvelope<FsWriteDone> =
                timeout(Duration::from_secs(60), read_frame(&mut recv))
                    .await
                    .map_err(|_| CoreError::Timeout)??;
            done_resp.into_result()?;
        }

        conn.close(0u32.into(), b"direct_done");

        self.emit_remote_event(
            controller_peer_id,
            RemoteEvent::DirectTransferProgress(DirectTransferProgress {
                transfer_id: args.transfer_id.clone(),
                state: FsTaskState::Completed,
                bytes_done: total_bytes,
                bytes_total: total_bytes.max(1),
                error: None,
            }),
        );
        Ok(())
    }

    pub(crate) async fn handle_incoming_direct(self: Arc<Self>, conn: Connection) {
        let remote_id = conn.remote_id().to_string();
        // 令牌一次性消费后仅绑定当前已认证连接（供该连接上的小文件 Batch 与多个大文件 SingleFile 流共用）
        let mut session_grant: Option<(String, DirectGrantRecord)> = None;

        loop {
            let accept_timeout = if session_grant.is_none() {
                DIRECT_HANDSHAKE_TIMEOUT
            } else {
                Duration::from_secs(30)
            };
            let Ok(Ok((mut send, mut recv))) = timeout(accept_timeout, conn.accept_bi()).await
            else {
                break;
            };

            if self.is_host_paused() {
                let _ = write_frame(
                    &mut send,
                    &ResponseEnvelope::<()>::err(
                        ErrorCode::Busy,
                        "对方当前开启了“暂停被控”，暂不接收直传文件",
                    ),
                )
                .await;
                let _ = send.finish();
                let _ = timeout(Duration::from_secs(2), send.stopped()).await;
                conn.close(403u32.into(), b"host_paused");
                return;
            }

            let Ok(Ok(req)) = timeout(
                DIRECT_HANDSHAKE_TIMEOUT,
                read_frame_limited::<_, DirectStreamRequest>(&mut recv, MAX_HANDSHAKE_FRAME_SIZE),
            )
            .await
            else {
                conn.close(401u32.into(), b"direct_req_timeout");
                return;
            };

            let req_token = match &req {
                DirectStreamRequest::Batch { token, .. }
                | DirectStreamRequest::SingleFile { token, .. }
                | DirectStreamRequest::Mkdir { token, .. }
                | DirectStreamRequest::ProbeResume { token, .. } => token.as_str(),
            };

            let grant = match &session_grant {
                Some((bound_tok, rec)) => {
                    if Instant::now() < rec.expires_at
                        && constant_time_eq(bound_tok.as_bytes(), req_token.as_bytes())
                    {
                        rec.clone()
                    } else {
                        let _ = write_frame(
                            &mut send,
                            &ResponseEnvelope::<()>::err(
                                ErrorCode::Unauthorized,
                                "直传令牌无效或已过期",
                            ),
                        )
                        .await;
                        let _ = send.finish();
                        let _ = timeout(Duration::from_secs(2), send.stopped()).await;
                        conn.close(401u32.into(), b"bad_direct_token");
                        return;
                    }
                }
                None => {
                    // 首次校验成功立即从表里移除销毁（单次有效，拒绝跨连接重放）
                    let Some(rec) = self.take_direct_grant(req_token, &remote_id) else {
                        let _ = write_frame(
                            &mut send,
                            &ResponseEnvelope::<()>::err(
                                ErrorCode::Unauthorized,
                                "直传令牌无效、已被使用或已过期",
                            ),
                        )
                        .await;
                        let _ = send.finish();
                        let _ = timeout(Duration::from_secs(2), send.stopped()).await;
                        conn.close(401u32.into(), b"bad_direct_token");
                        return;
                    };
                    session_grant = Some((req_token.to_string(), rec.clone()));
                    rec
                }
            };

            let mut restricted_perms = Permissions::default();
            restricted_perms
                .allowed_roots
                .push(to_display_path(&grant.dest_clean));

            match req {
                DirectStreamRequest::Batch { conflict, .. } => {
                    let _ = self
                        .fs_service()
                        .handle_write_batch_stream(
                            &restricted_perms,
                            FsWriteBatchArgs {
                                dest_dir: to_display_path(&grant.dest_clean),
                                conflict,
                            },
                            &mut recv,
                            &mut send,
                        )
                        .await;
                    let _ = send.finish();
                }
                DirectStreamRequest::SingleFile {
                    rel_path,
                    size,
                    offset,
                    modified_ms,
                    conflict,
                    ..
                } => {
                    let rel_clean = match sanitize_rel_subpath(&rel_path) {
                        Ok(p) => p,
                        Err(err) => {
                            let _ =
                                write_frame(&mut send, &ResponseEnvelope::<FsWriteReady>::Err(err))
                                    .await;
                            let _ = send.finish();
                            continue;
                        }
                    };
                    let target_clean = grant.dest_clean.join(&rel_clean);
                    let target_disp = to_display_path(&target_clean);
                    let _ = self
                        .fs_service()
                        .handle_write_stream(
                            &restricted_perms,
                            FsWriteArgs {
                                path: target_disp,
                                size,
                                offset,
                                modified_ms,
                                conflict,
                            },
                            &mut recv,
                            &mut send,
                        )
                        .await;
                    let _ = send.finish();
                }
                DirectStreamRequest::Mkdir { rel_path, .. } => {
                    let res = match sanitize_rel_subpath(&rel_path) {
                        Ok(rel_clean) => {
                            let target_disp = to_display_path(&grant.dest_clean.join(&rel_clean));
                            self.fs_service()
                                .mkdir(
                                    &restricted_perms,
                                    FsMkdirArgs {
                                        path: target_disp,
                                        parents: true,
                                    },
                                )
                                .await
                        }
                        Err(err) => Err(err),
                    };
                    let _ = match res {
                        Ok(rep) => write_frame(&mut send, &ResponseEnvelope::ok(rep)).await,
                        Err(err) => {
                            write_frame(&mut send, &ResponseEnvelope::<FsMkdirReply>::Err(err))
                                .await
                        }
                    };
                    let _ = send.finish();
                }
                DirectStreamRequest::ProbeResume { .. } => {
                    let _ = write_frame(
                        &mut send,
                        &ResponseEnvelope::<()>::err(
                            ErrorCode::Unsupported,
                            "不支持的直传子模式",
                        ),
                    )
                    .await;
                    let _ = send.finish();
                }
            }
        }
    }

    /// 校验直传令牌并立即移除销毁（单次有效，防止 5 分钟内跨连接重放）。
    fn take_direct_grant(
        &self,
        candidate_token: &str,
        remote_peer_id: &str,
    ) -> Option<DirectGrantRecord> {
        let mut guard = self
            .direct_grants_mutex()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        guard.retain(|_, v| Instant::now() < v.expires_at);
        let matched_key = guard.iter().find_map(|(stored_tok, rec)| {
            if rec.allowed_peer_id == remote_peer_id
                && constant_time_eq(stored_tok.as_bytes(), candidate_token.as_bytes())
            {
                Some(stored_tok.clone())
            } else {
                None
            }
        });
        matched_key.and_then(|k| guard.remove(&k))
    }
}

fn chunk_walk_entries(items: Vec<WalkEntry>) -> Vec<Vec<WalkEntry>> {
    let mut batches = Vec::new();
    let mut cur = Vec::new();
    let mut cur_bytes = 0u64;
    for item in items {
        cur_bytes += item.size;
        cur.push(item);
        if cur.len() >= BATCH_MAX_ITEMS || cur_bytes >= BATCH_MAX_BYTES {
            batches.push(std::mem::take(&mut cur));
            cur_bytes = 0;
        }
    }
    if !cur.is_empty() {
        batches.push(cur);
    }
    batches
}

fn join_remote_subpath(dest_dir: &str, rel_path: &str) -> String {
    let clean_rel = rel_path.trim_start_matches('\\').trim_start_matches('/');
    if dest_dir.ends_with('\\') || dest_dir.ends_with('/') {
        format!("{dest_dir}{clean_rel}")
    } else {
        format!(r"{dest_dir}\{clean_rel}")
    }
}
