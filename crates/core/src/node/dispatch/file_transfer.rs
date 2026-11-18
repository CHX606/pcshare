use super::*;
impl RpcStream<'_> {
    pub(super) async fn on_fs_read(&mut self, args: FsReadArgs) -> Result<(), CoreError> {
        let detail = args.path.clone();
        self.audit("fs.read", &detail, true);
        self.node
            .fs_service
            .handle_read_stream(&self.record.permissions, args, self.send)
            .await?;
        Ok(())
    }
    pub(super) async fn on_fs_write(&mut self, args: FsWriteArgs) -> Result<(), CoreError> {
        let detail = args.path.clone();
        let res = self
            .node
            .fs_service
            .handle_write_stream(&self.record.permissions, args, self.recv, self.send)
            .await;
        self.audit("fs.write", &detail, res.is_ok());
        res?;
        Ok(())
    }
    pub(super) async fn on_fs_hash(&mut self, args: FsHashArgs) -> Result<(), CoreError> {
        let res = self
            .node
            .fs_service
            .hash(&self.record.permissions, args)
            .await;
        write_rpc_result(self.send, res).await?;
        Ok(())
    }
    pub(super) async fn on_fs_watch(&mut self, args: FsWatchArgs) -> Result<(), CoreError> {
        let res = self
            .node
            .fs_watch
            .watch(self.peer, &self.record.permissions, &args.path);
        write_rpc_result(self.send, res).await?;
        Ok(())
    }
    pub(super) async fn on_fs_unwatch(&mut self, args: FsUnwatchArgs) -> Result<(), CoreError> {
        let res = self
            .node
            .fs_watch
            .unwatch(self.peer, &self.record.permissions, &args.path);
        write_rpc_result(self.send, res).await?;
        Ok(())
    }
    pub(super) async fn on_fs_probe_resume(
        &mut self,
        args: FsProbeResumeArgs,
    ) -> Result<(), CoreError> {
        let res = self
            .node
            .fs_service
            .probe_resume(&self.record.permissions, args)
            .await;
        write_rpc_result(self.send, res).await?;
        Ok(())
    }
    pub(super) async fn on_fs_walk(&mut self, args: FsWalkArgs) -> Result<(), CoreError> {
        self.node
            .fs_service
            .walk_to_stream(&self.record.permissions, args, self.send)
            .await?;
        Ok(())
    }
    pub(super) async fn on_fs_write_batch(
        &mut self,
        args: FsWriteBatchArgs,
    ) -> Result<(), CoreError> {
        let detail = args.dest_dir.clone();
        let res = self
            .node
            .fs_service
            .handle_write_batch_stream(&self.record.permissions, args, self.recv, self.send)
            .await;
        self.audit("fs.write_batch", &detail, res.is_ok());
        res?;
        Ok(())
    }
    pub(super) async fn on_fs_read_batch(
        &mut self,
        args: FsReadBatchArgs,
    ) -> Result<(), CoreError> {
        self.node
            .fs_service
            .handle_read_batch_stream(&self.record.permissions, args, self.send)
            .await?;
        Ok(())
    }
    pub(super) async fn on_peer_grant_direct(
        &mut self,
        args: PeerGrantDirectArgs,
    ) -> Result<(), CoreError> {
        let detail = args.dest_dir.clone();
        let res = self
            .node
            .handle_grant_direct(&self.record.permissions, args);
        self.audit("peer.grant_direct", &detail, res.is_ok());
        write_rpc_result(self.send, res).await?;
        Ok(())
    }
    pub(super) async fn on_peer_direct_transfer(
        &mut self,
        args: PeerDirectTransferArgs,
    ) -> Result<(), CoreError> {
        let detail = format!("-> {}", args.target_peer_id);
        let res =
            self.node
                .spawn_outgoing_direct_transfer(self.peer, &self.record.permissions, args);
        self.audit("peer.direct_transfer", &detail, res.is_ok());
        write_rpc_result(self.send, res).await?;
        Ok(())
    }
    pub(super) async fn on_fs_search(&mut self, args: FsSearchArgs) -> Result<(), CoreError> {
        self.audit("fs.search", &args.query, true);
        self.node
            .fs_service
            .search_to_stream(&self.record.permissions, args, self.send)
            .await?;
        Ok(())
    }
}
