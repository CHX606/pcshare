use super::*;

pub(super) struct LaunchArgs {
    pub(super) data_dir: PathBuf,
    pub(super) network_mode: NetworkMode,
    pub(super) background: bool,
}

pub(super) fn parse_launch_args() -> LaunchArgs {
    let mut data_dir = default_data_dir();
    let mut network_mode = NetworkMode::PublicOrCustom;
    let mut background = false;
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--data-dir" => {
                if let Some(val) = iter.next() {
                    data_dir = PathBuf::from(val);
                }
            }
            "--local" => {
                network_mode = NetworkMode::LocalTest;
            }
            "--background" => {
                background = true;
            }
            _ => {}
        }
    }
    LaunchArgs {
        data_dir,
        network_mode,
        background,
    }
}

/// 打开或聚焦主窗口（后台常驻无窗口时会按需创建 WebView，关窗时销毁 WebView 以确保常驻内存 < 50 MB）。
pub(super) fn create_or_focus_main_window(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.unminimize();
        let _ = win.show();
        let _ = win.set_focus();
        return;
    }
    let builder = WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
        .title("电脑共享")
        .inner_size(1120.0, 740.0)
        .min_inner_size(820.0, 540.0)
        .center();
    #[cfg(debug_assertions)]
    let builder = match std::env::var("PCSHARE_DEV_BROWSER_ARGS") {
        Ok(extra) if !extra.trim().is_empty() => builder.additional_browser_args(&extra),
        _ => builder,
    };
    let _ = builder.build();
}

fn fallback_tray_icon() -> Image<'static> {
    let mut rgba = Vec::with_capacity(16 * 16 * 4);
    for _ in 0..(16 * 16) {
        rgba.extend_from_slice(&[37, 99, 235, 255]);
    }
    Image::new_owned(rgba, 16, 16)
}

pub(super) fn setup_system_tray(
    app: &AppHandle,
    sink: &Arc<TauriEventSink>,
    quitting: Arc<AtomicBool>,
) -> tauri::Result<()> {
    let menu = tray_menu(app, sink)?;
    let icon = app
        .default_window_icon()
        .map(|i| Image::new_owned(i.rgba().to_vec(), i.width(), i.height()))
        .unwrap_or_else(fallback_tray_icon);

    TrayIconBuilder::with_id("pcshare-tray")
        .icon(icon)
        .tooltip("电脑共享 — 后台运行中")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(move |app, event| tray_menu_action(app, event.id().as_ref(), &quitting))
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                create_or_focus_main_window(tray.app_handle());
            }
        })
        .build(app)?;

    Ok(())
}

fn tray_menu_action(app_handle: &AppHandle, id: &str, quitting: &AtomicBool) {
    match id {
        "open" => {
            create_or_focus_main_window(app_handle);
        }
        "pause_host" => {
            if let Some(bridge) = app_handle.try_state::<ManagedBridge>() {
                let next = !bridge.node().is_host_paused();
                let _ = bridge.host_set_paused(next);
            }
        }
        "quit" => {
            quitting.store(true, Ordering::SeqCst);
            app_handle.exit(0);
        }
        _ => {}
    }
}

fn tray_menu(app: &AppHandle, sink: &TauriEventSink) -> tauri::Result<Menu<Wry>> {
    let open_item = MenuItem::with_id(app, "open", "打开主窗口", true, None::<&str>)?;
    let pause_item =
        CheckMenuItem::with_id(app, "pause_host", "暂停被控", true, false, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;
    let quit_item = MenuItem::with_id(app, "quit", "退出程序", true, None::<&str>)?;

    if let Ok(mut guard) = sink.pause_menu_item.lock() {
        *guard = Some(pause_item.clone());
    }

    Menu::with_items(app, &[&open_item, &pause_item, &sep, &quit_item])
}
