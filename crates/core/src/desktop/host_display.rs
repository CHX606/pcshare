//! 被控端：看屏期间按控制端的请求把一块显示器临时切到它的分辨率，流关闭时还原。
use crate::{CoreError, CoreNode};
use iroh::endpoint::{RecvStream, SendStream};
use pcshare_platform::desktop::display::{fit_monitor, restore_display};
use pcshare_proto::desktop::{DisplayFit, DisplayFitted};
use pcshare_proto::{PermissionScope, ResponseEnvelope, read_optional_frame, write_frame};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 正在被调整的显示器序号 → 发起调整的设备身份码。同一块屏幕同时只接受一个调整。
static HELD: Mutex<Option<HashMap<u32, String>>> = Mutex::new(None);

fn held() -> std::sync::MutexGuard<'static, Option<HashMap<u32, String>>> {
    HELD.lock().unwrap_or_else(|e| e.into_inner())
}

/// 占住一块显示器；离开作用域时还原分辨率并释放占用。
struct Lease {
    monitor: u32,
    device: Option<String>,
}

impl Drop for Lease {
    fn drop(&mut self) {
        if let Some(device) = self.device.take()
            && let Err(e) = restore_display(&device)
        {
            tracing::warn!("还原显示器 {device} 的分辨率失败：{e}");
        }
        if let Some(map) = held().as_mut() {
            map.remove(&self.monitor);
        }
    }
}

/// 占用显示器。同一设备刚关掉的旧会话可能还没来得及还原，最多等 3 秒。
async fn acquire(peer: &str, monitor: u32) -> Option<Lease> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        {
            let mut guard = held();
            let map = guard.get_or_insert_with(HashMap::new);
            match map.get(&monitor) {
                None => {
                    map.insert(monitor, peer.to_string());
                    return Some(Lease { monitor, device: None });
                }
                Some(owner) if owner != peer || Instant::now() >= deadline => return None,
                Some(_) => {}
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

pub(super) async fn serve(
    node: &Arc<CoreNode>,
    peer: &str,
    req: DisplayFit,
    send: &mut SendStream,
    recv: &mut RecvStream,
) -> Result<(), CoreError> {
    req.validate()?;
    node.desktop_unlocked()?;
    let name = node.storage.get_peer(peer).map(|p| p.name).unwrap_or_default();
    let Some(mut lease) = acquire(peer, req.monitor).await else {
        let busy = DisplayFitted {
            applied: false,
            width: 0,
            height: 0,
            previous_width: 0,
            previous_height: 0,
            note: Some("另一台设备正在调整这块屏幕的分辨率，保持原样".into()),
        };
        write_frame(send, &ResponseEnvelope::ok(busy)).await?;
        return hold(node, peer, recv).await;
    };
    let outcome = tokio::task::spawn_blocking(move || fit_monitor(req.monitor, req.width, req.height))
        .await
        .map_err(|e| CoreError::Internal(e.to_string()))??;
    let previous = outcome.previous;
    let applied = outcome.applied;
    if applied.is_some() {
        lease.device = Some(outcome.device.clone());
        // 显示器切换模式要一点时间，稳定后再回话，免得首帧截到黑屏
        tokio::time::sleep(Duration::from_millis(800)).await;
    }
    let shown = applied.unwrap_or(previous);
    let detail = match applied {
        Some(m) => format!("{}×{} → {}×{}", previous.width, previous.height, m.width, m.height),
        None => outcome.note.clone().unwrap_or_else(|| "分辨率保持不变".into()),
    };
    node.record_audit(peer, &name, pcshare_proto::desktop::DISPLAY, &detail, true);
    let reply = DisplayFitted {
        applied: applied.is_some(),
        width: shown.width,
        height: shown.height,
        previous_width: previous.width,
        previous_height: previous.height,
        note: outcome.note,
    };
    write_frame(send, &ResponseEnvelope::ok(reply)).await?;
    let result = hold(node, peer, recv).await;
    // 还原分辨率会阻塞到显示器切换完成，放到阻塞线程里做；中途被取消时由 Drop 兜底
    let _ = tokio::task::spawn_blocking(move || drop(lease)).await;
    result
}

/// 保持到控制端关闭这条流或连接断开；权限被收回、暂停被控时提前结束。锁屏不算结束。
async fn hold(node: &CoreNode, peer: &str, recv: &mut RecvStream) -> Result<(), CoreError> {
    let watch = async {
        loop {
            node.desktop_access(peer, PermissionScope::InputControl)?;
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    };
    tokio::select! {
        result = read_optional_frame::<_, u8>(recv) => result.map(|_| ()).or(Ok(())),
        result = watch => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 只测占用规则，不真正改分辨率（没有切换过的占用释放时不会调用系统接口）
    #[tokio::test]
    async fn one_adjuster_per_monitor_and_same_peer_waits_for_release() {
        let first = acquire("peer-a", 9001).await.expect("空闲时能占用");
        assert!(acquire("peer-b", 9001).await.is_none(), "别的设备不能抢");
        assert!(acquire("peer-b", 9002).await.is_some_and(|l| l.monitor == 9002));
        let release = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            drop(first);
        });
        assert!(acquire("peer-a", 9001).await.is_some(), "同一设备重开会话时等旧占用释放");
        release.await.unwrap();
    }
}
