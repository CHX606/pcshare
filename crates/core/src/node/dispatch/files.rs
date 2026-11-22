use super::*;
impl RpcStream<'_> {
    pub(super) async fn dispatch_files(&mut self, cmd: Command) -> Result<(), CoreError> {
        match cmd {
            Command::FsRoots => self.on_fs_roots().await,
            Command::FsList(args) => self.on_fs_list(args).await,
            Command::FsStat(args) => self.on_fs_stat(args).await,
            Command::FsMkdir(args) => self.on_fs_mkdir(args).await,
            Command::FsRename(args) => self.on_fs_rename(args).await,
            Command::FsCopy(args) => self.on_fs_copy(args).await,
            Command::FsMove(args) => self.on_fs_move(args).await,
            Command::FsDelete(args) => self.on_fs_delete(args).await,
            Command::FsTaskStatus(args) => self.on_fs_task_status(args).await,
            Command::FsTaskCancel(args) => self.on_fs_task_cancel(args).await,
            Command::FsRead(args) => self.on_fs_read(args).await,
            Command::FsWrite(args) => self.on_fs_write(args).await,
            Command::FsHash(args) => self.on_fs_hash(args).await,
            Command::FsWatch(args) => self.on_fs_watch(args).await,
            Command::FsUnwatch(args) => self.on_fs_unwatch(args).await,
            Command::FsProbeResume(args) => self.on_fs_probe_resume(args).await,
            Command::FsWalk(args) => self.on_fs_walk(args).await,
            Command::FsWriteBatch(args) => self.on_fs_write_batch(args).await,
            Command::FsReadBatch(args) => self.on_fs_read_batch(args).await,
            Command::PeerGrantDirect(args) => self.on_peer_grant_direct(args).await,
            Command::PeerDirectTransfer(args) => self.on_peer_direct_transfer(args).await,
            Command::FsSearch(args) => self.on_fs_search(args).await,
            other => self.dispatch_apps(other).await,
        }
    }
    async fn on_fs_roots(&mut self) -> Result<(), CoreError> {
        let res = self.node.fs_service.roots(&self.record.permissions);
        write_rpc_result(self.send, res).await?;
        Ok(())
    }
    async fn on_fs_list(&mut self, args: FsListArgs) -> Result<(), CoreError> {
        let detail = args.path.clone();
        let compress = self.supports_deflate();
        let res = self
            .node
            .fs_service
            .list_to_stream_opts(&self.record.permissions, args, self.send, compress)
            .await;
        self.audit("fs.list", &detail, matches!(res, Ok(true)));
        res?;
        Ok(())
    }
    async fn on_fs_stat(&mut self, args: FsStatArgs) -> Result<(), CoreError> {
        let res = self
            .node
            .fs_service
            .stat(&self.record.permissions, args)
            .await;
        write_rpc_result(self.send, res).await?;
        Ok(())
    }
    async fn on_fs_mkdir(&mut self, args: FsMkdirArgs) -> Result<(), CoreError> {
        let detail = args.path.clone();
        let res = self
            .node
            .fs_service
            .mkdir(&self.record.permissions, args)
            .await;
        self.audit("fs.mkdir", &detail, res.is_ok());
        write_rpc_result(self.send, res).await?;
        Ok(())
    }
    async fn on_fs_rename(&mut self, args: FsRenameArgs) -> Result<(), CoreError> {
        let detail = format!("{} -> {}", args.path, args.new_name);
        let res = self
            .node
            .fs_service
            .rename(&self.record.permissions, args)
            .await;
        self.audit("fs.rename", &detail, res.is_ok());
        write_rpc_result(self.send, res).await?;
        Ok(())
    }
    async fn on_fs_copy(&mut self, args: FsCopyMoveArgs) -> Result<(), CoreError> {
        let detail = args.dest_dir.clone();
        let res = self.node.fs_service.start_copy_or_move(
            self.peer,
            FsTaskKind::Copy,
            &self.record.permissions,
            args,
        );
        self.audit("fs.copy", &detail, res.is_ok());
        write_rpc_result(self.send, res).await?;
        Ok(())
    }
    async fn on_fs_move(&mut self, args: FsCopyMoveArgs) -> Result<(), CoreError> {
        let detail = args.dest_dir.clone();
        let res = self.node.fs_service.start_copy_or_move(
            self.peer,
            FsTaskKind::Move,
            &self.record.permissions,
            args,
        );
        self.audit("fs.move", &detail, res.is_ok());
        write_rpc_result(self.send, res).await?;
        Ok(())
    }
    async fn on_fs_delete(&mut self, args: FsDeleteArgs) -> Result<(), CoreError> {
        let detail = args.paths.join(", ");
        let res = self
            .node
            .fs_service
            .delete(&self.record.permissions, args)
            .await;
        self.audit("fs.delete", &detail, res.is_ok());
        write_rpc_result(self.send, res).await?;
        Ok(())
    }
    async fn on_fs_task_status(&mut self, args: FsTaskIdArgs) -> Result<(), CoreError> {
        let res = self.node.fs_service.task_status(&args.task_id);
        write_rpc_result(self.send, res).await?;
        Ok(())
    }
    async fn on_fs_task_cancel(&mut self, args: FsTaskIdArgs) -> Result<(), CoreError> {
        let res = match self.node.fs_service.task_cancel(&args.task_id) {
            Ok(prog) => Ok(prog),
            Err(err) if err.code == ErrorCode::NotFound => {
                if self.node.cancel_direct_transfer(&args.task_id) {
                    Ok(FsTaskProgress {
                        task_id: args.task_id,
                        kind: FsTaskKind::Copy,
                        state: FsTaskState::Cancelled,
                        processed_bytes: 0,
                        total_bytes: 0,
                        processed_items: 0,
                        total_items: 0,
                        current_item: None,
                        error: None,
                    })
                } else {
                    Err(err)
                }
            }
            Err(err) => Err(err),
        };
        write_rpc_result(self.send, res).await?;
        Ok(())
    }
}
