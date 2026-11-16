//! 看一眼（屏幕与窗口截图，任务 3.2）与在截图上点按输入（任务 3.3）。

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use iroh::endpoint::SendStream;
use pcshare_platform::{CapturedImage, Platform, PlatformMouseBtn};
use pcshare_proto::{
    CapturableWindow, Command, ErrorCode, InputClickArgs, InputKeyArgs, InputReply, MonitorInfo,
    MouseBtn, Permissions, ResponseEnvelope, RpcError, ShotHeader, ShotMonitorsReply,
    ShotRefreshSpec, ShotScreenArgs, ShotTransform, ShotWindowArgs, read_frame, write_frame,
};
use tokio::time::timeout;

use crate::CoreNode;
use crate::app_proc::to_proto_desktop;
use crate::error::{CoreError, map_platform_to_rpc};

const SHOT_RPC_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone)]
pub struct CapturedShot {
    pub header: ShotHeader,
    pub jpeg_bytes: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct InputResult {
    pub reply: InputReply,
    pub refreshed_jpeg: Option<Vec<u8>>,
}

fn to_shot_header_and_bytes(
    img: CapturedImage,
    desktop: pcshare_proto::DesktopStatus,
) -> (ShotHeader, Vec<u8>) {
    let hdr = ShotHeader {
        format: "jpeg".to_string(),
        byte_len: img.jpeg_bytes.len() as u32,
        transform: ShotTransform {
            origin_x: img.origin_x,
            origin_y: img.origin_y,
            source_width: img.source_width,
            source_height: img.source_height,
            image_width: img.image_width,
            image_height: img.image_height,
            dpi_scale_percent: img.dpi_scale_percent,
            target_hwnd: img.target_hwnd.map(|h| h as u64),
            monitor_index: img.monitor_index,
        },
        captured_at_ms: Utc::now().timestamp_millis() as u64,
        desktop,
    };
    (hdr, img.jpeg_bytes)
}

pub async fn handle_shot_monitors(
    platform: &Arc<dyn Platform>,
    perms: &Permissions,
) -> Result<ShotMonitorsReply, RpcError> {
    if !perms.screenshot {
        return Err(RpcError::new(
            ErrorCode::Forbidden,
            "对方没有开放“看屏幕截图”权限",
        ));
    }
    let plat = platform.clone();
    tokio::task::spawn_blocking(move || {
        let monitors = plat
            .list_monitors()
            .map_err(|e| map_platform_to_rpc(&e))?
            .into_iter()
            .map(|m| MonitorInfo {
                index: m.index,
                name: m.name,
                origin_x: m.origin_x,
                origin_y: m.origin_y,
                width: m.width,
                height: m.height,
                dpi_scale_percent: m.dpi_scale_percent,
                is_primary: m.is_primary,
            })
            .collect();
        let windows = plat
            .list_capturable_windows()
            .into_iter()
            .map(|w| CapturableWindow {
                hwnd: w.hwnd as u64,
                pid: w.pid,
                title: w.title,
                width: w.width,
                height: w.height,
            })
            .collect();
        let desktop = to_proto_desktop(plat.desktop_environment());
        Ok(ShotMonitorsReply {
            monitors,
            windows,
            desktop,
        })
    })
    .await
    .map_err(|e| RpcError::new(ErrorCode::Internal, format!("枚举显示器异常：{e}")))?
}

pub async fn handle_shot_screen_stream(
    platform: &Arc<dyn Platform>,
    perms: &Permissions,
    args: ShotScreenArgs,
    send: &mut SendStream,
) -> Result<(), CoreError> {
    if !perms.screenshot {
        write_frame(
            send,
            &ResponseEnvelope::<ShotHeader>::err(
                ErrorCode::Forbidden,
                "对方没有开放“看屏幕截图”权限",
            ),
        )
        .await?;
        return Ok(());
    }
    let plat = platform.clone();
    let quality = args.quality.unwrap_or(80);
    let res = tokio::task::spawn_blocking(move || {
        let desktop = to_proto_desktop(plat.desktop_environment());
        let img = plat
            .capture_screen_jpeg(args.monitor_index, args.max_width, quality)
            .map_err(|e| map_platform_to_rpc(&e))?;
        Ok::<_, RpcError>(to_shot_header_and_bytes(img, desktop))
    })
    .await
    .map_err(|e| CoreError::Internal(format!("截图任务异常：{e}")))?;

    match res {
        Ok((hdr, bytes)) => {
            write_frame(send, &ResponseEnvelope::ok(hdr)).await?;
            send.write_all(&bytes)
                .await
                .map_err(|e| CoreError::NotConnected(format!("发送截图数据中断：{e}")))?;
        }
        Err(err) => {
            write_frame(send, &ResponseEnvelope::<ShotHeader>::Err(err)).await?;
        }
    }
    Ok(())
}

pub async fn handle_shot_window_stream(
    platform: &Arc<dyn Platform>,
    perms: &Permissions,
    args: ShotWindowArgs,
    send: &mut SendStream,
) -> Result<(), CoreError> {
    if !perms.screenshot {
        write_frame(
            send,
            &ResponseEnvelope::<ShotHeader>::err(
                ErrorCode::Forbidden,
                "对方没有开放“看屏幕截图”权限",
            ),
        )
        .await?;
        return Ok(());
    }
    let plat = platform.clone();
    let quality = args.quality.unwrap_or(80);
    let res = tokio::task::spawn_blocking(move || {
        let desktop = to_proto_desktop(plat.desktop_environment());
        let img = plat
            .capture_window_jpeg(args.hwnd as usize, args.max_width, quality)
            .map_err(|e| map_platform_to_rpc(&e))?;
        Ok::<_, RpcError>(to_shot_header_and_bytes(img, desktop))
    })
    .await
    .map_err(|e| CoreError::Internal(format!("窗口截图任务异常：{e}")))?;

    match res {
        Ok((hdr, bytes)) => {
            write_frame(send, &ResponseEnvelope::ok(hdr)).await?;
            send.write_all(&bytes)
                .await
                .map_err(|e| CoreError::NotConnected(format!("发送窗口截图中断：{e}")))?;
        }
        Err(err) => {
            write_frame(send, &ResponseEnvelope::<ShotHeader>::Err(err)).await?;
        }
    }
    Ok(())
}

async fn maybe_capture_refresh(
    platform: &Arc<dyn Platform>,
    perms: &Permissions,
    spec: Option<ShotRefreshSpec>,
    uipi_blocked: bool,
) -> Option<(ShotHeader, Vec<u8>)> {
    let spec = spec?;
    if uipi_blocked || !perms.screenshot {
        return None;
    }
    // Brief settle delay so target window paints the click/key feedback
    tokio::time::sleep(Duration::from_millis(120)).await;
    let plat = platform.clone();
    let q = spec.quality.unwrap_or(80);
    tokio::task::spawn_blocking(move || {
        let desktop = to_proto_desktop(plat.desktop_environment());
        let img = if let Some(hwnd) = spec.hwnd {
            plat.capture_window_jpeg(hwnd as usize, spec.max_width, q).ok()?
        } else {
            plat.capture_screen_jpeg(spec.monitor_index, spec.max_width, q).ok()?
        };
        Some(to_shot_header_and_bytes(img, desktop))
    })
    .await
    .ok()
    .flatten()
}

pub async fn handle_input_click_stream(
    platform: &Arc<dyn Platform>,
    perms: &Permissions,
    args: InputClickArgs,
    send: &mut SendStream,
) -> Result<(), CoreError> {
    if !perms.input_control {
        write_frame(
            send,
            &ResponseEnvelope::<InputReply>::err(
                ErrorCode::Forbidden,
                "对方没有开放“在截图上点按操作”权限",
            ),
        )
        .await?;
        return Ok(());
    }
    let btn = match args.button {
        MouseBtn::Left => PlatformMouseBtn::Left,
        MouseBtn::Right => PlatformMouseBtn::Right,
        MouseBtn::Middle => PlatformMouseBtn::Middle,
    };
    let plat = platform.clone();
    let target_hwnd = args.target_hwnd.map(|h| h as usize);
    let outcome = tokio::task::spawn_blocking(move || {
        plat.send_mouse_click(args.screen_x, args.screen_y, btn, args.double, target_hwnd)
            .map_err(|e| map_platform_to_rpc(&e))
    })
    .await
    .map_err(|e| CoreError::Internal(format!("鼠标点击任务异常：{e}")))?;

    match outcome {
        Ok(out) => {
            let refreshed =
                maybe_capture_refresh(platform, perms, args.refresh_shot, out.uipi_blocked).await;
            let (shot_hdr, shot_bytes) = match refreshed {
                Some((h, b)) => (Some(h), Some(b)),
                None => (None, None),
            };
            let reply = InputReply {
                uipi_blocked: out.uipi_blocked,
                warning: out.warning,
                refreshed_shot: shot_hdr,
            };
            write_frame(send, &ResponseEnvelope::ok(reply)).await?;
            if let Some(bytes) = shot_bytes {
                send.write_all(&bytes)
                    .await
                    .map_err(|e| CoreError::NotConnected(format!("发送刷新截图中断：{e}")))?;
            }
        }
        Err(err) => {
            write_frame(send, &ResponseEnvelope::<InputReply>::Err(err)).await?;
        }
    }
    Ok(())
}

pub async fn handle_input_key_stream(
    platform: &Arc<dyn Platform>,
    perms: &Permissions,
    args: InputKeyArgs,
    send: &mut SendStream,
) -> Result<(), CoreError> {
    if !perms.input_control {
        write_frame(
            send,
            &ResponseEnvelope::<InputReply>::err(
                ErrorCode::Forbidden,
                "对方没有开放“在截图上点按操作”权限",
            ),
        )
        .await?;
        return Ok(());
    }
    let plat = platform.clone();
    let text = args.text.clone();
    let combo = args.key_combo.clone();
    let target_hwnd = args.target_hwnd.map(|h| h as usize);
    let outcome = tokio::task::spawn_blocking(move || {
        plat.send_key_or_text(text.as_deref(), combo.as_deref(), target_hwnd)
            .map_err(|e| map_platform_to_rpc(&e))
    })
    .await
    .map_err(|e| CoreError::Internal(format!("键盘输入任务异常：{e}")))?;

    match outcome {
        Ok(out) => {
            let refreshed =
                maybe_capture_refresh(platform, perms, args.refresh_shot, out.uipi_blocked).await;
            let (shot_hdr, shot_bytes) = match refreshed {
                Some((h, b)) => (Some(h), Some(b)),
                None => (None, None),
            };
            let reply = InputReply {
                uipi_blocked: out.uipi_blocked,
                warning: out.warning,
                refreshed_shot: shot_hdr,
            };
            write_frame(send, &ResponseEnvelope::ok(reply)).await?;
            if let Some(bytes) = shot_bytes {
                send.write_all(&bytes)
                    .await
                    .map_err(|e| CoreError::NotConnected(format!("发送刷新截图中断：{e}")))?;
            }
        }
        Err(err) => {
            write_frame(send, &ResponseEnvelope::<InputReply>::Err(err)).await?;
        }
    }
    Ok(())
}

impl CoreNode {
    pub async fn remote_shot_monitors(
        self: &Arc<Self>,
        peer_id: &str,
    ) -> Result<ShotMonitorsReply, CoreError> {
        self.call_rpc(peer_id, Command::ShotMonitors).await
    }

