use crate::{CoreError, CoreNode};
use iroh::endpoint::{RecvStream, SendStream};
use pcshare_platform::desktop::{ButtonState, PointerAction, inject, verify_target};
use pcshare_platform::{InputOutcome, Platform, PlatformError, PlatformMouseBtn as Btn};
use pcshare_proto::desktop::{ControlBatch, DesktopInput as Event, Source};
use pcshare_proto::{
    ErrorCode, MouseBtn, PermissionScope, ResponseEnvelope, RpcError, read_optional_frame,
    write_frame,
};
use std::{sync::Arc, time::Duration};

pub(super) async fn serve(
    node: &Arc<CoreNode>,
    peer: &str,
    source: Source,
    send: &mut SendStream,
    recv: &mut RecvStream,
) -> Result<(), CoreError> {
    let _exclusive = node
        .desktop_input_lock
        .try_lock()
        .map_err(|_| RpcError::new(ErrorCode::Busy, "另一桌面会话正在操作此电脑"))?;
    send.set_priority(16)
        .map_err(|e| CoreError::NotConnected(e.to_string()))?;
    node.desktop_unlocked()?;
    write_frame(send, &ResponseEnvelope::ok(())).await?;
    let target = match source {
        Source::Window(h) => Some(h as usize),
        _ => None,
    };
    let mut state = InputState {
        pressed: Vec::new(),
        target,
    };
    loop {
        let batch = tokio::select! {
            result = state.read(recv) => result?,
            result = node.watch_desktop_access(peer, PermissionScope::InputControl) => return result,
        };
        let Some(batch) = batch else { return Ok(()) };
        state.apply_batch(node, peer, source, batch, send).await?;
    }
}

async fn refreshed(
    node: &CoreNode,
    peer: &str,
    source: Source,
    send: &mut SendStream,
) -> Result<(), CoreError> {
    tokio::time::sleep(Duration::from_millis(100)).await;
    node.desktop_access(peer, PermissionScope::InputControl)?;
    node.desktop_unlocked()?;
    let shot = super::local_snapshot(node, source).await?;
    node.desktop_access(peer, PermissionScope::InputControl)?;
    write_frame(send, &ResponseEnvelope::ok(Some(shot.header))).await?;
    send.write_all(&shot.jpeg_bytes)
        .await
        .map_err(|e| CoreError::NotConnected(e.to_string()))
}

struct InputState {
    pressed: Vec<ButtonState>,
    target: Option<usize>,
}

impl InputState {
    async fn read(&self, recv: &mut RecvStream) -> Result<Option<ControlBatch>, CoreError> {
        let incoming = read_optional_frame::<_, ControlBatch>(recv);
        Ok(if self.pressed.is_empty() {
            incoming.await?
        } else {
            tokio::time::timeout(Duration::from_secs(12), incoming)
                .await
                .map_err(|_| CoreError::Timeout)??
        })
    }
    async fn apply_batch(
        &mut self,
        node: &CoreNode,
        peer: &str,
        source: Source,
        batch: ControlBatch,
        send: &mut SendStream,
    ) -> Result<(), CoreError> {
        node.desktop_access(peer, PermissionScope::InputControl)?;
        node.desktop_unlocked()?;
        if batch.events.len() > 64 || batch.events.is_empty() {
            return Err(super::invalid("输入批次须包含 1 至 64 个事件"));
        }
        for event in &batch.events {
            event.validate()?;
        }
        for event in batch.events {
            self.apply(&*node.platform, event)?;
        }
        if batch.refresh {
            refreshed(node, peer, source, send).await?;
        } else {
            write_frame(
                send,
                &ResponseEnvelope::ok(None::<pcshare_proto::ShotHeader>),
            )
            .await?;
        }
        Ok(())
    }

    fn apply(&mut self, platform: &dyn Platform, e: Event) -> Result<(), PlatformError> {
        match e {
            Event::Button { x, y, button, down } => self.change(
                ButtonState::Mouse(btn(button)),
                down,
                PointerAction::Button(x, y, btn(button), down),
            ),
            Event::Key { vk, down } => {
                self.change(ButtonState::Key(vk), down, PointerAction::Key(vk, down))
            }
            Event::Move { x, y } => inject(PointerAction::Move(x, y), self.target),
            Event::Wheel {
                x,
                y,
                delta_x,
                delta_y,
            } => inject(PointerAction::Wheel(x, y, delta_x, delta_y), self.target),
            Event::Click {
                x,
                y,
                button,
                double,
            } => {
                verify_target(self.target, Some((x, y)), true)?;
                accepted(platform.send_mouse_click(x, y, btn(button), double, self.target)?)
            }
            Event::Text { text } => self.text_or_shortcut(platform, Some(&text), None),
            Event::Shortcut { combo } => self.text_or_shortcut(platform, None, Some(&combo)),
            Event::Release => self.release(),
        }
    }

    fn text_or_shortcut(
        &self,
        platform: &dyn Platform,
        text: Option<&str>,
        combo: Option<&str>,
    ) -> Result<(), PlatformError> {
        verify_target(self.target, None, true)?;
        accepted(platform.send_key_or_text(text, combo, self.target)?)
    }

    fn change(
        &mut self,
        b: ButtonState,
        down: bool,
        action: PointerAction,
    ) -> Result<(), PlatformError> {
        if !down && !self.pressed.contains(&b) {
            return Ok(());
        }
        // A release must work even if a drag leaves the original window.
        if down || self.target.is_none() {
            inject(action, self.target)?;
        } else {
            inject(PointerAction::Release(b), None)?;
        }
        if down && !self.pressed.contains(&b) {
            self.pressed.push(b);
        }
        if !down {
            self.pressed.retain(|key| *key != b);
        }
        Ok(())
    }

    fn release(&mut self) -> Result<(), PlatformError> {
        while let Some(&key) = self.pressed.last() {
            inject(PointerAction::Release(key), None)?;
            self.pressed.pop();
        }
        Ok(())
    }
}

impl Drop for InputState {
    fn drop(&mut self) {
        for key in self.pressed.drain(..).rev() {
            if let Err(e) = inject(PointerAction::Release(key), None) {
                tracing::warn!("释放桌面输入失败：{e}");
            }
        }
    }
}

fn btn(b: MouseBtn) -> Btn {
    match b {
        MouseBtn::Left => Btn::Left,
        MouseBtn::Right => Btn::Right,
        MouseBtn::Middle => Btn::Middle,
    }
}

fn accepted(result: InputOutcome) -> Result<(), PlatformError> {
    if result.uipi_blocked {
        return Err(PlatformError::PermissionDenied(
            result
                .warning
                .unwrap_or_else(|| "Windows 拦截了输入".into()),
        ));
    }
    Ok(())
}
