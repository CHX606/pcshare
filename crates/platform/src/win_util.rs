use std::ffi::{OsStr, c_void};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{Ordering, compiler_fence};

use windows::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, GetLastError, HANDLE, HWND, WIN32_ERROR,
};
use windows::Win32::Graphics::Gdi::{MONITOR_DEFAULTTONEAREST, MonitorFromWindow};
use windows::Win32::Security::{
    GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
};
use windows::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx, CoTaskMemFree,
    CoUninitialize,
};
use windows::Win32::System::Ole::{OleInitialize, OleUninitialize};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, DESKTOP_CONTROL_FLAGS, DESKTOP_READOBJECTS, GetUserObjectInformationW,
    OpenInputDesktop, UOI_NAME,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, OpenProcess, OpenProcessToken,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, GetDpiForWindow, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{
    GA_ROOT, GA_ROOTOWNER, GUITHREADINFO, GW_OWNER, GetAncestor, GetGUIThreadInfo,
    GetSystemMetrics, GetWindow, GetWindowThreadProcessId, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};
use windows::core::{HRESULT, PWSTR};

use crate::PlatformError;

/// 将 UTF-8 字符串编码为以 NUL 结尾的 UTF-16 缓冲。
pub(crate) fn wide_null(s: &str) -> Vec<u16> {
    OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// 去除 Windows 路径的 `\\?\` 或 `\\?\UNC\` 扩展前缀。
pub(crate) fn strip_verbatim_prefix(path: &Path) -> String {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        text.into_owned()
    }
}

/// 为绝对路径补上 `\\?\` 或 `\\?\UNC\` 扩展前缀以支持超长路径。
pub(crate) fn to_verbatim_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    if text.starts_with(r"\\?\") {
        text.into_owned()
    } else if let Some(rest) = text.strip_prefix(r"\\") {
        format!(r"\\?\UNC\{rest}")
    } else {
        format!(r"\\?\{text}")
    }
}

/// 读取并释放由 COM 分配的 `PWSTR`（空指针安全）。
pub(crate) unsafe fn pwstr_to_string_and_free(pwstr: PWSTR) -> String {
    if pwstr.0.is_null() {
        return String::new();
    }
    unsafe {
        let mut len = 0usize;
        while *pwstr.0.add(len) != 0 {
            len += 1;
        }
        let slice = std::slice::from_raw_parts(pwstr.0, len);
        let text = String::from_utf16_lossy(slice);
        CoTaskMemFree(Some(pwstr.0 as *const c_void));
        text
    }
}

/// 防止编译器优化掉的敏感内存清零。
pub(crate) fn zeroize_bytes(buf: &mut [u8]) {
    for b in buf.iter_mut() {
        unsafe {
            std::ptr::write_volatile(b, 0);
        }
    }
    compiler_fence(Ordering::SeqCst);
}

/// 判断 `windows::core::Error` 是否对应指定的 Win32 错误码。
pub(crate) fn is_win32_error(err: &windows::core::Error, expected: WIN32_ERROR) -> bool {
    err.code() == HRESULT::from_win32(expected.0)
}

/// Win32 内核句柄的 RAII 封装，析构时自动调用 `CloseHandle`。
pub(crate) struct OwnedHandle(HANDLE);

unsafe impl Send for OwnedHandle {}
unsafe impl Sync for OwnedHandle {}

impl OwnedHandle {
    pub(crate) fn new(handle: HANDLE) -> Option<Self> {
        if handle.is_invalid() {
            None
        } else {
            Some(Self(handle))
        }
    }

    pub(crate) fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
            self.0 = HANDLE::default();
        }
    }
}

/// STA COM 单元的 RAII 守卫：在失败早返回或 panic 时也确保配对 `CoUninitialize`。
/// 必须在所有 COM 接口局部变量之前声明，以保证接口先释放后再反初始化 COM。
pub(crate) struct ComGuard {
    initialized: bool,
}

impl ComGuard {
    pub(crate) fn init_sta() -> Self {
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
        Self {
            initialized: hr.is_ok(),
        }
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.initialized {
            unsafe {
                CoUninitialize();
            }
        }
    }
}

/// OLE 单元的 RAII 守卫：确保在任何早返回分支上都配对 `OleUninitialize`。
pub(crate) struct OleGuard {
    initialized: bool,
}

impl OleGuard {
    pub(crate) fn init() -> Result<Self, PlatformError> {
        unsafe { OleInitialize(None) }
            .map_err(|e| PlatformError::System(format!("初始化系统 OLE 拖拽环境失败：{e}")))?;
        Ok(Self { initialized: true })
    }
}

impl Drop for OleGuard {
    fn drop(&mut self) {
        if self.initialized {
            unsafe {
                OleUninitialize();
            }
        }
    }
}

