use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::{Semaphore, watch};
use windows::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_GEN_FAILURE, ERROR_INSUFFICIENT_BUFFER, ERROR_INVALID_PARAMETER,
    FILETIME, HANDLE, HLOCAL, HWND, LPARAM, LocalFree, SIZE,
};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC,
    DeleteObject, GetDIBits, GetObjectW, HGDIOBJ,
};
use windows::Win32::Security::Cryptography::{
    CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
};
use windows::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, GetLogicalDrives, GetShortPathNameW, GetVolumeInformationW,
};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, IBindCtx};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows::Win32::System::RemoteDesktop::{
    ProcessIdToSessionId, WTSGetActiveConsoleSessionId,
};
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, GetExitCodeProcess, GetProcessTimes, GetSystemTimes,
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
    QueryFullProcessImageNameW, TerminateProcess,
};
use windows::Win32::UI::Shell::{
    BHID_EnumItems, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT,
    FOFX_ADDUNDORECORD, FOFX_RECYCLEONDELETE, FOLDERID_AppsFolder, FOLDERID_Desktop,
    FOLDERID_Documents, FOLDERID_Downloads, FOLDERID_Profile, FOS_ALLOWMULTISELECT,
    FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, FileOpenDialog, FileOperation,
    IEnumShellItems, IFileOpenDialog, IFileOperation, IFileOperationProgressSink, IShellItem,
    IShellItemImageFactory, KF_FLAG_DEFAULT, SHCreateItemFromParsingName, SHGetKnownFolderItem,
    SHGetKnownFolderPath, SIGDN_FILESYSPATH, SIGDN_NORMALDISPLAY, SIGDN_PARENTRELATIVEPARSING,
    SIIGBF_BIGGERSIZEOK, SIIGBF_ICONONLY, ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
    SW_SHOWNORMAL,
};
use windows::core::{BOOL, GUID, PCWSTR, PWSTR};

use crate::win_util::{
    ComGuard, OwnedHandle, current_input_desktop, is_elevated, is_win32_error,
    pwstr_to_string_and_free, strip_verbatim_prefix, to_verbatim_path, wide_null, zeroize_bytes,
};
use crate::{
    AutostartState, BoxFuture, CapturableWindowInfo, CapturedImage, ClipboardData,
    DesktopEnvironment, DriveInfo, InputOutcome, InstalledAppInfo, Platform, PlatformError,
    PlatformMonitor, PlatformMouseBtn, PlatformPowerAction, ProcessSummary, PtyOpenResult,
    ShellOpenOutcome, SpecialFolder, SpecialFolderKind, SystemMemory, WindowSummary,
    win_media_input, win_sys_pty,
};

const APP_CACHE_TTL: Duration = Duration::from_secs(5 * 60);
const MAX_ICON_CACHE_ENTRIES: usize = 512;
const MAX_CONCURRENT_ICON_WORKERS: usize = 4;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const STILL_ACTIVE_CODE: u32 = 259;
const RUN_REG_PATH: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_REG_VALUE: &str = "PcShare";
const SCHTASK_NAME: &str = "PcShareAutostart";

struct CpuSample {
    system_total_100ns: u64,
    proc_times_100ns: HashMap<u32, u64>,
}

/// 有上限的图标内存缓存（不缓存瞬态 `None`，超过上限按 FIFO 淘汰最旧条目）。
#[derive(Default)]
struct IconCache {
    entries: HashMap<String, Vec<u8>>,
    order: VecDeque<String>,
}

impl IconCache {
    fn get(&self, id: &str) -> Option<Vec<u8>> {
        self.entries.get(id).cloned()
    }

    fn insert(&mut self, id: String, png: Vec<u8>) {
        if let Some(existing) = self.entries.get_mut(&id) {
            *existing = png;
            return;
        }
        while self.entries.len() >= MAX_ICON_CACHE_ENTRIES {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.entries.remove(&oldest);
        }
        self.order.push_back(id.clone());
        self.entries.insert(id, png);
    }
}

