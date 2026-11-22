use super::*;

bridge_sync_cmds! {
    self_info() -> SelfInfo;
    host_status() -> UiHostStatus;
    device_list() -> Vec<UiDevice>;
    lan_list() -> Vec<UiLanPeer>;
    pair_begin() -> UiPairOffer;
    pair_cancel() -> ();
    pair_pending_list() -> Vec<UiPairRequest>;
}

bridge_blocking_cmds! {
    "读取设置任务" => settings_get() -> UiSettings;
    "保存设置任务" => settings_set(settings: UiSettings) -> ();
    "切换被控状态任务" => host_set_paused(paused: bool) -> ();
    "重命名设备任务" => device_rename(id: String, name: String) -> ();
    "移除设备任务" => device_remove(id: String) -> ();
    "保存设备权限任务" => device_set_permissions(id: String, permissions: UiPermissions) -> ();
    "审批配对请求任务" => pair_respond(request_id: String, allow: bool, permissions: UiPermissions) -> ();
}

bridge_arc_async_cmds! {
    device_reconnect(id: String) -> ();
    pair_connect(target: String, code: String) -> UiDevice;
}
