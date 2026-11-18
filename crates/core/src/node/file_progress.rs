use super::*;

pub(super) struct FileProgress<'a> {
    node: &'a CoreNode,
    on_progress: Option<ProgressCallback>,
    event: TransferProgressEvent,
    started: Instant,
    last: Instant,
}
impl<'a> FileProgress<'a> {
    pub fn new(
        node: &'a CoreNode,
        peer: &str,
        direction: TransferDirection,
        paths: (String, String),
        total: u64,
        callback: Option<ProgressCallback>,
    ) -> Self {
        Self {
            node,
            on_progress: callback,
            started: Instant::now(),
            last: Instant::now(),
            event: TransferProgressEvent {
                peer_id: peer.into(),
                direction,
                local_path: paths.0,
                remote_path: paths.1,
                transferred_bytes: 0,
                total_bytes: total,
                bytes_per_sec: 0.0,
            },
        }
    }
    pub fn report(&mut self, done: u64) {
        let total = self.event.total_bytes;
        if done != total && self.last.elapsed() < Duration::from_millis(100) {
            return;
        }
        self.event.transferred_bytes = done;
        self.event.bytes_per_sec = done as f64 / self.started.elapsed().as_secs_f64().max(0.001);
        if let Some(cb) = &self.on_progress {
            cb(done, total, self.event.bytes_per_sec);
        }
        let _ = self
            .node
            .events_tx
            .send(CoreEvent::TransferProgress(self.event.clone()));
        self.last = Instant::now();
    }
}
