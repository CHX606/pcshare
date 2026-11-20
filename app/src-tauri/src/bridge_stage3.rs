//! 桌面桥接层第 3 阶段扩展：
//! 快捷搜索与全局热键、流式文件搜索、看一眼与截图点按、远端文件本地编辑与回传冲突检测、
//! 电源与网络唤醒、交互式伪终端、剪贴板互通、批量操作、拖出到系统桌面。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime};

use base64::Engine as _;
use pcshare_core::error::map_platform_to_rpc;
use pcshare_core::path_guard::to_display_path;
use pcshare_core::{
    BatchCommand, BatchDeviceResult, CapturedShot, CoreError, EditSyncOutcome, InputResult,
    QuickHitKind, RemoteClipboardData, TerminalStreamEvent,
};
use pcshare_platform::ClipboardContent;
use pcshare_proto::{
    ClipKind, ConflictPolicy, EntryKind, ErrorCode, InputClickArgs, InputKeyArgs, LaunchKind,
    MouseBtn, PowerAction, PowerExecArgs, ShotRefreshSpec, ShotScreenArgs, ShotWindowArgs,
    TermOpenArgs, TermShell,
};
use serde::{Deserialize, Serialize};

use crate::bridge::{
    BridgeState, Reply, SELF_ID, UiDesktopState, UiErrorCode, UiLaunchResult,
    convert_desktop_status, convert_launch_reply,
};

