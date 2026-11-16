//! 任务 2.1：目录缓存（按“设备 + 规范化路径”缓存最近 200 个目录内容，退出时原子落盘，启动时加载，并计算增量差异）。

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::Utc;
use pcshare_proto::DirEntry;
use serde::{Deserialize, Serialize};

use crate::error::CoreError;
use crate::storage::write_json_atomic;

pub const MAX_DIR_CACHE_ENTRIES: usize = 200;
const DIR_CACHE_FILE: &str = "dir_cache.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedDir {
    pub device_id: String,
    pub path: String,
    pub entries: Vec<DirEntry>,
    pub cached_at_ms: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirCacheDiff {
    pub upserted: Vec<DirEntry>,
    pub removed: Vec<String>,
}

impl DirCacheDiff {
    pub fn has_changes(&self) -> bool {
        !self.upserted.is_empty() || !self.removed.is_empty()
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct PersistedDirCache {
    #[serde(default)]
    items: Vec<CachedDir>,
}

struct CacheInner {
    /// 最新访问排在队尾，最旧排在队首
    lru_order: VecDeque<String>,
    map: HashMap<String, CachedDir>,
}

pub struct DirCache {
    file_path: PathBuf,
    inner: Mutex<CacheInner>,
}

fn make_cache_key(device_id: &str, path: &str) -> String {
    let norm = path.trim().replace('/', "\\");
    let trimmed = if norm.len() > 3 {
        norm.trim_end_matches('\\').to_lowercase()
    } else {
        norm.to_lowercase()
    };
    format!("{}|{}", device_id.trim().to_lowercase(), trimmed)
}

impl DirCache {
    pub fn load_or_new(data_dir: &Path) -> Self {
        let file_path = data_dir.join(DIR_CACHE_FILE);
        let mut lru_order = VecDeque::new();
        let mut map = HashMap::new();

        if let Ok(raw) = std::fs::read_to_string(&file_path)
            && let Ok(persisted) = serde_json::from_str::<PersistedDirCache>(&raw)
        {
            for item in persisted.items {
                let key = make_cache_key(&item.device_id, &item.path);
                if map.contains_key(&key) {
                    lru_order.retain(|k| k != &key);
                }
                lru_order.push_back(key.clone());
                map.insert(key, item);
            }
            while lru_order.len() > MAX_DIR_CACHE_ENTRIES {
                if let Some(oldest) = lru_order.pop_front() {
                    map.remove(&oldest);
                }
            }
        }

        Self {
            file_path,
            inner: Mutex::new(CacheInner { lru_order, map }),
        }
    }

    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .map
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get(&self, device_id: &str, path: &str) -> Option<CachedDir> {
        let key = make_cache_key(device_id, path);
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let found = guard.map.get(&key).cloned()?;
        guard.lru_order.retain(|k| k != &key);
        guard.lru_order.push_back(key);
        Some(found)
    }

    pub fn put(&self, device_id: &str, path: &str, entries: Vec<DirEntry>) -> DirCacheDiff {
        let key = make_cache_key(device_id, path);
        let now_ms = Utc::now().timestamp_millis() as u64;
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());

        let diff = if let Some(prev) = guard.map.get(&key) {
            compute_dir_diff(&prev.entries, &entries)
        } else {
            DirCacheDiff {
                upserted: entries.clone(),
                removed: Vec::new(),
            }
        };

        let record = CachedDir {
            device_id: device_id.to_string(),
            path: path.to_string(),
            entries,
            cached_at_ms: now_ms,
        };

        guard.lru_order.retain(|k| k != &key);
        guard.lru_order.push_back(key.clone());
        guard.map.insert(key, record);

        while guard.lru_order.len() > MAX_DIR_CACHE_ENTRIES {
            if let Some(oldest) = guard.lru_order.pop_front() {
                guard.map.remove(&oldest);
            }
        }

        diff
    }

    pub fn put_silent(&self, device_id: &str, path: &str, entries: Vec<DirEntry>) {
        let key = make_cache_key(device_id, path);
        let now_ms = Utc::now().timestamp_millis() as u64;
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let record = CachedDir {
            device_id: device_id.to_string(),
            path: path.to_string(),
            entries,
            cached_at_ms: now_ms,
        };
        guard.lru_order.retain(|k| k != &key);
        guard.lru_order.push_back(key.clone());
        guard.map.insert(key, record);
        while guard.lru_order.len() > MAX_DIR_CACHE_ENTRIES {
            if let Some(oldest) = guard.lru_order.pop_front() {
                guard.map.remove(&oldest);
            }
        }
    }

    pub fn invalidate(&self, device_id: &str, path: &str) {
        let key = make_cache_key(device_id, path);
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.lru_order.retain(|k| k != &key);
        guard.map.remove(&key);
    }

    pub fn flush(&self) -> Result<(), CoreError> {
        let items: Vec<CachedDir> = {
            let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            guard
                .lru_order
                .iter()
                .filter_map(|k| guard.map.get(k).cloned())
                .collect()
        };
        write_json_atomic(&self.file_path, &PersistedDirCache { items })
    }
}

pub fn compute_dir_diff(old_entries: &[DirEntry], new_entries: &[DirEntry]) -> DirCacheDiff {
    let old_map: HashMap<&str, &DirEntry> =
        old_entries.iter().map(|e| (e.name.as_str(), e)).collect();
    let new_names: std::collections::HashSet<&str> =
        new_entries.iter().map(|e| e.name.as_str()).collect();

    let mut upserted = Vec::new();
    for entry in new_entries {
        match old_map.get(entry.name.as_str()) {
            Some(&old)
                if old.kind == entry.kind
                    && old.size == entry.size
                    && old.modified_ms == entry.modified_ms
                    && old.readonly == entry.readonly => {}
            _ => upserted.push(entry.clone()),
        }
    }

    let mut removed = Vec::new();
    for old in old_entries {
        if !new_names.contains(old.name.as_str()) {
            removed.push(old.name.clone());
        }
    }

    DirCacheDiff { upserted, removed }
}
