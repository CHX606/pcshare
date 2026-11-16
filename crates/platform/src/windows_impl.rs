use std::collections::HashMap;
use std::ffi::{OsStr, c_void};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::{
    CloseHandle, FILETIME, HANDLE, HLOCAL, HWND, LPARAM, LocalFree, SIZE,
};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC,
    DeleteObject, GetDIBits, GetObjectW, HGDIOBJ,
};
use windows::Win32::Security::Cryptography::{
    CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
};
use windows::Win32::Security::{
    GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
};
use windows::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, GetLogicalDrives, GetVolumeInformationW,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoCreateInstance,
    CoInitializeEx, CoTaskMemFree, CoUninitialize, IBindCtx,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows::Win32::System::RemoteDesktop::{
    ProcessIdToSessionId, WTSGetActiveConsoleSessionId,
};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, DESKTOP_CONTROL_FLAGS, DESKTOP_READOBJECTS, GetUserObjectInformationW,
    OpenInputDesktop, UOI_NAME,
};
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, GetProcessTimes, GetSystemTimes, OpenProcess,
    OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE, TerminateProcess,
};
use windows::Win32::UI::Shell::{
    BHID_EnumItems, FO_DELETE, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT,
    FOLDERID_AppsFolder, FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_Downloads,
    FOLDERID_Profile, FOS_ALLOWMULTISELECT, FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM,
    FOS_PICKFOLDERS, FileOpenDialog, IEnumShellItems, IFileOpenDialog, IShellItem,
    IShellItemImageFactory, KF_FLAG_DEFAULT, SHCreateItemFromParsingName, SHFILEOPSTRUCTW,
    SHFileOperationW, SHGetKnownFolderItem, SHGetKnownFolderPath, SIGDN_FILESYSPATH,
    SIGDN_NORMALDISPLAY, SIGDN_PARENTRELATIVEPARSING, SIIGBF_BIGGERSIZEOK, SIIGBF_ICONONLY,
    ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
    SW_SHOWNORMAL,
};
use windows::core::{BOOL, GUID, PCWSTR};

use crate::{
    AutostartState, BoxFuture, CapturableWindowInfo, CapturedImage, ClipboardData,
    DesktopEnvironment, DriveInfo, InputOutcome, InstalledAppInfo, Platform, PlatformError,
    PlatformMonitor, PlatformMouseBtn, PlatformPowerAction, ProcessSummary, PtyOpenResult,
    ShellOpenOutcome, SpecialFolder, SpecialFolderKind, SystemMemory, WindowSummary,
    win_media_input, win_sys_pty,
};

const APP_CACHE_TTL: Duration = Duration::from_secs(5 * 60);
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const RUN_REG_PATH: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_REG_VALUE: &str = "PcShare";
const SCHTASK_NAME: &str = "PcShareAutostart";

struct CpuSample {
    system_total_100ns: u64,
    proc_times_100ns: HashMap<u32, u64>,
}

pub struct WindowsPlatform {
    apps_cache: tokio::sync::Mutex<Option<(Instant, u64, Vec<InstalledAppInfo>)>>,
    icons_cache: Mutex<HashMap<String, Option<Vec<u8>>>>,
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
            icons_cache: Mutex::new(HashMap::new()),
            cpu_sample: Mutex::new(None),
            hotkey_ctrl: win_sys_pty::HotkeyController::default(),
        }
    }
}

fn wide_null(s: &str) -> Vec<u16> {
    OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn strip_verbatim_prefix(path: &Path) -> String {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        text.into_owned()
    }
}

fn filetime_to_u64(ft: FILETIME) -> u64 {
    ((ft.dwHighDateTime as u64) << 32) | (ft.dwLowDateTime as u64)
}

fn is_critical_process_name(pid: u32, name: &str, self_pid: u32) -> bool {
    if pid <= 4 || pid == self_pid {
        return true;
    }
    matches!(
        name.to_ascii_lowercase().as_str(),
        "system"
            | "registry"
            | "smss.exe"
            | "csrss.exe"
            | "wininit.exe"
            | "services.exe"
            | "lsass.exe"
            | "winlogon.exe"
            | "dwm.exe"
            | "fontdrvhost.exe"
            | "memory compression"
    )
}

fn is_elevated() -> bool {
    unsafe {
        let mut token = Default::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut ret_len = 0u32;
        let res = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut TOKEN_ELEVATION as *mut c_void),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut ret_len,
        );
        let _ = CloseHandle(token);
        res.is_ok() && elevation.TokenIsElevated != 0
    }
}

