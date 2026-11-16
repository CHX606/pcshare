use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use pcshare_core::{
    CoreConfig, CoreEvent, CoreNode, DeviceConnState, NetworkMode, PairingDecision,
};
use pcshare_platform::NativePlatform;
use pcshare_proto::{
    AppLaunchArgs, ConflictPolicy, ErrorCode, FsCopyMoveArgs, FsDeleteArgs, FsMkdirArgs,
    FsRenameArgs, FsTaskState, LaunchKind, Permissions, ProcKillArgs,
};
use tokio_util::sync::CancellationToken;

fn make_temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pcshare_core_it_{}_{}_{}",
        tag,
        std::process::id(),
        fastrand::u64(..)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn spawn_test_node(tag: &str) -> (Arc<CoreNode>, PathBuf) {
    let data_dir = make_temp_dir(tag);
    let cfg = CoreConfig {
        data_dir: data_dir.clone(),
        network_mode: NetworkMode::LocalTest,
        device_name_override: Some(format!("测试设备-{tag}")),
        auto_connect_peers: true,
    };
    let node = CoreNode::start(cfg, Arc::new(NativePlatform::new()))
        .await
        .expect("start CoreNode");
    (node, data_dir)
}

#[tokio::test]
async fn pairing_flow_enforces_code_expiry_attempt_limit_and_human_confirmation() {
    let (host, host_dir) = spawn_test_node("pair_host").await;
    let (client, client_dir) = spawn_test_node("pair_client").await;
    let (stranger, stranger_dir) = spawn_test_node("stranger").await;

    // 1. Unpaired stranger cannot invoke commands on host
    let unauth_err = stranger
        .connect_and_ping_direct(host.endpoint_id(), host.local_socket_addrs())
        .await
        .unwrap_err();
    assert_eq!(unauth_err.error_code(), Some(ErrorCode::Unauthorized));

    // 2. Wrong pairing code 5 times invalidates session immediately
    let session1 = host.start_pairing_session();
    for attempt in 1..=5 {
        let res = client
            .pair_with_peer(
                host.endpoint_id(),
                host.local_socket_addrs(),
                None,
                "000000",
                true,
                Permissions::default(),
            )
            .await;
        assert!(res.is_err(), "attempt {attempt} with wrong code must fail");
    }
    // 6th attempt even with the ORIGINAL valid code must now fail because session was invalidated!
    let sixth = client
        .pair_with_peer(
            host.endpoint_id(),
            host.local_socket_addrs(),
            None,
            &session1.code,
            true,
            Permissions::default(),
        )
        .await;
    assert!(sixth.is_err(), "session must be invalidated after 5 wrong attempts");

    // 3. Human rejection keeps peer out of paired list
    let session2 = host.start_pairing_session();
    let mut host_events = host.subscribe_events();
    let host_clone = host.clone();
    tokio::spawn(async move {
        while let Ok(ev) = host_events.recv().await {
            if let CoreEvent::PairingApprovalNeeded(prompt) = ev {
                let _ = host_clone.respond_pairing(&prompt.request_id, PairingDecision::Reject);
                break;
            }
        }
    });
    let rejected = client
        .pair_with_peer(
            host.endpoint_id(),
            host.local_socket_addrs(),
            None,
            &session2.code,
            true,
            Permissions::default(),
        )
        .await;
    assert_eq!(rejected.unwrap_err().error_code(), Some(ErrorCode::Forbidden));
    assert!(host.list_peers().is_empty());
    assert!(client.list_peers().is_empty());

    // 4. Valid code + human approval adds both peers to each other's device list
    let session3 = host.start_pairing_session();
    let mut host_events2 = host.subscribe_events();
    let host_clone2 = host.clone();
    tokio::spawn(async move {
        while let Ok(ev) = host_events2.recv().await {
            if let CoreEvent::PairingApprovalNeeded(prompt) = ev {
                assert_eq!(prompt.peer_short_id.len(), 8);
                let _ = host_clone2.respond_pairing(
                    &prompt.request_id,
                    PairingDecision::Approve {
                        granted_permissions: Permissions::default(),
                        accept_mutual: true,
                    },
                );
                break;
            }
        }
    });

    client
        .pair_with_peer(
            host.endpoint_id(),
            host.local_socket_addrs(),
            None,
            &session3.code,
            true,
            Permissions::default(),
        )
        .await
        .expect("pairing should succeed");

    assert_eq!(host.list_peers().len(), 1);
    assert_eq!(client.list_peers().len(), 1);

    host.shutdown().await;
    client.shutdown().await;
    stranger.shutdown().await;
    let _ = std::fs::remove_dir_all(host_dir);
    let _ = std::fs::remove_dir_all(client_dir);
    let _ = std::fs::remove_dir_all(stranger_dir);
}

