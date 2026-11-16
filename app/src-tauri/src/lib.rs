pub mod bridge;
pub mod bridge_stage3;
pub mod single_instance;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use bridge::{
    BridgeState, EventSink, Reply, SelfInfo, UiAppInfo, UiBatchDeviceResult,
    UiCapturedShot, UiClipboardPreview, UiConflictPolicy, UiDevice, UiDiagLogs, UiDiagSummary,
    UiEditSyncPrompt, UiEditSyncResult, UiFsCacheHit, UiFsChangedEvent, UiFsListBatchEvent,
    UiFsListing, UiFsRoot, UiFsSearchBatchEvent, UiHostStatus, UiInputOutcome, UiLanPeer,
    UiLaunchResult, UiLocalEditSession, UiNetcheckReport, UiPairOffer, UiPairRequest,
    UiPermissions, UiPowerResult, UiProcInfo, UiQuickHit, UiSettings, UiShotTargets,
    UiSystemNotice, UiTaskProgress, UiTermExitEvent, UiTermOutputEvent, UiTermSession,
    UiWakeResult,
};
use pcshare_core::lan_discovery::MDNS_DEFAULT_PORT;
use pcshare_core::{CoreConfig, CoreNode, NetworkMode, default_data_dir};
use pcshare_platform::NativePlatform;
use serde::Serialize;
use single_instance::SingleInstanceGuard;
use tauri::image::Image;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, State, WebviewUrl, WebviewWindowBuilder, Wry};

type ManagedBridge = Arc<BridgeState>;

#[derive(Serialize, Clone)]
struct DeviceRemovedPayload {
    id: String,
}

#[derive(Serialize, Clone)]
struct HostPausedPayload {
    paused: bool,
}

struct TauriEventSink {
    app: AppHandle,
    pause_menu_item: MutexSlot<CheckMenuItem<Wry>>,
}

type MutexSlot<T> = std::sync::Mutex<Option<T>>;

impl EventSink for TauriEventSink {
    fn emit_device_status(&self, device: UiDevice) {
        let _ = self.app.emit("device://status", device);
    }

    fn emit_device_removed(&self, id: String) {
        let _ = self
            .app
            .emit("device://removed", DeviceRemovedPayload { id });
    }

    fn emit_pair_request(&self, req: UiPairRequest) {
        // 如果窗口已关闭到托盘，收到配对请求时主动唤出窗口方便屏幕前的人点确认
        create_or_focus_main_window(&self.app);
        let _ = self.app.emit("pair://request", req);
    }

    fn emit_task_progress(&self, progress: UiTaskProgress) {
        let _ = self.app.emit("task://progress", progress);
    }

    fn emit_fs_list_batch(&self, batch: UiFsListBatchEvent) {
        let _ = self.app.emit("fs://list-batch", batch);
    }

    fn emit_fs_changed(&self, ev: UiFsChangedEvent) {
        let _ = self.app.emit("fs://changed", ev);
    }

    fn emit_lan_changed(&self, peers: Vec<UiLanPeer>) {
        let _ = self.app.emit("lan://changed", peers);
    }

    fn emit_host_paused(&self, paused: bool) {
        if let Ok(guard) = self.pause_menu_item.lock()
            && let Some(ref item) = *guard
        {
            let _ = item.set_checked(paused);
        }
        let _ = self.app.emit("host://paused", HostPausedPayload { paused });
    }

    fn emit_launcher_toggle(&self) {
        let _ = self.app.emit("launcher://toggle", ());
    }

    fn emit_fs_search_batch(&self, batch: UiFsSearchBatchEvent) {
        let _ = self.app.emit("fs://search-batch", batch);
    }

    fn emit_term_output(&self, ev: UiTermOutputEvent) {
        let _ = self.app.emit("term://output", ev);
    }

    fn emit_term_exit(&self, ev: UiTermExitEvent) {
        let _ = self.app.emit("term://exit", ev);
    }

    fn emit_edit_sync_prompt(&self, ev: UiEditSyncPrompt) {
        create_or_focus_main_window(&self.app);
        let _ = self.app.emit("edit://prompt", ev);
    }

