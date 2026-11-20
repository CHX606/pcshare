use pcshare_proto::*;
use serde_json::json;
use tokio::io::AsyncWriteExt;

#[tokio::test]
async fn roundtrip_all_stage1_commands_and_replies() {
    let permissions = Permissions {
        fs_read: true,
        fs_write: true,
        app_launch: true,
        proc_manage: true,
        screenshot: false,
        input_control: false,
        terminal: false,
        power: false,
        clipboard: false,
        allowed_roots: vec![r"C:\Users\Public".to_string()],
    };

    let commands = vec![
        Command::SysHello(HelloArgs {
            protocol_version: PROTOCOL_VERSION,
            app_version: "0.1.0".to_string(),
            device_name: "书房电脑".to_string(),
            os: "windows".to_string(),
            capabilities: vec!["fs.v1".to_string(), "app.v1".to_string(), "proc.v1".to_string()],
            mac_addresses: Vec::new(),
        }),
        Command::SysPing(PingArgs {
            seq: 42,
            client_ts_ms: 1_700_000_000_000,
        }),
        Command::SysInfo,
        Command::FsRoots,
        Command::FsList(FsListArgs {
            path: r"E:\work".to_string(),
            batch_size: Some(500),
        }),
        Command::FsStat(FsStatArgs {
            path: r"E:\work\readme.md".to_string(),
        }),
        Command::FsMkdir(FsMkdirArgs {
            path: r"E:\work\新建文件夹".to_string(),
            parents: true,
        }),
        Command::FsRename(FsRenameArgs {
            path: r"E:\work\a.txt".to_string(),
            new_name: "b.txt".to_string(),
        }),
        Command::FsCopy(FsCopyMoveArgs {
            sources: vec![r"E:\work\b.txt".to_string()],
            dest_dir: r"E:\backup".to_string(),
            conflict: ConflictPolicy::Overwrite,
        }),
        Command::FsMove(FsCopyMoveArgs {
            sources: vec![r"E:\backup\b.txt".to_string()],
            dest_dir: r"E:\archive".to_string(),
            conflict: ConflictPolicy::RenameKeepBoth,
        }),
        Command::FsDelete(FsDeleteArgs {
            paths: vec![r"E:\archive\b.txt".to_string()],
            permanent: false,
        }),
        Command::FsTaskStatus(FsTaskIdArgs {
            task_id: "task-001".to_string(),
        }),
        Command::FsTaskCancel(FsTaskIdArgs {
            task_id: "task-001".to_string(),
        }),
        Command::FsRead(FsReadArgs {
            path: r"E:\work\video.mp4".to_string(),
            offset: 1024,
            length: Some(65536),
        }),
        Command::FsWrite(FsWriteArgs {
            path: r"E:\work\upload.bin".to_string(),
            size: 1_048_576,
            offset: 0,
            modified_ms: Some(1_700_000_123_000),
            conflict: ConflictPolicy::Skip,
        }),
        Command::AppList(AppListArgs { refresh: true }),
        Command::AppLaunch(AppLaunchArgs {
            target: "微信".to_string(),
            kind: LaunchKind::AppName,
            args: None,
            skip_window_wait: false,
        }),
        Command::ProcList,
        Command::ProcKill(ProcKillArgs {
            pid: 9527,
            confirmed: true,
        }),
        Command::SubEvents,
    ];

    let (mut client, mut server) = tokio::io::duplex(256 * 1024);
    for cmd in &commands {
        let env = cmd.to_envelope().expect("encode command envelope");
        write_frame(&mut client, &env).await.expect("write command frame");
        let decoded_env: RequestEnvelope =
            read_frame(&mut server).await.expect("read command frame");
        let decoded_cmd =
            Command::from_envelope(decoded_env).expect("parse command from envelope");
        assert_eq!(&decoded_cmd, cmd);
    }

    // Verify replies, error codes, pairing messages, and events round-trip
    let replies_hello = ResponseEnvelope::ok(HelloReply {
        protocol_version: PROTOCOL_VERSION,
        app_version: "0.1.0".to_string(),
        device_name: "卧室电脑".to_string(),
        os: "windows".to_string(),
        capabilities: vec!["fs.v1".to_string()],
        mac_addresses: Vec::new(),
        granted_permissions: permissions.clone(),
    });
    write_frame(&mut server, &replies_hello).await.unwrap();
    let got_hello: ResponseEnvelope<HelloReply> = read_frame(&mut client).await.unwrap();
    assert_eq!(got_hello, replies_hello);

    // Unit `()` reply round-trip
    let unit_ok = ResponseEnvelope::ok(());
    write_frame(&mut server, &unit_ok).await.unwrap();
    let got_unit: ResponseEnvelope<()> = read_frame(&mut client).await.unwrap();
    assert_eq!(got_unit, unit_ok);

    // All 10 error codes round-trip with exact wire names
    let all_codes = [
        (ErrorCode::Unauthorized, "unauthorized"),
        (ErrorCode::Forbidden, "forbidden"),
        (ErrorCode::NotFound, "not_found"),
        (ErrorCode::AlreadyExists, "already_exists"),
        (ErrorCode::Busy, "busy"),
        (ErrorCode::Invalid, "invalid"),
        (ErrorCode::Unsupported, "unsupported"),
        (ErrorCode::Io, "io"),
        (ErrorCode::Cancelled, "cancelled"),
        (ErrorCode::Internal, "internal"),
    ];
    for (code, wire) in all_codes {
        let resp: ResponseEnvelope<FsStatReply> = ResponseEnvelope::err(code, "错误说明");
        let json_val = serde_json::to_value(&resp).unwrap();
        assert_eq!(json_val["ok"], json!(false));
        assert_eq!(json_val["err"]["code"], json!(wire));
        write_frame(&mut server, &resp).await.unwrap();
        let decoded: ResponseEnvelope<FsStatReply> = read_frame(&mut client).await.unwrap();
        assert_eq!(decoded, resp);
    }

    // Pairing messages round-trip
    let pair_req = PairRequest {
        v: PROTOCOL_VERSION,
        code: "123456".to_string(),
        requester_name: "笔记本".to_string(),
        requester_os: "windows".to_string(),
        requester_relay_url: Some("https://aps1-1.relay.n0.iroh.link.".to_string()),
        requester_macs: vec!["AA:BB:CC:DD:EE:FF".to_string()],
        mutual: true,
        granted_to_responder: permissions.clone(),
    };
    write_frame(&mut client, &pair_req).await.unwrap();
    let got_pair_req: PairRequest = read_frame(&mut server).await.unwrap();
    assert_eq!(got_pair_req, pair_req);

    let pair_resp = PairResponse::Approved {
        responder_name: "台式机".to_string(),
        responder_os: "windows".to_string(),
        responder_relay_url: None,
        responder_macs: vec!["11:22:33:44:55:66".to_string()],
        granted_to_requester: permissions,
        mutual_accepted: true,
    };
    write_frame(&mut server, &pair_resp).await.unwrap();
    let got_pair_resp: PairResponse = read_frame(&mut client).await.unwrap();
    assert_eq!(got_pair_resp, pair_resp);
}

