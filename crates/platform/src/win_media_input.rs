use std::ffi::c_void;
use std::io::Cursor;
use std::path::PathBuf;

use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND, LPARAM, POINT, RECT};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows::Win32::Graphics::Gdi::{
    BI_BITFIELDS, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLACKNESS, BitBlt, CreateCompatibleBitmap,
    CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, DeleteObject, EnumDisplayMonitors, GetDC,
    GetDIBits, GetMonitorInfoW, HBITMAP, HDC, HGDIOBJ, HMONITOR, HPALETTE, MONITORINFOEXW, PatBlt,
    ReleaseDC, SRCCOPY, SelectObject,
};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_ContainerFormatJpeg, GUID_WICPixelFormat24bppBGR,
    GUID_WICPixelFormat32bppBGRA, IWICBitmap, IWICBitmapFrameEncode, IWICBitmapSource,
    IWICImagingFactory, WICBitmapDitherTypeNone, WICBitmapEncoderNoCache, WICBitmapIgnoreAlpha,
    WICBitmapInterpolationModeFant, WICBitmapPaletteTypeCustom,
};
use windows::Win32::Storage::Xps::{PRINT_WINDOW_FLAGS, PrintWindow};
use windows::Win32::System::Com::StructuredStorage::PROPBAG2;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, CoCreateInstance, IDataObject, STREAM_SEEK_SET,
};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber,
    IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows::Win32::System::Ole::{
    CF_DIB, CF_UNICODETEXT, DROPEFFECT_COPY, DROPEFFECT_MOVE, IDropSource,
};
use windows::Win32::System::Variant::{VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_R4};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForMonitor, MDT_EFFECTIVE_DPI,
    SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, MOUSEEVENTF_ABSOLUTE,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK,
    MOUSEINPUT, SendInput, VIRTUAL_KEY, VK_APPS, VK_BACK, VK_CONTROL, VK_DELETE, VK_DOWN, VK_END,
    VK_ESCAPE, VK_F1, VK_HOME, VK_INSERT, VK_LEFT, VK_LMENU, VK_LWIN, VK_NEXT, VK_PRIOR,
    VK_RCONTROL, VK_RETURN, VK_RIGHT, VK_RMENU, VK_RWIN, VK_SHIFT, VK_SNAPSHOT, VK_SPACE, VK_TAB,
    VK_UP,
};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    BHID_DataObject, ILCreateFromPathW, ILFree, SHCreateMemStream,
    SHCreateShellItemArrayFromIDLists, SHDoDragDrop,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CURSOR_SHOWING, CURSORINFO, CreateWindowExW, DI_NORMAL, DestroyWindow, DrawIconEx, EnumWindows,
    GetCursorInfo, GetForegroundWindow, GetIconInfo, GetWindowRect, GetWindowTextLengthW,
    GetWindowTextW, GetWindowThreadProcessId, HICON, HWND_MESSAGE, ICONINFO, IsIconic, IsWindow,
    IsWindowVisible, MONITORINFOF_PRIMARY, MSG, PM_REMOVE, PeekMessageW, WINDOW_EX_STYLE,
    WINDOW_STYLE, WindowFromPoint,
};
use windows::core::{BOOL, Interface, PCWSTR, PWSTR};

use crate::win_util::{
    ComGuard, OleGuard, check_window_uipi_warning, is_default_input_desktop, is_extended_vk,
    strip_verbatim_prefix, virtual_screen_to_absolute, wide_null, window_dpi_scale_percent,
};
use crate::{
    CapturableWindowInfo, CapturedImage, ClipboardContent, ClipboardData, DesktopEnvironment,
    InputOutcome, PlatformError, PlatformMonitor, PlatformMouseBtn,
};

const PW_RENDERFULLCONTENT_FLAG: u32 = 0x0000_0002;
const MAX_CLIPBOARD_IMAGE_DIM: u32 = 8192;
const MAX_CLIPBOARD_IMAGE_PIXELS: usize = 33_554_432; // 32M 像素（8K 7680×4320 以内，防解压炸弹与 u32 溢出）
const MAX_CLIPBOARD_PNG_DECODE_BYTES: usize = 128 * 1024 * 1024;

// ──────────────────────────── GDI RAII 守卫 ────────────────────────────

struct ScreenDcGuard(HDC);
impl Drop for ScreenDcGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = ReleaseDC(None, self.0);
            }
        }
    }
}

struct MemDcGuard(HDC);
impl Drop for MemDcGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = DeleteDC(self.0);
            }
        }
    }
}

struct BitmapGuard(HBITMAP);
impl Drop for BitmapGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = DeleteObject(HGDIOBJ(self.0.0));
            }
        }
    }
}

struct HwndGuard(HWND);
impl Drop for HwndGuard {
    fn drop(&mut self) {
        if !self.0.0.is_null() {
            unsafe {
                let _ = DestroyWindow(self.0);
            }
        }
    }
}

struct PidlList(Vec<*mut ITEMIDLIST>);
impl Drop for PidlList {
    fn drop(&mut self) {
        for p in self.0.drain(..) {
            unsafe {
                ILFree(Some(p as *const _));
            }
        }
    }
}

// ──────────────────────────── 显示器与窗口枚举（任务 3.2）────────────────────────────

struct RawMonitorEntry {
    monitor: PlatformMonitor,
    raw_name: String,
    order: usize,
}

pub fn list_monitors_impl() -> Result<Vec<PlatformMonitor>, PlatformError> {
    let mut raw_list: Vec<RawMonitorEntry> = Vec::new();
    unsafe extern "system" fn enum_monitor_cb(
        hmon: HMONITOR,
        _hdc: HDC,
        _rect: *mut RECT,
        lparam: LPARAM,
    ) -> BOOL {
        let out = unsafe { &mut *(lparam.0 as *mut Vec<RawMonitorEntry>) };
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if unsafe { GetMonitorInfoW(hmon, &mut info.monitorInfo) }.as_bool() {
            let rc = info.monitorInfo.rcMonitor;
            let width = (rc.right - rc.left).max(0) as u32;
            let height = (rc.bottom - rc.top).max(0) as u32;
            if width > 0 && height > 0 {
                let mut dpi_x = 96u32;
                let mut dpi_y = 96u32;
                let _ =
                    unsafe { GetDpiForMonitor(hmon, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) };
                let scale_pct = ((dpi_x * 100 + 48) / 96).max(100);
                let name_len = info
                    .szDevice
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(info.szDevice.len());
                let raw_name = String::from_utf16_lossy(&info.szDevice[..name_len]);
                let is_primary = (info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY) != 0;
                let order = out.len();
                out.push(RawMonitorEntry {
                    monitor: PlatformMonitor {
                        index: order as u32,
                        name: String::new(),
                        origin_x: rc.left,
                        origin_y: rc.top,
                        width,
                        height,
                        dpi_scale_percent: scale_pct,
                        is_primary,
                    },
                    raw_name,
                    order,
                });
            }
        }
        BOOL(1)
    }

    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(enum_monitor_cb),
            LPARAM(&mut raw_list as *mut Vec<RawMonitorEntry> as isize),
        );
    }
    // 先把主显示器排在第一位，再按最终序号统一生成 index 与展示名称
    raw_list.sort_by_key(|e| (!e.monitor.is_primary, e.order));
    let monitors = raw_list
        .into_iter()
        .enumerate()
        .map(|(idx, mut entry)| {
            entry.monitor.index = idx as u32;
            entry.monitor.name = format_monitor_display_name(
                idx,
                entry.monitor.is_primary,
                entry.monitor.width,
                entry.monitor.height,
                &entry.raw_name,
            );
            entry.monitor
        })
        .collect();
    Ok(monitors)
}

fn format_monitor_display_name(
    idx: usize,
    is_primary: bool,
    width: u32,
    height: u32,
    raw_name: &str,
) -> String {
    if is_primary {
        format!("主显示器 #{}（{}×{}）", idx + 1, width, height)
    } else {
        format!("显示器 #{}（{}×{} {}）", idx + 1, width, height, raw_name)
    }
}

fn is_window_cloaked(hwnd: HWND) -> bool {
    let mut cloaked: u32 = 0;
    let res = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut c_void,
            std::mem::size_of::<u32>() as u32,
        )
    };
    res.is_ok() && cloaked != 0
}

