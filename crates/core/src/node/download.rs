use super::*;

impl CoreNode {
    /// 从对方电脑下载单个文件到本机（统一委托至 `transfer.rs` 可续传限速实现）。
    pub async fn download_file(
        self: &Arc<Self>,
        peer_id: &str,
        remote_path: &str,
        local_dest: &Path,
        conflict: ConflictPolicy,
        cancel: CancellationToken,
        on_progress: Option<ProgressCallback>,
    ) -> Result<FsWriteDone, CoreError> {
        self.download_file_resumable(
            peer_id,
            remote_path,
            local_dest,
            conflict,
            cancel,
            on_progress,
            None,
        )
        .await
    }
}
