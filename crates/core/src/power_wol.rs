//! 电源控制（10 秒可取消倒计时，任务 3.5）与网络唤醒（本机直发 + 同网段在线节点代发）。

use std::sync::Arc;
use std::time::Duration;

use pcshare_platform::PlatformPowerAction;
use pcshare_proto::{
    Command, ErrorCode, Permissions, PowerAction, PowerCancelReply, PowerExecArgs, PowerExecReply,
    PowerWakeWolArgs, PowerWakeWolReply, ResponseEnvelope, RpcError, read_frame, write_frame,
};
use serde::{Deserialize, Serialize};
use tokio::task::JoinSet;
use tokio::time::timeout;

use crate::CoreNode;
use crate::error::{CoreError, map_platform_to_rpc};

const MIN_POWER_COUNTDOWN_SECS: u32 = 10;
const MAX_POWER_COUNTDOWN_SECS: u32 = 120;
const WOL_RELAY_TIMEOUT: Duration = Duration::from_secs(3);

struct PowerCancelResetGuard<'a>(&'a CoreNode);

impl Drop for PowerCancelResetGuard<'_> {
    fn drop(&mut self) {
        self.0.set_active_power_cancel(None);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WakePeerReport {
    pub local_packets: usize,
    pub relay_peers_notified: usize,
    pub relayed_packets: usize,
}

fn action_label(action: PowerAction) -> &'static str {
    match action {
        PowerAction::Lock => "锁定屏幕",
        PowerAction::Sleep => "进入睡眠",
        PowerAction::Restart => "重启电脑",
        PowerAction::Shutdown => "关闭电脑",
    }
}

fn to_platform_power(action: PowerAction) -> PlatformPowerAction {
    match action {
        PowerAction::Lock => PlatformPowerAction::Lock,
        PowerAction::Sleep => PlatformPowerAction::Sleep,
        PowerAction::Restart => PlatformPowerAction::Restart,
        PowerAction::Shutdown => PlatformPowerAction::Shutdown,
    }
}

impl CoreNode {
    pub async fn handle_power_exec(
        &self,
        perms: &Permissions,
        args: PowerExecArgs,
    ) -> Result<PowerExecReply, RpcError> {
        if !perms.power {
            return Err(RpcError::new(
                ErrorCode::Forbidden,
                "对方没有开放“电源操作”权限",
            ));
        }

        // 并发电源请求互斥，防止互相覆盖取消句柄
        let _exec_guard = self.power_exec_lock.try_lock().map_err(|_| {
            RpcError::new(
                ErrorCode::Busy,
                "当前已有另一项电源操作正在倒计时中，请先取消或等待完成",
            )
        })?;

        // 非锁屏操作强制至少 10 秒可取消倒计时，忽略远端传入的 0 或过小值
        let countdown_secs = match args.action {
            PowerAction::Lock => 0,
            PowerAction::Sleep | PowerAction::Restart | PowerAction::Shutdown => args
                .countdown_secs
                .unwrap_or(MIN_POWER_COUNTDOWN_SECS)
                .clamp(MIN_POWER_COUNTDOWN_SECS, MAX_POWER_COUNTDOWN_SECS),
        };
        let requester = if args.requester_name.trim().is_empty() {
            "已配对设备".to_string()
        } else {
            args.requester_name.clone()
        };

        if countdown_secs > 0 {
            let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
            self.set_active_power_cancel(Some(cancel_tx));
            let _reset_guard = PowerCancelResetGuard(self);
            let proceed_res = self
                .platform
                .show_power_countdown(
                    action_label(args.action),
                    &requester,
                    countdown_secs,
                    cancel_rx,
                )
                .await;
            drop(_reset_guard);

            let proceed = proceed_res.map_err(|e| map_platform_to_rpc(&e))?;
            if !proceed {
                return Ok(PowerExecReply {
                    action: args.action,
                    cancelled_by_host_user: true,
                    executed: false,
                    message: Some(format!(
                        "远程「{}」已在倒计时期间被取消",
                        action_label(args.action)
                    )),
                });
            }
        }

        if !self.is_dry_run_power_actions() {
            let plat = self.platform.clone();
            let act = to_platform_power(args.action);
            tokio::task::spawn_blocking(move || plat.execute_power_action(act))
                .await
                .map_err(|e| RpcError::new(ErrorCode::Internal, format!("电源任务异常：{e}")))?
                .map_err(|e| map_platform_to_rpc(&e))?;
        }

        Ok(PowerExecReply {
            action: args.action,
            cancelled_by_host_user: false,
            executed: true,
            message: Some(format!("已触发{}", action_label(args.action))),
        })
    }

    pub fn handle_power_cancel(&self, perms: &Permissions) -> Result<PowerCancelReply, RpcError> {
        if !perms.power {
            return Err(RpcError::new(
                ErrorCode::Forbidden,
                "对方没有开放“电源操作”权限",
            ));
        }
        let cancelled = self.trigger_power_cancel();
        Ok(PowerCancelReply { cancelled })
    }

