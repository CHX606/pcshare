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
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Notify, Semaphore};
use tokio::task::JoinSet;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use crate::error::{CoreError, map_io_to_rpc};
use crate::events::{
    CoreEvent, DeviceConnState, ProgressCallback, TransferDirection, TransferProgressEvent,
};
use crate::fs_service::{
    MAX_BATCH_FILE_BYTES, STREAM_STOP_CODE_CANCEL, STREAM_STOP_CODE_PAUSE,
    create_sequential_write, deterministic_part_name, hash_file_prefix_sync, open_sequential_read,
    open_sequential_rw, pick_unique_target, replace_file_atomically, sanitize_rel_subpath,
    system_time_to_ms, write_all_at_offset,
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
}

mod direct_p2p;
mod download;
mod queue_batch;
mod upload;

fn chunk_walk_entries(items: Vec<WalkEntry>, concurrency: usize) -> Vec<Vec<WalkEntry>> {
    let item_cap = items
        .len()
        .div_ceil(concurrency.max(1))
        .clamp(128, BATCH_MAX_ITEMS);
    let mut batches = Vec::new();
    let mut cur = Vec::new();
    let mut cur_bytes = 0u64;
    for item in items {
        cur_bytes += item.size;
        cur.push(item);
        if cur.len() >= item_cap || cur_bytes >= BATCH_MAX_BYTES {
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
