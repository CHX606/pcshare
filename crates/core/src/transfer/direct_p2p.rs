use super::*;

impl CoreNode {
    // ────────────────── 任务 2.4：两台对方电脑之间直传（数据不经过控制端）──────────────────

    #[allow(clippy::too_many_arguments)]
    pub async fn transfer_between_peers_direct(
        self: &Arc<Self>,
        src_peer_id: &str,
        sources: &[String],
        dst_peer_id: &str,
        dest_dir: &str,
        conflict: ConflictPolicy,
        cancel: CancellationToken,
        on_progress: Option<ProgressCallback>,
    ) -> Result<(), CoreError> {
        // 确保与乙的连接就绪并获取乙的最新可达地址
        let dst_conn = self.get_or_connect(dst_peer_id).await?;
        let mut target_addrs: Vec<String> = dst_conn
            .paths()
            .iter()
            .filter_map(|p| match p.remote_addr() {
                TransportAddr::Ip(ip) => Some(ip.to_string()),
                _ => None,
            })
            .collect();
        if let Some(rec) = self.storage().get_peer(dst_peer_id) {
            for a in rec.direct_addrs {
                let s = a.to_string();
                if !target_addrs.contains(&s) {
                    target_addrs.push(s);
                }
            }
        }
        let target_relay = self.storage().get_peer(dst_peer_id).and_then(|r| r.relay_url);

        let token = csprng_token_hex_64();
        let transfer_id = format!("p2p-{}", &csprng_token_hex_64()[..16]);

        // 1. 先向乙签发一次性直传接收许可（5 分钟有效）
        let grant_rep: PeerGrantDirectReply = self
            .call_rpc(
                dst_peer_id,
                Command::PeerGrantDirect(PeerGrantDirectArgs {
                    token: token.clone(),
                    allowed_peer_id: src_peer_id.to_string(),
                    dest_dir: dest_dir.to_string(),
                    ttl_ms: 300_000,
                }),
            )
            .await?;
        if !grant_rep.accepted {
            return Err(CoreError::Internal("目标设备拒绝了直传接收许可".to_string()));
        }

        // 订阅来自甲的直传进度事件（等待 sub.events 真正就绪，去掉 50ms 盲等）
        let src_conn = self.get_or_connect(src_peer_id).await?;
        self.ensure_peer_event_subscription(src_peer_id, src_conn.clone())
            .await;
        let mut ev_rx = self.subscribe_events();

        // 2. 通知甲直接连接乙并推送数据
        let start_rep: PeerDirectTransferReply = self
            .call_rpc(
                src_peer_id,
                Command::PeerDirectTransfer(PeerDirectTransferArgs {
                    transfer_id: transfer_id.clone(),
                    target_peer_id: dst_peer_id.to_string(),
                    target_addrs,
                    target_relay_url: target_relay,
                    token,
                    sources: sources.to_vec(),
                    dest_dir: dest_dir.to_string(),
                    conflict,
                }),
            )
            .await?;
        if !start_rep.accepted {
            return Err(CoreError::Internal("源设备未能启动直传任务".to_string()));
        }

        let started = Instant::now();
        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    // 取消时通知源端中止直传，不再让源端继续向目标推送
                    let _ = timeout(
                        Duration::from_secs(2),
                        self.remote_task_cancel(src_peer_id, &transfer_id),
                    )
                    .await;
                    return Err(CoreError::Cancelled);
                }
                _ = src_conn.closed() => {
                    return Err(CoreError::NotConnected("源设备在直传途中断开连接，直传已中断".to_string()));
                }
                ev = ev_rx.recv() => {
                    match ev {
                        Ok(CoreEvent::RemoteDirectTransferProgress { peer_id, progress })
                            if peer_id == src_peer_id && progress.transfer_id == transfer_id =>
                        {
                            let spd = progress.bytes_done as f64 / started.elapsed().as_secs_f64().max(0.001);
                            if let Some(ref cb) = on_progress {
                                cb(progress.bytes_done, progress.bytes_total.max(1), spd);
                            }
                            match progress.state {
                                FsTaskState::Running => {}
                                FsTaskState::Completed => return Ok(()),
                                FsTaskState::Cancelled => return Err(CoreError::Cancelled),
                                FsTaskState::Failed => {
                                    return Err(match progress.error {
                                        Some(rpc) => CoreError::Rpc(rpc),
                                        None => CoreError::Internal("两台远端设备直传失败".to_string()),
                                    });
                                }
                            }
                        }
                        Ok(CoreEvent::DeviceStateChanged { peer_id, state: DeviceConnState::Offline { .. } })
                            if peer_id == src_peer_id =>
                        {
                            return Err(CoreError::NotConnected("源设备已离线，直传已中断".to_string()));
                        }
                        Ok(_) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            return Err(CoreError::Internal("事件通道已关闭".to_string()));
                        }
                    }
                }
            }
        }
    }

    pub(crate) fn handle_grant_direct(
        &self,
        perms: &Permissions,
        args: PeerGrantDirectArgs,
    ) -> Result<PeerGrantDirectReply, RpcError> {
        if !perms.fs_write {
            return Err(RpcError::new(ErrorCode::Forbidden, "没有写入文件的权限"));
        }
        let (dest_clean, dest_fs) = resolve_authorized_path(&args.dest_dir, perms)?;
        fs::create_dir_all(&dest_fs)
            .map_err(|e| map_io_to_rpc(&e, Some(&to_display_path(&dest_clean))))?;
        let ttl = Duration::from_millis(args.ttl_ms.clamp(5_000, 600_000));
        self.insert_direct_grant(
            args.token,
            DirectGrantRecord {
                allowed_peer_id: args.allowed_peer_id,
                dest_clean,
                expires_at: Instant::now() + ttl,
            },
        );
        Ok(PeerGrantDirectReply { accepted: true })
    }

    pub(crate) fn spawn_outgoing_direct_transfer(
        self: &Arc<Self>,
        controller_peer_id: &str,
        perms: &Permissions,
        args: PeerDirectTransferArgs,
    ) -> Result<PeerDirectTransferReply, RpcError> {
        if !perms.fs_read {
            return Err(RpcError::new(ErrorCode::Forbidden, "没有读取源文件的权限"));
        }
        for s in &args.sources {
            let _ = resolve_authorized_path(s, perms)?;
        }
        let cancel = CancellationToken::new();
        self.register_direct_transfer(args.transfer_id.clone(), cancel.clone());

        let node = self.clone();
        let ctrl_id = controller_peer_id.to_string();
        let perms_clone = perms.clone();
        tokio::spawn(async move {
            let res = node
                .run_outgoing_direct_worker(&ctrl_id, &perms_clone, &args, cancel)
                .await;
            node.unregister_direct_transfer(&args.transfer_id);
            match res {
                Ok(()) => {}
                Err(CoreError::Cancelled) => {
                    node.emit_remote_event(
                        &ctrl_id,
                        RemoteEvent::DirectTransferProgress(DirectTransferProgress {
                            transfer_id: args.transfer_id,
                            state: FsTaskState::Cancelled,
                            bytes_done: 0,
                            bytes_total: 1,
                            error: None,
                        }),
                    );
                }
                Err(err) => {
                    let rpc = match err {
                        CoreError::Rpc(r) => r,
                        other => RpcError::new(ErrorCode::Io, other.to_user_message()),
                    };
                    node.emit_remote_event(
                        &ctrl_id,
                        RemoteEvent::DirectTransferProgress(DirectTransferProgress {
                            transfer_id: args.transfer_id,
                            state: FsTaskState::Failed,
                            bytes_done: 0,
                            bytes_total: 1,
                            error: Some(rpc),
                        }),
                    );
                }
            }
        });
        Ok(PeerDirectTransferReply { accepted: true })
    }

    async fn run_outgoing_direct_worker(
        self: &Arc<Self>,
        controller_peer_id: &str,
        perms: &Permissions,
        args: &PeerDirectTransferArgs,
        cancel: CancellationToken,
    ) -> Result<(), CoreError> {
        if cancel.is_cancelled() {
            return Err(CoreError::Cancelled);
        }
        let walked = self
            .fs_service()
            .walk_local(perms, &args.sources)
            .await
            .map_err(CoreError::Rpc)?;
        let total_bytes: u64 = walked.iter().filter(|e| !e.dir).map(|e| e.size).sum();

        let target_id: EndpointId = args
            .target_peer_id
            .parse()
            .map_err(|_| CoreError::InvalidArgument("目标设备身份码格式无效".to_string()))?;
        let sock_addrs: Vec<SocketAddr> = args
            .target_addrs
            .iter()
            .filter_map(|s| s.parse().ok())
            .collect();
        let endpoint_addr =
            build_endpoint_addr(target_id, args.target_relay_url.as_deref(), &sock_addrs);

        let conn = timeout(
            Duration::from_secs(15),
            self.endpoint().connect(endpoint_addr, ALPN_DIRECT),
        )
        .await
        .map_err(|_| CoreError::Timeout)?
        .map_err(|e| CoreError::NotConnected(format!("无法直连目标设备：{e}")))?;

        let mut small_items = Vec::new();
        let mut large_files = Vec::new();
        for item in walked {
            if item.dir || item.size <= SMALL_FILE_BATCH_THRESHOLD {
                small_items.push(item);
            } else {
                large_files.push(item);
            }
        }

        let mut done_bytes = 0u64;
        let mut last_emit = Instant::now();

        // 1. 小文件与目录骨架走 Batch 流
        if !small_items.is_empty() {
            let (mut send, mut recv) = conn
                .open_bi()
                .await
                .map_err(|e| CoreError::NotConnected(format!("打开直传小文件流失败：{e}")))?;

            let req = DirectStreamRequest::Batch {
                token: args.token.clone(),
                conflict: args.conflict,
            };
            write_frame(&mut send, &req).await?;
            let ready: ResponseEnvelope<()> =
                timeout(Duration::from_secs(15), read_frame(&mut recv))
                    .await
                    .map_err(|_| CoreError::Timeout)??;
            ready.into_result()?;

            for item in small_items {
                if cancel.is_cancelled() {
                    let _ = send.reset(CLOSE_CANCELLED.into());
                    conn.close(CLOSE_CANCELLED.into(), b"direct_cancelled");
                    return Err(CoreError::Cancelled);
                }
                if item.dir {
                    write_frame(
                        &mut send,
                        &BatchFileItem {
                            done: false,
                            rel_path: item.rel_path,
                            dir: true,
                            size: 0,
                            modified_ms: item.modified_ms,
                            blake3_hex: None,
                        },
                    )
                    .await?;
                } else {
                    let fs_p = to_fs_path(Path::new(&item.abs_path));
                    let meta_len = tokio::fs::metadata(&fs_p).await?.len();
                    if meta_len > MAX_BATCH_FILE_BYTES {
                        let _ = send.reset(CLOSE_CANCELLED.into());
                        return Err(CoreError::Rpc(RpcError::new(
                            ErrorCode::Invalid,
                            format!(
                                "小文件大小变化超过直传批传上限（{meta_len} 字节）：{}",
                                item.abs_path
                            ),
                        )));
                    }
                    let bytes = tokio::fs::read(&fs_p).await?;
                    if bytes.len() as u64 > MAX_BATCH_FILE_BYTES {
                        let _ = send.reset(CLOSE_CANCELLED.into());
                        return Err(CoreError::Rpc(RpcError::new(
                            ErrorCode::Invalid,
                            format!("小文件大小变化超过直传批传上限：{}", item.abs_path),
                        )));
                    }
                    let hex = blake3::hash(&bytes).to_hex().to_string();
                    write_frame(
                        &mut send,
                        &BatchFileItem {
                            done: false,
                            rel_path: item.rel_path,
                            dir: false,
                            size: bytes.len() as u64,
                            modified_ms: item.modified_ms,
                            blake3_hex: Some(hex),
                        },
                    )
                    .await?;
                    if !bytes.is_empty() {
                        send.write_all(&bytes).await.map_err(|e| {
                            CoreError::NotConnected(format!("直传发送文件数据中断：{e}"))
                        })?;
                    }
                    done_bytes += bytes.len() as u64;
                    if last_emit.elapsed() >= Duration::from_millis(100) {
                        self.emit_remote_event(
                            controller_peer_id,
                            RemoteEvent::DirectTransferProgress(DirectTransferProgress {
                                transfer_id: args.transfer_id.clone(),
                                state: FsTaskState::Running,
                                bytes_done: done_bytes,
                                bytes_total: total_bytes.max(1),
                                error: None,
                            }),
                        );
                        last_emit = Instant::now();
                    }
                }
            }

            write_frame(
                &mut send,
                &BatchFileItem {
                    done: true,
                    ..Default::default()
                },
            )
            .await?;
            send.finish()
                .map_err(|e| CoreError::Internal(format!("结束直传流失败：{e}")))?;

            let done_resp: ResponseEnvelope<FsWriteBatchReply> =
                timeout(Duration::from_secs(60), read_frame(&mut recv))
                    .await
                    .map_err(|_| CoreError::Timeout)??;
            done_resp.into_result()?;
        }

        // 2. 大文件走 SingleFile 分块流式传输（64 KB 分块，绝不整文件读入内存防 OOM）
        let mut buf = vec![0u8; TRANSFER_CHUNK_SIZE];
        for item in large_files {
            if cancel.is_cancelled() {
                conn.close(CLOSE_CANCELLED.into(), b"direct_cancelled");
                return Err(CoreError::Cancelled);
            }
            let fs_p = to_fs_path(Path::new(&item.abs_path));
            let mut file = tokio::fs::File::open(&fs_p).await?;
            let file_size = file.metadata().await?.len();

            let (mut send, mut recv) = conn
                .open_bi()
                .await
                .map_err(|e| CoreError::NotConnected(format!("打开直传单文件流失败：{e}")))?;

            let req = DirectStreamRequest::SingleFile {
                token: args.token.clone(),
                rel_path: item.rel_path,
                size: file_size,
                offset: 0,
                modified_ms: item.modified_ms,
                conflict: args.conflict,
                blake3_hex: None,
            };
            write_frame(&mut send, &req).await?;
            let ready_resp: ResponseEnvelope<FsWriteReady> =
                timeout(Duration::from_secs(15), read_frame(&mut recv))
                    .await
                    .map_err(|_| CoreError::Timeout)??;
            let ready = ready_resp.into_result()?;
            if ready.skipped {
                let _ = send.finish();
                done_bytes += file_size;
                continue;
            }

            let mut sent = 0u64;
            while sent < file_size {
                if cancel.is_cancelled() {
                    let _ = send.reset(CLOSE_CANCELLED.into());
                    conn.close(CLOSE_CANCELLED.into(), b"direct_cancelled");
                    return Err(CoreError::Cancelled);
                }
                let want = ((file_size - sent) as usize).min(TRANSFER_CHUNK_SIZE);
                let n = file.read(&mut buf[..want]).await?;
                if n == 0 {
                    let _ = send.reset(CLOSE_PAUSED.into());
                    return Err(CoreError::Internal("直传读取本地文件意外提前结束".to_string()));
                }
                tokio::select! {
                    _ = cancel.cancelled() => {
                        let _ = send.reset(CLOSE_CANCELLED.into());
                        conn.close(CLOSE_CANCELLED.into(), b"direct_cancelled");
                        return Err(CoreError::Cancelled);
                    }
                    w = send.write_all(&buf[..n]) => {
                        w.map_err(|e| CoreError::NotConnected(format!("直传发送大文件数据中断：{e}")))?;
                    }
                }
                sent += n as u64;
                done_bytes += n as u64;
                if last_emit.elapsed() >= Duration::from_millis(100) {
                    self.emit_remote_event(
                        controller_peer_id,
                        RemoteEvent::DirectTransferProgress(DirectTransferProgress {
                            transfer_id: args.transfer_id.clone(),
                            state: FsTaskState::Running,
                            bytes_done: done_bytes,
                            bytes_total: total_bytes.max(1),
                            error: None,
                        }),
                    );
                    last_emit = Instant::now();
                }
            }
            send.finish()
                .map_err(|e| CoreError::Internal(format!("结束直传单文件流失败：{e}")))?;

            let done_resp: ResponseEnvelope<FsWriteDone> =
                timeout(Duration::from_secs(60), read_frame(&mut recv))
                    .await
                    .map_err(|_| CoreError::Timeout)??;
            done_resp.into_result()?;
        }

        conn.close(0u32.into(), b"direct_done");

        self.emit_remote_event(
            controller_peer_id,
            RemoteEvent::DirectTransferProgress(DirectTransferProgress {
                transfer_id: args.transfer_id.clone(),
                state: FsTaskState::Completed,
                bytes_done: total_bytes,
                bytes_total: total_bytes.max(1),
                error: None,
            }),
        );
        Ok(())
    }

    pub(crate) async fn handle_incoming_direct(self: Arc<Self>, conn: Connection) {
        let remote_id = conn.remote_id().to_string();
        // 令牌一次性消费后仅绑定当前已认证连接（供该连接上的小文件 Batch 与多个大文件 SingleFile 流共用）
        let mut session_grant: Option<(String, DirectGrantRecord)> = None;

        loop {
            let accept_timeout = if session_grant.is_none() {
                DIRECT_HANDSHAKE_TIMEOUT
            } else {
                Duration::from_secs(30)
            };
            let Ok(Ok((mut send, mut recv))) = timeout(accept_timeout, conn.accept_bi()).await
            else {
                break;
            };

            if self.is_host_paused() {
                let _ = write_frame(
                    &mut send,
                    &ResponseEnvelope::<()>::err(
                        ErrorCode::Busy,
                        "对方当前开启了“暂停被控”，暂不接收直传文件",
                    ),
                )
                .await;
                let _ = send.finish();
                let _ = timeout(Duration::from_secs(2), send.stopped()).await;
                conn.close(403u32.into(), b"host_paused");
                return;
            }

            let Ok(Ok(req)) = timeout(
                DIRECT_HANDSHAKE_TIMEOUT,
                read_frame_limited::<_, DirectStreamRequest>(&mut recv, MAX_HANDSHAKE_FRAME_SIZE),
            )
            .await
            else {
                conn.close(401u32.into(), b"direct_req_timeout");
                return;
            };

            let req_token = match &req {
                DirectStreamRequest::Batch { token, .. }
                | DirectStreamRequest::SingleFile { token, .. }
                | DirectStreamRequest::Mkdir { token, .. }
                | DirectStreamRequest::ProbeResume { token, .. } => token.as_str(),
            };

            let grant = match &session_grant {
                Some((bound_tok, rec)) => {
                    if Instant::now() < rec.expires_at
                        && constant_time_eq(bound_tok.as_bytes(), req_token.as_bytes())
                    {
                        rec.clone()
                    } else {
                        let _ = write_frame(
                            &mut send,
                            &ResponseEnvelope::<()>::err(
                                ErrorCode::Unauthorized,
                                "直传令牌无效或已过期",
                            ),
                        )
                        .await;
                        let _ = send.finish();
                        let _ = timeout(Duration::from_secs(2), send.stopped()).await;
                        conn.close(401u32.into(), b"bad_direct_token");
                        return;
                    }
                }
                None => {
                    // 首次校验成功立即从表里移除销毁（单次有效，拒绝跨连接重放）
                    let Some(rec) = self.take_direct_grant(req_token, &remote_id) else {
                        let _ = write_frame(
                            &mut send,
                            &ResponseEnvelope::<()>::err(
                                ErrorCode::Unauthorized,
                                "直传令牌无效、已被使用或已过期",
                            ),
                        )
                        .await;
                        let _ = send.finish();
                        let _ = timeout(Duration::from_secs(2), send.stopped()).await;
                        conn.close(401u32.into(), b"bad_direct_token");
                        return;
                    };
                    session_grant = Some((req_token.to_string(), rec.clone()));
                    rec
                }
            };

            let mut restricted_perms = Permissions::default();
            restricted_perms
                .allowed_roots
                .push(to_display_path(&grant.dest_clean));

            match req {
                DirectStreamRequest::Batch { conflict, .. } => {
                    let _ = self
                        .fs_service()
                        .handle_write_batch_stream(
                            &restricted_perms,
                            FsWriteBatchArgs {
                                dest_dir: to_display_path(&grant.dest_clean),
                                conflict,
                            },
                            &mut recv,
                            &mut send,
                        )
                        .await;
                    let _ = send.finish();
                }
                DirectStreamRequest::SingleFile {
                    rel_path,
                    size,
                    offset,
                    modified_ms,
                    conflict,
                    ..
                } => {
                    let rel_clean = match sanitize_rel_subpath(&rel_path) {
                        Ok(p) => p,
                        Err(err) => {
                            let _ =
                                write_frame(&mut send, &ResponseEnvelope::<FsWriteReady>::Err(err))
                                    .await;
                            let _ = send.finish();
                            continue;
                        }
                    };
                    let target_clean = grant.dest_clean.join(&rel_clean);
                    let target_disp = to_display_path(&target_clean);
                    let _ = self
                        .fs_service()
                        .handle_write_stream(
                            &restricted_perms,
                            FsWriteArgs {
                                path: target_disp,
                                size,
                                offset,
                                range_length: None,
                                defer_finalize: false,
                                modified_ms,
                                conflict,
                            },
                            &mut recv,
                            &mut send,
                        )
                        .await;
                    let _ = send.finish();
                }
                DirectStreamRequest::Mkdir { rel_path, .. } => {
                    let res = match sanitize_rel_subpath(&rel_path) {
                        Ok(rel_clean) => {
                            let target_disp = to_display_path(&grant.dest_clean.join(&rel_clean));
                            self.fs_service()
                                .mkdir(
                                    &restricted_perms,
                                    FsMkdirArgs {
                                        path: target_disp,
                                        parents: true,
                                    },
                                )
                                .await
                        }
                        Err(err) => Err(err),
                    };
                    let _ = match res {
                        Ok(rep) => write_frame(&mut send, &ResponseEnvelope::ok(rep)).await,
                        Err(err) => {
                            write_frame(&mut send, &ResponseEnvelope::<FsMkdirReply>::Err(err))
                                .await
                        }
                    };
                    let _ = send.finish();
                }
                DirectStreamRequest::ProbeResume { .. } => {
                    let _ = write_frame(
                        &mut send,
                        &ResponseEnvelope::<()>::err(
                            ErrorCode::Unsupported,
                            "不支持的直传子模式",
                        ),
                    )
                    .await;
                    let _ = send.finish();
                }
            }
        }
    }

    /// 校验直传令牌并立即移除销毁（单次有效，防止 5 分钟内跨连接重放）。
    fn take_direct_grant(
        &self,
        candidate_token: &str,
        remote_peer_id: &str,
    ) -> Option<DirectGrantRecord> {
        let mut guard = self
            .direct_grants_mutex()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        guard.retain(|_, v| Instant::now() < v.expires_at);
        let matched_key = guard.iter().find_map(|(stored_tok, rec)| {
            if rec.allowed_peer_id == remote_peer_id
                && constant_time_eq(stored_tok.as_bytes(), candidate_token.as_bytes())
            {
                Some(stored_tok.clone())
            } else {
                None
            }
        });
        matched_key.and_then(|k| guard.remove(&k))
    }
}
