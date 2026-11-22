//! 快捷搜索框索引与极速启动（任务 3.1，`<= 300ms`）以及多设备并发批量操作（任务 3.8）。

use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use pcshare_proto::{
    AppLaunchArgs, AppLaunchReply, InstalledApp, LaunchKind, PowerAction, PowerExecArgs,
    TermOpenArgs, TermShell,
};
use serde::{Deserialize, Serialize};
use tokio::task::JoinSet;

use crate::CoreNode;
use crate::error::CoreError;
use crate::events::DeviceConnState;
use crate::pinyin::{CompiledQuery, match_rank};
use crate::storage::RecentFolderItem;
use crate::terminal::TerminalStreamEvent;

const MAX_RECENT_FOLDERS: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuickHitKind {
    App,
    Folder,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuickSearchHit {
    pub peer_id: String,
    pub peer_name: String,
    pub online: bool,
    pub kind: QuickHitKind,
    pub title: String,
    pub subtitle: String,
    pub target: String,
    pub launch_kind: LaunchKind,
    pub score: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum BatchCommand {
    LaunchApp {
        target: String,
        #[serde(default)]
        kind: LaunchKind,
        #[serde(default)]
        args: Option<String>,
    },
    Power {
        action: PowerAction,
        #[serde(default)]
        countdown_secs: Option<u32>,
    },
    RunCommand {
        command: String,
        #[serde(default)]
        shell: TermShell,
        #[serde(default = "default_batch_cmd_timeout")]
        timeout_secs: u64,
    },
}

fn default_batch_cmd_timeout() -> u64 {
    10
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchDeviceResult {
    pub peer_id: String,
    pub peer_name: String,
    pub ok: bool,
    pub summary: String,
    pub output: Option<String>,
    pub elapsed_ms: u64,
}

/// 解析快捷搜索框输入 `设备名 关键字`（任务 3.1）。
/// 若首词能匹配某台设备名（含拼音首字母），则限定到该设备并用剩余部分作关键字；否则返回 `(None, 原查询)`。
pub fn parse_device_and_keyword<'a>(
    raw: &'a str,
    devices: &[(String, String)],
) -> (Option<String>, &'a str) {
    let trimmed = raw.trim();
    if let Some((first, rest)) = trimmed.split_once(char::is_whitespace) {
        let rest_trimmed = rest.trim();
        let mut best: Option<(u32, &str)> = None;
        for (id, name) in devices {
            if let Some(rank) = match_rank(name, first)
                && best.is_none_or(|(b_rank, _)| rank < b_rank)
            {
                best = Some((rank, id.as_str()));
            }
        }
        if let Some((_, matched_id)) = best {
            return (Some(matched_id.to_string()), rest_trimmed);
        }
    }
    (None, trimmed)
}

impl CoreNode {
    pub fn record_recent_folder(&self, peer_id: &str, peer_name: &str, path: &str) {
        let clean = path.trim();
        if clean.is_empty() {
            return;
        }
        let folder_name = clean
            .trim_end_matches(['\\', '/'])
            .rsplit(['\\', '/'])
            .find(|s| !s.is_empty())
            .unwrap_or(clean)
            .to_string();
        let now_ms = Utc::now().timestamp_millis() as u64;
        let _ = self.storage.update_settings(|s| {
            s.recent_folders
                .retain(|f| !(f.peer_id == peer_id && f.path.eq_ignore_ascii_case(clean)));
            s.recent_folders.insert(
                0,
                RecentFolderItem {
                    peer_id: peer_id.to_string(),
                    peer_name: peer_name.to_string(),
                    path: clean.to_string(),
                    name: folder_name,
                    opened_at_ms: now_ms,
                },
            );
            if s.recent_folders.len() > MAX_RECENT_FOLDERS {
                s.recent_folders.truncate(MAX_RECENT_FOLDERS);
            }
        });
    }

    pub fn cache_peer_apps(&self, peer_id: &str, apps: Vec<InstalledApp>) {
        self.apps_index_mutex()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(peer_id.to_string(), apps);
    }

    /// 跨设备快捷搜索（任务 3.1：支持 `设备名 关键字`、拼音首字母、单台在线省略设备名、合并软件与最近文件夹）。
    pub fn quick_search(&self, query: &str, limit: usize) -> Vec<QuickSearchHit> {
        let peers = self.storage.list_peers();
        let states = self.all_peer_states();
        let mut device_pairs: Vec<(String, String)> = peers
            .iter()
            .map(|p| (p.endpoint_id.clone(), p.name.clone()))
            .collect();
        let self_name = self.storage.settings().device_name;
        device_pairs.push(("self".to_string(), self_name.clone()));

        let (scoped_peer, keyword) = parse_device_and_keyword(query, &device_pairs);
        let compiled_kw = CompiledQuery::new(keyword);

        let is_online = |pid: &str| -> bool {
            if pid == "self" {
                return true;
            }
            self.has_active_conn(pid)
                || matches!(
                    states.get(pid),
                    Some(
                        DeviceConnState::OnlineDirect { .. }
                            | DeviceConnState::OnlineRelay { .. }
                    )
                )
        };

        let online_remote_ids: Vec<&str> = peers
            .iter()
            .filter(|p| is_online(&p.endpoint_id))
            .map(|p| p.endpoint_id.as_str())
            .collect();
        let single_online_remote = if online_remote_ids.len() == 1 {
            Some(online_remote_ids[0])
        } else {
            None
        };

        let mut hits = Vec::new();

        // 1. Search cached apps per device
        let apps_snapshot = self
            .apps_index_mutex()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();

        for (pid, pname) in &device_pairs {
            if let Some(ref target_id) = scoped_peer
                && pid != target_id
            {
                continue;
            }
            let online = is_online(pid);
            if let Some(apps) = apps_snapshot.get(pid) {
                for app in apps {
                    if let Some(rank) = compiled_kw.match_rank(&app.name) {
                        let bonus = if single_online_remote == Some(pid.as_str()) {
                            0
                        } else if online {
                            5
                        } else {
                            50
                        };
                        hits.push(QuickSearchHit {
                            peer_id: pid.clone(),
                            peer_name: pname.clone(),
                            online,
                            kind: QuickHitKind::App,
                            title: app.name.clone(),
                            subtitle: format!("{pname} · 已装软件"),
                            target: app.app_id.clone(),
                            launch_kind: LaunchKind::AppId,
                            score: rank * 10 + bonus,
                        });
                    }
                }
            }
        }

        // 2. Search recent folders across devices
        for rf in self.storage.settings().recent_folders {
            if let Some(ref target_id) = scoped_peer
                && &rf.peer_id != target_id
            {
                continue;
            }
            let rank_opt = compiled_kw
                .match_rank(&rf.name)
                .or_else(|| compiled_kw.match_rank(&rf.path));
            if let Some(rank) = rank_opt {
                let online = is_online(&rf.peer_id);
                let bonus = if single_online_remote == Some(rf.peer_id.as_str()) {
                    2
                } else if online {
                    7
                } else {
                    52
                };
                hits.push(QuickSearchHit {
                    peer_id: rf.peer_id,
                    peer_name: rf.peer_name.clone(),
                    online,
                    kind: QuickHitKind::Folder,
                    title: rf.name,
                    subtitle: format!("{} · {}", rf.peer_name, rf.path),
                    target: rf.path,
                    launch_kind: LaunchKind::PathOrTarget,
                    score: rank * 10 + bonus,
                });
            }
        }

        hits.sort_by(|a, b| a.score.cmp(&b.score).then_with(|| a.title.cmp(&b.title)));
        hits.truncate(limit.max(1));
        hits
    }

    /// 快捷搜索框按下回车极速启动（跳过 6 秒窗口等待，确保远端启动耗时 <= 300ms，任务 3.1）。
    pub async fn quick_launch_target(
        self: &Arc<Self>,
        peer_id: &str,
        target: &str,
        kind: LaunchKind,
    ) -> Result<AppLaunchReply, CoreError> {
        let req = AppLaunchArgs {
            target: target.to_string(),
            kind,
            args: None,
            skip_window_wait: true,
        };
        if peer_id == "self" {
            self.local_launch(req).await
        } else {
            self.remote_launch(peer_id, req).await
        }
    }

    /// 多设备并发批量执行（任务 3.8：同时发给多台已配对设备，按台返回执行结果）。
    pub async fn execute_batch(
        self: &Arc<Self>,
        peer_ids: Vec<String>,
        command: BatchCommand,
        on_item: Option<Arc<dyn Fn(BatchDeviceResult) + Send + Sync>>,
    ) -> Vec<BatchDeviceResult> {
        let mut set = JoinSet::new();
        for pid in peer_ids {
            let node = self.clone();
            let cmd = command.clone();
            let cb = on_item.clone();
            set.spawn(async move {
                let peer_name = if pid == "self" {
                    node.storage.settings().device_name
                } else {
                    node.storage
                        .get_peer(&pid)
                        .map(|p| p.name)
                        .unwrap_or_else(|| pid.chars().take(8).collect())
                };
                let started = Instant::now();
                let res = run_single_batch_item(&node, &pid, cmd).await;
                let elapsed_ms = started.elapsed().as_millis() as u64;
                let item = match res {
                    Ok((ok, summary, output)) => BatchDeviceResult {
                        peer_id: pid,
                        peer_name,
                        ok,
                        summary,
                        output,
                        elapsed_ms,
                    },
                    Err(err) => BatchDeviceResult {
                        peer_id: pid,
                        peer_name,
                        ok: false,
                        summary: err.to_user_message(),
                        output: None,
                        elapsed_ms,
                    },
                };
                if let Some(ref f) = cb {
                    f(item.clone());
                }
                item
            });
        }

        let mut out = Vec::new();
        while let Some(joined) = set.join_next().await {
            if let Ok(item) = joined {
                out.push(item);
            }
        }
        out.sort_by(|a, b| a.peer_name.cmp(&b.peer_name));
        out
    }
}

async fn run_single_batch_item(
    node: &Arc<CoreNode>,
    peer_id: &str,
    cmd: BatchCommand,
) -> Result<(bool, String, Option<String>), CoreError> {
    match cmd {
        BatchCommand::LaunchApp { target, kind, args } => {
            let req = AppLaunchArgs {
                target: target.clone(),
                kind,
                args,
                skip_window_wait: true,
            };
            if peer_id == "self" {
                node.local_launch(req).await?;
            } else {
                node.remote_launch(peer_id, req).await?;
            }
            Ok((true, format!("已发送打开指令：{target}"), None))
        }
        BatchCommand::Power {
            action,
            countdown_secs,
        } => {
            let req_name = node.storage.settings().device_name;
            let reply = node
                .remote_power_exec(
                    peer_id,
                    PowerExecArgs {
                        action,
                        countdown_secs,
                        requester_name: req_name,
                    },
                )
                .await?;
            if reply.cancelled_by_host_user {
                Ok((
                    true,
                    "对方屏幕前用户取消了本次电源操作".to_string(),
                    None,
                ))
            } else {
                Ok((
                    true,
                    reply.message.unwrap_or_else(|| "电源指令已执行".to_string()),
                    None,
                ))
            }
        }
        BatchCommand::RunCommand {
            command,
            shell,
            timeout_secs,
        } => {
            let open_args = TermOpenArgs {
                shell,
                cols: 120,
                rows: 32,
                cwd: None,
            };
            let mut term = if peer_id == "self" {
                node.open_local_terminal(open_args)?
            } else {
                node.open_remote_terminal(peer_id, open_args).await?
            };

            let script = match shell {
                TermShell::PowerShell => format!(
                    "$global:LASTEXITCODE = 0\r\n{command}\r\n$__pcshare_ok = $?\r\n$__pcshare_ec = $LASTEXITCODE\r\nif (-not $__pcshare_ok -and $__pcshare_ec -eq 0) {{ $__pcshare_ec = 1 }}\r\nexit $__pcshare_ec\r\n"
                ),
                TermShell::Cmd => format!("{command}\r\nexit %ERRORLEVEL%\r\n"),
            };
            term.send_input(script.as_bytes()).await?;

            let mut buf = Vec::new();
            let effective_timeout = timeout_secs.clamp(2, 120);
            let deadline = Instant::now() + Duration::from_secs(effective_timeout);
            let mut exited = false;
            let mut exit_code: Option<i32> = None;

            while let Some(rem) = deadline.checked_duration_since(Instant::now()) {
                if rem.is_zero() {
                    break;
                }
                let step = rem.min(Duration::from_millis(300));
                match tokio::time::timeout(step, term.output_rx.recv()).await {
                    Ok(Some(TerminalStreamEvent::Output(chunk))) => {
                        if buf.len() < 32 * 1024 {
                            let take = (32 * 1024 - buf.len()).min(chunk.len());
                            buf.extend_from_slice(&chunk[..take]);
                        }
                    }
                    Ok(Some(TerminalStreamEvent::Exited(code))) => {
                        exited = true;
                        exit_code = code;
                        break;
                    }
                    Ok(None) => {
                        exited = true;
                        break;
                    }
                    Err(_) => {}
                }
            }
            let _ = term.close().await;
            let text = strip_ansi_escapes(&String::from_utf8_lossy(&buf));
            if !exited {
                return Ok((
                    false,
                    format!("命令执行超时（超过 {effective_timeout} 秒）"),
                    Some(text),
                ));
            }
            if let Some(code) = exit_code
                && code != 0
            {
                return Ok((
                    false,
                    format!("命令执行失败（退出码 {code}）"),
                    Some(text),
                ));
            }
            Ok((true, "命令执行完成".to_string(), Some(text)))
        }
    }
}

fn strip_ansi_escapes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                let _ = chars.next();
                for ch in chars.by_ref() {
                    if ('@'..='~').contains(&ch) {
                        break;
                    }
                }
            } else if chars.peek() == Some(&']') {
                let _ = chars.next();
                while let Some(ch) = chars.next() {
                    if ch == '\x07' {
                        break;
                    }
                    if ch == '\x1b' && chars.peek() == Some(&'\\') {
                        let _ = chars.next();
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out.trim().to_string()
}
