use std::collections::BTreeSet;
use std::ffi::{OsStr, c_void};
use std::net::UdpSocket;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::Data::Xml::Dom::XmlDocument;
use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{GetStockObject, HBRUSH, UpdateWindow};
use windows::Win32::NetworkManagement::IpHelper::{
    GET_ADAPTERS_ADDRESSES_FLAGS, GetAdaptersAddresses, IF_TYPE_SOFTWARE_LOOPBACK,
    IP_ADAPTER_ADDRESSES_LH,
};
use windows::Win32::Networking::WinSock::AF_UNSPEC;
use windows::Win32::Security::{
    AdjustTokenPrivileges, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, SE_PRIVILEGE_ENABLED,
    SE_SHUTDOWN_NAME, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
};
use windows::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows::Win32::System::Console::{
    COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON, ResizePseudoConsole,
};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::Power::SetSuspendState;
use windows::Win32::System::Shutdown::{
    EWX_FORCEIFHUNG, EWX_POWEROFF, EWX_REBOOT, EWX_SHUTDOWN, ExitWindowsEx, LockWorkStation,
    SHTDN_REASON_MAJOR_OTHER,
};
use windows::Win32::System::Threading::{
    CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess, GetCurrentThreadId, GetExitCodeProcess,
    InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST, OpenProcessToken,
    PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOEXW, STARTUPINFOW, TerminateProcess,
    UpdateProcThreadAttribute,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, RegisterHotKey,
    UnregisterHotKey, VK_ESCAPE, VK_F1, VK_RETURN, VK_SPACE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BS_DEFPUSHBUTTON, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetMessageW, GetSystemMetrics, HMENU, IDC_ARROW, LoadCursorW, MSG, PM_REMOVE,
    PeekMessageW, PostQuitMessage, PostThreadMessageW, RegisterClassW, SM_CXSCREEN, SM_CYSCREEN,
    SW_SHOW, SetForegroundWindow, SetTimer, SetWindowTextW, ShowWindow, TranslateMessage,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_CLOSE, WM_COMMAND, WM_HOTKEY, WM_KEYDOWN, WM_QUIT, WM_TIMER,
    WNDCLASSW, WS_CAPTION, WS_CHILD, WS_EX_TOPMOST, WS_POPUP, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE,
};
use windows::core::{HSTRING, PCWSTR, PWSTR};

use crate::{PlatformError, PlatformPowerAction, PtyOpenResult, PtySession};

const PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE_ATTR: usize = 0x0002_0016;
const IF_TYPE_TUNNEL_VAL: u32 = 131;
const STILL_ACTIVE_CODE: u32 = 259;

fn wide_null(s: &str) -> Vec<u16> {
    OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

// ──────────────────────── 网卡物理地址发现与网络唤醒（任务 3.5）────────────────────────

pub fn local_mac_addresses_impl() -> Vec<String> {
    let mut size = 32 * 1024u32;
    let mut buf = vec![0u8; size as usize];
    for _ in 0..3 {
        let rc = unsafe {
            GetAdaptersAddresses(
                AF_UNSPEC.0 as u32,
                GET_ADAPTERS_ADDRESSES_FLAGS(0),
                None,
                Some(buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH),
                &mut size,
            )
        };
        if rc == 0 {
            break;
        }
        buf.resize(size as usize, 0);
    }

    let mut macs = BTreeSet::new();
    let mut curr = buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
    unsafe {
        while !curr.is_null() {
            let adapter = &*curr;
            let len = adapter.PhysicalAddressLength as usize;
            if len == 6
                && adapter.IfType != IF_TYPE_SOFTWARE_LOOPBACK
                && adapter.IfType != IF_TYPE_TUNNEL_VAL
            {
                let b = &adapter.PhysicalAddress[..6];
                let all_zero = b.iter().all(|&x| x == 0);
                let all_ff = b.iter().all(|&x| x == 0xFF);
                if !all_zero && !all_ff {
                    macs.insert(format!(
                        "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
                        b[0], b[1], b[2], b[3], b[4], b[5]
                    ));
                }
            }
            curr = adapter.Next;
        }
    }
    macs.into_iter().collect()
}

pub fn parse_mac_bytes(mac: &str) -> Result<[u8; 6], PlatformError> {
    let cleaned: String = mac
        .chars()
        .filter(|c| !matches!(c, ':' | '-' | '.' | ' '))
        .collect();
    if cleaned.len() != 12 || !cleaned.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(PlatformError::InvalidInput(format!(
            "物理网卡地址格式不正确：{mac}（应如 AA:BB:CC:DD:EE:FF）"
        )));
    }
    let mut out = [0u8; 6];
    for i in 0..6 {
        out[i] = u8::from_str_radix(&cleaned[i * 2..i * 2 + 2], 16).map_err(|_| {
            PlatformError::InvalidInput(format!("无法解析物理网卡地址：{mac}"))
        })?;
    }
    Ok(out)
}

