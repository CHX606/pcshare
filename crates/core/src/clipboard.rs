//! 剪贴板互通服务（任务 3.7：手动收发文字与 PNG 图片，以及可选自动同步）。

use std::sync::Arc;
use std::time::Duration;

use iroh::endpoint::{RecvStream, SendStream};
use pcshare_platform::{ClipboardContent, Platform};
use pcshare_proto::{
    ClipHeader, ClipKind, ClipSetArgs, ClipSetReply, Command, ErrorCode, Permissions,
    ResponseEnvelope, RpcError, read_frame, write_frame,
};
use tokio::time::timeout;

use crate::CoreNode;
use crate::error::{CoreError, map_platform_to_rpc};
use crate::events::CoreEvent;

const CLIP_RPC_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_CLIP_PNG_BYTES: u32 = 32 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct RemoteClipboardData {
    pub header: ClipHeader,
    pub png_bytes: Vec<u8>,
}

pub async fn handle_clip_get_stream(
    platform: &Arc<dyn Platform>,
    perms: &Permissions,
    send: &mut SendStream,
) -> Result<bool, CoreError> {
    if !perms.clipboard {
        write_frame(
            send,
            &ResponseEnvelope::<ClipHeader>::err(ErrorCode::Forbidden, "对方没有开放剪贴板互通权限"),
        )
        .await?;
        return Ok(false);
    }
    let plat = platform.clone();
    let res = tokio::task::spawn_blocking(move || {
        plat.get_clipboard().map_err(|e| map_platform_to_rpc(&e))
    })
    .await
    .map_err(|e| CoreError::Internal(format!("读取剪贴板异常：{e}")))?;

    match res {
        Ok(data) => {
            match data.content {
                ClipboardContent::Empty => {
                    let hdr = ClipHeader {
                        kind: ClipKind::Empty,
                        text: None,
                        image_width: None,
                        image_height: None,
                        png_byte_len: 0,
                        sequence: data.sequence,
                    };
                    write_frame(send, &ResponseEnvelope::ok(hdr)).await?;
                }
                ClipboardContent::Text(s) => {
                    let hdr = ClipHeader {
                        kind: ClipKind::Text,
                        text: Some(s),
                        image_width: None,
                        image_height: None,
                        png_byte_len: 0,
                        sequence: data.sequence,
                    };
                    write_frame(send, &ResponseEnvelope::ok(hdr)).await?;
                }
                ClipboardContent::ImagePng {
                    png_bytes,
                    width,
                    height,
                } => {
                    let hdr = ClipHeader {
                        kind: ClipKind::ImagePng,
                        text: None,
                        image_width: Some(width),
                        image_height: Some(height),
                        png_byte_len: png_bytes.len() as u32,
                        sequence: data.sequence,
                    };
                    write_frame(send, &ResponseEnvelope::ok(hdr)).await?;
                    send.write_all(&png_bytes)
                        .await
                        .map_err(|e| CoreError::NotConnected(format!("发送剪贴板图片中断：{e}")))?;
                }
            }
            Ok(true)
        }
        Err(err) => {
            write_frame(send, &ResponseEnvelope::<ClipHeader>::Err(err)).await?;
            Ok(false)
        }
    }
}

pub async fn handle_clip_set_stream<F>(
    platform: &Arc<dyn Platform>,
    perms: &Permissions,
    args: ClipSetArgs,
    recv: &mut RecvStream,
    send: &mut SendStream,
    on_applied_seq: F,
) -> Result<bool, CoreError>
where
    F: FnOnce(u32),
{
    if !perms.clipboard {
        write_frame(
            send,
            &ResponseEnvelope::<ClipSetReply>::err(
                ErrorCode::Forbidden,
                "对方没有开放剪贴板互通权限",
            ),
        )
        .await?;
        return Ok(false);
    }
    if args.png_byte_len > MAX_CLIP_PNG_BYTES {
        write_frame(
            send,
            &ResponseEnvelope::<ClipSetReply>::err(
                ErrorCode::Invalid,
                "剪贴板图片超过 32 MB 限制",
            ),
        )
        .await?;
        return Ok(false);
    }

    let png_buf = if args.kind == ClipKind::ImagePng && args.png_byte_len > 0 {
        let mut buf = vec![0u8; args.png_byte_len as usize];
        recv.read_exact(&mut buf)
            .await
            .map_err(|e| CoreError::NotConnected(format!("接收剪贴板图片中断：{e}")))?;
        buf
    } else {
        Vec::new()
    };

    let plat = platform.clone();
    let res = tokio::task::spawn_blocking(move || match args.kind {
        ClipKind::Empty => plat
            .set_clipboard_text("")
            .map_err(|e| map_platform_to_rpc(&e)),
        ClipKind::Text => plat
            .set_clipboard_text(args.text.as_deref().unwrap_or_default())
            .map_err(|e| map_platform_to_rpc(&e)),
        ClipKind::ImagePng => plat
            .set_clipboard_png(&png_buf)
            .map_err(|e| map_platform_to_rpc(&e)),
    })
    .await
    .map_err(|e| CoreError::Internal(format!("写入剪贴板异常：{e}")))?;

    match res {
        Ok(sequence) => {
            // 写入本机剪贴板成功后立即记录序号，防止自动同步轮询把刚收到的剪贴板再回推形成死循环
            on_applied_seq(sequence);
            write_frame(send, &ResponseEnvelope::ok(ClipSetReply { sequence })).await?;
            Ok(true)
        }
        Err(err) => {
            write_frame(send, &ResponseEnvelope::<ClipSetReply>::Err(err)).await?;
            Ok(false)
        }
    }
}

