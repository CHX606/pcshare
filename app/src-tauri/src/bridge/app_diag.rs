use super::*;

impl BridgeState {
    // ────────────────────────── 软件、图标与进程 ──────────────────────────

    pub async fn app_list(
        self: &Arc<Self>,
        device: String,
        force: bool,
    ) -> Reply<Vec<UiAppInfo>> {
        let res = if device == SELF_ID {
            self.node.local_apps(force).await
        } else {
            self.node.remote_apps(&device, force).await
        };
        Reply::from_core(res.map(|r| {
            r.apps
                .into_iter()
                .map(|a| UiAppInfo {
                    id: a.app_id,
                    name: a.name,
                })
                .collect()
        }))
    }

    pub async fn app_icon(
        self: &Arc<Self>,
        device: String,
        app_id: String,
    ) -> Reply<Option<String>> {
        let cache_key = format!("{device}|{app_id}");
        if let Some(hit) = self
            .icon_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&cache_key)
        {
            return Reply::ok(Some(hit));
        }

        let res = if device == SELF_ID {
            self.node.local_app_icon(&app_id).await
        } else {
            self.node.remote_app_icon(&device, &app_id).await
        };
        match res {
            Ok(rep) => {
                let data_url = rep.png_base64.filter(|s| !s.is_empty()).map(|b64| {
                    const PREFIX: &str = "data:image/png;base64,";
                    let mut out = String::with_capacity(PREFIX.len() + b64.len());
                    out.push_str(PREFIX);
                    out.push_str(&b64);
                    out
                });
                // 仅缓存成功提取到的图标，不缓存瞬态 None
                if let Some(ref url) = data_url {
                    self.icon_cache
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(cache_key, url.clone());
                }
                Reply::ok(data_url)
            }
            Err(e) => Reply::from_core(Err(e)),
        }
    }

    pub async fn app_launch(
        self: &Arc<Self>,
        device: String,
        target: String,
        args: Option<String>,
    ) -> Reply<UiLaunchResult> {
        let kind = classify_launch_target(&target, device == SELF_ID).await;
        let req = AppLaunchArgs {
            target,
            kind,
            args,
            skip_window_wait: false,
        };
        let res = if device == SELF_ID {
            self.node.local_launch(req).await
        } else {
            self.node.remote_launch(&device, req).await
        };
        Reply::from_core(res.map(convert_launch_reply))
    }

    pub async fn proc_list(self: &Arc<Self>, device: String) -> Reply<Vec<UiProcInfo>> {
        let res = if device == SELF_ID {
            self.node.local_processes().await
        } else {
            self.node.remote_processes(&device).await
        };
        Reply::from_core(res.map(|r| {
            r.processes
                .into_iter()
                .map(|p| UiProcInfo {
                    pid: p.pid,
                    name: p.name,
                    title: p.window_title.unwrap_or_default(),
                    memory: p.memory_bytes,
                    cpu: p.cpu_percent,
                    critical: p.is_critical,
                })
                .collect()
        }))
    }

    pub async fn proc_kill(self: &Arc<Self>, device: String, pid: u32) -> Reply<()> {
        let req = ProcKillArgs {
            pid,
            confirmed: true,
        };
        let res = if device == SELF_ID {
            self.node.local_kill_process(req).await
        } else {
            self.node.remote_kill_process(&device, req).await
        };
        Reply::from_core(res.map(|_| ()))
    }

    // ────────────────────────── 诊断页与网络体检（任务 2.11） ──────────────────────────

    pub fn diag_summary(&self) -> Reply<UiDiagSummary> {
        let settings = self.node.settings();
        let states = self.node.all_peer_states();
        let peer_routes = self
            .node
            .list_peers()
            .into_iter()
            .map(|p| {
                let (state_key, detail, rtt_ms) = match states.get(&p.endpoint_id) {
                    Some(DeviceConnState::OnlineDirect { addr, rtt_ms }) => (
                        "online_direct".to_string(),
                        format!("直连 ({addr}) · {} ms", (*rtt_ms).max(1)),
                        Some((*rtt_ms).max(1)),
                    ),
                    Some(DeviceConnState::OnlineRelay { relay_url, rtt_ms }) => (
                        "online_relay".to_string(),
                        format!("中转 ({relay_url}) · {} ms", (*rtt_ms).max(1)),
                        Some((*rtt_ms).max(1)),
                    ),
                    Some(DeviceConnState::Connecting) => {
                        ("connecting".to_string(), "正在重连…".to_string(), None)
                    }
                    Some(DeviceConnState::Offline { .. }) | None => {
                        ("offline".to_string(), "未连接".to_string(), None)
                    }
                };
                UiDiagPeerRoute {
                    peer_id: p.endpoint_id,
                    name: p.name,
                    state: state_key,
                    detail,
                    rtt_ms,
                }
            })
            .collect();

        let netcheck_targets = if settings.custom_relay_urls.is_empty() {
            pcshare_core::diagnostics::default_netcheck_relay_urls()
        } else {
            settings.custom_relay_urls
        };

        Reply::ok(UiDiagSummary {
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            endpoint_id: self.node.endpoint_id().to_string(),
            device_name: settings.device_name,
            os: std::env::consts::OS.to_string(),
            working_set_bytes: self.node.platform().current_process_working_set(),
            host_paused: self.node.is_host_paused(),
            speed_limit_bps: self.node.rate_limiter().limit_bps(),
            netcheck_targets,
            peer_routes,
        })
    }

    pub async fn diag_netcheck(self: &Arc<Self>) -> Reply<UiNetcheckReport> {
        Reply::from_core(self.node.run_netcheck().await.map(|rep| {
            let preferred_relay = rep
                .relays
                .iter()
                .find(|r| r.preferred)
                .map(|r| r.relay_host.clone());
            let relay_latencies = rep
                .relays
                .into_iter()
                .map(|r| UiRelayLatencyItem {
                    url: r.relay_host,
                    rtt_ms: r.ipv4_ms.or(r.ipv6_ms).or(r.https_ms),
                })
                .collect();
            UiNetcheckReport {
                checked_at_ms: rep.checked_at_ms,
                udp_ipv4: rep.udp_v4,
                udp_ipv6: rep.udp_v6,
                mapping_varies_by_dest_ip: rep.mapping_varies_by_dest_ip,
                upnp_available: None,
                pcp_available: None,
                nat_pmp_available: None,
                preferred_relay,
                relay_latencies,
                summary: rep.summary,
            }
        }))
    }

    pub fn diag_logs(&self, limit: usize) -> Reply<UiDiagLogs> {
        let cap = limit.clamp(10, 1000);
        let runtime_logs = self.node.recent_logs(cap);
        let audits = self
            .node
            .storage()
            .read_recent_audits(cap)
            .unwrap_or_default()
            .into_iter()
            .map(|a| {
                let ts = Local
                    .timestamp_millis_opt(a.timestamp_ms as i64)
                    .single()
                    .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
                    .unwrap_or_else(|| a.timestamp_ms.to_string());
                UiAuditEntry {
                    ts,
                    peer_id: a.peer_id,
                    peer_name: a.peer_name,
                    cmd: a.action,
                    detail: if a.detail.is_empty() {
                        None
                    } else {
                        Some(a.detail)
                    },
                    ok: a.ok,
                }
            })
            .collect();
        Reply::ok(UiDiagLogs {
            runtime_logs,
            audits,
        })
    }

    pub async fn diag_export(self: &Arc<Self>, dest_dir: String) -> Reply<String> {
        Reply::from_core(
            self.node
                .export_diagnostics(Path::new(&dest_dir))
                .await
                .map(|p| to_display_path(&p)),
        )
    }
}