    fn emit_batch_item(&self, ev: UiBatchDeviceResult) {
        let _ = self.app.emit("batch://item", ev);
    }

    fn emit_system_notice(&self, ev: UiSystemNotice) {
        // 窗口缩在托盘或未聚焦时优先发送系统桌面通知，同时通知前端展示提示（任务 3.10）
        let window_focused = self
            .app
            .get_webview_window("main")
            .and_then(|w| w.is_focused().ok())
            .unwrap_or(false);
        if !window_focused
            && let Some(bridge) = self.app.try_state::<ManagedBridge>()
        {
            let _ = bridge
                .node()
                .platform()
                .show_system_notification(&ev.title, &ev.body);
        }
        let _ = self.app.emit("app://notice", ev);
    }
}

// ────────────────────────────── Tauri 命令导出 ──────────────────────────────

#[tauri::command]
async fn self_info(state: State<'_, ManagedBridge>) -> Result<Reply<SelfInfo>, ()> {
    Ok(state.self_info())
}

#[tauri::command]
async fn settings_get(state: State<'_, ManagedBridge>) -> Result<Reply<UiSettings>, ()> {
    Ok(state.settings_get())
}

#[tauri::command]
async fn settings_set(
    state: State<'_, ManagedBridge>,
    settings: UiSettings,
) -> Result<Reply<()>, ()> {
    let bridge = state.inner().clone();
    Ok(tokio::task::spawn_blocking(move || bridge.settings_set(settings))
        .await
        .unwrap_or_else(|e| {
            Reply::err(
                bridge::UiErrorCode::Internal,
                format!("保存设置任务异常：{e}"),
            )
        }))
}

#[tauri::command]
async fn host_status(state: State<'_, ManagedBridge>) -> Result<Reply<UiHostStatus>, ()> {
    Ok(state.host_status())
}

#[tauri::command]
async fn host_set_paused(
    state: State<'_, ManagedBridge>,
    paused: bool,
) -> Result<Reply<()>, ()> {
    Ok(state.host_set_paused(paused))
}

#[tauri::command]
async fn device_list(state: State<'_, ManagedBridge>) -> Result<Reply<Vec<UiDevice>>, ()> {
    Ok(state.device_list())
}

#[tauri::command]
async fn lan_list(state: State<'_, ManagedBridge>) -> Result<Reply<Vec<UiLanPeer>>, ()> {
    Ok(state.lan_list())
}

#[tauri::command]
async fn device_rename(
    state: State<'_, ManagedBridge>,
    id: String,
    name: String,
) -> Result<Reply<()>, ()> {
    Ok(state.device_rename(id, name))
}

#[tauri::command]
async fn device_remove(state: State<'_, ManagedBridge>, id: String) -> Result<Reply<()>, ()> {
    Ok(state.device_remove(id))
}

#[tauri::command]
async fn device_set_permissions(
    state: State<'_, ManagedBridge>,
    id: String,
    permissions: UiPermissions,
) -> Result<Reply<()>, ()> {
    Ok(state.device_set_permissions(id, permissions))
}

#[tauri::command]
async fn device_reconnect(state: State<'_, ManagedBridge>, id: String) -> Result<Reply<()>, ()> {
    Ok(state.inner().clone().device_reconnect(id).await)
}

#[tauri::command]
async fn pair_begin(state: State<'_, ManagedBridge>) -> Result<Reply<UiPairOffer>, ()> {
    Ok(state.pair_begin())
}

#[tauri::command]
async fn pair_cancel(state: State<'_, ManagedBridge>) -> Result<Reply<()>, ()> {
    Ok(state.pair_cancel())
}

#[tauri::command]
async fn pair_connect(
    state: State<'_, ManagedBridge>,
    target: String,
    code: String,
) -> Result<Reply<UiDevice>, ()> {
    Ok(state.inner().clone().pair_connect(target, code).await)
}

#[tauri::command]
async fn pair_respond(
    state: State<'_, ManagedBridge>,
    request_id: String,
    allow: bool,
    permissions: UiPermissions,
) -> Result<Reply<()>, ()> {
    Ok(state.pair_respond(request_id, allow, permissions))
}