pub fn list_capturable_windows_impl() -> Vec<CapturableWindowInfo> {
    let mut list = Vec::new();
    unsafe extern "system" fn collect_cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let out = unsafe { &mut *(lparam.0 as *mut Vec<CapturableWindowInfo>) };
        unsafe {
            if !IsWindowVisible(hwnd).as_bool()
                || IsIconic(hwnd).as_bool()
                || is_window_cloaked(hwnd)
            {
                return BOOL(1);
            }
            let len = GetWindowTextLengthW(hwnd);
            if len <= 0 {
                return BOOL(1);
            }
            let mut rect = RECT::default();
            if GetWindowRect(hwnd, &mut rect).is_err() {
                return BOOL(1);
            }
            let w = rect.right - rect.left;
            let h = rect.bottom - rect.top;
            if w < 32 || h < 32 {
                return BOOL(1);
            }
            let mut buf = vec![0u16; len as usize + 1];
            let copied = GetWindowTextW(hwnd, &mut buf);
            if copied <= 0 {
                return BOOL(1);
            }
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            out.push(CapturableWindowInfo {
                hwnd: hwnd.0 as usize,
                pid,
                title: String::from_utf16_lossy(&buf[..copied as usize]),
                width: w as u32,
                height: h as u32,
            });
        }
        BOOL(1)
    }

    unsafe {
        let _ = EnumWindows(
            Some(collect_cb),
            LPARAM(&mut list as *mut Vec<CapturableWindowInfo> as isize),
        );
    }
    list
}

fn ensure_capturable_desktop(desktop: &DesktopEnvironment) -> Result<(), PlatformError> {
    if desktop.session_id == 0 {
        return Err(PlatformError::PermissionDenied(
            "对方电脑当前运行在系统服务环境（Session 0），没有可截取的图形桌面".to_string(),
        ));
    }
    if !desktop
        .input_desktop
        .as_deref()
        .is_some_and(|name| name.eq_ignore_ascii_case("Default"))
    {
        return Err(PlatformError::PermissionDenied(
            "对方电脑当前已锁屏或停留在系统登录/权限确认界面，无法截取画面".to_string(),
        ));
    }
    Ok(())
}

// ──────────────────────────── 桌面与窗口截图（任务 3.2）────────────────────────────

pub fn capture_screen_jpeg_impl(
    desktop: &DesktopEnvironment,
    monitor_index: Option<u32>,
    max_width: Option<u32>,
    quality: u8,
) -> Result<CapturedImage, PlatformError> {
    ensure_capturable_desktop(desktop)?;
    let monitors = list_monitors_impl()?;
    if monitors.is_empty() {
        return Err(PlatformError::NotFound(
            "对方电脑当前未检测到可用显示器".to_string(),
        ));
    }
    let idx = monitor_index.unwrap_or(0) as usize;
    let mon = monitors.get(idx).ok_or_else(|| {
        PlatformError::NotFound(format!(
            "找不到编号为 #{} 的显示器（共 {} 台）",
            idx + 1,
            monitors.len()
        ))
    })?;

    let (jpeg_bytes, img_w, img_h) = capture_dc_rect_to_jpeg(
        mon.origin_x,
        mon.origin_y,
        mon.width,
        mon.height,
        desktop,
        max_width,
        quality,
    )?;

    Ok(CapturedImage {
        jpeg_bytes,
        origin_x: mon.origin_x,
        origin_y: mon.origin_y,
        source_width: mon.width,
        source_height: mon.height,
        image_width: img_w,
        image_height: img_h,
        dpi_scale_percent: mon.dpi_scale_percent,
        target_hwnd: None,
        monitor_index: Some(mon.index),
    })
}

pub fn capture_window_jpeg_impl(
    desktop: &DesktopEnvironment,
    hwnd_raw: usize,
    max_width: Option<u32>,
    quality: u8,
) -> Result<CapturedImage, PlatformError> {
    ensure_capturable_desktop(desktop)?;
    let hwnd = HWND(hwnd_raw as *mut c_void);
    unsafe {
        if !IsWindow(Some(hwnd)).as_bool() {
            return Err(PlatformError::NotFound(
                "目标窗口已关闭或不存在".to_string(),
            ));
        }
        if IsIconic(hwnd).as_bool() {
            return Err(PlatformError::InvalidInput(
                "目标窗口当前处于最小化状态，无法直接截取，请先还原窗口或改截全屏".to_string(),
            ));
        }
        let mut rect = RECT::default();
        GetWindowRect(hwnd, &mut rect)
            .map_err(|e| PlatformError::System(format!("读取窗口尺寸失败：{e}")))?;
        let w = (rect.right - rect.left).max(0) as u32;
        let h = (rect.bottom - rect.top).max(0) as u32;
        if w == 0 || h == 0 {
            return Err(PlatformError::InvalidInput(
                "目标窗口尺寸为 0，无法截取".to_string(),
            ));
        }

        let screen_dc = GetDC(None);
        if screen_dc.is_invalid() {
            return Err(PlatformError::PermissionDenied(
                "对方电脑当前处于锁屏、权限确认界面或无桌面会话，无法截图".to_string(),
            ));
        }
        let screen_dc = ScreenDcGuard(screen_dc);
        let mem_dc_raw = CreateCompatibleDC(Some(screen_dc.0));
        if mem_dc_raw.is_invalid() {
            return Err(PlatformError::System("创建截图内存设备上下文失败".to_string()));
        }
        let mem_dc = MemDcGuard(mem_dc_raw);
        let bmp_raw = CreateCompatibleBitmap(screen_dc.0, w as i32, h as i32);
        if bmp_raw.is_invalid() {
            return Err(PlatformError::System("创建截图位图缓冲区失败".to_string()));
        }
        let bmp = BitmapGuard(bmp_raw);

        let old = SelectObject(mem_dc.0, HGDIOBJ(bmp.0.0));
        let mut captured =
            PrintWindow(hwnd, mem_dc.0, PRINT_WINDOW_FLAGS(PW_RENDERFULLCONTENT_FLAG)).as_bool();
        if !captured {
            captured = BitBlt(
                mem_dc.0,
                0,
                0,
                w as i32,
                h as i32,
                Some(screen_dc.0),
                rect.left,
                rect.top,
                SRCCOPY,
            )
            .is_ok();
        }
        // 调用 CreateBitmapFromHBITMAP / GetDIBits 前必须先解除位图选入 mem_dc
        let _ = SelectObject(mem_dc.0, old);

        if !captured {
            return Err(PlatformError::PermissionDenied(
                "无法截取该窗口画面（可能处于锁屏/安全桌面，或受窗口保护限制）".to_string(),
            ));
        }

        let (jpeg_bytes, img_w, img_h) =
            encode_hbitmap_to_jpeg(mem_dc.0, bmp.0, w, h, max_width, quality)?;
        Ok(CapturedImage {
            jpeg_bytes,
            origin_x: rect.left,
            origin_y: rect.top,
            source_width: w,
            source_height: h,
            image_width: img_w,
            image_height: img_h,
            dpi_scale_percent: window_dpi_scale_percent(hwnd),
            target_hwnd: Some(hwnd_raw),
            monitor_index: None,
        })
    }
}