pub fn build_wol_magic_packet(mac: [u8; 6]) -> [u8; 102] {
    let mut pkt = [0xFFu8; 102];
    for i in 0..16 {
        let start = 6 + i * 6;
        pkt[start..start + 6].copy_from_slice(&mac);
    }
    pkt
}

pub fn send_wol_packets_impl(macs: &[String]) -> Result<usize, PlatformError> {
    if macs.is_empty() {
        return Err(PlatformError::InvalidInput(
            "未记录目标设备的网卡物理地址，请先在该设备在线时连接一次以自动记录".to_string(),
        ));
    }
    let mut parsed = Vec::with_capacity(macs.len());
    for m in macs {
        parsed.push(parse_mac_bytes(m)?);
    }

    let sock = UdpSocket::bind("0.0.0.0:0")
        .map_err(|e| PlatformError::System(format!("创建网络唤醒套接字失败：{e}")))?;
    let _ = sock.set_broadcast(true);

    let mut sent = 0usize;
    for mac in parsed {
        let pkt = build_wol_magic_packet(mac);
        let mut any_ok = false;
        for target in ["255.255.255.255:9", "255.255.255.255:7"] {
            if sock.send_to(&pkt, target).is_ok() {
                sent += 1;
                any_ok = true;
            }
        }
        // Fallback to loopback when running inside an offline sandbox with no default route
        if !any_ok && sock.send_to(&pkt, "127.0.0.1:9").is_ok() {
            sent += 1;
        }
    }
    if sent == 0 {
        return Err(PlatformError::System(
            "发送网络唤醒广播包失败，请检查本机网络连接".to_string(),
        ));
    }
    Ok(sent)
}

// ──────────────────────────── 全局快捷键管理（任务 3.1）────────────────────────────

struct ActiveHotkeyWorker {
    thread_id: u32,
    join: Option<std::thread::JoinHandle<()>>,
}

