//! 任务 2.2：目录变化监听与防抖合并推送
//! - 每条连接最多同时监听 20 个目录（超出时自动移出该连接最早监听的目录）
//! - 目录内发生增删改后攒 200 毫秒合并推送一次 `RemoteEvent::DirChanged`
//! - 控制端离开目录或断开连接时自动解除监听

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use pcshare_proto::{
    ErrorCode, FsUnwatchReply, FsWatchReply, Permissions, RemoteEvent, RpcError,
};
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::events::CoreEvent;
use crate::path_guard::{resolve_authorized_path, to_display_path};

pub const MAX_WATCHES_PER_PEER: usize = 20;
const WATCH_DEBOUNCE: Duration = Duration::from_millis(200);

struct WatchInner {
    watcher: Option<RecommendedWatcher>,
    /// `peer_id -> VecDeque<norm_key>`（最多 20 个）
    peer_dirs: HashMap<String, VecDeque<String>>,
    /// `norm_key -> (fs_path, display_path, HashSet<peer_id>)`
    watched_paths: HashMap<String, (PathBuf, String, HashSet<String>)>,
}

pub struct FsWatchManager {
    inner: Arc<Mutex<WatchInner>>,
    shutdown: CancellationToken,
}

fn norm_key(clean: &Path) -> String {
    to_display_path(clean).to_lowercase()
}

