use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use pcshare_core::*;
use pcshare_platform::NativePlatform;
use pcshare_proto::*;
use tokio_util::sync::CancellationToken;

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pcshare_stage3_{tag}_{}_{}",
        std::process::id(),
        fastrand::u64(..)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn start_local_node(name: &str, dir: PathBuf) -> Arc<CoreNode> {
    let cfg = CoreConfig {
        data_dir: dir,
        network_mode: NetworkMode::LocalTest,
        device_name_override: Some(name.to_string()),
        auto_connect_peers: false,
    };
    let node = CoreNode::start(cfg, Arc::new(NativePlatform::new()))
        .await
        .expect("start local test node");
    node.set_dry_run_power_actions(true);
    node
}

async fn pair_two_nodes(
    client: &Arc<CoreNode>,
    host: &Arc<CoreNode>,
    host_grants_to_client: Permissions,
) -> String {
    let ticket = host.start_pairing_session();
    let mut host_events = host.subscribe_events();
    let host_clone = host.clone();
    let grants = host_grants_to_client.clone();
    let approver = tokio::spawn(async move {
        while let Ok(ev) = host_events.recv().await {
            if let CoreEvent::PairingApprovalNeeded(prompt) = ev {
                host_clone
                    .respond_pairing(
                        &prompt.request_id,
                        PairingDecision::Approve {
                            granted_permissions: grants,
                            accept_mutual: true,
                        },
                    )
                    .unwrap();
                break;
            }
        }
    });

    let rec = client
        .pair_with_peer(
            host.endpoint_id(),
            host.local_socket_addrs(),
            None,
            &ticket.code,
            true,
            Permissions::default(),
        )
        .await
        .expect("pair client with host");
    let _ = approver.await;
    rec.endpoint_id
}

#[tokio::test]
async fn stage3_progressive_file_search_and_mid_search_cancellation() {
    let dir_c = temp_dir("search_c");
    let dir_h = temp_dir("search_h");
    let search_root = temp_dir("search_root");

    // Create nested folders & files including Chinese names for Pinyin initial + substring search
    for i in 0..12 {
        let sub = search_root.join(format!("dept_{i:02}"));
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join(format!("年度报价单_{i:02}.xlsx")), b"sheet").unwrap();
        for j in 0..25 {
            std::fs::write(sub.join(format!("noise_{i}_{j}.txt")), b"x").unwrap();
        }
    }

    let client = start_local_node("控制端", dir_c.clone()).await;
    let host = start_local_node("文件机", dir_h.clone()).await;
    let host_id = pair_two_nodes(
        &client,
        &host,
        Permissions {
            allowed_roots: vec![search_root.to_string_lossy().to_string()],
            ..Permissions::default()
        },
    )
    .await;

    // 1. Progressive search by Pinyin initials "bjd" ("报价单")
    let mut batches_seen = 0usize;
    let mut matched = Vec::new();
    let root_str = search_root.to_string_lossy().to_string();
    client
        .remote_search_files(
            &host_id,
            &root_str,
            "bjd",
            Some(50),
            CancellationToken::new(),
            |batch| {
                batches_seen += 1;
                matched.extend(batch.entries);
            },
        )
        .await
        .expect("remote_search_files pinyin");
    assert!(batches_seen >= 1);
    assert_eq!(matched.len(), 12);

    // 2. Mid-search cancellation stops cleanly without hanging
    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();
    let res = client
        .remote_search_files(&host_id, &root_str, "noise", None, cancel, |_batch| {
            cancel_clone.cancel();
        })
        .await;
    assert!(matches!(res, Err(CoreError::Cancelled) | Ok(_)));

    client.shutdown().await;
    host.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir_c);
    let _ = std::fs::remove_dir_all(&dir_h);
    let _ = std::fs::remove_dir_all(&search_root);
}