/// 把当前鼠标指针画到截图位图上；`origin_x/origin_y` 是截图区域左上角的屏幕坐标。
/// 指针隐藏或读取失败时什么都不画，不影响截图本身。
unsafe fn draw_cursor_onto(mem_dc: HDC, origin_x: i32, origin_y: i32) {
    unsafe {
        let mut ci = CURSORINFO {
            cbSize: std::mem::size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        if GetCursorInfo(&mut ci).is_err() || ci.flags != CURSOR_SHOWING || ci.hCursor.is_invalid()
        {
            return;
        }
        let icon = HICON(ci.hCursor.0);
        let mut info = ICONINFO::default();
        let (hot_x, hot_y) = if GetIconInfo(icon, &mut info).is_ok() {
            if !info.hbmMask.is_invalid() {
                let _ = DeleteObject(HGDIOBJ(info.hbmMask.0));
            }
            if !info.hbmColor.is_invalid() {
                let _ = DeleteObject(HGDIOBJ(info.hbmColor.0));
            }
            (info.xHotspot as i32, info.yHotspot as i32)
        } else {
            (0, 0)
        };
        let _ = DrawIconEx(
            mem_dc,
            ci.ptScreenPos.x - origin_x - hot_x,
            ci.ptScreenPos.y - origin_y - hot_y,
            icon,
            0,
            0,
            0,
            None,
            DI_NORMAL,
        );
    }
}

/// 声明本进程按显示器 DPI 工作。不声明时，系统缩放 125%/150% 的电脑上
/// 截图坐标会被系统“虚拟化”，截不全、点按也会偏。已声明过（例如写在清单里）时调用失败，忽略即可。
pub fn enable_per_monitor_dpi_awareness() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

fn capture_dc_rect_to_jpeg(
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    desktop: &DesktopEnvironment,
    max_width: Option<u32>,
    quality: u8,
) -> Result<(Vec<u8>, u32, u32), PlatformError> {
    ensure_capturable_desktop(desktop)?;
    unsafe {
        let screen_dc = GetDC(None);
        if screen_dc.is_invalid() {
            return Err(PlatformError::PermissionDenied(
                "对方电脑当前处于锁屏、无显示器或权限确认界面，无法获取桌面画面".to_string(),
            ));
        }
        let screen_dc = ScreenDcGuard(screen_dc);
        let mem_dc_raw = CreateCompatibleDC(Some(screen_dc.0));
        if mem_dc_raw.is_invalid() {
            return Err(PlatformError::System("创建屏幕内存设备上下文失败".to_string()));
        }
        let mem_dc = MemDcGuard(mem_dc_raw);
        let bmp_raw = CreateCompatibleBitmap(screen_dc.0, w as i32, h as i32);
        if bmp_raw.is_invalid() {
            return Err(PlatformError::System("创建屏幕位图缓冲区失败".to_string()));
        }
        let bmp = BitmapGuard(bmp_raw);

        let old = SelectObject(mem_dc.0, HGDIOBJ(bmp.0.0));
        let blt_ok = BitBlt(
            mem_dc.0,
            0,
            0,
            w as i32,
            h as i32,
            Some(screen_dc.0),
            x,
            y,
            SRCCOPY,
        )
        .is_ok();

        // 仅在活动未锁屏的 Default 控制台桌面允许回退合成（锁屏/UAC/断连严格禁用）：
        // 先清背景防未初始化像素，再叠加未遮蔽窗口；若用户关闭或最小化全部窗口仍返回干净空桌面。
        let captured = if blt_ok {
            true
        } else if desktop.is_interactive() && is_default_input_desktop() {
            let _ = PatBlt(mem_dc.0, 0, 0, w as i32, h as i32, BLACKNESS);
            composite_desktop_windows_fallback(screen_dc.0, mem_dc.0, x, y, w, h);
            true
        } else {
            false
        };
        if captured {
            // BitBlt 截不到鼠标指针，自己画上去，方便看清对方鼠标停在哪里。
            draw_cursor_onto(mem_dc.0, x, y);
        }
        // 读取像素前先解除位图选入 mem_dc，满足 Win32 GetDIBits / WIC 契约
        let _ = SelectObject(mem_dc.0, old);

        if !captured {
            if !desktop.is_interactive() || !is_default_input_desktop() {
                return Err(PlatformError::PermissionDenied(
                    "对方电脑当前处于锁屏、无活动显示器或权限确认界面，无法截取桌面画面"
                        .to_string(),
                ));
            }
            return Err(PlatformError::PermissionDenied(
                "系统当前拒绝截取全屏画面（可能处于锁屏、安全桌面或远程会话已断开）".to_string(),
            ));
        }

        encode_hbitmap_to_jpeg(mem_dc.0, bmp.0, w, h, max_width, quality)
    }
}

unsafe fn extract_bgra_top_down(
    dc: HDC,
    bmp: HBITMAP,
    w: u32,
    h: u32,
) -> Result<Vec<u8>, PlatformError> {
    let byte_len = (w as usize)
        .checked_mul(h as usize)
        .and_then(|px| px.checked_mul(4))
        .ok_or_else(|| PlatformError::InvalidInput("截图尺寸超出内存范围".to_string()))?;
    let mut bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w as i32,
            biHeight: -(h as i32), // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut pixels = vec![0u8; byte_len];
    let lines = unsafe {
        GetDIBits(
            dc,
            bmp,
            0,
            h,
            Some(pixels.as_mut_ptr() as *mut c_void),
            &mut bmi,
            DIB_RGB_COLORS,
        )
    };
    if lines == 0 {
        return Err(PlatformError::System("读取屏幕像素缓冲失败".to_string()));
    }
    Ok(pixels)
}

unsafe fn composite_desktop_windows_fallback(
    screen_dc: HDC,
    dest_dc: HDC,
    origin_x: i32,
    origin_y: i32,
    width: u32,
    height: u32,
) {
    let windows = list_capturable_windows_impl();
    if windows.is_empty() {
        return;
    }
    // EnumWindows returns top-to-bottom Z-order; paint back-to-front
    for win in windows.into_iter().rev() {
        let hwnd = HWND(win.hwnd as *mut c_void);
        let mut rc = RECT::default();
        if unsafe { GetWindowRect(hwnd, &mut rc) }.is_err() {
            continue;
        }
        let ww = rc.right - rc.left;
        let wh = rc.bottom - rc.top;
        if ww <= 0 || wh <= 0 {
            continue;
        }
        if rc.right <= origin_x
            || rc.bottom <= origin_y
            || rc.left >= origin_x.saturating_add(width as i32)
            || rc.top >= origin_y.saturating_add(height as i32)
        {
            continue;
        }
        let wdc = unsafe { CreateCompatibleDC(Some(screen_dc)) };
        let wbmp = unsafe { CreateCompatibleBitmap(screen_dc, ww, wh) };
        if !wdc.is_invalid() && !wbmp.is_invalid() {
            let old = unsafe { SelectObject(wdc, HGDIOBJ(wbmp.0)) };
            if unsafe { PrintWindow(hwnd, wdc, PRINT_WINDOW_FLAGS(PW_RENDERFULLCONTENT_FLAG)) }
                .as_bool()
            {
                let _ = unsafe {
                    BitBlt(
                        dest_dc,
                        rc.left - origin_x,
                        rc.top - origin_y,
                        ww,
                        wh,
                        Some(wdc),
                        0,
                        0,
                        SRCCOPY,
                    )
                };
            }
            let _ = unsafe { SelectObject(wdc, old) };
        }
        if !wbmp.is_invalid() {
            let _ = unsafe { DeleteObject(HGDIOBJ(wbmp.0)) };
        }
        if !wdc.is_invalid() {
            let _ = unsafe { DeleteDC(wdc) };
        }
    }
}

/// 将已解除选入的 `HBITMAP` 直接交由 WIC 缩放并编码为 JPEG，优先使用 `CreateBitmapFromHBITMAP`
/// 省去一次整帧 `GetDIBits` 中间缓冲拷贝；若驱动不支持则回退 `GetDIBits`。
unsafe fn encode_hbitmap_to_jpeg(
    dc: HDC,
    bmp: HBITMAP,
    src_w: u32,
    src_h: u32,
    max_width: Option<u32>,
    quality: u8,
) -> Result<(Vec<u8>, u32, u32), PlatformError> {
    let dst_w = match max_width {
        Some(mw) if mw > 0 && mw < src_w => mw,
        _ => src_w,
    };
    let dst_h = if dst_w < src_w {
        (((src_h as u64) * (dst_w as u64) + (src_w as u64) / 2) / (src_w as u64)).max(1) as u32
    } else {
        src_h
    };

    let _com = ComGuard::init_sta();
    let factory: IWICImagingFactory =
        unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER) }
            .map_err(|e| PlatformError::System(format!("初始化系统图像编码器失败：{e}")))?;

    let bitmap: IWICBitmap = match unsafe {
        factory.CreateBitmapFromHBITMAP(bmp, HPALETTE::default(), WICBitmapIgnoreAlpha)
    } {
        Ok(b) => b,
        Err(_) => {
            let bgra = unsafe { extract_bgra_top_down(dc, bmp, src_w, src_h) }?;
            let stride = src_w
                .checked_mul(4)
                .ok_or_else(|| PlatformError::InvalidInput("截图行步长溢出".to_string()))?;
            unsafe {
                factory.CreateBitmapFromMemory(
                    src_w,
                    src_h,
                    &GUID_WICPixelFormat32bppBGRA,
                    stride,
                    &bgra,
                )
            }
            .map_err(|e| PlatformError::System(format!("创建 WIC 图像对象失败：{e}")))?
        }
    };

    let bytes = unsafe {
        encode_wic_bitmap_to_jpeg(&factory, &bitmap, src_w, src_h, dst_w, dst_h, quality)
    }?;
    Ok((bytes, dst_w, dst_h))
}