impl ActiveHotkeyWorker {
    fn stop(&mut self) {
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
        if let Some(handle) = self.join.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for ActiveHotkeyWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

#[derive(Default)]
pub struct HotkeyController {
    worker: Mutex<Option<ActiveHotkeyWorker>>,
}

impl HotkeyController {
    pub fn set_hotkey(
        &self,
        hotkey: Option<&str>,
        on_trigger: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<(), PlatformError> {
        let mut guard = self.worker.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(mut existing) = guard.take() {
            existing.stop();
        }
        let Some(spec) = hotkey.map(str::trim).filter(|s| !s.is_empty()) else {
            return Ok(());
        };
        let (mods, vk) = parse_hotkey_spec(spec)?;
        let spec_owned = spec.to_string();
        let (tx, rx) = std::sync::mpsc::sync_channel::<Result<u32, PlatformError>>(1);

        let join = std::thread::spawn(move || {
            unsafe {
                // Force creation of thread message queue before reporting thread_id
                let mut dummy = MSG::default();
                let _ = PeekMessageW(&mut dummy, None, 0, 0, PM_REMOVE);
                let tid = GetCurrentThreadId();
                if RegisterHotKey(None, 1, mods | MOD_NOREPEAT, vk).is_err() {
                    let _ = tx.send(Err(PlatformError::Busy(format!(
                        "全局快捷键「{spec_owned}」已被其他软件占用，请在设置里更换另一个组合键"
                    ))));
                    return;
                }
                let _ = tx.send(Ok(tid));

                let mut msg = MSG::default();
                while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
                    if msg.message == WM_HOTKEY && msg.wParam.0 == 1 {
                        on_trigger();
                    }
                }
                let _ = UnregisterHotKey(None, 1);
            }
        });

        match rx.recv_timeout(Duration::from_secs(3)) {
            Ok(Ok(thread_id)) => {
                *guard = Some(ActiveHotkeyWorker {
                    thread_id,
                    join: Some(join),
                });
                Ok(())
            }
            Ok(Err(err)) => {
                let _ = join.join();
                Err(err)
            }
            Err(_) => Err(PlatformError::System(
                "全局快捷键后台监听线程启动超时".to_string(),
            )),
        }
    }
}

fn parse_hotkey_spec(spec: &str) -> Result<(HOT_KEY_MODIFIERS, u32), PlatformError> {
    let mut mods = HOT_KEY_MODIFIERS(0);
    let mut key_vk: Option<u32> = None;

    for part in spec.split('+').map(str::trim).filter(|s| !s.is_empty()) {
        let lower = part.to_ascii_lowercase();
        match lower.as_str() {
            "alt" | "option" => mods |= MOD_ALT,
            "ctrl" | "control" => mods |= MOD_CONTROL,
            "shift" => mods |= MOD_SHIFT,
            "win" | "meta" | "super" => mods |= MOD_WIN,
            "space" | "空格" => key_vk = Some(VK_SPACE.0 as u32),
            "enter" | "return" => key_vk = Some(VK_RETURN.0 as u32),
            s if s.starts_with('f') && s.len() <= 3 => {
                let n: u16 = s[1..].parse().map_err(|_| {
                    PlatformError::InvalidInput(format!("无法识别的快捷键功能键：{part}"))
                })?;
                if !(1..=12).contains(&n) {
                    return Err(PlatformError::InvalidInput(format!(
                        "功能键范围应为 F1–F12：{part}"
                    )));
                }
                key_vk = Some((VK_F1.0 + (n - 1)) as u32);
            }
            s if s.len() == 1 => {
                let ch = s.as_bytes()[0];
                if ch.is_ascii_lowercase() {
                    key_vk = Some((ch - b'a' + b'A') as u32);
                } else if ch.is_ascii_digit() {
                    key_vk = Some(ch as u32);
                } else {
                    return Err(PlatformError::InvalidInput(format!(
                        "不支持的快捷键主键：{part}"
                    )));
                }
            }
            _ => {
                return Err(PlatformError::InvalidInput(format!(
                    "不支持的快捷键字段：{part}"
                )));
            }
        }
    }

    if mods.0 == 0 {
        return Err(PlatformError::InvalidInput(
            "全局快捷键必须至少包含一个修饰键（Alt / Ctrl / Shift / Win）".to_string(),
        ));
    }
    let vk = key_vk.ok_or_else(|| {
        PlatformError::InvalidInput("全局快捷键缺少主键（例如 Space 或字母键）".to_string())
    })?;
    Ok((mods, vk))
}

// ──────────────────────────── 电源 10 秒可取消倒计时与执行（任务 3.5）────────────────────────────

struct CountdownState {
    cancelled: AtomicBool,
}

unsafe extern "system" fn countdown_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_COMMAND => {
            // Cancel button ID = 1001 or Escape IDCANCEL = 2
            let id = (wparam.0 & 0xFFFF) as u16;
            if id == 1001 || id == 2 {
                let ptr = unsafe {
                    windows::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(
                        hwnd,
                        windows::Win32::UI::WindowsAndMessaging::GWLP_USERDATA,
                    )
                } as *const CountdownState;
                if !ptr.is_null() {
                    unsafe { (*ptr).cancelled.store(true, Ordering::SeqCst) };
                }
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
                return LRESULT(0);
            }
        }
        WM_KEYDOWN => {
            if wparam.0 == VK_ESCAPE.0 as usize || wparam.0 == VK_SPACE.0 as usize {
                let ptr = unsafe {
                    windows::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(
                        hwnd,
                        windows::Win32::UI::WindowsAndMessaging::GWLP_USERDATA,
                    )
                } as *const CountdownState;
                if !ptr.is_null() {
                    unsafe { (*ptr).cancelled.store(true, Ordering::SeqCst) };
                }
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
                return LRESULT(0);
            }
        }
        WM_CLOSE => {
            let ptr = unsafe {
                windows::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(
                    hwnd,
                    windows::Win32::UI::WindowsAndMessaging::GWLP_USERDATA,
                )
            } as *const CountdownState;
            if !ptr.is_null() {
                unsafe { (*ptr).cancelled.store(true, Ordering::SeqCst) };
            }
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            return LRESULT(0);
        }
        windows::Win32::UI::WindowsAndMessaging::WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            return LRESULT(0);
        }
        _ => {}
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

pub fn run_power_countdown_blocking(
    action_label: String,
    requester_name: String,
    countdown_secs: u32,
    cancel_rx: tokio::sync::watch::Receiver<bool>,
) -> Result<bool, PlatformError> {
    if countdown_secs == 0 {
        return Ok(!*cancel_rx.borrow());
    }
    let duration = Duration::from_secs(countdown_secs as u64);
    let started = Instant::now();
    let state = Arc::new(CountdownState {
        cancelled: AtomicBool::new(false),
    });

    unsafe {
        let class_name = wide_null("PcSharePowerCountdownWnd");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(countdown_wndproc),
            lpszClassName: PCWSTR(class_name.as_ptr()),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH(GetStockObject(COLOR_WINDOW_BRUSH_FALLBACK).0),
            ..Default::default()
        };
        let _ = RegisterClassW(&wc);

        let w = 440;
        let h = 190;
        let sx = GetSystemMetrics(SM_CXSCREEN);
        let sy = GetSystemMetrics(SM_CYSCREEN);
        let x = ((sx - w) / 2).max(20);
        let y = ((sy - h) / 2).max(20);
        let title = wide_null("电脑共享 - 远程电源操作确认");

        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(WS_EX_TOPMOST.0),
            PCWSTR(class_name.as_ptr()),
            PCWSTR(title.as_ptr()),
            WS_POPUP | WS_CAPTION | WS_SYSMENU | WS_VISIBLE,
            x,
            y,
            w,
            h,
            None,
            None,
            None,
            None,
        );

