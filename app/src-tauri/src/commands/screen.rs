use super::*;

#[tauri::command]
pub(crate) async fn shot_monitors(
    state: State<'_, ManagedBridge>,
    device: String,
) -> Result<Reply<UiShotTargets>, ()> {
    Ok(state.inner().clone().shot_monitors(device).await)
}

#[tauri::command]
pub(crate) async fn shot_capture(
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
pub(crate) async fn input_click(
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
pub(crate) async fn input_key(
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