unsafe fn encode_wic_bitmap_to_jpeg(
    factory: &IWICImagingFactory,
    bitmap: &IWICBitmap,
    src_w: u32,
    src_h: u32,
    dst_w: u32,
    dst_h: u32,
    quality: u8,
) -> Result<Vec<u8>, PlatformError> {
    let scaled_source: IWICBitmapSource = if dst_w != src_w || dst_h != src_h {
        let scaler = unsafe { factory.CreateBitmapScaler() }
            .map_err(|e| PlatformError::System(format!("创建图像缩放器失败：{e}")))?;
        unsafe { scaler.Initialize(bitmap, dst_w, dst_h, WICBitmapInterpolationModeFant) }
            .map_err(|e| PlatformError::System(format!("缩放截图失败：{e}")))?;
        scaler
            .cast()
            .map_err(|e| PlatformError::System(format!("转换缩放源失败：{e}")))?
    } else {
        bitmap
            .cast()
            .map_err(|e| PlatformError::System(format!("转换图像源失败：{e}")))?
    };

    let converter = unsafe { factory.CreateFormatConverter() }
        .map_err(|e| PlatformError::System(format!("创建像素格式转换器失败：{e}")))?;
    unsafe {
        converter.Initialize(
            &scaled_source,
            &GUID_WICPixelFormat24bppBGR,
            WICBitmapDitherTypeNone,
            None,
            0.0,
            WICBitmapPaletteTypeCustom,
        )
    }
    .map_err(|e| PlatformError::System(format!("转换 24 位 BGR 像素失败：{e}")))?;

    let stream = unsafe { SHCreateMemStream(None) }
        .ok_or_else(|| PlatformError::System("创建内存图像流失败".to_string()))?;

    let encoder = unsafe { factory.CreateEncoder(&GUID_ContainerFormatJpeg, std::ptr::null()) }
        .map_err(|e| PlatformError::System(format!("创建 JPEG 编码器失败：{e}")))?;
    unsafe { encoder.Initialize(&stream, WICBitmapEncoderNoCache) }
        .map_err(|e| PlatformError::System(format!("初始化 JPEG 编码器失败：{e}")))?;

    let mut frame: Option<IWICBitmapFrameEncode> = None;
    let mut props = None;
    unsafe { encoder.CreateNewFrame(&mut frame, &mut props) }
        .map_err(|e| PlatformError::System(format!("创建 JPEG 帧失败：{e}")))?;
    let frame = frame.ok_or_else(|| PlatformError::System("JPEG 帧对象为空".to_string()))?;

    if let Some(ref bag) = props {
        let mut prop_name = wide_null("ImageQuality");
        let prop = PROPBAG2 {
            pstrName: PWSTR(prop_name.as_mut_ptr()),
            ..Default::default()
        };
        let q = (quality.clamp(10, 100) as f32) / 100.0;
        let var = VARIANT {
            Anonymous: VARIANT_0 {
                Anonymous: std::mem::ManuallyDrop::new(VARIANT_0_0 {
                    vt: VT_R4,
                    wReserved1: 0,
                    wReserved2: 0,
                    wReserved3: 0,
                    Anonymous: VARIANT_0_0_0 { fltVal: q },
                }),
            },
        };
        let _ = unsafe { bag.Write(1, &prop, &var) };
    }

    unsafe { frame.Initialize(props.as_ref()) }
        .map_err(|e| PlatformError::System(format!("初始化 JPEG 帧参数失败：{e}")))?;
    unsafe { frame.SetSize(dst_w, dst_h) }
        .map_err(|e| PlatformError::System(format!("设置 JPEG 尺寸失败：{e}")))?;
    let mut fmt = GUID_WICPixelFormat24bppBGR;
    unsafe { frame.SetPixelFormat(&mut fmt) }
        .map_err(|e| PlatformError::System(format!("设置 JPEG 像素格式失败：{e}")))?;
    unsafe { frame.WriteSource(&converter, std::ptr::null()) }
        .map_err(|e| PlatformError::System(format!("写入 JPEG 图像像素失败：{e}")))?;
    unsafe { frame.Commit() }
        .map_err(|e| PlatformError::System(format!("提交 JPEG 帧失败：{e}")))?;
    unsafe { encoder.Commit() }
        .map_err(|e| PlatformError::System(format!("提交 JPEG 流失败：{e}")))?;

    unsafe { stream.Seek(0, STREAM_SEEK_SET, None) }
        .map_err(|e| PlatformError::System(format!("回绕 JPEG 内存流失败：{e}")))?;

    let mut out = Vec::with_capacity(64 * 1024);
    let mut chunk = [0u8; 16 * 1024];
    loop {
        let mut read_bytes = 0u32;
        let hr = unsafe {
            stream.Read(
                chunk.as_mut_ptr() as *mut c_void,
                chunk.len() as u32,
                Some(&mut read_bytes),
            )
        };
        if hr.is_err() {
            return Err(PlatformError::System(format!(
                "读取 JPEG 内存流失败：{}",
                windows::core::Error::from_hresult(hr)
            )));
        }
        if read_bytes == 0 {
            break;
        }
        out.extend_from_slice(&chunk[..read_bytes as usize]);
    }
    if out.is_empty() {
        return Err(PlatformError::System("编码生成的 JPEG 图像为空".to_string()));
    }
    Ok(out)
}

// ──────────────────────────── 输入注入与 UIPI 检测（任务 3.3）────────────────────────────

pub fn send_mouse_click_impl(
    screen_x: i32,
    screen_y: i32,
    button: PlatformMouseBtn,
    double: bool,
    target_hwnd: Option<usize>,
) -> Result<InputOutcome, PlatformError> {
    unsafe {
        if let Some(raw) = target_hwnd {
            let h = HWND(raw as *mut c_void);
            if IsWindow(Some(h)).as_bool()
                && let Some(warning) = check_window_uipi_warning(h)
            {
                return Ok(InputOutcome {
                    uipi_blocked: true,
                    warning: Some(warning),
                });
            }
            crate::desktop::verify_target(target_hwnd, Some((screen_x, screen_y)), true)?;
        }

        let point_hwnd = WindowFromPoint(POINT {
            x: screen_x,
            y: screen_y,
        });
        let hit_hwnd = if !point_hwnd.0.is_null() {
            point_hwnd
        } else if let Some(raw) = target_hwnd {
            HWND(raw as *mut c_void)
        } else {
            HWND::default()
        };

        if let Some(warning) = check_window_uipi_warning(hit_hwnd) {
            return Ok(InputOutcome {
                uipi_blocked: true,
                warning: Some(warning),
            });
        }

        let (dx, dy) = virtual_screen_to_absolute(screen_x, screen_y);

        let (down_flag, up_flag) = match button {
            PlatformMouseBtn::Left => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
            PlatformMouseBtn::Right => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
            PlatformMouseBtn::Middle => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
        };
        let base_flags = MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK;

        let make_mouse = |flags| INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx,
                    dy,
                    mouseData: 0,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };

        let mut inputs = vec![
            make_mouse(base_flags | MOUSEEVENTF_MOVE),
            make_mouse(base_flags | down_flag),
            make_mouse(base_flags | up_flag),
        ];
        if double {
            inputs.push(make_mouse(base_flags | down_flag));
            inputs.push(make_mouse(base_flags | up_flag));
        }

        let sent = SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) as usize;
        if sent != inputs.len() {
            // 若只发送了按下而未发送抬起，补发一次抬起防止鼠标按键卡住
            let mut down_balance = 0isize;
            for item in inputs.iter().take(sent) {
                let flags = item.Anonymous.mi.dwFlags;
                if (flags.0 & down_flag.0) != 0 {
                    down_balance += 1;
                }
                if (flags.0 & up_flag.0) != 0 {
                    down_balance -= 1;
                }
            }
            if down_balance > 0 {
                let release = [make_mouse(base_flags | up_flag)];
                let _ = SendInput(&release, std::mem::size_of::<INPUT>() as i32);
            }
            return Ok(InputOutcome {
                uipi_blocked: true,
                warning: Some(
                    "系统拦截了本次鼠标点击（对方窗口可能以管理员权限运行，或处于锁屏/安全桌面）"
                        .to_string(),
                ),
            });
        }
        Ok(InputOutcome {
            uipi_blocked: false,
            warning: None,
        })
    }
}