impl FsWatchManager {
    pub fn new(
        remote_events_tx: broadcast::Sender<(String, RemoteEvent)>,
        core_events_tx: broadcast::Sender<CoreEvent>,
    ) -> Arc<Self> {
        let (raw_tx, mut raw_rx) = mpsc::unbounded_channel::<PathBuf>();
        let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(ev) = res {
                // 只关心创建、删除、修改、重命名，忽略纯访问事件
                if matches!(
                    ev.kind,
                    notify::EventKind::Create(_)
                        | notify::EventKind::Modify(_)
                        | notify::EventKind::Remove(_)
                ) {
                    for p in ev.paths {
                        let _ = raw_tx.send(p);
                    }
                }
            }
        })
        .ok();

        let inner = Arc::new(Mutex::new(WatchInner {
            watcher,
            peer_dirs: HashMap::new(),
            watched_paths: HashMap::new(),
        }));
        let shutdown = CancellationToken::new();

        let inner_bg = inner.clone();
        let cancel_bg = shutdown.clone();
        tokio::spawn(async move {
            let mut pending: HashMap<String, Instant> = HashMap::new();
            let mut tick = tokio::time::interval(Duration::from_millis(50));
            loop {
                tokio::select! {
                    _ = cancel_bg.cancelled() => break,
                    maybe_path = raw_rx.recv() => {
                        let Some(changed_path) = maybe_path else { break; };
                        let matched_keys = match_watched_keys(&inner_bg, &changed_path);
                        let due = Instant::now() + WATCH_DEBOUNCE;
                        for k in matched_keys {
                            pending.entry(k).or_insert(due);
                        }
                    }
                    _ = tick.tick() => {
                        if pending.is_empty() {
                            continue;
                        }
                        let now = Instant::now();
                        let ready: Vec<String> = pending
                            .iter()
                            .filter_map(|(k, &due)| if now >= due { Some(k.clone()) } else { None })
                            .collect();
                        for k in ready {
                            pending.remove(&k);
                            let targets = {
                                let guard = inner_bg.lock().unwrap_or_else(|e| e.into_inner());
                                guard
                                    .watched_paths
                                    .get(&k)
                                    .map(|(_, disp, peers)| (disp.clone(), peers.iter().cloned().collect::<Vec<_>>()))
                            };
                            if let Some((disp_path, peers)) = targets {
                                for peer_id in peers {
                                    if peer_id == "self" {
                                        let _ = core_events_tx.send(CoreEvent::RemoteDirChanged {
                                            peer_id: "self".to_string(),
                                            path: disp_path.clone(),
                                        });
                                    } else {
                                        let _ = remote_events_tx.send((
                                            peer_id,
                                            RemoteEvent::DirChanged {
                                                path: disp_path.clone(),
                                            },
                                        ));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        });

        Arc::new(Self { inner, shutdown })
    }

    pub fn watch(
        &self,
        peer_id: &str,
        perms: &Permissions,
        raw_path: &str,
    ) -> Result<FsWatchReply, RpcError> {
        if !perms.fs_read {
            return Err(RpcError::new(ErrorCode::Forbidden, "没有读取文件夹的权限"));
        }
        let (clean, fs_path) = resolve_authorized_path(raw_path, perms)?;
        if !fs_path.is_dir() {
            return Err(RpcError::new(
                ErrorCode::NotFound,
                format!("要监听的文件夹不存在：{}", to_display_path(&clean)),
            ));
        }

        let key = norm_key(&clean);
        let disp = to_display_path(&clean);
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());

        {
            let queue = guard.peer_dirs.entry(peer_id.to_string()).or_default();
            if queue.iter().any(|k| k == &key) {
                queue.retain(|k| k != &key);
                queue.push_back(key);
                return Ok(FsWatchReply {
                    watched_count: queue.len(),
                });
            }
        }

        // 若该连接已满 20 个目录，先移出最早登记的一个
        let evicted_opt = {
            let queue = guard.peer_dirs.entry(peer_id.to_string()).or_default();
            if queue.len() >= MAX_WATCHES_PER_PEER {
                queue.pop_front()
            } else {
                None
            }
        };
        if let Some(evicted_key) = evicted_opt {
            remove_peer_from_key(&mut guard, peer_id, &evicted_key);
        }

        let need_os_watch = !guard.watched_paths.contains_key(&key);
        if need_os_watch
            && let Some(ref mut w) = guard.watcher
        {
            let _ = w.watch(&fs_path, RecursiveMode::NonRecursive);
        }

        guard
            .watched_paths
            .entry(key.clone())
            .or_insert_with(|| (fs_path, disp, HashSet::new()))
            .2
            .insert(peer_id.to_string());

        let queue = guard.peer_dirs.entry(peer_id.to_string()).or_default();
        queue.push_back(key);
        let count = queue.len();
        Ok(FsWatchReply {
            watched_count: count,
        })
    }

    pub fn unwatch(
        &self,
        peer_id: &str,
        perms: &Permissions,
        raw_path: &str,
    ) -> Result<FsUnwatchReply, RpcError> {
        let (clean, _) = resolve_authorized_path(raw_path, perms)?;
        let key = norm_key(&clean);
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let count = if let Some(queue) = guard.peer_dirs.get_mut(peer_id) {
            queue.retain(|k| k != &key);
            queue.len()
        } else {
            0
        };
        remove_peer_from_key(&mut guard, peer_id, &key);
        Ok(FsUnwatchReply {
            watched_count: count,
        })
    }

    pub fn unwatch_all_for_peer(&self, peer_id: &str) {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(keys) = guard.peer_dirs.remove(peer_id) {
            for k in keys {
                remove_peer_from_key(&mut guard, peer_id, &k);
            }
        }
    }

    pub fn shutdown(&self) {
        self.shutdown.cancel();
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.peer_dirs.clear();
        guard.watched_paths.clear();
        guard.watcher = None;
    }
}

fn remove_peer_from_key(guard: &mut WatchInner, peer_id: &str, key: &str) {
    let mut should_unwatch_fs: Option<PathBuf> = None;
    if let Some((fs_path, _, peers)) = guard.watched_paths.get_mut(key) {
        peers.remove(peer_id);
        if peers.is_empty() {
            should_unwatch_fs = Some(fs_path.clone());
        }
    }
    if let Some(fs_path) = should_unwatch_fs {
        guard.watched_paths.remove(key);
        if let Some(ref mut w) = guard.watcher {
            let _ = w.unwatch(&fs_path);
        }
    }
}

fn match_watched_keys(inner: &Mutex<WatchInner>, changed_path: &Path) -> Vec<String> {
    let raw = changed_path.to_string_lossy();
    let stripped = raw
        .strip_prefix(r"\\?\UNC\")
        .map(|s| format!(r"\\{s}"))
        .or_else(|| raw.strip_prefix(r"\\?\").map(|s| s.to_string()))
        .unwrap_or_else(|| raw.into_owned());
    let clean_changed = PathBuf::from(&stripped);
    let parent_opt = clean_changed.parent().map(|p| {
        let s = p.to_string_lossy().to_lowercase();
        if s.len() > 3 {
            s.trim_end_matches('\\').to_string()
        } else {
            s
        }
    });
    let self_lower = {
        let s = stripped.to_lowercase();
        if s.len() > 3 {
            s.trim_end_matches('\\').to_string()
        } else {
            s
        }
    };

    let guard = inner.lock().unwrap_or_else(|e| e.into_inner());
    let mut out = Vec::new();
    for k in guard.watched_paths.keys() {
        let k_norm = if k.len() > 3 {
            k.trim_end_matches('\\')
        } else {
            k.as_str()
        };
        if k_norm == self_lower || parent_opt.as_deref() == Some(k_norm) {
            out.push(k.clone());
        }
    }
    out
}