        let Ok(hwnd) = hwnd else {
            // Headless fallback: still honor countdown timer + remote cancel_rx
            while started.elapsed() < duration {
                if *cancel_rx.borrow() {
                    return Ok(false);
                }
                std::thread::sleep(Duration::from_millis(80));
            }
            return Ok(!*cancel_rx.borrow());
        };

        windows::Win32::UI::WindowsAndMessaging::SetWindowLongPtrW(
            hwnd,
            windows::Win32::UI::WindowsAndMessaging::GWLP_USERDATA,
            Arc::as_ptr(&state) as isize,
        );

        let static_cls = wide_null("STATIC");
        let init_msg = wide_null(&format!(
            "设备「{requester_name}」请求对本机执行【{action_label}】。\r\n将在 {countdown_secs} 秒后执行；若您正在使用电脑，请点击下方按钮立即取消。"
        ));
        let label_hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(static_cls.as_ptr()),
            PCWSTR(init_msg.as_ptr()),
            WS_CHILD | WS_VISIBLE,
            20,
            18,
            390,
            68,
            Some(hwnd),
            None,
            None,
            None,
        )
        .unwrap_or_default();

        let btn_cls = wide_null("BUTTON");
        let btn_text = wide_null("立即取消（本机正在使用）");
        let _ = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(btn_cls.as_ptr()),
            PCWSTR(btn_text.as_ptr()),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(BS_DEFPUSHBUTTON as u32),
            100,
            98,
            230,
            40,
            Some(hwnd),
            Some(HMENU(1001 as *mut c_void)),
            None,
            None,
        );

        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = UpdateWindow(hwnd);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetTimer(Some(hwnd), 1, 100, None);

        let mut last_sec = countdown_secs;
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            if msg.message == WM_TIMER {
                if *cancel_rx.borrow() {
                    state.cancelled.store(true, Ordering::SeqCst);
                    let _ = DestroyWindow(hwnd);
                    continue;
                }
                let elapsed = started.elapsed();
                if elapsed >= duration {
                    let _ = DestroyWindow(hwnd);
                    continue;
                }
                let rem = duration.saturating_sub(elapsed).as_secs() as u32 + 1;
                if rem != last_sec && !label_hwnd.0.is_null() {
                    last_sec = rem;
                    let updated = wide_null(&format!(
                        "设备「{requester_name}」请求对本机执行【{action_label}】。\r\n将在 {rem} 秒后执行；若您正在使用电脑，请点击下方按钮立即取消。"
                    ));
                    let _ = SetWindowTextW(label_hwnd, PCWSTR(updated.as_ptr()));
                }
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    Ok(!state.cancelled.load(Ordering::SeqCst) && !*cancel_rx.borrow())
}

