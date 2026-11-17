//! “打开软件”的越权检查：带参数、脚本文件、开放文件夹范围、相对路径。

use pcshare_core::app_proc::authorize_launch;
use pcshare_proto::{AppLaunchArgs, ErrorCode, LaunchKind, Permissions};

fn launch(target: &str, kind: LaunchKind, args: Option<&str>) -> AppLaunchArgs {
    AppLaunchArgs {
        target: target.to_string(),
        kind,
        args: args.map(ToOwned::to_owned),
        skip_window_wait: false,
    }
}

fn with_terminal() -> Permissions {
    Permissions {
        terminal: true,
        ..Permissions::default()
    }
}

fn err_code(perms: &Permissions, args: &AppLaunchArgs) -> ErrorCode {
    authorize_launch(perms, args).expect_err("应当被拒绝").code
}

#[test]
fn args_require_terminal_permission() {
    let p = Permissions::default();
    for kind in [LaunchKind::PathOrTarget, LaunchKind::AppId, LaunchKind::AppName] {
        let a = launch("cmd.exe", kind, Some("/c whoami"));
        assert_eq!(err_code(&p, &a), ErrorCode::Forbidden);
        assert!(authorize_launch(&with_terminal(), &a).is_ok());
    }
    // 空白参数等于没带参数
    assert!(authorize_launch(&p, &launch("notepad", LaunchKind::PathOrTarget, Some("  "))).is_ok());
}

#[test]
fn script_files_require_terminal_permission() {
    let p = Permissions::default();
    for target in [
        r"C:\Users\Public\a.bat",
        r"C:\Users\Public\b.PS1",
        r#""C:\Users\Public\c.vbs""#,
        "file:///C:/Users/Public/d.cmd",
        r"C:\Users\Public\e.bat. ",
    ] {
        let a = launch(target, LaunchKind::PathOrTarget, None);
        assert_eq!(err_code(&p, &a), ErrorCode::Forbidden, "{target}");
        assert!(authorize_launch(&with_terminal(), &a).is_ok(), "{target}");
    }
    // 普通程序、文档和网址不受影响（网址交给浏览器，末尾是 .js 也不会执行）
    for target in [
        r"C:\Windows\notepad.exe",
        r"C:\Users\Public\报告.docx",
        "https://example.com/a.js?x=1",
    ] {
        let a = launch(target, LaunchKind::PathOrTarget, None);
        assert!(authorize_launch(&p, &a).is_ok(), "{target}");
    }
}

#[cfg(windows)]
#[test]
fn paths_respect_allowed_roots() {
    let base = std::env::temp_dir().join(format!("pcshare_launch_{}", std::process::id()));
    let root = base.join("open");
    std::fs::create_dir_all(&root).unwrap();
    let root_str = root.to_string_lossy().to_string();
    let p = Permissions {
        allowed_roots: vec![root_str.clone()],
        ..Permissions::default()
    };

    let inside = format!(r"{root_str}\tool.exe");
    let got = authorize_launch(&p, &launch(&inside, LaunchKind::PathOrTarget, None)).unwrap();
    assert!(got.to_lowercase().ends_with(r"open\tool.exe"), "{got}");

    for outside in [
        r"C:\Windows\System32\notepad.exe".to_string(),
        format!(r"{root_str}\..\other.exe"),
    ] {
        let a = launch(&outside, LaunchKind::PathOrTarget, None);
        assert_eq!(err_code(&p, &a), ErrorCode::Forbidden, "{outside}");
    }

    // 只开放部分文件夹时，file: 和 shell: 地址不能用
    for target in ["file:///C:/Windows/notepad.exe", r"shell:startup"] {
        let a = launch(target, LaunchKind::PathOrTarget, None);
        assert_eq!(err_code(&p, &a), ErrorCode::Forbidden, "{target}");
        assert!(authorize_launch(&Permissions::default(), &a).is_ok(), "{target}");
    }

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn relative_paths_and_names() {
    let p = Permissions::default();
    assert_eq!(
        err_code(&p, &launch(r"..\..\x.exe", LaunchKind::PathOrTarget, None)),
        ErrorCode::Invalid
    );
    assert_eq!(
        err_code(&p, &launch("tools/x.exe", LaunchKind::PathOrTarget, None)),
        ErrorCode::Invalid
    );
    assert_eq!(
        err_code(&p, &launch("   ", LaunchKind::PathOrTarget, None)),
        ErrorCode::Invalid
    );
    for ok in ["notepad", "calc.exe", "ms-settings:display", "https://example.com"] {
        assert!(authorize_launch(&p, &launch(ok, LaunchKind::PathOrTarget, None)).is_ok(), "{ok}");
    }
}
