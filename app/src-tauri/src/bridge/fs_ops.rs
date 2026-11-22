use super::*;

impl BridgeState {
    // ────────────────────────── 文件缓存、监听、渐进浏览与操作 ──────────────────────────

    pub async fn fs_roots(self: &Arc<Self>, device: String) -> Reply<Vec<UiFsRoot>> {
        let reply_res: Result<FsRootsReply, CoreError> = if device == SELF_ID {
            self.node
                .fs_service()
                .roots(&Permissions::default())
                .map_err(CoreError::Rpc)
        } else {
            self.node.remote_roots(&device).await
        };

        Reply::from_core(reply_res.map(convert_roots))
    }

    pub fn fs_cache_get(&self, device: String, path: String) -> Reply<Option<UiFsCacheHit>> {
        let hit = self.node.dir_cache().get(&device, &path).map(|c| UiFsCacheHit {
            path: c.path,
            entries: c.entries.into_iter().map(convert_entry).collect(),
            cached_at: c.cached_at_ms,
        });
        Reply::ok(hit)
    }

    pub async fn fs_list(self: &Arc<Self>, device: String, path: String) -> Reply<UiFsListing> {
        if device == SELF_ID {
            match self
                .node
                .fs_service()
                .list_local(&Permissions::default(), &path)
                .await
            {
                Ok(batch) => {
                    self.node
                        .dir_cache()
                        .put_silent(SELF_ID, &batch.path, batch.entries.clone());
                    let self_name = self.node.settings().device_name;
                    self.node.record_recent_folder(SELF_ID, &self_name, &batch.path);
                    let total = batch.entries.len();
                    let ui_entries: Vec<UiFsEntry> =
                        batch.entries.into_iter().map(convert_entry).collect();
                    if total > FIRST_LIST_BATCH_SIZE {
                        self.sink.emit_fs_list_batch(UiFsListBatchEvent {
                            device: SELF_ID.to_string(),
                            path: batch.path.clone(),
                            entries: ui_entries[..FIRST_LIST_BATCH_SIZE].to_vec(),
                            total_hint: Some(total),
                            done: false,
                        });
                    }
                    Reply::ok(UiFsListing {
                        path: batch.path,
                        entries: ui_entries,
                    })
                }
                Err(e) => Reply::from_rpc(Err(e)),
            }
        } else {
            let mut canonical_path = path.clone();
            let sink = self.sink.clone();
            let dev_clone = device.clone();
            let res = self
                .node
                .remote_list_progressive(&device, &path, |batch| {
                    canonical_path = batch.path.clone();
                    if !batch.done {
                        sink.emit_fs_list_batch(UiFsListBatchEvent {
                            device: dev_clone.clone(),
                            path: batch.path,
                            entries: batch.entries.into_iter().map(convert_entry).collect(),
                            total_hint: batch.total_hint,
                            done: false,
                        });
                    }
                })
                .await;
            Reply::from_core(res.map(|entries| {
                let peer_name = self
                    .node
                    .storage()
                    .get_peer(&device)
                    .map(|p| p.name)
                    .unwrap_or_else(|| "远端电脑".to_string());
                self.node
                    .record_recent_folder(&device, &peer_name, &canonical_path);
                UiFsListing {
                    path: canonical_path,
                    entries: entries.into_iter().map(convert_entry).collect(),
                }
            }))
        }
    }

    pub async fn fs_watch(self: &Arc<Self>, device: String, path: String) -> Reply<usize> {
        let res = if device == SELF_ID {
            self.node
                .fs_watch()
                .watch(SELF_ID, &Permissions::default(), &path)
                .map(|r| r.watched_count)
                .map_err(CoreError::Rpc)
        } else {
            self.node
                .remote_watch(&device, &path)
                .await
                .map(|r| r.watched_count)
        };
        if res.is_ok() {
            self.active_watches
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert((device, path));
        }
        Reply::from_core(res)
    }

