//! Adaptive desktop sessions over two multiplexed, authenticated QUIC streams.
mod feedback;
mod host;
mod host_input;
mod host_video;
mod legacy;
mod policy;
mod session;
mod updates;
mod viewer;
mod wire;

pub use session::DesktopSession;
pub use viewer::{DesktopFrame, DesktopFrameInfo};

use crate::{CoreError, CoreNode};
use pcshare_proto::{ErrorCode, RpcError, ShotScreenArgs, ShotWindowArgs, desktop::Source};
use std::sync::Arc;

fn invalid(text: &str) -> CoreError {
    RpcError::new(ErrorCode::Invalid, text).into()
}

fn refresh_spec(source: Source) -> pcshare_proto::ShotRefreshSpec {
    pcshare_proto::ShotRefreshSpec {
        monitor_index: if let Source::Monitor(i) = source {
            Some(i)
        } else {
            None
        },
        hwnd: if let Source::Window(h) = source {
            Some(h)
        } else {
            None
        },
        max_width: Some(1280),
        quality: Some(66),
    }
}

async fn local_snapshot(node: &CoreNode, source: Source) -> Result<crate::CapturedShot, CoreError> {
    let spec = refresh_spec(source);
    match source {
        Source::Window(hwnd) => {
            node.local_shot_window(ShotWindowArgs {
                hwnd,
                max_width: spec.max_width,
                quality: spec.quality,
            })
            .await
        }
        Source::Monitor(_) => {
            node.local_shot_screen(ShotScreenArgs {
                monitor_index: spec.monitor_index,
                max_width: spec.max_width,
                quality: spec.quality,
            })
            .await
        }
    }
}

async fn snapshot(
    node: &Arc<CoreNode>,
    peer: &str,
    source: Source,
) -> Result<crate::CapturedShot, CoreError> {
    match source {
        Source::Window(hwnd) => {
            node.remote_shot_window(
                peer,
                ShotWindowArgs {
                    hwnd,
                    max_width: Some(1280),
                    quality: Some(66),
                },
            )
            .await
        }
        Source::Monitor(index) => {
            node.remote_shot_screen(
                peer,
                ShotScreenArgs {
                    monitor_index: Some(index),
                    max_width: Some(1280),
                    quality: Some(66),
                },
            )
            .await
        }
    }
}