#[tauri::command]
async fn fs_roots(
    state: State<'_, ManagedBridge>,
    device: String,
) -> Result<Reply<Vec<UiFsRoot>>, ()> {
    Ok(state.inner().clone().fs_roots(device).await)
}

#[tauri::command]
async fn fs_cache_get(
    state: State<'_, ManagedBridge>,
    device: String,
    path: String,
) -> Result<Reply<Option<UiFsCacheHit>>, ()> {
    Ok(state.fs_cache_get(device, path))
}

#[tauri::command]
async fn fs_list(
    state: State<'_, ManagedBridge>,
    device: String,
    path: String,
) -> Result<Reply<UiFsListing>, ()> {
    Ok(state.inner().clone().fs_list(device, path).await)
}

#[tauri::command]
async fn fs_watch(
    state: State<'_, ManagedBridge>,
    device: String,
    path: String,
) -> Result<Reply<usize>, ()> {
    Ok(state.inner().clone().fs_watch(device, path).await)
}

#[tauri::command]
async fn fs_unwatch(
    state: State<'_, ManagedBridge>,
    device: String,
    path: String,
) -> Result<Reply<usize>, ()> {
    Ok(state.inner().clone().fs_unwatch(device, path).await)
}

#[tauri::command]
async fn fs_check_conflicts(
    state: State<'_, ManagedBridge>,
    device: String,
    dest_dir: String,
    names: Vec<String>,
) -> Result<Reply<Vec<String>>, ()> {
    Ok(state
        .inner()
        .clone()
        .fs_check_conflicts(device, dest_dir, names)
        .await)
}

#[tauri::command]
async fn fs_mkdir(
    state: State<'_, ManagedBridge>,
    device: String,
    path: String,
) -> Result<Reply<()>, ()> {
    Ok(state.inner().clone().fs_mkdir(device, path).await)
}

#[tauri::command]
async fn fs_rename(
    state: State<'_, ManagedBridge>,
    device: String,
    path: String,
    new_name: String,
) -> Result<Reply<()>, ()> {
    Ok(state
        .inner()
        .clone()
        .fs_rename(device, path, new_name)
        .await)
}

#[tauri::command]
async fn fs_copy(
    state: State<'_, ManagedBridge>,
    device: String,
    sources: Vec<String>,
    dest_dir: String,
) -> Result<Reply<String>, ()> {
    Ok(state
        .inner()
        .clone()
        .fs_copy(device, sources, dest_dir)
        .await)
}

#[tauri::command]
async fn fs_move(
    state: State<'_, ManagedBridge>,
    device: String,
    sources: Vec<String>,
    dest_dir: String,
) -> Result<Reply<String>, ()> {
    Ok(state
        .inner()
        .clone()
        .fs_move(device, sources, dest_dir)
        .await)
}

#[tauri::command]
async fn fs_delete(
    state: State<'_, ManagedBridge>,
    device: String,
    paths: Vec<String>,
    permanent: bool,
) -> Result<Reply<()>, ()> {
    Ok(state
        .inner()
        .clone()
        .fs_delete(device, paths, permanent)
        .await)
}

#[tauri::command]
async fn transfer_start(
    state: State<'_, ManagedBridge>,
    src: String,
    sources: Vec<String>,
    dst: String,
    dest_dir: String,
    conflict: UiConflictPolicy,
) -> Result<Reply<String>, ()> {
    Ok(state
        .inner()
        .clone()
        .transfer_start(src, sources, dst, dest_dir, conflict)
        .await)
}

#[tauri::command]
async fn task_pause(state: State<'_, ManagedBridge>, task_id: String) -> Result<Reply<()>, ()> {
    Ok(state.task_pause(task_id))
}

#[tauri::command]
async fn task_resume(state: State<'_, ManagedBridge>, task_id: String) -> Result<Reply<()>, ()> {
    Ok(state.task_resume(task_id))
}

#[tauri::command]
async fn task_cancel(state: State<'_, ManagedBridge>, task_id: String) -> Result<Reply<()>, ()> {
    Ok(state.inner().clone().task_cancel(task_id).await)
}

#[tauri::command]
async fn transfer_get_speed_limit(
    state: State<'_, ManagedBridge>,
) -> Result<Reply<Option<u64>>, ()> {
    Ok(state.transfer_get_speed_limit())
}

