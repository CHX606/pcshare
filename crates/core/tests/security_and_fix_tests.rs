//! 核心层安全与稳定性回归测试（覆盖路径边界、同路径移动/复制、批传上限与报错、
//! 限速器单调预约、配对取消清理、移除对端断入站、编辑回存错误中止、电源最小倒计时与互斥、
//! 批量命令退出码校验、只读列目录与拼音/缓存修正）。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use pcshare_core::app_proc::authorize_launch;
use pcshare_core::dir_cache::{DirCache, MAX_CACHED_DIR_ITEMS};
use pcshare_core::fs_service::{FsService, MAX_BATCH_FILE_BYTES};
use pcshare_core::pairing::PairingManager;
use pcshare_core::path_guard::validate_single_name;
use pcshare_core::{
    BatchCommand, CoreConfig, CoreEvent, CoreNode, NetworkMode, PairingDecision, RateLimiter,
    TransferTaskControl, to_pinyin_initials,
};
use pcshare_platform::NativePlatform;
use pcshare_proto::{
    ALPN_DIRECT, ALPN_MAIN, AppLaunchArgs, BatchFileItem, Command, ConflictPolicy, DirEntry,
    DirectStreamRequest, EntryKind, ErrorCode, FsCopyMoveArgs, FsMkdirReply, FsReadBatchArgs,
    FsTaskKind, FsTaskState, FsWriteBatchArgs, FsWriteBatchReply, LaunchKind, PeerGrantDirectArgs,
    PeerGrantDirectReply, Permissions, PowerAction, PowerExecArgs, ResponseEnvelope, TermShell,
    read_frame, write_frame,
};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pcshare_fix_reg_{}_{}_{}",
        tag,
        std::process::id(),
        fastrand::u64(..)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn start_local_node(name: &str, data_dir: PathBuf) -> Arc<CoreNode> {
    let node = CoreNode::start(
        CoreConfig {
            data_dir,
            network_mode: NetworkMode::LocalTest,
            device_name_override: Some(name.to_string()),
            auto_connect_peers: false,
        },
        Arc::new(NativePlatform::new()),
    )
    .await
    .expect("start local node");
    node.set_dry_run_power_actions(true);
    node
}

async fn pair_nodes(client: &Arc<CoreNode>, host: &Arc<CoreNode>, perms: Permissions) -> String {
    let ticket = host.start_pairing_session();
    let mut host_events = host.subscribe_events();
    let host_c = host.clone();
    let perms_c = perms.clone();
    let approve = tokio::spawn(async move {
        while let Ok(ev) = host_events.recv().await {
            if let CoreEvent::PairingApprovalNeeded(prompt) = ev {
                host_c
                    .respond_pairing(
                        &prompt.request_id,
                        PairingDecision::Approve {
                            granted_permissions: perms_c,
                            accept_mutual: true,
                        },
                    )
                    .unwrap();
                break;
            }
        }
    });

    client
        .pair_with_peer(
            host.endpoint_id(),
            host.local_socket_addrs(),
            None,
            &ticket.code,
            true,
            perms,
        )
        .await
        .expect("pair_with_peer");
    approve.await.unwrap();
    host.endpoint_id().to_string()
}

#[test]
fn path_guard_rejects_reserved_names_trailing_dots_and_control_chars() {
    for bad in [
        "CON",
        "con.txt",
        "PRN",
        "aux.log",
        "NUL.tar.gz",
        "COM1",
        "com9.bin",
        "LPT1",
        "lpt9.dat",
        "report.",
        "report ",
        "bad\0name",
        "bad\nname",
        "bad\x1fname",
        ".",
        "..",
        "a/b",
        "a\\b",
    ] {
        let err = validate_single_name(bad).expect_err(&format!("should reject {bad:?}"));
        assert_eq!(err.code, ErrorCode::Invalid, "bad name={bad:?}");
    }

    for good in ["正常文件.docx", "console.txt", "archive.v1.2.tar.gz", "con1.txt"] {
        assert!(validate_single_name(good).is_ok(), "should allow {good:?}");
    }
}

