use std::sync::Arc;

use pcshare_proto::{ClipKind, DirectTransferProgress, FsTaskProgress, Permissions};
use serde::{Deserialize, Serialize};

/// 进度回调函数签名：`(已传字节数, 总字节数, 每秒字节数)`。
pub type ProgressCallback = Arc<dyn Fn(u64, u64, f64) + Send + Sync>;

/// 单台已配对设备的实时连接状态（见分阶段任务 1.5 节）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DeviceConnState {
    Offline {
        #[serde(default)]
        reason: Option<String>,
    },
    Connecting,
    OnlineRelay {
        relay_url: String,
        rtt_ms: u64,
    },
    OnlineDirect {
        addr: String,
        rtt_ms: u64,
    },
}

/// 被控端开启配对后生成的会话凭据。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingSessionTicket {
    pub endpoint_id: String,
    pub code: String,
    pub expires_at_ms: u64,
    pub invite_code: String,
}

/// 收到远端配对请求、等待用户在被控电脑前点击允许或拒绝。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingRequestPrompt {
    pub request_id: String,
    pub peer_id: String,
    /// 身份码后 8 位（见总体方案 7.2 节）
    pub peer_short_id: String,
    pub peer_name: String,
    pub peer_os: String,
    pub mutual: bool,
    pub expires_at_ms: u64,
}

/// 用户对配对请求的确认结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairingDecision {
    Approve {
        granted_permissions: Permissions,
        accept_mutual: bool,
    },
    Reject,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferDirection {
    Upload,
    Download,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TransferProgressEvent {
    pub peer_id: String,
    pub direction: TransferDirection,
    pub local_path: String,
    pub remote_path: String,
    pub transferred_bytes: u64,
    pub total_bytes: u64,
    pub bytes_per_sec: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditSyncPrompt {
    pub session_id: String,
    pub peer_id: String,
    pub peer_name: String,
    pub remote_path: String,
    pub local_path: String,
    pub file_name: String,
    pub conflict: bool,
}

/// 核心层向界面或命令行广播的事件。
#[derive(Debug, Clone)]
pub enum CoreEvent {
    StorageWarning {
        message: String,
    },
    DeviceStateChanged {
        peer_id: String,
        state: DeviceConnState,
    },
    PeersUpdated,
    PairingApprovalNeeded(PairingRequestPrompt),
    RemoteTaskProgress {
        peer_id: String,
        progress: FsTaskProgress,
    },
    RemoteDirChanged {
        peer_id: String,
        path: String,
    },
    RemoteDirectTransferProgress {
        peer_id: String,
        progress: DirectTransferProgress,
    },
    LanPeersChanged,
    HostPausedChanged {
        paused: bool,
    },
    TransferProgress(TransferProgressEvent),
    EditSyncPromptNeeded(EditSyncPrompt),
    ClipboardSynced {
        peer_id: String,
        kind: ClipKind,
    },
}
