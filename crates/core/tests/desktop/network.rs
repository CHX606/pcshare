use super::super::*;
use crate::desktop::DesktopSession;
use iroh::endpoint::transports::{FourTuple, PathSelection, PathSelectionContext, PathSelector};
use pcshare_platform::NativePlatform;
use pcshare_proto::desktop::Source;
#[path = "cleanup.rs"]
mod cleanup;
use pcshare_proto::Permissions;
use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    net::UdpSocket,
    sync::mpsc,
    time::{Instant, sleep_until},
};
use tokio_util::sync::CancellationToken;

// This relay accepts packets only from the two test nodes bound to loopback.
struct Link {
    addr: SocketAddr,
    slow: Arc<AtomicBool>,
    shaped: Arc<AtomicU64>,
    stop: CancellationToken,
}
impl Drop for Link {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

impl Link {
    async fn start(socket: Arc<UdpSocket>, client: SocketAddr, host: SocketAddr) -> Self {
        let addr = socket.local_addr().unwrap();
        let slow = Arc::new(AtomicBool::new(false));
        let shaped = Arc::new(AtomicU64::new(0));
        let stop = CancellationToken::new();
        let (tx, rx) = mpsc::channel(96);
        let worker = socket.clone();
        let cancel = stop.clone();
        tokio::spawn(async move {
            tokio::select! {_=cancel.cancelled()=>{},_=deliver(worker,client,rx)=>{}}
        });
        let s = slow.clone();
        let bytes = shaped.clone();
        let cancel = stop.clone();
        tokio::spawn(async move {
            tokio::select! {_=cancel.cancelled()=>{},_=route(socket,client,host,s,bytes,tx)=>{}}
        });
        Self {
            addr,
            slow,
            shaped,
            stop,
        }
    }
}

async fn route(
    socket: Arc<UdpSocket>,
    client: SocketAddr,
    host: SocketAddr,
    slow: Arc<AtomicBool>,
    shaped: Arc<AtomicU64>,
    tx: mpsc::Sender<(Instant, Vec<u8>)>,
) {
    let mut buf = vec![0; 65536];
    let mut free_at = Instant::now();
    loop {
        let (len, from) = socket.recv_from(&mut buf).await.unwrap();
        if from == client {
            socket.send_to(&buf[..len], host).await.unwrap();
            continue;
        }
        if from != host {
            continue;
        }
        if !slow.load(Ordering::Acquire) {
            socket.send_to(&buf[..len], client).await.unwrap();
            continue;
        }
        let now = Instant::now();
        free_at = free_at.max(now);
        let time = Duration::from_secs_f64(len as f64 * 8.0 / 1_000_000.0);
        let due = free_at + time + Duration::from_millis(270);
        if tx.try_send((due, buf[..len].to_vec())).is_ok() {
            free_at += time;
            shaped.fetch_add(len as u64, Ordering::Relaxed);
        }
    }
}

async fn deliver(
    socket: Arc<UdpSocket>,
    client: SocketAddr,
    mut rx: mpsc::Receiver<(Instant, Vec<u8>)>,
) {
    while let Some((due, packet)) = rx.recv().await {
        sleep_until(due).await;
        socket.send_to(&packet, client).await.unwrap();
    }
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "真实 Windows 桌面和本机可控 UDP 链路"]
async fn video_switches_on_a_bandwidth_and_delay_limited_quic_path() {
    let dir = |role| {
        std::env::temp_dir().join(format!(
            "pcshare_net_{role}_{}_{}",
            std::process::id(),
            fastrand::u64(..)
        ))
    };
    let dirs = [dir("view"), dir("host")];
    let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let pin = socket.local_addr().unwrap();
    let client = pinned_node(dirs[0].clone(), pin).await;
    let host = pinned_node(dirs[1].clone(), pin).await;
    let addr = |n: &CoreNode| {
        n.local_socket_addrs()
            .into_iter()
            .find(|a| a.is_ipv4())
            .unwrap()
    };
    let link = Link::start(socket, addr(&client), addr(&host)).await;
    authorize(&client, &host, link.addr);
    authorize(&host, &client, link.addr);
    let id = host.endpoint_id().to_string();
    let session = DesktopSession::open(&client, &id, Source::Monitor(0))
        .await
        .unwrap();
    assert_eq!(session.next().await.unwrap().info.mode, "step");
    assert_eq!(session.next().await.unwrap().info.mode, "live");
    link.slow.store(true, Ordering::Release);
    let started = Instant::now();
    let mut jpeg = Vec::new();
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let f = session.next().await.unwrap();
            if f.info.mode == "step" {
                jpeg = f.bytes;
                break;
            }
        }
    })
    .await
    .expect("QUIC link impairment did not cause fallback");
    assert!(jpeg.starts_with(&[255, 216]));
    let fallback = started.elapsed();
    let (_, rtt) = client.remote_ping(&id).await.unwrap();
    assert!(
        rtt >= Duration::from_millis(200),
        "measured RTT {rtt:?}; the test path was bypassed"
    );
    assert!(link.shaped.load(Ordering::Relaxed) > jpeg.len() as u64);
    assert!(
        tokio::time::timeout(Duration::from_millis(400), session.next())
            .await
            .is_err()
    );
    let restored = Instant::now();
    link.slow.store(false, Ordering::Release);
    let resumed = tokio::time::timeout(Duration::from_secs(40), session.next())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resumed.info.mode, "live");
    assert!(resumed.info.key);
    println!(
        "desktop UDP: 1 Mbps +270ms downstream; measured RTT {:.0}ms; fallback {:.2}s; recovery {:.2}s",
        rtt.as_secs_f64() * 1000.0,
        fallback.as_secs_f64(),
        restored.elapsed().as_secs_f64()
    );
    session.close();
    drop(session);
    drop(link);
    client.shutdown().await;
    host.shutdown().await;
    cleanup::remove_dirs(dirs).await;
}