#[test]
fn authorize_launch_rejects_windows_drive_relative_paths() {
    let perms = Permissions {
        allowed_roots: vec![r"C:\Allowed".to_string()],
        ..Permissions::default()
    };
    for bad in ["C:secret.docx", "c:sub\\tool.exe", "D:report.pdf"] {
        let err = authorize_launch(
            &perms,
            &AppLaunchArgs {
                target: bad.to_string(),
                kind: LaunchKind::PathOrTarget,
                args: None,
                skip_window_wait: false,
            },
        )
        .expect_err(&format!("must reject drive-relative path {bad}"));
        assert_eq!(err.code, ErrorCode::Invalid);
    }
}

#[test]
fn pinyin_buckets_fix_fa_xi_qian_and_dir_cache_limits_and_drive_keys() {
    assert_eq!(to_pinyin_initials("发吸浅"), "fxq");

    let dir = temp_dir("dircache");
    let cache = DirCache::load_or_new(&dir);
    let sample = vec![DirEntry {
        name: "a.txt".to_string(),
        path: r"C:\a.txt".to_string(),
        kind: EntryKind::File,
        size: 10,
        modified_ms: None,
        created_ms: None,
        readonly: false,
    }];
    cache.put("self", r"C:\", sample);
    assert!(cache.get("self", "C:").is_some());
    assert!(cache.get("self", "c:/").is_some());
    assert!(cache.get("self", r"C:\\").is_some());

    // Compact JSON check
    cache.flush().unwrap();
    let raw = std::fs::read_to_string(dir.join("dir_cache.json")).unwrap();
    assert!(!raw.contains('\n'), "dir_cache.json should use compact JSON");

    // Oversized directory (> MAX_CACHED_DIR_ITEMS) must not be retained in cache
    let huge: Vec<DirEntry> = (0..=MAX_CACHED_DIR_ITEMS)
        .map(|i| DirEntry {
            name: format!("f_{i}.txt"),
            path: format!(r"D:\big\f_{i}.txt"),
            kind: EntryKind::File,
            size: 1,
            modified_ms: None,
            created_ms: None,
            readonly: false,
        })
        .collect();
    cache.put("self", r"D:\big", huge);
    assert!(cache.get("self", r"D:\big").is_none());

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn pairing_manager_cancels_pending_approvals_on_session_cancel() {
    let mgr = PairingManager::new();
    let self_id = iroh::SecretKey::generate().public();
    let peer_id = iroh::SecretKey::generate().public();

    let ticket = mgr.start_session(self_id, None, &[]);
    assert_eq!(ticket.code.len(), 6);
    assert!(ticket.code.chars().all(|c| c.is_ascii_digit()));

    let (prompt, rx) = mgr
        .verify_code_and_prepare_approval(
            peer_id,
            "发起端".to_string(),
            "windows".to_string(),
            true,
            &ticket.code,
        )
        .expect("verify pairing code");

    // Cancelling the session must invalidate pending approval prompts and reject waiter
    mgr.cancel_session();
    let decision = rx.await.expect("waiter should receive rejection");
    assert!(matches!(decision, PairingDecision::Reject));
    assert!(
        mgr.respond(
            &prompt.request_id,
            PairingDecision::Approve {
                granted_permissions: Permissions::default(),
                accept_mutual: true,
            }
        )
        .is_err()
    );
}

#[tokio::test]
async fn fs_copy_and_move_reject_self_overwrite_and_descendant_target() {
    let work = temp_dir("self_copy_move");
    let file_a = work.join("a.txt");
    std::fs::write(&file_a, b"precious-data").unwrap();
    let folder = work.join("folder");
    let sub = folder.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(folder.join("doc.txt"), b"hello").unwrap();

    let (tx, _) = broadcast::channel(32);
    let svc = FsService::new(Arc::new(NativePlatform::new()), tx);
    let perms = Permissions::default();

    // 1. Move file onto itself with Overwrite -> must fail without deleting file
    let mv_task = svc
        .start_copy_or_move(
            "peer1",
            FsTaskKind::Move,
            &perms,
            FsCopyMoveArgs {
                sources: vec![file_a.to_string_lossy().to_string()],
                dest_dir: work.to_string_lossy().to_string(),
                conflict: ConflictPolicy::Overwrite,
            },
        )
        .expect("start move task");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mv_final = loop {
        let st = svc.task_status(&mv_task.task_id).unwrap();
        if st.state != FsTaskState::Running {
            break st;
        }
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(mv_final.state, FsTaskState::Failed);
    assert_eq!(
        mv_final.error.as_ref().map(|e| e.code),
        Some(ErrorCode::Invalid)
    );
    assert_eq!(std::fs::read(&file_a).unwrap(), b"precious-data");

    // 2. Copy file onto itself with Overwrite -> must fail without truncating file to 0 bytes
    let cp_task = svc
        .start_copy_or_move(
            "peer1",
            FsTaskKind::Copy,
            &perms,
            FsCopyMoveArgs {
                sources: vec![file_a.to_string_lossy().to_string()],
                dest_dir: work.to_string_lossy().to_string(),
                conflict: ConflictPolicy::Overwrite,
            },
        )
        .expect("start copy task");
    let cp_final = loop {
        let st = svc.task_status(&cp_task.task_id).unwrap();
        if st.state != FsTaskState::Running {
            break st;
        }
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(cp_final.state, FsTaskState::Failed);
    assert_eq!(std::fs::read(&file_a).unwrap(), b"precious-data");

    // 3. Copy directory into its own descendant -> must reject immediately
    let desc_err = svc
        .start_copy_or_move(
            "peer1",
            FsTaskKind::Copy,
            &perms,
            FsCopyMoveArgs {
                sources: vec![folder.to_string_lossy().to_string()],
                dest_dir: sub.to_string_lossy().to_string(),
                conflict: ConflictPolicy::Overwrite,
            },
        )
        .unwrap_err();
    assert_eq!(desc_err.code, ErrorCode::Invalid);

    let _ = std::fs::remove_dir_all(&work);
}

#[tokio::test]
async fn rate_limiter_monotonic_reservation_prevents_concurrent_amplification() {
    // 200 KB/s limit across 4 concurrent tasks each sending 3 x 32 KB = 384 KB total
    // True serialized duration ~ 1.82s (after 100ms burst); old buggy limiter amplified 4x (~0.4s)
    let limiter = RateLimiter::new(Some(200_000));
    let ctrl = TransferTaskControl::new();
    let started = Instant::now();
    let mut handles = Vec::new();
    for _ in 0..4 {
        let lim = limiter.clone();
        let c = ctrl.clone();
        handles.push(tokio::spawn(async move {
            for _ in 0..3 {
                lim.acquire_with_ctrl(32_000, &c, None).await.unwrap();
            }
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(1300),
        "concurrent rate limiter should not amplify speed; elapsed={elapsed:?}"
    );
}

#[tokio::test]
async fn write_batch_rejects_oversized_file_and_read_batch_errors_on_missing_file() {
    let work = temp_dir("batch_guard");
    let (tx, _) = broadcast::channel(16);
    let svc = FsService::new(Arc::new(NativePlatform::new()), tx);
    let perms = Permissions::default();

    // 1. handle_write_batch_stream rejects item.size > MAX_BATCH_FILE_BYTES without allocating
    {
        let (mut client_s, mut server_r) = tokio::io::duplex(16 * 1024);
        let (mut server_s, mut client_r) = tokio::io::duplex(16 * 1024);
        let dest = work.join("dst");
        let dest_str = dest.to_string_lossy().to_string();
        let svc_c = FsService::new(Arc::new(NativePlatform::new()), broadcast::channel(8).0);
        let perms_c = perms.clone();
        let server_task = tokio::spawn(async move {
            svc_c
                .handle_write_batch_stream(
                    &perms_c,
                    FsWriteBatchArgs {
                        dest_dir: dest_str,
                        conflict: ConflictPolicy::Overwrite,
                    },
                    &mut server_r,
                    &mut server_s,
                )
                .await
        });

        let ready: ResponseEnvelope<()> = read_frame(&mut client_r).await.unwrap();
        assert!(ready.into_result().is_ok());

        write_frame(
            &mut client_s,
            &BatchFileItem {
                done: false,
                rel_path: "huge.bin".to_string(),
                dir: false,
                size: MAX_BATCH_FILE_BYTES + 1,
                modified_ms: None,
                blake3_hex: None,
            },
        )
        .await
        .unwrap();

        let resp: ResponseEnvelope<FsWriteBatchReply> = read_frame(&mut client_r).await.unwrap();
        let err = resp.into_result().unwrap_err();
        assert_eq!(err.code, ErrorCode::Invalid);
        let ok = server_task.await.unwrap().unwrap();
        assert!(!ok);
    }

    // 2. handle_read_batch_stream returns explicit NotFound instead of silently skipping missing file
    {
        let (mut server_s, mut client_r) = tokio::io::duplex(16 * 1024);
        let missing = work.join("does_not_exist.txt").to_string_lossy().to_string();
        let ok = svc
            .handle_read_batch_stream(
                &perms,
                FsReadBatchArgs {
                    sources: vec![missing],
                },
                &mut server_s,
            )
            .await
            .unwrap();
        assert!(!ok);
        let resp: ResponseEnvelope<()> = read_frame(&mut client_r).await.unwrap();
        let err = resp.into_result().unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);
    }

    let _ = std::fs::remove_dir_all(&work);
}

#[tokio::test]
async fn fs_list_readonly_remove_peer_and_edit_sync_error_abort() {
    let dir_c = temp_dir("node_c");
    let dir_h = temp_dir("node_h");
    let work = temp_dir("node_work");
    let doc = work.join("note.txt");
    std::fs::write(&doc, "initial-remote-content").unwrap();

    let client = start_local_node("控制端", dir_c.clone()).await;
    let host = start_local_node("被控端", dir_h.clone()).await;
    let host_id = pair_nodes(&client, &host, Permissions::default()).await;
    let client_id = client.endpoint_id().to_string();

    // 1. fs_list_readonly works for self and remote without modifying recent_folders
    let self_entries = client
        .fs_list_readonly("self", &work.to_string_lossy())
        .await
        .expect("self fs_list_readonly");
    assert!(self_entries.iter().any(|e| e.name == "note.txt"));
    let remote_entries = client
        .fs_list_readonly(&host_id, &work.to_string_lossy())
        .await
        .expect("remote fs_list_readonly");
    assert!(remote_entries.iter().any(|e| e.name == "note.txt"));
    assert!(client.settings().recent_folders.is_empty());

    // 2. edit_sync must abort when remote fs.hash fails (e.g. host paused -> Busy) instead of overwriting
    let session = client
        .open_remote_file_locally(&host_id, &doc.to_string_lossy(), false)
        .await
        .expect("open_remote_file_locally");
    std::fs::write(&session.local_path, "local-edited-content").unwrap();
    host.set_host_paused(true);
    let sync_err = client
        .sync_back_edited_file(&session.session_id, false, false)
        .await
        .unwrap_err();
    assert_eq!(sync_err.error_code(), Some(ErrorCode::Busy));
    assert_eq!(
        std::fs::read_to_string(&doc).unwrap(),
        "initial-remote-content"
    );
    host.set_host_paused(false);

    // 3. remove_peer immediately disconnects existing incoming connection on host
    host.remove_peer(&client_id).unwrap();
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert!(client.remote_roots(&host_id).await.is_err());

    client.shutdown().await;
    host.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir_c);
    let _ = std::fs::remove_dir_all(&dir_h);
    let _ = std::fs::remove_dir_all(&work);
}

#[tokio::test]
async fn power_exec_enforces_min_10s_countdown_and_mutex_and_batch_exit_code() {
    let dir_c = temp_dir("pow_fix_c");
    let dir_h = temp_dir("pow_fix_h");
    let client = start_local_node("电源控制端", dir_c.clone()).await;
    let host = start_local_node("电源被控端", dir_h.clone()).await;
    let host_id = pair_nodes(
        &client,
        &host,
        Permissions {
            power: true,
            ..Permissions::default()
        },
    )
    .await;

    // 1. Passing countdown_secs: Some(0) for Shutdown must still enforce >= 10s countdown
    let client_first = client.clone();
    let host_id_first = host_id.clone();
    let first_task = tokio::spawn(async move {
        client_first
            .remote_power_exec(
                &host_id_first,
                PowerExecArgs {
                    action: PowerAction::Shutdown,
                    countdown_secs: Some(0),
                    requester_name: "测试端".to_string(),
                },
            )
            .await
    });

    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(
        !first_task.is_finished(),
        "countdown_secs: Some(0) must not bypass the 10-second shutdown countdown"
    );

    // 2. Concurrent power_exec must fail with Busy instead of overwriting cancel handle
    let second_err = client
        .remote_power_exec(
            &host_id,
            PowerExecArgs {
                action: PowerAction::Restart,
                countdown_secs: Some(10),
                requester_name: "测试端".to_string(),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(second_err.error_code(), Some(ErrorCode::Busy));

    // 3. Cancel active countdown -> first task cleanly returns cancelled_by_host_user = true
    let cancel_rep = client.remote_power_cancel(&host_id).await.unwrap();
    assert!(cancel_rep.cancelled);
    let first_rep = first_task.await.unwrap().expect("first power_exec result");
    assert!(first_rep.cancelled_by_host_user);
    assert!(!first_rep.executed);

    // 4. Batch RunCommand must treat non-zero exit code as failure (ok == false)
    let batch_res = client
        .execute_batch(
            vec!["self".to_string()],
            BatchCommand::RunCommand {
                command: "cmd /c exit 7".to_string(),
                shell: TermShell::Cmd,
                timeout_secs: 8,
            },
            None,
        )
        .await;
    assert_eq!(batch_res.len(), 1);
    assert!(
        !batch_res[0].ok,
        "non-zero exit code must produce ok=false, got: {:?}",
        batch_res[0]
    );
    assert!(
        batch_res[0].summary.contains('7'),
        "summary should include exit code 7, got: {:?}",
        batch_res[0]
    );

    // 5. Batch RunCommand timeout must return ok == false
    let timeout_res = client
        .execute_batch(
            vec!["self".to_string()],
            BatchCommand::RunCommand {
                command: "ping -n 6 127.0.0.1 >nul".to_string(),
                shell: TermShell::Cmd,
                timeout_secs: 2,
            },
            None,
        )
        .await;
    assert_eq!(timeout_res.len(), 1);
    assert!(
        !timeout_res[0].ok,
        "timed-out command must produce ok=false, got: {:?}",
        timeout_res[0]
    );
    assert!(
        timeout_res[0].summary.contains("超时"),
        "summary should indicate timeout, got: {:?}",
        timeout_res[0]
    );

    client.shutdown().await;
    host.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir_c);
    let _ = std::fs::remove_dir_all(&dir_h);
}

#[tokio::test]
async fn direct_transfer_respects_host_paused_and_consumes_single_use_grant() {
    let dir_ctrl = temp_dir("dt_ctrl");
    let dir_a = temp_dir("dt_a");
    let dir_b = temp_dir("dt_b");
    let work = temp_dir("dt_work");

    let ctrl = start_local_node("中控", dir_ctrl.clone()).await;
    let peer_a = start_local_node("源机", dir_a.clone()).await;
    let peer_b = start_local_node("目标机", dir_b.clone()).await;

    let a_id = pair_nodes(&ctrl, &peer_a, Permissions::default()).await;
    let b_id = pair_nodes(&ctrl, &peer_b, Permissions::default()).await;

    let src_file = work.join("sample.bin");
    let payload: Vec<u8> = (0..320 * 1024).map(|i| (i % 239) as u8).collect();
    std::fs::write(&src_file, &payload).unwrap();
    let dst_dir = work.join("dst");
    std::fs::create_dir_all(&dst_dir).unwrap();

    // Helper: construct local EndpointAddr for peer_b
    let mut b_addr = iroh::EndpointAddr::new(peer_b.endpoint_id());
    for sock in peer_b.local_socket_addrs() {
        b_addr = b_addr.with_ip_addr(sock);
    }

    // 1. Issue a direct grant via ctrl -> peer_b, then pause peer_b and verify handle_incoming_direct rejects with Busy
    let manual_token = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_string();
    {
        let ctrl_conn = ctrl
            .endpoint()
            .connect(b_addr.clone(), ALPN_MAIN)
            .await
            .expect("ctrl connect peer_b");
        let (mut send, mut recv) = ctrl_conn.open_bi().await.unwrap();
        let env = Command::PeerGrantDirect(PeerGrantDirectArgs {
            token: manual_token.clone(),
            allowed_peer_id: a_id.clone(),
            dest_dir: dst_dir.to_string_lossy().to_string(),
            ttl_ms: 60_000,
        })
        .to_envelope()
        .unwrap();
        write_frame(&mut send, &env).await.unwrap();
        send.finish().unwrap();
        let rep: ResponseEnvelope<PeerGrantDirectReply> = read_frame(&mut recv).await.unwrap();
        assert!(rep.into_result().unwrap().accepted);
        ctrl_conn.close(0u32.into(), b"done");
    }

    peer_b.set_host_paused(true);
    {
        let direct_conn = peer_a
            .endpoint()
            .connect(b_addr.clone(), ALPN_DIRECT)
            .await
            .expect("peer_a direct connect while paused");
        let (mut send, mut recv) = direct_conn.open_bi().await.unwrap();
        write_frame(
            &mut send,
            &DirectStreamRequest::Mkdir {
                token: manual_token.clone(),
                rel_path: "while_paused".to_string(),
            },
        )
        .await
        .unwrap();
        let resp: ResponseEnvelope<FsMkdirReply> = read_frame(&mut recv).await.unwrap();
        let err = resp.into_result().unwrap_err();
        assert_eq!(err.code, ErrorCode::Busy);
        assert!(!dst_dir.join("while_paused").exists());
    }

    // 2. Unpause -> first direct use consumes the grant and succeeds; second connection replaying same token is rejected
    peer_b.set_host_paused(false);
    {
        let direct_conn = peer_a
            .endpoint()
            .connect(b_addr.clone(), ALPN_DIRECT)
            .await
            .expect("peer_a direct connect first use");
        let (mut send, mut recv) = direct_conn.open_bi().await.unwrap();
        write_frame(
            &mut send,
            &DirectStreamRequest::Mkdir {
                token: manual_token.clone(),
                rel_path: "first_ok".to_string(),
            },
        )
        .await
        .unwrap();
        let resp: ResponseEnvelope<FsMkdirReply> = read_frame(&mut recv).await.unwrap();
        assert!(resp.into_result().is_ok());
        assert!(dst_dir.join("first_ok").is_dir());
        direct_conn.close(0u32.into(), b"done");
    }
    {
        let replay_conn = peer_a
            .endpoint()
            .connect(b_addr.clone(), ALPN_DIRECT)
            .await
            .expect("peer_a direct connect replay");
        let (mut send, mut recv) = replay_conn.open_bi().await.unwrap();
        write_frame(
            &mut send,
            &DirectStreamRequest::Mkdir {
                token: manual_token.clone(),
                rel_path: "replay_must_fail".to_string(),
            },
        )
        .await
        .unwrap();
        let resp: ResponseEnvelope<FsMkdirReply> = read_frame(&mut recv).await.unwrap();
        let err = resp.into_result().unwrap_err();
        assert_eq!(err.code, ErrorCode::Unauthorized);
        assert!(!dst_dir.join("replay_must_fail").exists());
    }

    // 3. End-to-end 320 KB file transfers via SingleFile streaming
    ctrl.transfer_between_peers_direct(
        &a_id,
        &[src_file.to_string_lossy().to_string()],
        &b_id,
        &dst_dir.to_string_lossy(),
        ConflictPolicy::Overwrite,
        CancellationToken::new(),
        None,
    )
    .await
    .expect("direct transfer should succeed");

    let got = std::fs::read(dst_dir.join("sample.bin")).unwrap();
    assert_eq!(blake3::hash(&got), blake3::hash(&payload));

    ctrl.shutdown().await;
    peer_a.shutdown().await;
    peer_b.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir_ctrl);
    let _ = std::fs::remove_dir_all(&dir_a);
    let _ = std::fs::remove_dir_all(&dir_b);
    let _ = std::fs::remove_dir_all(&work);
}
