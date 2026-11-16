//! 交互式伪终端通道服务（任务 3.6：ConPTY 双向流与尺寸同步）。

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use iroh::endpoint::{RecvStream, SendStream};
use pcshare_platform::Platform;
use pcshare_proto::{
    Command, ErrorCode, Permissions, ResponseEnvelope, TermClientFrame, TermOpenArgs,
    TermOpenReply, TermServerFrame, TermShell, read_frame, read_optional_frame, write_frame,
};
use tokio::sync::mpsc;
use tokio::time::timeout;

use crate::CoreNode;
use crate::error::{CoreError, map_platform_to_rpc};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalStreamEvent {
    Output(Vec<u8>),
    Exited(Option<i32>),
}

pub struct RemoteTerminalHandle {
    pub pid: u32,
    pub shell_name: String,
    cmd_tx: mpsc::UnboundedSender<TermClientFrame>,
    pub output_rx: mpsc::UnboundedReceiver<TerminalStreamEvent>,
}

impl RemoteTerminalHandle {
    pub async fn send_input(&self, bytes: &[u8]) -> Result<(), CoreError> {
        let data_b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
        self.cmd_tx
            .send(TermClientFrame::Input { data_b64 })
            .map_err(|_| CoreError::NotConnected("远程终端通道已关闭".to_string()))
    }

    pub async fn resize(&self, cols: u16, rows: u16) -> Result<(), CoreError> {
        self.cmd_tx
            .send(TermClientFrame::Resize { cols, rows })
            .map_err(|_| CoreError::NotConnected("远程终端通道已关闭".to_string()))
    }

    pub async fn close(&self) -> Result<(), CoreError> {
        let _ = self.cmd_tx.send(TermClientFrame::Close);
        Ok(())
    }
}

pub async fn handle_term_open_stream(
    platform: &Arc<dyn Platform>,
    perms: &Permissions,
    args: TermOpenArgs,
    send: &mut SendStream,
    recv: &mut RecvStream,
) -> Result<(), CoreError> {
    if !perms.terminal {
        write_frame(
            send,
            &ResponseEnvelope::<TermOpenReply>::err(
                ErrorCode::Forbidden,
                "对方没有开放“命令行终端”权限",
            ),
        )
        .await?;
        return Ok(());
    }

    let prefer_ps = args.shell == TermShell::PowerShell;
    let cwd_opt = args.cwd.as_deref().map(Path::new);
    let (pty, mut pty_rx) = match platform.open_pty_session(prefer_ps, args.cols, args.rows, cwd_opt)
    {
        Ok(pair) => pair,
        Err(err) => {
            write_frame(
                send,
                &ResponseEnvelope::<TermOpenReply>::Err(map_platform_to_rpc(&err)),
            )
            .await?;
            return Ok(());
        }
    };

    let reply = TermOpenReply {
        shell_name: pty.shell_name().to_string(),
        pid: pty.pid(),
    };
    write_frame(send, &ResponseEnvelope::ok(reply)).await?;

    let mut exit_ticker = tokio::time::interval(Duration::from_millis(200));
    exit_ticker.tick().await;

    loop {
        tokio::select! {
            _ = send.stopped() => {
                pty.close();
                break;
            }
            chunk_opt = pty_rx.recv() => {
                match chunk_opt {
                    Some(bytes) => {
                        let data_b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
                        if write_frame(send, &TermServerFrame::Output { data_b64 }).await.is_err() {
                            pty.close();
                            break;
                        }
                    }
                    None => {
                        let code = pty.try_wait().ok().flatten();
                        let _ = write_frame(send, &TermServerFrame::Exited { exit_code: code }).await;
                        pty.close();
                        break;
                    }
                }
            }
            client_msg = read_optional_frame::<_, TermClientFrame>(recv) => {
                match client_msg {
                    Ok(Some(TermClientFrame::Input { data_b64 })) => {
                        if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data_b64) {
                            let _ = pty.write_input(&bytes);
                        }
                    }
                    Ok(Some(TermClientFrame::Resize { cols, rows })) => {
                        let _ = pty.resize(cols, rows);
                    }
                    Ok(Some(TermClientFrame::Close)) | Ok(None) | Err(_) => {
                        pty.close();
                        break;
                    }
                }
            }
            _ = exit_ticker.tick() => {
                if let Ok(Some(code)) = pty.try_wait() {
                    // Drain any buffered output chunks before sending Exited
                    while let Ok(bytes) = pty_rx.try_recv() {
                        let data_b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
                        let _ = write_frame(send, &TermServerFrame::Output { data_b64 }).await;
                    }
                    let _ = write_frame(send, &TermServerFrame::Exited { exit_code: Some(code) }).await;
                    pty.close();
                    break;
                }
            }
        }
    }
    pty.close();
    Ok(())
}

