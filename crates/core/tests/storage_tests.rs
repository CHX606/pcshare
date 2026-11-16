use pcshare_core::storage::{AuditEntry, LocalStorage, PeerRecord, Settings};
use pcshare_platform::NativePlatform;
use pcshare_proto::Permissions;
use std::sync::Arc;

fn temp_data_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pcshare_core_storage_{}_{}_{}",
        tag,
        std::process::id(),
        fastrand::u64(..)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[test]
fn identity_persists_across_restarts_and_is_not_plaintext() {
    let dir = temp_data_dir("identity");
    let platform = Arc::new(NativePlatform::new());

    let (store1, warnings1) =
        LocalStorage::open(&dir, platform.clone()).expect("first open should succeed");
    assert!(warnings1.is_empty(), "clean startup should have no warnings");
    let id1 = store1.endpoint_id();

    // Verify key file exists and is NOT stored as 64-byte plaintext hex
    let raw_key_file = std::fs::read(dir.join("identity.dpapi")).expect("read encrypted key");
    let secret_hex = store1
        .secret_key()
        .to_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    assert_ne!(raw_key_file, secret_hex.as_bytes());
    assert_ne!(raw_key_file.as_slice(), &store1.secret_key().to_bytes());
    drop(store1);

    // Second open must yield identical EndpointId
    let (store2, warnings2) =
        LocalStorage::open(&dir, platform).expect("second open should succeed");
    assert!(warnings2.is_empty());
    assert_eq!(store2.endpoint_id(), id1);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn peers_and_settings_persist_atomically_and_recover_from_corruption() {
    let dir = temp_data_dir("corrupt");
    let platform = Arc::new(NativePlatform::new());

    let (store, _) = LocalStorage::open(&dir, platform.clone()).unwrap();
    let sample_peer_id = iroh::SecretKey::from_bytes(&[7u8; 32]).public();
    store
        .upsert_peer(PeerRecord {
            endpoint_id: sample_peer_id.to_string(),
            name: "客厅电脑".to_string(),
            os: Some("windows".to_string()),
            permissions: Permissions::default(),
            allow_incoming: true,
            connect_outgoing: true,
            relay_url: Some("https://aps1-1.relay.n0.iroh.link.".to_string()),
            direct_addrs: vec![],
            mac_addresses: vec![],
            capabilities: vec![],
            paired_at_ms: 1_700_000_000_000,
            last_seen_ms: Some(1_700_000_050_000),
        })
        .unwrap();

    store
        .update_settings(|s: &mut Settings| {
            s.device_name = "我的主力机".to_string();
        })
        .unwrap();
    drop(store);

    // Verify normal reload preserves peer & settings
    {
        let (reloaded, warnings) = LocalStorage::open(&dir, platform.clone()).unwrap();
        assert!(warnings.is_empty());
        assert_eq!(reloaded.settings().device_name, "我的主力机");
        let peers = reloaded.list_peers();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].name, "客厅电脑");
    }

    // Corrupt peers.json, settings.json, and identity.dpapi manually
    std::fs::write(dir.join("peers.json"), b"{bad json!!!").unwrap();
    std::fs::write(dir.join("settings.json"), b"not-json-at-all").unwrap();
    std::fs::write(dir.join("identity.dpapi"), b"corrupted-dpapi-bytes").unwrap();

    // Reopening must STILL succeed, back up all 3 corrupt files, and return 3 Chinese warnings
    let (recovered, warnings) =
        LocalStorage::open(&dir, platform).expect("must recover from corrupted files");
    assert_eq!(warnings.len(), 3, "expected 3 warnings, got: {warnings:?}");
    assert!(recovered.list_peers().is_empty());

    // Verify backup files were created and preserved
    let entries: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(entries.iter().any(|n| n.starts_with("peers.json.corrupt.")));
    assert!(entries
        .iter()
        .any(|n| n.starts_with("settings.json.corrupt.")));
    assert!(entries
        .iter()
        .any(|n| n.starts_with("identity.dpapi.corrupt.")));

    // Verify audit log append & read
    recovered
        .append_audit(AuditEntry {
            timestamp_ms: 1_700_000_100_000,
            peer_id: sample_peer_id.to_string(),
            peer_name: "客厅电脑".to_string(),
            action: "fs.list".to_string(),
            detail: r"C:\Users".to_string(),
            ok: true,
        })
        .unwrap();
    let logs = recovered.read_recent_audits(10).unwrap();
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0].action, "fs.list");

    let _ = std::fs::remove_dir_all(&dir);
}