    pub async fn local_shot_monitors(&self) -> Result<ShotMonitorsReply, CoreError> {
        let perms = Permissions {
            screenshot: true,
            ..Permissions::default()
        };
        Ok(handle_shot_monitors(&self.platform, &perms).await?)
    }

    pub async fn remote_shot_screen(
        self: &Arc<Self>,
        peer_id: &str,
        args: ShotScreenArgs,
    ) -> Result<CapturedShot, CoreError> {
        self.fetch_shot_command(peer_id, Command::ShotScreen(args))
            .await
    }

    pub async fn remote_shot_window(
        self: &Arc<Self>,
        peer_id: &str,
        args: ShotWindowArgs,
    ) -> Result<CapturedShot, CoreError> {
        self.fetch_shot_command(peer_id, Command::ShotWindow(args))
            .await
    }

    pub async fn local_shot_screen(&self, args: ShotScreenArgs) -> Result<CapturedShot, CoreError> {
        let plat = self.platform.clone();
        let q = args.quality.unwrap_or(80);
        let (header, jpeg_bytes) = tokio::task::spawn_blocking(move || {
            let desktop = to_proto_desktop(plat.desktop_environment());
            let img = plat
                .capture_screen_jpeg(args.monitor_index, args.max_width, q)
                .map_err(|e| map_platform_to_rpc(&e))?;
            Ok::<_, RpcError>(to_shot_header_and_bytes(img, desktop))
        })
        .await
        .map_err(|e| CoreError::Internal(format!("本地截图异常：{e}")))??;
        Ok(CapturedShot { header, jpeg_bytes })
    }

