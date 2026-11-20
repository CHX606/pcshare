use super::*;

#[tauri::command]
pub(crate) async fn fs_open_remote(
    state: State<'_, ManagedBridge>,
    device: String,
    path: String,
) -> Result<Reply<UiLaunchResult>, ()> {
    Ok(state.inner().clone().fs_open_remote(device, path).await)
}

#[tauri::command]
pub(crate) async fn fs_open_local_edit(
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
pub(crate) async fn fs_edit_sync_back(
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
pub(crate) async fn edit_pending_list(
    state: State<'_, ManagedBridge>,
) -> Result<Reply<Vec<UiEditSyncPrompt>>, ()> {
    Ok(state.edit_pending_list())
}

#[tauri::command]
pub(crate) async fn power_exec(
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
pub(crate) async fn power_cancel(
    state: State<'_, ManagedBridge>,
    device: String,
) -> Result<Reply<bool>, ()> {
    Ok(state.inner().clone().power_cancel(device).await)
}

#[tauri::command]
pub(crate) async fn power_wake(
    state: State<'_, ManagedBridge>,
    device: String,
) -> Result<Reply<UiWakeResult>, ()> {
    Ok(state.inner().clone().power_wake(device).await)
}

#[tauri::command]
pub(crate) async fn term_open(
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
pub(crate) async fn term_write(
    state: State<'_, ManagedBridge>,
    session_id: String,
    data_b64: String,
) -> Result<Reply<()>, ()> {
    Ok(state.term_write(session_id, data_b64).await)
}

#[tauri::command]
pub(crate) async fn term_resize(
    state: State<'_, ManagedBridge>,
    session_id: String,
    cols: u16,
    rows: u16,
) -> Result<Reply<()>, ()> {
    Ok(state.term_resize(session_id, cols, rows).await)
}

#[tauri::command]
pub(crate) async fn term_close(
    state: State<'_, ManagedBridge>,
    session_id: String,
) -> Result<Reply<()>, ()> {
    Ok(state.term_close(session_id).await)
}

#[tauri::command]
pub(crate) async fn clip_push(
    state: State<'_, ManagedBridge>,
    device: String,
) -> Result<Reply<String>, ()> {
    Ok(state.inner().clone().clip_push(device).await)
}

#[tauri::command]
pub(crate) async fn clip_pull(
    state: State<'_, ManagedBridge>,
    device: String,
) -> Result<Reply<UiClipboardPreview>, ()> {
    Ok(state.inner().clone().clip_pull(device).await)
}

#[tauri::command]
pub(crate) async fn clip_peek(
    state: State<'_, ManagedBridge>,
    device: String,
) -> Result<Reply<UiClipboardPreview>, ()> {
    Ok(state.inner().clone().clip_peek(device).await)
}

#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub(crate) async fn batch_exec(
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
pub(crate) async fn fs_drag_out(
    state: State<'_, ManagedBridge>,
    device: String,
    paths: Vec<String>,
) -> Result<Reply<bool>, ()> {
    Ok(state.inner().clone().fs_drag_out(device, paths).await)
}