type IconInflightMap = HashMap<String, watch::Receiver<Option<Option<Vec<u8>>>>>;

struct IconInflightGuard {
    map: Arc<Mutex<IconInflightMap>>,
    id: String,
    tx: Option<watch::Sender<Option<Option<Vec<u8>>>>>,
}

impl IconInflightGuard {
    fn complete(mut self, result: Option<Vec<u8>>) {
        self.map
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.id);
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(Some(result));
        }
    }
}

impl Drop for IconInflightGuard {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            self.map
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&self.id);
            let _ = tx.send(Some(None));
        }
    }
}

pub struct WindowsPlatform {
    apps_cache: tokio::sync::Mutex<Option<(Instant, u64, Vec<InstalledAppInfo>)>>,
    icons_cache: Mutex<IconCache>,
    icon_inflight: Arc<Mutex<IconInflightMap>>,
    icon_sem: Arc<Semaphore>,
    cpu_sample: Mutex<Option<CpuSample>>,
    hotkey_ctrl: win_sys_pty::HotkeyController,
}

impl Default for WindowsPlatform {
    fn default() -> Self {
        Self::new()
    }
}

impl WindowsPlatform {
    pub fn new() -> Self {
        Self {
            apps_cache: tokio::sync::Mutex::new(None),
            icons_cache: Mutex::new(IconCache::default()),
            icon_inflight: Arc::new(Mutex::new(HashMap::new())),
            icon_sem: Arc::new(Semaphore::new(MAX_CONCURRENT_ICON_WORKERS)),
            cpu_sample: Mutex::new(None),
            hotkey_ctrl: win_sys_pty::HotkeyController::default(),
        }
    }
}


mod win_helpers;
use win_helpers::*;

struct ZeroizeVec(Vec<u8>);
impl Drop for ZeroizeVec {
    fn drop(&mut self) {
        zeroize_bytes(&mut self.0);
    }
}

struct LocalBlobGuard {
    ptr: *mut u8,
    len: usize,
    zeroize: bool,
}

impl Drop for LocalBlobGuard {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe {
                if self.zeroize && self.len > 0 {
                    zeroize_bytes(std::slice::from_raw_parts_mut(self.ptr, self.len));
                }
                let _ = LocalFree(Some(HLOCAL(self.ptr as *mut c_void)));
            }
        }
    }
}


impl Platform for WindowsPlatform {
    fn list_drives(&self) -> Result<Vec<DriveInfo>, PlatformError> {
        let mask = unsafe { GetLogicalDrives() };
        let mut drives = Vec::new();
        for i in 0..26u8 {
            if (mask & (1 << i)) == 0 {
                continue;
            }
            let root = format!("{}:\\", (b'A' + i) as char);
            let root_w = wide_null(&root);

            let mut available = 0u64;
            let mut total = 0u64;
            unsafe {
                let _ = GetDiskFreeSpaceExW(
                    PCWSTR(root_w.as_ptr()),
                    Some(&mut available),
                    Some(&mut total),
                    None,
                );
            }

            let mut vol_buf = [0u16; 128];
            let label = unsafe {
                if GetVolumeInformationW(
                    PCWSTR(root_w.as_ptr()),
                    Some(&mut vol_buf),
                    None,
                    None,
                    None,
                    None,
                )
                .is_ok()
                {
                    let len = vol_buf
                        .iter()
                        .position(|&c| c == 0)
                        .unwrap_or(vol_buf.len());
                    String::from_utf16_lossy(&vol_buf[..len])
                } else {
                    String::new()
                }
            };

            drives.push(DriveInfo {
                path: root,
                label,
                total_bytes: total,
                available_bytes: available,
            });
        }
        Ok(drives)
    }

