use super::{CaptureArea, CaptureSource};
use crate::{PlatformError, PlatformMonitor};
use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Graphics::Gdi::{ClientToScreen, MONITOR_DEFAULTTONULL, MonitorFromPoint};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{GetClientRect, IsIconic, IsWindowVisible};

pub fn capture_area(
    source: CaptureSource,
    monitors: &[PlatformMonitor],
) -> Result<CaptureArea, PlatformError> {
    match source {
        CaptureSource::Window(raw) => window(raw),
        CaptureSource::Monitor(index) => monitor(index, monitors),
    }
}

fn monitor(index: u32, monitors: &[PlatformMonitor]) -> Result<CaptureArea, PlatformError> {
    let m = monitors
        .iter()
        .find(|m| m.index == index)
        .ok_or_else(|| PlatformError::NotFound("显示器已断开".into()))?;
    let pt = POINT {
        x: m.origin_x + (m.width / 2) as i32,
        y: m.origin_y + (m.height / 2) as i32,
    };
    let handle = unsafe { MonitorFromPoint(pt, MONITOR_DEFAULTTONULL) };
    if handle.0.is_null() {
        return Err(PlatformError::NotFound("显示器已断开".into()));
    }
    Ok(CaptureArea {
        x: m.origin_x,
        y: m.origin_y,
        width: m.width,
        height: m.height,
        dpi: m.dpi_scale_percent,
        handle: handle.0 as usize,
    })
}

fn window(raw: usize) -> Result<CaptureArea, PlatformError> {
    let hwnd = HWND(raw as _);
    let mut rect = RECT::default();
    let mut pt = POINT::default();
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
            return Err(PlatformError::NotFound("窗口已关闭、隐藏或最小化".into()));
        }
        GetClientRect(hwnd, &mut rect).map_err(|e| PlatformError::System(e.to_string()))?;
        if !ClientToScreen(hwnd, &mut pt).as_bool() || rect.right < 2 || rect.bottom < 2 {
            return Err(PlatformError::InvalidInput("窗口没有可显示的客户区".into()));
        }
        Ok(CaptureArea {
            x: pt.x,
            y: pt.y,
            width: rect.right as u32,
            height: rect.bottom as u32,
            dpi: (GetDpiForWindow(hwnd) * 100 + 48) / 96,
            handle: raw,
        })
    }
}