#[tauri::command]
async fn transfer_set_speed_limit(
    state: State<'_, ManagedBridge>,
    bytes_per_sec: Option<u64>,
) -> Result<Reply<()>, ()> {
    Ok(state.transfer_set_speed_limit(bytes_per_sec))
}

#[tauri::command]
async fn pick_local_files(state: State<'_, ManagedBridge>) -> Result<Reply<Vec<String>>, ()> {
    Ok(state.pick_local_files().await)
}

#[tauri::command]
async fn pick_local_dir(state: State<'_, ManagedBridge>) -> Result<Reply<Option<String>>, ()> {
    Ok(state.pick_local_dir().await)
}

#[tauri::command]
async fn app_list(
    state: State<'_, ManagedBridge>,
    device: String,
) -> Result<Reply<Vec<UiAppInfo>>, ()> {
    Ok(state.inner().clone().app_list(device).await)
}

#[tauri::command]
async fn app_icon(
    state: State<'_, ManagedBridge>,
    device: String,
    app_id: String,
) -> Result<Reply<Option<String>>, ()> {
    Ok(state.inner().clone().app_icon(device, app_id).await)
}

#[tauri::command]
async fn app_launch(
    state: State<'_, ManagedBridge>,
    device: String,
    target: String,
    args: Option<String>,
) -> Result<Reply<UiLaunchResult>, ()> {
    Ok(state.inner().clone().app_launch(device, target, args).await)
}

#[tauri::command]
async fn proc_list(
    state: State<'_, ManagedBridge>,
    device: String,
) -> Result<Reply<Vec<UiProcInfo>>, ()> {
    Ok(state.inner().clone().proc_list(device).await)
}

#[tauri::command]
async fn proc_kill(
    state: State<'_, ManagedBridge>,
    device: String,
    pid: u32,
) -> Result<Reply<()>, ()> {
    Ok(state.inner().clone().proc_kill(device, pid).await)
}

#[tauri::command]
async fn diag_summary(state: State<'_, ManagedBridge>) -> Result<Reply<UiDiagSummary>, ()> {
    Ok(state.diag_summary())
}

#[tauri::command]
async fn diag_netcheck(state: State<'_, ManagedBridge>) -> Result<Reply<UiNetcheckReport>, ()> {
    Ok(state.inner().clone().diag_netcheck().await)
}

#[tauri::command]
async fn diag_logs(
    state: State<'_, ManagedBridge>,
    limit: usize,
) -> Result<Reply<UiDiagLogs>, ()> {
    Ok(state.diag_logs(limit))
}

#[tauri::command]
async fn diag_export(
    state: State<'_, ManagedBridge>,
    dest_dir: String,
) -> Result<Reply<String>, ()> {
    Ok(state.inner().clone().diag_export(dest_dir).await)
}

#[tauri::command]
async fn launcher_consume_pending(state: State<'_, ManagedBridge>) -> Result<Reply<bool>, ()> {
    Ok(state.launcher_consume_pending())
}

#[tauri::command]
async fn quick_search(
    state: State<'_, ManagedBridge>,
    query: String,
    limit: Option<usize>,
) -> Result<Reply<Vec<UiQuickHit>>, ()> {
    Ok(state.quick_search(query, limit))
}

#[tauri::command]
async fn quick_launch(
    state: State<'_, ManagedBridge>,
    device: String,
    target: String,
    kind: String,
) -> Result<Reply<UiLaunchResult>, ()> {
    Ok(state.inner().clone().quick_launch(device, target, kind).await)
}

#[tauri::command]
async fn fs_search_start(
    state: State<'_, ManagedBridge>,
    device: String,
    path: String,
    query: String,
) -> Result<Reply<String>, ()> {
    Ok(state
        .inner()
        .clone()
        .fs_search_start(device, path, query)
        .await)
}

#[tauri::command]
async fn fs_search_cancel(
    state: State<'_, ManagedBridge>,
    search_id: String,
) -> Result<Reply<()>, ()> {
    Ok(state.fs_search_cancel(search_id))
}

