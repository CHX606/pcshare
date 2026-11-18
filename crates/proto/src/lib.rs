//! 电脑共享通信协议库（阶段 1）。
//!
//! 职责边界：只定义消息结构、帧编码解码、错误码、版本号与权限模型；不联网，不访问文件系统。

pub mod desktop;
pub mod error;
pub mod frame;
pub mod messages;

/// 主通信协议名（见总体方案 6.1 节）。
pub const ALPN_MAIN: &[u8] = b"pcshare/1";
/// 配对专用协议名（见总体方案 6.1 节）。
pub const ALPN_PAIR: &[u8] = b"pcshare/pair/1";
/// 两台受控电脑直传专用协议名（任务 2.4，由控制端签发一次性令牌授权）。
pub const ALPN_DIRECT: &[u8] = b"pcshare/direct/1";
/// 当前协议版本号。
pub const PROTOCOL_VERSION: u32 = 1;
/// 列目录单帧最大项数（见分阶段任务 1.7 节）。
pub const DEFAULT_LIST_BATCH_SIZE: usize = 2000;
/// 大目录首屏快速分批项数：500 项（见分阶段任务 2.3 节）。
pub const FIRST_LIST_BATCH_SIZE: usize = 500;
/// 小文件单流连续批传阈值：256 KB（见分阶段任务 2.4 节）。
pub const SMALL_FILE_BATCH_THRESHOLD: u64 = 256 * 1024;
/// 文件传输分块大小：64 KB（见分阶段任务 1.8 节）。
pub const TRANSFER_CHUNK_SIZE: usize = 64 * 1024;

pub use error::{ErrorCode, FrameError, RpcError};
pub use frame::{MAX_FRAME_SIZE, read_frame, read_optional_frame, write_frame, write_raw_frame};
pub use messages::*;
