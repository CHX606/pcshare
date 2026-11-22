use std::cell::Cell;
use std::collections::BTreeSet;
use std::ffi::c_void;
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::Data::Xml::Dom::XmlDocument;
use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};
use windows::Win32::Foundation::{
    ERROR_BUFFER_OVERFLOW, ERROR_NOT_ALL_ASSIGNED, GetLastError, HANDLE, HWND, LPARAM, LRESULT,
    WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    CreateFontIndirectW, DEFAULT_GUI_FONT, DeleteObject, GET_STOCK_OBJECT_FLAGS, GetStockObject,
    HBRUSH, HFONT, HGDIOBJ, UpdateWindow,
};
use windows::Win32::NetworkManagement::IpHelper::{
    GAA_FLAG_INCLUDE_GATEWAYS, GAA_FLAG_INCLUDE_PREFIX, GET_ADAPTERS_ADDRESSES_FLAGS,
    GetAdaptersAddresses, IF_TYPE_ETHERNET_CSMACD, IF_TYPE_IEEE80211, IF_TYPE_SOFTWARE_LOOPBACK,
    IP_ADAPTER_ADDRESSES_LH,
};
use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
use windows::Win32::Networking::WinSock::{AF_INET, AF_UNSPEC, SOCKADDR_IN};
use windows::Win32::Security::{
    AdjustTokenPrivileges, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, SE_PRIVILEGE_ENABLED,
    SE_SHUTDOWN_NAME, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
};
use windows::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows::Win32::System::Console::{
    COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON, ResizePseudoConsole,
};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::Power::SetSuspendState;
use windows::Win32::System::Shutdown::{
    EWX_FORCEIFHUNG, EWX_POWEROFF, EWX_REBOOT, EWX_SHUTDOWN, ExitWindowsEx, LockWorkStation,
    SHTDN_REASON_MAJOR_OTHER,
};
use windows::Win32::System::Threading::{
    CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess, GetCurrentThreadId, GetExitCodeProcess,
    InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST, OpenProcessToken,
    PROCESS_INFORMATION, ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOEXW, STARTUPINFOW,
    TerminateProcess, UpdateProcThreadAttribute,
};
use windows::Win32::UI::HiDpi::{GetDpiForSystem, GetDpiForWindow, SystemParametersInfoForDpi};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, RegisterHotKey,
    SetFocus, UnregisterHotKey, VK_ESCAPE, VK_F1, VK_RETURN, VK_SPACE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BS_DEFPUSHBUTTON, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GWLP_USERDATA, GetMessageW, GetSystemMetrics, GetWindowLongPtrW, HMENU, IDC_ARROW,
    IsDialogMessageW, KillTimer, LoadCursorW, MSG, NONCLIENTMETRICSW, PM_REMOVE, PeekMessageW,
    PostQuitMessage, PostThreadMessageW, RegisterClassW, SM_CXSCREEN, SM_CYSCREEN,
    SPI_GETNONCLIENTMETRICS, SW_SHOW, SendMessageW, SetForegroundWindow, SetTimer,
    SetWindowLongPtrW, SetWindowTextW, ShowWindow, TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE,
    WM_CLOSE, WM_COMMAND, WM_DESTROY, WM_HOTKEY, WM_KEYDOWN, WM_QUIT, WM_SETFONT, WM_TIMER,
    WNDCLASSW, WS_CAPTION, WS_CHILD, WS_EX_TOPMOST, WS_POPUP, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE,
};
use windows::core::{HSTRING, PCWSTR, PWSTR};

use crate::win_util::{OwnedHandle, wide_null};
use crate::{PlatformError, PlatformPowerAction, PtyOpenResult, PtySession};

const PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE_ATTR: usize = 0x0002_0016;
const IF_TYPE_TUNNEL_VAL: u32 = 131;
const STILL_ACTIVE_CODE: u32 = 259;
const PTY_OUTPUT_CHANNEL_CAPACITY: usize = 256;

// ──────────────────────── 网卡物理地址发现与网络唤醒（任务 3.5）────────────────────────

/// 以 8 字节对齐的 `Vec<u64>` 安全调用 `GetAdaptersAddresses`，仅在 `ERROR_BUFFER_OVERFLOW` 时扩容重试。
fn query_adapters_addresses(
    family: u32,
    flags: GET_ADAPTERS_ADDRESSES_FLAGS,
) -> Option<Vec<u64>> {
    let mut size = 32 * 1024u32;
    let mut buf: Vec<u64> = Vec::new();
    for _ in 0..4 {
        let words = (size as usize).div_ceil(std::mem::size_of::<u64>()).max(1);
        if buf.len() < words {
            buf.resize(words, 0);
        }
        let mut actual_size = (buf.len() * std::mem::size_of::<u64>()) as u32;
        let rc = unsafe {
            GetAdaptersAddresses(
                family,
                flags,
                None,
                Some(buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH),
                &mut actual_size,
            )
        };
        if rc == 0 {
            return (actual_size > 0).then_some(buf);
        }
        if rc == ERROR_BUFFER_OVERFLOW.0 && actual_size > 0 {
            size = actual_size;
            continue;
        }
        return None;
    }
    None
}