#[tauri::command]
async fn shot_monitors(
    state: State<'_, ManagedBridge>,
    device: String,
) -> Result<Reply<UiShotTargets>, ()> {
    Ok(state.inner().clone().shot_monitors(device).await)
}

#[tauri::command]
async fn shot_capture(
    state: State<'_, ManagedBridge>,
    device: String,
    monitor_index: Option<u32>,
    hwnd: Option<String>,
    max_width: Option<u32>,
    quality: Option<u8>,
) -> Result<Reply<UiCapturedShot>, ()> {
    Ok(state
        .inner()
        .clone()
        .shot_capture(device, monitor_index, hwnd, max_width, quality)
        .await)
}

#[allow(clippy::too_many_arguments)]
#[tauri::command]
async fn input_click(
    state: State<'_, ManagedBridge>,
    device: String,
    screen_x: i32,
    screen_y: i32,
    button: String,
    double: bool,
    target_hwnd: Option<String>,
    refresh_monitor: Option<u32>,
    refresh_hwnd: Option<String>,
    max_width: Option<u32>,
) -> Result<Reply<UiInputOutcome>, ()> {
    Ok(state
        .inner()
        .clone()
        .input_click(
            device,
            screen_x,
            screen_y,
            button,
            double,
            target_hwnd,
            refresh_monitor,
            refresh_hwnd,
            max_width,
        )
        .await)
}

#[allow(clippy::too_many_arguments)]
#[tauri::command]
async fn input_key(
    state: State<'_, ManagedBridge>,
    device: String,
    text: Option<String>,
    key_combo: Option<String>,
    target_hwnd: Option<String>,
    refresh_monitor: Option<u32>,
    refresh_hwnd: Option<String>,
    max_width: Option<u32>,
) -> Result<Reply<UiInputOutcome>, ()> {
    Ok(state
        .inner()
        .clone()
        .input_key(
            device,
            text,
            key_combo,
            target_hwnd,
            refresh_monitor,
            refresh_hwnd,
            max_width,
        )
        .await)
}

#[tauri::command]
async fn fs_open_remote(
    state: State<'_, ManagedBridge>,
    device: String,
    path: String,
) -> Result<Reply<UiLaunchResult>, ()> {
    Ok(state.inner().clone().fs_open_remote(device, path).await)
}

#[tauri::command]
async fn fs_open_local_edit(
    state: State<'_, ManagedBridge>,
    device: String,
    path: String,
    launch_app: Option<bool>,
) -> Result<Reply<UiLocalEditSession>, ()> {
    Ok(state
        .inner()
        .clone()
        .fs_open_local_edit(device, path, launch_app)
        .await)
}

#[tauri::command]
async fn fs_edit_sync_back(
    state: State<'_, ManagedBridge>,
    session_id: String,
    auto_sync: bool,
    force_overwrite: bool,
) -> Result<Reply<UiEditSyncResult>, ()> {
    Ok(state
        .inner()
        .clone()
        .fs_edit_sync_back(session_id, auto_sync, force_overwrite)
        .await)
}

#[tauri::command]
async fn power_exec(
    state: State<'_, ManagedBridge>,
    device: String,
    action: String,
    countdown_secs: Option<u32>,
) -> Result<Reply<UiPowerResult>, ()> {
    Ok(state
        .inner()
        .clone()
        .power_exec(device, action, countdown_secs)
        .await)
}

#[tauri::command]
async fn power_cancel(
    state: State<'_, ManagedBridge>,
    device: String,
) -> Result<Reply<bool>, ()> {
    Ok(state.inner().clone().power_cancel(device).await)
}

#[tauri::command]
async fn power_wake(
    state: State<'_, ManagedBridge>,
    device: String,
) -> Result<Reply<UiWakeResult>, ()> {
    Ok(state.inner().clone().power_wake(device).await)
}

#[tauri::command]
async fn term_open(
    state: State<'_, ManagedBridge>,
    device: String,
    shell: String,
    cols: u16,
    rows: u16,
    cwd: Option<String>,
) -> Result<Reply<UiTermSession>, ()> {
    Ok(state
        .inner()
        .clone()
        .term_open(device, shell, cols, rows, cwd)
        .await)
}

