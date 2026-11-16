use serde::{Deserialize, Serialize};
use thiserror::Error;

/// 协议标准错误码（见总体方案 6.3 节）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// 未配对或配对失效
    Unauthorized,
    /// 权限不够或越出开放目录范围
    Forbidden,
    /// 目标不存在
    NotFound,
    /// 目标已存在
    AlreadyExists,
    /// 文件正在被占用
    Busy,
    /// 参数不合法
    Invalid,
    /// 对方版本不认识该指令或协议版本
    Unsupported,
    /// 读写错误
    Io,
    /// 操作已取消
    Cancelled,
    /// 内部错误
    Internal,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unauthorized => "unauthorized",
            Self::Forbidden => "forbidden",
            Self::NotFound => "not_found",
            Self::AlreadyExists => "already_exists",
            Self::Busy => "busy",
            Self::Invalid => "invalid",
            Self::Unsupported => "unsupported",
            Self::Io => "io",
            Self::Cancelled => "cancelled",
            Self::Internal => "internal",
        }
    }
}

/// 协议回复里的错误详情。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Error)]
#[error("[{code:?}] {msg}")]
pub struct RpcError {
    pub code: ErrorCode,
    pub msg: String,
}

impl RpcError {
    pub fn new(code: ErrorCode, msg: impl Into<String>) -> Self {
        Self {
            code,
            msg: msg.into(),
        }
    }
}

/// 帧读写层的错误。
#[derive(Debug, Error)]
pub enum FrameError {
    #[error("单帧数据超过上限（当前 {size} 字节，上限 {max} 字节）")]
    FrameTooLarge { size: usize, max: usize },

    #[error("对方尚未发送任何数据就关闭了数据流")]
    UnexpectedEof,

    #[error("收到不完整的半截数据帧（期望 {expected} 字节，实际读到 {got} 字节）")]
    TruncatedFrame { expected: usize, got: usize },

    #[error("消息编码或解码失败：{0}")]
    Json(#[from] serde_json::Error),

    #[error("数据流读写出错：{0}")]
    Io(#[from] std::io::Error),
}