// ────────────────────────────── 映射与工具函数 ──────────────────────────────

/// 仅对连接、超时、协议帧或不支持直传等网络/能力错误回退本机中转；
/// 权限不足、文件不存在、参数非法、已取消、被控暂停等确定性错误直接报错，避免无效下载。
pub fn should_fallback_direct_to_relay(err: &CoreError) -> bool {
    match err {
        CoreError::NotConnected(_) | CoreError::Timeout | CoreError::Frame(_) => true,
        CoreError::Rpc(rpc) => matches!(
            rpc.code,
            ErrorCode::Unsupported | ErrorCode::Io | ErrorCode::Internal
        ),
        CoreError::Internal(_) => true,
        CoreError::Cancelled
        | CoreError::InvalidArgument(_)
        | CoreError::Platform(_)
        | CoreError::Io(_) => false,
    }
}

pub(super) fn emit_throttled_running(
    sink: &Arc<dyn EventSink>,
    last_emit: &Mutex<Instant>,
    progress: UiTaskProgress,
) {
    let mut guard = last_emit.lock().unwrap_or_else(|e| e.into_inner());
    if guard.elapsed() >= PROGRESS_THROTTLE {
        *guard = Instant::now();
        drop(guard);
        sink.emit_task_progress(progress);
    }
}

