use super::{host_input, host_video};
use crate::{CoreError, CoreNode, error::map_platform_to_rpc};
use iroh::endpoint::{Connection, RecvStream, SendStream};
use pcshare_proto::desktop::{CONTROL, VIDEO, VideoOpen};
use pcshare_proto::{
    ErrorCode, PROTOCOL_VERSION, PermissionScope, RequestEnvelope, ResponseEnvelope, RpcError,
    write_frame,
};
use std::{sync::Arc, time::Duration};

impl CoreNode {
    pub(crate) async fn serve_desktop(
        self: &Arc<Self>,
        peer: &str,
        conn: &Connection,
        env: RequestEnvelope,
        mut send: SendStream,
        mut recv: RecvStream,
    ) -> Result<(), CoreError> {
        let scope = if env.cmd == CONTROL {
            PermissionScope::InputControl
        } else {
            PermissionScope::Screenshot
        };
        let auth = self.desktop_access(peer, scope);
        let name = self
            .storage
            .get_peer(peer)
            .map(|p| p.name)
            .unwrap_or_default();
        self.record_audit(peer, &name, &env.cmd, "打开桌面会话", auth.is_ok());
        let result = match auth {
            Err(error) => Err(error.into()),
            Ok(()) if env.v != PROTOCOL_VERSION => {
                Err(RpcError::new(ErrorCode::Unsupported, "桌面协议版本不匹配").into())
            }
            Ok(()) => {
                self.desktop_channel(peer, conn, env, &mut send, &mut recv)
                    .await
            }
        };
        if let Err(error) = result {
            send_error(&mut send, error).await?;
        }
        send.finish()
            .map_err(|e| CoreError::NotConnected(e.to_string()))
    }

    async fn desktop_channel(
        self: &Arc<Self>,
        peer: &str,
        conn: &Connection,
        env: RequestEnvelope,
        send: &mut SendStream,
        recv: &mut RecvStream,
    ) -> Result<(), CoreError> {
        if env.cmd == VIDEO {
            let req: VideoOpen = parse(env.args)?;
            return host_video::serve(self, peer, conn, req, send, recv).await;
        }
        host_input::serve(self, peer, parse(env.args)?, send, recv).await
    }

    pub(super) fn desktop_access(
        &self,
        peer: &str,
        scope: PermissionScope,
    ) -> Result<(), RpcError> {
        let rec = self
            .storage
            .get_peer(peer)
            .filter(|p| p.allow_incoming)
            .ok_or_else(|| RpcError::new(ErrorCode::Unauthorized, "设备配对已解除"))?;
        if self.is_host_paused() {
            return Err(RpcError::new(ErrorCode::Busy, "对方已暂停被控"));
        }
        if !rec.permissions.screenshot || !rec.permissions.allows(scope) {
            return Err(RpcError::new(
                ErrorCode::Forbidden,
                "对方未授予桌面会话所需权限",
            ));
        }
        Ok(())
    }

    pub(super) fn desktop_unlocked(&self) -> Result<(), RpcError> {
        let desktop = self.platform.desktop_environment();
        if desktop.session_id == 0
            || !desktop
                .input_desktop
                .as_deref()
                .is_some_and(|s| s.eq_ignore_ascii_case("Default"))
        {
            return Err(RpcError::new(
                ErrorCode::Forbidden,
                "锁屏、权限确认或系统服务桌面不可远程操作",
            ));
        }
        Ok(())
    }

    pub(super) async fn watch_desktop_access(
        &self,
        peer: &str,
        scope: PermissionScope,
    ) -> Result<(), CoreError> {
        loop {
            self.desktop_access(peer, scope)?;
            self.desktop_unlocked()?;
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
}

fn parse<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> Result<T, CoreError> {
    serde_json::from_value(value).map_err(|e| super::invalid(&format!("桌面请求无效：{e}")))
}

async fn send_error(send: &mut SendStream, error: CoreError) -> Result<(), CoreError> {
    let rpc = match error {
        CoreError::Rpc(e) => e,
        CoreError::Platform(e) => map_platform_to_rpc(&e),
        other => RpcError::new(ErrorCode::Io, other.to_user_message()),
    };
    let error_frame = ResponseEnvelope::<()>::Err(rpc);
    let report = write_frame(send, &error_frame);
    tokio::time::timeout(Duration::from_millis(300), report)
        .await
        .map_err(|_| CoreError::Timeout)??;
    Ok(())
}
