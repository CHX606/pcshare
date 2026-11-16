//! Windows 专用部分：打开软件、枚举窗口、会话与权限信息、盘符。
//!
//! 直接声明系统接口，不依赖第三方库。以后支持其他系统时，只需要为这个文件
//! 提供对应的实现。

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;

type Handle = *mut c_void;

#[link(name = "shell32")]
unsafe extern "system" {
    fn ShellExecuteW(
        hwnd: Handle,
        operation: *const u16,
        file: *const u16,
        parameters: *const u16,
        directory: *const u16,
        show_cmd: i32,
    ) -> isize;
}

#[link(name = "ole32")]
unsafe extern "system" {
    fn CoInitializeEx(reserved: *const c_void, co_init: u32) -> i32;
    fn CoUninitialize();
}

#[link(name = "user32")]
unsafe extern "system" {
    fn EnumWindows(callback: unsafe extern "system" fn(Handle, isize) -> i32, lparam: isize) -> i32;
    fn IsWindowVisible(hwnd: Handle) -> i32;
    fn GetWindowTextLengthW(hwnd: Handle) -> i32;
    fn GetWindowTextW(hwnd: Handle, text: *mut u16, max_count: i32) -> i32;
    fn OpenInputDesktop(flags: u32, inherit: i32, desired_access: u32) -> Handle;
    fn CloseDesktop(desktop: Handle) -> i32;
    fn GetUserObjectInformationW(
        object: Handle,
        index: i32,
        info: *mut c_void,
        length: u32,
        length_needed: *mut u32,
    ) -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentProcess() -> Handle;
    fn GetCurrentProcessId() -> u32;
    fn ProcessIdToSessionId(process_id: u32, session_id: *mut u32) -> i32;
    fn WTSGetActiveConsoleSessionId() -> u32;
    fn GetLogicalDrives() -> u32;
    fn CloseHandle(handle: Handle) -> i32;
    fn GetLocalTime(time: *mut SystemTime);
}

#[repr(C)]
#[derive(Default)]
struct SystemTime {
    year: u16,
    month: u16,
    day_of_week: u16,
    day: u16,
    hour: u16,
    minute: u16,
    second: u16,
    milliseconds: u16,
}

/// 本地时间，格式 `时:分:秒`，用在被控端的滚动记录里。
pub fn local_time_hms() -> String {
    let mut time = SystemTime::default();
    unsafe { GetLocalTime(&mut time) };
    format!("{:02}:{:02}:{:02}", time.hour, time.minute, time.second)
}

#[link(name = "advapi32")]
unsafe extern "system" {
    fn OpenProcessToken(process: Handle, desired_access: u32, token: *mut Handle) -> i32;
    fn GetTokenInformation(
        token: Handle,
        class: i32,
        info: *mut c_void,
        length: u32,
        return_length: *mut u32,
    ) -> i32;
}

const SW_SHOWNORMAL: i32 = 1;
const COINIT_APARTMENTTHREADED: u32 = 0x2;
const COINIT_DISABLE_OLE1DDE: u32 = 0x4;
const DESKTOP_READOBJECTS: u32 = 0x0001;
const UOI_NAME: i32 = 2;
const TOKEN_QUERY: u32 = 0x0008;
const TOKEN_ELEVATION_CLASS: i32 = 20;

fn wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// 用系统的“打开”动作启动一个目标：程序名、完整路径、文档、快捷方式或
/// `shell:AppsFolder\...` 这样的应用标识都可以。
///
/// 必须在普通线程里调用（不要在异步任务里直接调），因为它会临时初始化 COM。
pub fn shell_open(target: &str, args: Option<&str>) -> Result<(), String> {
    let operation = wide("open");
    let file = wide(target);
    let params = args.filter(|a| !a.is_empty()).map(wide);
    let params_ptr = params.as_ref().map_or(std::ptr::null(), |p| p.as_ptr());

    // 有些打开方式（比如应用商店软件）要求线程先初始化 COM。
    let hr = unsafe {
        CoInitializeEx(
            std::ptr::null(),
            COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE,
        )
    };
    let code = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            operation.as_ptr(),
            file.as_ptr(),
            params_ptr,
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    if hr >= 0 {
        unsafe { CoUninitialize() };
    }

    if code > 32 {
        return Ok(());
    }
    let reason = match code {
        2 => "找不到这个程序或文件",
        3 => "找不到这个路径",
        5 => "没有权限",
        8 => "内存不足",
        26 | 28 | 29 | 30 => "系统忙，暂时打不开",
        27 | 31 => "这种文件没有关联的打开方式",
        32 => "缺少运行它需要的组件",
        _ => "系统拒绝了这次打开",
    };
    Err(format!("{reason}（系统错误码 {code}）"))
}

