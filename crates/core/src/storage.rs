//! 本地持久化存储（任务 1.4）：
//! - 密钥用平台数据保护接口（Windows DPAPI）加密保存
//! - 已配对设备表与设置文件全部采用“先写临时文件 + sync + 原子改名”
//! - 文件损坏时自动备份为 `.corrupt.<timestamp>.bak`，重建默认文件并返回中文提示
//! - 操作审计日志按天滚动，保留最近 30 天

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use chrono::{Duration as ChronoDuration, Local, NaiveDate, Utc};
use iroh::{EndpointId, SecretKey};
use pcshare_platform::Platform;
use pcshare_proto::Permissions;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::error::CoreError;

const IDENTITY_FILE: &str = "identity.dpapi";
const PEERS_FILE: &str = "peers.json";
const SETTINGS_FILE: &str = "settings.json";
const LOGS_DIR: &str = "logs";
const AUDIT_RETENTION_DAYS: i64 = 30;

fn default_true() -> bool {
    true
}

fn default_quick_hotkey() -> String {
    "Alt+Space".to_string()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentFolderItem {
    pub peer_id: String,
    pub peer_name: String,
    pub path: String,
    pub name: String,
    pub opened_at_ms: u64,
}

/// 默认数据目录：`%APPDATA%\pcshare`（非 Windows 回退到用户主目录 `.pcshare`）。
pub fn default_data_dir() -> PathBuf {
    if let Some(appdata) = std::env::var_os("APPDATA") {
        return PathBuf::from(appdata).join("pcshare");
    }
    if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
        return PathBuf::from(home).join(".pcshare");
    }
    PathBuf::from(".pcshare")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    pub device_name: String,
    #[serde(default)]
    pub custom_relay_urls: Vec<String>,
    #[serde(default)]
    pub autostart: bool,
    #[serde(default)]
    pub autostart_elevated: bool,
    #[serde(default)]
    pub autostart_prompted: bool,
    #[serde(default = "default_true")]
    pub portmap_enabled: bool,
    /// 是否优先将 IPv4 传输直绑到具备真实默认网关的物理网卡（开启代理 TUN 时可绕开虚拟网卡转发损耗）
    #[serde(default)]
    pub bind_physical_nic: bool,
    #[serde(default = "default_quick_hotkey")]
    pub quick_hotkey: String,
    #[serde(default)]
    pub clipboard_auto_sync: bool,
    #[serde(default)]
    pub recent_folders: Vec<RecentFolderItem>,
}

