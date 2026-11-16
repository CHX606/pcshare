//! 电脑共享核心层（阶段 1 + 阶段 2 + 阶段 3）。
//!
//! 职责边界：连接管理、设备配对、权限检查、本地存储、目录缓存与变化监听、文件操作与增强传输、
//! 局域网发现、软件与进程服务、网络体检与诊断日志、文件搜索、截图与点击控制、剪贴板互通、
//! 交互式伪终端、电源控制与网络唤醒、远端文件本地编辑回传、快捷启动与批量执行。
//! 不直接调用操作系统底层 API（统一经由 `pcshare-platform`），不依赖图形界面。

pub mod app_proc;
pub mod clipboard;
pub mod diagnostics;
pub mod dir_cache;
pub mod edit_sync;
pub mod error;
pub mod events;
pub mod fs_service;
pub mod fs_watch;
pub mod lan_discovery;
pub mod node;
pub mod pairing;
pub mod path_guard;
pub mod pinyin;
pub mod power_wol;
pub mod quick_batch;
pub mod screen_input;
pub mod search;
pub mod storage;
pub mod terminal;
pub mod transfer;

pub use clipboard::RemoteClipboardData;
pub use diagnostics::{DiagnosticPeerRoute, DiagnosticSummary, NetcheckReport, RelayLatencyItem};
pub use dir_cache::{CachedDir, DirCache, DirCacheDiff};
pub use edit_sync::{EditSyncManager, EditSyncOutcome, LocalEditSessionInfo};
pub use error::CoreError;
pub use events::{
    CoreEvent, DeviceConnState, EditSyncPrompt, PairingDecision, PairingRequestPrompt,
    PairingSessionTicket, ProgressCallback, TransferDirection, TransferProgressEvent,
};
pub use fs_watch::FsWatchManager;
pub use lan_discovery::{LanDiscovery, LanPeerAnnouncement};
pub use node::{CoreConfig, CoreNode, NetworkMode, node_capabilities};
pub use pairing::{ParsedInvite, format_invite_code, parse_invite_or_id};
pub use pinyin::{match_rank, to_pinyin_initials};
pub use power_wol::WakePeerReport;
pub use quick_batch::{
    BatchCommand, BatchDeviceResult, QuickHitKind, QuickSearchHit, parse_device_and_keyword,
};
pub use screen_input::{CapturedShot, InputResult};
pub use storage::{
    AuditEntry, LocalStorage, PeerRecord, RecentFolderItem, Settings, default_data_dir,
};
pub use terminal::{RemoteTerminalHandle, TerminalStreamEvent};
pub use transfer::{
    ProgressiveDirBatch, RateLimiter, TransferQueueOptions, TransferTaskControl, WireDeltaCallback,
};