fn build_vk_input(vk: VIRTUAL_KEY, key_up: bool) -> INPUT {
    let mut flags = if key_up {
        KEYEVENTF_KEYUP
    } else {
        KEYBD_EVENT_FLAGS(0)
    };
    if is_extended_vk(vk.0) {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn build_combo_inputs(vks: &[VIRTUAL_KEY]) -> Vec<INPUT> {
    let mut out = Vec::with_capacity(vks.len() * 2);
    for &vk in vks {
        out.push(build_vk_input(vk, false));
    }
    for &vk in vks.iter().rev() {
        out.push(build_vk_input(vk, true));
    }
    out
}

/// 统计在 `SendInput` 仅发送了前 `sent` 项后仍处于按下状态的虚拟键（按后进先出顺序返回，便于补释放防卡键）。
fn compute_stuck_keys_to_release(inputs: &[INPUT], sent: usize) -> Vec<VIRTUAL_KEY> {
    let mut pressed: Vec<VIRTUAL_KEY> = Vec::new();
    for item in inputs.iter().take(sent.min(inputs.len())) {
        if item.r#type != INPUT_KEYBOARD {
            continue;
        }
        let ki = unsafe { item.Anonymous.ki };
        if (ki.dwFlags.0 & KEYEVENTF_UNICODE.0) != 0 || ki.wVk.0 == 0 {
            continue;
        }
        if (ki.dwFlags.0 & KEYEVENTF_KEYUP.0) == 0 {
            pressed.push(ki.wVk);
        } else if let Some(pos) = pressed.iter().rposition(|&k| k == ki.wVk) {
            pressed.remove(pos);
        }
    }
    pressed.reverse();
    pressed
}

pub fn send_key_or_text_impl(
    text: Option<&str>,
    key_combo: Option<&str>,
    target_hwnd: Option<usize>,
) -> Result<InputOutcome, PlatformError> {
    unsafe {
        if let Some(raw) = target_hwnd {
            let h = HWND(raw as *mut c_void);
            if IsWindow(Some(h)).as_bool()
                && let Some(warning) = check_window_uipi_warning(h)
            {
                return Ok(InputOutcome {
                    uipi_blocked: true,
                    warning: Some(warning),
                });
            }
            crate::desktop::verify_target(target_hwnd, None, true)?;
        }

        let fg = GetForegroundWindow();
        let focus_hwnd = if !fg.0.is_null() {
            fg
        } else if let Some(raw) = target_hwnd {
            HWND(raw as *mut c_void)
        } else {
            HWND::default()
        };

        if let Some(warning) = check_window_uipi_warning(focus_hwnd) {
            return Ok(InputOutcome {
                uipi_blocked: true,
                warning: Some(warning),
            });
        }

        let mut inputs: Vec<INPUT> = Vec::new();
        if let Some(s) = text.filter(|t| !t.is_empty()) {
            for unit in s.encode_utf16() {
                inputs.push(INPUT {
                    r#type: INPUT_KEYBOARD,
                    Anonymous: INPUT_0 {
                        ki: KEYBDINPUT {
                            wVk: VIRTUAL_KEY(0),
                            wScan: unit,
                            dwFlags: KEYEVENTF_UNICODE,
                            time: 0,
                            dwExtraInfo: 0,
                        },
                    },
                });
                inputs.push(INPUT {
                    r#type: INPUT_KEYBOARD,
                    Anonymous: INPUT_0 {
                        ki: KEYBDINPUT {
                            wVk: VIRTUAL_KEY(0),
                            wScan: unit,
                            dwFlags: KEYEVENTF_UNICODE | KEYEVENTF_KEYUP,
                            time: 0,
                            dwExtraInfo: 0,
                        },
                    },
                });
            }
        }

        if let Some(combo) = key_combo.map(str::trim).filter(|c| !c.is_empty()) {
            let vks = parse_key_combo(combo)?;
            inputs.extend(build_combo_inputs(&vks));
        }

        if inputs.is_empty() {
            return Err(PlatformError::InvalidInput(
                "未提供要输入的文字或按键组合".to_string(),
            ));
        }

        let sent = SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) as usize;
        if sent != inputs.len() {
            let stuck = compute_stuck_keys_to_release(&inputs, sent);
            if !stuck.is_empty() {
                let releases: Vec<INPUT> = stuck
                    .into_iter()
                    .map(|vk| build_vk_input(vk, true))
                    .collect();
                let _ = SendInput(&releases, std::mem::size_of::<INPUT>() as i32);
            }
            return Ok(InputOutcome {
                uipi_blocked: true,
                warning: Some(
                    "系统拦截了本次键盘输入（对方当前焦点窗口可能以管理员权限运行，或处于安全桌面）"
                        .to_string(),
                ),
            });
        }
        Ok(InputOutcome {
            uipi_blocked: false,
            warning: None,
        })
    }
}

fn parse_key_combo(combo: &str) -> Result<Vec<VIRTUAL_KEY>, PlatformError> {
    let mut keys = Vec::new();
    for raw in combo.split('+').map(str::trim).filter(|p| !p.is_empty()) {
        let lower = raw.to_ascii_lowercase();
        let vk = match lower.as_str() {
            "ctrl" | "control" => VK_CONTROL,
            "rctrl" | "rcontrol" => VK_RCONTROL,
            "alt" | "menu" => VK_LMENU,
            "ralt" | "rmenu" => VK_RMENU,
            "shift" => VK_SHIFT,
            "win" | "meta" | "super" => VK_LWIN,
            "rwin" => VK_RWIN,
            "apps" | "contextmenu" => VK_APPS,
            "enter" | "return" => VK_RETURN,
            "esc" | "escape" => VK_ESCAPE,
            "tab" => VK_TAB,
            "space" => VK_SPACE,
            "backspace" | "bksp" => VK_BACK,
            "ins" | "insert" => VK_INSERT,
            "del" | "delete" => VK_DELETE,
            "printscreen" | "prtsc" => VK_SNAPSHOT,
            "up" | "arrowup" => VK_UP,
            "down" | "arrowdown" => VK_DOWN,
            "left" | "arrowleft" => VK_LEFT,
            "right" | "arrowright" => VK_RIGHT,
            "home" => VK_HOME,
            "end" => VK_END,
            "pageup" | "pgup" => VK_PRIOR,
            "pagedown" | "pgdn" => VK_NEXT,
            s if s.starts_with('f') && s.len() <= 3 => {
                if let Ok(n) = s[1..].parse::<u16>()
                    && (1..=12).contains(&n)
                {
                    VIRTUAL_KEY(VK_F1.0 + (n - 1))
                } else {
                    return Err(PlatformError::InvalidInput(format!(
                        "不支持的快捷键名：{raw}"
                    )));
                }
            }
            s if s.len() == 1 => {
                let ch = s.as_bytes()[0];
                if ch.is_ascii_lowercase() {
                    VIRTUAL_KEY((ch - b'a' + b'A') as u16)
                } else if ch.is_ascii_digit() {
                    VIRTUAL_KEY(ch as u16)
                } else {
                    return Err(PlatformError::InvalidInput(format!(
                        "不支持的按键字符：{raw}"
                    )));
                }
            }
            _ => {
                return Err(PlatformError::InvalidInput(format!(
                    "不支持的按键名称：{raw}"
                )));
            }
        };
        keys.push(vk);
    }
    if keys.is_empty() {
        return Err(PlatformError::InvalidInput("按键组合不能为空".to_string()));
    }
    Ok(keys)
}

// ──────────────────────────── 剪贴板读写（文字 + PNG/DIB，任务 3.7）────────────────────────────

struct ClipboardGuard;
impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseClipboard();
        }
    }
}