    pub async fn local_shot_window(&self, args: ShotWindowArgs) -> Result<CapturedShot, CoreError> {
        let plat = self.platform.clone();
        let q = args.quality.unwrap_or(80);
        let (header, jpeg_bytes) = tokio::task::spawn_blocking(move || {
            let desktop = to_proto_desktop(plat.desktop_environment());
            let img = plat
                .capture_window_jpeg(args.hwnd as usize, args.max_width, q)
                .map_err(|e| map_platform_to_rpc(&e))?;
            Ok::<_, RpcError>(to_shot_header_and_bytes(img, desktop))
        })
        .await
        .map_err(|e| CoreError::Internal(format!("本地窗口截图异常：{e}")))??;
        Ok(CapturedShot { header, jpeg_bytes })
    }

    async fn fetch_shot_command(
        self: &Arc<Self>,
        peer_id: &str,
        cmd: Command,
    ) -> Result<CapturedShot, CoreError> {
        let conn = self.get_or_connect(peer_id).await?;
        let env = cmd.to_envelope()?;
        timeout(SHOT_RPC_TIMEOUT, async {
            let (mut send, mut recv) = conn
                .open_bi()
                .await
                .map_err(|e| CoreError::NotConnected(format!("打开截图流失败：{e}")))?;
            write_frame(&mut send, &env).await?;
            send.finish()
                .map_err(|e| CoreError::Internal(format!("结束截图请求流失败：{e}")))?;
            let resp: ResponseEnvelope<ShotHeader> = read_frame(&mut recv).await?;
            let header = resp.into_result()?;
            let mut jpeg_bytes = vec![0u8; header.byte_len as usize];
            recv.read_exact(&mut jpeg_bytes)
                .await
                .map_err(|e| CoreError::NotConnected(format!("读取截图图像字节中断：{e}")))?;
            Ok(CapturedShot { header, jpeg_bytes })
        })
        .await
        .map_err(|_| CoreError::Timeout)?
    }

