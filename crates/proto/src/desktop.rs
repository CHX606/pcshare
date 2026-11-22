//! The two desktop streams share the existing authenticated main QUIC connection.
use crate::{ErrorCode, MouseBtn, PROTOCOL_VERSION, RequestEnvelope, RpcError, ShotTransform};
use serde::{Deserialize, Serialize};

pub const VIDEO: &str = "desktop.video";
pub const CONTROL: &str = "desktop.control";
/// 看屏期间让被控端把一块显示器临时切到控制端的分辨率；流关闭即还原。
pub const DISPLAY: &str = "desktop.display";
/// 被控端支持 [`DISPLAY`] 时在能力列表里带上这一项。
pub const CAP_DISPLAY_FIT: &str = "desktop.display.v1";
pub const MAX_VIDEO_PACKET: usize = 4 * 1024 * 1024;

/// [`DISPLAY`] 的请求：把第 `monitor` 块显示器调到接近 `width × height`（物理像素）。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct DisplayFit {
    pub monitor: u32,
    pub width: u32,
    pub height: u32,
}

impl DisplayFit {
    pub fn validate(&self) -> Result<(), RpcError> {
        let ok = (320..=16_384).contains(&self.width) && (240..=16_384).contains(&self.height);
        ok.then_some(())
            .ok_or_else(|| RpcError::new(ErrorCode::Invalid, "本机分辨率参数无效"))
    }
}

/// [`DISPLAY`] 的回复。`applied` 为假时屏幕保持原样，`note` 说明原因。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DisplayFitted {
    pub applied: bool,
    pub width: u32,
    pub height: u32,
    pub previous_width: u32,
    pub previous_height: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VideoTier {
    High,
    Balanced,
    Economy,
}

impl VideoTier {
    pub fn settings(self) -> (u32, u32, u32) {
        match self {
            Self::High => (1920, 60, 8_000_000),
            Self::Balanced => (1600, 30, 3_500_000),
            Self::Economy => (1024, 20, 1_000_000),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Monitor(u32),
    Window(u64),
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct VideoOpen {
    pub source: Source,
    pub tier: VideoTier,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct VideoReady {
    pub transform: ShotTransform,
    pub encoder: String,
    pub fps: u32,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Delivery {
    pub capacity_bps: Option<u64>,
    pub loss_per_mille: u16,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct PacketHeader {
    pub bytes: u32,
    pub timestamp_us: u64,
    pub key: bool,
    pub sequence: u64,
    #[serde(default)]
    pub delivery: Delivery,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<(i32, i32)>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum DesktopInput {
    Move {
        x: i32,
        y: i32,
    },
    Button {
        x: i32,
        y: i32,
        button: MouseBtn,
        down: bool,
    },
    Wheel {
        x: i32,
        y: i32,
        delta_x: i16,
        delta_y: i16,
    },
    Key {
        vk: u8,
        down: bool,
    },
    Click {
        x: i32,
        y: i32,
        button: MouseBtn,
        #[serde(default)]
        double: bool,
    },
    Text {
        text: String,
    },
    Shortcut {
        combo: String,
    },
    Release,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ControlBatch {
    pub events: Vec<DesktopInput>,
    pub refresh: bool,
}

impl DesktopInput {
    pub fn completes_action(&self) -> bool {
        !matches!(
            self,
            Self::Move { .. }
                | Self::Key { down: true, .. }
                | Self::Button { down: true, .. }
                | Self::Release
        )
    }

    pub fn validate(&self) -> Result<(), RpcError> {
        let valid = match self {
            Self::Text { text } => !text.is_empty() && text.len() <= 16_384,
            Self::Shortcut { combo } => !combo.is_empty() && combo.len() <= 128,
            Self::Key { vk, .. } => (8..=254).contains(vk),
            _ => true,
        };
        valid
            .then_some(())
            .ok_or_else(|| RpcError::new(ErrorCode::Invalid, "桌面输入参数无效"))
    }
}

pub fn envelope(cmd: &str, args: impl Serialize) -> Result<RequestEnvelope, crate::FrameError> {
    Ok(RequestEnvelope {
        v: PROTOCOL_VERSION,
        cmd: cmd.into(),
        args: serde_json::to_value(args)?,
    })
}
