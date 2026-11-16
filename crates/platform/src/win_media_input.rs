use std::ffi::{OsStr, c_void};
use std::io::Cursor;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;

use windows::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, GetLastError, GlobalFree, HANDLE, HGLOBAL, HWND, LPARAM,
    POINT, RECT,
};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, EnumDisplayMonitors, GetDC, GetDIBits, GetMonitorInfoW,
    HDC, HGDIOBJ, HMONITOR, MONITORINFOEXW, ReleaseDC, SRCCOPY, SelectObject,
};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_ContainerFormatJpeg, GUID_WICPixelFormat24bppBGR,
    GUID_WICPixelFormat32bppBGRA, IWICBitmapFrameEncode, IWICBitmapSource, IWICImagingFactory,
    WICBitmapDitherTypeNone, WICBitmapEncoderNoCache, WICBitmapInterpolationModeFant,
    WICBitmapPaletteTypeCustom,
};
use windows::Win32::Security::{
    GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
};
use windows::Win32::Storage::Xps::{PRINT_WINDOW_FLAGS, PrintWindow};
use windows::Win32::System::Com::StructuredStorage::PROPBAG2;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoCreateInstance,
    CoInitializeEx, CoUninitialize, IDataObject, STREAM_SEEK_SET,
};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber,
    IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows::Win32::System::Ole::{
    CF_DIB, CF_UNICODETEXT, DROPEFFECT_COPY, DROPEFFECT_MOVE, IDropSource, OleInitialize,
    OleUninitialize,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, OpenProcess, OpenProcessToken,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::System::Variant::{VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_R4};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE,
    MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
    MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_VIRTUALDESK, MOUSEINPUT, SendInput, VIRTUAL_KEY, VK_BACK, VK_CONTROL, VK_DELETE,
    VK_DOWN, VK_END, VK_ESCAPE, VK_F1, VK_HOME, VK_LEFT, VK_LMENU, VK_LWIN, VK_NEXT, VK_PRIOR,
    VK_RETURN, VK_RIGHT, VK_SHIFT, VK_SPACE, VK_TAB, VK_UP,
};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    BHID_DataObject, ILCreateFromPathW, ILFree, SHCreateMemStream,
    SHCreateShellItemArrayFromIDLists, SHDoDragDrop,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GA_ROOT, GetAncestor, GetForegroundWindow, GetSystemMetrics, GetWindowRect,
    GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindow,
    IsWindowVisible, MONITORINFOF_PRIMARY, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SetForegroundWindow, WindowFromPoint,
};
use windows::core::{BOOL, Interface, PCWSTR, PWSTR};

use crate::{
    CapturableWindowInfo, CapturedImage, ClipboardContent, ClipboardData, DesktopEnvironment,
    InputOutcome, PlatformError, PlatformMonitor, PlatformMouseBtn,
};

const PW_RENDERFULLCONTENT_FLAG: u32 = 0x0000_0002;

fn wide_null(s: &str) -> Vec<u16> {
    OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn self_is_elevated() -> bool {
    unsafe {
        let mut token = Default::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut ret_len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut TOKEN_ELEVATION as *mut c_void),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut ret_len,
        );
        let _ = CloseHandle(token);
        ok.is_ok() && elevation.TokenIsElevated != 0
    }
}

