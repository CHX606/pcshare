//! 被控端软件与进程管理服务（任务 1.9）。

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use pcshare_platform::{DesktopEnvironment, Platform, ShellOpenOutcome};
use pcshare_proto::{
    AppIconArgs, AppIconReply, AppLaunchArgs, AppLaunchReply, AppListArgs, AppListReply,
    DesktopStatus, ErrorCode, InstalledApp, LaunchKind, Permissions, ProcKillArgs, ProcKillReply,
    ProcListReply, ProcessEntry, RpcError,
};

use crate::error::map_platform_to_rpc;

pub fn to_proto_desktop(env: DesktopEnvironment) -> DesktopStatus {
    let interactive = env.is_interactive();
    let summary = env.summary();
    DesktopStatus {
        session_id: env.session_id,
        console_session_id: env.console_session_id,
        elevated: env.elevated,
        input_desktop: env.input_desktop,
        interactive,
        summary,
    }
}

pub async fn handle_app_list(
    platform: &Arc<dyn Platform>,
    perms: &Permissions,
    args: AppListArgs,
) -> Result<AppListReply, RpcError> {
    if !perms.app_launch {
        return Err(RpcError::new(ErrorCode::Forbidden, "对方没有开放启动软件的权限"));
    }
    let apps = platform
        .installed_apps(args.refresh)
        .await
        .map_err(|e| map_platform_to_rpc(&e))?
        .into_iter()
        .map(|a| InstalledApp {
            name: a.name,
            app_id: a.app_id,
        })
        .collect();
    Ok(AppListReply { apps })
}

pub async fn handle_app_icon(
    platform: &Arc<dyn Platform>,
    perms: &Permissions,
    args: AppIconArgs,
) -> Result<AppIconReply, RpcError> {
    if !perms.app_launch {
        return Err(RpcError::new(ErrorCode::Forbidden, "对方没有开放启动软件的权限"));
    }
    let png_opt = platform
        .app_icon(&args.app_id)
        .await
        .map_err(|e| map_platform_to_rpc(&e))?;
    let png_base64 = png_opt.map(|b| base64::engine::general_purpose::STANDARD.encode(b));
    Ok(AppIconReply { png_base64 })
}