    pub fn handle_power_wake_wol(
        &self,
        args: PowerWakeWolArgs,
    ) -> Result<PowerWakeWolReply, RpcError> {
        let packets_sent = self
            .platform
            .send_wol_packets(&args.mac_addresses)
            .map_err(|e| map_platform_to_rpc(&e))?;
        Ok(PowerWakeWolReply { packets_sent })
    }

    pub async fn remote_power_exec(
        self: &Arc<Self>,
        peer_id: &str,
        args: PowerExecArgs,
    ) -> Result<PowerExecReply, CoreError> {
        let conn = self.get_or_connect(peer_id).await?;
        let eff_secs = match args.action {
            PowerAction::Lock => 0,
            PowerAction::Sleep | PowerAction::Restart | PowerAction::Shutdown => args
                .countdown_secs
                .unwrap_or(MIN_POWER_COUNTDOWN_SECS)
                .clamp(MIN_POWER_COUNTDOWN_SECS, MAX_POWER_COUNTDOWN_SECS),
        };
        let wait_limit = Duration::from_secs(eff_secs as u64 + 20);
        let env = Command::PowerExec(args).to_envelope()?;
        timeout(wait_limit, async {
            let (mut send, mut recv) = conn
                .open_bi()
                .await
                .map_err(|e| CoreError::NotConnected(format!("打开电源控制通道失败：{e}")))?;
            write_frame(&mut send, &env).await?;
            send.finish()
                .map_err(|e| CoreError::Internal(format!("结束电源请求流失败：{e}")))?;
            let resp: ResponseEnvelope<PowerExecReply> = read_frame(&mut recv).await?;
            Ok(resp.into_result()?)
        })
        .await
        .map_err(|_| CoreError::Timeout)?
    }

    pub async fn remote_power_cancel(
        self: &Arc<Self>,
        peer_id: &str,
    ) -> Result<PowerCancelReply, CoreError> {
        self.call_rpc(peer_id, Command::PowerCancel).await
    }

    /// 网络唤醒（任务 3.5）：既在本机广播魔术包，也请当前所有在线的其他已配对设备代发唤醒广播包。
    pub async fn wake_peer(self: &Arc<Self>, peer_id: &str) -> Result<WakePeerReport, CoreError> {
        let peer = self.storage.get_peer(peer_id).ok_or_else(|| {
            CoreError::Rpc(RpcError::new(ErrorCode::NotFound, "找不到该已配对设备"))
        })?;
        if peer.mac_addresses.is_empty() {
            return Err(CoreError::InvalidArgument(format!(
                "尚未记录“{}”的网卡物理地址；请在该电脑在线时至少连接一次后再使用网络唤醒",
                peer.name
            )));
        }
        let macs = peer.mac_addresses.clone();
        let local_packets = self.platform.send_wol_packets(&macs).unwrap_or(0);

        let mut relay_peers_notified = 0usize;
        let mut relayed_packets = 0usize;

        let online_candidates: Vec<String> = self
            .storage
            .list_peers()
            .into_iter()
            .filter(|p| p.endpoint_id != peer_id)
            .filter(|p| {
                self.has_active_conn(&p.endpoint_id)
                    || matches!(
                        self.peer_state(&p.endpoint_id),
                        Some(
                            crate::events::DeviceConnState::OnlineDirect { .. }
                                | crate::events::DeviceConnState::OnlineRelay { .. }
                        )
                    )
            })
            .map(|p| p.endpoint_id)
            .collect();

        // 并发委托所有在线伙伴代发 WoL（短超时），避免单个慢节点串行阻塞 15 秒
        let mut join_set = JoinSet::new();
        for helper_id in online_candidates {
            let node = self.clone();
            let helper_macs = macs.clone();
            join_set.spawn(async move {
                timeout(
                    WOL_RELAY_TIMEOUT,
                    node.call_rpc::<PowerWakeWolReply>(
                        &helper_id,
                        Command::PowerWakeWol(PowerWakeWolArgs {
                            mac_addresses: helper_macs,
                        }),
                    ),
                )
                .await
                .ok()
                .and_then(Result::ok)
            });
        }
        while let Some(joined) = join_set.join_next().await {
            if let Ok(Some(rep)) = joined {
                relay_peers_notified += 1;
                relayed_packets += rep.packets_sent;
            }
        }

        if local_packets == 0 && relayed_packets == 0 {
            return Err(CoreError::Internal(
                "未能成功发出网络唤醒广播包，也没有在线的同网段伙伴代发".to_string(),
            ));
        }

        Ok(WakePeerReport {
            local_packets,
            relay_peers_notified,
            relayed_packets,
        })
    }
}
