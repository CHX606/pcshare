//! 电脑共享平台隔离层（阶段 1）。
//!
//! 职责边界：封装与操作系统相关的全部能力（打开目标、盘符与常用目录、桌面状态、窗口枚举、
//! 回收站、已装软件、进程管理、密钥保护、开机自启）。不联网，不依赖通信协议库。

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use thiserror::Error;

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
        let rights = if self.elevated { "管理员权限" } else { "普通权限" };
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

pub type PtyOpenResult =
    Result<(Box<dyn PtySession>, tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>), PlatformError>;

/// 跨平台抽象接口。
pub trait Platform: Send + Sync + 'static {
    /// 列出所有盘符或挂载点及容量。
    fn list_drives(&self) -> Result<Vec<DriveInfo>, PlatformError>;

    /// 获取桌面、下载、文档等常用文件夹。
    fn special_folders(&self) -> Result<Vec<SpecialFolder>, PlatformError>;

    /// 获取系统物理内存使用情况。
    fn memory_info(&self) -> Result<SystemMemory, PlatformError>;

    /// 获取电脑名与当前登录用户名。
    fn host_and_user(&self) -> (String, String);

    /// 获取当前进程所处的桌面会话与权限状态。
    fn desktop_environment(&self) -> DesktopEnvironment;

    /// 枚举当前桌面上带标题的所有可见顶层窗口。
    fn visible_windows(&self) -> Vec<WindowSummary>;

    /// 在普通线程中初始化 COM 并通过系统外壳打开程序、文档或 `shell:AppsFolder\...`。
    fn open_target<'a>(
        &'a self,
        target: &'a str,
        args: Option<&'a str>,
    ) -> BoxFuture<'a, Result<ShellOpenOutcome, PlatformError>>;

    /// 按应用标识打开开始菜单软件（包括应用商店软件）。
    fn launch_app_id<'a>(
        &'a self,
        app_id: &'a str,
        args: Option<&'a str>,
    ) -> BoxFuture<'a, Result<ShellOpenOutcome, PlatformError>>;

    /// 将一个或多个文件/文件夹移入系统回收站。
    fn move_to_trash(&self, paths: &[PathBuf]) -> Result<(), PlatformError>;

    /// 列出已装软件（阶段 2 使用系统原生外壳接口并保留 5 分钟内存缓存，`force_refresh = true` 时强制刷新）。
    fn installed_apps(
        &self,
        force_refresh: bool,
    ) -> BoxFuture<'_, Result<Vec<InstalledAppInfo>, PlatformError>>;

    /// 按软件标识提取 32x32 PNG 图标字节（带内存缓存），无可用图标时返回 `Ok(None)`。
    fn app_icon(&self, app_id: &str) -> BoxFuture<'_, Result<Option<Vec<u8>>, PlatformError>>;

    /// 查询当前进程自身的工作集内存占用（字节，用于任务 2.7 / 2.12 后台内存指标核验）。
    fn current_process_working_set(&self) -> u64;

    /// 列出当前正在运行的进程（含窗口标题、内存与处理器占用）。
    fn list_processes(&self) -> Result<Vec<ProcessSummary>, PlatformError>;

    /// 结束指定 PID 的进程（拒绝系统关键进程与自身进程），返回被结束的进程名。
    fn kill_process(&self, pid: u32) -> Result<String, PlatformError>;

    /// 使用系统数据保护接口加密敏感密钥字节。
    fn protect_secret(&self, plaintext: &[u8]) -> Result<Vec<u8>, PlatformError>;

    /// 解密由 `protect_secret` 生成的密文字节。
    fn unprotect_secret(&self, ciphertext: &[u8]) -> Result<Vec<u8>, PlatformError>;

    /// 查询当前开机自启状态。
    fn get_autostart(&self) -> Result<AutostartState, PlatformError>;

    /// 设置开机自启（支持普通登录自启或通过计划任务以管理员权限随登录自启）。
    fn set_autostart(
        &self,
        exe_path: &Path,
        enabled: bool,
        elevated: bool,
    ) -> Result<(), PlatformError>;

    /// 打开系统原生文件选择框（可多选），取消时返回空列表。
    fn pick_files(&self) -> BoxFuture<'_, Result<Vec<PathBuf>, PlatformError>>;

    /// 打开系统原生文件夹选择框，取消时返回 `None`。
    fn pick_folder(&self) -> BoxFuture<'_, Result<Option<PathBuf>, PlatformError>>;

    /// 枚举当前桌面所有显示器及其物理坐标与缩放比（任务 3.2）。
    fn list_monitors(&self) -> Result<Vec<PlatformMonitor>, PlatformError>;

    /// 枚举当前可截图的带标题可见窗口（任务 3.2）。
    fn list_capturable_windows(&self) -> Vec<CapturableWindowInfo>;

    /// 截取指定显示器（默认主显示器）为 JPEG 字节，支持按 `max_width` 等比缩放（任务 3.2）。
    fn capture_screen_jpeg(
        &self,
        monitor_index: Option<u32>,
        max_width: Option<u32>,
        quality: u8,
    ) -> Result<CapturedImage, PlatformError>;

    /// 截取指定窗口为 JPEG 字节，支持按 `max_width` 等比缩放（任务 3.2）。
    fn capture_window_jpeg(
        &self,
        hwnd: usize,
        max_width: Option<u32>,
        quality: u8,
    ) -> Result<CapturedImage, PlatformError>;

    /// 在远端屏幕绝对坐标上执行鼠标点击，自动检测管理员窗口 UIPI 拦截（任务 3.3）。
    fn send_mouse_click(
        &self,
        screen_x: i32,
        screen_y: i32,
        button: PlatformMouseBtn,
        double: bool,
        target_hwnd: Option<usize>,
    ) -> Result<InputOutcome, PlatformError>;

    /// 向远端当前窗口发送文本或组合快捷键，自动检测管理员窗口 UIPI 拦截（任务 3.3）。
    fn send_key_or_text(
        &self,
        text: Option<&str>,
        key_combo: Option<&str>,
        target_hwnd: Option<usize>,
    ) -> Result<InputOutcome, PlatformError>;

    /// 读取本机剪贴板（文字或 PNG 图片，任务 3.7）。
    fn get_clipboard(&self) -> Result<ClipboardData, PlatformError>;

    /// 将文字写入本机剪贴板并返回最新序号（任务 3.7）。
    fn set_clipboard_text(&self, text: &str) -> Result<u32, PlatformError>;

    /// 将 PNG 图片写入本机剪贴板（同时写入 CF_DIB 与 PNG 格式）并返回最新序号（任务 3.7）。
    fn set_clipboard_png(&self, png_bytes: &[u8]) -> Result<u32, PlatformError>;

    /// 获取本机剪贴板变化序号（用于自动同步检测，任务 3.7）。
    fn clipboard_sequence(&self) -> u32;

    /// 获取本机物理网卡地址列表（格式 `AA:BB:CC:DD:EE:FF`，用于网络唤醒，任务 3.5）。
    fn local_mac_addresses(&self) -> Vec<String>;

    /// 向局域网广播发送网络唤醒魔术包（任务 3.5）。
    fn send_wol_packets(&self, macs: &[String]) -> Result<usize, PlatformError>;

    /// 在本机桌面弹出可取消的电源操作倒计时窗口；返回 `Ok(true)` 表示倒计时结束应继续执行，`Ok(false)` 表示被取消（任务 3.5）。
    fn show_power_countdown<'a>(
        &'a self,
        action_label: &'a str,
        requester_name: &'a str,
        countdown_secs: u32,
        cancel_rx: tokio::sync::watch::Receiver<bool>,
    ) -> BoxFuture<'a, Result<bool, PlatformError>>;

    /// 执行系统电源操作（锁屏、睡眠、重启、关机，任务 3.5）。
    fn execute_power_action(&self, action: PlatformPowerAction) -> Result<(), PlatformError>;

    /// 打开交互式伪终端会话（PowerShell 或 cmd，任务 3.6）。
    fn open_pty_session(
        &self,
        prefer_powershell: bool,
        cols: u16,
        rows: u16,
        cwd: Option<&Path>,
    ) -> PtyOpenResult;

    /// 注册或更新全局快捷键（传 `None` 取消注册；若被占用返回 `PlatformError::Busy`，任务 3.1）。
    fn set_global_hotkey(
        &self,
        hotkey: Option<&str>,
        on_trigger: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<(), PlatformError>;

    /// 发送桌面系统通知（任务 3.10）。
    fn show_system_notification(&self, title: &str, body: &str) -> Result<(), PlatformError>;

    /// 发起把本地文件拖出到系统桌面或资源管理器的外壳拖拽会话（阶段 3 拖出补完）。
    fn start_file_drag_out<'a>(
        &'a self,
        paths: &'a [PathBuf],
    ) -> BoxFuture<'a, Result<bool, PlatformError>>;
}

#[cfg(windows)]
mod win_media_input;
#[cfg(windows)]
mod win_sys_pty;
#[cfg(windows)]
mod windows_impl;
#[cfg(windows)]
pub use windows_impl::WindowsPlatform as NativePlatform;

#[cfg(not(windows))]
mod stub_impl;
#[cfg(not(windows))]
pub use stub_impl::UnsupportedPlatform as NativePlatform;
