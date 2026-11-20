//! 任务 2.9：同一局域网自动发现（基于标准 mDNS / DNS-SD 组播报文 `_pcshare._udp.local`）
//! - `iroh 1.4.1` 主库不含局域网发现模块，故使用 `socket2` + `simple-dns` 广播并监听 DNS-SD TXT 记录。
//! - 发现到的陌生设备仅提供身份码、设备名与局域网直连地址提示，仍须走配对确认才能连接。

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use iroh::EndpointId;
use serde::{Deserialize, Serialize};
use simple_dns::rdata::{RData, TXT};
use simple_dns::{CLASS, Name, Packet, Question, ResourceRecord, TYPE};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::error::CoreError;
use crate::events::CoreEvent;

pub const MDNS_MULTICAST_V4: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
pub const MDNS_DEFAULT_PORT: u16 = 5353;
const SERVICE_DOMAIN: &str = "_pcshare._udp.local";
const ANNOUNCE_INTERVAL: Duration = Duration::from_secs(4);
const PEER_TTL: Duration = Duration::from_secs(25);
const MAX_LAN_IFACES: usize = 16;
const MAX_SEEN_LAN_PEERS: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LanPeerAnnouncement {
    pub endpoint_id: String,
    pub device_name: String,
    pub os: String,
    pub addr: SocketAddr,
    pub discovered_at_ms: u64,
}

struct SeenPeer {
    announcement: LanPeerAnnouncement,
    last_seen: Instant,
}

pub struct LanDiscovery {
    self_id: String,
    device_name: Arc<Mutex<String>>,
    iroh_port: u16,
    local_ipv4s: Mutex<Vec<Ipv4Addr>>,
    seen: Arc<Mutex<HashMap<String, SeenPeer>>>,
    events_tx: broadcast::Sender<CoreEvent>,
    worker_cancel: Mutex<Option<CancellationToken>>,
}

impl LanDiscovery {
    pub fn new(
        self_id: String,
        device_name: String,
        iroh_port: u16,
        events_tx: broadcast::Sender<CoreEvent>,
    ) -> Arc<Self> {
        Self::new_with_ifaces(self_id, device_name, iroh_port, Vec::new(), events_tx)
    }

    pub fn new_with_ifaces(
        self_id: String,
        device_name: String,
        iroh_port: u16,
        local_ipv4s: Vec<Ipv4Addr>,
        events_tx: broadcast::Sender<CoreEvent>,
    ) -> Arc<Self> {
        Arc::new(Self {
            self_id,
            device_name: Arc::new(Mutex::new(device_name)),
            iroh_port,
            local_ipv4s: Mutex::new(local_ipv4s),
            seen: Arc::new(Mutex::new(HashMap::new())),
            events_tx,
            worker_cancel: Mutex::new(None),
        })
    }

    pub fn set_local_ipv4s(&self, local_ipv4s: Vec<Ipv4Addr>) {
        *self
            .local_ipv4s
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = local_ipv4s;
    }

    pub fn update_device_name(&self, next_name: String) {
        *self.device_name.lock().unwrap_or_else(|e| e.into_inner()) = next_name;
    }

    pub fn start_default(self: &Arc<Self>) -> Result<(), CoreError> {
        self.start_on_port(MDNS_DEFAULT_PORT)
    }

    fn collect_lan_ipv4s(&self) -> Vec<Ipv4Addr> {
        let mut seen_ips = HashSet::new();
        let mut privates = Vec::new();
        let mut others = Vec::new();

        let mut push_ip = |ip: Ipv4Addr| {
            if ip.is_loopback() || ip.is_unspecified() || ip.is_link_local() || ip.is_multicast() {
                return;
            }
            if !seen_ips.insert(ip) {
                return;
            }
            if ip.is_private() {
                privates.push(ip);
            } else {
                others.push(ip);
            }
        };

        for ip in self
            .local_ipv4s
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .copied()
        {
            push_ip(ip);
        }

        // 通过本地路由表探测补充默认出口 IPv4（不发包，仅查路由）
        for probe in ["192.168.0.1:80", "10.255.255.254:80", "172.16.0.1:80", "192.0.2.1:80"] {
            if let Ok(s) = std::net::UdpSocket::bind("0.0.0.0:0")
                && s.connect(probe).is_ok()
                && let Ok(SocketAddr::V4(v4)) = s.local_addr()
            {
                push_ip(*v4.ip());
            }
        }

        privates.extend(others);
        privates.truncate(MAX_LAN_IFACES);
        privates
    }

