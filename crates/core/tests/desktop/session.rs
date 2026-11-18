#[path = "cleanup.rs"]
mod cleanup;
use super::{pair_two_nodes, start_local_node, temp_dir};
use pcshare_core::{CoreNode, desktop::DesktopSession};
use pcshare_proto::{
    ErrorCode, Permissions,
    desktop::{DesktopInput, Source},
};
use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

struct Pair {
    client: Arc<CoreNode>,
    host: Arc<CoreNode>,
    id: String,
    dirs: [PathBuf; 2],
}
impl Pair {
    async fn new(screenshot: bool, input_control: bool) -> Self {
        let dirs = [temp_dir("desktop_client"), temp_dir("desktop_host")];
        let client = start_local_node("viewer", dirs[0].clone()).await;
        let host = start_local_node("host", dirs[1].clone()).await;
        let id = pair_two_nodes(
            &client,
            &host,
            Permissions {
                screenshot,
                input_control,
                ..Default::default()
            },
        )
        .await;
        Self {
            client,
            host,
            id,
            dirs,
        }
    }
    async fn finish(self) {
        self.client.shutdown().await;
        self.host.shutdown().await;
        cleanup::remove_dirs(self.dirs).await;
    }
}

#[tokio::test]
async fn session_checks_screenshot_permission_before_opening() {
    let pair = Pair::new(false, false).await;
    let result = DesktopSession::open(&pair.client, &pair.id, Source::Monitor(0)).await;
    assert!(matches!(result,Err(e) if e.error_code()==Some(ErrorCode::Forbidden)));
    pair.finish().await;
}

