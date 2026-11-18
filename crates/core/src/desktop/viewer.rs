use super::updates::Updates;
use super::{policy::Policy, wire::VideoStream};
use crate::{CoreError, CoreNode};
use pcshare_proto::desktop::{Source, VideoOpen};
use pcshare_proto::{ErrorCode, ShotTransform};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::watch;

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopFrameInfo {
    pub mode: &'static str,
    pub format: &'static str,
    pub timestamp_us: u64,
    pub key: bool,
    pub epoch: u32,
    pub transform: ShotTransform,
    pub encoder: String,
    pub reason: Option<String>,
}
pub struct DesktopFrame {
    pub info: DesktopFrameInfo,
    pub bytes: Vec<u8>,
}
pub(super) struct Viewer {
    changes: watch::Receiver<u64>,
    policy: Policy,
    stream: Option<VideoStream>,
    source: Source,
    live_capable: bool,
    snapshot_due: bool,
    probe_at: Instant,
    epoch: u32,
    reason: Option<String>,
}

impl Viewer {
    pub fn new(
        source: Source,
        capable: bool,
        rtt: Duration,
        changes: watch::Receiver<u64>,
    ) -> Self {
        let mut policy = Policy::new(rtt);
        let reason = if !capable {
            policy.tier = None;
            Some("对方使用旧版桌面协议".into())
        } else if policy.tier.is_none() {
            Some(format!("当前链路往返 {} 毫秒", rtt.as_millis()))
        } else {
            None
        };
        Self {
            changes,
            policy,
            stream: None,
            source,
            live_capable: capable,
            snapshot_due: true,
            probe_at: Instant::now() + Duration::from_secs(2),
            epoch: 0,
            reason,
        }
    }

    pub fn close(&mut self) {
        self.stream = None;
    }

    pub async fn next(
        &mut self,
        node: &Arc<CoreNode>,
        peer: &str,
        refresh: &Updates,
    ) -> Result<DesktopFrame, CoreError> {
        loop {
            if self.snapshot_due {
                return self.step_frame(node, peer, refresh).await;
            }
            if let Some(tier) = self.policy.tier {
                let req = VideoOpen {
                    source: self.source,
                    tier,
                };
                if self.stream.is_none() {
                    let opened = self.open_video(node, peer, req).await;
                    if let Err(e) = opened {
                        self.failed(e)?;
                    }
                    if self.stream.is_none() {
                        continue;
                    }
                }
                let result = self.video().await;
                match result {
                    Ok(frame) => {
                        if self.changes.has_changed().unwrap_or(false) {
                            refresh.take(&mut self.changes);
                        }
                        return Ok(frame);
                    }
                    Err(e) => self.failed(e)?,
                }
            } else {
                self.wait_step(node, peer).await?;
            }
        }
    }

    async fn step_frame(
        &mut self,
        node: &Arc<CoreNode>,
        peer: &str,
        refresh: &Updates,
    ) -> Result<DesktopFrame, CoreError> {
        let shot = match refresh.take(&mut self.changes) {
            Some(shot) => shot,
            None => super::snapshot(node, peer, self.source).await?,
        };
        self.snapshot_due = false;
        Ok(DesktopFrame {
            info: DesktopFrameInfo {
                mode: "step",
                format: "jpeg",
                timestamp_us: shot.header.captured_at_ms * 1000,
                key: true,
                epoch: self.epoch,
                transform: shot.header.transform,
                encoder: "jpeg".into(),
                reason: self.reason.clone(),
            },
            bytes: shot.jpeg_bytes,
        })
    }

    async fn open_video(
        &mut self,
        node: &Arc<CoreNode>,
        peer: &str,
        req: VideoOpen,
    ) -> Result<(), CoreError> {
        tokio::select! {
            biased;
            changed = self.changes.changed() => {
                changed.map_err(|_| CoreError::Cancelled)?;
                self.snapshot_due = true;
            }
            opened = VideoStream::open(node, peer, req) => {
                self.stream = Some(opened?);
                self.epoch = self.epoch.wrapping_add(1);
            }
        }
        Ok(())
    }

    async fn video(&mut self) -> Result<DesktopFrame, CoreError> {
        let stream = self.stream.as_mut().expect("opened stream");
        let sample = stream.next().await?;
        let info = DesktopFrameInfo {
            mode: "live",
            format: "h264-annexb",
            timestamp_us: sample.head.timestamp_us,
            key: sample.head.key,
            epoch: self.epoch,
            transform: stream.ready.transform.clone(),
            encoder: stream.ready.encoder.clone(),
            reason: None,
        };
        if self
            .policy
            .observe(sample.rtt, sample.lag, sample.elapsed, sample.head.delivery)
        {
            self.stream = None;
            if self.policy.tier.is_none() {
                self.enter_step("视频延迟持续偏高".into());
            }
        }
        Ok(DesktopFrame {
            info,
            bytes: sample.data,
        })
    }

    fn failed(&mut self, e: CoreError) -> Result<(), CoreError> {
        self.stream = None;
        match e.error_code() {
            Some(
                ErrorCode::Forbidden
                | ErrorCode::Unauthorized
                | ErrorCode::Busy
                | ErrorCode::NotFound,
            ) => return Err(e),
            Some(ErrorCode::Unsupported) => {
                self.live_capable = false;
                self.enter_step(e.to_user_message());
            }
            Some(ErrorCode::Invalid) => {
                self.snapshot_due = true;
                self.reason = Some(e.to_user_message());
            }
            _ => {
                self.policy.lower();
                if self.policy.tier.is_none() {
                    self.enter_step(e.to_user_message());
                }
            }
        }
        Ok(())
    }

    fn enter_step(&mut self, reason: String) {
        self.policy.tier = None;
        self.snapshot_due = true;
        self.reason = Some(reason);
        self.probe_at = Instant::now() + self.policy.probe_delay;
    }

    async fn wait_step(&mut self, node: &Arc<CoreNode>, peer: &str) -> Result<(), CoreError> {
        let probe = async {
            tokio::time::sleep_until(self.probe_at.into()).await;
            node.remote_ping(peer).await
        };
        tokio::select! {
            biased;
            changed = self.changes.changed() => {
                changed.map_err(|_| CoreError::Cancelled)?;
                self.snapshot_due = true;
            }
            result = probe, if self.live_capable => {
                self.probe_at = Instant::now() + Duration::from_secs(2);
                let latency = match result {
                    Ok((_, latency)) => latency,
                    Err(CoreError::Timeout | CoreError::NotConnected(_)) => Duration::MAX,
                    Err(error) => return Err(error),
                };
                self.policy.probe(latency);
            }
        }
        Ok(())
    }
}