fn open_clipboard_with_retry() -> Result<ClipboardGuard, PlatformError> {
    for _ in 0..8 {
        if unsafe { OpenClipboard(None) }.is_ok() {
            return Ok(ClipboardGuard);
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    Err(PlatformError::Busy(
        "剪贴板正被其他软件占用，请稍后再试".to_string(),
    ))
}

pub fn clipboard_sequence_impl() -> u32 {
    unsafe { GetClipboardSequenceNumber() }
}

pub fn get_clipboard_impl() -> Result<ClipboardData, PlatformError> {
    let _guard = open_clipboard_with_retry()?;
    let sequence = unsafe { GetClipboardSequenceNumber() };

    let png_fmt_name = wide_null("PNG");
    let png_fmt = unsafe { RegisterClipboardFormatW(PCWSTR(png_fmt_name.as_ptr())) };
    let has_text = unsafe { IsClipboardFormatAvailable(CF_UNICODETEXT.0 as u32) }.is_ok();
    let has_png = png_fmt != 0 && unsafe { IsClipboardFormatAvailable(png_fmt) }.is_ok();
    let has_dib = unsafe { IsClipboardFormatAvailable(CF_DIB.0 as u32) }.is_ok();

    if has_png
        && !has_text
        && let Ok(bytes) = unsafe { read_clipboard_raw_bytes(png_fmt) }
        && let Ok((w, h)) = inspect_png_dimensions(&bytes)
    {
        return Ok(ClipboardData {
            sequence,
            content: ClipboardContent::ImagePng {
                png_bytes: bytes,
                width: w,
                height: h,
            },
        });
    }

    if has_text {
        let handle = unsafe { GetClipboardData(CF_UNICODETEXT.0 as u32) }
            .map_err(|e| PlatformError::System(format!("读取剪贴板文字失败：{e}")))?;
        let hglobal = HGLOBAL(handle.0);
        let ptr = unsafe { GlobalLock(hglobal) } as *const u16;
        if ptr.is_null() {
            return Err(PlatformError::System("锁定剪贴板文字内存失败".to_string()));
        }
        let max_u16 = (unsafe { GlobalSize(hglobal) }) / 2;
        let slice = unsafe { std::slice::from_raw_parts(ptr, max_u16) };
        let len = slice.iter().position(|&c| c == 0).unwrap_or(slice.len());
        let text = String::from_utf16_lossy(&slice[..len]);
        let _ = unsafe { GlobalUnlock(hglobal) };
        return Ok(ClipboardData {
            sequence,
            content: ClipboardContent::Text(text),
        });
    }

    if has_dib {
        let raw = unsafe { read_clipboard_raw_bytes(CF_DIB.0 as u32) }?;
        let (png_bytes, width, height) = dib_to_png(&raw)?;
        return Ok(ClipboardData {
            sequence,
            content: ClipboardContent::ImagePng {
                png_bytes,
                width,
                height,
            },
        });
    }

    Ok(ClipboardData {
        sequence,
        content: ClipboardContent::Empty,
    })
}

unsafe fn read_clipboard_raw_bytes(fmt: u32) -> Result<Vec<u8>, PlatformError> {
    let handle = unsafe { GetClipboardData(fmt) }
        .map_err(|e| PlatformError::System(format!("读取剪贴板图像数据失败：{e}")))?;
    let hglobal = HGLOBAL(handle.0);
    let size = unsafe { GlobalSize(hglobal) };
    if size == 0 {
        return Err(PlatformError::System("剪贴板数据为空".to_string()));
    }
    let ptr = unsafe { GlobalLock(hglobal) } as *const u8;
    if ptr.is_null() {
        return Err(PlatformError::System("锁定剪贴板内存失败".to_string()));
    }
    let bytes = unsafe { std::slice::from_raw_parts(ptr, size) }.to_vec();
    let _ = unsafe { GlobalUnlock(hglobal) };
    Ok(bytes)
}

fn inspect_png_dimensions(png_bytes: &[u8]) -> Result<(u32, u32), PlatformError> {
    let decoder = png::Decoder::new(Cursor::new(png_bytes));
    let reader = decoder
        .read_info()
        .map_err(|e| PlatformError::InvalidInput(format!("解析 PNG 图片头失败：{e}")))?;
    let info = reader.info();
    Ok((info.width, info.height))
}

fn validate_clipboard_image_dims(width: u32, height: u32) -> Result<usize, PlatformError> {
    if width == 0 || height == 0 {
        return Err(PlatformError::InvalidInput(
            "剪贴板图像宽或高为 0".to_string(),
        ));
    }
    if width > MAX_CLIPBOARD_IMAGE_DIM || height > MAX_CLIPBOARD_IMAGE_DIM {
        return Err(PlatformError::InvalidInput(format!(
            "剪贴板图像尺寸 {width}×{height} 超过上限 {MAX_CLIPBOARD_IMAGE_DIM}×{MAX_CLIPBOARD_IMAGE_DIM}"
        )));
    }
    let pixels = (width as usize)
        .checked_mul(height as usize)
        .ok_or_else(|| PlatformError::InvalidInput("剪贴板图像像素数溢出".to_string()))?;
    if pixels > MAX_CLIPBOARD_IMAGE_PIXELS {
        return Err(PlatformError::InvalidInput(format!(
            "剪贴板图像总像素数 {pixels} 超过安全上限 {MAX_CLIPBOARD_IMAGE_PIXELS}"
        )));
    }
    Ok(pixels)
}

fn dib_to_png(dib: &[u8]) -> Result<(Vec<u8>, u32, u32), PlatformError> {
    let base_hdr_size = std::mem::size_of::<BITMAPINFOHEADER>();
    if dib.len() < base_hdr_size {
        return Err(PlatformError::InvalidInput(
            "剪贴板位图数据长度不足".to_string(),
        ));
    }
    let hdr: BITMAPINFOHEADER =
        unsafe { std::ptr::read_unaligned(dib.as_ptr() as *const BITMAPINFOHEADER) };
    let bi_size = hdr.biSize as usize;
    if bi_size < base_hdr_size || bi_size > dib.len() {
        return Err(PlatformError::InvalidInput(
            "剪贴板位图头长度无效".to_string(),
        ));
    }
    if hdr.biWidth <= 0 || hdr.biHeight == 0 || hdr.biHeight == i32::MIN {
        return Err(PlatformError::InvalidInput(
            "剪贴板位图尺寸无效".to_string(),
        ));
    }
    let width = hdr.biWidth as u32;
    let top_down = hdr.biHeight < 0;
    let height = hdr.biHeight.unsigned_abs();
    let bpp = hdr.biBitCount as usize;
    if bpp != 24 && bpp != 32 {
        return Err(PlatformError::Unsupported(format!(
            "暂不支持 {} 位深度的特殊剪贴板位图格式",
            hdr.biBitCount
        )));
    }
    let pixel_count = validate_clipboard_image_dims(width, height)?;

    // 仅允许 BI_RGB（24/32 位）或 32 位 BI_BITFIELDS；并仅在 BITMAPINFOHEADER(40 字节) 时追加 12 字节掩码偏移
    let (r_idx, g_idx, b_idx, a_idx) = if hdr.biCompression == BI_RGB.0 {
        (2usize, 1usize, 0usize, 3usize)
    } else if hdr.biCompression == BI_BITFIELDS.0 && bpp == 32 {
        if dib.len() < base_hdr_size + 12 {
            return Err(PlatformError::InvalidInput(
                "剪贴板 BI_BITFIELDS 位图缺少通道掩码".to_string(),
            ));
        }
        let r_mask = u32::from_le_bytes(dib[40..44].try_into().unwrap());
        let g_mask = u32::from_le_bytes(dib[44..48].try_into().unwrap());
        let b_mask = u32::from_le_bytes(dib[48..52].try_into().unwrap());
        match (r_mask, g_mask, b_mask) {
            (0x00FF_0000, 0x0000_FF00, 0x0000_00FF) => (2, 1, 0, 3),
            (0x0000_00FF, 0x0000_FF00, 0x00FF_0000) => (0, 1, 2, 3),
            _ => {
                return Err(PlatformError::Unsupported(
                    "暂不支持非标准通道掩码的剪贴板位图".to_string(),
                ));
            }
        }
    } else {
        return Err(PlatformError::Unsupported(format!(
            "暂不支持压缩类型 {} 的剪贴板位图",
            hdr.biCompression
        )));
    };

    let extra_masks = if hdr.biSize as usize == base_hdr_size && hdr.biCompression == BI_BITFIELDS.0
    {
        12usize
    } else {
        0usize
    };
    let clr_bytes = (hdr.biClrUsed as usize)
        .checked_mul(4)
        .ok_or_else(|| PlatformError::InvalidInput("剪贴板位图调色板长度溢出".to_string()))?;
    let header_offset = bi_size
        .checked_add(extra_masks)
        .and_then(|v| v.checked_add(clr_bytes))
        .ok_or_else(|| PlatformError::InvalidInput("剪贴板位图偏移溢出".to_string()))?;
    let row_bits = (width as usize)
        .checked_mul(bpp)
        .ok_or_else(|| PlatformError::InvalidInput("剪贴板位图行宽溢出".to_string()))?;
    let row_stride = row_bits
        .div_ceil(32)
        .checked_mul(4)
        .ok_or_else(|| PlatformError::InvalidInput("剪贴板位图行步长溢出".to_string()))?;
    let pixel_bytes = row_stride
        .checked_mul(height as usize)
        .ok_or_else(|| PlatformError::InvalidInput("剪贴板位图数据长度溢出".to_string()))?;
    let needed = header_offset
        .checked_add(pixel_bytes)
        .ok_or_else(|| PlatformError::InvalidInput("剪贴板位图总长度溢出".to_string()))?;
    if dib.len() < needed {
        return Err(PlatformError::InvalidInput(
            "剪贴板位图像素数据不完整".to_string(),
        ));
    }

    let rgba_len = pixel_count
        .checked_mul(4)
        .ok_or_else(|| PlatformError::InvalidInput("剪贴板图像缓冲区溢出".to_string()))?;
    let mut rgba = vec![0u8; rgba_len];
    let mut any_nonzero_alpha = false;
    for y in 0..(height as usize) {
        let src_y = if top_down { y } else { (height as usize) - 1 - y };
        let src_row = &dib[header_offset + src_y * row_stride..];
        let dst_row = &mut rgba[y * (width as usize) * 4..(y + 1) * (width as usize) * 4];
        for x in 0..(width as usize) {
            if bpp == 32 {
                let px = &src_row[x * 4..x * 4 + 4];
                let a = px[a_idx];
                if a != 0 {
                    any_nonzero_alpha = true;
                }
                dst_row[x * 4] = px[r_idx];
                dst_row[x * 4 + 1] = px[g_idx];
                dst_row[x * 4 + 2] = px[b_idx];
                dst_row[x * 4 + 3] = a;
            } else {
                let b = src_row[x * 3];
                let g = src_row[x * 3 + 1];
                let r = src_row[x * 3 + 2];
                dst_row[x * 4] = r;
                dst_row[x * 4 + 1] = g;
                dst_row[x * 4 + 2] = b;
                dst_row[x * 4 + 3] = 255;
            }
        }
    }
    // 仅当整张 32 位位图全部像素 alpha 均为 0（传统 XRGB 填充）时才统一补 255，保留真实透明通道
    if bpp == 32 && !any_nonzero_alpha {
        for px in rgba.as_chunks_mut::<4>().0 {
            px[3] = 255;
        }
    }

    let mut png_out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png_out, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|e| PlatformError::System(format!("写入剪贴板 PNG 头失败：{e}")))?;
        writer
            .write_image_data(&rgba)
            .map_err(|e| PlatformError::System(format!("编码剪贴板 PNG 失败：{e}")))?;
    }
    Ok((png_out, width, height))
}

