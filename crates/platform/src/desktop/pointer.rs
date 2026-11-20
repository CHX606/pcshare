use super::{ButtonState, PointerAction};
use crate::win_util::{
    check_window_uipi_warning, is_extended_vk, virtual_screen_to_absolute, window_belongs_to_target,
};
use crate::{PlatformError, PlatformMouseBtn as Button};
use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

pub fn inject(action: PointerAction, target: Option<usize>) -> Result<(), PlatformError> {
    // 若指定单窗口目标且正在执行主动输入，先在窗口有效时检测 UIPI，
    // 避免管理员窗口导致 SetForegroundWindow 失败后误报成“失去焦点”。
    if matches!(
        action,
        PointerAction::Button(_, _, _, true)
            | PointerAction::Wheel(..)
            | PointerAction::Key(_, true)
    ) && let Some(raw) = target
    {
        let hwnd = HWND(raw as _);
        if unsafe { IsWindow(Some(hwnd)) }.as_bool()
            && let Some(warning) = check_window_uipi_warning(hwnd)
        {
            return Err(PlatformError::PermissionDenied(warning));
        }
    }

    match action {
        PointerAction::Move(x, y) | PointerAction::Wheel(x, y, ..) => {
            verify_target(target, Some((x, y)), false)?
        }
        PointerAction::Button(x, y, _, down) => verify_target(target, Some((x, y)), down)?,
        PointerAction::Key(_, down) => verify_target(target, None, down)?,
        PointerAction::Release(_) => {}
    }

    let uipi_hwnd = match action {
        PointerAction::Button(x, y, _, true) | PointerAction::Wheel(x, y, ..) => {
            let hit = unsafe { WindowFromPoint(POINT { x, y }) };
            if !hit.0.is_null() {
                Some(hit)
            } else {
                target.map(|raw| HWND(raw as _))
            }
        }
        PointerAction::Key(_, true) => {
            let fg = unsafe { GetForegroundWindow() };
            if !fg.0.is_null() {
                Some(fg)
            } else {
                target.map(|raw| HWND(raw as _))
            }
        }
        _ => None,
    };
    if let Some(hwnd) = uipi_hwnd
        && let Some(warning) = check_window_uipi_warning(hwnd)
    {
        return Err(PlatformError::PermissionDenied(warning));
    }

    let inputs = match action {
        PointerAction::Move(x, y) => vec![position(x, y)],
        PointerAction::Button(x, y, b, down) => vec![position(x, y), button(b, down)],
        PointerAction::Wheel(x, y, dx, dy) => wheel(x, y, dx, dy),
        PointerAction::Key(vk, down) => vec![key(vk, down)],
        PointerAction::Release(ButtonState::Mouse(b)) => vec![button(b, false)],
        PointerAction::Release(ButtonState::Key(vk)) => vec![key(vk, false)],
    };
    let sent = unsafe { SendInput(&inputs, size_of::<INPUT>() as i32) };
    if sent as usize != inputs.len() {
        return Err(PlatformError::PermissionDenied(
            "Windows 拦截了输入，目标可能处于安全桌面或使用了更高权限".into(),
        ));
    }
    Ok(())
}

pub fn verify_target(
    target: Option<usize>,
    point: Option<(i32, i32)>,
    focus: bool,
) -> Result<(), PlatformError> {
    let Some(raw) = target else { return Ok(()) };
    let hwnd = HWND(raw as _);
    if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        return Err(PlatformError::NotFound("目标窗口已关闭".into()));
    }
    if focus {
        let fg = unsafe { GetForegroundWindow() };
        if !window_belongs_to_target(fg, hwnd) {
            let _ = unsafe { SetForegroundWindow(hwnd) };
        }
    }
    let hit = match point {
        Some((x, y)) => unsafe { WindowFromPoint(POINT { x, y }) },
        None => unsafe { GetForegroundWindow() },
    };
    if !window_belongs_to_target(hit, hwnd) {
        return Err(PlatformError::PermissionDenied(
            "目标窗口已失去焦点或被遮挡，请先切换到该窗口".into(),
        ));
    }
    Ok(())
}

fn mouse(flags: MOUSE_EVENT_FLAGS, dx: i32, dy: i32, data: i32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data as u32,
                dwFlags: flags,
                ..Default::default()
            },
        },
    }
}

fn position(x: i32, y: i32) -> INPUT {
    let (dx, dy) = virtual_screen_to_absolute(x, y);
    mouse(
        MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
        dx,
        dy,
        0,
    )
}

fn button(b: Button, down: bool) -> INPUT {
    let flags = match b {
        Button::Left => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
        Button::Right => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
        Button::Middle => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
    };
    mouse(if down { flags.0 } else { flags.1 }, 0, 0, 0)
}

fn wheel(x: i32, y: i32, dx: i16, dy: i16) -> Vec<INPUT> {
    let mut batch = vec![position(x, y)];
    if dy != 0 {
        batch.push(mouse(MOUSEEVENTF_WHEEL, 0, 0, i32::from(dy)));
    }
    if dx != 0 {
        batch.push(mouse(MOUSEEVENTF_HWHEEL, 0, 0, i32::from(dx)));
    }
    batch
}

fn key(vk: u8, down: bool) -> INPUT {
    let mut flags = if down {
        KEYBD_EVENT_FLAGS(0)
    } else {
        KEYEVENTF_KEYUP
    };
    if is_extended_vk(vk.into()) {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk.into()),
                dwFlags: flags,
                ..Default::default()
            },
        },
    }
}
