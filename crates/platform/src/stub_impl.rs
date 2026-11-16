use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::{
    AutostartState, BoxFuture, CapturableWindowInfo, CapturedImage, ClipboardData,
    DesktopEnvironment, DriveInfo, InputOutcome, InstalledAppInfo, Platform, PlatformError,
    PlatformMonitor, PlatformMouseBtn, PlatformPowerAction, ProcessSummary, PtyOpenResult,
    ShellOpenOutcome, SpecialFolder, SystemMemory, WindowSummary,
};

#[derive(Default)]
pub struct UnsupportedPlatform;

impl UnsupportedPlatform {
    pub fn new() -> Self {
        Self
    }
}

fn unsupported<T>() -> Result<T, PlatformError> {
    Err(PlatformError::Unsupported(
        "当前操作系统暂未实现此平台功能（第一版仅支持 Windows）".to_string(),
    ))
}

impl Platform for UnsupportedPlatform {
    fn list_drives(&self) -> Result<Vec<DriveInfo>, PlatformError> {
        unsupported()
    }

    fn special_folders(&self) -> Result<Vec<SpecialFolder>, PlatformError> {
        unsupported()
    }

    fn memory_info(&self) -> Result<SystemMemory, PlatformError> {
        unsupported()
    }

    fn host_and_user(&self) -> (String, String) {
        (
            std::env::var("HOSTNAME").unwrap_or_else(|_| "unknown".to_string()),
            std::env::var("USER").unwrap_or_else(|_| "unknown".to_string()),
        )
    }

    fn desktop_environment(&self) -> DesktopEnvironment {
        DesktopEnvironment {
            session_id: 1,
            console_session_id: 1,
            elevated: false,
            input_desktop: None,
        }
    }

    fn visible_windows(&self) -> Vec<WindowSummary> {
        Vec::new()
    }

    fn open_target<'a>(
        &'a self,
        _target: &'a str,
        _args: Option<&'a str>,
    ) -> BoxFuture<'a, Result<ShellOpenOutcome, PlatformError>> {
        Box::pin(async { unsupported() })
    }

    fn launch_app_id<'a>(
        &'a self,
        _app_id: &'a str,
        _args: Option<&'a str>,
    ) -> BoxFuture<'a, Result<ShellOpenOutcome, PlatformError>> {
        Box::pin(async { unsupported() })
    }

    fn move_to_trash(&self, _paths: &[PathBuf]) -> Result<(), PlatformError> {
        unsupported()
    }

    fn installed_apps(
        &self,
        _force_refresh: bool,
    ) -> BoxFuture<'_, Result<Vec<InstalledAppInfo>, PlatformError>> {
        Box::pin(async { unsupported() })
    }

    fn app_icon(&self, _app_id: &str) -> BoxFuture<'_, Result<Option<Vec<u8>>, PlatformError>> {
        Box::pin(async { unsupported() })
    }

    fn current_process_working_set(&self) -> u64 {
        0
    }

    fn list_processes(&self) -> Result<Vec<ProcessSummary>, PlatformError> {
        unsupported()
    }

    fn kill_process(&self, _pid: u32) -> Result<String, PlatformError> {
        unsupported()
    }

    fn protect_secret(&self, _plaintext: &[u8]) -> Result<Vec<u8>, PlatformError> {
        unsupported()
    }

    fn unprotect_secret(&self, _ciphertext: &[u8]) -> Result<Vec<u8>, PlatformError> {
        unsupported()
    }

    fn get_autostart(&self) -> Result<AutostartState, PlatformError> {
        unsupported()
    }

    fn set_autostart(
        &self,
        _exe_path: &Path,
        _enabled: bool,
        _elevated: bool,
    ) -> Result<(), PlatformError> {
        unsupported()
    }

    fn pick_files(&self) -> BoxFuture<'_, Result<Vec<PathBuf>, PlatformError>> {
        Box::pin(async { unsupported() })
    }

    fn pick_folder(&self) -> BoxFuture<'_, Result<Option<PathBuf>, PlatformError>> {
        Box::pin(async { unsupported() })
    }

    fn list_monitors(&self) -> Result<Vec<PlatformMonitor>, PlatformError> {
        unsupported()
    }

    fn list_capturable_windows(&self) -> Vec<CapturableWindowInfo> {
        Vec::new()
    }

    fn capture_screen_jpeg(
        &self,
        _monitor_index: Option<u32>,
        _max_width: Option<u32>,
        _quality: u8,
    ) -> Result<CapturedImage, PlatformError> {
        unsupported()
    }

    fn capture_window_jpeg(
        &self,
        _hwnd: usize,
        _max_width: Option<u32>,
        _quality: u8,
    ) -> Result<CapturedImage, PlatformError> {
        unsupported()
    }

    fn send_mouse_click(
        &self,
        _screen_x: i32,
        _screen_y: i32,
        _button: PlatformMouseBtn,
        _double: bool,
        _target_hwnd: Option<usize>,
    ) -> Result<InputOutcome, PlatformError> {
        unsupported()
    }

    fn send_key_or_text(
        &self,
        _text: Option<&str>,
        _key_combo: Option<&str>,
        _target_hwnd: Option<usize>,
    ) -> Result<InputOutcome, PlatformError> {
        unsupported()
    }

    fn get_clipboard(&self) -> Result<ClipboardData, PlatformError> {
        unsupported()
    }

    fn set_clipboard_text(&self, _text: &str) -> Result<u32, PlatformError> {
        unsupported()
    }

    fn set_clipboard_png(&self, _png_bytes: &[u8]) -> Result<u32, PlatformError> {
        unsupported()
    }

    fn clipboard_sequence(&self) -> u32 {
        0
    }

    fn local_mac_addresses(&self) -> Vec<String> {
        Vec::new()
    }

    fn send_wol_packets(&self, _macs: &[String]) -> Result<usize, PlatformError> {
        unsupported()
    }

    fn show_power_countdown<'a>(
        &'a self,
        _action_label: &'a str,
        _requester_name: &'a str,
        _countdown_secs: u32,
        _cancel_rx: tokio::sync::watch::Receiver<bool>,
    ) -> BoxFuture<'a, Result<bool, PlatformError>> {
        Box::pin(async { unsupported() })
    }

    fn execute_power_action(&self, _action: PlatformPowerAction) -> Result<(), PlatformError> {
        unsupported()
    }

    fn open_pty_session(
        &self,
        _prefer_powershell: bool,
        _cols: u16,
        _rows: u16,
        _cwd: Option<&Path>,
    ) -> PtyOpenResult {
        unsupported()
    }

    fn set_global_hotkey(
        &self,
        _hotkey: Option<&str>,
        _on_trigger: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<(), PlatformError> {
        unsupported()
    }

    fn show_system_notification(&self, _title: &str, _body: &str) -> Result<(), PlatformError> {
        unsupported()
    }

    fn start_file_drag_out<'a>(
        &'a self,
        _paths: &'a [PathBuf],
    ) -> BoxFuture<'a, Result<bool, PlatformError>> {
        Box::pin(async { unsupported() })
    }
}