#[tokio::test]
async fn stage3_screenshot_and_click_key_permissions_and_capture() {
    let dir_c = temp_dir("shot_c");
    let dir_h = temp_dir("shot_h");
    let client = start_local_node("看屏端", dir_c.clone()).await;
    let host = start_local_node("桌面端", dir_h.clone()).await;

    // Default permissions have screenshot=false, input_control=false
    let host_id = pair_two_nodes(&client, &host, Permissions::default()).await;

    let denied_shot = client.remote_shot_monitors(&host_id).await.unwrap_err();
    assert_eq!(denied_shot.error_code(), Some(ErrorCode::Forbidden));

    let denied_click = client
        .remote_input_click(
            &host_id,
            InputClickArgs {
                screen_x: 100,
                screen_y: 100,
                button: MouseBtn::Left,
                double: false,
                target_hwnd: None,
                refresh_shot: None,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(denied_click.error_code(), Some(ErrorCode::Forbidden));

    // Grant screenshot permission on host (leave input_control=false first to verify independent scope)
    let client_id = client.endpoint_id().to_string();
    host.update_peer_permissions(
        &client_id,
        Permissions {
            screenshot: true,
            input_control: false,
            ..Permissions::default()
        },
    )
    .unwrap();

    // Spawn a visible countdown window on host so window & screen capture succeed even in isolated desktop
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let host_plat = host.platform().clone();
    let win_handle = tokio::spawn(async move {
        let _ = host_plat
            .show_power_countdown("看一眼测试窗口", "集成测试", 6, cancel_rx)
            .await;
    });
    tokio::time::sleep(Duration::from_millis(200)).await;

    let mon_reply = client
        .remote_shot_monitors(&host_id)
        .await
        .expect("remote_shot_monitors allowed");
    if let Some(w) = mon_reply
        .windows
        .iter()
        .find(|w| w.title.contains("远程电源操作确认"))
        .or_else(|| mon_reply.windows.first())
    {
        let shot = client
            .remote_shot_window(
                &host_id,
                ShotWindowArgs {
                    hwnd: w.hwnd,
                    max_width: Some(400),
                    quality: Some(80),
                },
            )
            .await
            .expect("remote_shot_window");
        assert!(shot.jpeg_bytes.starts_with(&[0xFF, 0xD8]));
        assert!(shot.header.transform.image_width <= 400);
    }

    // Input must STILL be forbidden until input_control=true
    let still_denied = client
        .remote_input_key(
            &host_id,
            InputKeyArgs {
                text: None,
                key_combo: Some("Esc".to_string()),
                target_hwnd: None,
                refresh_shot: None,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(still_denied.error_code(), Some(ErrorCode::Forbidden));

    let _ = cancel_tx.send(true);
    let _ = win_handle.await;

    client.shutdown().await;
    host.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir_c);
    let _ = std::fs::remove_dir_all(&dir_h);
}

#[tokio::test]
async fn stage3_clipboard_text_and_png_sync_and_permission_guard() {
    let dir_c = temp_dir("clip_c");
    let dir_h = temp_dir("clip_h");
    let client = start_local_node("剪贴端A", dir_c.clone()).await;
    let host = start_local_node("剪贴端B", dir_h.clone()).await;

    let host_id = pair_two_nodes(&client, &host, Permissions::default()).await;

    // Forbidden by default
    let err = client.remote_get_clipboard(&host_id).await.unwrap_err();
    assert_eq!(err.error_code(), Some(ErrorCode::Forbidden));

    host.update_peer_permissions(
        &client.endpoint_id().to_string(),
        Permissions {
            clipboard: true,
            ..Permissions::default()
        },
    )
    .unwrap();

    // Backup host clipboard so test leaves OS clipboard clean
    let backup = host.platform().get_clipboard().ok();

    client
        .remote_set_clipboard_text(&host_id, "跨设备剪贴板同步_123")
        .await
        .expect("remote_set_clipboard_text");
    let got = client
        .remote_get_clipboard(&host_id)
        .await
        .expect("remote_get_clipboard");
    assert_eq!(got.header.kind, ClipKind::Text);
    assert_eq!(got.header.text.as_deref(), Some("跨设备剪贴板同步_123"));

    // PNG image roundtrip over QUIC stream
    let mut png_bytes = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut png_bytes, 2, 2);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut w = enc.write_header().unwrap();
        w.write_image_data(&[10, 20, 30, 255, 40, 50, 60, 255, 70, 80, 90, 255, 100, 110, 120, 255])
            .unwrap();
    }
    client
        .remote_set_clipboard_png(&host_id, &png_bytes)
        .await
        .expect("remote_set_clipboard_png");
    let got_img = client
        .remote_get_clipboard(&host_id)
        .await
        .expect("remote_get_clipboard png");
    assert_eq!(got_img.header.kind, ClipKind::ImagePng);
    assert_eq!(got_img.header.image_width, Some(2));
    assert!(got_img.png_bytes.starts_with(b"\x89PNG\r\n\x1a\n"));

    if let Some(prev) = backup
        && let pcshare_platform::ClipboardContent::Text(s) = prev.content
    {
        let _ = host.platform().set_clipboard_text(&s);
    }

    client.shutdown().await;
    host.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir_c);
    let _ = std::fs::remove_dir_all(&dir_h);
}

