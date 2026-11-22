use crate::{CoreError, CoreNode};
use iroh::endpoint::{Connection, RecvStream, SendStream};
use pcshare_proto::desktop::{
    self, DesktopInput, MAX_VIDEO_PACKET, PacketHeader, VideoOpen, VideoReady,
};
use pcshare_proto::{ResponseEnvelope, read_frame, read_optional_frame, write_frame};
use serde::{Serialize, de::DeserializeOwned};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

pub(super) struct Channel {
    send: SendStream,
    recv: RecvStream,
}
impl Channel {
    pub async fn open<R: DeserializeOwned>(
        node: &Arc<CoreNode>,
        peer: &str,
        cmd: &str,
        args: impl Serialize,
    ) -> Result<(Self, R, Connection), CoreError> {
        let conn = node.get_or_connect(peer).await?;
        let env = desktop::envelope(cmd, args)?;
        let start = async {
            let (mut send, mut recv) = conn
                .open_bi()
                .await
                .map_err(|e| CoreError::NotConnected(e.to_string()))?;
            send.set_priority(16)
                .map_err(|e| CoreError::NotConnected(e.to_string()))?;
            write_frame(&mut send, &env).await?;
            let resp: ResponseEnvelope<R> = read_frame(&mut recv).await?;
            Ok((Self { send, recv }, resp.into_result()?, conn.clone()))
        };
        tokio::time::timeout(Duration::from_secs(24), start)
            .await
            .map_err(|_| CoreError::Timeout)?
    }

    /// 保持这条流直到会话取消或对方先结束，然后正常关闭，让对方据此收尾（例如还原分辨率）。
    pub async fn hold(mut self, cancel: CancellationToken) {
        tokio::select! {
            _ = cancel.cancelled() => {}
            _ = read_optional_frame::<_, u8>(&mut self.recv) => {}
        }
        let _ = self.send.finish();
    }

    pub async fn input(
        &mut self,
        events: Vec<DesktopInput>,
        refresh: bool,
    ) -> Result<Option<crate::CapturedShot>, CoreError> {
        tokio::time::timeout(Duration::from_secs(if refresh { 20 } else { 5 }), async {
            write_frame(&mut self.send, &desktop::ControlBatch { events, refresh }).await?;
            let resp: ResponseEnvelope<Option<pcshare_proto::ShotHeader>> =
                read_frame(&mut self.recv).await?;
            let Some(header) = resp.into_result()? else {
                return Ok(None);
            };
            if !refresh || header.byte_len as usize > pcshare_proto::MAX_FRAME_SIZE {
                return Err(super::invalid("输入回图长度无效"));
            }
            let mut jpeg_bytes = vec![0; header.byte_len as usize];
            self.recv
                .read_exact(&mut jpeg_bytes)
                .await
                .map_err(|e| CoreError::NotConnected(e.to_string()))?;
            Ok(Some(crate::CapturedShot { header, jpeg_bytes }))
        })
        .await
        .map_err(|_| CoreError::Timeout)?
    }
}

pub(super) struct VideoStream {
    channel: Channel,
    conn: Connection,
    pub ready: VideoReady,
    last_seq: u64,
    first: Option<(Instant, u64)>,
    previous: Instant,
    min_transit: i128,
}

pub(super) struct Sample {
    pub head: PacketHeader,
    pub data: Vec<u8>,
    pub rtt: Duration,
    pub lag: Duration,
    pub elapsed: Duration,
}

impl VideoStream {
    pub async fn open(
        node: &Arc<CoreNode>,
        peer: &str,
        args: VideoOpen,
    ) -> Result<Self, CoreError> {
        let (channel, ready, conn) = Channel::open(node, peer, desktop::VIDEO, args).await?;
        Ok(Self {
            channel,
            ready,
            conn,
            last_seq: 0,
            first: None,
            previous: Instant::now(),
            min_transit: 0,
        })
    }

    pub async fn next(&mut self) -> Result<Sample, CoreError> {
        if self.last_seq != 0 {
            write_frame(&mut self.channel.send, &self.last_seq).await?;
        }
        let head = self.packet_header().await?;
        let mut data = vec![0; head.bytes as usize];
        tokio::time::timeout(
            Duration::from_millis(1500),
            self.channel.recv.read_exact(&mut data),
        )
        .await
        .map_err(|_| CoreError::Timeout)?
        .map_err(|e| CoreError::NotConnected(e.to_string()))?;
        if let Some((x, y)) = head.origin {
            self.ready.transform.origin_x = x;
            self.ready.transform.origin_y = y;
        }
        self.last_seq = head.sequence;
        let now = Instant::now();
        let elapsed = now.duration_since(self.previous);
        self.previous = now;
        let (start, pts) = *self.first.get_or_insert((now, head.timestamp_us));
        let transit =
            start.elapsed().as_micros() as i128 - i128::from(head.timestamp_us.saturating_sub(pts));
        self.min_transit = self.min_transit.min(transit);
        let lag =
            Duration::from_micros((transit - self.min_transit).clamp(0, u64::MAX as i128) as u64);
        Ok(Sample {
            head,
            data,
            rtt: rtt(&self.conn),
            lag,
            elapsed,
        })
    }

    async fn packet_header(&mut self) -> Result<PacketHeader, CoreError> {
        let head = loop {
            let read = read_frame::<_, ResponseEnvelope<PacketHeader>>(&mut self.channel.recv);
            let meta = tokio::time::timeout(Duration::from_millis(1500), read)
                .await
                .map_err(|_| CoreError::Timeout)??
                .into_result()?;
            if meta.bytes != 0 || meta.sequence != 0 {
                break meta;
            }
        };
        if head.sequence != self.last_seq + 1
            || (self.last_seq == 0 && !head.key)
            || !(1..=MAX_VIDEO_PACKET).contains(&(head.bytes as usize))
        {
            return Err(super::invalid("视频帧头无效"));
        }
        Ok(head)
    }
}

pub(super) fn rtt(conn: &Connection) -> Duration {
    conn.paths()
        .iter()
        .find(|p| p.is_selected())
        .map(|p| p.rtt())
        .unwrap_or(Duration::from_millis(100))
}