/// 检查当前进程是否以管理员权限（提权令牌）运行。
pub(crate) fn is_elevated() -> bool {
    unsafe {
        let mut token_raw = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token_raw).is_err() {
            return false;
        }
        let Some(token) = OwnedHandle::new(token_raw) else {
            return false;
        };
        let mut elevation = TOKEN_ELEVATION::default();
        let mut ret_len = 0u32;
        let ok = GetTokenInformation(
            token.raw(),
            TokenElevation,
            Some(&mut elevation as *mut TOKEN_ELEVATION as *mut c_void),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut ret_len,
        );
        ok.is_ok() && elevation.TokenIsElevated != 0
    }
}

/// 查询当前正在接收输入的桌面名称（正常桌面为 `Some("Default")`，锁屏/UAC 为 `None` 或 `Some("Winlogon")`）。
pub(crate) fn current_input_desktop() -> Option<String> {
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

/// 实时检测当前输入桌面是否处于正常可交互的 `Default` 桌面。
pub(crate) fn is_default_input_desktop() -> bool {
    current_input_desktop()
        .as_deref()
        .is_some_and(|name| name.eq_ignore_ascii_case("Default"))
}

/// 读取指定窗口的缩放百分比（异常或系统返回 0 时回退至所在显示器 DPI 或 100）。
pub(crate) fn window_dpi_scale_percent(hwnd: HWND) -> u32 {
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    if dpi > 0 {
        return ((dpi * 100 + 48) / 96).max(100);
    }
    let hmon = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    if !hmon.0.is_null() {
        let mut dpi_x = 96u32;
        let mut dpi_y = 96u32;
        if unsafe { GetDpiForMonitor(hmon, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) }.is_ok()
            && dpi_x > 0
        {
            return ((dpi_x * 100 + 48) / 96).max(100);
        }
    }
    100
}

/// 将虚拟桌面单轴物理像素坐标换算到 `SendInput` 所需的 `0..=65535` 绝对坐标空间（使用 `i64` 防溢出、像素中心对齐无 1px 偏移）。
pub(crate) fn map_virtual_screen_axis(point: i32, origin: i32, span: i32) -> i32 {
    let start = i64::from(origin);
    let width = i64::from(span).max(1);
    (((i64::from(point) - start) * 65536 + 32768) / width).clamp(0, 65535) as i32
}

/// 将屏幕物理像素 `(screen_x, screen_y)` 换算为虚拟桌面 `0..=65535` 绝对坐标。
pub(crate) fn virtual_screen_to_absolute(screen_x: i32, screen_y: i32) -> (i32, i32) {
    unsafe {
        let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        (
            map_virtual_screen_axis(screen_x, vx, vw),
            map_virtual_screen_axis(screen_y, vy, vh),
        )
    }
}

/// 判断给定虚拟键码在 `SendInput` 中是否需要设置 `KEYEVENTF_EXTENDEDKEY`。
pub(crate) fn is_extended_vk(vk: u16) -> bool {
    matches!(
        vk,
        0x21..=0x28 | 0x2c..=0x2e | 0x5b..=0x5d | 0x6f | 0x90 | 0xa3 | 0xa5
    )
}

/// 检测目标窗口是否运行在高权限进程中（普通权限被控端会受 Windows UIPI 拦截）。
pub(crate) fn check_window_uipi_warning(hwnd: HWND) -> Option<String> {
    if hwnd.0.is_null() || is_elevated() {
        return None;
    }
    unsafe {
        let root = GetAncestor(hwnd, GA_ROOT);
        let target = if !root.0.is_null() { root } else { hwnd };
        let mut pid = 0u32;
        GetWindowThreadProcessId(target, Some(&mut pid));
        if pid == 0 || pid == GetCurrentProcessId() {
            return None;
        }
        let uipi_msg = "对方目标窗口以管理员权限运行，而当前被控端是普通权限，受 Windows 界面特权隔离（UIPI）限制无法向该窗口发送鼠标或键盘输入；请在对方电脑设置中开启管理员权限运行后再试。";
        match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            Ok(proc_raw) => {
                let proc_handle = OwnedHandle::new(proc_raw)?;
                let mut tok_raw = HANDLE::default();
                match OpenProcessToken(proc_handle.raw(), TOKEN_QUERY, &mut tok_raw) {
                    Ok(()) => {
                        let tok = OwnedHandle::new(tok_raw)?;
                        let mut elev = TOKEN_ELEVATION::default();
                        let mut ret_len = 0u32;
                        let target_elevated = GetTokenInformation(
                            tok.raw(),
                            TokenElevation,
                            Some(&mut elev as *mut TOKEN_ELEVATION as *mut c_void),
                            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                            &mut ret_len,
                        )
                        .is_ok()
                            && elev.TokenIsElevated != 0;
                        if target_elevated {
                            Some(uipi_msg.to_string())
                        } else {
                            None
                        }
                    }
                    Err(err) => {
                        if is_win32_error(&err, ERROR_ACCESS_DENIED)
                            || GetLastError() == ERROR_ACCESS_DENIED
                        {
                            Some(uipi_msg.to_string())
                        } else {
                            None
                        }
                    }
                }
            }
            Err(err) => {
                if is_win32_error(&err, ERROR_ACCESS_DENIED)
                    || GetLastError() == ERROR_ACCESS_DENIED
                {
                    Some(uipi_msg.to_string())
                } else {
                    None
                }
            }
        }
    }
}

