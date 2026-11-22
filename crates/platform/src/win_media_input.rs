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

pub(super) struct PidlList(pub(super) Vec<*mut ITEMIDLIST>);
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


mod win_clipboard;
mod win_input;

pub use win_clipboard::*;
pub use win_input::*;
