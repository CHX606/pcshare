use super::*;

#[derive(Serialize, Clone)]
struct DeviceRemovedPayload {
    id: String,
}

#[derive(Serialize, Clone)]
struct HostPausedPayload {
    paused: bool,
}

pub(super) struct TauriEventSink {
    pub(super) app: AppHandle,
    pub(super) pause_menu_item: MutexSlot<CheckMenuItem<Wry>>,
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
        if !window_focused && let Some(bridge) = self.app.try_state::<ManagedBridge>() {
            let _ = bridge
                .node()
                .platform()
                .show_system_notification(&ev.title, &ev.body);
        }
        let _ = self.app.emit("app://notice", ev);
    }
}
