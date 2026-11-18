use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pcshare_app::bridge::{
    BridgeState, EventSink, Reply, SELF_ID, UiConflictPolicy, UiDevice, UiEditSyncPrompt,
    UiErrorCode, UiFsChangedEvent, UiFsListBatchEvent, UiFsSearchBatchEvent, UiLanPeer,
    UiPairRequest, UiPermissions, UiSettings, UiTaskProgress, UiTaskState, UiTermOutputEvent,
};
use pcshare_app::single_instance::SingleInstanceGuard;
use pcshare_core::{CoreConfig, CoreNode, NetworkMode};
use pcshare_platform::NativePlatform;

fn temp_dir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "pcshare_bridge_it_{}_{}_{}",
        tag,
        std::process::id(),
        fastrand::u64(..)
    ));
    std::fs::create_dir_all(&p).unwrap();
    p
}

#[derive(Default)]
struct RecordingSink {
    device_statuses: Mutex<Vec<UiDevice>>,
    removed_ids: Mutex<Vec<String>>,
    pair_requests: Mutex<Vec<UiPairRequest>>,
    task_events: Mutex<Vec<(Instant, UiTaskProgress)>>,
    list_batches: Mutex<Vec<UiFsListBatchEvent>>,
    fs_changes: Mutex<Vec<UiFsChangedEvent>>,
    lan_changes: Mutex<Vec<Vec<UiLanPeer>>>,
    host_paused: Mutex<Vec<bool>>,
    search_batches: Mutex<Vec<UiFsSearchBatchEvent>>,
    term_outputs: Mutex<Vec<UiTermOutputEvent>>,
    edit_prompts: Mutex<Vec<UiEditSyncPrompt>>,
}

impl EventSink for RecordingSink {
    fn emit_device_status(&self, device: UiDevice) {
        self.device_statuses.lock().unwrap().push(device);
    }

    fn emit_device_removed(&self, id: String) {
        self.removed_ids.lock().unwrap().push(id);
    }

    fn emit_pair_request(&self, req: UiPairRequest) {
        self.pair_requests.lock().unwrap().push(req);
    }

    fn emit_task_progress(&self, progress: UiTaskProgress) {
        self.task_events
            .lock()
            .unwrap()
            .push((Instant::now(), progress));
    }

    fn emit_fs_list_batch(&self, batch: UiFsListBatchEvent) {
        self.list_batches.lock().unwrap().push(batch);
    }

    fn emit_fs_changed(&self, ev: UiFsChangedEvent) {
        self.fs_changes.lock().unwrap().push(ev);
    }

    fn emit_lan_changed(&self, peers: Vec<UiLanPeer>) {
        self.lan_changes.lock().unwrap().push(peers);
    }

    fn emit_host_paused(&self, paused: bool) {
        self.host_paused.lock().unwrap().push(paused);
    }

    fn emit_fs_search_batch(&self, batch: UiFsSearchBatchEvent) {
        self.search_batches.lock().unwrap().push(batch);
    }

    fn emit_term_output(&self, ev: UiTermOutputEvent) {
        self.term_outputs.lock().unwrap().push(ev);
    }

    fn emit_edit_sync_prompt(&self, ev: UiEditSyncPrompt) {
        self.edit_prompts.lock().unwrap().push(ev);
    }
}

async fn spawn_bridge(tag: &str) -> (Arc<BridgeState>, Arc<RecordingSink>, PathBuf) {
    let dir = temp_dir(tag);
    let node = CoreNode::start(
        CoreConfig {
            data_dir: dir.clone(),
            network_mode: NetworkMode::LocalTest,
            device_name_override: Some(format!("界面节点-{tag}")),
            auto_connect_peers: true,
        },
        Arc::new(NativePlatform::new()),
    )
    .await
    .expect("start CoreNode");
    let sink = Arc::new(RecordingSink::default());
    let bridge = BridgeState::new(node, sink.clone());
    (bridge, sink, dir)
}

