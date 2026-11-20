//! 远端文件的两种打开方式之“取回本机打开并监听保存回传”（任务 3.4）：
//! - 下载到 `<data_dir>/edit_cache/<session_id>/<file_name>` 并记录远端基线 BLAKE3 校验值
//! - 用 `notify` 监听该临时目录的保存事件，首次保存推送 `EditSyncPromptNeeded` 询问“要不要存回去”
//! - 回传前先调用 `fs.hash` 核对远端文件期间是否被别人改动过；若改动则提示冲突而不静默覆盖

use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant, SystemTime};

use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use pcshare_proto::{ConflictPolicy, ErrorCode, FsHashArgs, FsHashReply, RpcError};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::CoreNode;
use crate::error::{CoreError, map_platform_to_rpc};
use crate::events::{CoreEvent, EditSyncPrompt};

const EDIT_SAVE_DEBOUNCE: Duration = Duration::from_millis(300);
const MAX_EDIT_SESSIONS: usize = 32;
const EDIT_SESSION_TTL: Duration = Duration::from_secs(12 * 3600);
const STALE_CACHE_DIR_AGE: Duration = Duration::from_secs(24 * 3600);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalEditSessionInfo {
    pub session_id: String,
    pub peer_id: String,
    pub peer_name: String,
    pub remote_path: String,
    pub local_path: PathBuf,
    pub file_name: String,
    pub baseline_blake3_hex: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum EditSyncOutcome {
    Uploaded {
        bytes_written: u64,
        new_blake3_hex: String,
    },
    ConflictDetected {
        baseline_blake3_hex: String,
        remote_blake3_hex: String,
    },
}

struct ManagedEditSession {
    info: LocalEditSessionInfo,
    last_synced_local_hash: String,
    auto_sync_approved: bool,
    last_activity: Instant,
    _watcher: Option<RecommendedWatcher>,
}

pub struct EditSyncManager {
    cache_root: PathBuf,
    sessions: Arc<Mutex<HashMap<String, ManagedEditSession>>>,
    events_tx: broadcast::Sender<CoreEvent>,
}

impl EditSyncManager {
    pub fn new(data_dir: &Path, events_tx: broadcast::Sender<CoreEvent>) -> Arc<Self> {
        let cache_root = data_dir.join("edit_cache");
        let _ = fs::create_dir_all(&cache_root);
        cleanup_stale_cache_dirs(&cache_root);
        Arc::new(Self {
            cache_root,
            sessions: Arc::new(Mutex::new(HashMap::new())),
            events_tx,
        })
    }

    pub fn close_session(&self, session_id: &str) {
        let removed = self
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session_id);
        if let Some(sess) = removed
            && let Some(dir) = sess.info.local_path.parent()
        {
            let _ = fs::remove_dir_all(dir);
        }
    }

    pub fn remove_peer_sessions(&self, peer_id: &str) {
        let mut to_remove_dirs = Vec::new();
        {
            let mut guard = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            guard.retain(|_, sess| {
                if sess.info.peer_id == peer_id {
                    if let Some(dir) = sess.info.local_path.parent() {
                        to_remove_dirs.push(dir.to_path_buf());
                    }
                    false
                } else {
                    true
                }
            });
        }
        for dir in to_remove_dirs {
            let _ = fs::remove_dir_all(dir);
        }
    }

    fn prune_expired_sessions(&self) {
        let mut evicted_dirs = Vec::new();
        {
            let mut guard = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            guard.retain(|_, sess| {
                if sess.last_activity.elapsed() >= EDIT_SESSION_TTL {
                    if let Some(dir) = sess.info.local_path.parent() {
                        evicted_dirs.push(dir.to_path_buf());
                    }
                    false
                } else {
                    true
                }
            });
            while guard.len() >= MAX_EDIT_SESSIONS {
                let oldest = guard
                    .iter()
                    .min_by_key(|(_, s)| s.last_activity)
                    .map(|(k, _)| k.clone());
                let Some(k) = oldest else { break };
                if let Some(sess) = guard.remove(&k)
                    && let Some(dir) = sess.info.local_path.parent()
                {
                    evicted_dirs.push(dir.to_path_buf());
                }
            }
        }
        for dir in evicted_dirs {
            let _ = fs::remove_dir_all(dir);
        }
    }

    pub fn shutdown(&self) {
        let dirs: Vec<PathBuf> = {
            let mut guard = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            let list = guard
                .values()
                .filter_map(|s| s.info.local_path.parent().map(Path::to_path_buf))
                .collect();
            guard.clear();
            list
        };
        for dir in dirs {
            let _ = fs::remove_dir_all(dir);
        }
    }
}