    pub async fn remote_input_click(
        self: &Arc<Self>,
        peer_id: &str,
        args: InputClickArgs,
    ) -> Result<InputResult, CoreError> {
        self.send_input_command(peer_id, Command::InputClick(args))
            .await
    }

    pub async fn remote_input_key(
        self: &Arc<Self>,
        peer_id: &str,
        args: InputKeyArgs,
    ) -> Result<InputResult, CoreError> {
        self.send_input_command(peer_id, Command::InputKey(args))
            .await
    }

    async fn send_input_command(
        self: &Arc<Self>,
        peer_id: &str,
        cmd: Command,
    ) -> Result<InputResult, CoreError> {
        let conn = self.get_or_connect(peer_id).await?;
        let env = cmd.to_envelope()?;
        timeout(SHOT_RPC_TIMEOUT, async {
            let (mut send, mut recv) = conn
                .open_bi()
                .await
                .map_err(|e| CoreError::NotConnected(format!("打开远程输入流失败：{e}")))?;
            write_frame(&mut send, &env).await?;
            send.finish()
                .map_err(|e| CoreError::Internal(format!("结束输入请求流失败：{e}")))?;
            let resp: ResponseEnvelope<InputReply> = read_frame(&mut recv).await?;
            let reply = resp.into_result()?;
            let refreshed_jpeg = if let Some(ref hdr) = reply.refreshed_shot {
                let mut buf = vec![0u8; hdr.byte_len as usize];
                recv.read_exact(&mut buf)
                    .await
                    .map_err(|e| CoreError::NotConnected(format!("读取更新后截图中断：{e}")))?;
                Some(buf)
            } else {
                None
            };
            Ok(InputResult {
                reply,
                refreshed_jpeg,
            })
        })
        .await
        .map_err(|_| CoreError::Timeout)?
    }
}
