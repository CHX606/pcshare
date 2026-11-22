use super::capture_args::{CapturePath, Encoder, configure};
use super::{CaptureArea, CaptureSource, Encoding, VideoPacket, program};
use crate::PlatformError;
use std::path::Path;
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader, Lines};
use tokio::process::{Child, ChildStderr, ChildStdout, Command};

type Route = (Encoder, CapturePath);
static LAST_ROUTE: Mutex<Option<Route>> = Mutex::new(None);
static DXGI_VENDORS: OnceLock<Option<Vec<u32>>> = OnceLock::new();

const STATS_PREFIX: &str = "PCSHARE_FRAME ";
const STATS_TOKEN: &str = "PCSHARE_FRAME";
const STATS_END: &str = "END";
const MAX_PACKET_BYTES: usize = 4 * 1024 * 1024;

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
        let bytes = BufReader::with_capacity(128 * 1024, child.stdout.take().expect("piped stdout"));
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
            match parse_stats_line(&line) {
                StatsLine::Packet(header) => return self.read_packet(header).await,
                StatsLine::Corrupted => {
                    return Err(PlatformError::System(
                        "编码器帧统计输出受损（疑似与错误日志混流），正在重置视频流".into(),
                    ));
                }
                StatsLine::Log => {
                    if !line.is_empty() {
                        last = line.chars().take(300).collect();
                    }
                }
            }
        }
        Err(PlatformError::System(format!("编码进程已结束 {last}")))
    }

    async fn read_packet(&mut self, header: FrameHeader) -> Result<VideoPacket, PlatformError> {
        let timestamp_us = self
            .last_pts
            .map_or(header.raw_pts_us, |old| header.raw_pts_us.max(old.saturating_add(1)));
        self.last_pts = Some(timestamp_us);
        let mut bytes = vec![0u8; header.len];
        self.bytes.read_exact(&mut bytes).await.map_err(system)?;
        if !is_valid_annex_b_packet(&bytes, header.key) {
            return Err(PlatformError::System(
                "H.264 帧边界失同步（缺少有效 Annex B 起始码），正在重置视频流".into(),
            ));
        }
        Ok(VideoPacket {
            bytes,
            timestamp_us,
            key: header.key,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FrameHeader {
    len: usize,
    raw_pts_us: u64,
    key: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatsLine {
    Packet(FrameHeader),
    Log,
    Corrupted,
}

fn parse_stats_line(line: &str) -> StatsLine {
    let trimmed = line.trim_end_matches('\r');
    let Some(pos) = trimmed.find(STATS_PREFIX) else {
        return if trimmed.contains(STATS_TOKEN) {
            StatsLine::Corrupted
        } else {
            StatsLine::Log
        };
    };
    let rest = &trimmed[pos + STATS_PREFIX.len()..];
    if rest.contains(STATS_TOKEN) {
        return StatsLine::Corrupted;
    }
    match parse_frame_header(rest) {
        Ok(hdr) => StatsLine::Packet(hdr),
        Err(_) => StatsLine::Corrupted,
    }
}

fn parse_frame_header(info: &str) -> Result<FrameHeader, PlatformError> {
    let malformed = || PlatformError::System("编码器返回了无效的帧长度或时间戳".into());
    let mut fields = info.split_ascii_whitespace();
    let len: usize = fields
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(malformed)?;
    let raw_pts_us = packet_time(&mut fields)?;
    let key = match fields.next() {
        Some("K") => true,
        Some("N") => false,
        _ => return Err(malformed()),
    };
    if fields.next() != Some(STATS_END) || fields.next().is_some() {
        return Err(malformed());
    }
    if !(1..=MAX_PACKET_BYTES).contains(&len) {
        return Err(malformed());
    }
    Ok(FrameHeader {
        len,
        raw_pts_us,
        key,
    })
}

/// 校验 FFmpeg 输出的单个 H.264 访问单元以合法 Annex B NAL 起始码开头（且关键帧包含 SPS 或 IDR），
/// 用于在 `stderr` 意外丢行或混流时立刻检测字节流错位并触发重连。
fn is_valid_annex_b_packet(bytes: &[u8], key: bool) -> bool {
    let first_nal_offset = if bytes.starts_with(&[0, 0, 0, 1]) {
        4
    } else if bytes.starts_with(&[0, 0, 1]) {
        3
    } else {
        return false;
    };
    let Some(&first_hdr) = bytes.get(first_nal_offset) else {
        return false;
    };
    // H.264 NAL 头最高位 forbidden_zero_bit 必须为 0，且 nal_unit_type 不得为 0
    if (first_hdr & 0x80) != 0 || (first_hdr & 0x1F) == 0 {
        return false;
    }
    if !key {
        return true;
    }
    // 关键帧中至少应包含一个 IDR (5) 或 SPS (7) NAL 单元
    let mut i = 0usize;
    while i + 3 < bytes.len() {
        if bytes[i] == 0 && bytes[i + 1] == 0 {
            let nal_pos = if bytes[i + 2] == 1 {
                Some(i + 3)
            } else if i + 4 < bytes.len() && bytes[i + 2] == 0 && bytes[i + 3] == 1 {
                Some(i + 4)
            } else {
                None
            };
            if let Some(pos) = nal_pos {
                if let Some(&hdr) = bytes.get(pos)
                    && (hdr & 0x80) == 0
                    && matches!(hdr & 0x1F, 5 | 7)
                {
                    return true;
                }
                i = pos;
                continue;
            }
        }
        i += 1;
    }
    false
}

fn detect_dxgi_hardware_vendors() -> Option<Vec<u32>> {
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, IDXGIFactory1,
    };
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1().ok()? };
    let mut vendors = Vec::new();
    let mut idx = 0u32;
    while let Ok(adapter) = unsafe { factory.EnumAdapters1(idx) } {
        idx += 1;
        if let Ok(desc) = unsafe { adapter.GetDesc1() } {
            let is_software = (desc.Flags & (DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32)) != 0
                || desc.VendorId == 0x1414;
            if !is_software && !vendors.contains(&desc.VendorId) {
                vendors.push(desc.VendorId);
            }
        }
    }
    Some(vendors)
}

fn vendor_to_encoder(vendor_id: u32) -> Option<Encoder> {
    match vendor_id {
        0x10DE => Some(Encoder::Nvidia),
        0x8086 => Some(Encoder::Intel),
        0x1002 | 0x1022 => Some(Encoder::Amd),
        _ => None,
    }
}

fn candidates_for_vendors(vendors: Option<&[u32]>) -> Vec<Route> {
    use CapturePath::{Gdi, Graphics, Staged};
    use Encoder::{Amd, Intel, MediaFoundation, Nvidia, Software};

    match vendors {
        Some(list) => {
            let mut hw = Vec::with_capacity(3);
            for &vid in list {
                if let Some(enc) = vendor_to_encoder(vid)
                    && !hw.contains(&enc)
                {
                    hw.push(enc);
                }
            }
            let mut routes = Vec::with_capacity(hw.len() * 3 + 4);
            for &enc in &hw {
                routes.push((enc, Graphics));
            }
            for &enc in &hw {
                routes.push((enc, Staged));
            }
            routes.push((MediaFoundation, Staged));
            routes.push((Software, Staged));
            for &enc in &hw {
                routes.push((enc, Gdi));
            }
            routes.push((MediaFoundation, Gdi));
            routes.push((Software, Gdi));
            routes
        }
        None => vec![
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
        ],
    }
}

fn candidates() -> Vec<Route> {
    let cached = DXGI_VENDORS.get_or_init(detect_dxgi_hardware_vendors);
    candidates_for_vendors(cached.as_deref())
}

#[doc(hidden)]
pub fn prioritized_encoder_names_for_vendors(vendors: Option<&[u32]>) -> Vec<&'static str> {
    candidates_for_vendors(vendors)
        .into_iter()
        .map(|(enc, _)| enc.name())
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_parser_accepts_clean_and_log_prefixed_lines_and_flags_corruption() {
        assert_eq!(
            parse_stats_line("PCSHARE_FRAME 1024 9000 1/90000 K END"),
            StatsLine::Packet(FrameHeader {
                len: 1024,
                raw_pts_us: 100_000,
                key: true,
            })
        );
        // FFmpeg 日志未换行紧接统计行时仍能准确提取
        assert_eq!(
            parse_stats_line("[h264_nvenc @ 0x1] warnPCSHARE_FRAME 512 18000 1/90000 N END"),
            StatsLine::Packet(FrameHeader {
                len: 512,
                raw_pts_us: 200_000,
                key: false,
            })
        );
        // 普通错误日志行
        assert_eq!(
            parse_stats_line("[libx264 @ 0x1] some normal warning"),
            StatsLine::Log
        );
        // 日志插队进统计行内部或缺少 END 尾标记必须判定受损而非静默跳过
        assert_eq!(
            parse_stats_line("PCSHARE_FRAME 1024 [err] 9000 1/90000 K END"),
            StatsLine::Corrupted
        );
        assert_eq!(
            parse_stats_line("PCSHARE_FRAME 1024 9000 1/90000 K"),
            StatsLine::Corrupted
        );
        assert_eq!(
            parse_stats_line("PCSHARE_FRAME 1024 9000 1/90000 K END trailing"),
            StatsLine::Corrupted
        );
        assert_eq!(
            parse_stats_line("PCSHARE_FRAME"),
            StatsLine::Corrupted
        );
    }

    #[test]
    fn annex_b_validator_detects_stream_desync() {
        // 4 字节起始码 + SPS (0x67) + IDR (0x65)
        let valid_key = [0, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 1, 0x65, 0x88];
        assert!(is_valid_annex_b_packet(&valid_key, true));
        assert!(is_valid_annex_b_packet(&valid_key, false));

        // 3 字节起始码 + 非关键帧 slice (0x41)
        let valid_delta = [0, 0, 1, 0x41, 0x9A, 0x01];
        assert!(is_valid_annex_b_packet(&valid_delta, false));
        assert!(!is_valid_annex_b_packet(&valid_delta, true));

        // 失同步后的随机字节（无 Annex B 起始码）
        let desync = [0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x01];
        assert!(!is_valid_annex_b_packet(&desync, false));
        assert!(!is_valid_annex_b_packet(&desync, true));

        // forbidden_zero_bit == 1 的非法 NAL 头
        let bad_nal = [0, 0, 0, 1, 0x85, 0x10];
        assert!(!is_valid_annex_b_packet(&bad_nal, false));
    }
}