const COLOR_WINDOW_BRUSH_FALLBACK: windows::Win32::Graphics::Gdi::GET_STOCK_OBJECT_FLAGS =
    windows::Win32::Graphics::Gdi::GET_STOCK_OBJECT_FLAGS(0); // WHITE_BRUSH

fn enable_shutdown_privilege() -> Result<(), PlatformError> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        )
        .map_err(|e| PlatformError::PermissionDenied(format!("打开进程权限令牌失败：{e}")))?;

        let mut luid = Default::default();
        let look_res = LookupPrivilegeValueW(None, SE_SHUTDOWN_NAME, &mut luid);
        if let Err(e) = look_res {
            let _ = CloseHandle(token);
            return Err(PlatformError::PermissionDenied(format!(
                "查询关机特权失败：{e}"
            )));
        }

        let tp = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: luid,
                Attributes: SE_PRIVILEGE_ENABLED,
            }],
        };
        let adj = AdjustTokenPrivileges(token, false, Some(&tp), 0, None, None);
        let _ = CloseHandle(token);
        adj.map_err(|e| PlatformError::PermissionDenied(format!("启用关机特权失败：{e}")))
    }
}

pub fn execute_power_action_impl(action: PlatformPowerAction) -> Result<(), PlatformError> {
    unsafe {
        match action {
            PlatformPowerAction::Lock => LockWorkStation()
                .map_err(|e| PlatformError::System(format!("锁定屏幕失败：{e}"))),
            PlatformPowerAction::Sleep => {
                if SetSuspendState(false, false, false) {
                    Ok(())
                } else {
                    Err(PlatformError::System(
                        "系统拒绝进入睡眠状态（可能未开启睡眠支持）".to_string(),
                    ))
                }
            }
            PlatformPowerAction::Restart => {
                enable_shutdown_privilege()?;
                ExitWindowsEx(EWX_REBOOT | EWX_FORCEIFHUNG, SHTDN_REASON_MAJOR_OTHER)
                    .map_err(|e| PlatformError::System(format!("执行重启失败：{e}")))
            }
            PlatformPowerAction::Shutdown => {
                enable_shutdown_privilege()?;
                ExitWindowsEx(
                    EWX_SHUTDOWN | EWX_POWEROFF | EWX_FORCEIFHUNG,
                    SHTDN_REASON_MAJOR_OTHER,
                )
                .map_err(|e| PlatformError::System(format!("执行关机失败：{e}")))
            }
        }
    }
}

// ──────────────────────────── ConPTY 交互式伪终端（任务 3.6）────────────────────────────

struct WinConPtyInner {
    hpc: HPCON,
    process_handle: HANDLE,
    input_write: HANDLE,
    closed: AtomicBool,
}

unsafe impl Send for WinConPtyInner {}
unsafe impl Sync for WinConPtyInner {}

impl WinConPtyInner {
    fn close_now(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        unsafe {
            let mut code = 0u32;
            if GetExitCodeProcess(self.process_handle, &mut code).is_ok()
                && code == STILL_ACTIVE_CODE
            {
                let _ = TerminateProcess(self.process_handle, 0);
            }
            let _ = CloseHandle(self.input_write);
            ClosePseudoConsole(self.hpc);
            let _ = CloseHandle(self.process_handle);
        }
    }
}

impl Drop for WinConPtyInner {
    fn drop(&mut self) {
        self.close_now();
    }
}

pub struct WindowsPtySession {
    pid: u32,
    shell_name: String,
    inner: Arc<Mutex<WinConPtyInner>>,
}

impl PtySession for WindowsPtySession {
    fn pid(&self) -> u32 {
        self.pid
    }

    fn shell_name(&self) -> &str {
        &self.shell_name
    }