#[tauri::command]
async fn term_write(
    state: State<'_, ManagedBridge>,
    session_id: String,
    data_b64: String,
) -> Result<Reply<()>, ()> {
    Ok(state.term_write(session_id, data_b64).await)
}

#[tauri::command]
async fn term_resize(
    state: State<'_, ManagedBridge>,
    session_id: String,
    cols: u16,
    rows: u16,
) -> Result<Reply<()>, ()> {
    Ok(state.term_resize(session_id, cols, rows).await)
}

#[tauri::command]
async fn term_close(
    state: State<'_, ManagedBridge>,
    session_id: String,
) -> Result<Reply<()>, ()> {
    Ok(state.term_close(session_id).await)
}

#[tauri::command]
async fn clip_push(
    state: State<'_, ManagedBridge>,
    device: String,
) -> Result<Reply<String>, ()> {
    Ok(state.inner().clone().clip_push(device).await)
}

#[tauri::command]
async fn clip_pull(
    state: State<'_, ManagedBridge>,
    device: String,
) -> Result<Reply<UiClipboardPreview>, ()> {
    Ok(state.inner().clone().clip_pull(device).await)
}

#[tauri::command]
async fn clip_peek(
    state: State<'_, ManagedBridge>,
    device: String,
) -> Result<Reply<UiClipboardPreview>, ()> {
    Ok(state.inner().clone().clip_peek(device).await)
}

#[allow(clippy::too_many_arguments)]
#[tauri::command]
async fn batch_exec(
    state: State<'_, ManagedBridge>,
    devices: Vec<String>,
    op: String,
    target: Option<String>,
    power_action: Option<String>,
    command: Option<String>,
    shell: Option<String>,
) -> Result<Reply<Vec<UiBatchDeviceResult>>, ()> {
    Ok(state
        .inner()
        .clone()
        .batch_exec(devices, op, target, power_action, command, shell)
        .await)
}

#[tauri::command]
async fn fs_drag_out(
    state: State<'_, ManagedBridge>,
    device: String,
    paths: Vec<String>,
) -> Result<Reply<bool>, ()> {
    Ok(state.inner().clone().fs_drag_out(device, paths).await)
}

struct LaunchArgs {
    data_dir: PathBuf,
    network_mode: NetworkMode,
    background: bool,
}