fn is_same_or_owned_by(hwnd: HWND, target: HWND) -> bool {
    if hwnd.0.is_null() || target.0.is_null() {
        return false;
    }
    if hwnd == target {
        return true;
    }
    unsafe {
        let root = GetAncestor(hwnd, GA_ROOT);
        if root == target {
            return true;
        }
        if GetAncestor(hwnd, GA_ROOTOWNER) == target {
            return true;
        }
        let mut cur = if !root.0.is_null() { root } else { hwnd };
        for _ in 0..8 {
            let Ok(owner) = GetWindow(cur, GW_OWNER) else {
                break;
            };
            if owner.0.is_null() || owner == cur {
                break;
            }
            if owner == target {
                return true;
            }
            cur = owner;
        }
    }
    false
}

/// 判断落点/焦点窗口 `hit` 是否属于单窗口目标 `target`（包含其子控件、所属子对话框与右键弹出菜单）。
pub(crate) fn window_belongs_to_target(hit: HWND, target: HWND) -> bool {
    if is_same_or_owned_by(hit, target) {
        return true;
    }
    if hit.0.is_null() || target.0.is_null() {
        return false;
    }
    unsafe {
        let hit_tid = GetWindowThreadProcessId(hit, None);
        if hit_tid != 0 {
            let mut gti = GUITHREADINFO {
                cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
                ..Default::default()
            };
            if GetGUIThreadInfo(hit_tid, &mut gti).is_ok()
                && !gti.hwndMenuOwner.0.is_null()
                && is_same_or_owned_by(gti.hwndMenuOwner, target)
            {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_virtual_screen_axis_handles_negative_origin_extremes_and_edges() {
        // 标准 1920 宽显示器，首末像素落在像素中心且不越界
        assert_eq!(map_virtual_screen_axis(0, 0, 1920), 17);
        assert_eq!(map_virtual_screen_axis(1919, 0, 1920), 65518);
        // 反向验证 Windows 映射 floor(abs * span / 65536) == pixel
        for px in [0, 1, 959, 960, 1918, 1919] {
            let abs = map_virtual_screen_axis(px, 0, 1920) as i64;
            assert_eq!((abs * 1920) / 65536, px as i64);
        }
        // 多屏负坐标起点
        let abs_neg = map_virtual_screen_axis(-1920, -1920, 3840) as i64;
        assert_eq!((abs_neg * 3840) / 65536, 0);
        let abs_right = map_virtual_screen_axis(1919, -1920, 3840) as i64;
        assert_eq!((abs_right * 3840) / 65536, 3839);
        // i32 极值减法不溢出
        assert_eq!(map_virtual_screen_axis(i32::MAX, -1920, 3840), 65535);
        assert_eq!(map_virtual_screen_axis(i32::MIN, 1920, 3840), 0);
        // 异常跨度 <= 0 不除零
        assert_eq!(map_virtual_screen_axis(10, 0, 0), 65535);
    }

    #[test]
    fn extended_vk_classification_matches_windows_rules() {
        // 方向键、Home/End/PageUp/PageDown、Insert/Delete、Win 键、右 Ctrl/Alt 均为扩展键
        for vk in [
            0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x2c, 0x2d, 0x2e, 0x5b, 0x5c, 0x5d,
            0x6f, 0x90, 0xa3, 0xa5,
        ] {
            assert!(is_extended_vk(vk), "expected vk 0x{vk:02X} to be extended");
        }
        // 常规字母/数字/功能键/回车/Esc/普通修饰键不是扩展键
        for vk in [0x0D, 0x1B, 0x10, 0x11, 0x12, 0x41, 0x30, 0x70] {
            assert!(!is_extended_vk(vk), "expected vk 0x{vk:02X} not extended");
        }
    }

    #[test]
    fn verbatim_prefix_helpers_roundtrip_local_and_unc() {
        assert_eq!(
            strip_verbatim_prefix(Path::new(r"\\?\C:\foo\bar.txt")),
            r"C:\foo\bar.txt"
        );
        assert_eq!(
            strip_verbatim_prefix(Path::new(r"\\?\UNC\srv\share\a.txt")),
            r"\\srv\share\a.txt"
        );
        assert_eq!(
            to_verbatim_path(Path::new(r"C:\foo\bar.txt")),
            r"\\?\C:\foo\bar.txt"
        );
        assert_eq!(
            to_verbatim_path(Path::new(r"\\srv\share\a.txt")),
            r"\\?\UNC\srv\share\a.txt"
        );
    }

    #[test]
    fn zeroize_bytes_clears_buffer() {
        let mut data = vec![0xAAu8, 0x55, 0x12, 0x34];
        zeroize_bytes(&mut data);
        assert!(data.iter().all(|&b| b == 0));
    }
}