/// 一个可见的顶层窗口。
#[derive(Debug, Clone)]
pub struct WindowInfo {
    pub hwnd: usize,
    pub title: String,
}

unsafe extern "system" fn collect_window(hwnd: Handle, lparam: isize) -> i32 {
    // lparam 是 visible_windows 里那个 Vec 的地址，枚举期间一直有效。
    let list = unsafe { &mut *(lparam as *mut Vec<WindowInfo>) };
    unsafe {
        if IsWindowVisible(hwnd) == 0 {
            return 1;
        }
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return 1;
        }
        let mut buf = vec![0u16; len as usize + 1];
        let copied = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
        if copied <= 0 {
            return 1;
        }
        list.push(WindowInfo {
            hwnd: hwnd as usize,
            title: String::from_utf16_lossy(&buf[..copied as usize]),
        });
    }
    1
}

/// 当前桌面上所有带标题的可见窗口。
pub fn visible_windows() -> Vec<WindowInfo> {
    let mut list: Vec<WindowInfo> = Vec::new();
    unsafe {
        EnumWindows(collect_window, &mut list as *mut Vec<WindowInfo> as isize);
    }
    list
}

/// 本进程所处的登录环境，决定了打开的软件有没有窗口。
#[derive(Debug, Clone)]
pub struct SessionInfo {
    /// 本进程所在的会话编号。0 表示系统服务会话，那里打开的软件没有窗口。
    pub session_id: u32,
    /// 当前连着显示器和键盘的那个会话编号。
    pub console_session_id: u32,
    /// 是否以管理员权限运行。
    pub elevated: bool,
    /// 当前接收键盘鼠标的桌面名称。正常是 `Default`；锁屏或弹出权限确认框时
    /// 普通程序读不到，这里就是 `None`。
    pub input_desktop: Option<String>,
}

pub fn session_info() -> SessionInfo {
    let mut session_id = 0u32;
    unsafe {
        ProcessIdToSessionId(GetCurrentProcessId(), &mut session_id);
    }
    let console_session_id = unsafe { WTSGetActiveConsoleSessionId() };
    SessionInfo {
        session_id,
        console_session_id,
        elevated: is_elevated(),
        input_desktop: input_desktop_name(),
    }
}

fn is_elevated() -> bool {
    unsafe {
        let mut token: Handle = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut elevation = 0u32;
        let mut returned = 0u32;
        let ok = GetTokenInformation(
            token,
            TOKEN_ELEVATION_CLASS,
            &mut elevation as *mut u32 as *mut c_void,
            std::mem::size_of::<u32>() as u32,
            &mut returned,
        );
        CloseHandle(token);
        ok != 0 && elevation != 0
    }
}

fn input_desktop_name() -> Option<String> {
    unsafe {
        let desktop = OpenInputDesktop(0, 0, DESKTOP_READOBJECTS);
        if desktop.is_null() {
            return None;
        }
        let mut buf = [0u16; 256];
        let mut needed = 0u32;
        let ok = GetUserObjectInformationW(
            desktop,
            UOI_NAME,
            buf.as_mut_ptr() as *mut c_void,
            std::mem::size_of_val(&buf) as u32,
            &mut needed,
        );
        CloseDesktop(desktop);
        if ok == 0 {
            return None;
        }
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..len]))
    }
}

/// 本机所有盘符，例如 `["C:\\", "E:\\"]`。
pub fn logical_drives() -> Vec<String> {
    let mask = unsafe { GetLogicalDrives() };
    (0..26u8)
        .filter(|i| mask & (1 << i) != 0)
        .map(|i| format!("{}:\\", (b'A' + i) as char))
        .collect()
}