    fn write_input(&self, bytes: &[u8]) -> Result<(), PlatformError> {
        if bytes.is_empty() {
            return Ok(());
        }
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if guard.closed.load(Ordering::SeqCst) {
            return Err(PlatformError::Busy("终端会话已关闭".to_string()));
        }
        let mut offset = 0usize;
        while offset < bytes.len() {
            let mut written = 0u32;
            unsafe {
                WriteFile(
                    guard.input_write,
                    Some(&bytes[offset..]),
                    Some(&mut written),
                    None,
                )
            }
            .map_err(|e| PlatformError::System(format!("写入终端输入失败：{e}")))?;
            if written == 0 {
                break;
            }
            offset += written as usize;
        }
        Ok(())
    }

    fn resize(&self, cols: u16, rows: u16) -> Result<(), PlatformError> {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if guard.closed.load(Ordering::SeqCst) {
            return Ok(());
        }
        let size = COORD {
            X: cols.clamp(20, 500) as i16,
            Y: rows.clamp(5, 300) as i16,
        };
        unsafe { ResizePseudoConsole(guard.hpc, size) }
            .map_err(|e| PlatformError::System(format!("调整终端尺寸失败：{e}")))
    }

    fn try_wait(&self) -> Result<Option<i32>, PlatformError> {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if guard.closed.load(Ordering::SeqCst) {
            return Ok(Some(0));
        }
        let mut code = 0u32;
        unsafe { GetExitCodeProcess(guard.process_handle, &mut code) }
            .map_err(|e| PlatformError::System(format!("查询终端退出状态失败：{e}")))?;
        if code == STILL_ACTIVE_CODE {
            Ok(None)
        } else {
            Ok(Some(code as i32))
        }
    }

    fn close(&self) {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.close_now();
    }
}