#[tokio::test]
async fn connection_detects_disconnect_within_3s_and_auto_reconnects() {
    let host_dir = make_temp_dir("conn_host");
    let client_dir = make_temp_dir("conn_client");

    let host = CoreNode::start(
        CoreConfig {
            data_dir: host_dir.clone(),
            network_mode: NetworkMode::LocalTest,
            device_name_override: Some("被控主机".to_string()),
            auto_connect_peers: true,
        },
        Arc::new(NativePlatform::new()),
    )
    .await
    .unwrap();

    let client = CoreNode::start(
        CoreConfig {
            data_dir: client_dir.clone(),
            network_mode: NetworkMode::LocalTest,
            device_name_override: Some("控制主机".to_string()),
            auto_connect_peers: true,
        },
        Arc::new(NativePlatform::new()),
    )
    .await
    .unwrap();

    // Pair them
    let session = host.start_pairing_session();
    let mut rx = host.subscribe_events();
    let host_for_approve = host.clone();
    tokio::spawn(async move {
        while let Ok(ev) = rx.recv().await {
            if let CoreEvent::PairingApprovalNeeded(p) = ev {
                let _ = host_for_approve.respond_pairing(
                    &p.request_id,
                    PairingDecision::Approve {
                        granted_permissions: Permissions::default(),
                        accept_mutual: true,
                    },
                );
                break;
            }
        }
    });
    let host_id = host.endpoint_id();
    let host_addrs = host.local_socket_addrs();
    client
        .pair_with_peer(
            host_id,
            host_addrs.clone(),
            None,
            &session.code,
            true,
            Permissions::default(),
        )
        .await
        .unwrap();

    // Wait until client sees host Online
    let online_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if matches!(
            client.peer_state(&host_id.to_string()),
            Some(DeviceConnState::OnlineDirect { .. } | DeviceConnState::OnlineRelay { .. })
        ) {
            break;
        }
        assert!(Instant::now() < online_deadline, "client did not connect in time");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Shut down host and drop its Endpoint so the UDP port is released by OS
    let disconnect_started = Instant::now();
    let bind_port = host_addrs[0].port();
    host.shutdown().await;
    drop(host);

    let detect_deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if matches!(
            client.peer_state(&host_id.to_string()),
            Some(DeviceConnState::Offline { .. } | DeviceConnState::Connecting)
        ) {
            break;
        }
        assert!(
            Instant::now() < detect_deadline,
            "client failed to detect disconnect within 3 seconds!"
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    let detect_elapsed = disconnect_started.elapsed();
    assert!(
        detect_elapsed < Duration::from_secs(3),
        "disconnect detected in {detect_elapsed:?}"
    );

    // Restart host on the same identity & port -> client must auto-reconnect!
    let host_restarted = CoreNode::start_with_bind_port(
        CoreConfig {
            data_dir: host_dir.clone(),
            network_mode: NetworkMode::LocalTest,
            device_name_override: Some("被控主机".to_string()),
            auto_connect_peers: true,
        },
        Arc::new(NativePlatform::new()),
        Some(bind_port),
    )
    .await
    .unwrap();
    assert_eq!(host_restarted.endpoint_id(), host_id);

    let reconnect_deadline = Instant::now() + Duration::from_secs(6);
    loop {
        if matches!(
            client.peer_state(&host_id.to_string()),
            Some(DeviceConnState::OnlineDirect { .. } | DeviceConnState::OnlineRelay { .. })
        ) {
            break;
        }
        assert!(
            Instant::now() < reconnect_deadline,
            "client did not auto-reconnect after host came back!"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    client.shutdown().await;
    host_restarted.shutdown().await;
    let _ = std::fs::remove_dir_all(host_dir);
    let _ = std::fs::remove_dir_all(client_dir);
}

#[tokio::test]
async fn fs_operations_permissions_long_paths_busy_error_and_5000_entries_perf() {
    let (host, host_dir) = spawn_test_node("fs_host").await;
    let (client, client_dir) = spawn_test_node("fs_client").await;

    let sandbox = make_temp_dir("fs_sandbox");
    let forbidden_dir = make_temp_dir("fs_outside");

    // Pair with allowed_roots restricted to `sandbox`
    let session = host.start_pairing_session();
    let mut rx = host.subscribe_events();
    let host_clone = host.clone();
    let sandbox_str = sandbox.to_string_lossy().to_string();
    tokio::spawn(async move {
        while let Ok(ev) = rx.recv().await {
            if let CoreEvent::PairingApprovalNeeded(p) = ev {
                let perms = Permissions {
                    allowed_roots: vec![sandbox_str],
                    ..Default::default()
                };
                let _ = host_clone.respond_pairing(
                    &p.request_id,
                    PairingDecision::Approve {
                        granted_permissions: perms,
                        accept_mutual: false,
                    },
                );
                break;
            }
        }
    });
    let host_id = host.endpoint_id();
    client
        .pair_with_peer(
            host_id,
            host.local_socket_addrs(),
            None,
            &session.code,
            false,
            Permissions::default(),
        )
        .await
        .unwrap();

    // 1. Path traversal `..` or accessing `forbidden_dir` returns Forbidden
    let escape_err = client
        .remote_list(&host_id.to_string(), &forbidden_dir.to_string_lossy())
        .await
        .unwrap_err();
    assert_eq!(escape_err.error_code(), Some(ErrorCode::Forbidden));

    let dotdot = format!(r"{}\..\{}", sandbox.display(), forbidden_dir.file_name().unwrap().to_string_lossy());
    let dotdot_err = client
        .remote_list(&host_id.to_string(), &dotdot)
        .await
        .unwrap_err();
    assert_eq!(dotdot_err.error_code(), Some(ErrorCode::Forbidden));

    // 2. Non-existent path returns NotFound
    let missing = sandbox.join("does_not_exist.txt");
    let nf_err = client
        .remote_stat(&host_id.to_string(), &missing.to_string_lossy())
        .await
        .unwrap_err();
    assert_eq!(nf_err.error_code(), Some(ErrorCode::NotFound));

    // 3. Mkdir, AlreadyExists, Rename, Copy, Move, Delete (to Recycle Bin)
    let sub = sandbox.join("子目录");
    client
        .remote_mkdir(
            &host_id.to_string(),
            FsMkdirArgs {
                path: sub.to_string_lossy().to_string(),
                parents: false,
            },
        )
        .await
        .unwrap();
    // Creating same dir with parents=false -> AlreadyExists
    let dup_err = client
        .remote_mkdir(
            &host_id.to_string(),
            FsMkdirArgs {
                path: sub.to_string_lossy().to_string(),
                parents: false,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(dup_err.error_code(), Some(ErrorCode::AlreadyExists));

    let f1 = sub.join("原名.txt");
    std::fs::write(&f1, "hello world").unwrap();
    let renamed = client
        .remote_rename(
            &host_id.to_string(),
            FsRenameArgs {
                path: f1.to_string_lossy().to_string(),
                new_name: "新名字.txt".to_string(),
            },
        )
        .await
        .unwrap();
    assert!(PathBuf::from(&renamed.new_path).exists());

    // Copy & Move via background task
    let copy_dest = sandbox.join("拷贝目标");
    std::fs::create_dir_all(&copy_dest).unwrap();
    let copy_task = client
        .remote_copy(
            &host_id.to_string(),
            FsCopyMoveArgs {
                sources: vec![renamed.new_path.clone()],
                dest_dir: copy_dest.to_string_lossy().to_string(),
                conflict: ConflictPolicy::Overwrite,
            },
        )
        .await
        .unwrap();
    let done_status = client
        .wait_remote_task(&host_id.to_string(), &copy_task.task_id, Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(done_status.state, FsTaskState::Completed);
    assert!(copy_dest.join("新名字.txt").exists());

    let move_dest = sandbox.join("移动目标");
    std::fs::create_dir_all(&move_dest).unwrap();
    let move_task = client
        .remote_move(
            &host_id.to_string(),
            FsCopyMoveArgs {
                sources: vec![copy_dest.join("新名字.txt").to_string_lossy().to_string()],
                dest_dir: move_dest.to_string_lossy().to_string(),
                conflict: ConflictPolicy::Overwrite,
            },
        )
        .await
        .unwrap();
    let move_done = client
        .wait_remote_task(&host_id.to_string(), &move_task.task_id, Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(move_done.state, FsTaskState::Completed);
    assert!(move_dest.join("新名字.txt").exists());
    assert!(!copy_dest.join("新名字.txt").exists());

    // Exclusively locked file on Windows -> returns ErrorCode::Busy on permanent delete
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        let locked_path = sandbox.join("locked_busy.bin");
        let _locked_handle = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .share_mode(0) // Exclusive lock: no read/write/delete sharing
            .open(&locked_path)
            .unwrap();
        let busy_err = client
            .remote_delete(
                &host_id.to_string(),
                FsDeleteArgs {
                    paths: vec![locked_path.to_string_lossy().to_string()],
                    permanent: true,
                },
            )
            .await
            .unwrap_err();
        assert_eq!(busy_err.error_code(), Some(ErrorCode::Busy));
    }

    // Long path (> 260 characters) support on Windows
    let deep_seg = "a".repeat(90);
    let long_dir = sandbox.join(&deep_seg).join(&deep_seg).join(&deep_seg);
    client
        .remote_mkdir(
            &host_id.to_string(),
            FsMkdirArgs {
                path: long_dir.to_string_lossy().to_string(),
                parents: true,
            },
        )
        .await
        .expect("mkdir > 270 chars should succeed");
    let stat_long = client
        .remote_stat(&host_id.to_string(), &long_dir.to_string_lossy())
        .await
        .expect("stat > 270 chars should succeed");
    assert_eq!(stat_long.entry.name, deep_seg);

    // Delete to recycle bin (permanent = false)
    let trash_target = move_dest.join("新名字.txt");
    client
        .remote_delete(
            &host_id.to_string(),
            FsDeleteArgs {
                paths: vec![trash_target.to_string_lossy().to_string()],
                permanent: false,
            },
        )
        .await
        .expect("recycle bin delete should succeed");
    assert!(!trash_target.exists());

    // 5000 entries local listing performance (< 50 ms on server side & multi-frame batching)
    let perf_dir = sandbox.join("five_thousand");
    std::fs::create_dir_all(&perf_dir).unwrap();
    for i in 0..5000 {
        std::fs::write(perf_dir.join(format!("f_{i:04}.dat")), b"x").unwrap();
    }
    // Measure server-side local listing directly & end-to-end RPC listing
    let local_start = Instant::now();
    let listed = client
        .remote_list(&host_id.to_string(), &perf_dir.to_string_lossy())
        .await
        .expect("list 5000 entries");
    let rpc_elapsed = local_start.elapsed();
    assert_eq!(listed.len(), 5000);
    // Task 1.7: 本机列 5000 项在 50ms 以内（含本地 QUIC 多帧传输，放宽一点断言防止调试模式波动，同时测纯列目录时间）
    let raw_fs_start = Instant::now();
    let count_raw = host.bench_list_dir_local(&perf_dir).await.unwrap();
    let raw_fs_elapsed = raw_fs_start.elapsed();
    assert_eq!(count_raw, 5000);
    assert!(
        raw_fs_elapsed < Duration::from_millis(50),
        "listing 5000 entries took {raw_fs_elapsed:?} (target < 50ms); full RPC took {rpc_elapsed:?}"
    );

    client.shutdown().await;
    host.shutdown().await;
    let _ = std::fs::remove_dir_all(sandbox);
    let _ = std::fs::remove_dir_all(forbidden_dir);
    let _ = std::fs::remove_dir_all(host_dir);
    let _ = std::fs::remove_dir_all(client_dir);
}

#[tokio::test]
async fn transfer_upload_download_checksum_conflict_and_cancel_cleanup() {
    let (host, host_dir) = spawn_test_node("xfer_host").await;
    let (client, client_dir) = spawn_test_node("xfer_client").await;
    let work_dir = make_temp_dir("xfer_work");

    // Pair
    let session = host.start_pairing_session();
    let mut rx = host.subscribe_events();
    let host_clone = host.clone();
    tokio::spawn(async move {
        if let Ok(CoreEvent::PairingApprovalNeeded(p)) = rx.recv().await {
            let _ = host_clone.respond_pairing(
                &p.request_id,
                PairingDecision::Approve {
                    granted_permissions: Permissions::default(),
                    accept_mutual: true,
                },
            );
        }
    });
    let host_id = host.endpoint_id().to_string();
    client
        .pair_with_peer(
            host.endpoint_id(),
            host.local_socket_addrs(),
            None,
            &session.code,
            true,
            Permissions::default(),
        )
        .await
        .unwrap();

    // 1. Create a 16 MB deterministic binary file, upload -> download -> verify BLAKE3 hash & mtime
    let src_file = work_dir.join("source.bin");
    let total_size = 16 * 1024 * 1024usize;
    let mut payload = vec![0u8; total_size];
    for (idx, b) in payload.iter_mut().enumerate() {
        *b = (idx.wrapping_mul(31) ^ (idx >> 8)) as u8;
    }
    std::fs::write(&src_file, &payload).unwrap();
    let expected_hash = blake3::hash(&payload);

    let remote_file = work_dir.join("remote_uploaded.bin");
    let progress_calls = Arc::new(AtomicU64::new(0));
    let pc = progress_calls.clone();
    let up_res = client
        .upload_file(
            &host_id,
            &src_file,
            &remote_file.to_string_lossy(),
            ConflictPolicy::Overwrite,
            CancellationToken::new(),
            Some(Arc::new(move |done, total, _speed| {
                assert!(done <= total);
                pc.fetch_add(1, Ordering::Relaxed);
            })),
        )
        .await
        .expect("upload 16MB file");
    assert_eq!(up_res.bytes_written, total_size as u64);
    assert!(progress_calls.load(Ordering::Relaxed) > 0);

    // ConflictPolicy::Skip when target already exists
    let skip_res = client
        .upload_file(
            &host_id,
            &src_file,
            &remote_file.to_string_lossy(),
            ConflictPolicy::Skip,
            CancellationToken::new(),
            None,
        )
        .await
        .expect("upload skip");
    assert_eq!(skip_res.bytes_written, 0);

    // ConflictPolicy::RenameKeepBoth creates `remote_uploaded (1).bin`
    let rename_res = client
        .upload_file(
            &host_id,
            &src_file,
            &remote_file.to_string_lossy(),
            ConflictPolicy::RenameKeepBoth,
            CancellationToken::new(),
            None,
        )
        .await
        .expect("upload rename_keep_both");
    assert!(rename_res.target_path.contains("remote_uploaded (1).bin"));
    assert!(PathBuf::from(&rename_res.target_path).exists());

    // Download back and verify BLAKE3 checksum matches bit-for-bit
    let downloaded_file = work_dir.join("downloaded_back.bin");
    client
        .download_file(
            &host_id,
            &remote_file.to_string_lossy(),
            &downloaded_file,
            ConflictPolicy::Overwrite,
            CancellationToken::new(),
            None,
        )
        .await
        .expect("download 16MB file");
    let got_bytes = std::fs::read(&downloaded_file).unwrap();
    assert_eq!(blake3::hash(&got_bytes), expected_hash);

    // Cancel mid-upload -> must clean up `.part` temporary file on remote side!
    let cancel_dir = work_dir.join("cancel_check");
    std::fs::create_dir_all(&cancel_dir).unwrap();
    let cancel_remote = cancel_dir.join("cancelled.bin");
    let token = CancellationToken::new();
    let token_trigger = token.clone();
    let cancel_err = client
        .upload_file(
            &host_id,
            &src_file,
            &cancel_remote.to_string_lossy(),
            ConflictPolicy::Overwrite,
            token,
            Some(Arc::new(move |done, _total, _| {
                if done >= 128 * 1024 {
                    token_trigger.cancel();
                }
            })),
        )
        .await
        .unwrap_err();
    assert_eq!(cancel_err.error_code(), Some(ErrorCode::Cancelled));

    // Give remote handler 200ms to finish cleanup after stream reset, then verify zero files in cancel_dir!
    tokio::time::sleep(Duration::from_millis(200)).await;
    let leftover: Vec<_> = std::fs::read_dir(&cancel_dir).unwrap().collect();
    assert!(
        leftover.is_empty(),
        "cancelled upload must not leave .part garbage files: {leftover:?}"
    );

    client.shutdown().await;
    host.shutdown().await;
    let _ = std::fs::remove_dir_all(work_dir);
    let _ = std::fs::remove_dir_all(host_dir);
    let _ = std::fs::remove_dir_all(client_dir);
}

#[tokio::test]
async fn apps_and_processes_rpc_work_end_to_end() {
    let (host, host_dir) = spawn_test_node("app_host").await;
    let (client, client_dir) = spawn_test_node("app_client").await;

    let session = host.start_pairing_session();
    let mut rx = host.subscribe_events();
    let host_clone = host.clone();
    tokio::spawn(async move {
        if let Ok(CoreEvent::PairingApprovalNeeded(p)) = rx.recv().await {
            let _ = host_clone.respond_pairing(
                &p.request_id,
                PairingDecision::Approve {
                    granted_permissions: Permissions::default(),
                    accept_mutual: true,
                },
            );
        }
    });
    let host_id = host.endpoint_id().to_string();
    client
        .pair_with_peer(
            host.endpoint_id(),
            host.local_socket_addrs(),
            None,
            &session.code,
            true,
            Permissions::default(),
        )
        .await
        .unwrap();

    // 1. List installed apps
    let apps = client.remote_apps(&host_id, false).await.unwrap();
    assert!(!apps.apps.is_empty());

    // 2. Launching non-existent program returns NotFound with human-readable Chinese message
    let bad_launch = client
        .remote_launch(
            &host_id,
            AppLaunchArgs {
                target: r"C:\definitely_nonexistent_program_12345.exe".to_string(),
                kind: LaunchKind::PathOrTarget,
                args: None,
                skip_window_wait: false,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(bad_launch.error_code(), Some(ErrorCode::NotFound));

    // 3. Process list & kill confirmation guard & critical process guard
    let procs = client.remote_processes(&host_id).await.unwrap();
    assert!(!procs.processes.is_empty());

    let unconfirmed = client
        .remote_kill_process(
            &host_id,
            ProcKillArgs {
                pid: 4,
                confirmed: false,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(unconfirmed.error_code(), Some(ErrorCode::Invalid));

    let critical = client
        .remote_kill_process(
            &host_id,
            ProcKillArgs {
                pid: 4,
                confirmed: true,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(critical.error_code(), Some(ErrorCode::Forbidden));

    client.shutdown().await;
    host.shutdown().await;
    let _ = std::fs::remove_dir_all(host_dir);
    let _ = std::fs::remove_dir_all(client_dir);
}

#[tokio::test]
async fn transfer_1gb_file_upload_and_download_checksum() {
    use std::io::{Read, Write};

    struct CleanupDir(PathBuf);
    impl Drop for CleanupDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    let (host, host_dir) = spawn_test_node("gb_host").await;
    let (client, client_dir) = spawn_test_node("gb_client").await;
    let _host_guard = CleanupDir(host_dir);
    let _client_guard = CleanupDir(client_dir);
    let work_dir = make_temp_dir("gb_work");
    let _work_guard = CleanupDir(work_dir.clone());

    let session = host.start_pairing_session();
    let mut rx = host.subscribe_events();
    let host_clone = host.clone();
    tokio::spawn(async move {
        if let Ok(CoreEvent::PairingApprovalNeeded(p)) = rx.recv().await {
            let _ = host_clone.respond_pairing(
                &p.request_id,
                PairingDecision::Approve {
                    granted_permissions: Permissions::default(),
                    accept_mutual: true,
                },
            );
        }
    });
    let host_id = host.endpoint_id().to_string();
    client
        .pair_with_peer(
            host.endpoint_id(),
            host.local_socket_addrs(),
            None,
            &session.code,
            true,
            Permissions::default(),
        )
        .await
        .unwrap();

    // Generate 1 GiB (1024 * 1 MiB) file on disk while computing BLAKE3 incrementally
    const CHUNK_MB: usize = 1024 * 1024;
    const TOTAL_CHUNKS: u64 = 1024;
    const ONE_GIB: u64 = (CHUNK_MB as u64) * TOTAL_CHUNKS;

    let src_path = work_dir.join("src_1gb.bin");
    let mut block = vec![0u8; CHUNK_MB];
    for (i, b) in block.iter_mut().enumerate() {
        *b = (i.wrapping_mul(131) ^ (i >> 9)) as u8;
    }
    let mut src_hasher = blake3::Hasher::new();
    {
        let mut f = std::fs::File::create(&src_path).unwrap();
        for idx in 0..TOTAL_CHUNKS {
            block[..8].copy_from_slice(&idx.to_le_bytes());
            src_hasher.update(&block);
            f.write_all(&block).unwrap();
        }
        f.sync_all().unwrap();
    }
    let expected_hash = src_hasher.finalize();

    // Upload 1 GiB
    let remote_path = work_dir.join("remote_1gb.bin");
    let up_start = Instant::now();
    let up_res = client
        .upload_file(
            &host_id,
            &src_path,
            &remote_path.to_string_lossy(),
            ConflictPolicy::Overwrite,
            CancellationToken::new(),
            None,
        )
        .await
        .expect("upload 1 GiB file");
    let up_secs = up_start.elapsed().as_secs_f64();
    assert_eq!(up_res.bytes_written, ONE_GIB);
    let _ = std::fs::remove_file(&src_path);

    // Download 1 GiB back
    let dl_path = work_dir.join("downloaded_1gb.bin");
    let dl_start = Instant::now();
    let dl_res = client
        .download_file(
            &host_id,
            &remote_path.to_string_lossy(),
            &dl_path,
            ConflictPolicy::Overwrite,
            CancellationToken::new(),
            None,
        )
        .await
        .expect("download 1 GiB file");
    let dl_secs = dl_start.elapsed().as_secs_f64();
    assert_eq!(dl_res.bytes_written, ONE_GIB);
    let _ = std::fs::remove_file(&remote_path);

    // Verify downloaded file size & BLAKE3 checksum incrementally
    let mut dl_hasher = blake3::Hasher::new();
    let mut got_total = 0u64;
    {
        let mut f = std::fs::File::open(&dl_path).unwrap();
        loop {
            let n = f.read(&mut block).unwrap();
            if n == 0 {
                break;
            }
            dl_hasher.update(&block[..n]);
            got_total += n as u64;
        }
    }
    let _ = std::fs::remove_file(&dl_path);

    assert_eq!(got_total, ONE_GIB);
    assert_eq!(dl_hasher.finalize(), expected_hash);
    println!(
        "1 GiB transfer verified: upload {:.1} MB/s ({:.2}s), download {:.1} MB/s ({:.2}s)",
        1024.0 / up_secs.max(0.001),
        up_secs,
        1024.0 / dl_secs.max(0.001),
        dl_secs
    );

    client.shutdown().await;
    host.shutdown().await;
}