impl Settings {
    pub fn default_with_hostname(hostname: String) -> Self {
        Self {
            device_name: hostname,
            custom_relay_urls: Vec::new(),
            autostart: false,
            autostart_elevated: false,
            autostart_prompted: false,
            portmap_enabled: true,
            bind_physical_nic: false,
            quick_hotkey: default_quick_hotkey(),
            clipboard_auto_sync: false,
            recent_folders: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerRecord {
    pub endpoint_id: String,
    pub name: String,
    #[serde(default)]
    pub os: Option<String>,
    #[serde(default)]
    pub permissions: Permissions,
    /// 是否允许对方作为控制端连接并操作本机（按 `permissions` 执行检查）
    #[serde(default = "default_true")]
    pub allow_incoming: bool,
    /// 本机是否主动维持到对方的连接
    #[serde(default = "default_true")]
    pub connect_outgoing: bool,
    #[serde(default)]
    pub relay_url: Option<String>,
    #[serde(default)]
    pub direct_addrs: Vec<SocketAddr>,
    #[serde(default)]
    pub mac_addresses: Vec<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    pub paired_at_ms: u64,
    #[serde(default)]
    pub last_seen_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PeersFile {
    #[serde(default)]
    peers: Vec<PeerRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEntry {
    pub timestamp_ms: u64,
    pub peer_id: String,
    pub peer_name: String,
    pub action: String,
    pub detail: String,
    pub ok: bool,
}

pub struct LocalStorage {
    data_dir: PathBuf,
    platform: Arc<dyn Platform>,
    secret_key: SecretKey,
    peers: RwLock<Vec<PeerRecord>>,
    peers_io_lock: Mutex<()>,
    settings: RwLock<Settings>,
    last_audit_prune_day: Mutex<Option<NaiveDate>>,
}

impl LocalStorage {
    /// 打开或初始化本地存储，返回 `(LocalStorage, 启动修复提示列表)`。
    pub fn open(
        data_dir: impl AsRef<Path>,
        platform: Arc<dyn Platform>,
    ) -> Result<(Self, Vec<String>), CoreError> {
        let data_dir = data_dir.as_ref().to_path_buf();
        fs::create_dir_all(&data_dir)?;
        fs::create_dir_all(data_dir.join(LOGS_DIR))?;

        let mut warnings = Vec::new();
        let (hostname, _) = platform.host_and_user();

        // 1. 加载或重建设备密钥
        let id_path = data_dir.join(IDENTITY_FILE);
        let secret_key = match load_secret_key(&id_path, platform.as_ref()) {
            Ok(Some(key)) => key,
            Ok(None) => {
                let key = SecretKey::generate();
                save_secret_key(&id_path, &key, platform.as_ref())?;
                key
            }
            Err(reason) => {
                let backup = backup_corrupt_file(&id_path)?;
                let key = SecretKey::generate();
                save_secret_key(&id_path, &key, platform.as_ref())?;
                warnings.push(format!(
                    "本机身份密钥文件已损坏（{reason}），已备份为 {} 并重新生成新的身份码，之前配对过的设备需要重新配对。",
                    backup.file_name().unwrap_or_default().to_string_lossy()
                ));
                key
            }
        };

        // 2. 加载或重建已配对设备表
        let peers_path = data_dir.join(PEERS_FILE);
        let peers_file: PeersFile = load_or_rebuild_json(
            &peers_path,
            PeersFile::default,
            "已配对设备列表",
            &mut warnings,
        )?;

        // 3. 加载或重建设置文件
        let settings_path = data_dir.join(SETTINGS_FILE);
        let default_hostname = hostname.clone();
        let settings: Settings = load_or_rebuild_json(
            &settings_path,
            move || Settings::default_with_hostname(default_hostname),
            "设置文件",
            &mut warnings,
        )?;

        let storage = Self {
            data_dir,
            platform,
            secret_key,
            peers: RwLock::new(peers_file.peers),
            peers_io_lock: Mutex::new(()),
            settings: RwLock::new(settings),
            last_audit_prune_day: Mutex::new(Some(Local::now().date_naive())),
        };
        let _ = storage.cleanup_expired_audits();
        Ok((storage, warnings))
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn platform(&self) -> &Arc<dyn Platform> {
        &self.platform
    }

    pub fn secret_key(&self) -> &SecretKey {
        &self.secret_key
    }

    pub fn endpoint_id(&self) -> EndpointId {
        self.secret_key.public()
    }

    pub fn settings(&self) -> Settings {
        self.settings
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn update_settings<F>(&self, f: F) -> Result<Settings, CoreError>
    where
        F: FnOnce(&mut Settings),
    {
        let mut guard = self.settings.write().unwrap_or_else(|e| e.into_inner());
        let mut next = guard.clone();
        f(&mut next);
        write_json_atomic(&self.data_dir.join(SETTINGS_FILE), &next)?;
        *guard = next.clone();
        Ok(next)
    }

    pub fn list_peers(&self) -> Vec<PeerRecord> {
        self.peers.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn get_peer(&self, endpoint_id: &str) -> Option<PeerRecord> {
        let guard = self.peers.read().unwrap_or_else(|e| e.into_inner());
        guard.iter().find(|p| p.endpoint_id == endpoint_id).cloned()
    }

    pub fn upsert_peer(&self, record: PeerRecord) -> Result<(), CoreError> {
        let _io = self.peers_io_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut guard = self.peers.write().unwrap_or_else(|e| e.into_inner());
        let mut next = guard.clone();
        if let Some(existing) = next
            .iter_mut()
            .find(|p| p.endpoint_id == record.endpoint_id)
        {
            *existing = record;
        } else {
            next.push(record);
        }
        write_json_atomic(
            &self.data_dir.join(PEERS_FILE),
            &PeersFile {
                peers: next.clone(),
            },
        )?;
        *guard = next;
        Ok(())
    }

    /// 仅在内存中更新对端最近在线时间与中继地址（不写盘，避免高频心跳磨损磁盘）。
    /// 返回 `(是否找到该对端, relay_url 是否发生变化)`。
    pub fn touch_peer_seen_in_memory(
        &self,
        endpoint_id: &str,
        seen_ms: u64,
        relay_url: Option<String>,
    ) -> (bool, bool) {
        let mut guard = self.peers.write().unwrap_or_else(|e| e.into_inner());
        if let Some(peer) = guard.iter_mut().find(|p| p.endpoint_id == endpoint_id) {
            peer.last_seen_ms = Some(seen_ms);
            let mut relay_changed = false;
            if let Some(url) = relay_url
                && peer.relay_url.as_deref() != Some(url.as_str())
            {
                peer.relay_url = Some(url);
                relay_changed = true;
            }
            (true, relay_changed)
        } else {
            (false, false)
        }
    }

    /// 将当前内存中的已配对设备表快照原子落盘（不持 `peers` 写锁执行 `sync_all`）。
    pub fn flush_peers(&self) -> Result<(), CoreError> {
        let _io = self.peers_io_lock.lock().unwrap_or_else(|e| e.into_inner());
        let snapshot = self.peers.read().unwrap_or_else(|e| e.into_inner()).clone();
        write_json_atomic(&self.data_dir.join(PEERS_FILE), &PeersFile { peers: snapshot })
    }

    pub fn touch_peer_seen(
        &self,
        endpoint_id: &str,
        seen_ms: u64,
        relay_url: Option<String>,
    ) -> Result<(), CoreError> {
        let _io = self.peers_io_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut guard = self.peers.write().unwrap_or_else(|e| e.into_inner());
        let mut next = guard.clone();
        if let Some(peer) = next.iter_mut().find(|p| p.endpoint_id == endpoint_id) {
            peer.last_seen_ms = Some(seen_ms);
            if relay_url.is_some() {
                peer.relay_url = relay_url;
            }
            write_json_atomic(
                &self.data_dir.join(PEERS_FILE),
                &PeersFile {
                    peers: next.clone(),
                },
            )?;
            *guard = next;
        }
        Ok(())
    }

    pub fn update_peer_hello_meta(
        &self,
        endpoint_id: &str,
        capabilities: Vec<String>,
        mac_addresses: Vec<String>,
    ) -> Result<(), CoreError> {
        let _io = self.peers_io_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut guard = self.peers.write().unwrap_or_else(|e| e.into_inner());
        let mut next = guard.clone();
        if let Some(peer) = next.iter_mut().find(|p| p.endpoint_id == endpoint_id) {
            let mut changed = false;
            if !capabilities.is_empty() && peer.capabilities != capabilities {
                peer.capabilities = capabilities;
                changed = true;
            }
            if !mac_addresses.is_empty() && peer.mac_addresses != mac_addresses {
                peer.mac_addresses = mac_addresses;
                changed = true;
            }
            if changed {
                write_json_atomic(
                    &self.data_dir.join(PEERS_FILE),
                    &PeersFile {
                        peers: next.clone(),
                    },
                )?;
                *guard = next;
            }
        }
        Ok(())
    }

    pub fn rename_peer(&self, endpoint_id: &str, new_name: String) -> Result<bool, CoreError> {
        let _io = self.peers_io_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut guard = self.peers.write().unwrap_or_else(|e| e.into_inner());
        let mut next = guard.clone();
        let Some(peer) = next.iter_mut().find(|p| p.endpoint_id == endpoint_id) else {
            return Ok(false);
        };
        peer.name = new_name;
        write_json_atomic(
            &self.data_dir.join(PEERS_FILE),
            &PeersFile {
                peers: next.clone(),
            },
        )?;
        *guard = next;
        Ok(true)
    }

    pub fn update_peer_permissions(
        &self,
        endpoint_id: &str,
        permissions: Permissions,
    ) -> Result<bool, CoreError> {
        let _io = self.peers_io_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut guard = self.peers.write().unwrap_or_else(|e| e.into_inner());
        let mut next = guard.clone();
        let Some(peer) = next.iter_mut().find(|p| p.endpoint_id == endpoint_id) else {
            return Ok(false);
        };
        peer.permissions = permissions;
        write_json_atomic(
            &self.data_dir.join(PEERS_FILE),
            &PeersFile {
                peers: next.clone(),
            },
        )?;
        *guard = next;
        Ok(true)
    }

    pub fn remove_peer(&self, endpoint_id: &str) -> Result<bool, CoreError> {
        let _io = self.peers_io_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut guard = self.peers.write().unwrap_or_else(|e| e.into_inner());
        let mut next = guard.clone();
        let before = next.len();
        next.retain(|p| p.endpoint_id != endpoint_id);
        if next.len() == before {
            return Ok(false);
        }
        write_json_atomic(
            &self.data_dir.join(PEERS_FILE),
            &PeersFile {
                peers: next.clone(),
            },
        )?;
        *guard = next;
        Ok(true)
    }

    /// 追加一条按天滚动的操作记录（见总体方案 7.4 节）。
    pub fn logs_dir(&self) -> PathBuf {
        self.data_dir.join(LOGS_DIR)
    }

    pub fn append_audit(&self, entry: AuditEntry) -> Result<(), CoreError> {
        let logs_dir = self.data_dir.join(LOGS_DIR);
        fs::create_dir_all(&logs_dir)?;
        let today = Local::now().date_naive();
        let date_str = today.format("%Y-%m-%d");
        let log_path = logs_dir.join(format!("audit-{date_str}.jsonl"));
        let mut line = serde_json::to_string(&entry)
            .map_err(|e| CoreError::Internal(format!("序列化审计记录失败：{e}")))?;
        line.push('\n');
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)?;
        file.write_all(line.as_bytes())?;

        // 跨天运行时每日最多清理一次 30 天前的过期审计日志
        let should_prune = {
            let mut last = self
                .last_audit_prune_day
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if *last == Some(today) {
                false
            } else {
                *last = Some(today);
                true
            }
        };
        if should_prune {
            let _ = self.cleanup_expired_audits();
        }
        Ok(())
    }

    /// 读取最近 `limit` 条操作审计记录（按时间倒序）。
    pub fn read_recent_audits(&self, limit: usize) -> Result<Vec<AuditEntry>, CoreError> {
        let logs_dir = self.data_dir.join(LOGS_DIR);
        if !logs_dir.exists() || limit == 0 {
            return Ok(Vec::new());
        }
        let mut files: Vec<PathBuf> = fs::read_dir(&logs_dir)?
            .filter_map(|e| e.ok().map(|item| item.path()))
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("audit-") && n.ends_with(".jsonl"))
            })
            .collect();
        files.sort();
        files.reverse();

        let mut out = Vec::with_capacity(limit.min(512));
        for path in files {
            let need = limit.saturating_sub(out.len());
            if need == 0 {
                break;
            }
            let file = File::open(path)?;
            let mut ring: VecDeque<AuditEntry> = VecDeque::with_capacity(need.min(512));
            for line in BufReader::new(file).lines().map_while(Result::ok) {
                if let Ok(entry) = serde_json::from_str::<AuditEntry>(&line) {
                    if ring.len() == need {
                        ring.pop_front();
                    }
                    ring.push_back(entry);
                }
            }
            for entry in ring.into_iter().rev() {
                out.push(entry);
                if out.len() >= limit {
                    return Ok(out);
                }
            }
        }
        Ok(out)
    }

    fn cleanup_expired_audits(&self) -> Result<(), CoreError> {
        let logs_dir = self.data_dir.join(LOGS_DIR);
        if !logs_dir.exists() {
            return Ok(());
        }
        let cutoff = Local::now().date_naive() - ChronoDuration::days(AUDIT_RETENTION_DAYS);
        for item in fs::read_dir(&logs_dir)?.flatten() {
            let name = item.file_name();
            let Some(name_str) = name.to_str() else {
                continue;
            };
            if let Some(date_part) = name_str
                .strip_prefix("audit-")
                .and_then(|s| s.strip_suffix(".jsonl"))
                && let Ok(file_date) = NaiveDate::parse_from_str(date_part, "%Y-%m-%d")
                && file_date < cutoff
            {
                let _ = fs::remove_file(item.path());
            }
        }
        Ok(())
    }
}

fn load_secret_key(path: &Path, platform: &dyn Platform) -> Result<Option<SecretKey>, String> {
    match fs::read(path) {
        Ok(ciphertext) => {
            let plain = platform
                .unprotect_secret(&ciphertext)
                .map_err(|e| e.to_string())?;
            let bytes: [u8; 32] = plain
                .try_into()
                .map_err(|_| "解密后的密钥长度不是 32 字节".to_string())?;
            Ok(Some(SecretKey::from_bytes(&bytes)))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(format!("读取密钥文件出错：{err}")),
    }
}

fn save_secret_key(
    path: &Path,
    key: &SecretKey,
    platform: &dyn Platform,
) -> Result<(), CoreError> {
    let raw = key.to_bytes();
    let encrypted = platform.protect_secret(&raw)?;
    write_bytes_atomic(path, &encrypted)
}

fn load_or_rebuild_json<T, F>(
    path: &Path,
    make_default: F,
    label: &str,
    warnings: &mut Vec<String>,
) -> Result<T, CoreError>
where
    T: Serialize + DeserializeOwned,
    F: FnOnce() -> T,
{
    match fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<T>(&text) {
            Ok(val) => Ok(val),
            Err(err) => {
                let backup = backup_corrupt_file(path)?;
                let fresh = make_default();
                write_json_atomic(path, &fresh)?;
                warnings.push(format!(
                    "{label}已损坏（{err}），已自动备份为 {} 并恢复默认配置。",
                    backup.file_name().unwrap_or_default().to_string_lossy()
                ));
                Ok(fresh)
            }
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let fresh = make_default();
            write_json_atomic(path, &fresh)?;
            Ok(fresh)
        }
        Err(err) => Err(CoreError::Io(err)),
    }
}

fn backup_corrupt_file(path: &Path) -> Result<PathBuf, CoreError> {
    let ts = Utc::now().timestamp_millis();
    let file_name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let backup = path.with_file_name(format!("{file_name}.corrupt.{ts}.bak"));
    fs::rename(path, &backup)?;
    Ok(backup)
}

pub fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), CoreError> {
    let json = serde_json::to_vec_pretty(value)
        .map_err(|e| CoreError::Internal(format!("序列化配置失败：{e}")))?;
    write_bytes_atomic(path, &json)
}

/// 紧凑 JSON 原子落盘（用于目录缓存等体积敏感的大 JSON 文件）。
pub fn write_json_compact_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), CoreError> {
    let json = serde_json::to_vec(value)
        .map_err(|e| CoreError::Internal(format!("序列化缓存失败：{e}")))?;
    write_bytes_atomic(path, &json)
}

pub fn write_bytes_atomic(path: &Path, bytes: &[u8]) -> Result<(), CoreError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file_name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let tmp_path = path.with_file_name(format!(".{file_name}.{}.tmp", fastrand::u64(..)));
    {
        let mut file = File::create(&tmp_path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    if let Err(err) = fs::rename(&tmp_path, path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(CoreError::Io(err));
    }
    Ok(())
}
