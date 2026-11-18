use pcshare_proto::desktop::VideoTier::{self, Balanced, Economy, High};
use std::time::Duration;
#[cfg(test)]
mod tests;

pub(super) struct Policy {
    pub tier: Option<VideoTier>,
    bad: Duration,
    good: Duration,
    pub probe_delay: Duration,
    good_probes: u8,
}
const SECOND: Duration = Duration::from_secs(1);

impl Policy {
    pub fn new(rtt: Duration) -> Self {
        let tier = match rtt.as_millis() {
            0..=70 => Some(High),
            71..=150 => Some(Balanced),
            151..=230 => Some(Economy),
            _ => None,
        };
        Self {
            tier,
            bad: Duration::ZERO,
            good: Duration::ZERO,
            probe_delay: 2 * SECOND,
            good_probes: 0,
        }
    }

    pub fn observe(
        &mut self,
        rtt: Duration,
        lag: Duration,
        elapsed: Duration,
        link: pcshare_proto::desktop::Delivery,
    ) -> bool {
        let target = self.tier.map_or(0, |t| u64::from(t.settings().2));
        let limited =
            link.capacity_bps.is_some_and(|v| v < target) && lag > Duration::from_millis(45);
        let slow = limited
            || link.loss_per_mille >= 35
            || rtt > Duration::from_millis(215)
            || lag > Duration::from_millis(130);
        let fast = !limited
            && link.loss_per_mille < 10
            && rtt < Duration::from_millis(75)
            && lag < Duration::from_millis(40);
        let dt = elapsed.min(Duration::from_millis(150));
        self.bad = if slow { self.bad + dt } else { Duration::ZERO };
        self.good = if fast { self.good + dt } else { Duration::ZERO };
        if self.bad >= Duration::from_millis(460) {
            if rtt >= Duration::from_millis(250) && lag >= Duration::from_millis(130) {
                self.tier = Some(Economy);
            }
            self.lower();
            return true;
        }
        if self.good < 6 * SECOND {
            return false;
        }
        self.good = Duration::ZERO;
        self.probe_delay = 2 * SECOND;
        match self.tier {
            Some(Economy) => self.tier = Some(Balanced),
            Some(Balanced) => self.tier = Some(High),
            _ => return false,
        }
        true
    }

    pub fn lower(&mut self) {
        self.tier = match self.tier {
            Some(High) => Some(Balanced),
            Some(Balanced) => Some(Economy),
            _ => None,
        };
        self.bad = Duration::ZERO;
        self.good = Duration::ZERO;
        self.good_probes = 0;
        if self.tier.is_none() {
            self.probe_delay = (self.probe_delay * 2).min(60 * SECOND);
        }
    }

    pub fn probe(&mut self, rtt: Duration) -> bool {
        if rtt > Duration::from_millis(120) {
            self.good_probes = 0;
            return false;
        }
        self.good_probes += 1;
        if self.good_probes < 2 {
            return false;
        }
        self.tier = Some(Economy);
        self.good_probes = 0;
        true
    }
}
