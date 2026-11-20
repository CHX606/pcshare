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

/// 必须在主线程调用：打开或聚焦主窗口（关窗销毁 WebView2 以确保后台常驻内存 < 50 MB）。
/// 返回 `true` 表示主窗口原本不存在、本次为冷启新建；返回 `false` 表示主窗口原本已存在（仅恢复聚焦）或建窗失败。
pub(super) fn ensure_main_window_on_main_thread(app: &AppHandle) -> bool {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.unminimize();
        let _ = win.show();
        let _ = win.set_focus();
        return false;
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
    match builder.build() {
        Ok(win) => {
            let app_for_destroy = app.clone();
            win.on_window_event(move |event| {
                if let tauri::WindowEvent::Destroyed = event
                    && let Some(bridge) = app_for_destroy.try_state::<ManagedBridge>()
                {
                    let b = bridge.inner().clone();
                    tauri::async_runtime::spawn(async move {
                        b.on_main_window_destroyed().await;
                    });
                }
            });
            true
        }
        Err(err) => {
            if let Some(bridge) = app.try_state::<ManagedBridge>() {
                bridge
                    .node()
                    .log_event("ERROR", &format!("创建主窗口失败：{err}"));
            }
            false
        }
    }
}

/// 跨线程安全入口：统一通过 `run_on_main_thread` 串行建窗/聚焦，避免非主线程阻塞或并发建窗竞态。
pub(super) fn create_or_focus_main_window(app: &AppHandle) {
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        let _ = ensure_main_window_on_main_thread(&handle);
    });
}

/// 全局热键专用入口：在主线程串行判断主窗口是否已存在，再联动快捷启动框状态。
pub(super) fn open_or_toggle_launcher_window(app: &AppHandle, bridge: &Arc<BridgeState>) {
    let handle = app.clone();
    let launcher = bridge.clone();
    let _ = app.run_on_main_thread(move || {
        let cold_started = ensure_main_window_on_main_thread(&handle);
        launcher.on_launcher_hotkey(cold_started);
    });
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