#[tokio::test]
async fn ignores_unknown_fields_and_flags_unknown_commands_as_unsupported() {
    let (mut writer, mut reader) = tokio::io::duplex(64 * 1024);

    // Request with unknown top-level and nested fields must still succeed
    let raw_req = json!({
        "v": 1,
        "cmd": "fs.list",
        "future_trace_id": "abc-123",
        "args": {
            "path": "C:\\",
            "future_sort_hint": "natural"
        }
    });
    write_frame(&mut writer, &raw_req).await.unwrap();
    let env: RequestEnvelope = read_frame(&mut reader).await.unwrap();
    let parsed = Command::from_envelope(env).expect("should ignore unknown fields");
    assert_eq!(
        parsed,
        Command::FsList(FsListArgs {
            path: "C:\\".to_string(),
            batch_size: None,
        })
    );

    // Unknown command returns ErrorCode::Unsupported
    let unknown_cmd = json!({
        "v": 1,
        "cmd": "future.unknown_cmd",
        "args": { "foo": 1 }
    });
    write_frame(&mut writer, &unknown_cmd).await.unwrap();
    let env2: RequestEnvelope = read_frame(&mut reader).await.unwrap();
    let err = Command::from_envelope(env2).unwrap_err();
    assert_eq!(err.code, ErrorCode::Unsupported);

    // Unsupported major protocol version returns ErrorCode::Unsupported
    let future_ver = json!({
        "v": 2,
        "cmd": "sys.ping",
        "args": {}
    });
    write_frame(&mut writer, &future_ver).await.unwrap();
    let env3: RequestEnvelope = read_frame(&mut reader).await.unwrap();
    let err3 = Command::from_envelope(env3).unwrap_err();
    assert_eq!(err3.code, ErrorCode::Unsupported);

    // Response with unknown fields must still decode cleanly
    let raw_resp = json!({
        "ok": true,
        "server_extra": 99,
        "data": {
            "seq": 7,
            "client_ts_ms": 100,
            "server_ts_ms": 105,
            "future_metric": "fast"
        }
    });
    write_frame(&mut writer, &raw_resp).await.unwrap();
    let resp: ResponseEnvelope<PingReply> = read_frame(&mut reader).await.unwrap();
    assert_eq!(
        resp,
        ResponseEnvelope::ok(PingReply {
            seq: 7,
            client_ts_ms: 100,
            server_ts_ms: 105,
        })
    );
}

