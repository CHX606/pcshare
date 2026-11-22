use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use pcshare_core::pinyin::match_rank;
use pcshare_core::*;
use pcshare_platform::NativePlatform;
use pcshare_proto::*;
use tokio_util::sync::CancellationToken;

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pcshare_r1_{tag}_{}_{}",
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
    let grants = host_grants_to_client;
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

#[test]
fn round1_pinyin_and_fuzzy_match_throughput_and_correctness() {
    assert_eq!(match_rank("财务季度报表_01.xlsx", "财务季度报表_01.xlsx"), Some(0));
    assert_eq!(match_rank("财务季度报表_01.xlsx", "财务季度"), Some(1));
    assert_eq!(match_rank("2026_财务季度报表_01.xlsx", "季度报表"), Some(2));
    assert_eq!(match_rank("财务季度报表_01.xlsx", "cwjdbb"), Some(3));
    assert_eq!(match_rank("年度财务季度报表.xlsx", "cwjdbb"), Some(4));
    assert_eq!(match_rank("transfer_engine_v2.rs", "tev2"), Some(5));
    assert_eq!(match_rank("noise_module_alpha.txt", "cwjdbb"), None);

    let samples = [
        "node_modules_cache_entry_0042.js",
        "Cargo.toml",
        "target_release_incremental_dep.d",
        "Windows_System_Driver_Info.dll",
        "2026年项目预算汇总表_终版_v3.xlsx",
        "电脑共享安装包说明文档.md",
        "财务季度报表_08.xlsx",
        "screenshot_capture_1920x1080.jpg",
    ];
    let started = Instant::now();
    let mut hits = 0usize;
    for _ in 0..25_000 {
        for s in &samples {
            if match_rank(s, "cwjdbb").is_some() {
                hits += 1;
            }
        }
    }
    let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(hits, 25_000);
    println!("[BENCH-R1] 200k match_rank evals: {elapsed_ms:.2} ms (hits={hits})");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn round1_parallel_file_search_12k_files_and_cancellation() {
    let dir_c = temp_dir("s_c");
    let dir_h = temp_dir("s_h");
    let search_root = temp_dir("s_root");

    // 240 directories * 50 files = 12,000 files total (240 matching Chinese Pinyin files)
    let root_clone = search_root.clone();
    tokio::task::spawn_blocking(move || {
        for grp in 0..12 {
            let gdir = root_clone.join(format!("group_{grp:02}"));
            for sub_i in 0..20 {
                let sdir = gdir.join(format!("sub_{sub_i:02}"));
                std::fs::create_dir_all(&sdir).unwrap();
                let idx = grp * 20 + sub_i;
                std::fs::write(
                    sdir.join(format!("财务季度报表_{idx:03}.xlsx")),
                    b"xlsx-payload-1234",
                )
                .unwrap();
                for f in 0..49 {
                    std::fs::write(sdir.join(format!("asset_{idx}_{f:02}.dat")), b"x").unwrap();
                }
            }
        }
    })
    .await
    .unwrap();

    let client = start_local_node("控制端", dir_c.clone()).await;
    let host = start_local_node("文件机", dir_h.clone()).await;
    let host_id = pair_two_nodes(&client, &host, Permissions::default()).await;

    let started = Instant::now();
    let mut found = Vec::new();
    let scanned = client
        .remote_search_files(
            &host_id,
            &search_root.to_string_lossy(),
            "cwjdbb",
            Some(1000),
            CancellationToken::new(),
            |batch| {
                found.extend(batch.entries);
            },
        )
        .await
        .expect("remote search 12k files");
    let search_ms = started.elapsed().as_secs_f64() * 1000.0;
    println!(
        "[BENCH-R1] 12k-file remote Pinyin search: {search_ms:.2} ms (scanned_dirs={scanned}, hits={})",
        found.len()
    );
    assert_eq!(found.len(), 240);
    assert!(found.iter().all(|e| e.size == 17));
    assert!(scanned >= 241);

    // Mid-search cancellation must return Cancelled promptly
    let cancel = CancellationToken::new();
    let cancel_c = cancel.clone();
    let res = client
        .remote_search_files(
            &host_id,
            &search_root.to_string_lossy(),
            "asset_",
            Some(5000),
            cancel_c,
            move |b| {
                if !b.entries.is_empty() {
                    cancel.cancel();
                }
            },
        )
        .await;
    assert!(matches!(res, Err(CoreError::Cancelled)));

    client.shutdown().await;
    host.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir_c);
    let _ = std::fs::remove_dir_all(&dir_h);
    let _ = std::fs::remove_dir_all(&search_root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn round1_streaming_upload_and_multistream_download_128mb() {
    let dir_c = temp_dir("t_c");
    let dir_h = temp_dir("t_h");
    let work = temp_dir("t_work");

    let file_bytes: usize = 128 * 1024 * 1024;
    let src_path = work.join("source_128mb.bin");
    let uploaded_remote = work.join("uploaded_128mb.bin");
    let downloaded_local = work.join("downloaded_128mb.bin");

    let src_c = src_path.clone();
    let expected_hash = tokio::task::spawn_blocking(move || {
        use std::io::Write;
        let mut f = std::fs::File::create(&src_c).unwrap();
        let mut hasher = blake3::Hasher::new();
        let mut block = vec![0u8; 1024 * 1024];
        for (i, b) in block.iter_mut().enumerate() {
            *b = (i.wrapping_mul(31).wrapping_add(17) & 0xFF) as u8;
        }
        for chunk_idx in 0..128u8 {
            block[0] = chunk_idx;
            block[511] = chunk_idx.wrapping_mul(7);
            f.write_all(&block).unwrap();
            hasher.update(&block);
        }
        f.sync_all().unwrap();
        hasher.finalize().to_hex().to_string()
    })
    .await
    .unwrap();

    let client = start_local_node("控制端", dir_c.clone()).await;
    let host = start_local_node("文件机", dir_h.clone()).await;
    let host_id = pair_two_nodes(&client, &host, Permissions::default()).await;
    // Warm up connection before timing pure transfer data plane
    client.remote_ping(&host_id).await.unwrap();

    let up_start = Instant::now();
    let up_done = client
        .upload_file_resumable(
            &host_id,
            &src_path,
            &uploaded_remote.to_string_lossy(),
            ConflictPolicy::Overwrite,
            CancellationToken::new(),
            None,
            None,
        )
        .await
        .expect("upload 128MB");
    let up_secs = up_start.elapsed().as_secs_f64();
    let up_mbps = 128.0 / up_secs;
    assert_eq!(up_done.bytes_written, file_bytes as u64);

    let down_start = Instant::now();
    let down_done = client
        .download_file_resumable(
            &host_id,
            &uploaded_remote.to_string_lossy(),
            &downloaded_local,
            ConflictPolicy::Overwrite,
            CancellationToken::new(),
            None,
            None,
        )
        .await
        .expect("download 128MB");
    let down_secs = down_start.elapsed().as_secs_f64();
    let down_mbps = 128.0 / down_secs;
    assert_eq!(down_done.bytes_written, file_bytes as u64);

    println!(
        "[BENCH-R1] 128 MiB Upload: {:.2} ms ({up_mbps:.1} MiB/s) | Download: {:.2} ms ({down_mbps:.1} MiB/s)",
        up_secs * 1000.0,
        down_secs * 1000.0
    );

    let dl_c = downloaded_local.clone();
    let actual_hash = tokio::task::spawn_blocking(move || {
        let mut f = std::fs::File::open(&dl_c).unwrap();
        let mut hasher = blake3::Hasher::new();
        std::io::copy(&mut f, &mut hasher).unwrap();
        hasher.finalize().to_hex().to_string()
    })
    .await
    .unwrap();
    assert_eq!(actual_hash, expected_hash);

    // 验证 >= 32 MiB 多流并发分片下载支持基于已有 .pcshare.part 连续前缀断点续传
    let resumed_dest = work.join("resumed_128mb.bin");
    let part_path = work.join(".resumed_128mb.bin.pcshare.part");
    let pre_len: usize = 40 * 1024 * 1024;
    {
        use std::io::{Read, Write};
        let mut src_f = std::fs::File::open(&src_path).unwrap();
        let mut pre_buf = vec![0u8; pre_len];
        src_f.read_exact(&mut pre_buf).unwrap();
        let mut part_f = std::fs::File::create(&part_path).unwrap();
        part_f.write_all(&pre_buf).unwrap();
        part_f.sync_all().unwrap();
    }
    let wire_bytes = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let wire_c = wire_bytes.clone();
    let res_done = client
        .download_file_resumable(
            &host_id,
            &uploaded_remote.to_string_lossy(),
            &resumed_dest,
            ConflictPolicy::Overwrite,
            CancellationToken::new(),
            None,
            Some(Arc::new(move |delta| {
                wire_c.fetch_add(delta, std::sync::atomic::Ordering::Relaxed);
            })),
        )
        .await
        .expect("resumable parallel range download");
    assert_eq!(res_done.bytes_written, file_bytes as u64);
    assert_eq!(
        wire_bytes.load(std::sync::atomic::Ordering::Relaxed),
        (file_bytes - pre_len) as u64,
        "must only transmit remaining 88 MiB over parallel range streams"
    );

    let res_c = resumed_dest.clone();
    let resumed_hash = tokio::task::spawn_blocking(move || {
        let mut f = std::fs::File::open(&res_c).unwrap();
        let mut hasher = blake3::Hasher::new();
        std::io::copy(&mut f, &mut hasher).unwrap();
        hasher.finalize().to_hex().to_string()
    })
    .await
    .unwrap();
    assert_eq!(resumed_hash, expected_hash);

    client.shutdown().await;
    host.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir_c);
    let _ = std::fs::remove_dir_all(&dir_h);
    let _ = std::fs::remove_dir_all(&work);
}