    pub fn start_on_port(self: &Arc<Self>, port: u16) -> Result<(), CoreError> {
        self.stop();

        let lan_ifaces = self.collect_lan_ipv4s();

        let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
            .map_err(CoreError::Io)?;
        let _ = sock.set_reuse_address(true);
        let _ = sock.set_broadcast(true);
        let _ = sock.set_multicast_loop_v4(true);
        let _ = sock.set_multicast_ttl_v4(255);
        sock.set_nonblocking(true)?;
        sock.bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port).into())?;

        // 遍历本机各局域网 IPv4 接口分别加入组播组，防止多网卡或 TUN 虚拟网卡导致漏收
        for iface_ip in &lan_ifaces {
            let _ = sock.join_multicast_v4(&MDNS_MULTICAST_V4, iface_ip);
        }
        let _ = sock.join_multicast_v4(&MDNS_MULTICAST_V4, &Ipv4Addr::UNSPECIFIED);
        let _ = sock.join_multicast_v4(&MDNS_MULTICAST_V4, &Ipv4Addr::LOCALHOST);

        let std_udp: std::net::UdpSocket = sock.into();
        let udp = Arc::new(UdpSocket::from_std(std_udp)?);

        // 为每个局域网 IPv4 接口准备独立的组播发送套接字，避开虚拟网卡接管默认路由
        let mut iface_tx_sockets: Vec<Arc<UdpSocket>> = Vec::new();
        for iface_ip in &lan_ifaces {
            if let Ok(tx_sock) = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
                && tx_sock.set_nonblocking(true).is_ok()
                && tx_sock.set_multicast_loop_v4(true).is_ok()
                && tx_sock.set_multicast_ttl_v4(255).is_ok()
                && tx_sock.set_multicast_if_v4(iface_ip).is_ok()
                && tx_sock.bind(&SocketAddrV4::new(*iface_ip, 0).into()).is_ok()
            {
                let std_tx: std::net::UdpSocket = tx_sock.into();
                if let Ok(tokio_tx) = UdpSocket::from_std(std_tx) {
                    iface_tx_sockets.push(Arc::new(tokio_tx));
                }
            }
        }
        let iface_tx_sockets = Arc::new(iface_tx_sockets);

        let cancel = CancellationToken::new();
        *self
            .worker_cancel
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(cancel.clone());

        let mcast_target = SocketAddr::V4(SocketAddrV4::new(MDNS_MULTICAST_V4, port));
        let lo_target = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port));

        let disc = self.clone();
        let udp_tx = udp.clone();
        let iface_tx_for_loop = iface_tx_sockets.clone();
        let cancel_tx = cancel.clone();
        tokio::spawn(async move {
            // 启动时先发一次查询 + 两次广播，加快局域网设备互相看见的速度
            if let Some(q) = build_mdns_query_bytes() {
                send_multicast_all(&udp_tx, &iface_tx_for_loop, &q, mcast_target, lo_target).await;
            }
            disc.send_announcement(&udp_tx, &iface_tx_for_loop, mcast_target, lo_target)
                .await;
            tokio::time::sleep(Duration::from_millis(250)).await;
            disc.send_announcement(&udp_tx, &iface_tx_for_loop, mcast_target, lo_target)
                .await;

            let mut ticker = tokio::time::interval(ANNOUNCE_INTERVAL);
            loop {
                tokio::select! {
                    _ = cancel_tx.cancelled() => break,
                    _ = ticker.tick() => {
                        disc.prune_expired();
                        disc.send_announcement(&udp_tx, &iface_tx_for_loop, mcast_target, lo_target).await;
                    }
                }
            }
        });

        let disc_rx = self.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 4096];
            loop {
                let recv_res = tokio::select! {
                    _ = cancel.cancelled() => break,
                    r = udp.recv_from(&mut buf) => r,
                };
                let Ok((n, src_addr)) = recv_res else {
                    continue;
                };
                disc_rx
                    .handle_packet(
                        &buf[..n],
                        src_addr,
                        &udp,
                        &iface_tx_sockets,
                        mcast_target,
                        lo_target,
                    )
                    .await;
            }
        });

        Ok(())
    }

    pub fn discovered_peers(&self) -> Vec<LanPeerAnnouncement> {
        self.prune_expired();
        let guard = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        let mut list: Vec<LanPeerAnnouncement> =
            guard.values().map(|s| s.announcement.clone()).collect();
        list.sort_by(|a, b| a.device_name.cmp(&b.device_name));
        list
    }

    pub fn stop(&self) {
        if let Some(c) = self
            .worker_cancel
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            c.cancel();
        }
    }

    fn prune_expired(&self) {
        let mut guard = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        let before = guard.len();
        guard.retain(|_, v| v.last_seen.elapsed() < PEER_TTL);
        if guard.len() != before {
            let _ = self.events_tx.send(CoreEvent::LanPeersChanged);
        }
    }

    async fn send_announcement(
        &self,
        udp: &UdpSocket,
        iface_tx_sockets: &[Arc<UdpSocket>],
        mcast_target: SocketAddr,
        lo_target: SocketAddr,
    ) {
        if self.iroh_port == 0 {
            return;
        }
        let name = self
            .device_name
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(pkt) = build_mdns_announce_bytes(&self.self_id, &name, self.iroh_port) {
            send_multicast_all(udp, iface_tx_sockets, &pkt, mcast_target, lo_target).await;
        }
    }

    async fn handle_packet(
        &self,
        raw: &[u8],
        src_addr: SocketAddr,
        udp: &UdpSocket,
        iface_tx_sockets: &[Arc<UdpSocket>],
        mcast_target: SocketAddr,
        lo_target: SocketAddr,
    ) {
        let Ok(pkt) = Packet::parse(raw) else {
            return;
        };

        // 如果收到同网段对 `_pcshare._udp.local` 的查询，立即回应一次本机公告
        let has_our_query = pkt
            .questions
            .iter()
            .any(|q| q.qname.to_string().trim_end_matches('.') == SERVICE_DOMAIN);
        if has_our_query {
            self.send_announcement(udp, iface_tx_sockets, mcast_target, lo_target)
                .await;
        }

        for rr in pkt.answers.iter().chain(pkt.additional_records.iter()) {
            if rr.name.to_string().trim_end_matches('.') != SERVICE_DOMAIN {
                continue;
            }
            let RData::TXT(txt) = &rr.rdata else {
                continue;
            };
            let mut id_opt = None;
            let mut name_opt = None;
            let mut os_opt = None;
            let mut port_opt = None;

            for (k, v_opt) in txt.iter_raw() {
                let Some(v) = v_opt else { continue };
                let val_str = String::from_utf8_lossy(v).into_owned();
                match k {
                    b"id" => id_opt = Some(val_str),
                    b"name" => name_opt = Some(val_str),
                    b"os" => os_opt = Some(val_str),
                    b"port" => port_opt = val_str.parse::<u16>().ok(),
                    _ => {}
                }
            }

            let (Some(peer_id), Some(dev_name), Some(port)) = (id_opt, name_opt, port_opt) else {
                continue;
            };
            if peer_id == self.self_id || port == 0 || peer_id.parse::<EndpointId>().is_err() {
                continue;
            }

            let ip = match src_addr.ip() {
                IpAddr::V4(v4) if v4.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
                other => other,
            };
            let peer_sock = SocketAddr::new(ip, port);
            let now_ms = chrono::Utc::now().timestamp_millis() as u64;
            let ann = LanPeerAnnouncement {
                endpoint_id: peer_id.clone(),
                device_name: dev_name,
                os: os_opt.unwrap_or_else(|| "windows".to_string()),
                addr: peer_sock,
                discovered_at_ms: now_ms,
            };

            let changed = {
                let mut guard = self.seen.lock().unwrap_or_else(|e| e.into_inner());
                if !guard.contains_key(&peer_id) && guard.len() >= MAX_SEEN_LAN_PEERS {
                    guard.retain(|_, v| v.last_seen.elapsed() < PEER_TTL);
                    if guard.len() >= MAX_SEEN_LAN_PEERS {
                        continue;
                    }
                }
                let is_new_or_diff = guard.get(&peer_id).is_none_or(|old| {
                    old.announcement.device_name != ann.device_name
                        || old.announcement.addr != ann.addr
                });
                guard.insert(
                    peer_id,
                    SeenPeer {
                        announcement: ann,
                        last_seen: Instant::now(),
                    },
                );
                is_new_or_diff
            };
            if changed {
                let _ = self.events_tx.send(CoreEvent::LanPeersChanged);
            }
        }
    }
}