pub fn list_monitors_impl() -> Result<Vec<PlatformMonitor>, PlatformError> {
    let mut raw_list: Vec<PlatformMonitor> = Vec::new();
    unsafe extern "system" fn enum_monitor_cb(
        hmon: HMONITOR,
        _hdc: HDC,
        _rect: *mut RECT,
        lparam: LPARAM,
    ) -> BOOL {
        let out = unsafe { &mut *(lparam.0 as *mut Vec<PlatformMonitor>) };
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if unsafe { GetMonitorInfoW(hmon, &mut info.monitorInfo) }.as_bool() {
            let rc = info.monitorInfo.rcMonitor;
            let width = (rc.right - rc.left).max(0) as u32;
            let height = (rc.bottom - rc.top).max(0) as u32;
            if width > 0 && height > 0 {
                let mut dpi_x = 96u32;
                let mut dpi_y = 96u32;
                let _ = unsafe { GetDpiForMonitor(hmon, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) };
                let scale_pct = ((dpi_x * 100 + 48) / 96).max(100);
                let name_len = info
                    .szDevice
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(info.szDevice.len());
                let raw_name = String::from_utf16_lossy(&info.szDevice[..name_len]);
                let is_primary = (info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY) != 0;
                let idx = out.len() as u32;
                let display_name = if is_primary {
                    format!("主显示器 #{}（{}×{}）", idx + 1, width, height)
                } else {
                    format!("显示器 #{}（{}×{} {}）", idx + 1, width, height, raw_name)
                };
                out.push(PlatformMonitor {
                    index: idx,
                    name: display_name,
                    origin_x: rc.left,
                    origin_y: rc.top,
                    width,
                    height,
                    dpi_scale_percent: scale_pct,
                    is_primary,
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
            LPARAM(&mut raw_list as *mut Vec<PlatformMonitor> as isize),
        );
    }
    // Primary monitor first if present, re-indexing cleanly
    raw_list.sort_by_key(|m| (!m.is_primary, m.index));
    for (idx, m) in raw_list.iter_mut().enumerate() {
        m.index = idx as u32;
    }
    Ok(raw_list)
}

pub fn list_capturable_windows_impl() -> Vec<CapturableWindowInfo> {
    let mut list = Vec::new();
    unsafe extern "system" fn collect_cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let out = unsafe { &mut *(lparam.0 as *mut Vec<CapturableWindowInfo>) };
        unsafe {
            if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
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
    if desktop
        .input_desktop
        .as_deref()
        .is_some_and(|name| name.eq_ignore_ascii_case("Winlogon"))
    {
        return Err(PlatformError::PermissionDenied(
            "对方电脑当前已锁屏或停留在系统登录/权限确认界面，无法截取画面".to_string(),
        ));
    }
    Ok(())
}

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

    let bgra = capture_dc_rect_bgra(mon.origin_x, mon.origin_y, mon.width, mon.height, desktop)?;
    let (jpeg_bytes, img_w, img_h) =
        encode_bgra_to_jpeg(&bgra, mon.width, mon.height, max_width, quality)?;

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
        let mem_dc = CreateCompatibleDC(Some(screen_dc));
        let bmp = CreateCompatibleBitmap(screen_dc, w as i32, h as i32);
        if mem_dc.is_invalid() || bmp.is_invalid() {
            if !bmp.is_invalid() {
                let _ = DeleteObject(HGDIOBJ(bmp.0));
            }
            if !mem_dc.is_invalid() {
                let _ = DeleteDC(mem_dc);
            }
            let _ = ReleaseDC(None, screen_dc);
            return Err(PlatformError::System("创建截图位图缓冲区失败".to_string()));
        }
        let old = SelectObject(mem_dc, HGDIOBJ(bmp.0));
        let mut captured = PrintWindow(hwnd, mem_dc, PRINT_WINDOW_FLAGS(PW_RENDERFULLCONTENT_FLAG))
            .as_bool();
        if !captured {
            captured = BitBlt(
                mem_dc,
                0,
                0,
                w as i32,
                h as i32,
                Some(screen_dc),
                rect.left,
                rect.top,
                SRCCOPY,
            )
            .is_ok();
        }

        let bgra_res = if captured {
            extract_bgra_top_down(mem_dc, bmp, w, h)
        } else {
            Err(PlatformError::PermissionDenied(
                "无法截取该窗口画面（可能处于锁屏/安全桌面，或受窗口保护限制）".to_string(),
            ))
        };

        let _ = SelectObject(mem_dc, old);
        let _ = DeleteObject(HGDIOBJ(bmp.0));
        let _ = DeleteDC(mem_dc);
        let _ = ReleaseDC(None, screen_dc);

        let bgra = bgra_res?;
        let (jpeg_bytes, img_w, img_h) = encode_bgra_to_jpeg(&bgra, w, h, max_width, quality)?;
        Ok(CapturedImage {
            jpeg_bytes,
            origin_x: rect.left,
            origin_y: rect.top,
            source_width: w,
            source_height: h,
            image_width: img_w,
            image_height: img_h,
            dpi_scale_percent: 100,
            target_hwnd: Some(hwnd_raw),
            monitor_index: None,
        })
    }
}

fn capture_dc_rect_bgra(
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    desktop: &DesktopEnvironment,
) -> Result<Vec<u8>, PlatformError> {
    unsafe {
        let screen_dc = GetDC(None);
        if screen_dc.is_invalid() {
            return Err(PlatformError::PermissionDenied(
                "对方电脑当前处于锁屏、无显示器或权限确认界面，无法获取桌面画面".to_string(),
            ));
        }
        let mem_dc = CreateCompatibleDC(Some(screen_dc));
        let bmp = CreateCompatibleBitmap(screen_dc, w as i32, h as i32);
        if mem_dc.is_invalid() || bmp.is_invalid() {
            if !bmp.is_invalid() {
                let _ = DeleteObject(HGDIOBJ(bmp.0));
            }
            if !mem_dc.is_invalid() {
                let _ = DeleteDC(mem_dc);
            }
            let _ = ReleaseDC(None, screen_dc);
            return Err(PlatformError::System("创建屏幕位图缓冲区失败".to_string()));
        }
        let old = SelectObject(mem_dc, HGDIOBJ(bmp.0));
        let blt_ok = BitBlt(
            mem_dc,
            0,
            0,
            w as i32,
            h as i32,
            Some(screen_dc),
            x,
            y,
            SRCCOPY,
        )
        .is_ok();

        let res = if blt_ok || composite_desktop_windows_fallback(screen_dc, mem_dc, x, y, w, h) {
            extract_bgra_top_down(mem_dc, bmp, w, h)
        } else if !desktop.is_interactive() {
            Err(PlatformError::PermissionDenied(
                "对方电脑当前处于锁屏、无活动显示器或权限确认界面，无法截取桌面画面".to_string(),
            ))
        } else {
            Err(PlatformError::PermissionDenied(
                "系统当前拒绝截取全屏画面（可能处于锁屏、安全桌面或远程会话已断开）".to_string(),
            ))
        };

        let _ = SelectObject(mem_dc, old);
        let _ = DeleteObject(HGDIOBJ(bmp.0));
        let _ = DeleteDC(mem_dc);
        let _ = ReleaseDC(None, screen_dc);
        res
    }
}

unsafe fn extract_bgra_top_down(
    dc: HDC,
    bmp: windows::Win32::Graphics::Gdi::HBITMAP,
    w: u32,
    h: u32,
) -> Result<Vec<u8>, PlatformError> {
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
    let mut pixels = vec![0u8; (w as usize) * (h as usize) * 4];
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
) -> bool {
    let windows = list_capturable_windows_impl();
    if windows.is_empty() {
        return false;
    }
    let mut painted_any = false;
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
            || rc.left >= origin_x + (width as i32)
            || rc.top >= origin_y + (height as i32)
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
                painted_any = true;
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
    painted_any
}

fn encode_bgra_to_jpeg(
    bgra: &[u8],
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

    let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
    let res = unsafe {
        encode_bgra_to_jpeg_com(bgra, src_w, src_h, dst_w, dst_h, quality)
    };
    if hr.is_ok() {
        unsafe { CoUninitialize() };
    }
    res.map(|bytes| (bytes, dst_w, dst_h))
}

unsafe fn encode_bgra_to_jpeg_com(
    bgra: &[u8],
    src_w: u32,
    src_h: u32,
    dst_w: u32,
    dst_h: u32,
    quality: u8,
) -> Result<Vec<u8>, PlatformError> {
    let factory: IWICImagingFactory =
        unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER) }
            .map_err(|e| PlatformError::System(format!("初始化系统图像编码器失败：{e}")))?;

    let bitmap = unsafe {
        factory.CreateBitmapFromMemory(
            src_w,
            src_h,
            &GUID_WICPixelFormat32bppBGRA,
            src_w * 4,
            bgra,
        )
    }
    .map_err(|e| PlatformError::System(format!("创建 WIC 图像对象失败：{e}")))?;

    let scaled_source: IWICBitmapSource = if dst_w != src_w || dst_h != src_h {
        let scaler = unsafe { factory.CreateBitmapScaler() }
            .map_err(|e| PlatformError::System(format!("创建图像缩放器失败：{e}")))?;
        unsafe {
            scaler.Initialize(
                &bitmap,
                dst_w,
                dst_h,
                WICBitmapInterpolationModeFant,
            )
        }
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
        if hr.is_err() || read_bytes == 0 {
            break;
        }
        out.extend_from_slice(&chunk[..read_bytes as usize]);
    }
    Ok(out)
}

