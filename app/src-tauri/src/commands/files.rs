use super::*;

#[tauri::command]
pub(crate) async fn fs_roots(
    state: State<'_, ManagedBridge>,
    device: String,
) -> Result<Reply<Vec<UiFsRoot>>, ()> {
    Ok(state.inner().clone().fs_roots(device).await)
}

#[tauri::command]
pub(crate) async fn fs_cache_get(
    state: State<'_, ManagedBridge>,
    device: String,
    path: String,
) -> Result<Reply<Option<UiFsCacheHit>>, ()> {
    Ok(state.fs_cache_get(device, path))
}

#[tauri::command]
pub(crate) async fn fs_list(
    state: State<'_, ManagedBridge>,
    device: String,
    path: String,
) -> Result<Reply<UiFsListing>, ()> {
    Ok(state.inner().clone().fs_list(device, path).await)
}

#[tauri::command]
pub(crate) async fn fs_watch(
    state: State<'_, ManagedBridge>,
    device: String,
    path: String,
) -> Result<Reply<usize>, ()> {
    Ok(state.inner().clone().fs_watch(device, path).await)
}

#[tauri::command]
pub(crate) async fn fs_unwatch(
    state: State<'_, ManagedBridge>,
    device: String,
    path: String,
) -> Result<Reply<usize>, ()> {
    Ok(state.inner().clone().fs_unwatch(device, path).await)
}

#[tauri::command]
pub(crate) async fn fs_check_conflicts(
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
pub(crate) async fn fs_mkdir(
    state: State<'_, ManagedBridge>,
    device: String,
    path: String,
) -> Result<Reply<()>, ()> {
    Ok(state.inner().clone().fs_mkdir(device, path).await)
}

#[tauri::command]
pub(crate) async fn fs_rename(
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
pub(crate) async fn fs_copy(
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
pub(crate) async fn fs_move(
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
pub(crate) async fn fs_delete(
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
pub(crate) async fn transfer_start(
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
pub(crate) async fn task_list(
    state: State<'_, ManagedBridge>,
) -> Result<Reply<Vec<UiTaskProgress>>, ()> {
    Ok(state.task_list())
}

#[tauri::command]
pub(crate) async fn task_pause(
    state: State<'_, ManagedBridge>,
    task_id: String,
) -> Result<Reply<()>, ()> {
    Ok(state.task_pause(task_id))
}

#[tauri::command]
pub(crate) async fn task_resume(
    state: State<'_, ManagedBridge>,
    task_id: String,
) -> Result<Reply<()>, ()> {
    Ok(state.task_resume(task_id))
}

#[tauri::command]
pub(crate) async fn task_cancel(
    state: State<'_, ManagedBridge>,
    task_id: String,
) -> Result<Reply<()>, ()> {
    Ok(state.inner().clone().task_cancel(task_id).await)
}

#[tauri::command]
pub(crate) async fn transfer_get_speed_limit(
    state: State<'_, ManagedBridge>,
) -> Result<Reply<Option<u64>>, ()> {
    Ok(state.transfer_get_speed_limit())
}

#[tauri::command]
pub(crate) async fn transfer_set_speed_limit(
    state: State<'_, ManagedBridge>,
    bytes_per_sec: Option<u64>,
) -> Result<Reply<()>, ()> {
    Ok(state.transfer_set_speed_limit(bytes_per_sec))
}

#[tauri::command]
pub(crate) async fn pick_local_files(
    state: State<'_, ManagedBridge>,
) -> Result<Reply<Vec<String>>, ()> {
    Ok(state.pick_local_files().await)
}

#[tauri::command]
pub(crate) async fn pick_local_dir(
    state: State<'_, ManagedBridge>,
) -> Result<Reply<Option<String>>, ()> {
    Ok(state.pick_local_dir().await)
}