pub fn local_mac_addresses_impl() -> Vec<String> {
    let Some(buf) = query_adapters_addresses(AF_UNSPEC.0 as u32, GET_ADAPTERS_ADDRESSES_FLAGS(0))
    else {
        return Vec::new();
    };

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

fn ipv4_subnet_broadcast(ip: Ipv4Addr, prefix_len: u8) -> Option<Ipv4Addr> {
    if ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() {
        return None;
    }
    if prefix_len == 0 {
        return Some(Ipv4Addr::BROADCAST);
    }
    if prefix_len >= 31 {
        return None;
    }
    let mask = u32::MAX << (32 - prefix_len);
    let bcast = u32::from(ip) | !mask;
    Some(Ipv4Addr::from(bcast))
}

fn collect_ipv4_broadcast_targets() -> Vec<(Ipv4Addr, Ipv4Addr)> {
    let Some(buf) = query_adapters_addresses(AF_INET.0 as u32, GAA_FLAG_INCLUDE_PREFIX) else {
        return Vec::new();
    };

    let mut targets = BTreeSet::new();
    let mut curr = buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
    unsafe {
        while !curr.is_null() {
            let adapter = &*curr;
            let valid_phys = adapter.PhysicalAddressLength == 6
                && !adapter.PhysicalAddress[..6].iter().all(|&x| x == 0);
            if adapter.OperStatus == IfOperStatusUp
                && adapter.IfType != IF_TYPE_SOFTWARE_LOOPBACK
                && adapter.IfType != IF_TYPE_TUNNEL_VAL
                && valid_phys
            {
                let mut ua_ptr = adapter.FirstUnicastAddress;
                while !ua_ptr.is_null() {
                    let ua = &*ua_ptr;
                    if !ua.Address.lpSockaddr.is_null()
                        && ua.Address.iSockaddrLength >= std::mem::size_of::<SOCKADDR_IN>() as i32
                    {
                        let sa: SOCKADDR_IN =
                            std::ptr::read_unaligned(ua.Address.lpSockaddr as *const SOCKADDR_IN);
                        if sa.sin_family == AF_INET {
                            let b = sa.sin_addr.S_un.S_un_b;
                            let ip = Ipv4Addr::new(b.s_b1, b.s_b2, b.s_b3, b.s_b4);
                            if let Some(bcast) = ipv4_subnet_broadcast(ip, ua.OnLinkPrefixLength) {
                                targets.insert((ip, bcast));
                            }
                        }
                    }
                    ua_ptr = ua.Next;
                }
            }
            curr = adapter.Next;
        }
    }
    targets.into_iter().collect()
}

unsafe fn borrowed_pwstr_to_string(ptr: PWSTR) -> String {
    if ptr.0.is_null() {
        return String::new();
    }
    unsafe {
        let mut len = 0usize;
        while len < 512 && *ptr.0.add(len) != 0 {
            len += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(ptr.0, len))
    }
}

/// 枚举本机活动物理网卡（有线以太网 / Wi-Fi），过滤掉虚拟/代理 TUN 网卡与 `198.18.0.0/15` Fake-IP 网段，
/// 选取具备真实默认网关且路由度量最优的物理 IPv4 地址，用于开启物理网卡直绑时绕开 TUN 劫持。
pub fn primary_physical_ipv4_impl() -> Option<Ipv4Addr> {
    let buf = query_adapters_addresses(
        AF_INET.0 as u32,
        GAA_FLAG_INCLUDE_PREFIX | GAA_FLAG_INCLUDE_GATEWAYS,
    )?;

    let mut best: Option<((u8, u32), Ipv4Addr)> = None;
    let mut curr = buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
    unsafe {
        while !curr.is_null() {
            let adapter = &*curr;
            let is_phys_type = adapter.IfType == IF_TYPE_ETHERNET_CSMACD
                || adapter.IfType == IF_TYPE_IEEE80211;
            let valid_mac = adapter.PhysicalAddressLength == 6
                && !adapter.PhysicalAddress[..6].iter().all(|&x| x == 0)
                && !adapter.PhysicalAddress[..6].iter().all(|&x| x == 0xFF);
            let has_gateway = !adapter.FirstGatewayAddress.is_null();

            if adapter.OperStatus == IfOperStatusUp && is_phys_type && valid_mac && has_gateway {
                let desc = borrowed_pwstr_to_string(adapter.Description);
                let friendly = borrowed_pwstr_to_string(adapter.FriendlyName);
                if !crate::is_virtual_or_tunnel_adapter_name(&desc, &friendly) {
                    let mut ua_ptr = adapter.FirstUnicastAddress;
                    while !ua_ptr.is_null() {
                        let ua = &*ua_ptr;
                        if !ua.Address.lpSockaddr.is_null()
                            && ua.Address.iSockaddrLength
                                >= std::mem::size_of::<SOCKADDR_IN>() as i32
                        {
                            let sa: SOCKADDR_IN = std::ptr::read_unaligned(
                                ua.Address.lpSockaddr as *const SOCKADDR_IN,
                            );
                            if sa.sin_family == AF_INET {
                                let b = sa.sin_addr.S_un.S_un_b;
                                let ip = Ipv4Addr::new(b.s_b1, b.s_b2, b.s_b3, b.s_b4);
                                let is_link_local = b.s_b1 == 169 && b.s_b2 == 254;
                                let is_fake_tun = b.s_b1 == 198 && (b.s_b2 == 18 || b.s_b2 == 19);
                                if !ip.is_loopback()
                                    && !ip.is_unspecified()
                                    && !ip.is_multicast()
                                    && !is_link_local
                                    && !is_fake_tun
                                {
                                    // 同等带网关条件下按 Windows 自身 Ipv4Metric 升序择优，同度量优先有线网卡
                                    let kind_rank = if adapter.IfType == IF_TYPE_ETHERNET_CSMACD {
                                        0u8
                                    } else {
                                        1u8
                                    };
                                    let score = (kind_rank, adapter.Ipv4Metric);
                                    let replace = match best {
                                        None => true,
                                        Some(((best_kind, best_metric), _)) => {
                                            (adapter.Ipv4Metric, kind_rank)
                                                < (best_metric, best_kind)
                                        }
                                    };
                                    if replace {
                                        best = Some((score, ip));
                                    }
                                }
                            }
                        }
                        ua_ptr = ua.Next;
                    }
                }
            }
            curr = adapter.Next;
        }
    }
    best.map(|(_, ip)| ip)
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
        out[i] = u8::from_str_radix(&cleaned[i * 2..i * 2 + 2], 16)
            .map_err(|_| PlatformError::InvalidInput(format!("无法解析物理网卡地址：{mac}")))?;
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

    // 逐物理 IPv4 接口绑定并发送子网定向广播，避免多网卡或代理 TUN 下默认路由吃掉全局广播
    let iface_targets = collect_ipv4_broadcast_targets();
    let mut iface_sockets: Vec<(UdpSocket, Ipv4Addr)> = Vec::with_capacity(iface_targets.len());
    for (local_ip, bcast_ip) in iface_targets {
        if let Ok(sock) = UdpSocket::bind(SocketAddrV4::new(local_ip, 0)) {
            let _ = sock.set_broadcast(true);
            iface_sockets.push((sock, bcast_ip));
        }
    }

    let global_sock = UdpSocket::bind("0.0.0.0:0").ok();
    if let Some(ref sock) = global_sock {
        let _ = sock.set_broadcast(true);
    }

    let mut sent = 0usize;
    for mac in parsed {
        let pkt = build_wol_magic_packet(mac);
        for (sock, bcast_ip) in &iface_sockets {
            for port in [9u16, 7u16] {
                if sock.send_to(&pkt, SocketAddrV4::new(*bcast_ip, port)).is_ok() {
                    sent += 1;
                }
            }
        }
        if let Some(ref sock) = global_sock {
            for target in ["255.255.255.255:9", "255.255.255.255:7"] {
                if sock.send_to(&pkt, target).is_ok() {
                    sent += 1;
                }
            }
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

type HotkeyCallbackSlot = Arc<Mutex<Arc<dyn Fn() + Send + Sync>>>;

struct ActiveHotkeyWorker {
    mods: HOT_KEY_MODIFIERS,
    vk: u32,
    callback: HotkeyCallbackSlot,
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
        let Some(spec) = hotkey.map(str::trim).filter(|s| !s.is_empty()) else {
            let mut guard = self.worker.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(mut existing) = guard.take() {
                existing.stop();
            }
            return Ok(());
        };

        // 先前置校验语法，语法错误时不破坏已生效的旧热键
        let (mods, vk) = parse_hotkey_spec(spec)?;

        let mut guard = self.worker.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = guard.as_ref()
            && existing.mods == mods
            && existing.vk == vk
        {
            // 同一热键组合直接原地切换回调，无需先注销再重注
            *existing.callback.lock().unwrap_or_else(|e| e.into_inner()) = on_trigger;
            return Ok(());
        }

        // 不同热键组合：先注册新热键，确认成功后再停掉旧热键，避免失败时丢失原热键
        let spec_owned = spec.to_string();
        let callback_slot: HotkeyCallbackSlot = Arc::new(Mutex::new(on_trigger));
        let cb_for_thread = callback_slot.clone();
        let (tx, rx) = std::sync::mpsc::sync_channel::<Result<u32, PlatformError>>(1);

        let join = std::thread::spawn(move || unsafe {
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
                    let cb = cb_for_thread
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone();
                    cb();
                }
            }
            let _ = UnregisterHotKey(None, 1);
        });

        match rx.recv_timeout(Duration::from_secs(3)) {
            Ok(Ok(thread_id)) => {
                if let Some(mut old) = guard.take() {
                    old.stop();
                }
                *guard = Some(ActiveHotkeyWorker {
                    mods,
                    vk,
                    callback: callback_slot,
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

    let set_primary = |slot: &mut Option<u32>, vk: u32| -> Result<(), PlatformError> {
        if slot.is_some() {
            return Err(PlatformError::InvalidInput(format!(
                "全局快捷键只能包含一个主键：{spec}"
            )));
        }
        *slot = Some(vk);
        Ok(())
    };

    for part in spec.split('+').map(str::trim).filter(|s| !s.is_empty()) {
        let lower = part.to_ascii_lowercase();
        match lower.as_str() {
            "alt" | "option" => mods |= MOD_ALT,
            "ctrl" | "control" => mods |= MOD_CONTROL,
            "shift" => mods |= MOD_SHIFT,
            "win" | "meta" | "super" => mods |= MOD_WIN,
            "space" | "空格" => set_primary(&mut key_vk, VK_SPACE.0 as u32)?,
            "enter" | "return" => set_primary(&mut key_vk, VK_RETURN.0 as u32)?,
            s if s.starts_with('f') && s.len() <= 3 => {
                let n: u16 = s[1..].parse().map_err(|_| {
                    PlatformError::InvalidInput(format!("无法识别的快捷键功能键：{part}"))
                })?;
                if !(1..=12).contains(&n) {
                    return Err(PlatformError::InvalidInput(format!(
                        "功能键范围应为 F1–F12：{part}"
                    )));
                }
                set_primary(&mut key_vk, (VK_F1.0 + (n - 1)) as u32)?;
            }
            s if s.len() == 1 => {
                let ch = s.as_bytes()[0];
                if ch.is_ascii_lowercase() {
                    set_primary(&mut key_vk, (ch - b'a' + b'A') as u32)?;
                } else if ch.is_ascii_digit() {
                    set_primary(&mut key_vk, ch as u32)?;
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
    label_hwnd: HWND,
    action_label: String,
    requester_name: String,
    duration: Duration,
    started: Instant,
    last_sec: Cell<u32>,
    cancel_rx: tokio::sync::watch::Receiver<bool>,
}

struct FontGuard {
    font: HFONT,
    owned: bool,
}

impl Drop for FontGuard {
    fn drop(&mut self) {
        if self.owned && !self.font.0.is_null() {
            unsafe {
                let _ = DeleteObject(HGDIOBJ(self.font.0));
            }
        }
    }
}

fn scale_px(px: i32, dpi: u32) -> i32 {
    ((i64::from(px) * i64::from(dpi.max(96)) + 48) / 96) as i32
}

unsafe fn create_ui_font_for_dpi(dpi: u32) -> FontGuard {
    let mut ncm = NONCLIENTMETRICSW {
        cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32,
        ..Default::default()
    };
    if unsafe {
        SystemParametersInfoForDpi(
            SPI_GETNONCLIENTMETRICS.0,
            ncm.cbSize,
            Some(&mut ncm as *mut NONCLIENTMETRICSW as *mut c_void),
            0,
            dpi.max(96),
        )
    }
    .is_ok()
    {
        let font = unsafe { CreateFontIndirectW(&ncm.lfMessageFont) };
        if !font.0.is_null() {
            return FontGuard { font, owned: true };
        }
    }
    FontGuard {
        font: HFONT(unsafe { GetStockObject(DEFAULT_GUI_FONT) }.0),
        owned: false,
    }
}

fn format_countdown_prompt(requester_name: &str, action_label: &str, rem_secs: u32) -> String {
    format!(
        "设备「{requester_name}」请求对本机执行【{action_label}】。\r\n将在 {rem_secs} 秒后执行；若您正在使用电脑，请点击下方按钮或按 Esc/回车立即取消。"
    )
}

unsafe fn countdown_state_ref<'a>(hwnd: HWND) -> Option<&'a CountdownState> {
    let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const CountdownState;
    if ptr.is_null() {
        None
    } else {
        Some(unsafe { &*ptr })
    }
}

unsafe extern "system" fn countdown_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_TIMER if wparam.0 == 1 => {
            if let Some(state) = unsafe { countdown_state_ref(hwnd) } {
                if *state.cancel_rx.borrow() {
                    state.cancelled.store(true, Ordering::SeqCst);
                    unsafe {
                        let _ = DestroyWindow(hwnd);
                    }
                    return LRESULT(0);
                }
                let elapsed = state.started.elapsed();
                if elapsed >= state.duration {
                    unsafe {
                        let _ = DestroyWindow(hwnd);
                    }
                    return LRESULT(0);
                }
                let rem_ms = state.duration.saturating_sub(elapsed).as_millis() as u64;
                let rem = (rem_ms.div_ceil(1000) as u32).max(1);
                if rem != state.last_sec.get() && !state.label_hwnd.0.is_null() {
                    state.last_sec.set(rem);
                    let updated = wide_null(&format_countdown_prompt(
                        &state.requester_name,
                        &state.action_label,
                        rem,
                    ));
                    unsafe {
                        let _ = SetWindowTextW(state.label_hwnd, PCWSTR(updated.as_ptr()));
                    }
                }
            }
            return LRESULT(0);
        }
        WM_COMMAND => {
            // Cancel button ID = 1001 or Escape IDCANCEL = 2
            let id = (wparam.0 & 0xFFFF) as u16;
            if id == 1001 || id == 2 {
                if let Some(state) = unsafe { countdown_state_ref(hwnd) } {
                    state.cancelled.store(true, Ordering::SeqCst);
                }
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
                return LRESULT(0);
            }
        }
        WM_KEYDOWN => {
            if wparam.0 == VK_ESCAPE.0 as usize
                || wparam.0 == VK_SPACE.0 as usize
                || wparam.0 == VK_RETURN.0 as usize
            {
                if let Some(state) = unsafe { countdown_state_ref(hwnd) } {
                    state.cancelled.store(true, Ordering::SeqCst);
                }
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
                return LRESULT(0);
            }
        }
        WM_CLOSE => {
            if let Some(state) = unsafe { countdown_state_ref(hwnd) } {
                state.cancelled.store(true, Ordering::SeqCst);
            }
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            return LRESULT(0);
        }
        WM_DESTROY => {
            unsafe {
                let _ = KillTimer(Some(hwnd), 1);
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                PostQuitMessage(0);
            }
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

        let sys_dpi = GetDpiForSystem();
        let init_dpi = if sys_dpi > 0 { sys_dpi } else { 96 };
        let w = scale_px(440, init_dpi);
        let h = scale_px(190, init_dpi);
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

        let win_dpi = GetDpiForWindow(hwnd);
        let dpi = if win_dpi > 0 { win_dpi } else { init_dpi };
        let font_guard = create_ui_font_for_dpi(dpi);

        let static_cls = wide_null("STATIC");
        let init_msg = wide_null(&format_countdown_prompt(
            &requester_name,
            &action_label,
            countdown_secs,
        ));
        let label_hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(static_cls.as_ptr()),
            PCWSTR(init_msg.as_ptr()),
            WS_CHILD | WS_VISIBLE,
            scale_px(20, dpi),
            scale_px(18, dpi),
            scale_px(390, dpi),
            scale_px(68, dpi),
            Some(hwnd),
            None,
            None,
            None,
        )
        .unwrap_or_default();

        let btn_cls = wide_null("BUTTON");
        let btn_text = wide_null("立即取消（本机正在使用）");
        let btn_hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(btn_cls.as_ptr()),
            PCWSTR(btn_text.as_ptr()),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(BS_DEFPUSHBUTTON as u32),
            scale_px(100, dpi),
            scale_px(98, dpi),
            scale_px(230, dpi),
            scale_px(40, dpi),
            Some(hwnd),
            Some(HMENU(1001 as *mut c_void)),
            None,
            None,
        )
        .unwrap_or_default();

        if !font_guard.font.0.is_null() {
            if !label_hwnd.0.is_null() {
                let _ = SendMessageW(
                    label_hwnd,
                    WM_SETFONT,
                    Some(WPARAM(font_guard.font.0 as usize)),
                    Some(LPARAM(1)),
                );
            }
            if !btn_hwnd.0.is_null() {
                let _ = SendMessageW(
                    btn_hwnd,
                    WM_SETFONT,
                    Some(WPARAM(font_guard.font.0 as usize)),
                    Some(LPARAM(1)),
                );
            }
        }

        let state = CountdownState {
            cancelled: AtomicBool::new(false),
            label_hwnd,
            action_label,
            requester_name,
            duration,
            started,
            last_sec: Cell::new(countdown_secs),
            cancel_rx,
        };
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, &state as *const CountdownState as isize);

        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = UpdateWindow(hwnd);
        let _ = SetForegroundWindow(hwnd);
        if !btn_hwnd.0.is_null() {
            let _ = SetFocus(Some(btn_hwnd));
        }
        let _ = SetTimer(Some(hwnd), 1, 100, None);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            if !IsDialogMessageW(hwnd, &msg).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        drop(font_guard);

        Ok(!state.cancelled.load(Ordering::SeqCst) && !*state.cancel_rx.borrow())
    }
}

const COLOR_WINDOW_BRUSH_FALLBACK: GET_STOCK_OBJECT_FLAGS = GET_STOCK_OBJECT_FLAGS(0); // WHITE_BRUSH

fn enable_shutdown_privilege() -> Result<(), PlatformError> {
    unsafe {
        let mut token_raw = HANDLE::default();
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token_raw,
        )
        .map_err(|e| PlatformError::PermissionDenied(format!("打开进程权限令牌失败：{e}")))?;
        let token = OwnedHandle::new(token_raw)
            .ok_or_else(|| PlatformError::PermissionDenied("进程权限令牌句柄无效".to_string()))?;

        let mut luid = Default::default();
        LookupPrivilegeValueW(None, SE_SHUTDOWN_NAME, &mut luid)
            .map_err(|e| PlatformError::PermissionDenied(format!("查询关机特权失败：{e}")))?;

        let tp = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: luid,
                Attributes: SE_PRIVILEGE_ENABLED,
            }],
        };
        AdjustTokenPrivileges(token.raw(), false, Some(&tp), 0, None, None)
            .map_err(|e| PlatformError::PermissionDenied(format!("启用关机特权失败：{e}")))?;
        if GetLastError() == ERROR_NOT_ALL_ASSIGNED {
            return Err(PlatformError::PermissionDenied(
                "当前进程缺少关机或重启特权（ERROR_NOT_ALL_ASSIGNED）".to_string(),
            ));
        }
        Ok(())
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

struct ConPtyControl {
    hpc: Option<HPCON>,
    process_handle: Option<OwnedHandle>,
    job_handle: Option<OwnedHandle>,
}

unsafe impl Send for ConPtyControl {}

pub struct WindowsPtySession {
    pid: u32,
    shell_name: String,
    closed: AtomicBool,
    ctrl: Mutex<ConPtyControl>,
    /// 独立写锁：避免阻塞式 `WriteFile` 死锁 `close`/`resize`/`try_wait`，并确保关写句柄前无在途写入。
    write_lock: Mutex<Option<OwnedHandle>>,
}

impl WindowsPtySession {
    fn close_internal(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        // 第一阶段：只持控制锁，关闭 Job Object（回收整棵进程树）、兜底终止 Shell、关闭伪控制台以唤醒阻塞写
        {
            let mut ctrl = self.ctrl.lock().unwrap_or_else(|e| e.into_inner());
            drop(ctrl.job_handle.take());
            if let Some(ref proc_h) = ctrl.process_handle {
                let mut code = 0u32;
                if unsafe { GetExitCodeProcess(proc_h.raw(), &mut code) }.is_ok()
                    && code == STILL_ACTIVE_CODE
                {
                    unsafe {
                        let _ = TerminateProcess(proc_h.raw(), 0);
                    }
                }
            }
            if let Some(hpc) = ctrl.hpc.take() {
                unsafe {
                    ClosePseudoConsole(hpc);
                }
            }
            drop(ctrl.process_handle.take());
        }
        // 第二阶段：释放控制锁后再等待写锁并关闭写端句柄，杜绝死锁与句柄复用竞态
        {
            let mut writer = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
            drop(writer.take());
        }
    }
}

impl Drop for WindowsPtySession {
    fn drop(&mut self) {
        self.close_internal();
    }
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
        if self.closed.load(Ordering::SeqCst) {
            return Err(PlatformError::Busy("终端会话已关闭".to_string()));
        }
        let guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        if self.closed.load(Ordering::SeqCst) {
            return Err(PlatformError::Busy("终端会话已关闭".to_string()));
        }
        let Some(ref writer) = *guard else {
            return Err(PlatformError::Busy("终端会话已关闭".to_string()));
        };
        let mut offset = 0usize;
        while offset < bytes.len() {
            let mut written = 0u32;
            unsafe {
                WriteFile(
                    writer.raw(),
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
        if self.closed.load(Ordering::SeqCst) {
            return Ok(());
        }
        let guard = self.ctrl.lock().unwrap_or_else(|e| e.into_inner());
        let Some(hpc) = guard.hpc else {
            return Ok(());
        };
        let size = COORD {
            X: cols.clamp(20, 500) as i16,
            Y: rows.clamp(5, 300) as i16,
        };
        unsafe { ResizePseudoConsole(hpc, size) }
            .map_err(|e| PlatformError::System(format!("调整终端尺寸失败：{e}")))
    }

    fn try_wait(&self) -> Result<Option<i32>, PlatformError> {
        let guard = self.ctrl.lock().unwrap_or_else(|e| e.into_inner());
        if self.closed.load(Ordering::SeqCst) {
            return Ok(Some(0));
        }
        let Some(ref proc_h) = guard.process_handle else {
            return Ok(Some(0));
        };
        let mut code = 0u32;
        unsafe { GetExitCodeProcess(proc_h.raw(), &mut code) }
            .map_err(|e| PlatformError::System(format!("查询终端退出状态失败：{e}")))?;
        if code == STILL_ACTIVE_CODE {
            Ok(None)
        } else {
            Ok(Some(code as i32))
        }
    }

    fn close(&self) {
        self.close_internal();
    }
}

struct PseudoConsoleGuard(Option<HPCON>);
impl Drop for PseudoConsoleGuard {
    fn drop(&mut self) {
        if let Some(hpc) = self.0.take() {
            unsafe {
                ClosePseudoConsole(hpc);
            }
        }
    }
}

struct AttrListGuard(LPPROC_THREAD_ATTRIBUTE_LIST);
impl Drop for AttrListGuard {
    fn drop(&mut self) {
        if !self.0.0.is_null() {
            unsafe {
                DeleteProcThreadAttributeList(self.0);
            }
        }
    }
}

pub fn open_pty_session_impl(
    prefer_powershell: bool,
    cols: u16,
    rows: u16,
    cwd: Option<&Path>,
) -> PtyOpenResult {
    let (pid, shell_label, hpc, proc_handle, job_handle, app_in_write, app_out_read) = unsafe {
        let mut pty_in_read_raw = HANDLE::default();
        let mut app_in_write_raw = HANDLE::default();
        CreatePipe(&mut pty_in_read_raw, &mut app_in_write_raw, None, 0)
            .map_err(|e| PlatformError::System(format!("创建终端输入管道失败：{e}")))?;
        let pty_in_read = OwnedHandle::new(pty_in_read_raw)
            .ok_or_else(|| PlatformError::System("终端输入读端句柄无效".to_string()))?;
        let app_in_write = OwnedHandle::new(app_in_write_raw)
            .ok_or_else(|| PlatformError::System("终端输入写端句柄无效".to_string()))?;

        let mut app_out_read_raw = HANDLE::default();
        let mut pty_out_write_raw = HANDLE::default();
        CreatePipe(&mut app_out_read_raw, &mut pty_out_write_raw, None, 0)
            .map_err(|e| PlatformError::System(format!("创建终端输出管道失败：{e}")))?;
        let app_out_read = OwnedHandle::new(app_out_read_raw)
            .ok_or_else(|| PlatformError::System("终端输出读端句柄无效".to_string()))?;
        let pty_out_write = OwnedHandle::new(pty_out_write_raw)
            .ok_or_else(|| PlatformError::System("终端输出写端句柄无效".to_string()))?;

        let size = COORD {
            X: cols.clamp(20, 500) as i16,
            Y: rows.clamp(5, 300) as i16,
        };
        let mut hpc_guard = PseudoConsoleGuard(Some(
            CreatePseudoConsole(size, pty_in_read.raw(), pty_out_write.raw(), 0)
                .map_err(|e| PlatformError::System(format!("创建伪终端失败：{e}")))?,
        ));
        let hpc = hpc_guard.0.unwrap();

        let mut attr_size = 0usize;
        let _ = InitializeProcThreadAttributeList(None, 1, None, &mut attr_size);
        let attr_words = attr_size.max(64).div_ceil(std::mem::size_of::<usize>());
        let mut attr_buf = vec![0usize; attr_words];
        let attr_list = LPPROC_THREAD_ATTRIBUTE_LIST(attr_buf.as_mut_ptr() as *mut c_void);
        InitializeProcThreadAttributeList(Some(attr_list), 1, None, &mut attr_size)
            .map_err(|e| PlatformError::System(format!("初始化伪终端进程属性失败：{e}")))?;
        let attr_guard = AttrListGuard(attr_list);

        UpdateProcThreadAttribute(
            attr_list,
            0,
            PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE_ATTR,
            Some(hpc.0 as *const c_void),
            std::mem::size_of::<HPCON>(),
            None,
            None,
        )
        .map_err(|e| PlatformError::System(format!("绑定伪终端句柄失败：{e}")))?;

        // 创建带 KILL_ON_JOB_CLOSE 的作业对象，确保关闭会话时回收整棵子进程树
        let mut job_handle = CreateJobObjectW(None, PCWSTR::null())
            .ok()
            .and_then(OwnedHandle::new);
        if let Some(ref job) = job_handle {
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                job.raw(),
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
            .is_err()
            {
                job_handle = None;
            }
        }

        let (shell_label, cmd_str) = if prefer_powershell {
            ("powershell.exe", "powershell.exe -NoLogo")
        } else {
            ("cmd.exe", "cmd.exe")
        };
        let mut cmd_wide = wide_null(cmd_str);
        let cwd_wide = cwd
            .filter(|p| p.is_dir())
            .map(|p| wide_null(&p.to_string_lossy()));
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
            EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | CREATE_SUSPENDED,
            None,
            cwd_pcwstr,
            &si_ex.StartupInfo,
            &mut pi,
        );

        drop(attr_guard);
        drop(pty_in_read);
        drop(pty_out_write);

        create_res
            .map_err(|e| PlatformError::System(format!("启动终端程序 {shell_label} 失败：{e}")))?;

        let proc_handle = OwnedHandle::new(pi.hProcess)
            .ok_or_else(|| PlatformError::System("终端进程句柄无效".to_string()))?;
        let thread_handle = OwnedHandle::new(pi.hThread)
            .ok_or_else(|| PlatformError::System("终端主线程句柄无效".to_string()))?;

        // 在主线程恢复运行前将其加入 Job Object，消除子进程逃逸窗口
        if let Some(ref job) = job_handle
            && AssignProcessToJobObject(job.raw(), proc_handle.raw()).is_err()
        {
            job_handle = None;
        }
        if ResumeThread(thread_handle.raw()) == u32::MAX {
            let _ = TerminateProcess(proc_handle.raw(), 1);
            return Err(PlatformError::System(format!(
                "恢复终端程序 {shell_label} 运行失败"
            )));
        }
        drop(thread_handle);

        (
            pi.dwProcessId,
            shell_label,
            hpc_guard.0.take(),
            proc_handle,
            job_handle,
            app_in_write,
            app_out_read,
        )
    };

    let (tx, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(PTY_OUTPUT_CHANNEL_CAPACITY);
    std::thread::spawn(move || {
        let read_handle = app_out_read;
        let mut buf = [0u8; 4096];
        loop {
            let mut n = 0u32;
            let ok = unsafe { ReadFile(read_handle.raw(), Some(&mut buf), Some(&mut n), None) };
            if ok.is_err() || n == 0 {
                break;
            }
            if tx.blocking_send(buf[..n as usize].to_vec()).is_err() {
                break;
            }
        }
    });

    let session = WindowsPtySession {
        pid,
        shell_name: shell_label.to_string(),
        closed: AtomicBool::new(false),
        ctrl: Mutex::new(ConPtyControl {
            hpc,
            process_handle: Some(proc_handle),
            job_handle,
        }),
        write_lock: Mutex::new(Some(app_in_write)),
    };
    Ok((Box::new(session), rx))
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
        if let Ok(notifier) =
            ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(aumid))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipv4_subnet_broadcast_computes_directed_address_and_filters_special() {
        assert_eq!(
            ipv4_subnet_broadcast(Ipv4Addr::new(192, 168, 1, 42), 24),
            Some(Ipv4Addr::new(192, 168, 1, 255))
        );
        assert_eq!(
            ipv4_subnet_broadcast(Ipv4Addr::new(10, 20, 30, 5), 16),
            Some(Ipv4Addr::new(10, 20, 255, 255))
        );
        assert_eq!(
            ipv4_subnet_broadcast(Ipv4Addr::new(172, 16, 12, 9), 20),
            Some(Ipv4Addr::new(172, 16, 15, 255))
        );
        // 回环地址、全零地址、/31 /32 均不生成定向广播
        assert_eq!(ipv4_subnet_broadcast(Ipv4Addr::LOCALHOST, 8), None);
        assert_eq!(ipv4_subnet_broadcast(Ipv4Addr::UNSPECIFIED, 24), None);
        assert_eq!(
            ipv4_subnet_broadcast(Ipv4Addr::new(192, 168, 1, 1), 32),
            None
        );
        assert_eq!(
            ipv4_subnet_broadcast(Ipv4Addr::new(192, 168, 1, 1), 31),
            None
        );
    }

    #[test]
    fn parse_hotkey_spec_rejects_multiple_primary_keys_and_missing_mods() {
        assert!(parse_hotkey_spec("Ctrl+Alt+Space").is_ok());
        assert!(parse_hotkey_spec("Alt+F11").is_ok());
        assert!(matches!(
            parse_hotkey_spec("Ctrl+A+B"),
            Err(PlatformError::InvalidInput(_))
        ));
        assert!(matches!(
            parse_hotkey_spec("Ctrl+Space+Enter"),
            Err(PlatformError::InvalidInput(_))
        ));
        assert!(matches!(
            parse_hotkey_spec("Space"),
            Err(PlatformError::InvalidInput(_))
        ));
        assert!(matches!(
            parse_hotkey_spec("Ctrl+Shift"),
            Err(PlatformError::InvalidInput(_))
        ));
    }

    #[test]
    fn scale_px_scales_cleanly_at_high_dpi() {
        assert_eq!(scale_px(440, 96), 440);
        assert_eq!(scale_px(440, 144), 660);
        assert_eq!(scale_px(440, 192), 880);
        assert_eq!(scale_px(190, 0), 190);
    }
}