fn parse_launch_args() -> LaunchArgs {
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
fn create_or_focus_main_window(app: &AppHandle) {
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

fn setup_system_tray(
    app: &AppHandle,
    sink: &Arc<TauriEventSink>,
    quitting: Arc<AtomicBool>,
) -> tauri::Result<()> {
    let open_item = MenuItem::with_id(app, "open", "打开主窗口", true, None::<&str>)?;
    let pause_item = CheckMenuItem::with_id(app, "pause_host", "暂停被控", true, false, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;
    let quit_item = MenuItem::with_id(app, "quit", "退出程序", true, None::<&str>)?;

    if let Ok(mut guard) = sink.pause_menu_item.lock() {
        *guard = Some(pause_item.clone());
    }

    let menu = Menu::with_items(app, &[&open_item, &pause_item, &sep, &quit_item])?;
    let icon = app
        .default_window_icon()
        .map(|i| Image::new_owned(i.rgba().to_vec(), i.width(), i.height()))
        .unwrap_or_else(fallback_tray_icon);

    TrayIconBuilder::with_id("pcshare-tray")
        .icon(icon)
        .tooltip("电脑共享 — 后台运行中")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(move |app_handle, event| match event.id().as_ref() {
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
        })
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

pub fn run() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing_subscriber::filter::LevelFilter::WARN)
        .with_target(false)
        .try_init();

    let args = parse_launch_args();
    let data_dir = args.data_dir;
    let network_mode = args.network_mode;
    let start_in_background = args.background;

    let app_slot: Arc<OnceLock<AppHandle>> = Arc::new(OnceLock::new());
    let slot_for_wakeup = app_slot.clone();
    let quitting = Arc::new(AtomicBool::new(false));
    let quitting_for_run = quitting.clone();

    let Some(_instance_guard) = SingleInstanceGuard::try_acquire(&data_dir, move || {
        if let Some(handle) = slot_for_wakeup.get() {
            create_or_focus_main_window(handle);
        }
    }) else {
        // 已有实例运行并已发送唤醒指令（哪怕原实例当前处于无窗口后台常驻状态，也会立即重建窗口）
        return;
    };

    let app = tauri::Builder::default()
        .setup(move |app| {
            let handle = app.handle().clone();
            let _ = app_slot.set(handle.clone());

            let sink = Arc::new(TauriEventSink {
                app: handle.clone(),
                pause_menu_item: std::sync::Mutex::new(None),
            });
            let sink_for_tray = sink.clone();

            let cfg = CoreConfig {
                data_dir,
                network_mode,
                device_name_override: None,
                auto_connect_peers: true,
            };
            let bridge = tauri::async_runtime::block_on(async move {
                let node = CoreNode::start(cfg, Arc::new(NativePlatform::new())).await?;
                if network_mode == NetworkMode::PublicOrCustom {
                    let _ = node.start_lan_discovery_on_port(MDNS_DEFAULT_PORT);
                }
                let sink_dyn: Arc<dyn EventSink> = sink;
                Ok::<_, pcshare_core::CoreError>(BridgeState::new(node, sink_dyn))
            })
            .map_err(|e| std::io::Error::other(e.to_user_message()))?;

            app.manage(bridge.clone());
            let _ = setup_system_tray(&handle, &sink_for_tray, quitting.clone());

            // 注册全局唤出快捷键（默认 Alt+Space，任务 3.1）
            let hk_handle = handle.clone();
            let hk_bridge = bridge.clone();
            let hk_warn = bridge.bind_hotkey_handler(Arc::new(move || {
                create_or_focus_main_window(&hk_handle);
                hk_bridge.mark_launcher_pending();
            }));

            if !start_in_background {
                create_or_focus_main_window(&handle);
            }
            if let Err(conflict_msg) = hk_warn {
                sink_for_tray.emit_system_notice(UiSystemNotice {
                    title: "全局快捷键被占用".to_string(),
                    body: conflict_msg,
                    kind: "warn".to_string(),
                });
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            self_info,
            settings_get,
            settings_set,
            host_status,
            host_set_paused,
            device_list,
            lan_list,
            device_rename,
            device_remove,
            device_set_permissions,
            device_reconnect,
            pair_begin,
            pair_cancel,
            pair_connect,
            pair_respond,
            fs_roots,
            fs_cache_get,
            fs_list,
            fs_watch,
            fs_unwatch,
            fs_check_conflicts,
            fs_mkdir,
            fs_rename,
            fs_copy,
            fs_move,
            fs_delete,
            transfer_start,
            task_pause,
            task_resume,
            task_cancel,
            transfer_get_speed_limit,
            transfer_set_speed_limit,
            pick_local_files,
            pick_local_dir,
            app_list,
            app_icon,
            app_launch,
            proc_list,
            proc_kill,
            diag_summary,
            diag_netcheck,
            diag_logs,
            diag_export,
            launcher_consume_pending,
            quick_search,
            quick_launch,
            fs_search_start,
            fs_search_cancel,
            shot_monitors,
            shot_capture,
            input_click,
            input_key,
            fs_open_remote,
            fs_open_local_edit,
            fs_edit_sync_back,
            power_exec,
            power_cancel,
            power_wake,
            term_open,
            term_write,
            term_resize,
            term_close,
            clip_push,
            clip_pull,
            clip_peek,
            batch_exec,
            fs_drag_out,
        ])
        .build(tauri::generate_context!())
        .expect("构建电脑共享桌面应用失败");

    app.run(move |app_handle, event| match event {
        tauri::RunEvent::ExitRequested { api, .. } => {
            // 关闭最后一个窗口时销毁了浏览器内核以释放内存，但除非用户在托盘点“退出程序”，否则保持后台常驻
            if !quitting_for_run.load(Ordering::SeqCst) {
                api.prevent_exit();
            }
        }
        tauri::RunEvent::Exit => {
            if let Some(bridge) = app_handle.try_state::<ManagedBridge>() {
                let b = bridge.inner().clone();
                tauri::async_runtime::block_on(async move {
                    b.shutdown().await;
                });
            }
        }
        _ => {}
    });
}