fn current_input_desktop() -> Option<String> {
    unsafe {
        let desktop =
            OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_READOBJECTS).ok()?;
        let mut buf = [0u16; 256];
        let mut needed = 0u32;
        let ok = GetUserObjectInformationW(
            HANDLE(desktop.0),
            UOI_NAME,
            Some(buf.as_mut_ptr() as *mut c_void),
            std::mem::size_of_val(&buf) as u32,
            Some(&mut needed),
        );
        let _ = CloseDesktop(desktop);
        ok.ok()?;
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..len]))
    }
}

unsafe extern "system" fn collect_visible_window(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let list = unsafe { &mut *(lparam.0 as *mut Vec<WindowSummary>) };
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() {
            return BOOL(1);
        }
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return BOOL(1);
        }
        let mut buf = vec![0u16; len as usize + 1];
        let copied = GetWindowTextW(hwnd, &mut buf);
        if copied <= 0 {
            return BOOL(1);
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        list.push(WindowSummary {
            hwnd: hwnd.0 as usize,
            pid,
            title: String::from_utf16_lossy(&buf[..copied as usize]),
        });
    }
    BOOL(1)
}

fn known_folder_path(folder_id: &GUID) -> Option<PathBuf> {
    unsafe {
        let pwstr = SHGetKnownFolderPath(folder_id, KF_FLAG_DEFAULT, None).ok()?;
        let mut len = 0usize;
        while *pwstr.0.add(len) != 0 {
            len += 1;
        }
        let slice = std::slice::from_raw_parts(pwstr.0, len);
        let path_str = String::from_utf16_lossy(slice);
        CoTaskMemFree(Some(pwstr.0 as *const c_void));
        Some(PathBuf::from(path_str))
    }
}

fn shell_execute_blocking(target: String, args: Option<String>) -> Result<(), PlatformError> {
    let op = wide_null("open");
    let file = wide_null(&target);
    let params = args.filter(|a| !a.is_empty()).map(|a| wide_null(&a));
    let params_pcwstr = params
        .as_ref()
        .map_or(PCWSTR::null(), |p| PCWSTR(p.as_ptr()));

    let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
    let instance = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(op.as_ptr()),
            PCWSTR(file.as_ptr()),
            params_pcwstr,
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    if hr.is_ok() {
        unsafe { CoUninitialize() };
    }

    let code = instance.0 as isize;
    if code > 32 {
        return Ok(());
    }
    match code {
        2 => Err(PlatformError::NotFound(format!(
            "找不到这个程序或文件：{target}"
        ))),
        3 => Err(PlatformError::NotFound(format!(
            "找不到这个路径：{target}"
        ))),
        5 => Err(PlatformError::ElevationRequired(
            "打开该程序需要管理员权限，或系统拒绝访问".to_string(),
        )),
        8 => Err(PlatformError::System("对方电脑内存不足，无法启动程序".to_string())),
        26 | 28 | 29 | 30 => Err(PlatformError::Busy("系统正忙，暂时打不开".to_string())),
        27 | 31 => Err(PlatformError::InvalidInput(
            "这种文件在对方电脑上没有关联的打开方式".to_string(),
        )),
        32 => Err(PlatformError::Busy(
            "目标文件或组件正被占用，暂时无法打开".to_string(),
        )),
        _ => Err(PlatformError::System(format!(
            "系统拒绝了这次打开（错误码 {code}）"
        ))),
    }
}

fn system_and_proc_times() -> (u64, HashMap<u32, (String, u64, u64)>) {
    let mut sys_total = 0u64;
    unsafe {
        let mut idle = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        if GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)).is_ok() {
            sys_total = filetime_to_u64(kernel).saturating_add(filetime_to_u64(user));
        }
    }

    let mut procs = HashMap::new();
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return (sys_total, procs);
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                let pid = entry.th32ProcessID;
                let name_len = entry
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(entry.szExeFile.len());
                let name = String::from_utf16_lossy(&entry.szExeFile[..name_len]);

                let mut mem_bytes = 0u64;
                let mut proc_time = 0u64;
                if pid > 0
                    && let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)
                {
                    let mut pmc = PROCESS_MEMORY_COUNTERS {
                        cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
                        ..Default::default()
                    };
                    if K32GetProcessMemoryInfo(
                        handle,
                        &mut pmc,
                        std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
                    )
                    .as_bool()
                    {
                        mem_bytes = pmc.WorkingSetSize as u64;
                    }

                    let mut create = FILETIME::default();
                    let mut exit = FILETIME::default();
                    let mut ktime = FILETIME::default();
                    let mut utime = FILETIME::default();
                    if GetProcessTimes(handle, &mut create, &mut exit, &mut ktime, &mut utime)
                        .is_ok()
                    {
                        proc_time = filetime_to_u64(ktime).saturating_add(filetime_to_u64(utime));
                    }
                    let _ = CloseHandle(handle);
                }
                procs.insert(pid, (name, mem_bytes, proc_time));

                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
    }
    (sys_total, procs)
}