#[tokio::test]
async fn stage3_interactive_conpty_terminal_and_permission_guard() {
    let dir_c = temp_dir("term_c");
    let dir_h = temp_dir("term_h");
    let client = start_local_node("终端控制端", dir_c.clone()).await;
    let host = start_local_node("终端被控端", dir_h.clone()).await;

    let host_id = pair_two_nodes(&client, &host, Permissions::default()).await;

    // Default terminal=false must reject
    let denied = client
        .open_remote_terminal(
            &host_id,
            TermOpenArgs {
                shell: TermShell::Cmd,
                cols: 100,
                rows: 30,
                cwd: None,
            },
        )
        .await;
    assert_eq!(
        denied.err().and_then(|e| e.error_code()),
        Some(ErrorCode::Forbidden)
    );

    host.update_peer_permissions(
        &client.endpoint_id().to_string(),
        Permissions {
            terminal: true,
            ..Permissions::default()
        },
    )
    .unwrap();

    let mut term = client
        .open_remote_terminal(
            &host_id,
            TermOpenArgs {
                shell: TermShell::Cmd,
                cols: 100,
                rows: 30,
                cwd: None,
            },
        )
        .await
        .expect("open_remote_terminal allowed");
    assert!(term.pid > 0);
    term.resize(120, 36).await.expect("resize remote terminal");
    term.send_input(b"echo STAGE3_REMOTE_PTY_OK\r\nexit\r\n")
        .await
        .expect("send terminal input");

    let mut output_buf = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), term.output_rx.recv()).await {
            Ok(Some(TerminalStreamEvent::Output(bytes))) => {
                output_buf.extend_from_slice(&bytes);
                if String::from_utf8_lossy(&output_buf).contains("STAGE3_REMOTE_PTY_OK") {
                    break;
                }
            }
            Ok(Some(TerminalStreamEvent::Exited(_))) | Ok(None) => break,
            Err(_) => {}
        }
    }
    let _ = term.close().await;
    let rendered = String::from_utf8_lossy(&output_buf);
    assert!(
        rendered.contains("STAGE3_REMOTE_PTY_OK"),
        "expected remote ConPTY output marker, got: {rendered}"
    );

    client.shutdown().await;
    host.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir_c);
    let _ = std::fs::remove_dir_all(&dir_h);
}