impl CoreNode {
    pub async fn remote_get_clipboard(
        self: &Arc<Self>,
        peer_id: &str,
    ) -> Result<RemoteClipboardData, CoreError> {
        let conn = self.get_or_connect(peer_id).await?;
        let env = Command::ClipGet.to_envelope()?;
        timeout(CLIP_RPC_TIMEOUT, async {
            let (mut send, mut recv) = conn
                .open_bi()
                .await
                .map_err(|e| CoreError::NotConnected(format!("打开剪贴板读取流失败：{e}")))?;
            write_frame(&mut send, &env).await?;
            send.finish()
                .map_err(|e| CoreError::Internal(format!("结束剪贴板请求流失败：{e}")))?;
            let resp: ResponseEnvelope<ClipHeader> = read_frame(&mut recv).await?;
            let header = resp.into_result()?;
            let mut png_bytes = Vec::new();
            if header.kind == ClipKind::ImagePng && header.png_byte_len > 0 {
                png_bytes.resize(header.png_byte_len as usize, 0);
                recv.read_exact(&mut png_bytes)
                    .await
                    .map_err(|e| CoreError::NotConnected(format!("读取剪贴板图像中断：{e}")))?;
            }
            Ok(RemoteClipboardData { header, png_bytes })
        })
        .await
        .map_err(|_| CoreError::Timeout)?
    }

    pub async fn remote_set_clipboard_text(
        self: &Arc<Self>,
        peer_id: &str,
        text: &str,
    ) -> Result<ClipSetReply, CoreError> {
        self.call_rpc(
            peer_id,
            Command::ClipSet(ClipSetArgs {
                kind: ClipKind::Text,
                text: Some(text.to_string()),
                png_byte_len: 0,
            }),
        )
        .await
    }

    pub async fn remote_set_clipboard_png(
        self: &Arc<Self>,
        peer_id: &str,
        png_bytes: &[u8],
    ) -> Result<ClipSetReply, CoreError> {
        let conn = self.get_or_connect(peer_id).await?;
        let cmd = Command::ClipSet(ClipSetArgs {
            kind: ClipKind::ImagePng,
            text: None,
            png_byte_len: png_bytes.len() as u32,
        });
        let env = cmd.to_envelope()?;
        timeout(CLIP_RPC_TIMEOUT, async {
            let (mut send, mut recv) = conn
                .open_bi()
                .await
                .map_err(|e| CoreError::NotConnected(format!("打开剪贴板写入流失败：{e}")))?;
            write_frame(&mut send, &env).await?;
            send.write_all(png_bytes)
                .await
                .map_err(|e| CoreError::NotConnected(format!("发送剪贴板图片中断：{e}")))?;
            send.finish()
                .map_err(|e| CoreError::Internal(format!("结束剪贴板发送流失败：{e}")))?;
            let resp: ResponseEnvelope<ClipSetReply> = read_frame(&mut recv).await?;
            Ok(resp.into_result()?)
        })
        .await
        .map_err(|_| CoreError::Timeout)?
    }

    /// 把本机当前剪贴板（文字或图片）发送到指定远端设备。
    pub async fn push_local_clipboard_to_peer(
        self: &Arc<Self>,
        peer_id: &str,
    ) -> Result<ClipKind, CoreError> {
        let plat = self.platform.clone();
        let local = tokio::task::spawn_blocking(move || {
            plat.get_clipboard().map_err(|e| map_platform_to_rpc(&e))
        })
        .await
        .map_err(|e| CoreError::Internal(format!("读取本机剪贴板异常：{e}")))??;

        let kind = match local.content {
            ClipboardContent::Empty => {
                return Err(CoreError::InvalidArgument(
                    "本机剪贴板当前为空，没有可发送的文字或图片".to_string(),
                ));
            }
            ClipboardContent::Text(s) => {
                self.remote_set_clipboard_text(peer_id, &s).await?;
                ClipKind::Text
            }
            ClipboardContent::ImagePng { png_bytes, .. } => {
                self.remote_set_clipboard_png(peer_id, &png_bytes).await?;
                ClipKind::ImagePng
            }
        };
        let _ = self.events_tx.send(CoreEvent::ClipboardSynced {
            peer_id: peer_id.to_string(),
            kind,
        });
        Ok(kind)
    }

    /// 把指定远端设备的剪贴板取回到本机剪贴板。
    pub async fn pull_peer_clipboard_to_local(
        self: &Arc<Self>,
        peer_id: &str,
    ) -> Result<RemoteClipboardData, CoreError> {
        let remote = self.remote_get_clipboard(peer_id).await?;
        let plat = self.platform.clone();
        let kind = remote.header.kind;
        let text_clone = remote.header.text.clone();
        let png_clone = remote.png_bytes.clone();

        let applied_seq = tokio::task::spawn_blocking(move || match kind {
            ClipKind::Empty => Err(RpcError::new(
                ErrorCode::Invalid,
                "对方电脑剪贴板当前为空",
            )),
            ClipKind::Text => plat
                .set_clipboard_text(text_clone.as_deref().unwrap_or_default())
                .map_err(|e| map_platform_to_rpc(&e)),
            ClipKind::ImagePng => plat
                .set_clipboard_png(&png_clone)
                .map_err(|e| map_platform_to_rpc(&e)),
        })
        .await
        .map_err(|e| CoreError::Internal(format!("写入本机剪贴板异常：{e}")))??;

        self.record_applied_clipboard_seq(applied_seq);
        let _ = self.events_tx.send(CoreEvent::ClipboardSynced {
            peer_id: peer_id.to_string(),
            kind,
        });
        Ok(remote)
    }
}
