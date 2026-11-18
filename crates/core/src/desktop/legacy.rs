use crate::{CapturedShot, CoreError, CoreNode, InputResult};
use pcshare_proto::desktop::{DesktopInput as Event, Source};
use pcshare_proto::{ErrorCode, InputClickArgs, InputKeyArgs, RpcError};
use std::sync::Arc;

enum Action {
    Click(InputClickArgs),
    Key(InputKeyArgs),
    Ignore,
}

pub(super) async fn send(
    node: &Arc<CoreNode>,
    peer: &str,
    source: Source,
    event: Event,
    refresh: bool,
) -> Result<Option<CapturedShot>, CoreError> {
    let result = match action(source, event, refresh)? {
        Action::Click(args) => node.remote_input_click(peer, args).await?,
        Action::Key(args) => node.remote_input_key(peer, args).await?,
        Action::Ignore => return Ok(None),
    };
    legacy_image(result)
}

fn action(source: Source, event: Event, refresh: bool) -> Result<Action, CoreError> {
    let spec = super::refresh_spec(source);
    let target_hwnd = spec.hwnd;
    let refresh_shot = refresh.then_some(spec);
    let key = |text, key_combo| {
        Action::Key(InputKeyArgs {
            text,
            key_combo,
            target_hwnd,
            refresh_shot: refresh_shot.clone(),
        })
    };
    Ok(match event {
        Event::Click {
            x,
            y,
            button,
            double,
        } => Action::Click(InputClickArgs {
            screen_x: x,
            screen_y: y,
            button,
            double,
            target_hwnd,
            refresh_shot,
        }),
        Event::Text { text } => key(Some(text), None),
        Event::Shortcut { combo } => key(None, Some(combo)),
        Event::Move { .. } | Event::Release => Action::Ignore,
        _ => {
            return Err(RpcError::new(
                ErrorCode::Unsupported,
                "旧版对端支持单击、双击、文字和组合键",
            )
            .into());
        }
    })
}

fn legacy_image(result: InputResult) -> Result<Option<CapturedShot>, CoreError> {
    if result.reply.uipi_blocked {
        return Err(RpcError::new(
            ErrorCode::Forbidden,
            result
                .reply
                .warning
                .unwrap_or_else(|| "Windows 拦截了输入".into()),
        )
        .into());
    }
    Ok(result
        .reply
        .refreshed_shot
        .zip(result.refreshed_jpeg)
        .map(|(header, jpeg_bytes)| CapturedShot { header, jpeg_bytes }))
}
