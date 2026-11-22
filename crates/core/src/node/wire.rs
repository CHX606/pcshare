use super::*;

pub(crate) fn build_endpoint_addr(
    target_id: EndpointId,
    relay_url: Option<&str>,
    direct_addrs: &[SocketAddr],
) -> EndpointAddr {
    let mut addr = EndpointAddr::new(target_id);
    if let Some(raw_url) = relay_url
        && let Ok(parsed) = raw_url.parse::<RelayUrl>()
    {
        addr = addr.with_relay_url(parsed);
    }
    for &sock in direct_addrs {
        addr = addr.with_ip_addr(sock);
    }
    addr
}

pub(super) fn inspect_conn_state(conn: &Connection) -> DeviceConnState {
    let paths = conn.paths();
    let selected = paths
        .iter()
        .find(|p| p.is_selected())
        .or_else(|| paths.iter().next());

    if let Some(p) = selected {
        let rtt_ms = p.rtt().as_millis() as u64;
        match p.remote_addr() {
            TransportAddr::Ip(ip) => DeviceConnState::OnlineDirect {
                addr: ip.to_string(),
                rtt_ms,
            },
            TransportAddr::Relay(url) => DeviceConnState::OnlineRelay {
                relay_url: url.to_string(),
                rtt_ms,
            },
            _ => DeviceConnState::OnlineDirect {
                addr: "unknown".to_string(),
                rtt_ms,
            },
        }
    } else {
        DeviceConnState::OnlineDirect {
            addr: "connected".to_string(),
            rtt_ms: 0,
        }
    }
}

pub(super) async fn rpc_once<R: DeserializeOwned>(
    conn: &Connection,
    cmd: &Command,
    limit: Duration,
) -> Result<R, CoreError> {
    let env = cmd.to_envelope()?;
    timeout(limit, async {
        let (mut send, mut recv) = conn
            .open_bi()
            .await
            .map_err(|e| CoreError::NotConnected(format!("和对方的连接已断开：{e}")))?;
        let _ = send.set_priority(8);
        write_frame(&mut send, &env).await?;
        send.finish()
            .map_err(|e| CoreError::Internal(format!("结束请求流失败：{e}")))?;
        let resp: ResponseEnvelope<R> = read_frame(&mut recv).await?;
        Ok(resp.into_result()?)
    })
    .await
    .map_err(|_| CoreError::Timeout)?
}

pub(super) async fn write_rpc_result<T: serde::Serialize>(
    send: &mut SendStream,
    res: Result<T, RpcError>,
) -> Result<(), CoreError> {
    write_rpc_result_opts(send, res, false).await
}

pub(super) async fn write_rpc_result_opts<T: serde::Serialize>(
    send: &mut SendStream,
    res: Result<T, RpcError>,
    compress: bool,
) -> Result<(), CoreError> {
    let env = match res {
        Ok(data) => ResponseEnvelope::Ok(data),
        Err(err) => ResponseEnvelope::Err(err),
    };
    let _ = write_frame_maybe_compressed(send, &env, compress).await?;
    Ok(())
}

pub fn node_capabilities() -> Vec<String> {
    let mut caps = vec![
        "sys.v1".to_string(),
        "fs.v1".to_string(),
        "fs.v2".to_string(),
        "transfer.v1".to_string(),
        "transfer.v2".to_string(),
        CAP_TRANSFER_STRIPED.to_string(),
        CAP_FRAME_DEFLATE.to_string(),
        "app.v1".to_string(),
        "app.v2".to_string(),
        "proc.v1".to_string(),
        "events.v1".to_string(),
        "events.v2".to_string(),
        "search.v1".to_string(),
        "shot.v1".to_string(),
        "input.v1".to_string(),
        "power.v1".to_string(),
        "clip.v1".to_string(),
        "term.v1".to_string(),
    ];
    if cfg!(windows) {
        caps.push("desktop.v1".into());
        caps.push(pcshare_proto::desktop::CAP_DISPLAY_FIT.into());
    }
    caps
}

pub(super) fn observed_addrs(conn: &Connection) -> Vec<SocketAddr> {
    conn.paths()
        .iter()
        .filter_map(|p| match p.remote_addr() {
            TransportAddr::Ip(ip) => Some(*ip),
            _ => None,
        })
        .collect()
}