fn unwrap_ok<T: std::fmt::Debug>(r: Reply<T>) -> T {
    match r {
        Reply::Ok { data } => data,
        Reply::Err { err } => panic!("expected Reply::Ok, got Err({err:?})"),
    }
}

fn unwrap_err<T: std::fmt::Debug>(r: Reply<T>) -> UiErrorCode {
    match r {
        Reply::Ok { data } => panic!("expected Reply::Err, got Ok({data:?})"),
        Reply::Err { err } => err.code,
    }
}

#[test]
fn single_instance_guard_blocks_second_instance_and_triggers_wakeup() {
    let dir = temp_dir("single_inst");
    let wake_count = Arc::new(AtomicUsize::new(0));
    let wc = wake_count.clone();

    let primary = SingleInstanceGuard::try_acquire(&dir, move || {
        wc.fetch_add(1, Ordering::SeqCst);
    })
    .expect("primary instance must acquire guard");

    // Second instance with the same data_dir must return None and wake primary
    let secondary = SingleInstanceGuard::try_acquire(&dir, || {});
    assert!(secondary.is_none(), "second instance must be rejected");

    let deadline = Instant::now() + Duration::from_secs(2);
    while wake_count.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(wake_count.load(Ordering::SeqCst), 1);

    drop(primary);
    // After primary drops, a new instance can acquire again
    let reacquired = SingleInstanceGuard::try_acquire(&dir, || {});
    assert!(reacquired.is_some());
    drop(reacquired);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn bridge_commands_and_events_cover_full_stage1_contract() {
    let (host, host_sink, host_dir) = spawn_bridge("host").await;
    let (client, client_sink, client_dir) = spawn_bridge("client").await;
    let work_dir = temp_dir("work");

    // 1. self_info & settings_get / settings_set
    let info = unwrap_ok(client.self_info());
    assert_eq!(info.id.len(), 64);
    assert_eq!(info.name, "界面节点-client");
    assert_eq!(info.version, env!("CARGO_PKG_VERSION"));

    let cur_settings = unwrap_ok(client.settings_get());
    assert_eq!(cur_settings.device_name, "界面节点-client");
    unwrap_ok(client.settings_set(UiSettings {
        device_name: "书房控制机".to_string(),
        autostart: cur_settings.autostart,
        autostart_elevated: cur_settings.autostart_elevated,
        autostart_prompted: true,
        quick_hotkey: cur_settings.quick_hotkey,
        clipboard_auto_sync: false,
    }));
    assert_eq!(unwrap_ok(client.self_info()).name, "书房控制机");

    // 2. Pairing error codes: wrong code -> bad_code, 5th failure -> expired
    let offer1 = unwrap_ok(host.pair_begin());
    let addr_invite_bad = offer1.invite.replace(&format!("code={}", offer1.code), "code=000000");
    for i in 1..=4 {
        let code = unwrap_err(client.pair_connect(addr_invite_bad.clone(), String::new()).await);
        assert_eq!(code, UiErrorCode::BadCode, "attempt {i} should return bad_code");
    }
    let fifth = unwrap_err(client.pair_connect(addr_invite_bad, String::new()).await);
    assert_eq!(fifth, UiErrorCode::Expired, "5th wrong attempt must return expired");

    // 3. Valid pairing with `pair://request` event & `pair_respond`
    let offer2 = unwrap_ok(host.pair_begin());
    let host_clone = host.clone();
    let host_sink_clone = host_sink.clone();
    tokio::spawn(async move {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let req_opt = host_sink_clone.pair_requests.lock().unwrap().first().cloned();
            if let Some(req) = req_opt {
                unwrap_ok(host_clone.pair_respond(
                    req.request_id,
                    true,
                    UiPermissions::default_stage1(),
                ));
                break;
            }
            assert!(Instant::now() < deadline, "pair://request event did not arrive");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    });

    let paired_dev = unwrap_ok(client.pair_connect(offer2.invite, String::new()).await);
    let host_id = host.self_id();
    assert_eq!(paired_dev.id, host_id);
    assert_eq!(unwrap_ok(client.device_list()).len(), 1);
    assert_eq!(unwrap_ok(host.device_list()).len(), 1);

    // 4. Device rename, set_permissions, reconnect
    unwrap_ok(client.device_rename(host_id.clone(), "客厅电脑".to_string()));
    assert_eq!(unwrap_ok(client.device_list())[0].name, "客厅电脑");

    let mut custom_perms = UiPermissions::default_stage1();
    custom_perms.write_files = false;
    unwrap_ok(client.device_set_permissions(host_id.clone(), custom_perms));
    assert!(!unwrap_ok(client.device_list())[0].permissions.as_ref().unwrap().write_files);

    unwrap_ok(client.device_reconnect(host_id.clone()).await);

    // 5. Local (`self`) & remote (`host_id`) file operations
    let self_roots = unwrap_ok(client.fs_roots(SELF_ID.to_string()).await);
    assert!(!self_roots.is_empty());
    let remote_roots = unwrap_ok(client.fs_roots(host_id.clone()).await);
    assert!(!remote_roots.is_empty());

    let sub_dir = work_dir.join("新建测试夹");
    unwrap_ok(client.fs_mkdir(host_id.clone(), sub_dir.to_string_lossy().to_string()).await);

    let local_src = work_dir.join("本地源文件.bin");
    // 8 MB payload so transfer emits throttled progress updates
    let payload = vec![0x5Au8; 8 * 1024 * 1024];
    std::fs::write(&local_src, &payload).unwrap();

    // 6. Cross-device upload (`self` -> `host_id`) & progress throttle check
    let up_task_id = unwrap_ok(
        client
            .transfer_start(
                SELF_ID.to_string(),
                vec![local_src.to_string_lossy().to_string()],
                host_id.clone(),
                sub_dir.to_string_lossy().to_string(),
                UiConflictPolicy::Overwrite,
            )
            .await,
    );

    let wait_deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let done = client_sink
            .task_events
            .lock()
            .unwrap()
            .iter()
            .any(|(_, p)| p.id == up_task_id && p.state == UiTaskState::Done);
        if done {
            break;
        }
        assert!(Instant::now() < wait_deadline, "upload task did not finish");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }

    // Verify running progress events (excluding initial 0-byte registration & terminal state) respect ~100ms throttle
    {
        let evs: Vec<(Instant, UiTaskProgress)> = client_sink
            .task_events
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, p)| p.id == up_task_id && p.state == UiTaskState::Running && p.bytes_done > 0)
            .cloned()
            .collect();
        for pair in evs.windows(2) {
            let gap = pair[1].0.duration_since(pair[0].0);
            assert!(
                gap >= Duration::from_millis(85),
                "progress events emitted too frequently ({gap:?} < 100ms throttle)"
            );
        }
    }

    let listing = unwrap_ok(
        client
            .fs_list(host_id.clone(), sub_dir.to_string_lossy().to_string())
            .await,
    );
    assert_eq!(listing.entries.len(), 1);
    assert_eq!(listing.entries[0].name, "本地源文件.bin");

    // Rename & Copy on remote via bridge
    let uploaded_remote = sub_dir.join("本地源文件.bin");
    unwrap_ok(
        client
            .fs_rename(
                host_id.clone(),
                uploaded_remote.to_string_lossy().to_string(),
                "已改名.bin".to_string(),
            )
            .await,
    );

    let copy_dest = work_dir.join("远端复制目录");
    unwrap_ok(client.fs_mkdir(host_id.clone(), copy_dest.to_string_lossy().to_string()).await);
    let copy_tid = unwrap_ok(
        client
            .fs_copy(
                host_id.clone(),
                vec![sub_dir.join("已改名.bin").to_string_lossy().to_string()],
                copy_dest.to_string_lossy().to_string(),
            )
            .await,
    );
    let copy_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let done = client_sink
            .task_events
            .lock()
            .unwrap()
            .iter()
            .any(|(_, p)| p.id == copy_tid && p.state == UiTaskState::Done);
        if done {
            break;
        }
        assert!(Instant::now() < copy_deadline, "remote copy task did not finish");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert!(copy_dest.join("已改名.bin").exists());

    // Download (`host_id` -> `self`)
    let dl_dir = work_dir.join("本地下载目录");
    std::fs::create_dir_all(&dl_dir).unwrap();
    let dl_tid = unwrap_ok(
        client
            .transfer_start(
                host_id.clone(),
                vec![copy_dest.join("已改名.bin").to_string_lossy().to_string()],
                SELF_ID.to_string(),
                dl_dir.to_string_lossy().to_string(),
                UiConflictPolicy::Rename,
            )
            .await,
    );
    let dl_deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let done = client_sink
            .task_events
            .lock()
            .unwrap()
            .iter()
            .any(|(_, p)| p.id == dl_tid && p.state == UiTaskState::Done);
        if done {
            break;
        }
        assert!(Instant::now() < dl_deadline, "download task did not finish");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert_eq!(std::fs::read(dl_dir.join("已改名.bin")).unwrap().len(), payload.len());

    // 7. Apps & Processes on both `self` and remote
    let apps = unwrap_ok(client.app_list(host_id.clone()).await);
    assert!(!apps.is_empty());
    let missing_launch = unwrap_err(
        client
            .app_launch(
                host_id.clone(),
                r"C:\nonexistent_bridge_test_12345.exe".to_string(),
                None,
            )
            .await,
    );
    assert_eq!(missing_launch, UiErrorCode::NotFound);

    let procs = unwrap_ok(client.proc_list(host_id.clone()).await);
    assert!(!procs.is_empty());
    let crit_err = unwrap_err(client.proc_kill(host_id.clone(), 4).await);
    assert_eq!(crit_err, UiErrorCode::Forbidden);

    // 8. Delete & remove device
    unwrap_ok(
        client
            .fs_delete(
                host_id.clone(),
                vec![copy_dest.join("已改名.bin").to_string_lossy().to_string()],
                true,
            )
            .await,
    );
    assert!(!copy_dest.join("已改名.bin").exists());

    unwrap_ok(client.device_remove(host_id.clone()));
    assert!(client_sink.removed_ids.lock().unwrap().contains(&host_id));

    client.shutdown().await;
    host.shutdown().await;
    let _ = std::fs::remove_dir_all(work_dir);
    let _ = std::fs::remove_dir_all(host_dir);
    let _ = std::fs::remove_dir_all(client_dir);
}