#[cfg(windows)]
#[tokio::test]
async fn old_peer_waits_for_refresh_and_close_cancels_pending_frame() {
    let pair = Pair::new(true, false).await;
    pair.client.remote_ping(&pair.id).await.unwrap();
    pair.client
        .storage()
        .update_peer_hello_meta(&pair.id, vec!["shot.v1".into()], vec![])
        .unwrap();
    let session = Arc::new(
        DesktopSession::open(&pair.client, &pair.id, Source::Monitor(0))
            .await
            .unwrap(),
    );
    assert!(!session.supports_raw_input());
    assert_eq!(session.next().await.unwrap().info.mode, "step");
    let reader = session.clone();
    let mut next = tokio::spawn(async move { reader.next().await });
    assert!(
        tokio::time::timeout(Duration::from_millis(250), &mut next)
            .await
            .is_err()
    );
    session.refresh();
    assert!(next.await.unwrap().unwrap().bytes.starts_with(&[255, 216]));
    let reader = session.clone();
    let waiting = tokio::spawn(async move { reader.next().await });
    session.close();
    assert!(matches!(
        waiting.await.unwrap(),
        Err(pcshare_core::CoreError::Cancelled)
    ));
    drop(session);
    pair.finish().await;
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
#[ignore = "需要解锁的 Windows 桌面，实采显示器并在内存中解码"]
async fn native_video_over_paired_quic_and_live_permission_revocation() {
    use tokio::io::AsyncWriteExt;
    let pair = Pair::new(true, false).await;
    let session = DesktopSession::open(&pair.client, &pair.id, Source::Monitor(0))
        .await
        .unwrap();
    assert!(session.supports_raw_input());
    let first = session.next().await.unwrap();
    assert_eq!(first.info.format, "jpeg");
    let error = session
        .input(vec![DesktopInput::Release])
        .await
        .unwrap_err();
    assert_eq!(error.error_code(), Some(ErrorCode::Forbidden));
    let started = Instant::now();
    let first = tokio::time::timeout(Duration::from_secs(35), session.next())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.info.mode, "live", "{:?}", first.info.reason);
    assert!(first.info.key);
    let startup = started.elapsed();
    let mut bytes = first.bytes;
    let mut gaps = Vec::new();
    let start = Instant::now();
    let mut prev = start;
    let mut last = first.info.timestamp_us;
    for _ in 1..90 {
        let frame = tokio::time::timeout(Duration::from_secs(4), session.next())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(frame.info.epoch, first.info.epoch);
        assert!(frame.info.timestamp_us > last);
        last = frame.info.timestamp_us;
        bytes.extend_from_slice(&frame.bytes);
        let now = Instant::now();
        gaps.push((now - prev).as_micros());
        prev = now;
    }
    gaps.sort_unstable();
    println!(
        "desktop: encoder={} size={}x{} first={:.0}ms fps={:.1} median={:.1}ms p95={:.1}ms stream={} bytes",
        first.info.encoder,
        first.info.transform.image_width,
        first.info.transform.image_height,
        startup.as_secs_f64() * 1000.0,
        89.0 / start.elapsed().as_secs_f64(),
        gaps[44] as f64 / 1000.0,
        gaps[84] as f64 / 1000.0,
        bytes.len()
    );
    let binary = std::fs::read_dir(pair.dirs[1].join("media"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path()
        .join("ffmpeg.exe");
    let mut decoder = tokio::process::Command::new(binary)
        .args([
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-xerror",
            "-f",
            "h264",
            "-i",
            "pipe:0",
            "-progress",
            "pipe:1",
            "-f",
            "null",
            "-",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    decoder
        .stdin
        .take()
        .unwrap()
        .write_all(&bytes)
        .await
        .unwrap();
    let decoded = decoder.wait_with_output().await.unwrap();
    assert!(
        decoded.status.success(),
        "{}",
        String::from_utf8_lossy(&decoded.stderr)
    );
    assert!(
        String::from_utf8_lossy(&decoded.stdout)
            .lines()
            .any(|s| s.trim() == "frame=90")
    );
    let viewer = pair.client.endpoint_id().to_string();
    pair.host
        .update_peer_permissions(
            &viewer,
            Permissions {
                screenshot: true,
                input_control: true,
                ..Default::default()
            },
        )
        .unwrap();
    session.input(vec![DesktopInput::Release]).await.unwrap();
    pair.host
        .update_peer_permissions(&viewer, Permissions::default())
        .unwrap();
    assert!(session.input(vec![DesktopInput::Release]).await.is_err());
    for _ in 0..6 {
        if let Err(e) = session.next().await {
            assert_eq!(e.error_code(), Some(ErrorCode::Forbidden));
            session.close();
            drop(session);
            pair.finish().await;
            return;
        }
    }
    panic!("video stayed active after revocation");
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
#[ignore = "真实桌面与接收端背压，包含两次网络恢复探测"]
async fn slow_consumer_falls_back_returns_one_action_image_and_recovers() {
    let pair = Pair::new(true, true).await;
    let session = DesktopSession::open(&pair.client, &pair.id, Source::Monitor(0))
        .await
        .unwrap();
    assert_eq!(session.next().await.unwrap().info.format, "jpeg");
    let first = session.next().await.unwrap();
    assert_eq!(first.info.mode, "live", "{:?}", first.info.reason);
    let since = Instant::now();
    let mut slow_frames = 0;
    loop {
        assert!(
            since.elapsed() < Duration::from_secs(75),
            "never entered step mode"
        );
        tokio::time::sleep(Duration::from_millis(900)).await;
        let frame = session.next().await.unwrap();
        if frame.info.mode == "step" {
            break;
        }
        slow_frames += 1;
    }
    let transition = since.elapsed();
    let audits = || {
        pair.host
            .storage()
            .read_recent_audits(100)
            .unwrap()
            .into_iter()
            .filter(|e| e.action == "shot.screen")
            .count()
    };
    let before = audits();
    assert!(
        tokio::time::timeout(Duration::from_millis(400), session.next())
            .await
            .is_err()
    );
    // A release for an unheld key exercises the complete weak-link action/feedback
    // path without injecting any input into the user's desktop.
    session
        .input(vec![DesktopInput::Key {
            vk: 0x87,
            down: false,
        }])
        .await
        .unwrap();
    let after_action = session.next().await.unwrap();
    assert_eq!(after_action.info.mode, "step");
    assert!(after_action.bytes.starts_with(&[255, 216]));
    assert_eq!(
        audits(),
        before,
        "a second screenshot RPC was sent for the same action"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(400), session.next())
            .await
            .is_err()
    );
    let recovered = tokio::time::timeout(Duration::from_secs(65), session.next())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(recovered.info.mode, "live");
    assert!(recovered.info.key);
    println!(
        "desktop pressure: {slow_frames} delayed frames, fallback after {:.1}s; action returned one JPEG, recovered after {:.1}s",
        transition.as_secs_f64(),
        since.elapsed().as_secs_f64() - transition.as_secs_f64()
    );
    session.close();
    drop(session);
    pair.finish().await;
}
