use super::*;

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

pub(super) fn build_combo_inputs(vks: &[VIRTUAL_KEY]) -> Vec<INPUT> {
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
pub(super) fn compute_stuck_keys_to_release(inputs: &[INPUT], sent: usize) -> Vec<VIRTUAL_KEY> {
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

pub(super) fn parse_key_combo(combo: &str) -> Result<Vec<VIRTUAL_KEY>, PlatformError> {
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

