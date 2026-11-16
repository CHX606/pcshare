#[cfg(windows)]
mod win_tests {
    use pcshare_platform::{ NativePlatform, Platform, PlatformError };
    use std::process::{Command, Stdio};

    #[test]
    fn lists_drives_special_folders_memory_and_desktop_status() {
        let platform = NativePlatform::new();

        let drives = platform.list_drives().expect("list drives");
        assert!(!drives.is_empty(), "Windows should have at least one drive");
        assert!(drives.iter().any(|d| d.path.ends_with(":\\")));
        assert!(drives.iter().any(|d| d.total_bytes > 0));

        let folders = platform.special_folders().expect("special folders");
        assert!(!folders.is_empty(), "Should resolve standard user folders");

        let mem = platform.memory_info().expect("memory info");
        assert!(mem.total_bytes > 0);
        assert!(mem.available_bytes > 0 && mem.available_bytes <= mem.total_bytes);

        let desktop = platform.desktop_environment();
        let summary = desktop.summary();
        assert!(!summary.is_empty());

        // Visible windows call should succeed without panicking
        let _windows = platform.visible_windows();
    }

    #[test]
    fn dpapi_protects_and_unprotects_secret_and_detects_corruption() {
        let platform = NativePlatform::new();
        let secret = b"0123456789abcdef0123456789abcdef";

        let encrypted = platform.protect_secret(secret).expect("DPAPI protect");
        assert_ne!(encrypted.as_slice(), secret);

        let decrypted = platform
            .unprotect_secret(&encrypted)
            .expect("DPAPI unprotect");
        assert_eq!(decrypted.as_slice(), secret);

        // Corrupted data must fail cleanly
        let mut corrupted = encrypted;
        if let Some(last) = corrupted.last_mut() {
            *last ^= 0xFF;
        }
        let err = platform.unprotect_secret(&corrupted).unwrap_err();
        assert!(matches!(err, PlatformError::System(_)));
    }