    pub async fn fs_unwatch(self: &Arc<Self>, device: String, path: String) -> Reply<usize> {
        let res = if device == SELF_ID {
            self.node
                .fs_watch()
                .unwatch(SELF_ID, &Permissions::default(), &path)
                .map(|r| r.watched_count)
                .map_err(CoreError::Rpc)
        } else {
            self.node
                .remote_unwatch(&device, &path)
                .await
                .map(|r| r.watched_count)
        };
        if res.is_ok() {
            let lower = path.to_lowercase();
            self.active_watches
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|(d, p)| !(d == &device && (p == &path || p.to_lowercase() == lower)));
        }
        Reply::from_core(res)
    }

    pub async fn fs_check_conflicts(
        self: &Arc<Self>,
        device: String,
        dest_dir: String,
        names: Vec<String>,
    ) -> Reply<Vec<String>> {
        if names.is_empty() {
            return Reply::ok(Vec::new());
        }
        let entries = match self.node.fs_list_readonly(&device, &dest_dir).await {
            Ok(entries) => entries,
            Err(e) => return Reply::from_core(Err(e)),
        };
        let existing: HashSet<String> = entries
            .into_iter()
            .map(|e| e.name.to_lowercase())
            .collect();
        let conflicts = names
            .into_iter()
            .filter(|n| existing.contains(&n.to_lowercase()))
            .collect();
        Reply::ok(conflicts)
    }

    pub async fn fs_mkdir(self: &Arc<Self>, device: String, path: String) -> Reply<()> {
        let args = FsMkdirArgs {
            path,
            parents: true,
        };
        if device == SELF_ID {
            Reply::from_rpc(
                self.node
                    .fs_service()
                    .mkdir(&Permissions::default(), args)
                    .await
                    .map(|_| ()),
            )
        } else {
            Reply::from_core(self.node.remote_mkdir(&device, args).await.map(|_| ()))
        }
    }

    pub async fn fs_rename(
        self: &Arc<Self>,
        device: String,
        path: String,
        new_name: String,
    ) -> Reply<()> {
        let args = FsRenameArgs { path, new_name };
        if device == SELF_ID {
            Reply::from_rpc(
                self.node
                    .fs_service()
                    .rename(&Permissions::default(), args)
                    .await
                    .map(|_| ()),
            )
        } else {
            Reply::from_core(self.node.remote_rename(&device, args).await.map(|_| ()))
        }
    }

    pub async fn fs_copy(
        self: &Arc<Self>,
        device: String,
        sources: Vec<String>,
        dest_dir: String,
    ) -> Reply<String> {
        self.start_device_copy_or_move(device, sources, dest_dir, false)
            .await
    }

    pub async fn fs_move(
        self: &Arc<Self>,
        device: String,
        sources: Vec<String>,
        dest_dir: String,
    ) -> Reply<String> {
        self.start_device_copy_or_move(device, sources, dest_dir, true)
            .await
    }

    async fn start_device_copy_or_move(
        self: &Arc<Self>,
        device: String,
        sources: Vec<String>,
        dest_dir: String,
        is_move: bool,
    ) -> Reply<String> {
        if sources.is_empty() {
            return Reply::err(UiErrorCode::Invalid, "请先选择要操作的文件或文件夹");
        }
        let kind = if is_move {
            UiTaskKind::Move
        } else {
            UiTaskKind::Copy
        };
        let title = build_copy_move_title(&sources, &dest_dir, is_move);
        let args = FsCopyMoveArgs {
            sources,
            dest_dir,
            conflict: ConflictPolicy::RenameKeepBoth,
        };

        if device == SELF_ID {
            let fs_svc = self.node.fs_service().clone();
            let fs_kind = if is_move {
                FsTaskKind::Move
            } else {
                FsTaskKind::Copy
            };
            let task_reply =
                match fs_svc.start_copy_or_move(SELF_ID, fs_kind, &Permissions::default(), args) {
                    Ok(r) => r,
                    Err(e) => return Reply::from_rpc(Err(e)),
                };
            let (seq, ui_task_id) = self.next_task_id();
            let meta = FsTaskMeta {
                seq,
                kind,
                title: title.clone(),
                device_id: SELF_ID.to_string(),
                bytes_done: Arc::new(AtomicU64::new(0)),
                bytes_total: Arc::new(AtomicU64::new(1)),
                speed_bits: Arc::new(AtomicU64::new(0)),
            };
            self.active_tasks
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(
                    ui_task_id.clone(),
                    CancellableTask::LocalFsTask {
                        raw_task_id: task_reply.task_id.clone(),
                        meta: meta.clone(),
                    },
                );

            let bridge = self.clone();
            let tid = ui_task_id.clone();
            let raw_tid = task_reply.task_id;
            tokio::spawn(async move {
                bridge.poll_local_fs_task(tid, raw_tid, meta).await;
            });
            Reply::ok(ui_task_id)
        } else {
            let start_res = if is_move {
                self.node.remote_move(&device, args).await
            } else {
                self.node.remote_copy(&device, args).await
            };
            let task_reply = match start_res {
                Ok(r) => r,
                Err(e) => return Reply::from_core(Err(e)),
            };
            let (seq, ui_task_id) = self.next_task_id();
            let meta = FsTaskMeta {
                seq,
                kind,
                title: title.clone(),
                device_id: device.clone(),
                bytes_done: Arc::new(AtomicU64::new(0)),
                bytes_total: Arc::new(AtomicU64::new(1)),
                speed_bits: Arc::new(AtomicU64::new(0)),
            };
            self.active_tasks
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(
                    ui_task_id.clone(),
                    CancellableTask::RemoteFsTask {
                        peer_id: device.clone(),
                        remote_task_id: task_reply.task_id.clone(),
                        meta: meta.clone(),
                    },
                );

            let bridge = self.clone();
            let tid = ui_task_id.clone();
            let raw_tid = task_reply.task_id;
            tokio::spawn(async move {
                bridge
                    .poll_remote_fs_task(tid, device, raw_tid, meta)
                    .await;
            });
            Reply::ok(ui_task_id)
        }
    }

    pub async fn fs_delete(
        self: &Arc<Self>,
        device: String,
        paths: Vec<String>,
        permanent: bool,
    ) -> Reply<()> {
        let args = FsDeleteArgs { paths, permanent };
        if device == SELF_ID {
            Reply::from_rpc(
                self.node
                    .fs_service()
                    .delete(&Permissions::default(), args)
                    .await
                    .map(|_| ()),
            )
        } else {
            Reply::from_core(self.node.remote_delete(&device, args).await.map(|_| ()))
        }
    }
}