#[tokio::test]
async fn stage3_power_mac_recording_wol_relay_and_cancellable_countdown() {
    let dir_a = temp_dir("pow_a");
    let dir_b = temp_dir("pow_b");
    let dir_c = temp_dir("pow_c");
    let node_a = start_local_node("控制端A", dir_a.clone()).await;
    let node_b = start_local_node("同网段在线机B", dir_b.clone()).await;
    let node_c = start_local_node("睡眠目标机C", dir_c.clone()).await;

    let peer_b_id = pair_two_nodes(&node_a, &node_b, Permissions::default()).await;
    let peer_c_id = pair_two_nodes(&node_a, &node_c, Permissions::default()).await;

    // Ensure peer C has a recorded MAC address (in case sandbox has no physical NIC, seed one if empty)
    let mut c_rec = node_a.storage().get_peer(&peer_c_id).unwrap();
    if c_rec.mac_addresses.is_empty() {
        c_rec.mac_addresses = vec!["AA:BB:CC:11:22:33".to_string()];
        node_a.storage().upsert_peer(c_rec).unwrap();
    }

    // Connect A -> B so B is online and can relay WoL for C
    let _ = node_a.remote_ping(&peer_b_id).await.unwrap();

    // Shut down C to simulate C sleeping/offline, then wake C from A (local WoL + relay via online B!)
    let wake_report = node_a.wake_peer(&peer_c_id).await.expect("wake_peer");
    assert!(wake_report.local_packets >= 1);
    assert_eq!(wake_report.relay_peers_notified, 1);
    assert!(wake_report.relayed_packets >= 1);

    // PowerExec must be Forbidden when power=false
    let denied_pow = node_a
        .remote_power_exec(
            &peer_b_id,
            PowerExecArgs {
                action: PowerAction::Shutdown,
                countdown_secs: Some(3),
                requester_name: "控制端A".to_string(),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(denied_pow.error_code(), Some(ErrorCode::Forbidden));

    // Grant power=true on B (dry_run_power_actions is already enabled on start_local_node!)
    node_b
        .update_peer_permissions(
            &node_a.endpoint_id().to_string(),
            Permissions {
                power: true,
                ..Permissions::default()
            },
        )
        .unwrap();

    // Start 5s shutdown countdown and cancel after 300ms -> cancelled_by_host_user=true, executed=false
    let node_a_cancel = node_a.clone();
    let b_id_cancel = peer_b_id.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        let _ = node_a_cancel.remote_power_cancel(&b_id_cancel).await;
    });
    let pow_reply = node_a
        .remote_power_exec(
            &peer_b_id,
            PowerExecArgs {
                action: PowerAction::Shutdown,
                countdown_secs: Some(5),
                requester_name: "控制端A".to_string(),
            },
        )
        .await
        .expect("power exec cancelled");
    assert!(pow_reply.cancelled_by_host_user);
    assert!(!pow_reply.executed);

    node_a.shutdown().await;
    node_b.shutdown().await;
    node_c.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir_a);
    let _ = std::fs::remove_dir_all(&dir_b);
    let _ = std::fs::remove_dir_all(&dir_c);
}

#[tokio::test]
async fn stage3_remote_file_local_edit_watch_save_back_and_conflict_detection() {
    let dir_c = temp_dir("edit_c");
    let dir_h = temp_dir("edit_h");
    let work_dir = temp_dir("edit_work");
    let remote_doc = work_dir.join("合同草稿.txt");
    std::fs::write(&remote_doc, "第一版内容").unwrap();

    let client = start_local_node("编辑端", dir_c.clone()).await;
    let host = start_local_node("存储端", dir_h.clone()).await;
    let host_id = pair_two_nodes(
        &client,
        &host,
        Permissions {
            allowed_roots: vec![work_dir.to_string_lossy().to_string()],
            ..Permissions::default()
        },
    )
    .await;

    let mut events = client.subscribe_events();
    let remote_path_str = remote_doc.to_string_lossy().to_string();
    let session = client
        .open_remote_file_locally(&host_id, &remote_path_str, false)
        .await
        .expect("open_remote_file_locally");
    assert_eq!(
        std::fs::read_to_string(&session.local_path).unwrap(),
        "第一版内容"
    );

    // Modify local temp file -> watcher detects save and emits EditSyncPromptNeeded
    tokio::time::sleep(Duration::from_millis(150)).await;
    std::fs::write(&session.local_path, "第二版_本机保存").unwrap();

    let mut got_prompt = false;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Ok(Ok(CoreEvent::EditSyncPromptNeeded(p))) =
            tokio::time::timeout(Duration::from_millis(300), events.recv()).await
            && p.session_id == session.session_id
        {
            got_prompt = true;
            break;
        }
    }
    assert!(got_prompt, "file watcher must emit EditSyncPromptNeeded after local save");

    // Approve save-back -> uploads cleanly when remote file was untouched
    let outcome = client
        .sync_back_edited_file(&session.session_id, false, false)
        .await
        .expect("sync_back_edited_file");
    assert!(matches!(outcome, EditSyncOutcome::Uploaded { .. }));
    assert_eq!(
        std::fs::read_to_string(&remote_doc).unwrap(),
        "第二版_本机保存"
    );

    // Now simulate someone ALSO editing the remote file while we edit locally -> must detect conflict!
    std::fs::write(&remote_doc, "远端其他人改过的版本").unwrap();
    std::fs::write(&session.local_path, "本机第三版冲突修改").unwrap();
    let conflict_outcome = client
        .sync_back_edited_file(&session.session_id, false, false)
        .await
        .expect("sync_back conflict check");
    assert!(
        matches!(conflict_outcome, EditSyncOutcome::ConflictDetected { .. }),
        "expected ConflictDetected when remote file changed, got {conflict_outcome:?}"
    );
    // Remote file must NOT be overwritten unless force_overwrite=true
    assert_eq!(
        std::fs::read_to_string(&remote_doc).unwrap(),
        "远端其他人改过的版本"
    );

    client.shutdown().await;
    host.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir_c);
    let _ = std::fs::remove_dir_all(&dir_h);
    let _ = std::fs::remove_dir_all(&work_dir);
}