fn cleanup_stale_cache_dirs(cache_root: &Path) {
    let Ok(rd) = fs::read_dir(cache_root) else {
        return;
    };
    let now = SystemTime::now();
    for item in rd.flatten() {
        let path = item.path();
        if let Ok(meta) = item.metadata()
            && let Ok(mtime) = meta.modified()
            && now.duration_since(mtime).unwrap_or_default() >= STALE_CACHE_DIR_AGE
        {
            if meta.is_dir() {
                let _ = fs::remove_dir_all(&path);
            } else {
                let _ = fs::remove_file(&path);
            }
        }
    }
}

fn hash_file_blake3(path: &Path) -> std::io::Result<String> {
    let mut f = fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

async fn hash_file_blake3_async(path: PathBuf) -> Result<String, CoreError> {
    tokio::task::spawn_blocking(move || hash_file_blake3(&path))
        .await
        .map_err(|e| CoreError::Internal(format!("计算本地文件校验值异常：{e}")))?
        .map_err(CoreError::Io)
}

impl CoreNode {
    /// 任务 3.4 方式一：直接在对方电脑上用默认程序打开文件。
    pub async fn open_file_on_remote(
        self: &Arc<Self>,
        peer_id: &str,
        remote_path: &str,
    ) -> Result<pcshare_proto::AppLaunchReply, CoreError> {
        self.remote_launch(
            peer_id,
            pcshare_proto::AppLaunchArgs {
                target: remote_path.to_string(),
                kind: pcshare_proto::LaunchKind::PathOrTarget,
                args: None,
                skip_window_wait: false,
            },
        )
        .await
    }

    /// 任务 3.4 方式二：取回本机临时目录打开，并监听本地保存事件以便回传与检测冲突。
    pub async fn open_remote_file_locally(
        self: &Arc<Self>,
        peer_id: &str,
        remote_path: &str,
        launch_default_app: bool,
    ) -> Result<LocalEditSessionInfo, CoreError> {
        let peer_name = self
            .storage
            .get_peer(peer_id)
            .map(|p| p.name)
            .unwrap_or_else(|| "远端电脑".to_string());

        let baseline: FsHashReply = self
            .call_rpc(
                peer_id,
                pcshare_proto::Command::FsHash(FsHashArgs {
                    path: remote_path.to_string(),
                    length: None,
                }),
            )
            .await?;

        let file_name = remote_path
            .rsplit(['\\', '/'])
            .find(|s| !s.is_empty())
            .unwrap_or("remote_file")
            .to_string();
        let session_id = format!("edit-{:012x}", fastrand::u64(..));
        let session_dir = self.edit_sync_manager().cache_root.join(&session_id);
        tokio::fs::create_dir_all(&session_dir).await?;
        let local_path = session_dir.join(&file_name);

        self.download_file(
            peer_id,
            remote_path,
            &local_path,
            ConflictPolicy::Overwrite,
            CancellationToken::new(),
            None,
        )
        .await?;

        let local_hash = hash_file_blake3_async(local_path.clone()).await?;
        let info = LocalEditSessionInfo {
            session_id: session_id.clone(),
            peer_id: peer_id.to_string(),
            peer_name,
            remote_path: remote_path.to_string(),
            local_path: local_path.clone(),
            file_name,
            baseline_blake3_hex: baseline.blake3_hex,
        };

        self.edit_sync_manager().prune_expired_sessions();
        let watcher = self.spawn_edit_file_watcher(&info)?;
        {
            let mut guard = self
                .edit_sync_manager()
                .sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            guard.insert(
                session_id,
                ManagedEditSession {
                    info: info.clone(),
                    last_synced_local_hash: local_hash,
                    auto_sync_approved: false,
                    last_activity: Instant::now(),
                    _watcher: Some(watcher),
                },
            );
        }

        if launch_default_app {
            let local_str = local_path.to_string_lossy().to_string();
            self.platform
                .open_target(&local_str, None)
                .await
                .map_err(|e| CoreError::Rpc(map_platform_to_rpc(&e)))?;
        }

        Ok(info)
    }

    fn spawn_edit_file_watcher(
        self: &Arc<Self>,
        info: &LocalEditSessionInfo,
    ) -> Result<RecommendedWatcher, CoreError> {
        let weak_node: Weak<CoreNode> = Arc::downgrade(self);
        let session_id = info.session_id.clone();
        let watch_dir = info
            .local_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        let target_file = info.local_path.clone();
        let rt_handle = tokio::runtime::Handle::current();
        // 尾沿防抖代号：编辑器原子保存的多步写/改名事件合并到最后一次稳定 300ms 后触发
        let debounce_gen = Arc::new(AtomicU64::new(0));

        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let Ok(event) = res else { return };
            if matches!(event.kind, notify::EventKind::Access(_)) {
                return;
            }
            let my_gen = debounce_gen.fetch_add(1, Ordering::SeqCst) + 1;
            let gen_ref = debounce_gen.clone();
            let weak = weak_node.clone();
            let sid = session_id.clone();
            let file_p = target_file.clone();
            rt_handle.spawn(async move {
                tokio::time::sleep(EDIT_SAVE_DEBOUNCE).await;
                if gen_ref.load(Ordering::SeqCst) != my_gen {
                    return;
                }
                let Some(node) = weak.upgrade() else { return };
                node.on_local_edit_file_changed(&sid, &file_p).await;
            });
        })
        .map_err(|e| CoreError::Internal(format!("启动本地编辑监听失败：{e}")))?;

        watcher
            .watch(&watch_dir, RecursiveMode::NonRecursive)
            .map_err(|e| CoreError::Internal(format!("监听临时编辑目录失败：{e}")))?;
        Ok(watcher)
    }

    async fn on_local_edit_file_changed(self: &Arc<Self>, session_id: &str, file_path: &Path) {
        let Ok(current_hash) = hash_file_blake3_async(file_path.to_path_buf()).await else {
            return;
        };
        let (prompt, auto_approved) = {
            let mut guard = self
                .edit_sync_manager()
                .sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let Some(sess) = guard.get_mut(session_id) else {
                return;
            };
            sess.last_activity = Instant::now();
            if current_hash == sess.last_synced_local_hash {
                return;
            }
            (
                EditSyncPrompt {
                    session_id: sess.info.session_id.clone(),
                    peer_id: sess.info.peer_id.clone(),
                    peer_name: sess.info.peer_name.clone(),
                    remote_path: sess.info.remote_path.clone(),
                    local_path: sess.info.local_path.to_string_lossy().to_string(),
                    file_name: sess.info.file_name.clone(),
                    conflict: false,
                },
                sess.auto_sync_approved,
            )
        };

        if auto_approved {
            let _ = self.sync_back_edited_file(session_id, true, false).await;
        } else {
            let _ = self
                .edit_sync_manager()
                .events_tx
                .send(CoreEvent::EditSyncPromptNeeded(prompt));
        }
    }

    /// 将本地修改后的临时文件存回远端（回传前强制核对远端文件的 BLAKE3 校验值以防覆盖他人修改）。
    pub async fn sync_back_edited_file(
        self: &Arc<Self>,
        session_id: &str,
        remember_auto_sync: bool,
        force_overwrite_conflict: bool,
    ) -> Result<EditSyncOutcome, CoreError> {
        let info = {
            let mut guard = self
                .edit_sync_manager()
                .sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let sess = guard.get_mut(session_id).ok_or_else(|| {
                CoreError::Rpc(RpcError::new(
                    ErrorCode::NotFound,
                    "找不到对应的本地编辑会话",
                ))
            })?;
            sess.last_activity = Instant::now();
            if remember_auto_sync {
                sess.auto_sync_approved = true;
            }
            sess.info.clone()
        };

        let new_local_hash = hash_file_blake3_async(info.local_path.clone()).await?;

        // 回传前必须核对远端当前校验值；仅当明确 NotFound（远端已删除）时按新建/覆盖放行，
        // 遇到超时、断网、文件占用、权限不足等错误一律中止回传，绝不盲目覆盖。
        let remote_hash_res: Result<FsHashReply, CoreError> = self
            .call_rpc(
                &info.peer_id,
                pcshare_proto::Command::FsHash(FsHashArgs {
                    path: info.remote_path.clone(),
                    length: None,
                }),
            )
            .await;

        match remote_hash_res {
            Ok(remote_hash) => {
                if remote_hash.blake3_hex != info.baseline_blake3_hex && !force_overwrite_conflict {
                    let _ = self
                        .edit_sync_manager()
                        .events_tx
                        .send(CoreEvent::EditSyncPromptNeeded(EditSyncPrompt {
                            session_id: info.session_id.clone(),
                            peer_id: info.peer_id.clone(),
                            peer_name: info.peer_name.clone(),
                            remote_path: info.remote_path.clone(),
                            local_path: info.local_path.to_string_lossy().to_string(),
                            file_name: info.file_name.clone(),
                            conflict: true,
                        }));
                    return Ok(EditSyncOutcome::ConflictDetected {
                        baseline_blake3_hex: info.baseline_blake3_hex,
                        remote_blake3_hex: remote_hash.blake3_hex,
                    });
                }
            }
            Err(CoreError::Rpc(ref rpc)) if rpc.code == ErrorCode::NotFound => {}
            Err(err) => return Err(err),
        }

        let done = self
            .upload_file(
                &info.peer_id,
                &info.local_path,
                &info.remote_path,
                ConflictPolicy::Overwrite,
                CancellationToken::new(),
                None,
            )
            .await?;

        {
            let mut guard = self
                .edit_sync_manager()
                .sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(sess) = guard.get_mut(session_id) {
                sess.info.baseline_blake3_hex = new_local_hash.clone();
                sess.last_synced_local_hash = new_local_hash.clone();
                sess.last_activity = Instant::now();
            }
        }

        Ok(EditSyncOutcome::Uploaded {
            bytes_written: done.bytes_written,
            new_blake3_hex: new_local_hash,
        })
    }
}
