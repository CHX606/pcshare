use pcshare_platform::PlatformError;
use pcshare_proto::{ErrorCode, FrameError, RpcError};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("{0}")]
    Rpc(#[from] RpcError),

    #[error("协议传输错误：{0}")]
    Frame(#[from] FrameError),

    #[error("{0}")]
    Platform(#[from] PlatformError),

    #[error("文件读写错误：{0}")]
    Io(#[from] std::io::Error),

    #[error("参数错误：{0}")]
    InvalidArgument(String),

    #[error("尚未连接到对方设备：{0}")]
    NotConnected(String),

    #[error("等待对方响应超时（请检查网络是否通畅）")]
    Timeout,

    #[error("操作已取消")]
    Cancelled,

    #[error("{0}")]
    Internal(String),
}

impl CoreError {
    pub fn error_code(&self) -> Option<ErrorCode> {
        match self {
            Self::Rpc(err) => Some(err.code),
            Self::Platform(err) => Some(map_platform_to_rpc(err).code),
            Self::Io(err) => Some(map_io_to_rpc(err, None).code),
            Self::InvalidArgument(_) => Some(ErrorCode::Invalid),
            Self::Cancelled => Some(ErrorCode::Cancelled),
            _ => None,
        }
    }

    pub fn to_user_message(&self) -> String {
        match self {
            Self::Rpc(err) => err.msg.clone(),
            other => other.to_string(),
        }
    }
}

/// 将系统 `io::Error` 映射为协议标准错误码与通俗中文说明（见总体方案 6.3 节）。
pub fn map_io_to_rpc(err: &std::io::Error, target_hint: Option<&str>) -> RpcError {
    // Windows 共享冲突或文件锁定错误码：32 (ERROR_SHARING_VIOLATION), 33 (ERROR_LOCK_VIOLATION)
    if matches!(err.raw_os_error(), Some(32 | 33)) {
        return RpcError::new(
            ErrorCode::Busy,
            match target_hint {
                Some(p) => format!("“{p}”正在被其他程序占用，请先关闭相关程序后再试"),
                None => "文件正在被其他程序占用，请先关闭相关程序后再试".to_string(),
            },
        );
    }

    match err.kind() {
        std::io::ErrorKind::NotFound => RpcError::new(
            ErrorCode::NotFound,
            match target_hint {
                Some(p) => format!("找不到这个文件或文件夹：{p}"),
                None => "找不到指定的文件或文件夹".to_string(),
            },
        ),
        std::io::ErrorKind::AlreadyExists => RpcError::new(
            ErrorCode::AlreadyExists,
            match target_hint {
                Some(p) => format!("同名文件或文件夹已存在：{p}"),
                None => "同名文件或文件夹已存在".to_string(),
            },
        ),
        std::io::ErrorKind::PermissionDenied => RpcError::new(
            ErrorCode::Forbidden,
            match target_hint {
                Some(p) => format!("系统拒绝访问“{p}”，可能没有权限、文件只读或正在使用"),
                None => "系统拒绝访问该目标，可能没有权限或文件为只读".to_string(),
            },
        ),
        std::io::ErrorKind::InvalidInput => {
            RpcError::new(ErrorCode::Invalid, format!("路径或文件名不合法：{err}"))
        }
        _ => RpcError::new(ErrorCode::Io, format!("读写文件时出错：{err}")),
    }
}

/// 将 `PlatformError` 映射为协议标准错误码。
pub fn map_platform_to_rpc(err: &PlatformError) -> RpcError {
    match err {
        PlatformError::NotFound(msg) => RpcError::new(ErrorCode::NotFound, msg.clone()),
        PlatformError::PermissionDenied(msg) | PlatformError::ElevationRequired(msg) => {
            RpcError::new(ErrorCode::Forbidden, msg.clone())
        }
        PlatformError::Busy(msg) => RpcError::new(ErrorCode::Busy, msg.clone()),
        PlatformError::InvalidInput(msg) => RpcError::new(ErrorCode::Invalid, msg.clone()),
        PlatformError::Unsupported(msg) => RpcError::new(ErrorCode::Unsupported, msg.clone()),
        PlatformError::System(msg) => RpcError::new(ErrorCode::Internal, msg.clone()),
    }
}