async fn send_multicast_all(
    main_udp: &UdpSocket,
    iface_tx_sockets: &[Arc<UdpSocket>],
    pkt: &[u8],
    mcast_target: SocketAddr,
    lo_target: SocketAddr,
) {
    for tx in iface_tx_sockets {
        let _ = tx.send_to(pkt, mcast_target).await;
    }
    let _ = main_udp.send_to(pkt, mcast_target).await;
    let _ = main_udp.send_to(pkt, lo_target).await;
}

fn build_mdns_query_bytes() -> Option<Vec<u8>> {
    let mut pkt = Packet::new_query(0);
    let q = Question::new(
        Name::new_unchecked(SERVICE_DOMAIN),
        TYPE::TXT.into(),
        CLASS::IN.into(),
        false,
    );
    pkt.questions.push(q);
    pkt.build_bytes_vec().ok()
}

fn build_mdns_announce_bytes(self_id: &str, device_name: &str, iroh_port: u16) -> Option<Vec<u8>> {
    let id_kv = format!("id={self_id}");
    // 控制 TXT 单条字符串在 200 字节内，防止超长主机名超出 DNS 单段 255 字节上限
    let safe_name: String = device_name.chars().take(48).collect();
    let name_kv = format!("name={safe_name}");
    let os_kv = format!("os={}", std::env::consts::OS);
    let port_kv = format!("port={iroh_port}");

    let mut txt = TXT::new();
    txt.add_string(&id_kv).ok()?;
    txt.add_string(&name_kv).ok()?;
    txt.add_string(&os_kv).ok()?;
    txt.add_string(&port_kv).ok()?;

    let mut pkt = Packet::new_reply(0);
    pkt.answers.push(ResourceRecord::new(
        Name::new_unchecked(SERVICE_DOMAIN),
        CLASS::IN,
        30,
        RData::TXT(txt),
    ));
    pkt.build_bytes_vec().ok()
}