fn enumerate_start_apps_native_sta() -> Result<Vec<InstalledAppInfo>, PlatformError> {
    unsafe {
        let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
        let need_uninit = hr.is_ok();
        let res = (|| -> Result<Vec<InstalledAppInfo>, PlatformError> {
            let folder: IShellItem =
                SHGetKnownFolderItem(&FOLDERID_AppsFolder, KF_FLAG_DEFAULT, None)
                    .map_err(|e| PlatformError::System(format!("无法打开系统应用程序目录：{e}")))?;
            let enumerator: IEnumShellItems = folder
                .BindToHandler(None::<&IBindCtx>, &BHID_EnumItems)
                .map_err(|e| PlatformError::System(format!("无法枚举已安装软件：{e}")))?;

            let mut seen = std::collections::HashSet::new();
            let mut apps = Vec::with_capacity(256);
            loop {
                let mut batch: [Option<IShellItem>; 32] = Default::default();
                let mut fetched = 0u32;
                let _ = enumerator.Next(&mut batch, Some(&mut fetched));
                if fetched == 0 {
                    break;
                }
                for item in batch.into_iter().take(fetched as usize).flatten() {
                    let name_pw = item.GetDisplayName(SIGDN_NORMALDISPLAY);
                    let id_pw = item.GetDisplayName(SIGDN_PARENTRELATIVEPARSING);
                    let name = if let Ok(pw) = name_pw {
                        let s = pw.to_string().unwrap_or_default();
                        CoTaskMemFree(Some(pw.0 as *const c_void));
                        s
                    } else {
                        String::new()
                    };
                    let app_id = if let Ok(pw) = id_pw {
                        let s = pw.to_string().unwrap_or_default();
                        CoTaskMemFree(Some(pw.0 as *const c_void));
                        s
                    } else {
                        String::new()
                    };
                    let name_trim = name.trim();
                    let id_trim = app_id.trim();
                    if !name_trim.is_empty() && !id_trim.is_empty() && seen.insert(id_trim.to_string()) {
                        apps.push(InstalledAppInfo {
                            name: name_trim.to_string(),
                            app_id: id_trim.to_string(),
                        });
                    }
                }
            }
            apps.sort_by_key(|a| a.name.to_lowercase());
            Ok(apps)
        })();
        if need_uninit {
            CoUninitialize();
        }
        res
    }
}

fn compute_installed_apps_fingerprint() -> u64 {
    fn hash_dir_shallow(h: &mut u64, dir: &Path, depth: u8) {
        let Ok(meta) = std::fs::metadata(dir) else {
            return;
        };
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        *h = h.wrapping_mul(1099511628211).wrapping_add(mtime ^ 0x9e3779b97f4a7c15);
        if depth == 0 {
            return;
        }
        if let Ok(rd) = std::fs::read_dir(dir) {
            let mut count = 0u64;
            for entry in rd.flatten() {
                count += 1;
                if depth > 1
                    && let Ok(ft) = entry.file_type()
                    && ft.is_dir()
                {
                    hash_dir_shallow(h, &entry.path(), depth - 1);
                }
            }
            *h = h.wrapping_mul(1099511628211).wrapping_add(count);
        }
    }

    let mut fp = 14695981039346656037u64;
    if let Some(pd) = std::env::var_os("ProgramData") {
        hash_dir_shallow(
            &mut fp,
            &PathBuf::from(pd).join(r"Microsoft\Windows\Start Menu\Programs"),
            2,
        );
    }
    if let Some(ad) = std::env::var_os("APPDATA") {
        hash_dir_shallow(
            &mut fp,
            &PathBuf::from(ad).join(r"Microsoft\Windows\Start Menu\Programs"),
            2,
        );
    }
    if let Some(lad) = std::env::var_os("LOCALAPPDATA") {
        hash_dir_shallow(&mut fp, &PathBuf::from(lad).join("Packages"), 1);
    }
    fp
}

