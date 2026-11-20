use super::*;

impl CoreNode {
    pub async fn start(
        config: CoreConfig,
        platform: Arc<dyn Platform>,
    ) -> Result<Arc<Self>, CoreError> {
        Self::start_with_bind_port(config, platform, None).await
    }

    pub async fn start_with_bind_port(
        config: CoreConfig,
        platform: Arc<dyn Platform>,
        bind_port_v4: Option<u16>,
    ) -> Result<Arc<Self>, CoreError> {
        let (storage, warnings) = LocalStorage::open(&config.data_dir, platform.clone())?;
        if let Some(ref name_override) = config.device_name_override {
            let _ = storage.update_settings(|s| {
                s.device_name = name_override.clone();
            });
        }
        let storage = Arc::new(storage);
        let endpoint = bind_endpoint(&config, &storage, bind_port_v4).await?;
        let node = Self::with_endpoint(config, storage, platform, endpoint, warnings.clone());
        node.start_services(warnings);
        Ok(node)
    }
    fn with_endpoint(
        config: CoreConfig,
        storage: Arc<LocalStorage>,
        platform: Arc<dyn Platform>,
        endpoint: Endpoint,
        warnings: Vec<String>,
    ) -> Arc<Self> {
        let settings = storage.settings();
        let clip_auto = settings.clipboard_auto_sync;
        let services = services(&config, &platform, &endpoint, settings);
        Arc::new(Self {
            desktop_input_lock: tokio::sync::Mutex::new(()),
            power_exec_lock: tokio::sync::Mutex::new(()),
            config,
            storage,
            platform,
            endpoint,
            fs_service: services.fs_service,
            dir_cache: services.dir_cache,
            fs_watch: services.fs_watch,
            lan_discovery: services.lan_discovery,
            rate_limiter: services.rate_limiter,
            edit_sync: services.edit_sync,
            apps_index: Mutex::new(HashMap::new()),
            active_power_cancel: Mutex::new(None),
            dry_run_power_actions: AtomicBool::new(false),
            last_applied_clip_seq: AtomicU32::new(services.initial_clip_seq),
            clipboard_auto_sync_enabled: AtomicBool::new(clip_auto),
            clipboard_auto_sync_notify: Arc::new(Notify::new()),
            host_paused: AtomicBool::new(false),
            direct_grants: Mutex::new(HashMap::new()),
            active_direct_transfers: Mutex::new(HashMap::new()),
            pairing: services.pairing,
            events_tx: services.events_tx,
            remote_events_tx: services.remote_events_tx,
            active_conns: RwLock::new(HashMap::new()),
            incoming_conns: RwLock::new(HashMap::new()),
            dial_locks: Mutex::new(HashMap::new()),
            peer_states: RwLock::new(HashMap::new()),
            peer_workers: Mutex::new(HashMap::new()),
            next_worker_id: AtomicU64::new(1),
            accept_handle: Mutex::new(None),
            shutdown_token: CancellationToken::new(),
            startup_warnings: warnings,
        })
    }
    fn start_services(self: &Arc<Self>, warnings: Vec<String>) {
        self.log_event("INFO", "核心节点已启动");
        if self.config.network_mode == NetworkMode::PublicOrCustom {
            let _ = self.lan_discovery.start_default();
        }

        for w in warnings {
            let _ = self
                .events_tx
                .send(CoreEvent::StorageWarning { message: w });
        }

        // 启动入站连接接收循环
        self.spawn_accept_loop();
        self.spawn_clipboard_autosync_loop();

        // 为所有已配对设备启动持久连接与自动重连任务
        for peer in self.storage.list_peers() {
            self.set_peer_state(&peer.endpoint_id, DeviceConnState::Offline { reason: None });
            if self.config.auto_connect_peers && peer.connect_outgoing {
                self.ensure_peer_worker(&peer.endpoint_id);
            }
        }
    }
}

