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

bridge_arc_async_cmds! {
    app_icon(device: String, app_id: String) -> Option<String>;
    app_launch(device: String, target: String, args: Option<String>) -> UiLaunchResult;
    proc_list(device: String) -> Vec<UiProcInfo>;
    proc_kill(device: String, pid: u32) -> ();
    diag_netcheck() -> UiNetcheckReport;
    diag_export(dest_dir: String) -> String;
    quick_launch(device: String, target: String, kind: String) -> UiLaunchResult;
    fs_search_start(device: String, path: String, query: String) -> String;
}

bridge_sync_cmds! {
    diag_summary() -> UiDiagSummary;
    launcher_consume_pending() -> bool;
    quick_search(query: String, limit: Option<usize>) -> Vec<UiQuickHit>;
    fs_search_cancel(search_id: String) -> ();
}

bridge_blocking_cmds! {
    "读取诊断日志任务" => diag_logs(limit: usize) -> UiDiagLogs;
}
