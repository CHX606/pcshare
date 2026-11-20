use super::*;

impl CoreNode {
    /// 上传本机单个文件到对方电脑（统一委托至 `transfer.rs` 可续传限速实现）。
    pub async fn upload_file(
        self: &Arc<Self>,
        peer_id: &str,
        local_path: &Path,
        remote_path: &str,
        conflict: ConflictPolicy,
        cancel: CancellationToken,
        on_progress: Option<ProgressCallback>,
    ) -> Result<FsWriteDone, CoreError> {
        self.upload_file_resumable(
            peer_id,
            local_path,
            remote_path,
            conflict,
            cancel,
            on_progress,
            None,
        )
        .await
    }
}
