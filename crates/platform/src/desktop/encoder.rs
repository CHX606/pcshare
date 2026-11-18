use super::capture_args::{CapturePath, Encoder, configure};
use std::sync::Mutex;

type Route = (Encoder, CapturePath);
static LAST_ROUTE: Mutex<Option<Route>> = Mutex::new(None);
use super::{CaptureArea, CaptureSource, Encoding, VideoPacket, program};
use crate::PlatformError;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader, Lines};
use tokio::process::{Child, ChildStderr, ChildStdout, Command};

pub struct VideoCapture {
    _child: Child,
    bytes: BufReader<ChildStdout>,
    stats: Lines<BufReader<ChildStderr>>,
    encoder: Encoder,
    last_pts: Option<u64>,
}

impl VideoCapture {
    pub async fn open(
        data: &Path,
        source: CaptureSource,
        area: CaptureArea,
        cfg: Encoding,
    ) -> Result<(Self, VideoPacket), PlatformError> {
        let dir = data.to_owned();
        let exe = tokio::task::spawn_blocking(move || program::executable(&dir))
            .await
            .map_err(|e| PlatformError::System(e.to_string()))??;
        let mut errors = Vec::new();
        let last = *LAST_ROUTE.lock().unwrap_or_else(|e| e.into_inner());
        for (encoder, path) in last
            .into_iter()
            .chain(candidates().into_iter().filter(|r| Some(*r) != last))
        {
            match Self::try_encoder(&exe, source, area, cfg, encoder, path).await {
                Ok(capture) => {
                    *LAST_ROUTE.lock().unwrap_or_else(|e| e.into_inner()) = Some((encoder, path));
                    return Ok(capture);
                }
                Err(error) => errors.push(format!("{}: {error}", encoder.name())),
            }
        }
        Err(PlatformError::Unsupported(format!(
            "实时编码不可用：{}",
            errors.join(" / ")
        )))
    }

    async fn try_encoder(
        exe: &Path,
        source: CaptureSource,
        area: CaptureArea,
        cfg: Encoding,
        encoder: Encoder,
        path: CapturePath,
    ) -> Result<(Self, VideoPacket), PlatformError> {
        let mut cmd = Command::new(exe);
        configure(&mut cmd, source, area, cfg, encoder, path);
        cmd.kill_on_drop(true)
            .creation_flags(0x08000000)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(system)?;
        let bytes = BufReader::new(child.stdout.take().expect("piped stdout"));
        let stats = BufReader::new(child.stderr.take().expect("piped stderr")).lines();
        let mut capture = Self {
            _child: child,
            bytes,
            stats,
            encoder,
            last_pts: None,
        };
        let first = tokio::time::timeout(Duration::from_secs(3), capture.next())
            .await
            .map_err(|_| PlatformError::Unsupported("编码器未送出首帧".into()))??;
        if !first.key {
            return Err(PlatformError::System("编码器首帧无法独立解码".into()));
        }
        Ok((capture, first))
    }

    pub fn encoder(&self) -> &'static str {
        self.encoder.name()
    }

    pub async fn next(&mut self) -> Result<VideoPacket, PlatformError> {
        let mut last = String::new();
        while let Some(line) = self.stats.next_line().await.map_err(system)? {
            if let Some(info) = line.strip_prefix("PCSHARE ") {
                return self.packet(info).await;
            }
            if !line.is_empty() {
                last = line.chars().take(300).collect();
            }
        }
        Err(PlatformError::System(format!("编码进程已结束 {last}")))
    }

    async fn packet(&mut self, info: &str) -> Result<VideoPacket, PlatformError> {
        let malformed = || PlatformError::System("编码器返回了无效的帧长度或时间戳".into());
        let mut fields = info.split_ascii_whitespace();
        let len: usize = fields
            .next()
            .and_then(|s| s.parse().ok())
            .ok_or_else(malformed)?;
        let raw = packet_time(&mut fields)?;
        let timestamp_us = self
            .last_pts
            .map_or(raw, |old| raw.max(old.saturating_add(1)));
        self.last_pts = Some(timestamp_us);
        let key = match fields.next() {
            Some("K") => true,
            Some("N") => false,
            _ => return Err(malformed()),
        };
        if !(1..=4 * 1024 * 1024).contains(&len) {
            return Err(malformed());
        }
        let mut bytes = vec![0; len];
        self.bytes.read_exact(&mut bytes).await.map_err(system)?;
        Ok(VideoPacket {
            bytes,
            timestamp_us,
            key,
        })
    }
}

fn candidates() -> [Route; 13] {
    use CapturePath::{Gdi, Graphics, Staged};
    use Encoder::{Amd, Intel, MediaFoundation, Nvidia, Software};
    [
        (Nvidia, Graphics),
        (Intel, Graphics),
        (Amd, Graphics),
        (Nvidia, Staged),
        (Intel, Staged),
        (Amd, Staged),
        (MediaFoundation, Staged),
        (Software, Staged),
        (Nvidia, Gdi),
        (Intel, Gdi),
        (Amd, Gdi),
        (MediaFoundation, Gdi),
        (Software, Gdi),
    ]
}

fn system(e: std::io::Error) -> PlatformError {
    PlatformError::System(e.to_string())
}

fn packet_time(fields: &mut std::str::SplitAsciiWhitespace<'_>) -> Result<u64, PlatformError> {
    let malformed = || PlatformError::System("编码器返回了无效的帧长度或时间戳".into());
    let pts: i64 = fields
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(malformed)?;
    let tb = fields
        .next()
        .and_then(|s| s.split_once('/'))
        .ok_or_else(malformed)?;
    let num: u64 = tb.0.parse().map_err(|_| malformed())?;
    let den: u64 = tb.1.parse().map_err(|_| malformed())?;
    if num == 0 || den == 0 {
        return Err(malformed());
    }
    let us = (u128::from(pts.max(0) as u64) * u128::from(num)).saturating_mul(1_000_000)
        / u128::from(den);
    u64::try_from(us).map_err(|_| malformed())
}