#[derive(Debug)]
struct Pinned(SocketAddr);
impl PathSelector for Pinned {
    fn select(&self, ctx: &PathSelectionContext<'_>) -> PathSelection {
        let mut selection = PathSelection::none();
        if let Some(p) = ctx
            .paths()
            .find(|p| matches!(p.network_path(),FourTuple::Ip{remote,..} if *remote==self.0))
        {
            selection.set(&p);
        }
        selection
    }
}
async fn pinned_node(dir: PathBuf, proxy: SocketAddr) -> Arc<CoreNode> {
    let platform: Arc<dyn Platform> = Arc::new(NativePlatform::new());
    let (storage, warnings) = LocalStorage::open(&dir, platform.clone()).unwrap();
    let endpoint = Endpoint::builder(presets::Minimal)
        .secret_key(storage.secret_key().clone())
        .alpns(vec![
            ALPN_MAIN.to_vec(),
            ALPN_PAIR.to_vec(),
            ALPN_DIRECT.to_vec(),
        ])
        .clear_ip_transports()
        .bind_addr(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .path_selector(Arc::new(Pinned(proxy)))
        .bind()
        .await
        .unwrap();
    let cfg = CoreConfig {
        data_dir: dir,
        network_mode: NetworkMode::LocalTest,
        auto_connect_peers: false,
        device_name_override: None,
    };
    let node =
        CoreNode::with_endpoint(cfg, Arc::new(storage), platform, endpoint, warnings.clone());
    node.start_services(warnings);
    node
}

fn authorize(node: &CoreNode, peer: &CoreNode, relay: SocketAddr) {
    let record = PeerRecord {
        endpoint_id: peer.endpoint_id().to_string(),
        name: "测试对端".into(),
        os: Some("windows".into()),
        permissions: Permissions {
            screenshot: true,
            ..Default::default()
        },
        allow_incoming: true,
        connect_outgoing: false,
        relay_url: None,
        direct_addrs: vec![relay],
        mac_addresses: vec![],
        capabilities: node_capabilities(),
        paired_at_ms: 0,
        last_seen_ms: None,
    };
    node.storage.upsert_peer(record).unwrap();
}