#[tokio::test]
async fn bridge_stage2_cache_watch_batches_folder_transfer_pause_direct_p2p_icons_and_diag() {
    let (ctrl, ctrl_sink, ctrl_dir) = spawn_bridge("s2_ctrl").await;
    let (peer_a, a_sink, a_dir) = spawn_bridge("s2_a").await;
    let (peer_b, b_sink, b_dir) = spawn_bridge("s2_b").await;
    let work = temp_dir("s2_bridge_work");

    // Pair ctrl -> peer_a and ctrl -> peer_b
    for (peer, sink) in [(&peer_a, &a_sink), (&peer_b, &b_sink)] {
        let offer = unwrap_ok(peer.pair_begin());
        let p_clone = peer.clone();
        let s_clone = sink.clone();
        tokio::spawn(async move {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if let Some(req) = s_clone.pair_requests.lock().unwrap().first().cloned() {
                    unwrap_ok(p_clone.pair_respond(
                        req.request_id,
                        true,
                        UiPermissions::default_stage1(),
                    ));
                    break;
                }
                assert!(Instant::now() < deadline);
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });
        unwrap_ok(ctrl.pair_connect(offer.invite, String::new()).await);
    }

    let a_id = peer_a.self_id();
    let b_id = peer_b.self_id();

    // 1. Progressive large directory batches (`fs://list-batch`) + `< 16ms` `fs_cache_get`
    let large_dir = work.join("large_700");
    std::fs::create_dir_all(&large_dir).unwrap();
    for i in 0..700 {
        std::fs::write(large_dir.join(format!("item_{i:03}.txt")), b"123").unwrap();
    }
    let large_path = large_dir.to_string_lossy().to_string();
    let listed = unwrap_ok(ctrl.fs_list(a_id.clone(), large_path.clone()).await);
    assert_eq!(listed.entries.len(), 700);
    assert!(
        !ctrl_sink.list_batches.lock().unwrap().is_empty(),
        "fs_list > 500 items must emit progressive fs://list-batch event"
    );

    let t_cache = Instant::now();
    let cached = unwrap_ok(ctrl.fs_cache_get(a_id.clone(), large_path.clone())).expect("cache hit");
    assert!(t_cache.elapsed() < Duration::from_millis(16));
    assert_eq!(cached.entries.len(), 700);

    // 2. Directory watch (`fs://changed` within 1s) & conflict check
    tokio::time::sleep(Duration::from_millis(200)).await;
    unwrap_ok(ctrl.fs_watch(a_id.clone(), large_path.clone()).await);
    std::fs::write(large_dir.join("added_live.txt"), b"live").unwrap();
    let watch_deadline = Instant::now() + Duration::from_secs(1);
    loop {
        if ctrl_sink
            .fs_changes
            .lock()
            .unwrap()
            .iter()
            .any(|ev| ev.device == a_id)
        {
            break;
        }
        assert!(Instant::now() < watch_deadline, "fs://changed did not arrive within 1s");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    unwrap_ok(ctrl.fs_unwatch(a_id.clone(), large_path.clone()).await);

    let conflicts = unwrap_ok(
        ctrl.fs_check_conflicts(
            a_id.clone(),
            large_path.clone(),
            vec!["item_001.txt".to_string(), "never_seen.txt".to_string()],
        )
        .await,
    );
    assert_eq!(conflicts, vec!["item_001.txt".to_string()]);

    // 3. Recursive folder upload + pause/resume + speed limit + direct peer-to-peer A -> B transfer!
    unwrap_ok(ctrl.transfer_set_speed_limit(Some(50 * 1024 * 1024)));
    assert_eq!(unwrap_ok(ctrl.transfer_get_speed_limit()), Some(50 * 1024 * 1024));
    unwrap_ok(ctrl.transfer_set_speed_limit(None));

    let local_folder = work.join("folder_to_upload");
    std::fs::create_dir_all(local_folder.join("nested")).unwrap();
    std::fs::write(local_folder.join("nested").join("doc.txt"), b"folder payload").unwrap();

    let a_dest = work.join("a_dest");
    std::fs::create_dir_all(&a_dest).unwrap();
    let folder_tid = unwrap_ok(
        ctrl.transfer_start(
            SELF_ID.to_string(),
            vec![local_folder.to_string_lossy().to_string()],
            a_id.clone(),
            a_dest.to_string_lossy().to_string(),
            UiConflictPolicy::Overwrite,
        )
        .await,
    );
    unwrap_ok(ctrl.task_pause(folder_tid.clone()));
    unwrap_ok(ctrl.task_resume(folder_tid.clone()));

    let folder_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if ctrl_sink
            .task_events
            .lock()
            .unwrap()
            .iter()
            .any(|(_, p)| p.id == folder_tid && p.state == UiTaskState::Done)
        {
            break;
        }
        assert!(Instant::now() < folder_deadline, "folder upload did not finish");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert_eq!(
        std::fs::read(a_dest.join("folder_to_upload").join("nested").join("doc.txt")).unwrap(),
        b"folder payload"
    );

    // Direct peer-to-peer transfer (`a_id` -> `b_id`)
    let b_dest = work.join("b_dest");
    std::fs::create_dir_all(&b_dest).unwrap();
    let p2p_tid = unwrap_ok(
        ctrl.transfer_start(
            a_id.clone(),
            vec![a_dest.join("folder_to_upload").to_string_lossy().to_string()],
            b_id.clone(),
            b_dest.to_string_lossy().to_string(),
            UiConflictPolicy::Overwrite,
        )
        .await,
    );
    let p2p_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if ctrl_sink
            .task_events
            .lock()
            .unwrap()
            .iter()
            .any(|(_, p)| p.id == p2p_tid && p.state == UiTaskState::Done)
        {
            break;
        }
        assert!(Instant::now() < p2p_deadline, "direct P2P folder transfer did not finish");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert_eq!(
        std::fs::read(b_dest.join("folder_to_upload").join("nested").join("doc.txt")).unwrap(),
        b"folder payload"
    );

    // 4. Host pause mode, app_icon, LAN list, and Diagnostics summary + export
    unwrap_ok(peer_a.host_set_paused(true));
    assert!(unwrap_ok(peer_a.host_status()).paused);
    assert_eq!(
        unwrap_err(ctrl.fs_roots(a_id.clone()).await),
        UiErrorCode::Busy
    );
    unwrap_ok(peer_a.host_set_paused(false));

    let apps = unwrap_ok(ctrl.app_list(SELF_ID.to_string()).await);
    if let Some(first_app) = apps.first() {
        let _ = unwrap_ok(ctrl.app_icon(SELF_ID.to_string(), first_app.id.clone()).await);
    }

    let _ = unwrap_ok(ctrl.lan_list());
    let summary = unwrap_ok(ctrl.diag_summary());
    assert_eq!(summary.app_version, env!("CARGO_PKG_VERSION"));
    assert!(summary.working_set_bytes > 0);
    let logs = unwrap_ok(ctrl.diag_logs(50));
    assert!(!logs.runtime_logs.is_empty());

    let diag_out = work.join("diag_out");
    std::fs::create_dir_all(&diag_out).unwrap();
    let bundle_path = unwrap_ok(ctrl.diag_export(diag_out.to_string_lossy().to_string()).await);
    assert!(PathBuf::from(&bundle_path).join("summary.json").exists());

    ctrl.shutdown().await;
    peer_a.shutdown().await;
    peer_b.shutdown().await;
    let _ = std::fs::remove_dir_all(work);
    let _ = std::fs::remove_dir_all(ctrl_dir);
    let _ = std::fs::remove_dir_all(a_dir);
    let _ = std::fs::remove_dir_all(b_dir);
}

#[tokio::test]
async fn bridge_stage3_search_shot_term_power_edit_quick_and_batch() {
    use base64::Engine as _;

    let (ctrl, ctrl_sink, ctrl_dir) = spawn_bridge("s3_ctrl").await;
    let (host, host_sink, host_dir) = spawn_bridge("s3_host").await;
    host.node().set_dry_run_power_actions(true);
    let work = temp_dir("s3_bridge_work");
    std::fs::write(work.join("财务报表2026.txt"), "初始报表").unwrap();

    // Pair with full Stage 3 permissions granted on host
    let offer = unwrap_ok(host.pair_begin());
    let h_clone = host.clone();
    let hs_clone = host_sink.clone();
    tokio::spawn(async move {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(req) = hs_clone.pair_requests.lock().unwrap().first().cloned() {
                let mut perms = UiPermissions::default_stage1();
                perms.screenshot = true;
                perms.input = true;
                perms.terminal = true;
                perms.power = true;
                perms.clipboard = true;
                unwrap_ok(h_clone.pair_respond(req.request_id, true, perms));
                break;
            }
            assert!(Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });
    let paired = unwrap_ok(ctrl.pair_connect(offer.invite, String::new()).await);
    let host_id = paired.id;
    assert!(paired.features.contains(&"shot".to_string()));
    assert!(paired.features.contains(&"term".to_string()));
    assert!(paired.features.contains(&"search".to_string()));

    // 1. Progressive file search via bridge (`fs_search_start` -> `fs://search-batch`)
    let search_id = unwrap_ok(
        ctrl.fs_search_start(
            host_id.clone(),
            work.to_string_lossy().to_string(),
            "cwbb".to_string(),
        )
        .await,
    );
    let search_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let done = ctrl_sink
            .search_batches
            .lock()
            .unwrap()
            .iter()
            .any(|b| b.search_id == search_id && b.done);
        if done {
            break;
        }
        assert!(Instant::now() < search_deadline, "fs_search did not complete");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let found_names: Vec<String> = ctrl_sink
        .search_batches
        .lock()
        .unwrap()
        .iter()
        .filter(|b| b.search_id == search_id)
        .flat_map(|b| b.entries.iter().map(|e| e.name.clone()))
        .collect();
    assert!(found_names.iter().any(|n| n.contains("财务报表2026")));

    // 2. Remote terminal via bridge (`term_open` / `term_write` / `term_resize` / `term_close`)
    let term_sess = unwrap_ok(
        ctrl.term_open(host_id.clone(), "cmd".to_string(), 100, 28, None)
            .await,
    );
    unwrap_ok(ctrl.term_resize(term_sess.session_id.clone(), 120, 34).await);
    let cmd_b64 =
        base64::engine::general_purpose::STANDARD.encode(b"echo BRIDGE_PTY_OK\r\nexit\r\n");
    unwrap_ok(ctrl.term_write(term_sess.session_id.clone(), cmd_b64).await);

    let term_deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let combined: Vec<u8> = ctrl_sink
            .term_outputs
            .lock()
            .unwrap()
            .iter()
            .filter(|o| o.session_id == term_sess.session_id)
            .filter_map(|o| base64::engine::general_purpose::STANDARD.decode(&o.data_b64).ok())
            .flatten()
            .collect();
        if String::from_utf8_lossy(&combined).contains("BRIDGE_PTY_OK") {
            break;
        }
        assert!(Instant::now() < term_deadline, "term://output did not arrive");
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    unwrap_ok(ctrl.term_close(term_sess.session_id).await);

    // 3. Remote file local edit & save-back (`fs_open_local_edit` & `fs_edit_sync_back`)
    let remote_file = work.join("财务报表2026.txt").to_string_lossy().to_string();
    let edit_sess = unwrap_ok(
        ctrl.fs_open_local_edit(host_id.clone(), remote_file.clone(), Some(false))
            .await,
    );
    std::fs::write(&edit_sess.local_path, "通过桥接修改后的报表").unwrap();
    let sync_res = unwrap_ok(
        ctrl.fs_edit_sync_back(edit_sess.session_id, false, false)
            .await,
    );
    assert!(!sync_res.conflict);
    assert_eq!(
        std::fs::read_to_string(work.join("财务报表2026.txt")).unwrap(),
        "通过桥接修改后的报表"
    );

    // 4. Quick search + Batch execution (`quick_search` + `batch_exec`)
    let _ = unwrap_ok(ctrl.fs_list(host_id.clone(), work.to_string_lossy().to_string()).await);
    let q_hits = unwrap_ok(ctrl.quick_search("s3_bridge_work".to_string(), Some(10)));
    assert!(!q_hits.is_empty());

    let batch_res = unwrap_ok(
        ctrl.batch_exec(
            vec![host_id.clone()],
            "run_command".to_string(),
            None,
            None,
            Some("echo BRIDGE_BATCH_777".to_string()),
            Some("cmd".to_string()),
        )
        .await,
    );
    assert_eq!(batch_res.len(), 1);
    assert!(batch_res[0].ok);
    assert!(batch_res[0].output.as_deref().unwrap_or_default().contains("BRIDGE_BATCH_777"));

    // 5. Power cancel safety check
    let ctrl_c = ctrl.clone();
    let hid_c = host_id.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let _ = ctrl_c.power_cancel(hid_c).await;
    });
    let pow_res = unwrap_ok(
        ctrl.power_exec(host_id.clone(), "shutdown".to_string(), Some(4))
            .await,
    );
    assert!(pow_res.cancelled);

    ctrl.shutdown().await;
    host.shutdown().await;
    let _ = std::fs::remove_dir_all(work);
    let _ = std::fs::remove_dir_all(ctrl_dir);
    let _ = std::fs::remove_dir_all(host_dir);
}