    fn special_folders(&self) -> Result<Vec<SpecialFolder>, PlatformError> {
        let specs = [
            (SpecialFolderKind::Desktop, "桌面", &FOLDERID_Desktop),
            (SpecialFolderKind::Downloads, "下载", &FOLDERID_Downloads),
            (SpecialFolderKind::Documents, "文档", &FOLDERID_Documents),
            (SpecialFolderKind::Home, "个人文件夹", &FOLDERID_Profile),
        ];
        let mut out = Vec::new();
        for (kind, name, guid) in specs {
            if let Some(path) = known_folder_path(guid)
                && path.exists()
            {
                out.push(SpecialFolder {
                    kind,
                    name: name.to_string(),
                    path,
                });
            }
        }
        Ok(out)
    }

    fn memory_info(&self) -> Result<SystemMemory, PlatformError> {
        let mut status = MEMORYSTATUSEX {
            dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
            ..Default::default()
        };
        unsafe {
            GlobalMemoryStatusEx(&mut status)
                .map_err(|err| PlatformError::System(format!("读取系统内存信息失败：{err}")))?;
        }
        Ok(SystemMemory {
            total_bytes: status.ullTotalPhys,
            available_bytes: status.ullAvailPhys,
        })
    }

    fn host_and_user(&self) -> (String, String) {
        let computer = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "未知电脑".to_string());
        let user = std::env::var("USERNAME").unwrap_or_else(|_| "未知用户".to_string());
        (computer, user)
    }

    fn desktop_environment(&self) -> DesktopEnvironment {
        let mut session_id = 0u32;
        unsafe {
            let _ = ProcessIdToSessionId(GetCurrentProcessId(), &mut session_id);
        }
        let console_session_id = unsafe { WTSGetActiveConsoleSessionId() };
        DesktopEnvironment {
            session_id,
            console_session_id,
            elevated: is_elevated(),
            input_desktop: current_input_desktop(),
        }
    }

    fn visible_windows(&self) -> Vec<WindowSummary> {
        let mut list = Vec::new();
        unsafe {
            let _ = EnumWindows(
                Some(collect_visible_window),
                LPARAM(&mut list as *mut Vec<WindowSummary> as isize),
            );
        }
        list
    }

    fn open_target<'a>(
        &'a self,
        target: &'a str,
        args: Option<&'a str>,
    ) -> BoxFuture<'a, Result<ShellOpenOutcome, PlatformError>> {
        let target_owned = target.to_string();
        let args_owned = args.map(ToOwned::to_owned);
        Box::pin(async move {
            let before_env = self.desktop_environment();
            let (tx, mut rx) = tokio::sync::oneshot::channel();
            // 在独立的系统普通线程里初始化公寓态 COM 并调用 ShellExecuteW，不污染异步工作线程
            std::thread::spawn(move || {
                let res = shell_execute_blocking(target_owned, args_owned);
                let _ = tx.send(res);
            });

            // 如果对方处于普通权限桌面，而 ShellExecuteW 因弹出 UAC 安全桌面一直阻塞等待人点击：
            match tokio::time::timeout(Duration::from_millis(2500), &mut rx).await {
                Ok(Ok(Ok(()))) => Ok(ShellOpenOutcome::Launched),
                Ok(Ok(Err(err))) => Err(err),
                Ok(Err(_)) => Err(PlatformError::System(
                    "打开目标的后台线程意外退出".to_string(),
                )),
                Err(_) => {
                    let now_env = self.desktop_environment();
                    if !before_env.elevated
                        && before_env.input_desktop.is_some()
                        && now_env.input_desktop.is_none()
                    {
                        return Ok(ShellOpenOutcome::SuspectedUacPrompt);
                    }
                    match tokio::time::timeout(Duration::from_secs(8), rx).await {
                        Ok(Ok(Ok(()))) => Ok(ShellOpenOutcome::Launched),
                        Ok(Ok(Err(err))) => Err(err),
                        Ok(Err(_)) => Err(PlatformError::System(
                            "打开目标的后台线程意外退出".to_string(),
                        )),
                        Err(_) => Ok(ShellOpenOutcome::SuspectedUacPrompt),
                    }
                }
            }
        })
    }

    fn launch_app_id<'a>(
        &'a self,
        app_id: &'a str,
        args: Option<&'a str>,
    ) -> BoxFuture<'a, Result<ShellOpenOutcome, PlatformError>> {
        let shell_uri = format!(r"shell:AppsFolder\{app_id}");
        let args_owned = args.map(ToOwned::to_owned);
        Box::pin(async move { self.open_target(&shell_uri, args_owned.as_deref()).await })
    }

    fn move_to_trash(&self, paths: &[PathBuf]) -> Result<(), PlatformError> {
        if paths.is_empty() {
            return Ok(());
        }
        // 使用 symlink_metadata 检查路径本身是否存在，兼容悬空/损坏符号链接
        for p in paths {
            if let Err(err) = std::fs::symlink_metadata(p) {
                if err.kind() == std::io::ErrorKind::NotFound {
                    return Err(PlatformError::NotFound(format!(
                        "找不到要删除的文件或文件夹：{}",
                        p.display()
                    )));
                }
                return Err(PlatformError::System(format!(
                    "无法读取待删除路径信息（{}）：{err}",
                    p.display()
                )));
            }
        }
        let owned = paths.to_vec();
        std::thread::spawn(move || move_to_trash_sta(owned))
            .join()
            .map_err(|_| PlatformError::System("移入回收站的后台线程异常中止".to_string()))?
    }

    fn installed_apps(
        &self,
        force_refresh: bool,
    ) -> BoxFuture<'_, Result<Vec<InstalledAppInfo>, PlatformError>> {
        Box::pin(async move {
            let mut guard = self.apps_cache.lock().await;
            let live_fp = tokio::task::spawn_blocking(compute_installed_apps_fingerprint)
                .await
                .map_err(|e| PlatformError::System(format!("计算软件列表指纹失败：{e}")))?;
            if !force_refresh
                && let Some((cached_at, cached_fp, list)) = guard.as_ref()
                && cached_at.elapsed() < APP_CACHE_TTL
                && *cached_fp == live_fp
            {
                return Ok(list.clone());
            }
            let apps = query_start_apps(live_fp, !force_refresh).await?;
            *guard = Some((Instant::now(), live_fp, apps.clone()));
            Ok(apps)
        })
    }

    fn app_icon(&self, app_id: &str) -> BoxFuture<'_, Result<Option<Vec<u8>>, PlatformError>> {
        let id = app_id.trim().to_string();
        Box::pin(async move {
            if id.is_empty() {
                return Ok(None);
            }
            if let Some(cached) = self
                .icons_cache
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&id)
            {
                return Ok(Some(cached));
            }

            // 合并同一 app_id 的并发提取请求，避免重复开 STA 线程
            enum IconAction {
                Wait(watch::Receiver<Option<Option<Vec<u8>>>>),
                Run(IconInflightGuard),
            }
            let action = {
                let mut inflight = self.icon_inflight.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(rx) = inflight.get(&id).cloned() {
                    IconAction::Wait(rx)
                } else {
                    let (tx, rx) = watch::channel(None);
                    inflight.insert(id.clone(), rx);
                    IconAction::Run(IconInflightGuard {
                        map: self.icon_inflight.clone(),
                        id: id.clone(),
                        tx: Some(tx),
                    })
                }
            };
            let guard = match action {
                IconAction::Wait(mut rx) => {
                    let res = rx
                        .wait_for(|v| v.is_some())
                        .await
                        .ok()
                        .and_then(|v| v.clone().flatten());
                    return Ok(res);
                }
                IconAction::Run(guard) => guard,
            };

            // 限制并发 STA 图标提取线程上限，防止一次打开软件页触发线程风暴
            let _permit = self
                .icon_sem
                .acquire()
                .await
                .map_err(|_| PlatformError::System("图标提取并发调度器已关闭".to_string()))?;

            let id_clone = id.clone();
            let (tx, rx) = tokio::sync::oneshot::channel();
            std::thread::spawn(move || {
                let _ = tx.send(extract_app_icon_png_sta(&id_clone));
            });
            let png = rx.await.unwrap_or(None);
            if let Some(ref bytes) = png {
                self.icons_cache
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(id, bytes.clone());
            }
            guard.complete(png.clone());
            Ok(png)
        })
    }

    fn current_process_working_set(&self) -> u64 {
        unsafe {
            let mut pmc = PROCESS_MEMORY_COUNTERS::default();
            if K32GetProcessMemoryInfo(
                GetCurrentProcess(),
                &mut pmc,
                std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
            )
            .as_bool()
            {
                pmc.WorkingSetSize as u64
            } else {
                0
            }
        }
    }

    fn list_processes(&self) -> Result<Vec<ProcessSummary>, PlatformError> {
        let self_pid = std::process::id();
        let mut window_titles: HashMap<u32, String> = HashMap::new();
        for win in self.visible_windows() {
            window_titles.entry(win.pid).or_insert(win.title);
        }

        let (mut sys_total, mut snap) = system_and_proc_times();
        let mut prev_guard = self.cpu_sample.lock().unwrap_or_else(|e| e.into_inner());
        if prev_guard.is_none() {
            let first_proc_times = snap
                .iter()
                .map(|(&pid, &(_, _, t))| (pid, t))
                .collect::<HashMap<_, _>>();
            *prev_guard = Some(CpuSample {
                system_total_100ns: sys_total,
                proc_times_100ns: first_proc_times,
            });
            std::thread::sleep(Duration::from_millis(60));
            let second = system_and_proc_times();
            sys_total = second.0;
            snap = second.1;
        }

        let prev = prev_guard.as_ref().unwrap();
        let sys_delta = sys_total.saturating_sub(prev.system_total_100ns);

        let mut next_proc_times = HashMap::with_capacity(snap.len());
        let mut out = Vec::with_capacity(snap.len());
        for (pid, (name, memory_bytes, proc_time)) in snap {
            next_proc_times.insert(pid, proc_time);
            let cpu_percent = if sys_delta > 0
                && let Some(&old_time) = prev.proc_times_100ns.get(&pid)
            {
                let delta = proc_time.saturating_sub(old_time);
                ((delta as f64 / sys_delta as f64) * 100.0).clamp(0.0, 100.0) as f32
            } else {
                0.0
            };
            let is_critical = is_critical_process_name(pid, &name, self_pid);
            out.push(ProcessSummary {
                pid,
                name,
                window_title: window_titles.remove(&pid),
                memory_bytes,
                cpu_percent,
                is_critical,
            });
        }

        *prev_guard = Some(CpuSample {
            system_total_100ns: sys_total,
            proc_times_100ns: next_proc_times,
        });

        out.sort_by_key(|a| std::cmp::Reverse(a.memory_bytes));
        Ok(out)
    }

    fn kill_process(&self, pid: u32) -> Result<String, PlatformError> {
        let self_pid = std::process::id();
        if pid <= 4 || pid == self_pid {
            return Err(PlatformError::PermissionDenied(
                "不允许结束系统核心进程或本程序自身".to_string(),
            ));
        }

        // 单句柄打开目标进程并在同一句柄上校验进程名再结束，消除全量遍历开销与 PID 复用 TOCTOU
        let handle_raw = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE,
                false,
                pid,
            )
        }
        .map_err(|err| {
            if is_win32_error(&err, ERROR_INVALID_PARAMETER) {
                PlatformError::NotFound(format!("找不到进程（PID {pid}），它可能已经退出"))
            } else if is_win32_error(&err, ERROR_ACCESS_DENIED) {
                PlatformError::PermissionDenied(format!(
                    "没有权限结束进程（PID {pid}）：{err}"
                ))
            } else {
                PlatformError::PermissionDenied(format!("无法打开目标进程（PID {pid}）：{err}"))
            }
        })?;
        let handle = OwnedHandle::new(handle_raw)
            .ok_or_else(|| PlatformError::NotFound(format!("找不到进程（PID {pid}）")))?;

        // 已退出但外部仍持有句柄的僵尸进程 OpenProcess 仍会成功，先检查退出码避免误报系统错
        let mut exit_code = 0u32;
        if unsafe { GetExitCodeProcess(handle.raw(), &mut exit_code) }.is_ok()
            && exit_code != STILL_ACTIVE_CODE
        {
            return Err(PlatformError::NotFound(format!(
                "找不到进程（PID {pid}），它可能已经退出"
            )));
        }

        let name = query_process_exe_name(handle.raw(), pid)?;
        if is_critical_process_name(pid, &name, self_pid) {
            return Err(PlatformError::PermissionDenied(format!(
                "“{name}”是系统关键进程，为防止电脑蓝屏或死机，不允许结束"
            )));
        }

        unsafe { TerminateProcess(handle.raw(), 1) }
            .map_err(|err| PlatformError::PermissionDenied(format!("结束进程“{name}”失败：{err}")))?;
        Ok(name)
    }

    fn protect_secret(&self, plaintext: &[u8]) -> Result<Vec<u8>, PlatformError> {
        let mut input = ZeroizeVec(plaintext.to_vec());
        let cb_data = u32::try_from(input.0.len())
            .map_err(|_| PlatformError::InvalidInput("待保护密钥长度超出上限".to_string()))?;
        // Windows CryptProtectData 在 cbData == 0 时仍要求 pbData 非空，否则返回 E_INVALIDARG
        let mut empty_sentinel = 0u8;
        let in_blob = CRYPT_INTEGER_BLOB {
            cbData: cb_data,
            pbData: if input.0.is_empty() {
                &mut empty_sentinel
            } else {
                input.0.as_mut_ptr()
            },
        };
        let mut out_blob = CRYPT_INTEGER_BLOB::default();
        unsafe {
            CryptProtectData(
                &in_blob,
                PCWSTR::null(),
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out_blob,
            )
            .map_err(|err| PlatformError::System(format!("加密保护密钥失败：{err}")))?;

            let _out_guard = LocalBlobGuard {
                ptr: out_blob.pbData,
                len: out_blob.cbData as usize,
                zeroize: false,
            };
            if out_blob.cbData == 0 {
                return Ok(Vec::new());
            }
            if out_blob.pbData.is_null() {
                return Err(PlatformError::System("系统加密输出指针为空".to_string()));
            }
            Ok(std::slice::from_raw_parts(out_blob.pbData, out_blob.cbData as usize).to_vec())
        }
    }

    fn unprotect_secret(&self, ciphertext: &[u8]) -> Result<Vec<u8>, PlatformError> {
        if ciphertext.is_empty() {
            return Err(PlatformError::System("密钥密文为空".to_string()));
        }
        let mut input = ciphertext.to_vec();
        let cb_data = u32::try_from(input.len())
            .map_err(|_| PlatformError::InvalidInput("密钥密文长度超出上限".to_string()))?;
        let in_blob = CRYPT_INTEGER_BLOB {
            cbData: cb_data,
            pbData: input.as_mut_ptr(),
        };
        let mut out_blob = CRYPT_INTEGER_BLOB::default();
        unsafe {
            CryptUnprotectData(
                &in_blob,
                None,
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out_blob,
            )
            .map_err(|err| {
                PlatformError::System(format!("解密密钥失败（文件可能已损坏）：{err}"))
            })?;

            // 解密出的临时明文在释放 LocalFree 前自动清零
            let _out_guard = LocalBlobGuard {
                ptr: out_blob.pbData,
                len: out_blob.cbData as usize,
                zeroize: true,
            };
            if out_blob.cbData == 0 {
                return Ok(Vec::new());
            }
            if out_blob.pbData.is_null() {
                return Err(PlatformError::System("系统解密输出指针为空".to_string()));
            }
            Ok(std::slice::from_raw_parts(out_blob.pbData, out_blob.cbData as usize).to_vec())
        }
    }

    fn get_autostart(&self) -> Result<AutostartState, PlatformError> {
        if schtask_exists() {
            return Ok(AutostartState {
                enabled: true,
                elevated: true,
            });
        }

        let enabled = windows_registry::CURRENT_USER
            .open(RUN_REG_PATH)
            .and_then(|key| key.get_string(RUN_REG_VALUE))
            .is_ok();
        Ok(AutostartState {
            enabled,
            elevated: false,
        })
    }

    fn set_autostart(
        &self,
        exe_path: &Path,
        enabled: bool,
        elevated: bool,
    ) -> Result<(), PlatformError> {
        use std::os::windows::process::CommandExt;

        if !enabled {
            // 若管理员自启计划任务存在但无权限删除，直接报错而不返回假成功
            delete_schtask_if_exists()?;
            remove_run_reg_value();
            return Ok(());
        }

        let clean_exe = strip_verbatim_prefix(exe_path);
        let quoted_cmd = format!("\"{clean_exe}\" --background");
        if elevated {
            // 先创建管理员计划任务，成功后再清理旧注册表自启项，失败时保留原有自启
            let out = std::process::Command::new("schtasks.exe")
                .args([
                    "/Create",
                    "/TN",
                    SCHTASK_NAME,
                    "/TR",
                    &quoted_cmd,
                    "/SC",
                    "ONLOGON",
                    "/RL",
                    "HIGHEST",
                    "/F",
                ])
                .creation_flags(CREATE_NO_WINDOW)
                .output()
                .map_err(|err| {
                    PlatformError::System(format!("无法调用系统计划任务工具：{err}"))
                })?;
            if !out.status.success() {
                return Err(PlatformError::PermissionDenied(
                    "设置“以管理员权限开机自启”需要先以管理员身份运行本程序".to_string(),
                ));
            }
            remove_run_reg_value();
        } else {
            // 先校验注册表可打开，再删除旧高权限计划任务（若无权删除则报错且不写入重复项），最后写入普通自启
            let key = windows_registry::CURRENT_USER
                .create(RUN_REG_PATH)
                .map_err(|err| {
                    PlatformError::System(format!("打开自启动注册表项失败：{err}"))
                })?;
            delete_schtask_if_exists()?;
            key.set_string(RUN_REG_VALUE, &quoted_cmd).map_err(|err| {
                PlatformError::System(format!("写入自启动注册表项失败：{err}"))
            })?;
        }
        Ok(())
    }

    fn pick_files(&self) -> BoxFuture<'_, Result<Vec<PathBuf>, PlatformError>> {
        Box::pin(async {
            let (tx, rx) = tokio::sync::oneshot::channel();
            std::thread::spawn(move || {
                let _ = tx.send(run_file_dialog_sta(false));
            });
            rx.await
                .map_err(|_| PlatformError::System("选择文件的后台线程意外退出".to_string()))?
        })
    }

    fn pick_folder(&self) -> BoxFuture<'_, Result<Option<PathBuf>, PlatformError>> {
        Box::pin(async {
            let (tx, rx) = tokio::sync::oneshot::channel();
            std::thread::spawn(move || {
                let res = run_file_dialog_sta(true).map(|mut v| v.pop());
                let _ = tx.send(res);
            });
            rx.await
                .map_err(|_| PlatformError::System("选择文件夹的后台线程意外退出".to_string()))?
        })
    }

    fn list_monitors(&self) -> Result<Vec<PlatformMonitor>, PlatformError> {
        win_media_input::list_monitors_impl()
    }

    fn list_capturable_windows(&self) -> Vec<CapturableWindowInfo> {
        win_media_input::list_capturable_windows_impl()
    }

    fn capture_screen_jpeg(
        &self,
        monitor_index: Option<u32>,
        max_width: Option<u32>,
        quality: u8,
    ) -> Result<CapturedImage, PlatformError> {
        let desktop = self.desktop_environment();
        win_media_input::capture_screen_jpeg_impl(&desktop, monitor_index, max_width, quality)
    }

    fn capture_window_jpeg(
        &self,
        hwnd: usize,
        max_width: Option<u32>,
        quality: u8,
    ) -> Result<CapturedImage, PlatformError> {
        let desktop = self.desktop_environment();
        win_media_input::capture_window_jpeg_impl(&desktop, hwnd, max_width, quality)
    }

    fn send_mouse_click(
        &self,
        screen_x: i32,
        screen_y: i32,
        button: PlatformMouseBtn,
        double: bool,
        target_hwnd: Option<usize>,
    ) -> Result<InputOutcome, PlatformError> {
        win_media_input::send_mouse_click_impl(screen_x, screen_y, button, double, target_hwnd)
    }

    fn send_key_or_text(
        &self,
        text: Option<&str>,
        key_combo: Option<&str>,
        target_hwnd: Option<usize>,
    ) -> Result<InputOutcome, PlatformError> {
        win_media_input::send_key_or_text_impl(text, key_combo, target_hwnd)
    }

    fn get_clipboard(&self) -> Result<ClipboardData, PlatformError> {
        win_media_input::get_clipboard_impl()
    }

    fn set_clipboard_text(&self, text: &str) -> Result<u32, PlatformError> {
        win_media_input::set_clipboard_text_impl(text)
    }

    fn set_clipboard_png(&self, png_bytes: &[u8]) -> Result<u32, PlatformError> {
        win_media_input::set_clipboard_png_impl(png_bytes)
    }

    fn clipboard_sequence(&self) -> u32 {
        win_media_input::clipboard_sequence_impl()
    }

    fn local_mac_addresses(&self) -> Vec<String> {
        win_sys_pty::local_mac_addresses_impl()
    }

    fn primary_physical_ipv4(&self) -> Option<std::net::Ipv4Addr> {
        win_sys_pty::primary_physical_ipv4_impl()
    }

    fn send_wol_packets(&self, macs: &[String]) -> Result<usize, PlatformError> {
        win_sys_pty::send_wol_packets_impl(macs)
    }

    fn show_power_countdown<'a>(
        &'a self,
        action_label: &'a str,
        requester_name: &'a str,
        countdown_secs: u32,
        cancel_rx: tokio::sync::watch::Receiver<bool>,
    ) -> BoxFuture<'a, Result<bool, PlatformError>> {
        let action_owned = action_label.to_string();
        let req_owned = requester_name.to_string();
        Box::pin(async move {
            let (tx, rx) = tokio::sync::oneshot::channel();
            std::thread::spawn(move || {
                let _ = tx.send(win_sys_pty::run_power_countdown_blocking(
                    action_owned,
                    req_owned,
                    countdown_secs,
                    cancel_rx,
                ));
            });
            rx.await
                .map_err(|_| PlatformError::System("电源倒计时线程意外退出".to_string()))?
        })
    }

    fn execute_power_action(&self, action: PlatformPowerAction) -> Result<(), PlatformError> {
        win_sys_pty::execute_power_action_impl(action)
    }

    fn open_pty_session(
        &self,
        prefer_powershell: bool,
        cols: u16,
        rows: u16,
        cwd: Option<&Path>,
    ) -> PtyOpenResult {
        win_sys_pty::open_pty_session_impl(prefer_powershell, cols, rows, cwd)
    }

    fn set_global_hotkey(
        &self,
        hotkey: Option<&str>,
        on_trigger: std::sync::Arc<dyn Fn() + Send + Sync>,
    ) -> Result<(), PlatformError> {
        self.hotkey_ctrl.set_hotkey(hotkey, on_trigger)
    }

    fn show_system_notification(&self, title: &str, body: &str) -> Result<(), PlatformError> {
        win_sys_pty::show_system_notification_impl(title, body)
    }

    fn start_file_drag_out<'a>(
        &'a self,
        paths: &'a [PathBuf],
    ) -> BoxFuture<'a, Result<bool, PlatformError>> {
        let owned = paths.to_vec();
        Box::pin(async move {
            let (tx, rx) = tokio::sync::oneshot::channel();
            std::thread::spawn(move || {
                let _ = tx.send(win_media_input::start_file_drag_out_sta(owned));
            });
            rx.await
                .map_err(|_| PlatformError::System("拖拽文件到外部线程意外退出".to_string()))?
        })
    }
}
