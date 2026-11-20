use super::*;

#[tauri::command]
pub(crate) async fn self_info(state: State<'_, ManagedBridge>) -> Result<Reply<SelfInfo>, ()> {
    Ok(state.self_info())
}

#[tauri::command]
pub(crate) async fn settings_get(state: State<'_, ManagedBridge>) -> Result<Reply<UiSettings>, ()> {
    let bridge = state.inner().clone();
    spawn_blocking_reply("读取设置任务", move || bridge.settings_get()).await
}

#[tauri::command]
pub(crate) async fn settings_set(
    state: State<'_, ManagedBridge>,
    settings: UiSettings,
) -> Result<Reply<()>, ()> {
    let bridge = state.inner().clone();
    spawn_blocking_reply("保存设置任务", move || bridge.settings_set(settings)).await
}

#[tauri::command]
pub(crate) async fn host_status(
    state: State<'_, ManagedBridge>,
) -> Result<Reply<UiHostStatus>, ()> {
    Ok(state.host_status())
}

#[tauri::command]
pub(crate) async fn host_set_paused(
    state: State<'_, ManagedBridge>,
    paused: bool,
) -> Result<Reply<()>, ()> {
    let bridge = state.inner().clone();
    spawn_blocking_reply("切换被控状态任务", move || bridge.host_set_paused(paused)).await
}

#[tauri::command]
pub(crate) async fn device_list(
    state: State<'_, ManagedBridge>,
) -> Result<Reply<Vec<UiDevice>>, ()> {
    Ok(state.device_list())
}

#[tauri::command]
pub(crate) async fn lan_list(state: State<'_, ManagedBridge>) -> Result<Reply<Vec<UiLanPeer>>, ()> {
    Ok(state.lan_list())
}

#[tauri::command]
pub(crate) async fn device_rename(
    state: State<'_, ManagedBridge>,
    id: String,
    name: String,
) -> Result<Reply<()>, ()> {
    let bridge = state.inner().clone();
    spawn_blocking_reply("重命名设备任务", move || bridge.device_rename(id, name)).await
}

#[tauri::command]
pub(crate) async fn device_remove(
    state: State<'_, ManagedBridge>,
    id: String,
) -> Result<Reply<()>, ()> {
    let bridge = state.inner().clone();
    spawn_blocking_reply("移除设备任务", move || bridge.device_remove(id)).await
}

#[tauri::command]
pub(crate) async fn device_set_permissions(
    state: State<'_, ManagedBridge>,
    id: String,
    permissions: UiPermissions,
) -> Result<Reply<()>, ()> {
    let bridge = state.inner().clone();
    spawn_blocking_reply("保存设备权限任务", move || {
        bridge.device_set_permissions(id, permissions)
    })
    .await
}

#[tauri::command]
pub(crate) async fn device_reconnect(
    state: State<'_, ManagedBridge>,
    id: String,
) -> Result<Reply<()>, ()> {
    Ok(state.inner().clone().device_reconnect(id).await)
}

#[tauri::command]
pub(crate) async fn pair_begin(state: State<'_, ManagedBridge>) -> Result<Reply<UiPairOffer>, ()> {
    Ok(state.pair_begin())
}

#[tauri::command]
pub(crate) async fn pair_cancel(state: State<'_, ManagedBridge>) -> Result<Reply<()>, ()> {
    Ok(state.pair_cancel())
}

#[tauri::command]
pub(crate) async fn pair_connect(
    state: State<'_, ManagedBridge>,
    target: String,
    code: String,
) -> Result<Reply<UiDevice>, ()> {
    Ok(state.inner().clone().pair_connect(target, code).await)
}

#[tauri::command]
pub(crate) async fn pair_respond(
    state: State<'_, ManagedBridge>,
    request_id: String,
    allow: bool,
    permissions: UiPermissions,
) -> Result<Reply<()>, ()> {
    let bridge = state.inner().clone();
    spawn_blocking_reply("审批配对请求任务", move || {
        bridge.pair_respond(request_id, allow, permissions)
    })
    .await
}

#[tauri::command]
pub(crate) async fn pair_pending_list(
    state: State<'_, ManagedBridge>,
) -> Result<Reply<Vec<UiPairRequest>>, ()> {
    Ok(state.pair_pending_list())
}