struct NodeServices {
    fs_service: Arc<FsService>,
    dir_cache: Arc<DirCache>,
    fs_watch: Arc<FsWatchManager>,
    lan_discovery: Arc<LanDiscovery>,
    rate_limiter: Arc<RateLimiter>,
    edit_sync: Arc<EditSyncManager>,
    initial_clip_seq: u32,
    pairing: Arc<PairingManager>,
    events_tx: broadcast::Sender<CoreEvent>,
    remote_events_tx: broadcast::Sender<(String, RemoteEvent)>,
}
fn services(
    config: &CoreConfig,
    platform: &Arc<dyn Platform>,
    endpoint: &Endpoint,
    settings: Settings,
) -> NodeServices {
    let (events_tx, _) = broadcast::channel(256);
    let (remote_events_tx, _) = broadcast::channel(256);
    let fs_service = Arc::new(FsService::new(platform.clone(), remote_events_tx.clone()));
    let dir_cache = Arc::new(DirCache::load_or_new(&config.data_dir));
    let fs_watch = FsWatchManager::new(remote_events_tx.clone(), events_tx.clone());
    // 局域网 mDNS 使用 IPv4 组播，必须选用配对的 IPv4 监听端口，避免双栈下错用 IPv6 端口
    let bound_addrs: Vec<SocketAddr> = endpoint.addr().ip_addrs().copied().collect();
    let iroh_port = bound_addrs
        .iter()
        .find_map(|a| match a {
            SocketAddr::V4(v4) => Some(v4.port()),
            SocketAddr::V6(_) => None,
        })
        .unwrap_or(0);
    let local_ipv4s: Vec<Ipv4Addr> = bound_addrs
        .iter()
        .filter_map(|a| match a {
            SocketAddr::V4(v4) if !v4.ip().is_loopback() && !v4.ip().is_unspecified() => {
                Some(*v4.ip())
            }
            _ => None,
        })
        .collect();
    let lan_discovery = LanDiscovery::new_with_ifaces(
        endpoint.id().to_string(),
        settings.device_name.clone(),
        iroh_port,
        local_ipv4s,
        events_tx.clone(),
    );
    let edit_sync = EditSyncManager::new(&config.data_dir, events_tx.clone());
    let initial_clip_seq = platform.clipboard_sequence();

    NodeServices {
        fs_service,
        dir_cache,
        fs_watch,
        lan_discovery,
        rate_limiter: RateLimiter::new(None),
        edit_sync,
        initial_clip_seq,
        pairing: Arc::new(PairingManager::new()),
        events_tx,
        remote_events_tx,
    }
}

async fn bind_endpoint(
    config: &CoreConfig,
    storage: &LocalStorage,
    bind_port_v4: Option<u16>,
) -> Result<Endpoint, CoreError> {
    let settings = storage.settings();
    let mut builder = match config.network_mode {
        NetworkMode::PublicOrCustom => Endpoint::builder(presets::N0),
        NetworkMode::LocalTest => Endpoint::builder(presets::Minimal),
    };
    builder = builder.secret_key(storage.secret_key().clone()).alpns(vec![
        ALPN_MAIN.to_vec(),
        ALPN_PAIR.to_vec(),
        ALPN_DIRECT.to_vec(),
    ]);

    builder = relay_settings(builder, config, &settings.custom_relay_urls)?;
    if let Some(port) = bind_port_v4 {
        builder = builder
            .clear_ip_transports()
            .bind_addr(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))
            .map_err(|e| CoreError::Internal(format!("绑定指定本地端口失败：{e}")))?;
    } else if config.network_mode == NetworkMode::LocalTest {
        builder = builder
            .clear_ip_transports()
            .bind_addr(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
            .map_err(|e| CoreError::Internal(format!("绑定本地测试端口失败：{e}")))?;
    }

    builder
        .bind()
        .await
        .map_err(|e| CoreError::Internal(format!("启动网络连接层失败：{e}")))
}
fn relay_settings(
    mut builder: iroh::endpoint::Builder,
    config: &CoreConfig,
    urls: &[String],
) -> Result<iroh::endpoint::Builder, CoreError> {
    if config.network_mode == NetworkMode::PublicOrCustom && !urls.is_empty() {
        let mut relay_urls = Vec::new();
        for raw in urls {
            let parsed: RelayUrl = raw.parse().map_err(|e| {
                CoreError::InvalidArgument(format!("无效的自定义中转服务器地址“{raw}”：{e}"))
            })?;
            relay_urls.push(parsed);
        }
        builder = builder.relay_mode(RelayMode::custom(relay_urls));
    }

    Ok(builder)
}

#[cfg(all(test, windows))]
#[path = "../../tests/desktop/network.rs"]
mod desktop_network_tests;
