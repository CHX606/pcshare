use super::updates::Updates;
use super::{
    viewer::{DesktopFrame, Viewer},
    wire::{self, Channel},
};
use crate::{CoreError, CoreNode};
use pcshare_proto::desktop::{self, DesktopInput as Event, Source};
use pcshare_proto::{ErrorCode, RpcError};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

pub struct DesktopSession {
    node: Arc<CoreNode>,
    peer: String,
    source: Source,
    modern: bool,
    viewer: Mutex<Viewer>,
    control: Mutex<Option<Channel>>,
    refresh: Updates,
    step: AtomicBool,
    motion_version: AtomicU64,
    cancel: CancellationToken,
}

impl DesktopSession {
    pub async fn open(node: &Arc<CoreNode>, peer: &str, source: Source) -> Result<Self, CoreError> {
        if peer == "self" || peer == node.endpoint_id().to_string() {
            return Err(super::invalid("本机预览请使用截图接口"));
        }
        let conn = node.get_or_connect(peer).await?;
        let targets = node.remote_shot_monitors(peer).await?;
        let found = match source {
            Source::Window(h) => targets.windows.iter().any(|w| w.hwnd == h),
            Source::Monitor(i) => targets.monitors.iter().any(|m| m.index == i),
        };
        if !found {
            return Err(RpcError::new(ErrorCode::NotFound, "找不到所选的显示器或窗口").into());
        }
        let modern = node
            .storage
            .get_peer(peer)
            .is_some_and(|p| p.capabilities.iter().any(|c| c == "desktop.v1"));
        let (refresh, changes) = Updates::new();
        Ok(Self {
            node: node.clone(),
            peer: peer.into(),
            source,
            modern,
            viewer: Mutex::new(Viewer::new(source, modern, wire::rtt(&conn), changes)),
            control: Mutex::new(None),
            refresh,
            step: AtomicBool::new(true),
            motion_version: AtomicU64::new(0),
            cancel: CancellationToken::new(),
        })
    }

    pub fn supports_raw_input(&self) -> bool {
        self.modern
    }
    pub fn refresh(&self) {
        self.refresh.publish(None);
    }
    pub fn close(&self) {
        self.cancel.cancel();
        if let Ok(mut view) = self.viewer.try_lock() {
            view.close();
        }
        if let Ok(mut control) = self.control.try_lock() {
            control.take();
        }
    }

    pub async fn next(&self) -> Result<DesktopFrame, CoreError> {
        let mut view = self
            .viewer
            .try_lock()
            .map_err(|_| RpcError::new(ErrorCode::Busy, "前一帧尚未返回"))?;
        let frame = tokio::select! {
            biased;
            _ = self.cancel.cancelled() => {
                view.close();
                return Err(CoreError::Cancelled);
            }
            result = view.next(&self.node, &self.peer, &self.refresh) => result?,
        };
        self.step
            .store(frame.info.mode == "step", Ordering::Release);
        Ok(frame)
    }

    pub async fn input(&self, events: Vec<Event>) -> Result<(), CoreError> {
        if events.is_empty() || events.len() > 64 {
            return Err(super::invalid("输入批次须包含 1 至 64 个事件"));
        }
        for e in &events {
            e.validate()?;
        }
        let version = self
            .motion_version
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        let events = compact_moves(events);
        let motion = matches!(events.as_slice(), [Event::Move { .. }]).then_some(version);
        let changed = events.iter().any(Event::completes_action);
        if self.cancel.is_cancelled() {
            return Err(CoreError::Cancelled);
        }
        let refresh = changed && self.step.load(Ordering::Acquire);
        match self.write_input(events, refresh, motion).await {
            Ok(shot) => {
                if refresh {
                    self.refresh.publish(shot);
                }
                Ok(())
            }
            Err(e) => {
                if refresh {
                    self.refresh();
                }
                Err(e)
            }
        }
    }

    async fn write_input(
        &self,
        events: Vec<Event>,
        refresh: bool,
        motion: Option<u64>,
    ) -> Result<Option<crate::CapturedShot>, CoreError> {
        let mut guard = tokio::select! {
            biased;
            _ = self.cancel.cancelled() => return Err(CoreError::Cancelled),
            guard = self.control.lock() => guard,
        };
        if motion.is_some_and(|n| self.motion_version.load(Ordering::Acquire) != n) {
            return Ok(None);
        }
        if !self.modern {
            return self.legacy_batch(events, refresh).await;
        }
        if guard.is_none() {
            let open = Channel::open(&self.node, &self.peer, desktop::CONTROL, self.source);
            let (channel, (), _) = tokio::select! {
                biased;
                _ = self.cancel.cancelled() => return Err(CoreError::Cancelled),
                result = open => result?,
            };
            *guard = Some(channel);
        }
        let send = guard.as_mut().expect("opened input").input(events, refresh);
        let res = tokio::select! {
            biased;
            _ = self.cancel.cancelled() => Err(CoreError::Cancelled),
            result = send => result,
        };
        if res.is_err() {
            *guard = None;
        }
        res
    }
    async fn legacy_batch(
        &self,
        events: Vec<Event>,
        refresh: bool,
    ) -> Result<Option<crate::CapturedShot>, CoreError> {
        let last = events.iter().rposition(Event::completes_action);
        let mut shot = None;
        for (i, event) in events.into_iter().enumerate() {
            let capture = refresh && last == Some(i);
            let send = super::legacy::send(&self.node, &self.peer, self.source, event, capture);
            let result = tokio::select! {
                biased;
                _ = self.cancel.cancelled() => return Err(CoreError::Cancelled),
                result = send => result?,
            };
            if result.is_some() {
                shot = result;
            }
        }
        Ok(shot)
    }
}

fn compact_moves(events: Vec<Event>) -> Vec<Event> {
    let mut ordered = Vec::with_capacity(events.len());
    for event in events {
        if matches!(event, Event::Move { .. }) && matches!(ordered.last(), Some(Event::Move { .. }))
        {
            ordered.pop();
        }
        ordered.push(event);
    }
    ordered
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mouse_coalescing_preserves_order_across_key_transitions() {
        let m = |x| Event::Move { x, y: 4 };
        let down = Event::Key { vk: 16, down: true };
        let up = Event::Key {
            vk: 16,
            down: false,
        };
        let events = vec![m(1), m(2), down.clone(), m(3), m(4), up.clone(), m(5)];
        assert_eq!(compact_moves(events), vec![m(2), down, m(4), up, m(5)]);
        assert!(
            Event::Text {
                text: String::new()
            }
            .validate()
            .is_err()
        );
        assert!(!Event::Key { vk: 16, down: true }.completes_action());
        assert!(
            Event::Key {
                vk: 16,
                down: false
            }
            .completes_action()
        );
    }
}
