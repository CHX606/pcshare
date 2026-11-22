use super::{ManagedBridge, Reply, State};
use crate::bridge::{ApiErrorBody, UiErrorCode, UiShotTransform};
use pcshare_core::{
    CoreError,
    desktop::{DesktopFrame, DesktopSession},
};
use pcshare_proto::desktop::{DesktopInput, Source};
use serde::Serialize;
use std::sync::Arc;
use tauri::{Manager, Resource, ResourceId, Webview, ipc::Response};

struct SessionResource(Arc<DesktopSession>);
impl Resource for SessionResource {
    fn close(self: Arc<Self>) {
        self.0.close();
    }
}
impl Drop for SessionResource {
    fn drop(&mut self) {
        self.0.close();
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Opened {
    session_id: ResourceId,
    raw_input: bool,
}

#[tauri::command]
pub(crate) async fn desktop_open(
    webview: Webview,
    state: State<'_, ManagedBridge>,
    device: String,
    monitor_index: Option<u32>,
    hwnd: Option<String>,
) -> Result<Reply<Opened>, ()> {
    let source = match (monitor_index, hwnd) {
        (Some(_), Some(_)) => {
            return Ok(Reply::err(UiErrorCode::Invalid, "只能选择一个显示器或窗口"));
        }
        (i, None) => Source::Monitor(i.unwrap_or(0)),
        (_, Some(h)) => match h.parse::<u64>() {
            Ok(v) if v > 0 => Source::Window(v),
            _ => return Ok(Reply::err(UiErrorCode::Invalid, "无效的窗口编号")),
        },
    };
    let result = DesktopSession::open(state.node(), &device, source)
        .await
        .map(|session| {
            let raw_input = session.supports_raw_input();
            let session_id = webview
                .resources_table()
                .add(SessionResource(Arc::new(session)));
            Opened {
                session_id,
                raw_input,
            }
        });
    Ok(Reply::from_core(result))
}

fn session(webview: &Webview, id: ResourceId) -> Result<Arc<DesktopSession>, ApiErrorBody> {
    webview
        .resources_table()
        .get::<SessionResource>(id)
        .map(|r| r.0.clone())
        .map_err(|_| ApiErrorBody {
            code: UiErrorCode::NotFound,
            msg: "桌面会话已关闭".into(),
        })
}

#[tauri::command]
pub(crate) async fn desktop_next(
    webview: Webview,
    session_id: ResourceId,
) -> Result<Response, ApiErrorBody> {
    let frame = session(&webview, session_id)?
        .next()
        .await
        .map_err(api_error)?;
    packet(frame)
        .map(Response::new)
        .map_err(|e| api_error(CoreError::Internal(e.to_string())))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FrameInfo {
    mode: &'static str,
    format: &'static str,
    timestamp_us: u64,
    key: bool,
    epoch: u32,
    codec: Option<String>,
    encoder: String,
    reason: Option<String>,
    transform: UiShotTransform,
}

fn packet(frame: DesktopFrame) -> Result<Vec<u8>, serde_json::Error> {
    let f = frame.info;
    let t = f.transform;
    let transform = UiShotTransform {
        origin_x: t.origin_x,
        origin_y: t.origin_y,
        source_width: t.source_width,
        source_height: t.source_height,
        image_width: t.image_width,
        image_height: t.image_height,
        dpi_scale_percent: t.dpi_scale_percent,
        target_hwnd: t.target_hwnd.map(|v| v.to_string()),
        monitor_index: t.monitor_index,
    };
    let codec = if f.key && f.format == "h264-annexb" {
        avc_codec(&frame.bytes)
    } else {
        None
    };
    let info = FrameInfo {
        mode: f.mode,
        format: f.format,
        timestamp_us: f.timestamp_us,
        key: f.key,
        epoch: f.epoch,
        encoder: f.encoder,
        reason: f.reason,
        transform,
        codec,
    };
    let mut packet = Vec::with_capacity(4 + 256 + frame.bytes.len());
    packet.extend_from_slice(&[0u8; 4]);
    serde_json::to_writer(&mut packet, &info)?;
    let header_len = packet.len() - 4;
    packet[..4].copy_from_slice(&(header_len as u32).to_be_bytes());
    packet.extend_from_slice(&frame.bytes);
    Ok(packet)
}

fn avc_codec(data: &[u8]) -> Option<String> {
    let s = data
        .windows(7)
        .find(|n| n[..3] == [0, 0, 1] && n[3] & 31 == 7)?;
    Some(format!("avc1.{:02X}{:02X}{:02X}", s[4], s[5], s[6]))
}

fn api_error(e: CoreError) -> ApiErrorBody {
    match Reply::<()>::from_core(Err(e)) {
        Reply::Err { err } => err,
        Reply::Ok { .. } => unreachable!(),
    }
}

#[tauri::command]
pub(crate) async fn desktop_input(
    webview: Webview,
    session_id: ResourceId,
    events: Vec<DesktopInput>,
) -> Result<Reply<()>, ApiErrorBody> {
    let session = session(&webview, session_id)?;
    Ok(Reply::from_core(session.input(events).await))
}

#[tauri::command]
pub(crate) fn desktop_refresh(
    webview: Webview,
    session_id: ResourceId,
) -> Result<Reply<()>, ApiErrorBody> {
    session(&webview, session_id)?.refresh();
    Ok(Reply::ok(()))
}

#[tauri::command]
pub(crate) fn desktop_close(
    webview: Webview,
    session_id: ResourceId,
) -> Result<Reply<()>, ApiErrorBody> {
    let mut table = webview.resources_table();
    let resource = table
        .take::<SessionResource>(session_id)
        .map_err(|_| ApiErrorBody {
            code: UiErrorCode::NotFound,
            msg: "桌面会话已关闭".into(),
        })?;
    resource.0.close();
    Ok(Reply::ok(()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pcshare_core::desktop::DesktopFrameInfo;
    use pcshare_proto::ShotTransform;
    #[test]
    fn binary_packet_preserves_video_and_decodes_with_frontend_field_names() {
        let bytes = vec![0, 0, 0, 1, 0x67, 0x64, 0x00, 0x28, 0, 0, 1, 0x65, 1];
        let transform = ShotTransform {
            origin_x: -1920,
            origin_y: 0,
            source_width: 1920,
            source_height: 1080,
            image_width: 1280,
            image_height: 720,
            dpi_scale_percent: 125,
            target_hwnd: Some(123),
            monitor_index: None,
        };
        let frame = DesktopFrame {
            info: DesktopFrameInfo {
                mode: "live",
                format: "h264-annexb",
                timestamp_us: 1000,
                key: true,
                epoch: 2,
                transform,
                encoder: "test".into(),
                reason: None,
            },
            bytes: bytes.clone(),
        };
        let wire = packet(frame).unwrap();
        let len = u32::from_be_bytes(wire[..4].try_into().unwrap()) as usize;
        let meta: serde_json::Value = serde_json::from_slice(&wire[4..4 + len]).unwrap();
        assert_eq!(meta["codec"], "avc1.640028");
        assert_eq!(meta["timestampUs"], 1000);
        assert_eq!(meta["transform"]["targetHwnd"], "123");
        assert_eq!(meta["transform"]["originX"], -1920);
        assert_eq!(&wire[4 + len..], bytes.as_slice());
    }
}
