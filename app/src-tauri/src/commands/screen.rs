use super::*;

bridge_arc_async_cmds! {
    shot_monitors(device: String) -> UiShotTargets;
    shot_capture(
        device: String,
        monitor_index: Option<u32>,
        hwnd: Option<String>,
        max_width: Option<u32>,
        quality: Option<u8>,
    ) -> UiCapturedShot;
    input_click(
        device: String,
        screen_x: i32,
        screen_y: i32,
        button: String,
        double: bool,
        target_hwnd: Option<String>,
        refresh_monitor: Option<u32>,
        refresh_hwnd: Option<String>,
        max_width: Option<u32>,
    ) -> UiInputOutcome;
    input_key(
        device: String,
        text: Option<String>,
        key_combo: Option<String>,
        target_hwnd: Option<String>,
        refresh_monitor: Option<u32>,
        refresh_hwnd: Option<String>,
        max_width: Option<u32>,
    ) -> UiInputOutcome;
}