    #[test]
    fn moves_temporary_file_to_recycle_bin() {
        let platform = NativePlatform::new();
        let temp_dir = std::env::temp_dir().join(format!(
            "pcshare_trash_test_{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let file_path = temp_dir.join("recycle_me_测试文件.txt");
        std::fs::write(&file_path, "temporary test content for recycle bin").unwrap();
        assert!(file_path.exists());

        platform
            .move_to_trash(std::slice::from_ref(&file_path))
            .expect("move_to_trash should succeed");

        assert!(
            !file_path.exists(),
            "file should no longer exist at original path after moving to recycle bin"
        );
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn lists_installed_apps_and_uses_cache() {
        let platform = NativePlatform::new();
        let apps_first = platform
            .installed_apps(false)
            .await
            .expect("list installed apps");
        assert!(!apps_first.is_empty(), "Start menu should have apps");

        let started = std::time::Instant::now();
        let apps_cached = platform
            .installed_apps(false)
            .await
            .expect("cached installed apps");
        let cached_elapsed = started.elapsed();
        assert_eq!(apps_first.len(), apps_cached.len());
        assert!(
            cached_elapsed.as_millis() < 50,
            "5-minute cache should return in <50ms, took {cached_elapsed:?}"
        );
    }

    #[test]
    fn lists_processes_refuses_critical_and_kills_spawned_child() {
        let platform = NativePlatform::new();
        let my_pid = std::process::id();

        let procs = platform.list_processes().expect("list processes");
        assert!(
            procs.iter().any(|p| p.pid == my_pid),
            "process list should include current process"
        );

        // Refuses killing self or system PID 4
        let self_kill = platform.kill_process(my_pid);
        assert!(matches!(self_kill, Err(PlatformError::PermissionDenied(_))));
        let sys_kill = platform.kill_process(4);
        assert!(matches!(sys_kill, Err(PlatformError::PermissionDenied(_))));

        // Spawn a harmless sleeping ping process and terminate it via platform.kill_process
        let mut child = Command::new("ping.exe")
            .args(["127.0.0.1", "-n", "30"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn dummy ping");
        let child_pid = child.id();

        let killed_name = platform
            .kill_process(child_pid)
            .expect("kill spawned dummy process");
        assert!(killed_name.to_lowercase().contains("ping"));
        let _ = child.wait();
    }

    #[tokio::test]
    async fn native_apps_list_under_100ms_and_extracts_png_icon_and_working_set() {
        let platform = NativePlatform::new();

        // Populate native Shell index once, then verify even a brand-new NativePlatform instance
        // verifies Start Menu + AppX fingerprint and returns all apps in < 100 ms!
        let _ = platform
            .installed_apps(true)
            .await
            .expect("initial native installed_apps");

        let fresh_platform = NativePlatform::new();
        let started = std::time::Instant::now();
        let apps = fresh_platform
            .installed_apps(false)
            .await
            .expect("native installed_apps");
        let cold_elapsed = started.elapsed();
        assert!(!apps.is_empty(), "Start menu should return installed apps");
        assert!(
            cold_elapsed.as_millis() < 100,
            "Task 2.10 requires native app list in < 100ms, took {cold_elapsed:?}"
        );

        // Task 2.10: app_icon returns valid PNG bytes (magic \x89PNG\r\n\x1a\n) for at least one installed app
        let mut got_png = false;
        for app in apps.iter().take(10) {
            if let Ok(Some(png_bytes)) = platform.app_icon(&app.app_id).await {
                assert!(
                    png_bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
                    "icon must be valid PNG"
                );
                // Second lookup must hit cache cleanly
                let cached = platform.app_icon(&app.app_id).await.unwrap();
                assert_eq!(cached.as_deref(), Some(png_bytes.as_slice()));
                got_png = true;
                break;
            }
        }
        assert!(got_png, "should extract PNG icon for at least one Start Menu app");

        // Task 2.7: current process working set RAM query
        let ws = platform.current_process_working_set();
        assert!(ws > 512 * 1024, "working set should be > 512 KB, got {ws}");
    }

    #[test]
    fn discovers_mac_addresses_and_validates_wol_magic_packet() {
        let platform = NativePlatform::new();
        let _macs = platform.local_mac_addresses();
        // Sending WoL to loopback/broadcast for a test MAC must validate format and send >= 1 packet
        let sent = platform
            .send_wol_packets(&["AA:BB:CC:DD:EE:FF".to_string()])
            .expect("send WoL magic packet");
        assert!(sent >= 1);

        // Invalid MAC format must return InvalidInput
        let bad = platform.send_wol_packets(&["not-a-mac".to_string()]);
        assert!(matches!(bad, Err(PlatformError::InvalidInput(_))));
    }

    #[test]
    fn registers_global_hotkey_and_detects_conflict() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let p1 = NativePlatform::new();
        let p2 = NativePlatform::new();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_clone = hits.clone();

        p1. set_global_hotkey(
            Some("Ctrl+Alt+Shift+F11"),
            Arc::new(move || {
                hits_clone.fetch_add(1, Ordering::SeqCst);
            }),
        )
        .expect("register unused hotkey");

        // Second registration of the exact same global hotkey must fail with Busy
        let conflict = p2.set_global_hotkey(Some("Ctrl+Alt+Shift+F11"), Arc::new(|| {}));
        assert!(
            matches!(conflict, Err(PlatformError::Busy(_))),
            "expected Busy conflict error, got {conflict:?}"
        );

        // Unregistering frees the hotkey so p2 can now claim it
        p1.set_global_hotkey(None, Arc::new(|| {}))
            .expect("unregister hotkey");
        p2.set_global_hotkey(Some("Ctrl+Alt+Shift+F11"), Arc::new(|| {}))
            .expect("claim hotkey after release");
        p2.set_global_hotkey(None, Arc::new(|| {})).unwrap();
    }

    #[tokio::test]
    async fn captures_monitor_and_window_jpeg_and_downscales_to_max_width() {
        let platform = NativePlatform::new();
        // Spawn our cancellable countdown window briefly so there is a guaranteed visible Win32 window
        // even inside an isolated headless/exebox desktop, exercising both window & screen JPEG scaling!
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let countdown_task = {
            let p_win = NativePlatform::new();
            tokio::spawn(async move {
                let _ = p_win
                    .show_power_countdown("截图自检窗口", "测试套件", 5, cancel_rx)
                    .await;
            })
        };
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let cap_windows = platform.list_capturable_windows();
        if let Some(target_win) = cap_windows
            .iter()
            .find(|w| w.title.contains("远程电源操作确认"))
            .or_else(|| cap_windows.first())
        {
            let win_shot = platform
                .capture_window_jpeg(target_win.hwnd, Some(320), 80)
                .expect("capture visible window as downscaled JPEG");
            assert!(
                win_shot.jpeg_bytes.starts_with(&[0xFF, 0xD8]),
                "window JPEG must start with SOI 0xFFD8"
            );
            assert!(win_shot.image_width <= 320);
            assert!(win_shot.source_width >= win_shot.image_width);
        }

        let monitors = platform.list_monitors().expect("list monitors");
        if !monitors.is_empty() {
            match platform.capture_screen_jpeg(Some(0), Some(640), 80) {
                Ok(shot) => {
                    assert!(
                        shot.jpeg_bytes.starts_with(&[0xFF, 0xD8]),
                        "captured screen bytes must start with JPEG SOI marker"
                    );
                    assert!(shot.image_width <= 640);
                    assert!(shot.source_width >= shot.image_width);
                }
                Err(PlatformError::PermissionDenied(_)) => {
                    // Valid human-readable error when OS desktop is locked / non-interactive
                }
                Err(other) => panic!("unexpected screen capture error: {other:?}"),
            }
        }

        let _ = cancel_tx.send(true);
        let _ = countdown_task.await;
    }

    #[tokio::test]
    async fn opens_conpty_session_executes_command_and_resizes() {
        let platform = NativePlatform::new();
        let (session, mut rx) = platform
            .open_pty_session(false, 100, 30, None)
            .expect("open cmd.exe ConPTY session");
        assert!(session.pid() > 0);

        session
            .resize(120, 36)
            .expect("resize ConPTY pseudo console");
        session
            .write_input(b"echo PCSHARE_CONPTY_OK\r\nexit\r\n")
            .expect("write input to ConPTY");

        let mut collected = Vec::new();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(8);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv()).await {
                Ok(Some(chunk)) => {
                    collected.extend_from_slice(&chunk);
                    let text = String::from_utf8_lossy(&collected);
                    if text.contains("PCSHARE_CONPTY_OK") {
                        break;
                    }
                }
                Ok(None) => break,
                Err(_) => {}
            }
        }
        session.close();
        let output = String::from_utf8_lossy(&collected);
        assert!(
            output.contains("PCSHARE_CONPTY_OK"),
            "ConPTY output did not contain marker, got: {output}"
        );
    }

    #[tokio::test]
    async fn power_countdown_cancels_cleanly_when_signaled() {
        let platform = NativePlatform::new();
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let fut = platform.show_power_countdown("关机", "单元测试控制端", 5, cancel_rx);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            let _ = cancel_tx.send(true);
        });
        let should_proceed = fut.await.expect("countdown future");
        assert!(!should_proceed, "cancelled countdown must return false");
    }

