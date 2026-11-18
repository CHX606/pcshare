pub mod bridge;
pub mod bridge_stage3;
pub mod single_instance;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use bridge::{
    BridgeState, EventSink, Reply, SelfInfo, UiAppInfo, UiBatchDeviceResult, UiCapturedShot,
    UiClipboardPreview, UiConflictPolicy, UiDevice, UiDiagLogs, UiDiagSummary, UiEditSyncPrompt,
    UiEditSyncResult, UiFsCacheHit, UiFsChangedEvent, UiFsListBatchEvent, UiFsListing, UiFsRoot,
    UiFsSearchBatchEvent, UiHostStatus, UiInputOutcome, UiLanPeer, UiLaunchResult,
    UiLocalEditSession, UiNetcheckReport, UiPairOffer, UiPairRequest, UiPermissions, UiPowerResult,
    UiProcInfo, UiQuickHit, UiSettings, UiShotTargets, UiSystemNotice, UiTaskProgress,
    UiTermExitEvent, UiTermOutputEvent, UiTermSession, UiWakeResult,
};
use pcshare_core::lan_discovery::MDNS_DEFAULT_PORT;
use pcshare_core::{CoreConfig, CoreNode, NetworkMode, default_data_dir};
use pcshare_platform::NativePlatform;
use serde::Serialize;
use single_instance::SingleInstanceGuard;
use tauri::image::Image;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, State, WebviewUrl, WebviewWindowBuilder, Wry};

type ManagedBridge = Arc<BridgeState>;

mod commands;
mod event_sink;
mod shell;
use event_sink::TauriEventSink;
use shell::{create_or_focus_main_window, parse_launch_args, setup_system_tray};

pub fn run() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing_subscriber::filter::LevelFilter::WARN)
        .with_target(false)
        .try_init();

    let args = parse_launch_args();
    let slot: Arc<OnceLock<AppHandle>> = Arc::new(OnceLock::new());
    let wake = slot.clone();
    let Some(_guard) = SingleInstanceGuard::try_acquire(&args.data_dir, move || {
        if let Some(handle) = wake.get() {
            create_or_focus_main_window(handle);
        }
    }) else {
        return;
    };
    let quitting = Arc::new(AtomicBool::new(false));
    let exit = quitting.clone();
    let app = tauri::Builder::default()
        .setup(move |app| setup(app, args, slot, quitting))
        .invoke_handler(commands::handlers!())
        .build(tauri::generate_context!())
        .expect("构建电脑共享桌面应用失败");
    app.run(move |handle, event| on_run_event(handle, event, &exit));
}

fn setup(
    app: &mut tauri::App,
    args: shell::LaunchArgs,
    slot: Arc<OnceLock<AppHandle>>,
    quitting: Arc<AtomicBool>,
) -> Result<(), Box<dyn std::error::Error>> {
    let handle = app.handle().clone();
    let _ = slot.set(handle.clone());
    let sink = Arc::new(TauriEventSink {
        app: handle.clone(),
        pause_menu_item: std::sync::Mutex::new(None),
    });
    let bridge = start_bridge(args.data_dir, args.network_mode, sink.clone())?;
    app.manage(bridge.clone());
    let _ = setup_system_tray(&handle, &sink, quitting);
    let wake = handle.clone();
    let launcher = bridge.clone();
    let hotkey = bridge.bind_hotkey_handler(Arc::new(move || {
        create_or_focus_main_window(&wake);
        launcher.mark_launcher_pending();
    }));
    if !args.background {
        create_or_focus_main_window(&handle);
    }
    if let Err(body) = hotkey {
        sink.emit_system_notice(UiSystemNotice {
            title: "全局快捷键被占用".into(),
            body,
            kind: "warn".into(),
        });
    }
    Ok(())
}

fn start_bridge(
    data_dir: PathBuf,
    network_mode: NetworkMode,
    sink: Arc<dyn EventSink>,
) -> Result<Arc<BridgeState>, std::io::Error> {
    let cfg = CoreConfig {
        data_dir,
        network_mode,
        device_name_override: None,
        auto_connect_peers: true,
    };
    tauri::async_runtime::block_on(async move {
        let node = CoreNode::start(cfg, Arc::new(NativePlatform::new())).await?;
        if network_mode == NetworkMode::PublicOrCustom {
            let _ = node.start_lan_discovery_on_port(MDNS_DEFAULT_PORT);
        }
        Ok::<_, pcshare_core::CoreError>(BridgeState::new(node, sink))
    })
    .map_err(|e| std::io::Error::other(e.to_user_message()))
}

fn on_run_event(app_handle: &AppHandle, event: tauri::RunEvent, quitting: &AtomicBool) {
    match event {
        tauri::RunEvent::ExitRequested { api, .. } => {
            // 关闭最后一个窗口时销毁了浏览器内核以释放内存，但除非用户在托盘点“退出程序”，否则保持后台常驻
            if !quitting.load(Ordering::SeqCst) {
                api.prevent_exit();
            }
        }
        tauri::RunEvent::Exit => {
            if let Some(bridge) = app_handle.try_state::<ManagedBridge>() {
                let b = bridge.inner().clone();
                tauri::async_runtime::block_on(async move {
                    b.shutdown().await;
                });
            }
        }
        _ => {}
    }
}