pub(super) fn emit_fs_task_progress(
    sink: &Arc<dyn EventSink>,
    last_emit: &Mutex<Instant>,
    ui_task_id: &str,
    meta: &FsTaskMeta,
    p: &FsTaskProgress,
    started: Instant,
) -> bool {
    let speed = p.processed_bytes as f64 / started.elapsed().as_secs_f64().max(0.001);
    let total = p.total_bytes.max(p.processed_bytes).max(1);
    let (state, terminal) = match p.state {
        FsTaskState::Running => (UiTaskState::Running, false),
        FsTaskState::Completed => (UiTaskState::Done, true),
        FsTaskState::Failed => (UiTaskState::Failed, true),
        FsTaskState::Cancelled => (UiTaskState::Cancelled, true),
    };
    let bytes_done = if state == UiTaskState::Done {
        total
    } else {
        p.processed_bytes.min(total)
    };
    meta.bytes_done.store(bytes_done, Ordering::Relaxed);
    meta.bytes_total.store(total, Ordering::Relaxed);
    meta.speed_bits.store(speed.to_bits(), Ordering::Relaxed);

    let error = p.error.as_ref().map(map_rpc_error);
    let payload = UiTaskProgress {
        id: ui_task_id.to_string(),
        kind: meta.kind,
        title: meta.title.clone(),
        src_device: meta.device_id.clone(),
        dst_device: meta.device_id.clone(),
        bytes_done,
        bytes_total: total,
        speed,
        state,
        error,
    };

    if terminal {
        sink.emit_task_progress(payload);
        true
    } else {
        emit_throttled_running(sink, last_emit, payload);
        false
    }
}

pub(super) fn to_ui_device(peer: &PeerRecord, state: Option<&DeviceConnState>) -> UiDevice {
    let os = match peer
        .os
        .as_deref()
        .unwrap_or("unknown")
        .to_ascii_lowercase()
        .as_str()
    {
        "windows" => "windows",
        "macos" | "darwin" => "macos",
        "linux" => "linux",
        _ => "unknown",
    }
    .to_string();

    let status = match state {
        Some(DeviceConnState::Connecting) => UiDeviceStatus::Connecting,
        Some(DeviceConnState::OnlineDirect { rtt_ms, .. }) => UiDeviceStatus::Online {
            link: UiLinkKind::Direct,
            rtt_ms: (*rtt_ms).max(1),
        },
        Some(DeviceConnState::OnlineRelay { rtt_ms, .. }) => UiDeviceStatus::Online {
            link: UiLinkKind::Relay,
            rtt_ms: (*rtt_ms).max(1),
        },
        Some(DeviceConnState::Offline { .. }) | None => UiDeviceStatus::Offline {
            last_seen: peer.last_seen_ms,
        },
    };

    let mut features = vec![
        "fs".to_string(),
        "transfer".to_string(),
        "apps".to_string(),
        "procs".to_string(),
    ];
    let caps = &peer.capabilities;
    let has_cap = |tag: &str| caps.is_empty() || caps.iter().any(|c| c == tag);
    if has_cap("shot.v1") {
        features.push("shot".to_string());
    }
    if has_cap("input.v1") {
        features.push("input".to_string());
    }
    if has_cap("power.v1") {
        features.push("power".to_string());
    }
    if has_cap("clip.v1") {
        features.push("clip".to_string());
    }
    if has_cap("term.v1") {
        features.push("term".to_string());
    }
    if has_cap("search.v1") {
        features.push("search".to_string());
    }

    UiDevice {
        id: peer.endpoint_id.clone(),
        name: peer.name.clone(),
        os,
        status,
        features,
        has_mac: !peer.mac_addresses.is_empty(),
        permissions: Some(UiPermissions::from_proto(&peer.permissions)),
    }
}

