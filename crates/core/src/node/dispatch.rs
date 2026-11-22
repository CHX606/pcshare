use super::*;

mod apps;
mod file_transfer;
mod files;
mod media;
mod system;

struct RpcStream<'a> {
    node: &'a Arc<CoreNode>,
    peer: &'a str,
    record: &'a PeerRecord,
    send: &'a mut SendStream,
    recv: Option<RecvStream>,
}
impl RpcStream<'_> {
    fn audit(&self, action: &str, detail: &str, ok: bool) {
        self.node
            .record_audit(self.peer, &self.record.name, action, detail, ok);
    }
    fn supports_deflate(&self) -> bool {
        self.record
            .capabilities
            .iter()
            .any(|c| c == CAP_FRAME_DEFLATE)
    }
}

impl CoreNode {
    pub(super) async fn handle_command_stream(
        self: &Arc<Self>,
        peer_id: &str,
        conn: &Connection,
        mut send: SendStream,
        mut recv: RecvStream,
    ) -> Result<(), CoreError> {
        let Some(peer_rec) = self.storage.get_peer(peer_id) else {
            let err = RpcError::new(ErrorCode::Unauthorized, "设备配对已解除");
            return reject(&mut send, err).await;
        };
        if !peer_rec.allow_incoming {
            let err = RpcError::new(ErrorCode::Unauthorized, "对方已关闭允许此设备入站控制");
            return reject(&mut send, err).await;
        }

        let env: RequestEnvelope = read_frame(&mut recv).await?;
        if matches!(
            env.cmd.as_str(),
            pcshare_proto::desktop::VIDEO | pcshare_proto::desktop::CONTROL
        ) {
            return self.serve_desktop(peer_id, conn, env, send, recv).await;
        }
        let cmd = match self.parse_authorized(env, peer_id, &peer_rec) {
            Ok(c) => c,
            Err(e) => return reject(&mut send, e).await,
        };
        let mut stream = RpcStream {
            node: self,
            peer: peer_id,
            record: &peer_rec,
            send: &mut send,
            recv: Some(recv),
        };
        stream.dispatch_system(cmd).await?;
        let _ = send.finish();
        Ok(())
    }

    fn parse_authorized(
        &self,
        env: RequestEnvelope,
        peer: &str,
        rec: &PeerRecord,
    ) -> Result<Command, RpcError> {
        let cmd = Command::from_envelope(env)?;
        self.authorize_command(peer, rec, &cmd)?;
        Ok(cmd)
    }
    fn authorize_command(
        &self,
        peer: &str,
        rec: &PeerRecord,
        cmd: &Command,
    ) -> Result<(), RpcError> {
        if self.is_host_paused()
            && !matches!(
                cmd,
                Command::SysHello(_) | Command::SysPing(_) | Command::SubEvents
            )
        {
            return Err(RpcError::new(
                ErrorCode::Busy,
                "对方这台电脑当前开启了“暂停被控”，恢复后才能操作",
            ));
        }
        if !rec.permissions.allows(cmd.required_permission()) {
            self.record_audit(peer, &rec.name, cmd.cmd_name(), "权限不足", false);
            return Err(RpcError::new(
                ErrorCode::Forbidden,
                "对方没有授予执行此操作的权限",
            ));
        }
        Ok(())
    }
    pub(crate) fn record_audit(
        &self,
        peer_id: &str,
        peer_name: &str,
        action: &str,
        detail: &str,
        ok: bool,
    ) {
        let _ = self.storage.append_audit(AuditEntry {
            timestamp_ms: Utc::now().timestamp_millis() as u64,
            peer_id: peer_id.to_string(),
            peer_name: peer_name.to_string(),
            action: action.to_string(),
            detail: detail.to_string(),
            ok,
        });
    }
}

async fn reject(send: &mut SendStream, error: RpcError) -> Result<(), CoreError> {
    write_frame(send, &ResponseEnvelope::<()>::Err(error)).await?;
    let _ = send.finish();
    Ok(())
}
