use super::feedback::Feedback;
use crate::{CoreError, CoreNode};
use iroh::endpoint::{Connection, RecvStream, SendStream};
use pcshare_platform::desktop::{
    CaptureArea, CaptureSource, Encoding, VideoCapture, VideoPacket, capture_area,
};
use pcshare_proto::desktop::{PacketHeader, Source, VideoOpen, VideoReady};
use pcshare_proto::{
    PermissionScope, ResponseEnvelope, ShotTransform, read_optional_frame, write_frame,
};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::watch;

pub(super) async fn serve(
    node: &Arc<CoreNode>,
    peer: &str,
    conn: &Connection,
    req: VideoOpen,
    send: &mut SendStream,
    recv: &mut RecvStream,
) -> Result<(), CoreError> {
    node.desktop_unlocked()?;
    let source = native(req.source);
    let area = geometry(node, source)?;
    let cfg = settings(area, req);
    let (ack, credits) = watch::channel(0u64);
    let video = async {
        let open = VideoCapture::open(node.storage.data_dir(), source, area, cfg);
        let (encoder, first) = tokio::select! {
            result = open => result?,
            result = node.watch_desktop_access(peer, PermissionScope::Screenshot) => return result,
        };
        node.desktop_access(peer, PermissionScope::Screenshot)?;
        node.desktop_unlocked()?;
        announce(send, req, area, cfg, encoder.encoder()).await?;
        let mut stream = Producer {
            encoder,
            first: Some(first),
            feedback: Feedback::new(conn),
            credits,
            sequence: 0,
            area,
            source,
            checked: Instant::now(),
        };
        stream.pump(node, peer, send).await
    };
    tokio::select! {
        result = video => result,
        result = acknowledgements(recv, ack) => result,
    }
}

struct Producer {
    encoder: VideoCapture,
    first: Option<VideoPacket>,
    credits: watch::Receiver<u64>,
    feedback: Feedback,
    sequence: u64,
    area: CaptureArea,
    source: CaptureSource,
    checked: Instant,
}

impl Producer {
    async fn pump(
        &mut self,
        node: &CoreNode,
        peer: &str,
        send: &mut SendStream,
    ) -> Result<(), CoreError> {
        loop {
            if self.sequence >= 4 {
                let limit = self.sequence - 3;
                let credit = tokio::time::timeout(
                    Duration::from_millis(650),
                    self.credits.wait_for(|ack| *ack >= limit),
                )
                .await;
                credit
                    .map_err(|_| CoreError::Timeout)?
                    .map_err(|_| CoreError::Cancelled)?;
            }
            let frame = match self.first.take() {
                Some(f) => f,
                None => next_or_heartbeat(&mut self.encoder, node, peer, send).await?,
            };
            node.desktop_access(peer, PermissionScope::Screenshot)?;
            let origin = self.update_area(node)?;
            self.sequence += 1;
            send_packet(send, self.sequence, frame, self.feedback.sample(), origin).await?;
        }
    }
    fn update_area(&mut self, node: &CoreNode) -> Result<Option<(i32, i32)>, CoreError> {
        if self.checked.elapsed() < Duration::from_millis(200) {
            return Ok(None);
        }
        self.checked = Instant::now();
        node.desktop_unlocked()?;
        let next = geometry(node, self.source)?;
        if next.width != self.area.width
            || next.height != self.area.height
            || next.handle != self.area.handle
            || next.dpi != self.area.dpi
        {
            return Err(super::invalid("画面尺寸已变化，正在重新同步"));
        }
        let moved = next.x != self.area.x || next.y != self.area.y;
        self.area = next;
        Ok(moved.then_some((next.x, next.y)))
    }
}