#[tokio::test]
async fn stage3_quick_launcher_under_300ms_and_batch_execution() {
    let dir_a = temp_dir("quick_a");
    let dir_b = temp_dir("quick_b");
    let dir_c = temp_dir("quick_c");
    let script_dir = temp_dir("quick_scripts");
    let marker_file = script_dir.join("launched_ok.txt");
    let bat_file = script_dir.join("fast_touch.bat");
    std::fs::write(
        &bat_file,
        format!("@echo off\r\necho ok > \"{}\"\r\n", marker_file.display()),
    )
    .unwrap();

    let node_a = start_local_node("主控台", dir_a.clone()).await;
    let node_b = start_local_node("书房主机", dir_b.clone()).await;
    let node_c = start_local_node("卧室电脑", dir_c.clone()).await;

    let peer_b_id = pair_two_nodes(&node_a, &node_b, {
        Permissions {
            terminal: true,
            ..Permissions::default()
        }
    })
    .await;
    let peer_c_id = pair_two_nodes(&node_a, &node_c, {
        Permissions {
            terminal: true,
            ..Permissions::default()
        }
    })
    .await;

    // Warm connection + seed a recent folder & app cache entry on B
    let _ = node_a.remote_ping(&peer_b_id).await.unwrap();
    let _ = node_a.remote_ping(&peer_c_id).await.unwrap();
    node_a.record_recent_folder(&peer_b_id, "书房主机", &script_dir.to_string_lossy());

    // 1. Test `设备名 关键字` + Pinyin initials ("sf quick_scripts")
    let hits = node_a.quick_search("sf quick_scripts", 10);
    assert!(
        hits.iter().any(|h| h.peer_id == peer_b_id),
        "quick_search should match '书房主机' via pinyin initials 'sf'"
    );

    // 2. Verify Task 3.1 latency metric: <= 300 ms from Enter to remote host starting launch!
    let t0 = Instant::now();
    node_a
        .quick_launch_target(
            &peer_b_id,
            &bat_file.to_string_lossy(),
            LaunchKind::PathOrTarget,
        )
        .await
        .expect("quick_launch_target");
    let launch_elapsed = t0.elapsed();
    assert!(
        launch_elapsed.as_millis() <= 300,
        "Task 3.1 requires <= 300ms to remote launch start, took {launch_elapsed:?}"
    );

    // 3. Verify Task 3.8 Multi-device Batch Execution across B and C concurrently
    let results = node_a
        .execute_batch(
            vec![peer_b_id.clone(), peer_c_id.clone()],
            BatchCommand::RunCommand {
                command: "echo BATCH_NODE_OK".to_string(),
                shell: TermShell::Cmd,
                timeout_secs: 6,
            },
            None,
        )
        .await;
    assert_eq!(results.len(), 2);
    for r in &results {
        assert!(r.ok, "batch command failed on {}: {:?}", r.peer_name, r.summary);
        assert!(
            r.output.as_deref().unwrap_or_default().contains("BATCH_NODE_OK"),
            "batch output missing marker: {:?}",
            r.output
        );
    }

    node_a.shutdown().await;
    node_b.shutdown().await;
    node_c.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir_a);
    let _ = std::fs::remove_dir_all(&dir_b);
    let _ = std::fs::remove_dir_all(&dir_c);
    let _ = std::fs::remove_dir_all(&script_dir);
}
