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
use crate::path_guard::{resolve_authorized_path, to_display_path};

/// 打开后会直接执行其中命令的脚本类文件。打开它们等同于执行命令，需要“命令行”权限。
const SCRIPT_EXTENSIONS: &[&str] = &[
    "bat", "cmd", "ps1", "psm1", "psd1", "vbs", "vbe", "js", "jse", "wsf", "wsh", "hta",
];

/// 检查一次“打开软件”请求是否越权，通过时返回实际要交给系统打开的目标。
///
/// - 带命令行参数打开任何程序（例如 `cmd.exe /c ...`）等同于执行任意命令，要求同时有“命令行”权限。
/// - 打开脚本类文件同理。
/// - 按完整路径打开时，和文件操作一样受“只开放这几个文件夹”限制。
/// - 不接受带目录的相对路径：它会相对被控程序的工作目录解析，绕过上面的文件夹限制。
pub fn authorize_launch(perms: &Permissions, args: &AppLaunchArgs) -> Result<String, RpcError> {
    let has_args = args.args.as_deref().is_some_and(|a| !a.trim().is_empty());
    if has_args && !perms.terminal {
        return Err(RpcError::new(
            ErrorCode::Forbidden,
            "带参数打开软件等同于执行命令，需要对方同时开放“命令行”权限",
        ));
    }

    let target = args.target.trim().trim_matches('"').trim();
    if args.kind != LaunchKind::PathOrTarget {
        return Ok(target.to_string());
    }
    if target.is_empty() {
        return Err(RpcError::new(ErrorCode::Invalid, "要打开的路径不能为空"));
    }

    let scheme = url_scheme(target);
    // 网址交给浏览器打开，末尾是 .js 也不会被当脚本执行；其他情况（含 file:）都要看扩展名。
    let is_web = matches!(scheme.as_deref(), Some("http" | "https"));
    if !is_web && is_script_target(target) && !perms.terminal {
        return Err(RpcError::new(
            ErrorCode::Forbidden,
            "打开脚本文件（.bat、.ps1 等）等同于执行命令，需要对方同时开放“命令行”权限",
        ));
    }

    if looks_like_absolute_path(target) {
        let (clean, _) = resolve_authorized_path(target, perms)?;
        return Ok(to_display_path(&clean));
    }

    if looks_like_drive_relative_path(target) {
        return Err(RpcError::new(
            ErrorCode::Invalid,
            format!("请填写完整绝对路径（例如 C:\\目录\\文件），不支持盘符相对路径：{target}"),
        ));
    }

    if let Some(scheme) = scheme.as_deref() {
        // file: 和 shell: 能打开任意文件夹或文件，绕过开放文件夹的限制。
        if !perms.allowed_roots.is_empty() && matches!(scheme, "file" | "shell") {
            return Err(RpcError::new(
                ErrorCode::Forbidden,
                "对方只开放了部分文件夹，不能用 file: 或 shell: 地址打开，请直接填写完整路径",
            ));
        }
        return Ok(target.to_string());
    }

    if target.contains(['\\', '/']) {
        return Err(RpcError::new(
            ErrorCode::Invalid,
            format!("请填写完整路径（例如 D:\\软件\\程序.exe），不支持相对路径：{target}"),
        ));
    }
    // 只有一个名字（如 notepad、calc.exe），由系统按 PATH 和“应用路径”查找。
    Ok(target.to_string())
}

fn looks_like_absolute_path(target: &str) -> bool {
    let b = target.as_bytes();
    let drive = b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':';
    let drive_abs = drive && (b.len() == 2 || b[2] == b'\\' || b[2] == b'/');
    drive_abs || target.starts_with(r"\\") || target.starts_with("//")
}

/// 识别 Windows 盘符相对路径（例如 `C:secret.docx`、`c:dir\a.exe`），防止绕过 `allowed_roots` 边界。
fn looks_like_drive_relative_path(target: &str) -> bool {
    let b = target.as_bytes();
    b.len() >= 3
        && b[0].is_ascii_alphabetic()
        && b[1] == b':'
        && b[2] != b'\\'
        && b[2] != b'/'
}

/// 取出 `https:`、`ms-settings:` 这类地址的协议名（小写）；盘符 `C:` 不算。
fn url_scheme(target: &str) -> Option<String> {
    let (scheme, _) = target.split_once(':')?;
    let valid = scheme.len() >= 2
        && scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    valid.then(|| scheme.to_ascii_lowercase())
}

fn is_script_target(target: &str) -> bool {
    // 去掉 URL 里可能附带的 ?参数 和 #锚点 后再看扩展名
    let core = target.split(['?', '#']).next().unwrap_or(target);
    let last = core.rsplit(['\\', '/']).next().unwrap_or(core);
    let ext = last.trim_end_matches(['.', ' ']).rsplit_once('.').map(|(_, e)| e);
    ext.is_some_and(|e| SCRIPT_EXTENSIONS.iter().any(|s| s.eq_ignore_ascii_case(e)))
}

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
    let launch_target = authorize_launch(perms, &args)?;

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
                .open_target(&launch_target, args.args.as_deref())
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