const TERM_BATCH_MAX_BYTES: usize = 16 * 1024;
const TERM_BATCH_WINDOW: Duration = Duration::from_millis(16);
const DRAG_OUT_STALE_AGE: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiFsSearchHit {
    pub name: String,
    pub path: String,
    pub dir: bool,
    pub size: u64,
    pub mtime: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiFsSearchBatchEvent {
    pub search_id: String,
    pub device: String,
    pub entries: Vec<UiFsSearchHit>,
    pub scanned_dirs: u64,
    pub done: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiQuickHit {
    pub peer_id: String,
    pub peer_name: String,
    pub online: bool,
    pub kind: String,
    pub title: String,
    pub subtitle: String,
    pub target: String,
    pub launch_kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiMonitorInfo {
    pub index: u32,
    pub name: String,
    pub origin_x: i32,
    pub origin_y: i32,
    pub width: u32,
    pub height: u32,
    pub dpi_scale_percent: u32,
    pub is_primary: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiCapturableWindow {
    pub hwnd: String,
    pub pid: u32,
    pub title: String,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiShotTargets {
    pub monitors: Vec<UiMonitorInfo>,
    pub windows: Vec<UiCapturableWindow>,
    pub desktop: UiDesktopState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiShotTransform {
    pub origin_x: i32,
    pub origin_y: i32,
    pub source_width: u32,
    pub source_height: u32,
    pub image_width: u32,
    pub image_height: u32,
    pub dpi_scale_percent: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_hwnd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub monitor_index: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiCapturedShot {
    pub image_data_url: String,
    pub byte_len: u32,
    pub captured_at_ms: u64,
    pub transform: UiShotTransform,
    pub desktop: UiDesktopState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiInputOutcome {
    pub uipi_blocked: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refreshed_shot: Option<UiCapturedShot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiLocalEditSession {
    pub session_id: String,
    pub peer_id: String,
    pub peer_name: String,
    pub remote_path: String,
    pub local_path: String,
    pub file_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiEditSyncPrompt {
    pub session_id: String,
    pub peer_id: String,
    pub peer_name: String,
    pub remote_path: String,
    pub local_path: String,
    pub file_name: String,
    pub conflict: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiEditSyncResult {
    pub conflict: bool,
    pub bytes_written: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiPowerResult {
    pub action: String,
    pub cancelled: bool,
    pub executed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiWakeResult {
    pub local_packets: usize,
    pub relay_peers_notified: usize,
    pub relayed_packets: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiTermSession {
    pub session_id: String,
    pub pid: u32,
    pub shell_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiTermOutputEvent {
    pub session_id: String,
    pub data_b64: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiTermExitEvent {
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiClipboardPreview {
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_data_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiBatchDeviceResult {
    pub peer_id: String,
    pub peer_name: String,
    pub ok: bool,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiSystemNotice {
    pub title: String,
    pub body: String,
    pub kind: String,
}

fn convert_captured_shot(shot: CapturedShot) -> UiCapturedShot {
    let b64 = base64::engine::general_purpose::STANDARD.encode(&shot.jpeg_bytes);
    let t = shot.header.transform;
    UiCapturedShot {
        image_data_url: format!("data:image/jpeg;base64,{b64}"),
        byte_len: shot.header.byte_len,
        captured_at_ms: shot.header.captured_at_ms,
        transform: UiShotTransform {
            origin_x: t.origin_x,
            origin_y: t.origin_y,
            source_width: t.source_width,
            source_height: t.source_height,
            image_width: t.image_width,
            image_height: t.image_height,
            dpi_scale_percent: t.dpi_scale_percent,
            target_hwnd: t.target_hwnd.map(|h| h.to_string()),
            monitor_index: t.monitor_index,
        },
        desktop: convert_desktop_status(&shot.header.desktop),
    }
}

fn parse_optional_hwnd(raw: Option<String>, label: &str) -> Result<Option<u64>, String> {
    let Some(s) = raw else {
        return Ok(None);
    };
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    match trimmed.parse::<u64>() {
        Ok(h) if h > 0 => Ok(Some(h)),
        _ => Err(format!("无效的{label}编号")),
    }
}

fn build_refresh_spec(
    refresh_monitor: Option<u32>,
    refresh_hwnd: Option<u64>,
    max_width: Option<u32>,
) -> Option<ShotRefreshSpec> {
    if let Some(h) = refresh_hwnd {
        return Some(ShotRefreshSpec {
            monitor_index: None,
            hwnd: Some(h),
            max_width,
            quality: Some(80),
        });
    }
    refresh_monitor.map(|m| ShotRefreshSpec {
        monitor_index: Some(m),
        hwnd: None,
        max_width,
        quality: Some(80),
    })
}

fn convert_input_outcome(r: InputResult) -> UiInputOutcome {
    let refreshed_shot = r
        .reply
        .refreshed_shot
        .zip(r.refreshed_jpeg)
        .map(|(header, jpeg_bytes)| convert_captured_shot(CapturedShot { header, jpeg_bytes }));
    UiInputOutcome {
        uipi_blocked: r.reply.uipi_blocked,
        warning: r.reply.warning,
        refreshed_shot,
    }
}

fn convert_remote_clipboard(d: RemoteClipboardData) -> UiClipboardPreview {
    match d.header.kind {
        ClipKind::Empty => UiClipboardPreview {
            kind: "empty".to_string(),
            text: None,
            image_data_url: None,
            width: None,
            height: None,
        },
        ClipKind::Text => UiClipboardPreview {
            kind: "text".to_string(),
            text: d.header.text,
            image_data_url: None,
            width: None,
            height: None,
        },
        ClipKind::ImagePng => {
            let b64 = base64::engine::general_purpose::STANDARD.encode(&d.png_bytes);
            UiClipboardPreview {
                kind: "image_png".to_string(),
                text: None,
                image_data_url: Some(format!("data:image/png;base64,{b64}")),
                width: d.header.image_width,
                height: d.header.image_height,
            }
        }
    }
}

fn convert_batch_result(r: BatchDeviceResult) -> UiBatchDeviceResult {
    UiBatchDeviceResult {
        peer_id: r.peer_id,
        peer_name: r.peer_name,
        ok: r.ok,
        summary: r.summary,
        output: r.output,
        elapsed_ms: r.elapsed_ms,
    }
}

/// 清理 `drag_out_staging/drag-*` 临时拖出目录。
/// - `max_age == None`：清理全部 `drag-*` 子目录（用于启动与退出关停）；
/// - `max_age == Some(age)`：仅清理早于 `age` 的过期目录（避免干扰刚刚拖出正在被系统粘贴的文件）。
pub fn cleanup_drag_out_staging(data_dir: &Path, max_age: Option<Duration>) {
    let root = data_dir.join("drag_out_staging");
    let Ok(rd) = std::fs::read_dir(&root) else {
        return;
    };
    let now = SystemTime::now();
    for entry in rd.flatten() {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if !name_str.starts_with("drag-") {
            continue;
        }
        let path = entry.path();
        if let Some(age) = max_age {
            let is_stale = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|mt| now.duration_since(mt).ok())
                .is_some_and(|elapsed| elapsed >= age);
            if !is_stale {
                continue;
            }
        }
        if path.is_dir() {
            let _ = std::fs::remove_dir_all(&path);
        } else {
            let _ = std::fs::remove_file(&path);
        }
    }
}

impl BridgeState {
    // ────────────────────────── 任务 3.1：快捷搜索框与全局热键 ──────────────────────────

    /// 处理全局快捷键呼出：
    /// - 仅当主窗口当时不存在、本次需要冷启建窗（`cold_started == true`）时置位 `pending_launcher_open = true`，
    ///   避免与前端启动消费抵消；
    /// - 主窗口已存在（`cold_started == false`）时直接发送 `launcher://toggle` 且保持 `pending_launcher_open = false`。
    pub fn on_launcher_hotkey(&self, cold_started: bool) {
        if cold_started {
            self.pending_launcher_open.store(true, Ordering::SeqCst);
        } else {
            self.pending_launcher_open.store(false, Ordering::SeqCst);
            self.sink.emit_launcher_toggle();
        }
    }

    pub fn mark_launcher_pending(&self) {
        self.on_launcher_hotkey(true);
    }

    pub fn launcher_consume_pending(&self) -> Reply<bool> {
        Reply::ok(self.pending_launcher_open.swap(false, Ordering::SeqCst))
    }

    pub fn quick_search(&self, query: String, limit: Option<usize>) -> Reply<Vec<UiQuickHit>> {
        let hits = self
            .node()
            .quick_search(&query, limit.unwrap_or(12))
            .into_iter()
            .map(|h| UiQuickHit {
                peer_id: h.peer_id,
                peer_name: h.peer_name,
                online: h.online,
                kind: match h.kind {
                    QuickHitKind::App => "app".to_string(),
                    QuickHitKind::Folder => "folder".to_string(),
                },
                title: h.title,
                subtitle: h.subtitle,
                target: h.target,
                launch_kind: match h.launch_kind {
                    LaunchKind::AppId => "app_id".to_string(),
                    LaunchKind::AppName => "app_name".to_string(),
                    LaunchKind::PathOrTarget => "path".to_string(),
                },
            })
            .collect();
        Reply::ok(hits)
    }

    pub async fn quick_launch(
        self: &Arc<Self>,
        device: String,
        target: String,
        kind: String,
    ) -> Reply<UiLaunchResult> {
        let lk = match kind.as_str() {
            "app_name" => LaunchKind::AppName,
            "path" => LaunchKind::PathOrTarget,
            _ => LaunchKind::AppId,
        };
        Reply::from_core(
            self.node()
                .quick_launch_target(&device, &target, lk)
                .await
                .map(convert_launch_reply),
        )
    }

    // ────────────────────────── 任务 3.9：边搜边回可取消文件搜索 ──────────────────────────

    pub async fn fs_search_start(
        self: &Arc<Self>,
        device: String,
        path: String,
        query: String,
    ) -> Reply<String> {
        if query.trim().is_empty() {
            return Reply::err(UiErrorCode::Invalid, "请输入要搜索的文件名关键字");
        }
        let search_id = format!("search-{}", self.next_seq());
        let cancel = self.shutdown.child_token();
        self.active_searches
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(search_id.clone(), cancel.clone());

        let bridge = self.clone();
        let sid = search_id.clone();
        tokio::spawn(async move {
            let sink = bridge.sink.clone();
            let dev_for_cb = device.clone();
            let sid_for_cb = sid.clone();
            let on_batch = move |batch: pcshare_proto::FsSearchBatch| {
                let entries = batch
                    .entries
                    .into_iter()
                    .map(|e| UiFsSearchHit {
                        name: e.name,
                        path: e.path,
                        dir: e.kind == EntryKind::Dir,
                        size: e.size,
                        mtime: e.modified_ms.unwrap_or(0),
                    })
                    .collect();
                sink.emit_fs_search_batch(UiFsSearchBatchEvent {
                    search_id: sid_for_cb.clone(),
                    device: dev_for_cb.clone(),
                    entries,
                    scanned_dirs: batch.scanned_dirs,
                    done: batch.done,
                });
            };

            let res = if device == SELF_ID {
                bridge
                    .node()
                    .local_search_files(&path, &query, Some(500), cancel, on_batch)
                    .await
            } else {
                bridge
                    .node()
                    .remote_search_files(&device, &path, &query, Some(500), cancel, on_batch)
                    .await
            };

            bridge
                .active_searches
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&sid);

            if res.is_err() {
                bridge.sink.emit_fs_search_batch(UiFsSearchBatchEvent {
                    search_id: sid,
                    device,
                    entries: Vec::new(),
                    scanned_dirs: 0,
                    done: true,
                });
            }
        });

        Reply::ok(search_id)
    }

    pub fn fs_search_cancel(&self, search_id: String) -> Reply<()> {
        if let Some(tok) = self
            .active_searches
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&search_id)
        {
            tok.cancel();
        }
        Reply::ok(())
    }

    // ────────────────────────── 任务 3.2 & 3.3：截图与点击键盘控制 ──────────────────────────

    pub async fn shot_monitors(self: &Arc<Self>, device: String) -> Reply<UiShotTargets> {
        let res = if device == SELF_ID {
            self.node().local_shot_monitors().await
        } else {
            self.node().remote_shot_monitors(&device).await
        };
        Reply::from_core(res.map(|r| UiShotTargets {
            monitors: r
                .monitors
                .into_iter()
                .map(|m| UiMonitorInfo {
                    index: m.index,
                    name: m.name,
                    origin_x: m.origin_x,
                    origin_y: m.origin_y,
                    width: m.width,
                    height: m.height,
                    dpi_scale_percent: m.dpi_scale_percent,
                    is_primary: m.is_primary,
                })
                .collect(),
            windows: r
                .windows
                .into_iter()
                .map(|w| UiCapturableWindow {
                    hwnd: w.hwnd.to_string(),
                    pid: w.pid,
                    title: w.title,
                    width: w.width,
                    height: w.height,
                })
                .collect(),
            desktop: convert_desktop_status(&r.desktop),
        }))
    }

    pub async fn shot_capture(
        self: &Arc<Self>,
        device: String,
        monitor_index: Option<u32>,
        hwnd: Option<String>,
        max_width: Option<u32>,
        quality: Option<u8>,
    ) -> Reply<UiCapturedShot> {
        let parsed_hwnd = match parse_optional_hwnd(hwnd, "窗口句柄") {
            Ok(h) => h,
            Err(msg) => return Reply::err(UiErrorCode::Invalid, msg),
        };
        let res = if let Some(h) = parsed_hwnd {
            let args = ShotWindowArgs {
                hwnd: h,
                max_width,
                quality,
            };
            if device == SELF_ID {
                self.node().local_shot_window(args).await
            } else {
                self.node().remote_shot_window(&device, args).await
            }
        } else {
            let args = ShotScreenArgs {
                monitor_index,
                max_width,
                quality,
            };
            if device == SELF_ID {
                self.node().local_shot_screen(args).await
            } else {
                self.node().remote_shot_screen(&device, args).await
            }
        };
        Reply::from_core(res.map(convert_captured_shot))
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn input_click(
        self: &Arc<Self>,
        device: String,
        screen_x: i32,
        screen_y: i32,
        button: String,
        double: bool,
        target_hwnd: Option<String>,
        refresh_monitor: Option<u32>,
        refresh_hwnd: Option<String>,
        max_width: Option<u32>,
    ) -> Reply<UiInputOutcome> {
        if device == SELF_ID {
            return Reply::err(
                UiErrorCode::Invalid,
                "不能对本机自己的预览画面循环发送模拟点击",
            );
        }
        let btn = match button.as_str() {
            "right" => MouseBtn::Right,
            "middle" => MouseBtn::Middle,
            _ => MouseBtn::Left,
        };
        let thwnd = match parse_optional_hwnd(target_hwnd, "目标窗口句柄") {
            Ok(h) => h,
            Err(msg) => return Reply::err(UiErrorCode::Invalid, msg),
        };
        let rhwnd = match parse_optional_hwnd(refresh_hwnd, "刷新窗口句柄") {
            Ok(h) => h,
            Err(msg) => return Reply::err(UiErrorCode::Invalid, msg),
        };
        let args = InputClickArgs {
            screen_x,
            screen_y,
            button: btn,
            double,
            target_hwnd: thwnd,
            refresh_shot: build_refresh_spec(refresh_monitor, rhwnd, max_width),
        };
        Reply::from_core(
            self.node()
                .remote_input_click(&device, args)
                .await
                .map(convert_input_outcome),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn input_key(
        self: &Arc<Self>,
        device: String,
        text: Option<String>,
        key_combo: Option<String>,
        target_hwnd: Option<String>,
        refresh_monitor: Option<u32>,
        refresh_hwnd: Option<String>,
        max_width: Option<u32>,
    ) -> Reply<UiInputOutcome> {
        if device == SELF_ID {
            return Reply::err(
                UiErrorCode::Invalid,
                "不能对本机自己的预览画面循环发送模拟按键",
            );
        }
        let thwnd = match parse_optional_hwnd(target_hwnd, "目标窗口句柄") {
            Ok(h) => h,
            Err(msg) => return Reply::err(UiErrorCode::Invalid, msg),
        };
        let rhwnd = match parse_optional_hwnd(refresh_hwnd, "刷新窗口句柄") {
            Ok(h) => h,
            Err(msg) => return Reply::err(UiErrorCode::Invalid, msg),
        };
        let args = InputKeyArgs {
            text,
            key_combo,
            target_hwnd: thwnd,
            refresh_shot: build_refresh_spec(refresh_monitor, rhwnd, max_width),
        };
        Reply::from_core(
            self.node()
                .remote_input_key(&device, args)
                .await
                .map(convert_input_outcome),
        )
    }

    // ────────────────────────── 任务 3.4：远端文件两种打开方式与编辑回传 ──────────────────────────

    pub async fn fs_open_remote(
        self: &Arc<Self>,
        device: String,
        path: String,
    ) -> Reply<UiLaunchResult> {
        if device == SELF_ID {
            return self.app_launch(device, path, None).await;
        }
        Reply::from_core(
            self.node()
                .open_file_on_remote(&device, &path)
                .await
                .map(convert_launch_reply),
        )
    }

    pub async fn fs_open_local_edit(
        self: &Arc<Self>,
        device: String,
        path: String,
        launch_app: Option<bool>,
    ) -> Reply<UiLocalEditSession> {
        if device == SELF_ID {
            return Reply::err(UiErrorCode::Invalid, "本机文件可直接双击打开，无需取回临时副本");
        }
        Reply::from_core(
            self.node()
                .open_remote_file_locally(&device, &path, launch_app.unwrap_or(true))
                .await
                .map(|info| UiLocalEditSession {
                    session_id: info.session_id,
                    peer_id: info.peer_id,
                    peer_name: info.peer_name,
                    remote_path: info.remote_path,
                    local_path: to_display_path(&info.local_path),
                    file_name: info.file_name,
                }),
        )
    }

    pub async fn fs_edit_sync_back(
        self: &Arc<Self>,
        session_id: String,
        auto_sync: bool,
        force_overwrite: bool,
    ) -> Reply<UiEditSyncResult> {
        let res = self
            .node()
            .sync_back_edited_file(&session_id, auto_sync, force_overwrite)
            .await;
        match &res {
            Ok(EditSyncOutcome::Uploaded { .. }) => {
                self.remove_pending_edit_prompt(&session_id);
            }
            Ok(EditSyncOutcome::ConflictDetected { .. }) => {
                self.mark_pending_edit_conflict(&session_id);
            }
            Err(CoreError::Rpc(rpc)) if rpc.code == ErrorCode::NotFound => {
                self.remove_pending_edit_prompt(&session_id);
            }
            _ => {}
        }
        Reply::from_core(res.map(|outcome| match outcome {
            EditSyncOutcome::Uploaded { bytes_written, .. } => UiEditSyncResult {
                conflict: false,
                bytes_written,
            },
            EditSyncOutcome::ConflictDetected { .. } => UiEditSyncResult {
                conflict: true,
                bytes_written: 0,
            },
        }))
    }

    // ────────────────────────── 任务 3.5：电源操作与网络唤醒 ──────────────────────────

    pub async fn power_exec(
        self: &Arc<Self>,
        device: String,
        action: String,
        countdown_secs: Option<u32>,
    ) -> Reply<UiPowerResult> {
        if device == SELF_ID {
            return Reply::err(UiErrorCode::Invalid, "为防误触，不支持在控制界面直接关停本机");
        }
        let act = match action.as_str() {
            "lock" => PowerAction::Lock,
            "sleep" => PowerAction::Sleep,
            "restart" => PowerAction::Restart,
            "shutdown" => PowerAction::Shutdown,
            _ => return Reply::err(UiErrorCode::Invalid, format!("未知电源动作：{action}")),
        };
        let req_name = self.node().settings().device_name;
        Reply::from_core(
            self.node()
                .remote_power_exec(
                    &device,
                    PowerExecArgs {
                        action: act,
                        countdown_secs,
                        requester_name: req_name,
                    },
                )
                .await
                .map(|r| UiPowerResult {
                    action,
                    cancelled: r.cancelled_by_host_user,
                    executed: r.executed,
                    message: r.message,
                }),
        )
    }

    pub async fn power_cancel(self: &Arc<Self>, device: String) -> Reply<bool> {
        Reply::from_core(
            self.node()
                .remote_power_cancel(&device)
                .await
                .map(|r| r.cancelled),
        )
    }

    pub async fn power_wake(self: &Arc<Self>, device: String) -> Reply<UiWakeResult> {
        Reply::from_core(self.node().wake_peer(&device).await.map(|r| UiWakeResult {
            local_packets: r.local_packets,
            relay_peers_notified: r.relay_peers_notified,
            relayed_packets: r.relayed_packets,
        }))
    }

    // ────────────────────────── 任务 3.6：交互式伪终端通道 ──────────────────────────

    pub async fn term_open(
        self: &Arc<Self>,
        device: String,
        shell: String,
        cols: u16,
        rows: u16,
        cwd: Option<String>,
    ) -> Reply<UiTermSession> {
        let sh = if shell.eq_ignore_ascii_case("cmd") {
            TermShell::Cmd
        } else {
            TermShell::PowerShell
        };
        let args = TermOpenArgs {
            shell: sh,
            cols: cols.clamp(20, 500),
            rows: rows.clamp(5, 300),
            cwd,
        };
        let res = if device == SELF_ID {
            self.node().open_local_terminal(args)
        } else {
            self.node().open_remote_terminal(&device, args).await
        };

        match res {
            Ok(mut handle) => {
                let session_id = format!("term-{}", self.next_seq());
                let pid = handle.pid;
                let shell_name = handle.shell_name.clone();
                let mut out_rx = std::mem::replace(
                    &mut handle.output_rx,
                    tokio::sync::mpsc::channel(1).1,
                );
                let handle_arc = Arc::new(handle);
                self.active_terminals
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(session_id.clone(), handle_arc);

                let sink = self.sink.clone();
                let sid = session_id.clone();
                let bridge_weak = Arc::downgrade(self);
                let shutdown = self.shutdown.clone();
                tokio::spawn(async move {
                    let mut buf: Vec<u8> = Vec::with_capacity(TERM_BATCH_MAX_BYTES);
                    let mut flush_deadline: Option<tokio::time::Instant> = None;

                    let flush = |buf: &mut Vec<u8>| {
                        if !buf.is_empty() {
                            let data_b64 = base64::engine::general_purpose::STANDARD.encode(&*buf);
                            buf.clear();
                            sink.emit_term_output(UiTermOutputEvent {
                                session_id: sid.clone(),
                                data_b64,
                            });
                        }
                    };

                    loop {
                        if let Some(deadline) = flush_deadline {
                            tokio::select! {
                                biased;
                                _ = shutdown.cancelled() => {
                                    break;
                                }
                                _ = tokio::time::sleep_until(deadline) => {
                                    flush(&mut buf);
                                    flush_deadline = None;
                                }
                                ev = out_rx.recv() => {
                                    match ev {
                                        Some(TerminalStreamEvent::Output(bytes)) => {
                                            if !bytes.is_empty() {
                                                buf.extend_from_slice(&bytes);
                                                if buf.len() >= TERM_BATCH_MAX_BYTES {
                                                    flush(&mut buf);
                                                    flush_deadline = None;
                                                }
                                            }
                                        }
                                        Some(TerminalStreamEvent::Exited(code)) => {
                                            flush(&mut buf);
                                            sink.emit_term_exit(UiTermExitEvent {
                                                session_id: sid.clone(),
                                                exit_code: code,
                                            });
                                            break;
                                        }
                                        None => {
                                            flush(&mut buf);
                                            break;
                                        }
                                    }
                                }
                            }
                        } else {
                            tokio::select! {
                                biased;
                                _ = shutdown.cancelled() => {
                                    break;
                                }
                                ev = out_rx.recv() => {
                                    match ev {
                                        Some(TerminalStreamEvent::Output(bytes)) => {
                                            if !bytes.is_empty() {
                                                buf.extend_from_slice(&bytes);
                                                if buf.len() >= TERM_BATCH_MAX_BYTES {
                                                    flush(&mut buf);
                                                } else {
                                                    flush_deadline =
                                                        Some(tokio::time::Instant::now() + TERM_BATCH_WINDOW);
                                                }
                                            }
                                        }
                                        Some(TerminalStreamEvent::Exited(code)) => {
                                            sink.emit_term_exit(UiTermExitEvent {
                                                session_id: sid.clone(),
                                                exit_code: code,
                                            });
                                            break;
                                        }
                                        None => break,
                                    }
                                }
                            }
                        }
                    }
                    if let Some(b) = bridge_weak.upgrade() {
                        b.active_terminals
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .remove(&sid);
                    }
                });

                Reply::ok(UiTermSession {
                    session_id,
                    pid,
                    shell_name,
                })
            }
            Err(err) => Reply::from_core(Err(err)),
        }
    }

    pub async fn term_write(&self, session_id: String, data_b64: String) -> Reply<()> {
        let handle_opt = self
            .active_terminals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&session_id)
            .cloned();
        let Some(handle) = handle_opt else {
            return Reply::err(UiErrorCode::NotFound, "终端会话已结束或不存在");
        };
        let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data_b64) else {
            return Reply::err(UiErrorCode::Invalid, "终端输入编码格式不正确");
        };
        Reply::from_core(handle.send_input(&bytes).await)
    }

    pub async fn term_resize(&self, session_id: String, cols: u16, rows: u16) -> Reply<()> {
        let handle_opt = self
            .active_terminals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&session_id)
            .cloned();
        let Some(handle) = handle_opt else {
            return Reply::ok(());
        };
        Reply::from_core(handle.resize(cols, rows).await)
    }

    pub async fn term_close(&self, session_id: String) -> Reply<()> {
        let removed = self
            .active_terminals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&session_id);
        if let Some(handle) = removed {
            let _ = handle.close().await;
        }
        Reply::ok(())
    }

    // ────────────────────────── 任务 3.7：剪贴板互通 ──────────────────────────

    pub async fn clip_push(self: &Arc<Self>, device: String) -> Reply<String> {
        if device == SELF_ID {
            return Reply::err(UiErrorCode::Invalid, "请选择一台已配对的远端电脑");
        }
        Reply::from_core(
            self.node()
                .push_local_clipboard_to_peer(&device)
                .await
                .map(|k| match k {
                    ClipKind::Text => "text".to_string(),
                    ClipKind::ImagePng => "image_png".to_string(),
                    ClipKind::Empty => "empty".to_string(),
                }),
        )
    }

    pub async fn clip_pull(self: &Arc<Self>, device: String) -> Reply<UiClipboardPreview> {
        if device == SELF_ID {
            return Reply::err(UiErrorCode::Invalid, "请选择一台已配对的远端电脑");
        }
        Reply::from_core(
            self.node()
                .pull_peer_clipboard_to_local(&device)
                .await
                .map(convert_remote_clipboard),
        )
    }

    pub async fn clip_peek(self: &Arc<Self>, device: String) -> Reply<UiClipboardPreview> {
        if device == SELF_ID {
            let plat = self.node().platform().clone();
            let res = tokio::task::spawn_blocking(move || {
                plat.get_clipboard()
                    .map_err(|e| CoreError::Rpc(map_platform_to_rpc(&e)))
            })
            .await
            .map_err(|e| CoreError::Internal(format!("读取本机剪贴板异常：{e}")))
            .and_then(|r| r);

            return Reply::from_core(res.map(|d| match d.content {
                ClipboardContent::Empty => UiClipboardPreview {
                    kind: "empty".to_string(),
                    text: None,
                    image_data_url: None,
                    width: None,
                    height: None,
                },
                ClipboardContent::Text(s) => UiClipboardPreview {
                    kind: "text".to_string(),
                    text: Some(s),
                    image_data_url: None,
                    width: None,
                    height: None,
                },
                ClipboardContent::ImagePng {
                    png_bytes,
                    width,
                    height,
                } => {
                    let b64 = base64::engine::general_purpose::STANDARD.encode(&png_bytes);
                    UiClipboardPreview {
                        kind: "image_png".to_string(),
                        text: None,
                        image_data_url: Some(format!("data:image/png;base64,{b64}")),
                        width: Some(width),
                        height: Some(height),
                    }
                }
            }));
        }

        Reply::from_core(
            self.node()
                .remote_get_clipboard(&device)
                .await
                .map(convert_remote_clipboard),
        )
    }

    // ────────────────────────── 任务 3.8：多设备并发批量执行 ──────────────────────────

    pub async fn batch_exec(
        self: &Arc<Self>,
        devices: Vec<String>,
        op: String,
        target: Option<String>,
        power_action: Option<String>,
        command: Option<String>,
        shell: Option<String>,
    ) -> Reply<Vec<UiBatchDeviceResult>> {
        let mut seen = HashSet::with_capacity(devices.len());
        let deduped_devices: Vec<String> = devices
            .into_iter()
            .filter(|id| !id.trim().is_empty() && seen.insert(id.clone()))
            .collect();
        if deduped_devices.is_empty() {
            return Reply::err(UiErrorCode::Invalid, "请至少勾选一台目标设备");
        }
        let batch_cmd = match op.as_str() {
            "launch_app" => {
                let Some(t) = target.filter(|s| !s.trim().is_empty()) else {
                    return Reply::err(UiErrorCode::Invalid, "请输入要批量打开的软件名或路径");
                };
                let kind = if t.contains('\\') || t.contains('/') || t.contains(':') {
                    LaunchKind::PathOrTarget
                } else if t.contains('!') {
                    LaunchKind::AppId
                } else {
                    LaunchKind::AppName
                };
                BatchCommand::LaunchApp {
                    target: t,
                    kind,
                    args: None,
                }
            }
            "power" => {
                let act = match power_action.as_deref().unwrap_or("lock") {
                    "lock" => PowerAction::Lock,
                    "sleep" => PowerAction::Sleep,
                    "restart" => PowerAction::Restart,
                    "shutdown" => PowerAction::Shutdown,
                    other => {
                        return Reply::err(
                            UiErrorCode::Invalid,
                            format!("不支持的批量电源动作：{other}"),
                        );
                    }
                };
                BatchCommand::Power {
                    action: act,
                    countdown_secs: None,
                }
            }
            "run_command" => {
                let Some(cmd_str) = command.filter(|s| !s.trim().is_empty()) else {
                    return Reply::err(UiErrorCode::Invalid, "请输入要批量执行的命令内容");
                };
                let sh = if shell.as_deref().is_some_and(|s| s.eq_ignore_ascii_case("cmd")) {
                    TermShell::Cmd
                } else {
                    TermShell::PowerShell
                };
                BatchCommand::RunCommand {
                    command: cmd_str,
                    shell: sh,
                    timeout_secs: 15,
                }
            }
            other => {
                return Reply::err(UiErrorCode::Invalid, format!("未知批量操作类型：{other}"));
            }
        };

        let sink = self.sink.clone();
        let results = self
            .node()
            .execute_batch(
                deduped_devices,
                batch_cmd,
                Some(Arc::new(move |r| {
                    sink.emit_batch_item(convert_batch_result(r));
                })),
            )
            .await
            .into_iter()
            .map(convert_batch_result)
            .collect();
        Reply::ok(results)
    }

    // ────────────────────────── 阶段 3 拖出文件到系统桌面/外部 ──────────────────────────

    pub async fn fs_drag_out(
        self: &Arc<Self>,
        device: String,
        paths: Vec<String>,
    ) -> Reply<bool> {
        if paths.is_empty() {
            return Reply::err(UiErrorCode::Invalid, "请先选择要拖出的文件");
        }
        let mut created_staging: Option<PathBuf> = None;
        let local_paths: Vec<PathBuf> = if device == SELF_ID {
            paths.into_iter().map(PathBuf::from).collect()
        } else {
            let data_dir = self.node().storage().data_dir().to_path_buf();
            let cleanup_dir = data_dir.clone();
            tokio::task::spawn_blocking(move || {
                cleanup_drag_out_staging(&cleanup_dir, Some(DRAG_OUT_STALE_AGE));
            });

            let staging = data_dir
                .join("drag_out_staging")
                .join(format!("drag-{}", fastrand::u64(..)));
            if let Err(e) = tokio::fs::create_dir_all(&staging).await {
                return Reply::err(UiErrorCode::Io, format!("创建临时拖出目录失败：{e}"));
            }
            // speed_limit_bps 留空以自动读取并动态跟随全局 RateLimiter 限速
            let opts = pcshare_core::TransferQueueOptions {
                conflict: ConflictPolicy::Overwrite,
                concurrency: 4,
                speed_limit_bps: None,
            };
            if let Err(err) = self
                .node()
                .download_paths_from_peer(
                    &device,
                    &paths,
                    &staging,
                    opts,
                    pcshare_core::TransferTaskControl::from_token(self.shutdown.child_token()),
                    None,
                )
                .await
            {
                let _ = tokio::fs::remove_dir_all(&staging).await;
                return Reply::from_core(Err(err));
            }
            let mut staged = Vec::new();
            if let Ok(mut rd) = tokio::fs::read_dir(&staging).await {
                while let Ok(Some(item)) = rd.next_entry().await {
                    staged.push(item.path());
                }
            }
            created_staging = Some(staging);
            staged
        };

        match self.node().platform().start_file_drag_out(&local_paths).await {
            Ok(dropped) => {
                if !dropped && let Some(staging) = created_staging {
                    let _ = tokio::fs::remove_dir_all(&staging).await;
                }
                Reply::ok(dropped)
            }
            Err(pe) => {
                if let Some(staging) = created_staging {
                    let _ = tokio::fs::remove_dir_all(&staging).await;
                }
                Reply::from_rpc(Err(map_platform_to_rpc(&pe)))
            }
        }
    }
}
