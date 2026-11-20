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
        // 请求已先存入 BridgeState 待处理队列；若主窗口原本不存在，前端冷启就绪后会通过 pair_pending_list 补显
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
        // 提示已先存入 BridgeState 待处理队列；若主窗口冷启尚未监听事件，前端就绪后会通过 edit_pending_list 补显
        create_or_focus_main_window(&self.app);
        let _ = self.app.emit("edit://prompt", ev);
    }

    fn emit_batch_item(&self, ev: UiBatchDeviceResult) {
        let _ = self.app.emit("batch://item", ev);
    }

    fn emit_system_notice(&self, ev: UiSystemNotice) {
        // 窗口缩在托盘或未聚焦时在阻塞线程池发送 WinRT 桌面通知，避免阻塞 Tokio 工作线程
        let window_focused = self
            .app
            .get_webview_window("main")
            .and_then(|w| w.is_focused().ok())
            .unwrap_or(false);
        if !window_focused && let Some(bridge) = self.app.try_state::<ManagedBridge>() {
            let plat = bridge.node().platform().clone();
            let title = ev.title.clone();
            let body = ev.body.clone();
            tauri::async_runtime::spawn_blocking(move || {
                let _ = plat.show_system_notification(&title, &body);
            });
        }
        let _ = self.app.emit("app://notice", ev);
    }
}