impl CoreNode {
    pub async fn open_remote_terminal(
        self: &Arc<Self>,
        peer_id: &str,
        args: TermOpenArgs,
    ) -> Result<RemoteTerminalHandle, CoreError> {
        let conn = self.get_or_connect(peer_id).await?;
        let (mut send, mut recv) = conn
            .open_bi()
            .await
            .map_err(|e| CoreError::NotConnected(format!("打开终端连接流失败：{e}")))?;

        let env = Command::TermOpen(args).to_envelope()?;
        write_frame(&mut send, &env).await?;

        let resp: ResponseEnvelope<TermOpenReply> =
            timeout(Duration::from_secs(15), read_frame(&mut recv))
                .await
                .map_err(|_| CoreError::Timeout)??;
        let open_reply = resp.into_result()?;

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<TermClientFrame>();
        let (out_tx, out_rx) = mpsc::unbounded_channel::<TerminalStreamEvent>();

        tokio::spawn(async move {
            while let Some(frame) = cmd_rx.recv().await {
                let is_close = matches!(frame, TermClientFrame::Close);
                if write_frame(&mut send, &frame).await.is_err() || is_close {
                    let _ = send.finish();
                    break;
                }
            }
        });

        tokio::spawn(async move {
            while let Ok(Some(frame)) = read_optional_frame::<_, TermServerFrame>(&mut recv).await {
                match frame {
                    TermServerFrame::Output { data_b64 } => {
                        if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data_b64)
                            && out_tx.send(TerminalStreamEvent::Output(bytes)).is_err()
                        {
                            break;
                        }
                    }
                    TermServerFrame::Exited { exit_code } => {
                        let _ = out_tx.send(TerminalStreamEvent::Exited(exit_code));
                        break;
                    }
                }
            }
        });

        Ok(RemoteTerminalHandle {
            pid: open_reply.pid,
            shell_name: open_reply.shell_name,
            cmd_tx,
            output_rx: out_rx,
        })
    }

    pub fn open_local_terminal(
        &self,
        args: TermOpenArgs,
    ) -> Result<RemoteTerminalHandle, CoreError> {
        let prefer_ps = args.shell == TermShell::PowerShell;
        let cwd_opt = args.cwd.as_deref().map(Path::new);
        let (pty, mut pty_rx) = self
            .platform
            .open_pty_session(prefer_ps, args.cols, args.rows, cwd_opt)
            .map_err(|e| CoreError::Rpc(map_platform_to_rpc(&e)))?;

        let pid = pty.pid();
        let shell_name = pty.shell_name().to_string();
        let pty_arc: Arc<dyn pcshare_platform::PtySession> = Arc::from(pty);
        let pty_writer = pty_arc.clone();

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<TermClientFrame>();
        let (out_tx, out_rx) = mpsc::unbounded_channel::<TerminalStreamEvent>();

        tokio::spawn(async move {
            while let Some(frame) = cmd_rx.recv().await {
                match frame {
                    TermClientFrame::Input { data_b64 } => {
                        if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data_b64)
                        {
                            let _ = pty_writer.write_input(&bytes);
                        }
                    }
                    TermClientFrame::Resize { cols, rows } => {
                        let _ = pty_writer.resize(cols, rows);
                    }
                    TermClientFrame::Close => {
                        pty_writer.close();
                        break;
                    }
                }
            }
            pty_writer.close();
        });

        tokio::spawn(async move {
            while let Some(bytes) = pty_rx.recv().await {
                if out_tx.send(TerminalStreamEvent::Output(bytes)).is_err() {
                    break;
                }
            }
            let code = pty_arc.try_wait().ok().flatten();
            let _ = out_tx.send(TerminalStreamEvent::Exited(code));
            pty_arc.close();
        });

        Ok(RemoteTerminalHandle {
            pid,
            shell_name,
            cmd_tx,
            output_rx: out_rx,
        })
    }
}
