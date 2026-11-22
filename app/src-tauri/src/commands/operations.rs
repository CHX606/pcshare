use super::*;

bridge_arc_async_cmds! {
    fs_open_remote(device: String, path: String) -> UiLaunchResult;
    fs_open_local_edit(
        device: String,
        path: String,
        launch_app: Option<bool>,
    ) -> UiLocalEditSession;
    fs_edit_sync_back(
        session_id: String,
        auto_sync: bool,
        force_overwrite: bool,
    ) -> UiEditSyncResult;
    power_exec(
        device: String,
        action: String,
        countdown_secs: Option<u32>,
    ) -> UiPowerResult;
    power_cancel(device: String) -> bool;
    power_wake(device: String) -> UiWakeResult;
    term_open(
        device: String,
        shell: String,
        cols: u16,
        rows: u16,
        cwd: Option<String>,
    ) -> UiTermSession;
    clip_push(device: String) -> String;
    clip_pull(device: String) -> UiClipboardPreview;
    clip_peek(device: String) -> UiClipboardPreview;
    batch_exec(
        devices: Vec<String>,
        op: String,
        target: Option<String>,
        power_action: Option<String>,
        command: Option<String>,
        shell: Option<String>,
    ) -> Vec<UiBatchDeviceResult>;
    fs_drag_out(device: String, paths: Vec<String>) -> bool;
}

bridge_sync_cmds! {
    edit_pending_list() -> Vec<UiEditSyncPrompt>;
}

bridge_ref_async_cmds! {
    term_write(session_id: String, data_b64: String) -> ();
    term_resize(session_id: String, cols: u16, rows: u16) -> ();
    term_close(session_id: String) -> ();
}