async fn next_or_heartbeat(
    encoder: &mut VideoCapture,
    node: &CoreNode,
    peer: &str,
    send: &mut SendStream,
) -> Result<VideoPacket, CoreError> {
    let next = encoder.next();
    tokio::pin!(next);
    loop {
        tokio::select! {
            frame = &mut next => return Ok(frame?),
            _ = tokio::time::sleep(Duration::from_millis(350)) => {
                node.desktop_access(peer, PermissionScope::Screenshot)?;
                node.desktop_unlocked()?;
                let idle = PacketHeader { bytes: 0, timestamp_us: 0, key: false, sequence: 0, delivery: Default::default(), origin: None };
                write_frame(send, &ResponseEnvelope::ok(idle)).await?;
            }
        }
    }
}

async fn send_packet(
    send: &mut SendStream,
    sequence: u64,
    f: VideoPacket,
    delivery: pcshare_proto::desktop::Delivery,
    origin: Option<(i32, i32)>,
) -> Result<(), CoreError> {
    let head = PacketHeader {
        bytes: f.bytes.len() as u32,
        timestamp_us: f.timestamp_us,
        key: f.key,
        sequence,
        delivery,
        origin,
    };
    let write = async {
        write_frame(send, &ResponseEnvelope::ok(head)).await?;
        send.write_all(&f.bytes)
            .await
            .map_err(|e| CoreError::NotConnected(e.to_string()))
    };
    match tokio::time::timeout(Duration::from_millis(900), write).await {
        Ok(r) => r,
        Err(_) => {
            let _ = send.reset(1u32.into());
            Err(CoreError::Timeout)
        }
    }
}

async fn acknowledgements(recv: &mut RecvStream, ack: watch::Sender<u64>) -> Result<(), CoreError> {
    let mut previous = 0;
    while let Some(n) = read_optional_frame::<_, u64>(recv).await? {
        if n != previous + 1 {
            return Err(super::invalid("桌面帧确认顺序错误"));
        }
        previous = n;
        ack.send_replace(n);
    }
    Ok(())
}

fn native(source: Source) -> CaptureSource {
    match source {
        Source::Window(h) => CaptureSource::Window(h as usize),
        Source::Monitor(i) => CaptureSource::Monitor(i),
    }
}

fn geometry(node: &CoreNode, source: CaptureSource) -> Result<CaptureArea, CoreError> {
    let monitors = if matches!(source, CaptureSource::Monitor(_)) {
        node.platform.list_monitors()?
    } else {
        vec![]
    };
    Ok(capture_area(source, &monitors)?)
}

fn settings(a: CaptureArea, req: VideoOpen) -> Encoding {
    let (max, fps, bitrate) = req.tier.settings();
    let scale = (max as f64 / a.width.max(a.height) as f64).min(1.0);
    let fit = |v: u32| ((v as f64 * scale) as u32 / 2 * 2).max(2);
    Encoding {
        width: fit(a.width),
        height: fit(a.height),
        fps,
        bitrate,
    }
}

fn transform(a: CaptureArea, c: Encoding, s: Source) -> ShotTransform {
    ShotTransform {
        origin_x: a.x,
        origin_y: a.y,
        source_width: a.width,
        source_height: a.height,
        image_width: c.width,
        image_height: c.height,
        dpi_scale_percent: a.dpi,
        target_hwnd: if let Source::Window(h) = s {
            Some(h)
        } else {
            None
        },
        monitor_index: if let Source::Monitor(i) = s {
            Some(i)
        } else {
            None
        },
    }
}

async fn announce(
    send: &mut SendStream,
    req: VideoOpen,
    area: CaptureArea,
    cfg: Encoding,
    name: &str,
) -> Result<(), CoreError> {
    let ready = VideoReady {
        transform: transform(area, cfg, req.source),
        fps: cfg.fps,
        encoder: name.into(),
    };
    send.set_priority(-4)
        .map_err(|e| CoreError::NotConnected(e.to_string()))?;
    write_frame(send, &ResponseEnvelope::ok(ready)).await?;
    Ok(())
}
