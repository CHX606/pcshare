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

const TERM_CMD_CHANNEL_CAP: usize = 128;
const TERM_OUT_CHANNEL_CAP: usize = 256;
const TERM_CLIENT_FRAME_CHANNEL_CAP: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalStreamEvent {
    Output(Vec<u8>),
    Exited(Option<i32>),
}

pub struct RemoteTerminalHandle {
    pub pid: u32,
    pub shell_name: String,
    cmd_tx: mpsc::Sender<TermClientFrame>,
    pub output_rx: mpsc::Receiver<TerminalStreamEvent>,
}

impl RemoteTerminalHandle {
    pub async fn send_input(&self, bytes: &[u8]) -> Result<(), CoreError> {
        let data_b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
        self.cmd_tx
            .send(TermClientFrame::Input { data_b64 })
            .await
            .map_err(|_| CoreError::NotConnected("远程终端通道已关闭".to_string()))
    }

    pub async fn resize(&self, cols: u16, rows: u16) -> Result<(), CoreError> {
        self.cmd_tx
            .send(TermClientFrame::Resize { cols, rows })
            .await
            .map_err(|_| CoreError::NotConnected("远程终端通道已关闭".to_string()))
    }

    pub async fn close(&self) -> Result<(), CoreError> {
        if self.cmd_tx.try_send(TermClientFrame::Close).is_err() {
            let _ = self.cmd_tx.send(TermClientFrame::Close).await;
        }
        Ok(())
    }
}

enum PtyWriteOp {
    Input(Vec<u8>),
    Resize(u16, u16),
    Close,
}