pub(super) fn convert_roots(r: FsRootsReply) -> Vec<UiFsRoot> {
    let mut out = Vec::with_capacity(r.drives.len() + r.special_folders.len());
    for d in r.drives {
        let name = if d.name == d.path {
            format!("本地磁盘 ({})", d.path.trim_end_matches('\\'))
        } else {
            d.name
        };
        out.push(UiFsRoot {
            name,
            path: d.path,
            kind: "drive".to_string(),
            total: d.total_bytes,
            free: d.available_bytes,
        });
    }
    for f in r.special_folders {
        let kind = match f.kind {
            RootKind::Desktop => "desktop",
            RootKind::Downloads => "downloads",
            RootKind::Documents => "documents",
            RootKind::Home | RootKind::AllowedFolder => "home",
            RootKind::Drive => "drive",
        }
        .to_string();
        out.push(UiFsRoot {
            name: f.name,
            path: f.path,
            kind,
            total: f.total_bytes,
            free: f.available_bytes,
        });
    }
    out
}

pub(super) fn convert_entry(e: DirEntry) -> UiFsEntry {
    UiFsEntry {
        name: e.name,
        dir: e.kind == EntryKind::Dir,
        size: e.size,
        mtime: e.modified_ms.unwrap_or(0),
    }
}

pub async fn classify_launch_target(target: &str, is_local: bool) -> LaunchKind {
    let t = target.trim();
    let bytes = t.as_bytes();
    let is_drive_path = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/');
    if is_drive_path
        || t.starts_with(r"\\")
        || t.starts_with("shell:")
        || t.starts_with("http://")
        || t.starts_with("https://")
    {
        LaunchKind::PathOrTarget
    } else if is_local
        && let Ok(clean) = normalize_lexical_absolute(t)
        && tokio::fs::try_exists(to_fs_path(&clean))
            .await
            .unwrap_or(false)
    {
        LaunchKind::PathOrTarget
    } else {
        LaunchKind::AppId
    }
}

pub(crate) fn convert_desktop_status(d: &pcshare_proto::DesktopStatus) -> UiDesktopState {
    let place = if d.session_id == 0 {
        "service"
    } else if d.input_desktop.is_none() {
        "locked"
    } else if d.session_id != d.console_session_id {
        "remote"
    } else {
        "normal"
    };
    UiDesktopState {
        place: place.to_string(),
        elevated: d.elevated,
    }
}

pub(crate) fn convert_launch_reply(r: AppLaunchReply) -> UiLaunchResult {
    UiLaunchResult {
        new_windows: r.new_windows,
        desktop: convert_desktop_status(&r.desktop),
        elevation_prompt: if r.uac_prompt_suspected {
            Some(true)
        } else {
            None
        },
    }
}

pub(super) fn build_copy_move_title(sources: &[String], dest_dir: &str, is_move: bool) -> String {
    let verb = if is_move { "移动" } else { "复制" };
    let first_name = PathBuf::from(&sources[0])
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| sources[0].clone());
    let dest_name = PathBuf::from(dest_dir)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| dest_dir.to_string());
    if sources.len() == 1 {
        format!("{verb} {first_name} → {dest_name}")
    } else {
        format!("{verb} {} 项 → {dest_name}", sources.len())
    }
}

pub(super) fn map_rpc_error(err: &RpcError) -> ApiErrorBody {
    let code = match err.code {
        ErrorCode::Unauthorized => {
            if err.msg.contains("配对码不正确") {
                UiErrorCode::BadCode
            } else if err.msg.contains("有效期") || err.msg.contains("作废") {
                UiErrorCode::Expired
            } else {
                UiErrorCode::Unauthorized
            }
        }
        ErrorCode::Forbidden => UiErrorCode::Forbidden,
        ErrorCode::NotFound => UiErrorCode::NotFound,
        ErrorCode::AlreadyExists => UiErrorCode::AlreadyExists,
        ErrorCode::Busy => UiErrorCode::Busy,
        ErrorCode::Invalid => UiErrorCode::Invalid,
        ErrorCode::Unsupported => UiErrorCode::Unsupported,
        ErrorCode::Io => UiErrorCode::Io,
        ErrorCode::Cancelled => UiErrorCode::Cancelled,
        ErrorCode::Internal => UiErrorCode::Internal,
    };
    ApiErrorBody {
        code,
        msg: err.msg.clone(),
    }
}

