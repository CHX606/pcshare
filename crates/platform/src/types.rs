use super::*;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PlatformError {
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    PermissionDenied(String),
    #[error("{0}")]
    ElevationRequired(String),
    #[error("{0}")]
    Busy(String),
    #[error("{0}")]
    InvalidInput(String),
    #[error("{0}")]
    Unsupported(String),
    #[error("{0}")]
    System(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriveInfo {
    pub path: String,
    pub label: String,
    pub total_bytes: u64,
    pub available_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpecialFolderKind {
    Desktop,
    Downloads,
    Documents,
    Home,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecialFolder {
    pub kind: SpecialFolderKind,
    pub name: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemMemory {
    pub total_bytes: u64,
    pub available_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesktopEnvironment {
    pub session_id: u32,
    pub console_session_id: u32,
    pub elevated: bool,
    pub input_desktop: Option<String>,
}

impl DesktopEnvironment {
    /// 当前是否处于正常可交互且连着显示器的桌面。
    pub fn is_interactive(&self) -> bool {
        self.session_id != 0
            && self.session_id == self.console_session_id
            && self.input_desktop.is_some()
    }

    pub fn summary(&self) -> String {
        let place = if self.session_id == 0 {
            "系统服务环境（这里打开的软件不会有窗口）"
        } else if self.input_desktop.is_none() {
            "已登录，但当前是锁屏或权限确认界面"
        } else if self.session_id != self.console_session_id {
            "已登录，但不是接显示器的会话"
        } else {
            "正常桌面"
        };
        let rights = if self.elevated {
            "管理员权限"
        } else {
            "普通权限"
        };
        format!("{place}，{rights}")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowSummary {
    pub hwnd: usize,
    pub pid: u32,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledAppInfo {
    pub name: String,
    pub app_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcessSummary {
    pub pid: u32,
    pub name: String,
    pub window_title: Option<String>,
    pub memory_bytes: u64,
    pub cpu_percent: f32,
    pub is_critical: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutostartState {
    pub enabled: bool,
    pub elevated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellOpenOutcome {
    /// 系统已接受打开指令
    Launched,
    /// 疑似在对方桌面弹出了管理员权限确认框（UAC），正在等待对方点击确认
    SuspectedUacPrompt,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformMonitor {
    pub index: u32,
    pub name: String,
    pub origin_x: i32,
    pub origin_y: i32,
    pub width: u32,
    pub height: u32,
    pub dpi_scale_percent: u32,
    pub is_primary: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapturableWindowInfo {
    pub hwnd: usize,
    pub pid: u32,
    pub title: String,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedImage {
    pub jpeg_bytes: Vec<u8>,
    pub origin_x: i32,
    pub origin_y: i32,
    pub source_width: u32,
    pub source_height: u32,
    pub image_width: u32,
    pub image_height: u32,
    pub dpi_scale_percent: u32,
    pub target_hwnd: Option<usize>,
    pub monitor_index: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlatformMouseBtn {
    Left,
    Right,
    Middle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputOutcome {
    pub uipi_blocked: bool,
    pub warning: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlatformPowerAction {
    Lock,
    Sleep,
    Restart,
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardContent {
    Empty,
    Text(String),
    ImagePng {
        png_bytes: Vec<u8>,
        width: u32,
        height: u32,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardData {
    pub sequence: u32,
    pub content: ClipboardContent,
}

/// 伪终端交互会话抽象（任务 3.6）。
pub trait PtySession: Send + Sync {
    fn pid(&self) -> u32;
    fn shell_name(&self) -> &str;
    fn write_input(&self, bytes: &[u8]) -> Result<(), PlatformError>;
    fn resize(&self, cols: u16, rows: u16) -> Result<(), PlatformError>;
    fn try_wait(&self) -> Result<Option<i32>, PlatformError>;
    fn close(&self);
}

pub type PtyOpenResult = Result<
    (
        Box<dyn PtySession>,
        tokio::sync::mpsc::Receiver<Vec<u8>>,
    ),
    PlatformError,
>;