pub async fn handle_term_open_stream<F>(
    platform: &Arc<dyn Platform>,
    perms: &Permissions,
    args: TermOpenArgs,
    send: &mut SendStream,
    recv: RecvStream,
    on_open_result: F,
) -> Result<bool, CoreError>
where
    F: FnOnce(bool),
{
    if !perms.terminal {
        on_open_result(false);
        write_frame(
            send,
            &ResponseEnvelope::<TermOpenReply>::err(
                ErrorCode::Forbidden,
                "对方没有开放“命令行终端”权限",
            ),
        )
        .await?;
        return Ok(false);
    }

    let prefer_ps = args.shell == TermShell::PowerShell;
    let cols = args.cols;
    let rows = args.rows;
    let cwd_owned = args.cwd.clone();
    let plat_open = platform.clone();
    let open_res = tokio::task::spawn_blocking(move || {
        let cwd_opt = cwd_owned.as_deref().map(Path::new);
        plat_open.open_pty_session(prefer_ps, cols, rows, cwd_opt)
    })
    .await
    .map_err(|e| CoreError::Internal(format!("打开伪终端线程异常：{e}")))?;

    let (pty_box, mut pty_rx) = match open_res {
        Ok(pair) => pair,
        Err(err) => {
            on_open_result(false);
            write_frame(
                send,
                &ResponseEnvelope::<TermOpenReply>::Err(map_platform_to_rpc(&err)),
            )
            .await?;
            return Ok(false);
        }
    };

    let pty: Arc<dyn pcshare_platform::PtySession> = Arc::from(pty_box);
    let reply = TermOpenReply {
        shell_name: pty.shell_name().to_string(),
        pid: pty.pid(),
    };
    write_frame(send, &ResponseEnvelope::ok(reply)).await?;
    on_open_result(true);

    // 将非取消安全的帧读取拆到独立任务，经有界通道送入主 select!，彻底消除半包丢字节错位
    let (client_tx, mut client_rx) =
        mpsc::channel::<Result<Option<TermClientFrame>, pcshare_proto::FrameError>>(
            TERM_CLIENT_FRAME_CHANNEL_CAP,
        );
    let reader_task = tokio::spawn(async move {
        let mut recv = recv;
        loop {
            let res = read_optional_frame::<_, TermClientFrame>(&mut recv).await;
            let stop = !matches!(
                res,
                Ok(Some(
                    TermClientFrame::Input { .. } | TermClientFrame::Resize { .. }
                ))
            );
            if client_tx.send(res).await.is_err() || stop {
                break;
            }
        }
    });

    // 同步 PTY 写入与 resize 移入专用阻塞线程，保序且不阻塞 Tokio 工作线程
    let (pty_op_tx, mut pty_op_rx) = mpsc::channel::<PtyWriteOp>(TERM_CLIENT_FRAME_CHANNEL_CAP);
    let pty_writer = pty.clone();
    let writer_task = tokio::task::spawn_blocking(move || {
        while let Some(op) = pty_op_rx.blocking_recv() {
            match op {
                PtyWriteOp::Input(bytes) => {
                    let _ = pty_writer.write_input(&bytes);
                }
                PtyWriteOp::Resize(c, r) => {
                    let _ = pty_writer.resize(c, r);
                }
                PtyWriteOp::Close => {
                    pty_writer.close();
                    break;
                }
            }
        }
        pty_writer.close();
    });

    let mut exit_ticker = tokio::time::interval(Duration::from_millis(200));
    exit_ticker.tick().await;

    loop {
        tokio::select! {
            _ = send.stopped() => {
                break;
            }
            chunk_opt = pty_rx.recv() => {
                match chunk_opt {
                    Some(bytes) => {
                        let data_b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
                        if write_frame(send, &TermServerFrame::Output { data_b64 }).await.is_err() {
                            break;
                        }
                    }
                    None => {
                        let pty_wait = pty.clone();
                        let code = tokio::task::spawn_blocking(move || pty_wait.try_wait().ok().flatten())
                            .await
                            .ok()
                            .flatten();
                        let _ = write_frame(send, &TermServerFrame::Exited { exit_code: code }).await;
                        break;
                    }
                }
            }
            client_opt = client_rx.recv() => {
                match client_opt {
                    Some(Ok(Some(TermClientFrame::Input { data_b64 }))) => {
                        if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data_b64)
                            && pty_op_tx.send(PtyWriteOp::Input(bytes)).await.is_err()
                        {
                            break;
                        }
                    }
                    Some(Ok(Some(TermClientFrame::Resize { cols, rows }))) => {
                        if pty_op_tx.send(PtyWriteOp::Resize(cols, rows)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Some(TermClientFrame::Close)))
                    | Some(Ok(None))
                    | Some(Err(_))
                    | None => {
                        break;
                    }
                }
            }
            _ = exit_ticker.tick() => {
                let pty_wait = pty.clone();
                let exited = tokio::task::spawn_blocking(move || pty_wait.try_wait().ok().flatten())
                    .await
                    .ok()
                    .flatten();
                if let Some(code) = exited {
                    // Drain any buffered output chunks before sending Exited
                    while let Ok(bytes) = pty_rx.try_recv() {
                        let data_b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
                        let _ = write_frame(send, &TermServerFrame::Output { data_b64 }).await;
                    }
                    let _ = write_frame(send, &TermServerFrame::Exited { exit_code: Some(code) }).await;
                    break;
                }
            }
        }
    }

    reader_task.abort();
    let _ = pty_op_tx.try_send(PtyWriteOp::Close);
    drop(pty_op_tx);
    let pty_close = pty.clone();
    let _ = tokio::task::spawn_blocking(move || pty_close.close()).await;
    let _ = writer_task.await;
    Ok(true)
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

        let (cmd_tx, mut cmd_rx) = mpsc::channel::<TermClientFrame>(TERM_CMD_CHANNEL_CAP);
        let (out_tx, out_rx) = mpsc::channel::<TerminalStreamEvent>(TERM_OUT_CHANNEL_CAP);

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
                            && out_tx
                                .send(TerminalStreamEvent::Output(bytes))
                                .await
                                .is_err()
                        {
                            break;
                        }
                    }
                    TermServerFrame::Exited { exit_code } => {
                        let _ = out_tx.send(TerminalStreamEvent::Exited(exit_code)).await;
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

        let (cmd_tx, mut cmd_rx) = mpsc::channel::<TermClientFrame>(TERM_CMD_CHANNEL_CAP);
        let (out_tx, out_rx) = mpsc::channel::<TerminalStreamEvent>(TERM_OUT_CHANNEL_CAP);

        // 同步 PTY 写入移出 Tokio 工作线程
        tokio::task::spawn_blocking(move || {
            while let Some(frame) = cmd_rx.blocking_recv() {
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
            let mut exit_ticker = tokio::time::interval(Duration::from_millis(200));
            exit_ticker.tick().await;
            let mut exit_code: Option<i32> = None;
            loop {
                tokio::select! {
                    chunk_opt = pty_rx.recv() => {
                        match chunk_opt {
                            Some(bytes) => {
                                if out_tx
                                    .send(TerminalStreamEvent::Output(bytes))
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                            None => {
                                let pty_wait = pty_arc.clone();
                                exit_code = tokio::task::spawn_blocking(move || {
                                    pty_wait.try_wait().ok().flatten()
                                })
                                .await
                                .ok()
                                .flatten();
                                break;
                            }
                        }
                    }
                    _ = exit_ticker.tick() => {
                        let pty_wait = pty_arc.clone();
                        let exited = tokio::task::spawn_blocking(move || {
                            pty_wait.try_wait().ok().flatten()
                        })
                        .await
                        .ok()
                        .flatten();
                        if let Some(code) = exited {
                            while let Ok(bytes) = pty_rx.try_recv() {
                                if out_tx
                                    .send(TerminalStreamEvent::Output(bytes))
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                            exit_code = Some(code);
                            break;
                        }
                    }
                }
            }
            let pty_close = pty_arc.clone();
            let _ = tokio::task::spawn_blocking(move || pty_close.close()).await;
            let _ = out_tx.send(TerminalStreamEvent::Exited(exit_code)).await;
        });

        Ok(RemoteTerminalHandle {
            pid,
            shell_name,
            cmd_tx,
            output_rx: out_rx,
        })
    }
}