pub async fn handle_app_launch(
    platform: &Arc<dyn Platform>,
    perms: &Permissions,
    args: AppLaunchArgs,
) -> Result<AppLaunchReply, RpcError> {
    if !perms.app_launch {
        return Err(RpcError::new(ErrorCode::Forbidden, "对方没有开放启动软件的权限"));
    }

    let before_desktop = platform.desktop_environment();
    let before_hwnds: HashSet<usize> = platform
        .visible_windows()
        .into_iter()
        .map(|w| w.hwnd)
        .collect();

    let (resolved_name, outcome) = match args.kind {
        LaunchKind::AppId => {
            let out = platform
                .launch_app_id(&args.target, args.args.as_deref())
                .await
                .map_err(|e| map_platform_to_rpc(&e))?;
            (None, out)
        }
        LaunchKind::PathOrTarget => {
            let out = platform
                .open_target(&args.target, args.args.as_deref())
                .await
                .map_err(|e| map_platform_to_rpc(&e))?;
            (None, out)
        }
        LaunchKind::AppName => {
            let apps = platform
                .installed_apps(false)
                .await
                .map_err(|e| map_platform_to_rpc(&e))?;
            let needle = args.target.trim().to_lowercase();
            let chosen = if let Some(exact) =
                apps.iter().find(|a| a.name.to_lowercase() == needle)
            {
                exact
            } else {
                let matches: Vec<_> = apps
                    .iter()
                    .filter(|a| a.name.to_lowercase().contains(&needle))
                    .collect();
                match matches.as_slice() {
                    [] => {
                        return Err(RpcError::new(
                            ErrorCode::NotFound,
                            format!("对方的开始菜单里找不到名称包含“{}”的软件", args.target),
                        ));
                    }
                    [only] => *only,
                    many => {
                        let names: Vec<&str> =
                            many.iter().take(8).map(|a| a.name.as_str()).collect();
                        return Err(RpcError::new(
                            ErrorCode::Invalid,
                            format!(
                                "有 {} 个软件名称都包含“{}”，请指定更具体的名称：{}",
                                many.len(),
                                args.target,
                                names.join("、")
                            ),
                        ));
                    }
                }
            };
            let app_name = chosen.name.clone();
            let app_id = chosen.app_id.clone();
            let out = platform
                .launch_app_id(&app_id, args.args.as_deref())
                .await
                .map_err(|e| map_platform_to_rpc(&e))?;
            (Some(app_name), out)
        }
    };

    if outcome == ShellOpenOutcome::SuspectedUacPrompt {
        let desktop = to_proto_desktop(platform.desktop_environment());
        return Ok(AppLaunchReply {
            resolved_name,
            new_windows: Vec::new(),
            desktop,
            uac_prompt_suspected: true,
            hint: Some(
                "对方电脑弹出了管理员权限确认框（UAC），需要有人在屏幕前点击允许；若需免确认，可在对方设置中开启“以管理员权限随登录启动”。"
                    .to_string(),
            ),
        });
    }

    if args.skip_window_wait {
        let after_desktop = to_proto_desktop(platform.desktop_environment());
        return Ok(AppLaunchReply {
            resolved_name,
            new_windows: Vec::new(),
            desktop: after_desktop,
            uac_prompt_suspected: false,
            hint: None,
        });
    }

    // 观察最多 6 秒收集新出现的顶层窗口标题（见分阶段任务 1.9 节）
    let mut new_windows = Vec::new();
    let mut uac_suspected = false;
    if before_desktop.session_id != 0 {
        let deadline = Instant::now() + Duration::from_secs(6);
        while Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let now_env = platform.desktop_environment();
            if !before_desktop.elevated
                && before_desktop.input_desktop.is_some()
                && now_env.input_desktop.is_none()
            {
                uac_suspected = true;
                break;
            }
            let diff: Vec<String> = platform
                .visible_windows()
                .into_iter()
                .filter(|w| !before_hwnds.contains(&w.hwnd))
                .map(|w| w.title)
                .collect();
            if !diff.is_empty() {
                tokio::time::sleep(Duration::from_millis(400)).await;
                new_windows = platform
                    .visible_windows()
                    .into_iter()
                    .filter(|w| !before_hwnds.contains(&w.hwnd))
                    .map(|w| w.title)
                    .collect();
                break;
            }
        }
    }

    let after_desktop = to_proto_desktop(platform.desktop_environment());
    let hint = if uac_suspected {
        Some(
            "对方电脑疑似弹出了管理员权限确认框，需要有人在对方屏幕前点击确认。"
                .to_string(),
        )
    } else if after_desktop.session_id == 0 || !after_desktop.interactive {
        Some("已启动，但对方当前处于锁屏或非活动桌面状态，现在屏幕上看不到窗口。".to_string())
    } else if new_windows.is_empty() {
        Some("已发送启动指令，未检测到新窗口（软件可能本来就已经开着，或后台静默运行）。".to_string())
    } else {
        None
    };

    Ok(AppLaunchReply {
        resolved_name,
        new_windows,
        desktop: after_desktop,
        uac_prompt_suspected: uac_suspected,
        hint,
    })
}

pub async fn handle_proc_list(
    platform: &Arc<dyn Platform>,
    perms: &Permissions,
) -> Result<ProcListReply, RpcError> {
    if !perms.proc_manage {
        return Err(RpcError::new(ErrorCode::Forbidden, "对方没有开放管理进程的权限"));
    }
    let plat = platform.clone();
    let list = tokio::task::spawn_blocking(move || plat.list_processes())
        .await
        .map_err(|e| RpcError::new(ErrorCode::Internal, format!("查询进程异常：{e}")))?
        .map_err(|e| map_platform_to_rpc(&e))?;

    let processes = list
        .into_iter()
        .map(|p| ProcessEntry {
            pid: p.pid,
            name: p.name,
            window_title: p.window_title,
            memory_bytes: p.memory_bytes,
            cpu_percent: p.cpu_percent,
            is_critical: p.is_critical,
        })
        .collect();
    Ok(ProcListReply { processes })
}

pub async fn handle_proc_kill(
    platform: &Arc<dyn Platform>,
    perms: &Permissions,
    args: ProcKillArgs,
) -> Result<ProcKillReply, RpcError> {
    if !perms.proc_manage {
        return Err(RpcError::new(ErrorCode::Forbidden, "对方没有开放管理进程的权限"));
    }
    if !args.confirmed {
        return Err(RpcError::new(
            ErrorCode::Invalid,
            "结束进程会导致未保存的工作丢失，请先二次确认（confirmed = true）",
        ));
    }
    let plat = platform.clone();
    let pid = args.pid;
    let killed_name = tokio::task::spawn_blocking(move || plat.kill_process(pid))
        .await
        .map_err(|e| RpcError::new(ErrorCode::Internal, format!("结束进程异常：{e}")))?
        .map_err(|e| map_platform_to_rpc(&e))?;

    Ok(ProcKillReply {
        pid,
        name: killed_name,
    })
}