#[derive(Serialize, Deserialize)]
struct PersistedAppsIndex {
    fingerprint: u64,
    apps: Vec<InstalledAppInfo>,
}

fn apps_index_cache_path() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("PcShare")
        .join("apps_index_v2.json")
}

fn extract_app_icon_png_sta(app_id: &str) -> Option<Vec<u8>> {
    unsafe {
        let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
        let need_uninit = hr.is_ok();
        let out = (|| -> Option<Vec<u8>> {
            let parsing = wide_null(&format!(r"shell:AppsFolder\{app_id}"));
            let factory: IShellItemImageFactory =
                SHCreateItemFromParsingName(PCWSTR(parsing.as_ptr()), None::<&IBindCtx>).ok()?;
            let hbm = factory
                .GetImage(
                    SIZE { cx: 32, cy: 32 },
                    SIIGBF_ICONONLY | SIIGBF_BIGGERSIZEOK,
                )
                .ok()?;

            let mut bm = BITMAP::default();
            let got_obj = GetObjectW(
                HGDIOBJ(hbm.0),
                std::mem::size_of::<BITMAP>() as i32,
                Some(&mut bm as *mut _ as *mut c_void),
            );
            if got_obj == 0 || bm.bmWidth <= 0 || bm.bmHeight <= 0 {
                let _ = DeleteObject(HGDIOBJ(hbm.0));
                return None;
            }
            let w = bm.bmWidth as usize;
            let h = bm.bmHeight.unsigned_abs() as usize;

            let hdc = CreateCompatibleDC(None);
            if hdc.is_invalid() {
                let _ = DeleteObject(HGDIOBJ(hbm.0));
                return None;
            }

            let mut bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w as i32,
                    biHeight: -(h as i32), // top-down DIB
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut pixels = vec![0u8; w * h * 4];
            let lines = GetDIBits(
                hdc,
                hbm,
                0,
                h as u32,
                Some(pixels.as_mut_ptr() as *mut c_void),
                &mut bmi,
                DIB_RGB_COLORS,
            );
            let _ = DeleteDC(hdc);
            let _ = DeleteObject(HGDIOBJ(hbm.0));
            if lines == 0 {
                return None;
            }

            // BGRA -> RGBA（若全图 alpha 均为 0，说明是传统 24 位图标转成 32 位，补不透明 alpha=255）
            let any_alpha = pixels.as_chunks::<4>().0.iter().any(|px| px[3] != 0);
            for px in pixels.as_chunks_mut::<4>().0 {
                px.swap(0, 2);
                if !any_alpha {
                    px[3] = 255;
                }
            }

            let mut png_bytes = Vec::with_capacity(2048);
            {
                let mut encoder = png::Encoder::new(&mut png_bytes, w as u32, h as u32);
                encoder.set_color(png::ColorType::Rgba);
                encoder.set_depth(png::BitDepth::Eight);
                let mut writer = encoder.write_header().ok()?;
                writer.write_image_data(&pixels).ok()?;
            }
            Some(png_bytes)
        })();
        if need_uninit {
            CoUninitialize();
        }
        out
    }
}

