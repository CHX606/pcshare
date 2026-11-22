use super::*;

bridge_sync_cmds! {
    fs_cache_get(device: String, path: String) -> Option<UiFsCacheHit>;
    task_list() -> Vec<UiTaskProgress>;
    task_pause(task_id: String) -> ();
    task_resume(task_id: String) -> ();
    transfer_get_speed_limit() -> Option<u64>;
    transfer_set_speed_limit(bytes_per_sec: Option<u64>) -> ();
}

bridge_arc_async_cmds! {
    fs_roots(device: String) -> Vec<UiFsRoot>;
    fs_list(device: String, path: String) -> UiFsListing;
    fs_watch(device: String, path: String) -> usize;
    fs_unwatch(device: String, path: String) -> usize;
    fs_check_conflicts(device: String, dest_dir: String, names: Vec<String>) -> Vec<String>;
    fs_mkdir(device: String, path: String) -> ();
    fs_rename(device: String, path: String, new_name: String) -> ();
    fs_copy(device: String, sources: Vec<String>, dest_dir: String) -> String;
    fs_move(device: String, sources: Vec<String>, dest_dir: String) -> String;
    fs_delete(device: String, paths: Vec<String>, permanent: bool) -> ();
    transfer_start(
        src: String,
        sources: Vec<String>,
        dst: String,
        dest_dir: String,
        conflict: UiConflictPolicy,
    ) -> String;
    task_cancel(task_id: String) -> ();
}

bridge_ref_async_cmds! {
    pick_local_files() -> Vec<String>;
    pick_local_dir() -> Option<String>;
}