#[tokio::test]
async fn rejects_oversized_frames_on_write_and_read() {
    let (mut writer, mut reader) = tokio::io::duplex(1024);

    // 1. Write side rejects > 16 MB payload
    let huge = "x".repeat(MAX_FRAME_SIZE + 64);
    let write_err = write_frame(&mut writer, &huge).await.unwrap_err();
    assert!(matches!(write_err, FrameError::FrameTooLarge { .. }));

    // 2. Read side rejects header claiming > 16 MB immediately without allocating
    let claimed_len = (MAX_FRAME_SIZE as u32) + 1;
    writer.write_all(&claimed_len.to_be_bytes()).await.unwrap();
    let read_err = read_frame::<_, serde_json::Value>(&mut reader)
        .await
        .unwrap_err();
    assert!(matches!(read_err, FrameError::FrameTooLarge { .. }));
}

#[tokio::test]
async fn truncated_half_frame_errors_immediately_instead_of_hanging() {
    // Case A: Only 2 bytes of the 4-byte length header arrive before EOF
    {
        let (mut writer, mut reader) = tokio::io::duplex(1024);
        writer.write_all(&[0x00, 0x00]).await.unwrap();
        drop(writer);
        let err = read_frame::<_, RequestEnvelope>(&mut reader)
            .await
            .unwrap_err();
        assert!(matches!(err, FrameError::TruncatedFrame { .. }));
    }

    // Case B: Header claims 50 bytes, but only 12 bytes arrive before EOF
    {
        let (mut writer, mut reader) = tokio::io::duplex(1024);
        writer.write_all(&50u32.to_be_bytes()).await.unwrap();
        writer.write_all(b"{\"v\":1,\"cmd\"").await.unwrap();
        drop(writer);
        let err = read_frame::<_, RequestEnvelope>(&mut reader)
            .await
            .unwrap_err();
        assert!(matches!(err, FrameError::TruncatedFrame { .. }));
    }

    // Case C: Clean EOF before any frame bytes returns Ok(None) via read_optional_frame
    {
        let (writer, mut reader) = tokio::io::duplex(1024);
        drop(writer);
        let opt = read_optional_frame::<_, RequestEnvelope>(&mut reader)
            .await
            .unwrap();
        assert!(opt.is_none());
    }
}