async fn query_start_apps(
    expected_fp: u64,
    allow_disk_index: bool,
) -> Result<Vec<InstalledAppInfo>, PlatformError> {
    let cache_path = apps_index_cache_path();
    if allow_disk_index
        && let Ok(raw) = std::fs::read_to_string(&cache_path)
        && let Ok(parsed) = serde_json::from_str::<PersistedAppsIndex>(&raw)
        && parsed.fingerprint == expected_fp
        && !parsed.apps.is_empty()
    {
        return Ok(parsed.apps);
    }

    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let _ = tx.send(enumerate_start_apps_native_sta());
    });
    let apps = rx
        .await
        .map_err(|_| PlatformError::System("枚举已安装软件的后台线程意外中止".to_string()))??;

    if let Some(parent) = cache_path.parent() {
        let _ = std::fs::create_dir_all(parent);
        if let Ok(json) = serde_json::to_string(&PersistedAppsIndex {
            fingerprint: expected_fp,
            apps: apps.clone(),
        }) {
            let _ = std::fs::write(&cache_path, json);
        }
    }
    Ok(apps)
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
            GlobalMemoryStatusEx(&mut status).map_err(|err| {
                PlatformError::System(format!("读取系统内存信息失败：{err}"))
            })?;
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
                Ok(Err(_)) => Err(PlatformError::System("打开目标的后台线程意外退出".to_string())),
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
                        Ok(Err(_)) => {
                            Err(PlatformError::System("打开目标的后台线程意外退出".to_string()))
                        }
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
        // SHFileOperationW 要求完整的绝对路径（不带 \\?\ 前缀），并以双 NUL 结尾。
        let mut double_null_buf: Vec<u16> = Vec::new();
        for p in paths {
            if !p.exists() {
                return Err(PlatformError::NotFound(format!(
                    "找不到要删除的文件或文件夹：{}",
                    p.display()
                )));
            }
            let canonical = std::fs::canonicalize(p).unwrap_or_else(|_| p.clone());
            let clean = strip_verbatim_prefix(&canonical);
            double_null_buf.extend(OsStr::new(&clean).encode_wide());
            double_null_buf.push(0);
        }
        double_null_buf.push(0);

        let mut file_op = SHFILEOPSTRUCTW {
            hwnd: HWND::default(),
            wFunc: FO_DELETE,
            pFrom: PCWSTR(double_null_buf.as_ptr()),
            pTo: PCWSTR::null(),
            fFlags: (FOF_ALLOWUNDO.0 | FOF_NOCONFIRMATION.0 | FOF_NOERRORUI.0 | FOF_SILENT.0)
                as u16,
            ..Default::default()
        };

        let code = unsafe { SHFileOperationW(&mut file_op) };
        if code != 0 {
            return Err(PlatformError::System(format!(
                "无法将文件移入回收站（系统错误码 {code}）"
            )));
        }
        if file_op.fAnyOperationsAborted.as_bool() {
            return Err(PlatformError::System("移入回收站的操作被系统中止".to_string()));
        }
        Ok(())
    }

    fn installed_apps(
        &self,
        force_refresh: bool,
    ) -> BoxFuture<'_, Result<Vec<InstalledAppInfo>, PlatformError>> {
        Box::pin(async move {
            let mut guard = self.apps_cache.lock().await;
            let live_fp = compute_installed_apps_fingerprint();
            if let Some((cached_at, cached_fp, list)) = guard.as_ref() {
                if !force_refresh && cached_at.elapsed() < APP_CACHE_TTL && *cached_fp == live_fp {
                    return Ok(list.clone());
                }
                if force_refresh && *cached_fp == live_fp {
                    return Ok(list.clone());
                }
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
                .cloned()
            {
                return Ok(cached);
            }
            let id_clone = id.clone();
            let (tx, rx) = tokio::sync::oneshot::channel();
            std::thread::spawn(move || {
                let _ = tx.send(extract_app_icon_png_sta(&id_clone));
            });
            let png = rx.await.unwrap_or(None);
            self.icons_cache
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(id, png.clone());
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

        let (mut sys_total, mut snap) = system_and_proc_times_wrapper();
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
            let second = system_and_proc_times_wrapper();
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

        let (_, snap) = system_and_proc_times_wrapper();
        let Some((name, _, _)) = snap.get(&pid).cloned() else {
            return Err(PlatformError::NotFound(format!(
                "找不到进程（PID {pid}），它可能已经退出"
            )));
        };

        if is_critical_process_name(pid, &name, self_pid) {
            return Err(PlatformError::PermissionDenied(format!(
                "“{name}”是系统关键进程，为防止电脑蓝屏或死机，不允许结束"
            )));
        }

        unsafe {
            let handle = OpenProcess(PROCESS_TERMINATE, false, pid).map_err(|err| {
                PlatformError::PermissionDenied(format!(
                    "没有权限结束进程“{name}”（PID {pid}）：{err}"
                ))
            })?;
            let term_res = TerminateProcess(handle, 1);
            let _ = CloseHandle(handle);
            term_res.map_err(|err| {
                PlatformError::PermissionDenied(format!("结束进程“{name}”失败：{err}"))
            })?;
        }
        Ok(name)
    }

    fn protect_secret(&self, plaintext: &[u8]) -> Result<Vec<u8>, PlatformError> {
        let mut input = plaintext.to_vec();
        let in_blob = CRYPT_INTEGER_BLOB {
            cbData: input.len() as u32,
            pbData: input.as_mut_ptr(),
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

            let encrypted =
                std::slice::from_raw_parts(out_blob.pbData, out_blob.cbData as usize).to_vec();
            let _ = LocalFree(Some(HLOCAL(out_blob.pbData as *mut c_void)));
            Ok(encrypted)
        }
    }

    fn unprotect_secret(&self, ciphertext: &[u8]) -> Result<Vec<u8>, PlatformError> {
        if ciphertext.is_empty() {
            return Err(PlatformError::System("密钥密文为空".to_string()));
        }
        let mut input = ciphertext.to_vec();
        let in_blob = CRYPT_INTEGER_BLOB {
            cbData: input.len() as u32,
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
            .map_err(|err| PlatformError::System(format!("解密密钥失败（文件可能已损坏）：{err}")))?;

            let decrypted =
                std::slice::from_raw_parts(out_blob.pbData, out_blob.cbData as usize).to_vec();
            let _ = LocalFree(Some(HLOCAL(out_blob.pbData as *mut c_void)));
            Ok(decrypted)
        }
    }

    fn get_autostart(&self) -> Result<AutostartState, PlatformError> {
        use std::os::windows::process::CommandExt;

        // 先检查计划任务（管理员权限自启）
        if let Ok(output) = std::process::Command::new("schtasks.exe")
            .args(["/Query", "/TN", SCHTASK_NAME])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            && output.status.success()
        {
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

        // 先清理旧的注册表项和计划任务，避免重复启动
        if let Ok(key) = windows_registry::CURRENT_USER.create(RUN_REG_PATH) {
            let _ = key.remove_value(RUN_REG_VALUE);
        }
        let _ = std::process::Command::new("schtasks.exe")
            .args(["/Delete", "/TN", SCHTASK_NAME, "/F"])
            .creation_flags(CREATE_NO_WINDOW)
            .output();

        if !enabled {
            return Ok(());
        }

        let clean_exe = strip_verbatim_prefix(exe_path);
        let quoted_cmd = format!("\"{clean_exe}\" --background");
        if elevated {
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
        } else {
            let key = windows_registry::CURRENT_USER
                .create(RUN_REG_PATH)
                .map_err(|err| PlatformError::System(format!("打开自启动注册表项失败：{err}")))?;
            key.set_string(RUN_REG_VALUE, &quoted_cmd)
                .map_err(|err| PlatformError::System(format!("写入自启动注册表项失败：{err}")))?;
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

fn shell_item_to_path(item: &IShellItem) -> Option<PathBuf> {
    unsafe {
        let pwstr = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let mut len = 0usize;
        while *pwstr.0.add(len) != 0 {
            len += 1;
        }
        let slice = std::slice::from_raw_parts(pwstr.0, len);
        let text = String::from_utf16_lossy(slice);
        CoTaskMemFree(Some(pwstr.0 as *const c_void));
        Some(PathBuf::from(text))
    }
}

fn run_file_dialog_sta(pick_folder: bool) -> Result<Vec<PathBuf>, PlatformError> {
    const HRESULT_CANCELLED: i32 = 0x800704C7u32 as i32; // ERROR_CANCELLED (1223)

    let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
    let res = unsafe {
        let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)
            .map_err(|e| PlatformError::System(format!("创建文件选择对话框失败：{e}")))?;

        let mut opts = dialog
            .GetOptions()
            .map_err(|e| PlatformError::System(format!("读取对话框选项失败：{e}")))?;
        opts |= FOS_FORCEFILESYSTEM | FOS_FILEMUSTEXIST;
        if pick_folder {
            opts |= FOS_PICKFOLDERS;
        } else {
            opts |= FOS_ALLOWMULTISELECT;
        }
        dialog
            .SetOptions(opts)
            .map_err(|e| PlatformError::System(format!("设置对话框选项失败：{e}")))?;

        match dialog.Show(None) {
            Ok(()) => {
                let mut out = Vec::new();
                if pick_folder {
                    if let Ok(item) = dialog.GetResult()
                        && let Some(p) = shell_item_to_path(&item)
                    {
                        out.push(p);
                    }
                } else {
                    let items = dialog
                        .GetResults()
                        .map_err(|e| PlatformError::System(format!("读取已选文件失败：{e}")))?;
                    let count = items.GetCount().unwrap_or(0);
                    for idx in 0..count {
                        if let Ok(item) = items.GetItemAt(idx)
                            && let Some(p) = shell_item_to_path(&item)
                        {
                            out.push(p);
                        }
                    }
                }
                Ok(out)
            }
            Err(err) if err.code().0 == HRESULT_CANCELLED => Ok(Vec::new()),
            Err(err) => Err(PlatformError::System(format!("显示文件选择框失败：{err}"))),
        }
    };
    if hr.is_ok() {
        unsafe { CoUninitialize() };
    }
    res
}

fn system_and_proc_times_wrapper() -> (u64, HashMap<u32, (String, u64, u64)>) {
    system_and_proc_times()
}