pub fn set_clipboard_text_impl(text: &str) -> Result<u32, PlatformError> {
    let wide = wide_null(text);
    let byte_len = wide.len() * 2;
    let _guard = open_clipboard_with_retry()?;
    unsafe {
        EmptyClipboard().map_err(|e| PlatformError::System(format!("清空剪贴板失败：{e}")))?;
        write_bytes_to_clipboard_format(
            CF_UNICODETEXT.0 as u32,
            std::slice::from_raw_parts(wide.as_ptr() as *const u8, byte_len),
        )?;
        Ok(GetClipboardSequenceNumber())
    }
}

pub fn set_clipboard_png_impl(png_bytes: &[u8]) -> Result<u32, PlatformError> {
    let dib_bytes = png_to_dib(png_bytes)?;
    let _guard = open_clipboard_with_retry()?;
    unsafe {
        EmptyClipboard().map_err(|e| PlatformError::System(format!("清空剪贴板失败：{e}")))?;
        let png_fmt_name = wide_null("PNG");
        let png_fmt = RegisterClipboardFormatW(PCWSTR(png_fmt_name.as_ptr()));
        if png_fmt != 0 {
            write_bytes_to_clipboard_format(png_fmt, png_bytes)?;
        }
        write_bytes_to_clipboard_format(CF_DIB.0 as u32, &dib_bytes)?;
        Ok(GetClipboardSequenceNumber())
    }
}

unsafe fn write_bytes_to_clipboard_format(fmt: u32, bytes: &[u8]) -> Result<(), PlatformError> {
    let hglobal = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes.len()) }
        .map_err(|e| PlatformError::System(format!("分配剪贴板全局内存失败：{e}")))?;
    let ptr = unsafe { GlobalLock(hglobal) } as *mut u8;
    if ptr.is_null() {
        let _ = unsafe { GlobalFree(Some(hglobal)) };
        return Err(PlatformError::System("锁定剪贴板写入内存失败".to_string()));
    }
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
        let _ = GlobalUnlock(hglobal);
    }
    if let Err(err) = unsafe { SetClipboardData(fmt, Some(HANDLE(hglobal.0))) } {
        let _ = unsafe { GlobalFree(Some(hglobal)) };
        return Err(PlatformError::System(format!("设置剪贴板数据失败：{err}")));
    }
    Ok(())
}

fn png_to_dib(png_bytes: &[u8]) -> Result<Vec<u8>, PlatformError> {
    let mut decoder = png::Decoder::new_with_limits(
        Cursor::new(png_bytes),
        png::Limits {
            bytes: MAX_CLIPBOARD_PNG_DECODE_BYTES,
        },
    );
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder
        .read_info()
        .map_err(|e| PlatformError::InvalidInput(format!("解析剪贴板 PNG 图片失败：{e}")))?;

    let header_w = reader.info().width;
    let header_h = reader.info().height;
    let pixel_count = validate_clipboard_image_dims(header_w, header_h)?;
    let dib_pixel_bytes = pixel_count
        .checked_mul(4)
        .ok_or_else(|| PlatformError::InvalidInput("剪贴板 PNG 像素字节数溢出".to_string()))?;
    let bi_size_image = u32::try_from(dib_pixel_bytes)
        .map_err(|_| PlatformError::InvalidInput("剪贴板 PNG 像素长度超出 DIB 上限".to_string()))?;

    let buf_size = reader
        .output_buffer_size()
        .filter(|&sz| sz <= MAX_CLIPBOARD_PNG_DECODE_BYTES)
        .ok_or_else(|| PlatformError::InvalidInput("剪贴板 PNG 图像尺寸过大".to_string()))?;
    let mut buf = vec![0u8; buf_size];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|e| PlatformError::InvalidInput(format!("解码剪贴板 PNG 帧失败：{e}")))?;
    if info.width != header_w || info.height != header_h {
        return Err(PlatformError::InvalidInput(
            "剪贴板 PNG 帧尺寸与图像头不一致".to_string(),
        ));
    }
    let width = info.width;
    let height = info.height;
    let src = &buf[..info.buffer_size()];

    let hdr = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: width as i32,
        biHeight: height as i32, // bottom-up DIB for maximum Windows app compatibility
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB.0,
        biSizeImage: bi_size_image,
        ..Default::default()
    };

    let hdr_size = std::mem::size_of::<BITMAPINFOHEADER>();
    let total_len = hdr_size
        .checked_add(dib_pixel_bytes)
        .ok_or_else(|| PlatformError::InvalidInput("剪贴板 DIB 总长度溢出".to_string()))?;
    let mut out = vec![0u8; total_len];
    unsafe {
        std::ptr::copy_nonoverlapping(
            &hdr as *const BITMAPINFOHEADER as *const u8,
            out.as_mut_ptr(),
            hdr_size,
        );
    }
    let pixels = &mut out[hdr_size..];
    for y in 0..(height as usize) {
        let dst_y = (height as usize) - 1 - y;
        let dst_row =
            &mut pixels[dst_y * (width as usize) * 4..(dst_y + 1) * (width as usize) * 4];
        for x in 0..(width as usize) {
            let (r, g, b, a) = match info.color_type {
                png::ColorType::Rgba => {
                    let i = (y * (width as usize) + x) * 4;
                    (src[i], src[i + 1], src[i + 2], src[i + 3])
                }
                png::ColorType::Rgb => {
                    let i = (y * (width as usize) + x) * 3;
                    (src[i], src[i + 1], src[i + 2], 255)
                }
                png::ColorType::GrayscaleAlpha => {
                    let i = (y * (width as usize) + x) * 2;
                    (src[i], src[i], src[i], src[i + 1])
                }
                png::ColorType::Grayscale | png::ColorType::Indexed => {
                    let v = src[y * (width as usize) + x];
                    (v, v, v, 255)
                }
            };
            dst_row[x * 4] = b;
            dst_row[x * 4 + 1] = g;
            dst_row[x * 4 + 2] = r;
            dst_row[x * 4 + 3] = a;
        }
    }
    Ok(out)
}

// ──────────────────────────── 拖出文件到系统桌面 / 资源管理器 ────────────────────────────

