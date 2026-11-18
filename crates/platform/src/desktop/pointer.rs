use super::{ButtonState, PointerAction};
use crate::{PlatformError, PlatformMouseBtn as Button};
use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

pub fn inject(action: PointerAction, target: Option<usize>) -> Result<(), PlatformError> {
    match action {
        PointerAction::Move(x, y) | PointerAction::Wheel(x, y, ..) => {
            verify_target(target, Some((x, y)), false)?
        }
        PointerAction::Button(x, y, _, down) => verify_target(target, Some((x, y)), down)?,
        PointerAction::Key(_, down) => verify_target(target, None, down)?,
        PointerAction::Release(_) => {}
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
    if focus && unsafe { GetForegroundWindow() } != hwnd {
        let _ = unsafe { SetForegroundWindow(hwnd) };
    }
    let hit = match point {
        Some((x, y)) => unsafe { WindowFromPoint(POINT { x, y }) },
        None => unsafe { GetForegroundWindow() },
    };
    if unsafe { GetAncestor(hit, GA_ROOT) } != hwnd {
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
    let axis = |point: i32, origin, size| unsafe {
        let start = i64::from(GetSystemMetrics(origin));
        let span = i64::from(GetSystemMetrics(size)).max(1);
        (((i64::from(point) - start) * 65536 + 32768) / span).clamp(0, 65535) as i32
    };
    mouse(
        MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
        axis(x, SM_XVIRTUALSCREEN, SM_CXVIRTUALSCREEN),
        axis(y, SM_YVIRTUALSCREEN, SM_CYVIRTUALSCREEN),
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
    if matches!(vk, 0x21..=0x28 | 0x2c..=0x2e | 0x5b..=0x5d | 0x6f | 0x90 | 0xa3 | 0xa5) {
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
