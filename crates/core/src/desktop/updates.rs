use crate::CapturedShot;
use std::sync::Mutex;
use tokio::sync::watch;

/// Retain only the newest completed action image while the viewer is busy rendering.
pub(super) struct Updates {
    frame: Mutex<Option<CapturedShot>>,
    revision: watch::Sender<u64>,
}
impl Updates {
    pub fn new() -> (Self, watch::Receiver<u64>) {
        let (revision, reader) = watch::channel(0);
        (
            Self {
                frame: Mutex::new(None),
                revision,
            },
            reader,
        )
    }
    pub fn publish(&self, shot: Option<CapturedShot>) {
        let mut slot = self.frame.lock().unwrap_or_else(|e| e.into_inner());
        *slot = shot;
        self.revision.send_modify(|n| *n = n.wrapping_add(1));
    }
    pub fn take(&self, reader: &mut watch::Receiver<u64>) -> Option<CapturedShot> {
        let mut slot = self.frame.lock().unwrap_or_else(|e| e.into_inner());
        reader.borrow_and_update();
        slot.take()
    }
}