// ──────────────────────────── 输入注入与 UIPI 检测（任务 3.3）────────────────────────────

fn check_window_uipi_warning(hwnd: HWND) -> Option<String> {
    if hwnd.0.is_null() || self_is_elevated() {
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
            Ok(proc) => {
                let mut tok = HANDLE::default();
                let mut target_elevated = false;
                if OpenProcessToken(proc, TOKEN_QUERY, &mut tok).is_ok() {
                    let mut elev = TOKEN_ELEVATION::default();
                    let mut ret_len = 0u32;
                    if GetTokenInformation(
                        tok,
                        TokenElevation,
                        Some(&mut elev as *mut TOKEN_ELEVATION as *mut c_void),
                        std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                        &mut ret_len,
                    )
                    .is_ok()
                    {
                        target_elevated = elev.TokenIsElevated != 0;
                    }
                    let _ = CloseHandle(tok);
                }
                let _ = CloseHandle(proc);
                if target_elevated {
                    Some(uipi_msg.to_string())
                } else {
                    None
                }
            }
            Err(_) => {
                let last = GetLastError();
                if last == ERROR_ACCESS_DENIED {
                    Some(uipi_msg.to_string())
                } else {
                    None
                }
            }
        }
    }
}

pub fn send_mouse_click_impl(
    screen_x: i32,
    screen_y: i32,
    button: PlatformMouseBtn,
    double: bool,
    target_hwnd: Option<usize>,
) -> Result<InputOutcome, PlatformError> {
    unsafe {
        let hit_hwnd = if let Some(raw) = target_hwnd {
            let h = HWND(raw as *mut c_void);
            if IsWindow(Some(h)).as_bool() {
                let _ = SetForegroundWindow(h);
                h
            } else {
                WindowFromPoint(POINT {
                    x: screen_x,
                    y: screen_y,
                })
            }
        } else {
            WindowFromPoint(POINT {
                x: screen_x,
                y: screen_y,
            })
        };

        if let Some(warning) = check_window_uipi_warning(hit_hwnd) {
            return Ok(InputOutcome {
                uipi_blocked: true,
                warning: Some(warning),
            });
        }

        let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN).max(2);
        let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN).max(2);
        let dx = (((screen_x - vx) as i64 * 65535) / ((vw - 1) as i64)).clamp(0, 65535) as i32;
        let dy = (((screen_y - vy) as i64 * 65535) / ((vh - 1) as i64)).clamp(0, 65535) as i32;

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

        let sent = SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
        if sent == 0 {
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

pub fn send_key_or_text_impl(
    text: Option<&str>,
    key_combo: Option<&str>,
    target_hwnd: Option<usize>,
) -> Result<InputOutcome, PlatformError> {
    unsafe {
        let focus_hwnd = if let Some(raw) = target_hwnd {
            let h = HWND(raw as *mut c_void);
            if IsWindow(Some(h)).as_bool() {
                let _ = SetForegroundWindow(h);
                h
            } else {
                GetForegroundWindow()
            }
        } else {
            GetForegroundWindow()
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
            for &vk in &vks {
                inputs.push(INPUT {
                    r#type: INPUT_KEYBOARD,
                    Anonymous: INPUT_0 {
                        ki: KEYBDINPUT {
                            wVk: vk,
                            wScan: 0,
                            dwFlags: Default::default(),
                            time: 0,
                            dwExtraInfo: 0,
                        },
                    },
                });
            }
            for &vk in vks.iter().rev() {
                inputs.push(INPUT {
                    r#type: INPUT_KEYBOARD,
                    Anonymous: INPUT_0 {
                        ki: KEYBDINPUT {
                            wVk: vk,
                            wScan: 0,
                            dwFlags: KEYEVENTF_KEYUP,
                            time: 0,
                            dwExtraInfo: 0,
                        },
                    },
                });
            }
        }

        if inputs.is_empty() {
            return Err(PlatformError::InvalidInput(
                "未提供要输入的文字或按键组合".to_string(),
            ));
        }

        let sent = SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
        if sent == 0 {
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
            "alt" | "menu" => VK_LMENU,
            "shift" => VK_SHIFT,
            "win" | "meta" | "super" => VK_LWIN,
            "enter" | "return" => VK_RETURN,
            "esc" | "escape" => VK_ESCAPE,
            "tab" => VK_TAB,
            "space" => VK_SPACE,
            "backspace" | "bksp" => VK_BACK,
            "del" | "delete" => VK_DELETE,
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

    if has_png && !has_text
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

fn dib_to_png(dib: &[u8]) -> Result<(Vec<u8>, u32, u32), PlatformError> {
    if dib.len() < std::mem::size_of::<BITMAPINFOHEADER>() {
        return Err(PlatformError::InvalidInput("剪贴板位图数据长度不足".to_string()));
    }
    let hdr: BITMAPINFOHEADER =
        unsafe { std::ptr::read_unaligned(dib.as_ptr() as *const BITMAPINFOHEADER) };
    let width = hdr.biWidth.unsigned_abs();
    let top_down = hdr.biHeight < 0;
    let height = hdr.biHeight.unsigned_abs();
    let bpp = hdr.biBitCount as usize;
    if width == 0 || height == 0 || (bpp != 24 && bpp != 32) {
        return Err(PlatformError::Unsupported(format!(
            "暂不支持 {} 位深度的特殊剪贴板位图格式",
            hdr.biBitCount
        )));
    }
    let header_offset = (hdr.biSize as usize)
        + if hdr.biCompression == 3 { 12 } else { 0 }
        + (hdr.biClrUsed as usize) * 4;
    let row_stride = (width as usize * bpp).div_ceil(32) * 4;
    let needed = header_offset + row_stride * (height as usize);
    if dib.len() < needed {
        return Err(PlatformError::InvalidInput("剪贴板位图像素数据不完整".to_string()));
    }

    let mut rgba = vec![0u8; (width as usize) * (height as usize) * 4];
    for y in 0..(height as usize) {
        let src_y = if top_down { y } else { (height as usize) - 1 - y };
        let src_row = &dib[header_offset + src_y * row_stride..];
        let dst_row = &mut rgba[y * (width as usize) * 4..(y + 1) * (width as usize) * 4];
        for x in 0..(width as usize) {
            if bpp == 32 {
                let b = src_row[x * 4];
                let g = src_row[x * 4 + 1];
                let r = src_row[x * 4 + 2];
                let a = src_row[x * 4 + 3];
                dst_row[x * 4] = r;
                dst_row[x * 4 + 1] = g;
                dst_row[x * 4 + 2] = b;
                dst_row[x * 4 + 3] = if a == 0 { 255 } else { a };
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
        EmptyClipboard()
            .map_err(|e| PlatformError::System(format!("清空剪贴板失败：{e}")))?;
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
        EmptyClipboard()
            .map_err(|e| PlatformError::System(format!("清空剪贴板失败：{e}")))?;
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
    let mut decoder = png::Decoder::new(Cursor::new(png_bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder
        .read_info()
        .map_err(|e| PlatformError::InvalidInput(format!("解析剪贴板 PNG 图片失败：{e}")))?;
    let buf_size = reader
        .output_buffer_size()
        .ok_or_else(|| PlatformError::InvalidInput("剪贴板 PNG 图像尺寸过大".to_string()))?;
    let mut buf = vec![0u8; buf_size];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|e| PlatformError::InvalidInput(format!("解码剪贴板 PNG 帧失败：{e}")))?;
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
        biSizeImage: width * height * 4,
        ..Default::default()
    };

    let hdr_size = std::mem::size_of::<BITMAPINFOHEADER>();
    let mut out = vec![0u8; hdr_size + (width as usize) * (height as usize) * 4];
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
        let dst_row = &mut pixels[dst_y * (width as usize) * 4..(dst_y + 1) * (width as usize) * 4];
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
    let ole_hr = unsafe { OleInitialize(None) };
    let res = unsafe {
        let mut pidls: Vec<*mut ITEMIDLIST> = Vec::with_capacity(paths.len());
        for p in &paths {
            let wide = wide_null(&p.to_string_lossy());
            let pidl = ILCreateFromPathW(PCWSTR(wide.as_ptr()));
            if !pidl.is_null() {
                pidls.push(pidl);
            }
        }
        if pidls.is_empty() {
            return Err(PlatformError::NotFound(
                "找不到准备拖出的本地临时文件".to_string(),
            ));
        }
        let const_pidls: Vec<*const ITEMIDLIST> = pidls.iter().map(|&p| p as *const _).collect();
        let arr_res = SHCreateShellItemArrayFromIDLists(&const_pidls);
        for p in pidls {
            ILFree(Some(p as *const _));
        }
        let item_array =
            arr_res.map_err(|e| PlatformError::System(format!("构建拖拽文件列表失败：{e}")))?;
        let data_obj: IDataObject = item_array
            .BindToHandler(None, &BHID_DataObject)
            .map_err(|e| PlatformError::System(format!("创建系统拖拽数据对象失败：{e}")))?;

        let effect = SHDoDragDrop(
            None,
            &data_obj,
            None::<&IDropSource>,
            DROPEFFECT_COPY | DROPEFFECT_MOVE,
        )
        .map_err(|e| PlatformError::System(format!("系统拖拽会话失败：{e}")))?;
        Ok(effect.0 != 0)
    };
    if ole_hr.is_ok() {
        unsafe { OleUninitialize() };
    }
    res
}
