use iroh::endpoint::Connection;
use pcshare_proto::desktop::Delivery;
use std::time::{Duration, Instant};

/// Sample the sender's selected path. Receiver-side loss counters describe the wrong direction.
pub(super) struct Feedback {
    conn: Connection,
    since: Instant,
    previous: Option<(u64, u64)>,
    value: Delivery,
}
impl Feedback {
    pub fn new(conn: &Connection) -> Self {
        Self {
            conn: conn.clone(),
            since: Instant::now(),
            previous: None,
            value: Delivery::default(),
        }
    }
    pub fn sample(&mut self) -> Delivery {
        if self.since.elapsed() < Duration::from_millis(400) {
            return self.value;
        }
        self.since = Instant::now();
        let paths = self.conn.paths();
        let Some(path) = paths.iter().find(|p| p.is_selected()) else {
            return Delivery::default();
        };
        let stats = path.stats();
        let counts = (stats.sent_packets, stats.lost_packets);
        let (sent, lost) = self.previous.replace(counts).unwrap_or(counts);
        let packets = counts.0.saturating_sub(sent);
        let loss = counts.1.saturating_sub(lost);
        self.value = Delivery {
            capacity_bps: stats.bandwidth_estimate,
            loss_per_mille: if packets >= 20 {
                (loss.saturating_mul(1000) / packets).min(1000) as u16
            } else {
                0
            },
        };
        self.value
    }
}