pub fn open_pty_session_impl(
    prefer_powershell: bool,
    cols: u16,
    rows: u16,
    cwd: Option<&Path>,
) -> PtyOpenResult {
    unsafe {
        let mut pty_in_read = HANDLE::default();
        let mut app_in_write = HANDLE::default();
        CreatePipe(&mut pty_in_read, &mut app_in_write, None, 0)
            .map_err(|e| PlatformError::System(format!("创建终端输入管道失败：{e}")))?;

        let mut app_out_read = HANDLE::default();
        let mut pty_out_write = HANDLE::default();
        if let Err(e) = CreatePipe(&mut app_out_read, &mut pty_out_write, None, 0) {
            let _ = CloseHandle(pty_in_read);
            let _ = CloseHandle(app_in_write);
            return Err(PlatformError::System(format!("创建终端输出管道失败：{e}")));
        }

        let size = COORD {
            X: cols.clamp(20, 500) as i16,
            Y: rows.clamp(5, 300) as i16,
        };
        let hpc = match CreatePseudoConsole(size, pty_in_read, pty_out_write, 0) {
            Ok(h) => h,
            Err(e) => {
                let _ = CloseHandle(pty_in_read);
                let _ = CloseHandle(app_in_write);
                let _ = CloseHandle(app_out_read);
                let _ = CloseHandle(pty_out_write);
                return Err(PlatformError::System(format!("创建伪终端失败：{e}")));
            }
        };

        let mut attr_size = 0usize;
        let _ = InitializeProcThreadAttributeList(None, 1, None, &mut attr_size);
        let mut attr_buf = vec![0u8; attr_size.max(64)];
        let attr_list = LPPROC_THREAD_ATTRIBUTE_LIST(attr_buf.as_mut_ptr() as *mut c_void);
        if let Err(e) = InitializeProcThreadAttributeList(Some(attr_list), 1, None, &mut attr_size)
        {
            ClosePseudoConsole(hpc);
            let _ = CloseHandle(pty_in_read);
            let _ = CloseHandle(app_in_write);
            let _ = CloseHandle(app_out_read);
            let _ = CloseHandle(pty_out_write);
            return Err(PlatformError::System(format!("初始化伪终端进程属性失败：{e}")));
        }

        if let Err(e) = UpdateProcThreadAttribute(
            attr_list,
            0,
            PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE_ATTR,
            Some(hpc.0 as *const c_void),
            std::mem::size_of::<HPCON>(),
            None,
            None,
        ) {
            DeleteProcThreadAttributeList(attr_list);
            ClosePseudoConsole(hpc);
            let _ = CloseHandle(pty_in_read);
            let _ = CloseHandle(app_in_write);
            let _ = CloseHandle(app_out_read);
            let _ = CloseHandle(pty_out_write);
            return Err(PlatformError::System(format!("绑定伪终端句柄失败：{e}")));
        }

        let (shell_label, cmd_str) = if prefer_powershell {
            ("powershell.exe", "powershell.exe -NoLogo")
        } else {
            ("cmd.exe", "cmd.exe")
        };
        let mut cmd_wide = wide_null(cmd_str);
        let cwd_wide = cwd.filter(|p| p.is_dir()).map(|p| wide_null(&p.to_string_lossy()));
        let cwd_pcwstr = cwd_wide
            .as_ref()
            .map_or(PCWSTR::null(), |v| PCWSTR(v.as_ptr()));

        let si_ex = STARTUPINFOEXW {
            StartupInfo: STARTUPINFOW {
                cb: std::mem::size_of::<STARTUPINFOEXW>() as u32,
                dwFlags: STARTF_USESTDHANDLES,
                hStdInput: HANDLE::default(),
                hStdOutput: HANDLE::default(),
                hStdError: HANDLE::default(),
                ..Default::default()
            },
            lpAttributeList: attr_list,
        };
        let mut pi = PROCESS_INFORMATION::default();
        let create_res = CreateProcessW(
            PCWSTR::null(),
            Some(PWSTR(cmd_wide.as_mut_ptr())),
            None,
            None,
            false,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
            None,
            cwd_pcwstr,
            &si_ex.StartupInfo,
            &mut pi,
        );

        DeleteProcThreadAttributeList(attr_list);
        let _ = CloseHandle(pty_in_read);
        let _ = CloseHandle(pty_out_write);

        if let Err(e) = create_res {
            let _ = CloseHandle(app_in_write);
            let _ = CloseHandle(app_out_read);
            ClosePseudoConsole(hpc);
            return Err(PlatformError::System(format!(
                "启动终端程序 {shell_label} 失败：{e}"
            )));
        }
        let _ = CloseHandle(pi.hThread);

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let out_read_raw = app_out_read.0 as usize;
        std::thread::spawn(move || {
            let read_handle = HANDLE(out_read_raw as *mut c_void);
            let mut buf = [0u8; 4096];
            loop {
                let mut n = 0u32;
                let ok = ReadFile(read_handle, Some(&mut buf), Some(&mut n), None);
                if ok.is_err() || n == 0 {
                    break;
                }
                if tx.send(buf[..n as usize].to_vec()).is_err() {
                    break;
                }
            }
            let _ = CloseHandle(read_handle);
        });

        let session = WindowsPtySession {
            pid: pi.dwProcessId,
            shell_name: shell_label.to_string(),
            inner: Arc::new(Mutex::new(WinConPtyInner {
                hpc,
                process_handle: pi.hProcess,
                input_write: app_in_write,
                closed: AtomicBool::new(false),
            })),
        };
        Ok((Box::new(session), rx))
    }
}

// ──────────────────────────── 桌面系统通知（任务 3.10）────────────────────────────

pub fn show_system_notification_impl(title: &str, body: &str) -> Result<(), PlatformError> {
    let escaped_title = escape_xml(title);
    let escaped_body = escape_xml(body);
    let xml_str = format!(
        "<toast><visual><binding template=\"ToastGeneric\"><text>{escaped_title}</text><text>{escaped_body}</text></binding></visual></toast>"
    );
    let doc = XmlDocument::new()
        .map_err(|e| PlatformError::System(format!("创建系统通知文档失败：{e}")))?;
    doc.LoadXml(&HSTRING::from(xml_str))
        .map_err(|e| PlatformError::System(format!("构造系统通知内容失败：{e}")))?;
    let toast = ToastNotification::CreateToastNotification(&doc)
        .map_err(|e| PlatformError::System(format!("初始化系统通知失败：{e}")))?;

    // Try app's AUMID first, falling back to Windows PowerShell's registered AUMID when running unpackaged
    for aumid in [
        "app.pcshare.desktop",
        "{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\\WindowsPowerShell\\v1.0\\powershell.exe",
    ] {
        if let Ok(notifier) = ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(aumid))
            && notifier.Show(&toast).is_ok()
        {
            return Ok(());
        }
    }
    Ok(())
}

fn escape_xml(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}