#[tokio::test]
async fn roundtrip_all_stage2_commands_events_and_resume_headers() {
    let stage2_cmds = vec![
        Command::FsHash(FsHashArgs {
            path: r"E:\work\big.iso".to_string(),
            length: Some(1_048_576),
        }),
        Command::FsWatch(FsWatchArgs {
            path: r"E:\work".to_string(),
        }),
        Command::FsUnwatch(FsUnwatchArgs {
            path: r"E:\work".to_string(),
        }),
        Command::FsProbeResume(FsProbeResumeArgs {
            dest_dir: r"E:\dest".to_string(),
            file_name: "big.iso".to_string(),
            conflict: ConflictPolicy::Overwrite,
        }),
        Command::FsWalk(FsWalkArgs {
            sources: vec![r"E:\work\folder".to_string()],
        }),
        Command::FsWriteBatch(FsWriteBatchArgs {
            dest_dir: r"E:\dest".to_string(),
            conflict: ConflictPolicy::RenameKeepBoth,
        }),
        Command::FsReadBatch(FsReadBatchArgs {
            sources: vec![r"E:\work\a.txt".to_string(), r"E:\work\b.txt".to_string()],
        }),
        Command::PeerGrantDirect(PeerGrantDirectArgs {
            token: "tok-123456".to_string(),
            allowed_peer_id: "a".repeat(64),
            dest_dir: r"E:\dest".to_string(),
            ttl_ms: 300_000,
        }),
        Command::PeerDirectTransfer(PeerDirectTransferArgs {
            transfer_id: "xfer-01".to_string(),
            target_peer_id: "b".repeat(64),
            target_addrs: vec!["127.0.0.1:19999".to_string()],
            target_relay_url: None,
            token: "tok-123456".to_string(),
            sources: vec![r"E:\src\doc.pdf".to_string()],
            dest_dir: r"E:\dest".to_string(),
            conflict: ConflictPolicy::Overwrite,
        }),
        Command::AppIcon(AppIconArgs {
            app_id: "Microsoft.WindowsCalculator_8wekyb3d8bbwe!App".to_string(),
        }),
    ];

    let (mut client, mut server) = tokio::io::duplex(256 * 1024);
    for cmd in &stage2_cmds {
        let env = cmd.to_envelope().expect("encode stage2 command");
        write_frame(&mut client, &env).await.unwrap();
        let got_env: RequestEnvelope = read_frame(&mut server).await.unwrap();
        let got_cmd = Command::from_envelope(got_env).expect("decode stage2 command");
        assert_eq!(&got_cmd, cmd);
    }

    let ev_dir = RemoteEvent::DirChanged {
        path: r"E:\work".to_string(),
    };
    write_frame(&mut server, &ev_dir).await.unwrap();
    let got_ev: RemoteEvent = read_frame(&mut client).await.unwrap();
    assert_eq!(got_ev, ev_dir);
}

#[tokio::test]
async fn roundtrip_all_stage3_commands_permissions_and_frames() {
    let default_perms = Permissions::default();
    assert!(!default_perms.allows(PermissionScope::Screenshot));
    assert!(!default_perms.allows(PermissionScope::InputControl));
    assert!(!default_perms.allows(PermissionScope::Terminal));
    assert!(!default_perms.allows(PermissionScope::Power));
    assert!(!default_perms.allows(PermissionScope::Clipboard));

    let full_perms = Permissions {
        screenshot: true,
        input_control: true,
        terminal: true,
        power: true,
        clipboard: true,
        ..Permissions::default()
    };
    assert!(full_perms.allows(PermissionScope::Screenshot));
    assert!(full_perms.allows(PermissionScope::InputControl));
    assert!(full_perms.allows(PermissionScope::Terminal));
    assert!(full_perms.allows(PermissionScope::Power));
    assert!(full_perms.allows(PermissionScope::Clipboard));

    let stage3_cmds = vec![
        Command::FsSearch(FsSearchArgs {
            path: r"E:\work".to_string(),
            query: "报价单".to_string(),
            max_results: Some(200),
        }),
        Command::ShotMonitors,
        Command::ShotScreen(ShotScreenArgs {
            monitor_index: Some(0),
            max_width: Some(1280),
            quality: Some(80),
        }),
        Command::ShotWindow(ShotWindowArgs {
            hwnd: 0x123456,
            max_width: Some(1600),
            quality: Some(80),
        }),
        Command::InputClick(InputClickArgs {
            screen_x: 640,
            screen_y: 360,
            button: MouseBtn::Left,
            double: true,
            target_hwnd: Some(0x123456),
            refresh_shot: Some(ShotRefreshSpec {
                monitor_index: Some(0),
                hwnd: None,
                max_width: Some(1280),
                quality: Some(80),
            }),
        }),
        Command::InputKey(InputKeyArgs {
            text: Some("你好世界".to_string()),
            key_combo: Some("Ctrl+S".to_string()),
            target_hwnd: None,
            refresh_shot: None,
        }),
        Command::PowerExec(PowerExecArgs {
            action: PowerAction::Shutdown,
            countdown_secs: Some(10),
            requester_name: "客厅笔记本".to_string(),
        }),
        Command::PowerCancel,
        Command::PowerWakeWol(PowerWakeWolArgs {
            mac_addresses: vec!["AA:BB:CC:DD:EE:FF".to_string()],
        }),
        Command::ClipGet,
        Command::ClipSet(ClipSetArgs {
            kind: ClipKind::Text,
            text: Some("跨电脑剪贴板文本".to_string()),
            png_byte_len: 0,
        }),
        Command::TermOpen(TermOpenArgs {
            shell: TermShell::PowerShell,
            cols: 120,
            rows: 32,
            cwd: Some(r"E:\work".to_string()),
        }),
    ];

    let (mut client, mut server) = tokio::io::duplex(256 * 1024);
    for cmd in &stage3_cmds {
        let env = cmd.to_envelope().expect("encode stage3 command");
        write_frame(&mut client, &env).await.unwrap();
        let got_env: RequestEnvelope = read_frame(&mut server).await.unwrap();
        let got_cmd = Command::from_envelope(got_env).expect("decode stage3 command");
        assert_eq!(&got_cmd, cmd);
    }

    // Coordinate mapping helper precision check
    let transform = ShotTransform {
        origin_x: -1920,
        origin_y: 0,
        source_width: 1920,
        source_height: 1080,
        image_width: 960,
        image_height: 540,
        dpi_scale_percent: 150,
        target_hwnd: None,
        monitor_index: Some(1),
    };
    assert_eq!(transform.map_image_point(480, 270), (-960, 540));

    // Terminal bi-stream frames roundtrip
    let term_in = TermClientFrame::Resize { cols: 132, rows: 40 };
    write_frame(&mut client, &term_in).await.unwrap();
    let got_in: TermClientFrame = read_frame(&mut server).await.unwrap();
    assert_eq!(got_in, term_in);

    let term_out = TermServerFrame::Exited { exit_code: Some(0) };
    write_frame(&mut server, &term_out).await.unwrap();
    let got_out: TermServerFrame = read_frame(&mut client).await.unwrap();
    assert_eq!(got_out, term_out);
}