    #[test]
    fn clipboard_text_and_png_roundtrip() {
        use pcshare_platform::ClipboardContent;

        let platform = NativePlatform::new();
        // Backup prior clipboard text if any so we restore cleanly
        let backup = platform.get_clipboard().ok();

        platform
            .set_clipboard_text("电脑共享_阶段3_剪贴板测试")
            .expect("set clipboard text");
        let got_text = platform.get_clipboard().expect("get clipboard text");
        assert_eq!(
            got_text.content,
            ClipboardContent::Text("电脑共享_阶段3_剪贴板测试".to_string())
        );

        // Build a tiny 2x2 RGBA PNG in memory and verify PNG + DIB clipboard roundtrip
        let mut png_bytes = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut png_bytes, 2, 2);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut writer = enc.write_header().unwrap();
            writer
                .write_image_data(&[
                    255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 0, 255,
                ])
                .unwrap();
        }
        platform
            .set_clipboard_png(&png_bytes)
            .expect("set clipboard png");
        let got_img = platform.get_clipboard().expect("get clipboard png");
        match got_img.content {
            ClipboardContent::ImagePng {
                png_bytes: out_png,
                width,
                height,
            } => {
                assert_eq!((width, height), (2, 2));
                assert!(out_png.starts_with(b"\x89PNG\r\n\x1a\n"));
            }
            other => panic!("expected ImagePng on clipboard, got {other:?}"),
        }

        // Restore prior clipboard text if present
        if let Some(prev) = backup
            && let ClipboardContent::Text(s) = prev.content
        {
            let _ = platform.set_clipboard_text(&s);
        }
    }
}
