//! Native input and video services used by an authorized desktop session.
#[cfg(not(windows))]
use crate::PlatformError;
use crate::PlatformMouseBtn;

#[cfg(windows)]
mod capture_args;
#[cfg(windows)]
mod encoder;
#[cfg(windows)]
mod pointer;
#[cfg(windows)]
mod program;
#[cfg(windows)]
mod target;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ButtonState {
    Mouse(PlatformMouseBtn),
    Key(u8),
}

#[derive(Clone, Copy)]
pub enum PointerAction {
    Move(i32, i32),
    Button(i32, i32, PlatformMouseBtn, bool),
    Wheel(i32, i32, i16, i16),
    Key(u8, bool),
    Release(ButtonState),
}

#[derive(Clone, Copy, Debug)]
pub enum CaptureSource {
    Monitor(u32),
    Window(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CaptureArea {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub dpi: u32,
    pub handle: usize,
}

#[derive(Clone, Copy)]
pub struct Encoding {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u32,
}

pub struct VideoPacket {
    pub bytes: Vec<u8>,
    pub timestamp_us: u64,
    pub key: bool,
}

#[cfg(windows)]
pub use encoder::VideoCapture;
#[cfg(windows)]
pub use pointer::{inject, verify_target};
#[cfg(windows)]
pub use target::capture_area;

#[cfg(not(windows))]
mod unsupported {
    use super::*;
    pub struct VideoCapture;
    fn error() -> PlatformError {
        PlatformError::Unsupported("实时桌面需要 Windows".into())
    }
    pub fn verify_target(
        _: Option<usize>,
        _: Option<(i32, i32)>,
        _: bool,
    ) -> Result<(), PlatformError> {
        Err(error())
    }
    pub fn inject(_: PointerAction, _: Option<usize>) -> Result<(), PlatformError> {
        Err(error())
    }
    pub fn capture_area(
        _: CaptureSource,
        _: &[crate::PlatformMonitor],
    ) -> Result<CaptureArea, PlatformError> {
        Err(error())
    }
    impl VideoCapture {
        pub async fn open(
            _: &std::path::Path,
            _: CaptureSource,
            _: CaptureArea,
            _: Encoding,
        ) -> Result<(Self, VideoPacket), PlatformError> {
            Err(error())
        }
        pub async fn next(&mut self) -> Result<VideoPacket, PlatformError> {
            Err(error())
        }
        pub fn encoder(&self) -> &'static str {
            "unsupported"
        }
    }
}
#[cfg(not(windows))]
pub use unsupported::*;