#[tokio::test]
async fn read_frame_limited_enforces_handshake_cap_and_reads_chunked_frames() {
    // 1. Handshake limit rejects frame claiming > MAX_HANDSHAKE_FRAME_SIZE immediately
    {
        let (mut writer, mut reader) = tokio::io::duplex(4096);
        let claimed = (MAX_HANDSHAKE_FRAME_SIZE as u32) + 1;
        writer.write_all(&claimed.to_be_bytes()).await.unwrap();
        let err = read_frame_limited::<_, serde_json::Value>(&mut reader, MAX_HANDSHAKE_FRAME_SIZE)
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            FrameError::FrameTooLarge {
                size,
                max
            } if size == MAX_HANDSHAKE_FRAME_SIZE + 1 && max == MAX_HANDSHAKE_FRAME_SIZE
        ));
    }

    // 2. Claiming 16 MB then closing after 16 bytes returns TruncatedFrame without 16 MB allocation
    {
        let (mut writer, mut reader) = tokio::io::duplex(4096);
        writer
            .write_all(&(MAX_FRAME_SIZE as u32).to_be_bytes())
            .await
            .unwrap();
        writer.write_all(b"0123456789abcdef").await.unwrap();
        drop(writer);
        let err = read_optional_frame::<_, serde_json::Value>(&mut reader)
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            FrameError::TruncatedFrame {
                expected,
                got
            } if expected == MAX_FRAME_SIZE && got == 16
        ));
    }

    // 3. Valid multi-chunk frame (> 64 KB) decodes cleanly via incremental chunked reader
    {
        let (mut writer, mut reader) = tokio::io::duplex(256 * 1024);
        let large_text = "a".repeat(150 * 1024);
        let payload = json!({ "blob": large_text });
        let write_task = tokio::spawn(async move {
            write_frame(&mut writer, &payload).await.unwrap();
        });
        let decoded: serde_json::Value = read_frame(&mut reader).await.unwrap();
        write_task.await.unwrap();
        assert_eq!(decoded["blob"].as_str().unwrap().len(), 150 * 1024);
    }
}
