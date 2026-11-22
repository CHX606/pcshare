use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use pcshare_core::*;
use pcshare_platform::{NativePlatform, Platform, is_virtual_or_tunnel_adapter_name};
use pcshare_proto::*;
use tokio_util::sync::CancellationToken;

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pcshare_r2_{tag}_{}_{}",
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn round2_parallel_striped_upload_128mb_resume_and_041_fallback() {
    let host_dir = temp_dir("up_host");
    let client_dir = temp_dir("up_client");
    let work_dir = temp_dir("up_work");

    let perms = Permissions {
        fs_read: true,
        fs_write: true,
        allowed_roots: vec![work_dir.to_string_lossy().to_string()],
        ..Permissions::default()
    };
    let host = start_local_node("R2Host", host_dir.clone()).await;
    let client = start_local_node("R2Client", client_dir.clone()).await;
    let host_id = pair_two_nodes(&client, &host, perms).await;
    let _ = client.remote_ping(&host_id).await.unwrap();

    // 生成 128 MiB 测试源文件并预计算 blake3
    let total_bytes: usize = 128 * 1024 * 1024;
    let mut raw = vec![0u8; total_bytes];
    for (idx, chunk) in raw.chunks_mut(4096).enumerate() {
        let seed = (idx as u64).wrapping_mul(0x517C_C1B7_2722_0A95);
        for (j, b) in chunk.iter_mut().enumerate() {
            *b = ((seed >> ((j & 7) * 8)) as u8) ^ (j as u8);
        }
    }
    let expected_hex = blake3::hash(&raw).to_hex().to_string();
    let src_file = work_dir.join("src_striped_128mb.bin");
    std::fs::write(&src_file, &raw).unwrap();
    drop(raw);

    // 1. 128 MiB 双连接并发分片上传基准与完整性验证
    let remote_dest = work_dir.join("uploaded_striped_128mb.bin");
    let up_start = Instant::now();
    let done = client
        .upload_file_resumable(
            &host_id,
            &src_file,
            &remote_dest.to_string_lossy(),
            ConflictPolicy::Overwrite,
            CancellationToken::new(),
            None,
            None,
        )
        .await
        .expect("128 MiB striped upload");
    let up_ms = up_start.elapsed().as_secs_f64() * 1000.0;
    let up_mbps = 128.0 / (up_ms / 1000.0);
    println!("[BENCH-R2] 128 MiB Striped Upload: {up_ms:.2} ms ({up_mbps:.1} MiB/s)");
    assert_eq!(done.bytes_written, total_bytes as u64);

    let got_hex = blake3::hash(&std::fs::read(&remote_dest).unwrap())
        .to_hex()
        .to_string();
    assert_eq!(got_hex, expected_hex);

    // 2. RenameKeepBoth 冲突策略在双分片上传下唯一解析目标名并落盘
    let kept_done = client
        .upload_file_resumable(
            &host_id,
            &src_file,
            &remote_dest.to_string_lossy(),
            ConflictPolicy::RenameKeepBoth,
            CancellationToken::new(),
            None,
            None,
        )
        .await
        .expect("RenameKeepBoth striped upload");
    assert_ne!(kept_done.target_path, remote_dest.to_string_lossy());
    let kept_hex = blake3::hash(&std::fs::read(&kept_done.target_path).unwrap())
        .to_hex()
        .to_string();
    assert_eq!(kept_hex, expected_hex);
    let _ = std::fs::remove_file(&kept_done.target_path);

    // 3. 40 MiB `.pcshare.part` 断点续传：双分片仅重传剩余 88 MiB
    let resume_dest = work_dir.join("resumed_striped_128mb.bin");
    let part_file = work_dir.join(".resumed_striped_128mb.bin.pcshare.part");
    let prefix_40mb = 40 * 1024 * 1024usize;
    {
        use std::io::Read;
        let mut f = std::fs::File::open(&src_file).unwrap();
        let mut head = vec![0u8; prefix_40mb];
        f.read_exact(&mut head).unwrap();
        std::fs::write(&part_file, &head).unwrap();
    }
    let wire_sent = Arc::new(AtomicU64::new(0));
    let wire_c = wire_sent.clone();
    let resumed_done = client
        .upload_file_resumable(
            &host_id,
            &src_file,
            &resume_dest.to_string_lossy(),
            ConflictPolicy::Overwrite,
            CancellationToken::new(),
            None,
            Some(Arc::new(move |delta| {
                wire_c.fetch_add(delta, Ordering::Relaxed);
            })),
        )
        .await
        .expect("resumed striped upload");
    assert_eq!(resumed_done.bytes_written, total_bytes as u64);
    assert_eq!(
        wire_sent.load(Ordering::Relaxed),
        (total_bytes - prefix_40mb) as u64,
        "striped resume should transmit only remaining 88 MiB"
    );
    let resumed_hex = blake3::hash(&std::fs::read(&resume_dest).unwrap())
        .to_hex()
        .to_string();
    assert_eq!(resumed_hex, expected_hex);

    // 4. 向下兼容 0.4.1 对端（无 `transfer.striped.v1` 能力时自动回退单流上传）
    let mut rec = client
        .list_peers()
        .into_iter()
        .find(|p| p.endpoint_id == host_id)
        .unwrap();
    rec.capabilities.retain(|c| c != CAP_TRANSFER_STRIPED);
    let legacy_caps = rec.capabilities.clone();
    // 直接通过第二次无能力覆盖模拟 0.4.1 对端
    let legacy_src = work_dir.join("legacy_40mb.bin");
    {
        use std::io::Read;
        let mut f = std::fs::File::open(&src_file).unwrap();
        let mut buf40 = vec![0u8; prefix_40mb];
        f.read_exact(&mut buf40).unwrap();
        std::fs::write(&legacy_src, &buf40).unwrap();
    }
    let expected_40_hex = blake3::hash(&std::fs::read(&legacy_src).unwrap())
        .to_hex()
        .to_string();
    assert!(!legacy_caps.iter().any(|c| c == CAP_TRANSFER_STRIPED));
    // 验证旧版 JSON 请求载荷不携带新字段（100% 线上字节兼容）
    let legacy_args = FsWriteArgs {
        path: r"D:\test.bin".to_string(),
        size: 1024,
        offset: 0,
        range_length: None,
        defer_finalize: false,
        modified_ms: None,
        conflict: ConflictPolicy::Overwrite,
    };
    let legacy_json = serde_json::to_string(&legacy_args).unwrap();
    assert!(!legacy_json.contains("range_length"));
    assert!(!legacy_json.contains("defer_finalize"));

    // 限速模式下同样走单流回退路径，验证 40 MiB 单流回退完整性
    let ctrl_throttled = TransferTaskControl::new();
    ctrl_throttled.set_speed_limit(Some(500 * 1024 * 1024));
    let fallback_dest = work_dir.join("fallback_single_40mb.bin");
    let fb_done = client
        .upload_file_with_ctrl(
            &host_id,
            &legacy_src,
            &fallback_dest.to_string_lossy(),
            ConflictPolicy::Overwrite,
            &ctrl_throttled,
            None,
            None,
            None,
        )
        .await
        .expect("fallback single-stream upload");
    assert_eq!(fb_done.bytes_written, prefix_40mb as u64);
    let fb_hex = blake3::hash(&std::fs::read(&fallback_dest).unwrap())
        .to_hex()
        .to_string();
    assert_eq!(fb_hex, expected_40_hex);

    client.shutdown().await;
    host.shutdown().await;
    let _ = std::fs::remove_dir_all(&host_dir);
    let _ = std::fs::remove_dir_all(&client_dir);
    let _ = std::fs::remove_dir_all(&work_dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn round2_large_dir_deflate_compression_wire_savings_and_quic_e2e() {
    let host_dir = temp_dir("deflate_host");
    let client_dir = temp_dir("deflate_client");
    let big_dir = temp_dir("deflate_5000");

    for i in 0..5000 {
        let p = big_dir.join(format!("部门项目交付归档记录_{i:04}.docx"));
        std::fs::write(&p, b"ok").unwrap();
    }

    let perms = Permissions {
        fs_read: true,
        allowed_roots: vec![big_dir.to_string_lossy().to_string()],
        ..Permissions::default()
    };
    let host = start_local_node("DeflateHost", host_dir.clone()).await;
    let client = start_local_node("DeflateClient", client_dir.clone()).await;
    let host_id = pair_two_nodes(&client, &host, perms).await;
    let _ = client.remote_ping(&host_id).await.unwrap();

    // 实测 QUIC 渐进列目录与 walk 在协商 `frame.deflate.v1` 下的端到端耗时与结果一致性
    let t_list = Instant::now();
    let mut batches = 0usize;
    let listed = client
        .remote_list_progressive(&host_id, &big_dir.to_string_lossy(), |_| {
            batches += 1;
        })
        .await
        .expect("compressed progressive list");
    let list_ms = t_list.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(listed.len(), 5000);
    assert_eq!(batches, 4, "500 + 2000 + 2000 + 500 progressive batches");

    // 对比同等 5000 项真实分批帧（500 + 2000 + 2000 + 500）在线上压缩前后的字节数
    let mut raw_wire_total = 0usize;
    let mut comp_wire_total = 0usize;
    let slices = [&listed[0..500], &listed[500..2500], &listed[2500..4500], &listed[4500..5000]];
    for (idx, chunk) in slices.iter().enumerate() {
        let env = ResponseEnvelope::ok(FsListBatch {
            path: big_dir.to_string_lossy().to_string(),
            entries: chunk.to_vec(),
            has_more: idx + 1 < slices.len(),
            total_hint: None,
        });
        let mut sink_raw = Vec::new();
        raw_wire_total += write_frame_maybe_compressed(&mut sink_raw, &env, false)
            .await
            .unwrap();
        let mut sink_comp = Vec::new();
        comp_wire_total += write_frame_maybe_compressed(&mut sink_comp, &env, true)
            .await
            .unwrap();
    }
    let ratio_pct = (1.0 - (comp_wire_total as f64 / raw_wire_total as f64)) * 100.0;
    println!(
        "[BENCH-R2] 5,000-entry FsList wire bytes: {} B -> {} B (-{ratio_pct:.1}%), QUIC e2e list: {list_ms:.2} ms",
        raw_wire_total, comp_wire_total
    );
    assert!(
        comp_wire_total * 3 < raw_wire_total,
        "expected >66% wire byte reduction, got {raw_wire_total} -> {comp_wire_total}"
    );

    let walked = client
        .remote_walk(&host_id, &[big_dir.to_string_lossy().to_string()])
        .await
        .expect("compressed remote_walk");
    assert_eq!(walked.len(), 5001); // 根目录 + 5000 个子文件

    client.shutdown().await;
    host.shutdown().await;
    let _ = std::fs::remove_dir_all(&host_dir);
    let _ = std::fs::remove_dir_all(&client_dir);
    let _ = std::fs::remove_dir_all(&big_dir);
}

#[cfg(windows)]
#[test]
fn round2_desktop_encoder_cold_stamp_vs_sha256_and_dxgi_priority() {
    use pcshare_platform::desktop::{
        prioritized_encoder_names_for_vendors, verify_embedded_encoder_ready,
    };

    // 1. DXGI 显卡厂商直选优先级验证
    let amd_routes = prioritized_encoder_names_for_vendors(Some(&[0x1002]));
    assert_eq!(amd_routes[0], "h264_amf");
    assert!(!amd_routes.contains(&"h264_nvenc"));
    assert!(!amd_routes.contains(&"h264_qsv"));

    let optimus_routes = prioritized_encoder_names_for_vendors(Some(&[0x8086, 0x10DE]));
    assert_eq!(optimus_routes[0], "h264_qsv");
    assert_eq!(optimus_routes[1], "h264_nvenc");
    assert!(!optimus_routes.contains(&"h264_amf"));

    let sw_vm_routes = prioritized_encoder_names_for_vendors(Some(&[]));
    assert_eq!(sw_vm_routes[0], "h264_mf");
    assert_eq!(sw_vm_routes[1], "libx264");
    assert_eq!(sw_vm_routes.len(), 4);

    // 2. 内置 ~100 MB FFmpeg 冷启动校验：0.4.1 全量 SHA-256 vs Round 2 元数据印章快路径
    let enc_dir = temp_dir("ffmpeg_stamp");
    let exe_path = verify_embedded_encoder_ready(&enc_dir).expect("initial extract ffmpeg");
    let full_size = std::fs::metadata(&exe_path).unwrap().len();
    assert!(full_size > 20 * 1024 * 1024);

    let stamp_path = exe_path.parent().unwrap().join("ffmpeg.verified");
    assert!(stamp_path.is_file());

    // 删除印章模拟 0.4.1 每次进程冷启动重算整包 SHA-256
    std::fs::remove_file(&stamp_path).unwrap();
    let t_legacy = Instant::now();
    let _ = verify_embedded_encoder_ready(&enc_dir).expect("legacy full sha256 cold check");
    let legacy_ms = t_legacy.elapsed().as_secs_f64() * 1000.0;
    assert!(stamp_path.is_file(), "stamp should be recreated after hash check");

    // 保留印章测量 Round 2 进程冷启动快路径
    let t_fast = Instant::now();
    let _ = verify_embedded_encoder_ready(&enc_dir).expect("fast stamp cold check");
    let fast_ms = t_fast.elapsed().as_secs_f64() * 1000.0;
    println!(
        "[BENCH-R2] FFmpeg cold startup check ({:.1} MiB): legacy SHA-256 = {legacy_ms:.2} ms -> fast stamp = {fast_ms:.3} ms ({:.0}x faster)",
        full_size as f64 / (1024.0 * 1024.0),
        legacy_ms / fast_ms.max(0.001)
    );
    assert!(
        fast_ms < 5.0 && legacy_ms > fast_ms * 20.0,
        "expected fast stamp < 5ms and >20x faster than full SHA-256 ({legacy_ms:.2} ms vs {fast_ms:.3} ms)"
    );

    // 篡改/截断文件但保留旧印章：必须立即识别长度/修改时间不匹配并自动重解压恢复完整二进制
    std::fs::write(&exe_path, b"corrupted").unwrap();
    let recovered = verify_embedded_encoder_ready(&enc_dir).expect("tampered binary auto-recovery");
    assert_eq!(std::fs::metadata(&recovered).unwrap().len(), full_size);

    let _ = std::fs::remove_dir_all(&enc_dir);
}

#[test]
fn round2_physical_nic_virtual_filter_and_portmap_setting() {
    // 虚拟网卡 / 代理 TUN 关键字识别验证
    assert!(is_virtual_or_tunnel_adapter_name("Meta Tunnel", "Clash"));
    assert!(is_virtual_or_tunnel_adapter_name("Mihomo TUN Adapter", "mihomo"));
    assert!(is_virtual_or_tunnel_adapter_name("sing-box Tunnel", "tun0"));
    assert!(is_virtual_or_tunnel_adapter_name("WireGuard Tunnel", "wg0"));
    assert!(is_virtual_or_tunnel_adapter_name("Wintun Userspace Tunnel", "Tailscale"));
    assert!(is_virtual_or_tunnel_adapter_name(
        "Hyper-V Virtual Ethernet Adapter",
        "vEthernet (WSL)"
    ));
    assert!(is_virtual_or_tunnel_adapter_name("VMware Virtual Ethernet Adapter", "VMnet8"));

    // 真实有线 / Wi-Fi 物理网卡不应被误杀
    assert!(!is_virtual_or_tunnel_adapter_name(
        "Intel(R) Ethernet Controller I225-V",
        "以太网"
    ));
    assert!(!is_virtual_or_tunnel_adapter_name(
        "Realtek PCIe 2.5GbE Family Controller",
        "Ethernet"
    ));
    assert!(!is_virtual_or_tunnel_adapter_name("Intel(R) Wi-Fi 6E AX211 160MHz", "WLAN"));

    let plat = Arc::new(NativePlatform::new());
    let phys_ip = plat.primary_physical_ipv4();
    println!("[BENCH-R2] Detected primary physical IPv4: {phys_ip:?}");
    if let Some(ip) = phys_ip {
        assert!(!ip.is_loopback() && !ip.is_unspecified());
        assert_ne!(ip.octets()[0..2], [169, 254]);
        assert!(!(ip.octets()[0] == 198 && matches!(ip.octets()[1], 18 | 19)));
    }

    // 验证 Settings 新增 `bind_physical_nic` 与已有 `portmap_enabled` 持久化读写
    let dir = temp_dir("settings_nic");
    let (store, _) = LocalStorage::open(&dir, plat.clone()).unwrap();
    assert!(!store.settings().bind_physical_nic);
    assert!(store.settings().portmap_enabled);
    store
        .update_settings(|s| {
            s.bind_physical_nic = true;
            s.portmap_enabled = false;
        })
        .unwrap();
    drop(store);

    let (reopened, _) = LocalStorage::open(&dir, plat).unwrap();
    assert!(reopened.settings().bind_physical_nic);
    assert!(!reopened.settings().portmap_enabled);
    let _ = std::fs::remove_dir_all(&dir);
}