pub(super) fn map_core_error(err: &CoreError) -> ApiErrorBody {
    match err {
        CoreError::Rpc(rpc) => map_rpc_error(rpc),
        CoreError::Platform(pe) => map_rpc_error(&map_platform_to_rpc(pe)),
        CoreError::NotConnected(msg) => ApiErrorBody {
            code: UiErrorCode::Offline,
            msg: msg.clone(),
        },
        CoreError::Timeout => ApiErrorBody {
            code: UiErrorCode::Timeout,
            msg: "对方迟迟没有回应，请检查网络是否通畅后再试".to_string(),
        },
        CoreError::Cancelled => ApiErrorBody {
            code: UiErrorCode::Cancelled,
            msg: "已取消".to_string(),
        },
        CoreError::InvalidArgument(msg) => ApiErrorBody {
            code: UiErrorCode::Invalid,
            msg: msg.clone(),
        },
        CoreError::Io(io_err) => map_rpc_error(&map_io_to_rpc(io_err, None)),
        CoreError::Frame(fe) => ApiErrorBody {
            code: UiErrorCode::Internal,
            msg: format!("协议传输错误：{fe}"),
        },
        CoreError::Internal(msg) => ApiErrorBody {
            code: UiErrorCode::Internal,
            msg: msg.clone(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_cache_enforces_limit_and_clears_removed_device() {
        let mut cache = IconCache::new(3);
        cache.insert("devA|app1".to_string(), "data:1".to_string());
        cache.insert("devA|app2".to_string(), "data:2".to_string());
        cache.insert("devB|app1".to_string(), "data:3".to_string());
        // 命中 devA|app1 使其成为最近使用，下一次插入应淘汰 devA|app2
        assert_eq!(cache.get("devA|app1").as_deref(), Some("data:1"));
        cache.insert("devB|app2".to_string(), "data:4".to_string());
        assert_eq!(cache.len(), 3);
        assert!(cache.get("devA|app2").is_none());
        assert_eq!(cache.get("devA|app1").as_deref(), Some("data:1"));

        cache.remove_device("devB");
        assert_eq!(cache.len(), 1);
        assert!(cache.get("devB|app1").is_none());
        assert!(cache.get("devB|app2").is_none());
        assert_eq!(cache.get("devA|app1").as_deref(), Some("data:1"));
    }

    #[test]
    fn direct_transfer_fallback_distinguishes_deterministic_vs_network_errors() {
        assert!(!should_fallback_direct_to_relay(&CoreError::Cancelled));
        assert!(!should_fallback_direct_to_relay(&CoreError::InvalidArgument(
            "bad".into()
        )));
        for code in [
            ErrorCode::Forbidden,
            ErrorCode::Unauthorized,
            ErrorCode::NotFound,
            ErrorCode::AlreadyExists,
            ErrorCode::Invalid,
            ErrorCode::Cancelled,
            ErrorCode::Busy,
        ] {
            let err = CoreError::Rpc(RpcError::new(code, "deterministic"));
            assert!(
                !should_fallback_direct_to_relay(&err),
                "code {code:?} must not fallback to relay"
            );
        }

        assert!(should_fallback_direct_to_relay(&CoreError::NotConnected(
            "offline".into()
        )));
        assert!(should_fallback_direct_to_relay(&CoreError::Timeout));
        assert!(should_fallback_direct_to_relay(&CoreError::Rpc(
            RpcError::new(ErrorCode::Unsupported, "old peer")
        )));
    }

    #[tokio::test]
    async fn classify_launch_target_only_checks_local_filesystem_for_self() {
        assert_eq!(
            classify_launch_target(r"C:\Windows\notepad.exe", false).await,
            LaunchKind::PathOrTarget
        );
        assert_eq!(
            classify_launch_target("Microsoft.WindowsCalculator_8wekyb3d8bbwe!App", false).await,
            LaunchKind::AppId
        );
        // 远端设备对非盘符/非协议格式绝不查本机文件系统
        let cwd = std::env::current_dir().unwrap();
        let rel_marker = cwd.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(
            classify_launch_target(&rel_marker, false).await,
            LaunchKind::AppId
        );
    }
}