pub fn start_file_drag_out_sta(paths: Vec<PathBuf>) -> Result<bool, PlatformError> {
    if paths.is_empty() {
        return Err(PlatformError::InvalidInput("未选择要拖出的文件".to_string()));
    }
    let mut cleaned_paths = Vec::with_capacity(paths.len());
    for p in &paths {
        if std::fs::symlink_metadata(p).is_err() {
            return Err(PlatformError::NotFound(format!(
                "找不到准备拖出的本地文件：{}",
                p.display()
            )));
        }
        let abs = std::path::absolute(p).unwrap_or_else(|_| p.clone());
        cleaned_paths.push(strip_verbatim_prefix(&abs));
    }

    // RAII 守卫必须在所有 OLE/COM 对象之前声明，保证任意早返回分支均会先释放对象再 OleUninitialize
    let _ole = OleGuard::init()?;

    unsafe {
        // 确保当前 STA 线程已建立消息队列并持有隐藏消息窗口，避免无窗口线程拖出被立即取消
        let mut dummy_msg = MSG::default();
        let _ = PeekMessageW(&mut dummy_msg, None, 0, 0, PM_REMOVE);
        let static_cls = wide_null("STATIC");
        let helper_hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(static_cls.as_ptr()),
            PCWSTR::null(),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            None,
            None,
        )
        .ok()
        .map(HwndGuard);

        let mut pidls = PidlList(Vec::with_capacity(cleaned_paths.len()));
        for clean in &cleaned_paths {
            let wide = wide_null(clean);
            let pidl = ILCreateFromPathW(PCWSTR(wide.as_ptr()));
            if pidl.is_null() {
                return Err(PlatformError::NotFound(format!(
                    "无法为待拖出路径创建系统外壳项：{clean}"
                )));
            }
            pidls.0.push(pidl);
        }

        let const_pidls: Vec<*const ITEMIDLIST> = pidls.0.iter().map(|&p| p as *const _).collect();
        let item_array = SHCreateShellItemArrayFromIDLists(&const_pidls)
            .map_err(|e| PlatformError::System(format!("构建拖拽文件列表失败：{e}")))?;
        drop(pidls);

        let data_obj: IDataObject = item_array
            .BindToHandler(None, &BHID_DataObject)
            .map_err(|e| PlatformError::System(format!("创建系统拖拽数据对象失败：{e}")))?;

        let effect = SHDoDragDrop(
            helper_hwnd.as_ref().map(|g| g.0),
            &data_obj,
            None::<&IDropSource>,
            DROPEFFECT_COPY | DROPEFFECT_MOVE,
        )
        .map_err(|e| PlatformError::System(format!("系统拖拽会话失败：{e}")))?;
        Ok(effect.0 != 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_capturable_desktop_only_allows_default_in_interactive_session() {
        let default_ok = DesktopEnvironment {
            session_id: 1,
            console_session_id: 1,
            elevated: false,
            input_desktop: Some("Default".to_string()),
        };
        assert!(ensure_capturable_desktop(&default_ok).is_ok());

        for blocked in [
            DesktopEnvironment {
                session_id: 0,
                console_session_id: 1,
                elevated: true,
                input_desktop: Some("Default".to_string()),
            },
            DesktopEnvironment {
                session_id: 1,
                console_session_id: 1,
                elevated: false,
                input_desktop: None,
            },
            DesktopEnvironment {
                session_id: 1,
                console_session_id: 1,
                elevated: false,
                input_desktop: Some("Winlogon".to_string()),
            },
            DesktopEnvironment {
                session_id: 1,
                console_session_id: 1,
                elevated: false,
                input_desktop: Some("Screen-saver".to_string()),
            },
        ] {
            assert!(
                matches!(
                    ensure_capturable_desktop(&blocked),
                    Err(PlatformError::PermissionDenied(_))
                ),
                "expected PermissionDenied for {blocked:?}"
            );
        }
    }

    #[test]
    fn dib_to_png_handles_v4_v5_bitfields_and_preserves_true_alpha() {
        // 构造 2x1 的 BITMAPV5HEADER (124 字节) + BI_BITFIELDS，含 1 个透明像素 (a=0) 与 1 个半透明像素 (a=128)
        let mut dib = vec![0u8; 124 + 8];
        let hdr = BITMAPINFOHEADER {
            biSize: 124,
            biWidth: 2,
            biHeight: -1, // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_BITFIELDS.0,
            biSizeImage: 8,
            ..Default::default()
        };
        unsafe {
            std::ptr::copy_nonoverlapping(
                &hdr as *const BITMAPINFOHEADER as *const u8,
                dib.as_mut_ptr(),
                40,
            );
        }
        // V4/V5 通道掩码位于头部内偏移 40..52（BGRA）
        dib[40..44].copy_from_slice(&0x00FF_0000u32.to_le_bytes());
        dib[44..48].copy_from_slice(&0x0000_FF00u32.to_le_bytes());
        dib[48..52].copy_from_slice(&0x0000_00FFu32.to_le_bytes());
        // 像素 0: BGRA(10, 20, 30, 0) 完全透明；像素 1: BGRA(40, 50, 60, 128) 半透明
        dib[124..132].copy_from_slice(&[10, 20, 30, 0, 40, 50, 60, 128]);

        let (png_bytes, w, h) = dib_to_png(&dib).expect("V5 BI_BITFIELDS DIB should convert");
        assert_eq!((w, h), (2, 1));
        let mut reader = png::Decoder::new(Cursor::new(&png_bytes))
            .read_info()
            .unwrap();
        let mut out = vec![0u8; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut out).unwrap();
        assert_eq!(&out[..info.buffer_size()], &[30, 20, 10, 0, 60, 50, 40, 128]);
    }

    #[test]
    fn dib_to_png_fills_alpha_only_when_entire_image_alpha_is_zero() {
        // 2x1 BITMAPINFOHEADER (40 字节) + BI_BITFIELDS (12 字节掩码) + 全 0 alpha
        let mut dib = vec![0u8; 40 + 12 + 8];
        let hdr = BITMAPINFOHEADER {
            biSize: 40,
            biWidth: 2,
            biHeight: -1,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_BITFIELDS.0,
            ..Default::default()
        };
        unsafe {
            std::ptr::copy_nonoverlapping(
                &hdr as *const BITMAPINFOHEADER as *const u8,
                dib.as_mut_ptr(),
                40,
            );
        }
        dib[40..44].copy_from_slice(&0x00FF_0000u32.to_le_bytes());
        dib[44..48].copy_from_slice(&0x0000_FF00u32.to_le_bytes());
        dib[48..52].copy_from_slice(&0x0000_00FFu32.to_le_bytes());
        dib[52..60].copy_from_slice(&[1, 2, 3, 0, 4, 5, 6, 0]);

        let (png_bytes, w, h) = dib_to_png(&dib).expect("V1 BI_BITFIELDS DIB should convert");
        assert_eq!((w, h), (2, 1));
        let mut reader = png::Decoder::new(Cursor::new(&png_bytes))
            .read_info()
            .unwrap();
        let mut out = vec![0u8; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut out).unwrap();
        assert_eq!(&out[..info.buffer_size()], &[3, 2, 1, 255, 6, 5, 4, 255]);
    }

    #[test]
    fn dib_to_png_rejects_unsupported_compression_and_png_to_dib_rejects_oversized() {
        let mut bad_dib = vec![0u8; 40 + 16];
        let hdr = BITMAPINFOHEADER {
            biSize: 40,
            biWidth: 2,
            biHeight: 2,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: 4, // BI_JPEG
            ..Default::default()
        };
        unsafe {
            std::ptr::copy_nonoverlapping(
                &hdr as *const BITMAPINFOHEADER as *const u8,
                bad_dib.as_mut_ptr(),
                40,
            );
        }
        assert!(matches!(
            dib_to_png(&bad_dib),
            Err(PlatformError::Unsupported(_))
        ));

        // 校验尺寸与总像素上限防护
        assert!(matches!(
            validate_clipboard_image_dims(MAX_CLIPBOARD_IMAGE_DIM + 1, 1),
            Err(PlatformError::InvalidInput(_))
        ));
        assert!(matches!(
            validate_clipboard_image_dims(8192, 8192),
            Err(PlatformError::InvalidInput(_))
        ));
        assert!(validate_clipboard_image_dims(7680, 4320).is_ok());
    }

    #[test]
    fn combo_inputs_set_extended_flag_and_compute_stuck_keys_on_partial_send() {
        let vks = parse_key_combo("Ctrl+Shift+Delete").expect("parse combo");
        let inputs = build_combo_inputs(&vks);
        assert_eq!(inputs.len(), 6);
        // Delete 是扩展键，按下和抬起都必须带 KEYEVENTF_EXTENDEDKEY
        let del_down = unsafe { inputs[2].Anonymous.ki };
        let del_up = unsafe { inputs[3].Anonymous.ki };
        assert_eq!(del_down.wVk, VK_DELETE);
        assert_ne!(del_down.dwFlags.0 & KEYEVENTF_EXTENDEDKEY.0, 0);
        assert_ne!(del_up.dwFlags.0 & KEYEVENTF_EXTENDEDKEY.0, 0);
        assert_ne!(del_up.dwFlags.0 & KEYEVENTF_KEYUP.0, 0);

        // 若 SendInput 只发出前 2 项（Ctrl down, Shift down），应按逆序补释放 [Shift, Ctrl]
        let stuck = compute_stuck_keys_to_release(&inputs, 2);
        assert_eq!(stuck, vec![VK_SHIFT, VK_CONTROL]);
        // 若发出前 4 项（Ctrl/Shift/Del down + Del up），剩 [Shift, Ctrl]
        assert_eq!(
            compute_stuck_keys_to_release(&inputs, 4),
            vec![VK_SHIFT, VK_CONTROL]
        );
        // 全部发出则无卡键
        assert!(compute_stuck_keys_to_release(&inputs, 6).is_empty());
    }
}
