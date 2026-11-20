use super::*;

#[tauri::command]
pub(crate) async fn app_list(
    state: State<'_, ManagedBridge>,
    device: String,
    force: Option<bool>,
) -> Result<Reply<Vec<UiAppInfo>>, ()> {
    Ok(state
        .inner()
        .clone()
        .app_list(device, force.unwrap_or(false))
        .await)
}

#[tauri::command]
pub(crate) async fn app_icon(
    state: State<'_, ManagedBridge>,
    device: String,
    app_id: String,
) -> Result<Reply<Option<String>>, ()> {
    Ok(state.inner().clone().app_icon(device, app_id).await)
}

#[tauri::command]
pub(crate) async fn app_launch(
    state: State<'_, ManagedBridge>,
    device: String,
    target: String,
    args: Option<String>,
) -> Result<Reply<UiLaunchResult>, ()> {
    Ok(state.inner().clone().app_launch(device, target, args).await)
}

#[tauri::command]
pub(crate) async fn proc_list(
    state: State<'_, ManagedBridge>,
    device: String,
) -> Result<Reply<Vec<UiProcInfo>>, ()> {
    Ok(state.inner().clone().proc_list(device).await)
}

#[tauri::command]
pub(crate) async fn proc_kill(
    state: State<'_, ManagedBridge>,
    device: String,
    pid: u32,
) -> Result<Reply<()>, ()> {
    Ok(state.inner().clone().proc_kill(device, pid).await)
}

#[tauri::command]
pub(crate) async fn diag_summary(
    state: State<'_, ManagedBridge>,
) -> Result<Reply<UiDiagSummary>, ()> {
    Ok(state.diag_summary())
}

#[tauri::command]
pub(crate) async fn diag_netcheck(
    state: State<'_, ManagedBridge>,
) -> Result<Reply<UiNetcheckReport>, ()> {
    Ok(state.inner().clone().diag_netcheck().await)
}

#[tauri::command]
pub(crate) async fn diag_logs(
    state: State<'_, ManagedBridge>,
    limit: usize,
) -> Result<Reply<UiDiagLogs>, ()> {
    let bridge = state.inner().clone();
    spawn_blocking_reply("读取诊断日志任务", move || bridge.diag_logs(limit)).await
}

#[tauri::command]
pub(crate) async fn diag_export(
    state: State<'_, ManagedBridge>,
    dest_dir: String,
) -> Result<Reply<String>, ()> {
    Ok(state.inner().clone().diag_export(dest_dir).await)
}

#[tauri::command]
pub(crate) async fn launcher_consume_pending(
    state: State<'_, ManagedBridge>,
) -> Result<Reply<bool>, ()> {
    Ok(state.launcher_consume_pending())
}

#[tauri::command]
pub(crate) async fn quick_search(
    state: State<'_, ManagedBridge>,
    query: String,
    limit: Option<usize>,
) -> Result<Reply<Vec<UiQuickHit>>, ()> {
    Ok(state.quick_search(query, limit))
}

#[tauri::command]
pub(crate) async fn quick_launch(
    state: State<'_, ManagedBridge>,
    device: String,
    target: String,
    kind: String,
) -> Result<Reply<UiLaunchResult>, ()> {
    Ok(state
        .inner()
        .clone()
        .quick_launch(device, target, kind)
        .await)
}

#[tauri::command]
pub(crate) async fn fs_search_start(
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
pub(crate) async fn fs_search_cancel(
    state: State<'_, ManagedBridge>,
    search_id: String,
) -> Result<Reply<()>, ()> {
    Ok(state.fs_search_cancel(search_id))
}
