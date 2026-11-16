//! 任务 2.11：内置网络体检（移植自 `probe`）、按天滚动运行日志、连接线路汇总与诊断包导出。

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{Duration as ChronoDuration, Local, NaiveDate, Utc};
use iroh::endpoint::presets;
use iroh::unstable_net_report::Probe;
use iroh::{Endpoint, RelayMode, RelayUrl, Watcher};
use serde::{Deserialize, Serialize};
use tokio::time::timeout;

use crate::error::CoreError;
use crate::events::DeviceConnState;
use crate::storage::{AuditEntry, write_json_atomic};

const RUNTIME_LOG_PREFIX: &str = "runtime-";
const RETENTION_DAYS: i64 = 30;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayLatencyItem {
    pub relay_host: String,
    pub https_ms: Option<u64>,
    pub ipv4_ms: Option<u64>,
    pub ipv6_ms: Option<u64>,
    pub preferred: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetcheckReport {
    pub checked_at_ms: u64,
    pub relay_online: bool,
    pub udp_v4: bool,
    pub udp_v6: bool,
    pub nat_status: String,
    pub masked_global_v4: Option<String>,
    pub masked_global_v6: Option<String>,
    pub captive_portal: bool,
    pub relays: Vec<RelayLatencyItem>,
    pub summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticPeerRoute {
    pub peer_id: String,
    pub name: String,
    pub state: DeviceConnState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticSummary {
    pub exported_at_ms: u64,
    pub app_version: String,
    pub endpoint_id: String,
    pub device_name: String,
    pub os: String,
    pub working_set_bytes: u64,
    pub host_paused: bool,
    pub custom_relays: Vec<String>,
    pub peer_routes: Vec<DiagnosticPeerRoute>,
    pub audit_count: usize,
}

pub fn append_runtime_log(logs_dir: &Path, level: &str, msg: &str) {
    let _ = fs::create_dir_all(logs_dir);
    let today = Local::now().format("%Y-%m-%d").to_string();
    let path = logs_dir.join(format!("{RUNTIME_LOG_PREFIX}{today}.log"));
    let ts = Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "[{ts}] [{level}] {msg}");
    }
    prune_old_runtime_logs(logs_dir);
}

pub fn read_recent_runtime_logs(logs_dir: &Path, limit: usize) -> Vec<String> {
    let Ok(rd) = fs::read_dir(logs_dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(RUNTIME_LOG_PREFIX) && n.ends_with(".log"))
        })
        .collect();
    files.sort();

    let mut lines = Vec::new();
    for p in files.into_iter().rev() {
        if let Ok(f) = fs::File::open(&p) {
            let mut file_lines: Vec<String> =
                BufReader::new(f).lines().map_while(Result::ok).collect();
            file_lines.reverse();
            for line in file_lines {
                lines.push(line);
                if lines.len() >= limit {
                    lines.reverse();
                    return lines;
                }
            }
        }
    }
    lines.reverse();
    lines
}

fn prune_old_runtime_logs(logs_dir: &Path) {
    let cutoff = Local::now().date_naive() - ChronoDuration::days(RETENTION_DAYS);
    let Ok(rd) = fs::read_dir(logs_dir) else {
        return;
    };
    for item in rd.flatten() {
        let name = item.file_name().to_string_lossy().into_owned();
        if let Some(date_part) = name
            .strip_prefix(RUNTIME_LOG_PREFIX)
            .and_then(|s| s.strip_suffix(".log"))
            && let Ok(d) = NaiveDate::parse_from_str(date_part, "%Y-%m-%d")
            && d < cutoff
        {
            let _ = fs::remove_file(item.path());
        }
    }
}

/// 执行网络体检（仅在用户明确点击时调用；会连接 iroh 公共中转或自定义中转服务器）。
pub async fn run_netcheck(custom_relay_urls: &[String]) -> Result<NetcheckReport, CoreError> {
    let mut builder = Endpoint::builder(presets::N0);
    if !custom_relay_urls.is_empty() {
        let mut parsed_urls = Vec::new();
        for u in custom_relay_urls {
            let p: RelayUrl = u
                .parse()
                .map_err(|e| CoreError::InvalidArgument(format!("中转服务器地址无效“{u}”：{e}")))?;
            parsed_urls.push(p);
        }
        builder = builder.relay_mode(RelayMode::custom(parsed_urls));
    }
    let endpoint = builder
        .bind()
        .await
        .map_err(|e| CoreError::Internal(format!("无法启动网络探测端点：{e}")))?;

    let mut watcher = endpoint.net_report();
    let report_opt = timeout(Duration::from_secs(18), watcher.initialized())
        .await
        .ok();
    let relay_online = timeout(Duration::from_secs(8), endpoint.online())
        .await
        .is_ok();
    endpoint.close().await;

    let checked_at_ms = Utc::now().timestamp_millis() as u64;
    let Some(report) = report_opt else {
        return Ok(NetcheckReport {
            checked_at_ms,
            relay_online,
            udp_v4: false,
            udp_v6: false,
            nat_status: "未能完成探测".to_string(),
            masked_global_v4: None,
            masked_global_v6: None,
            captive_portal: false,
            relays: Vec::new(),
            summary: "没有拿到探测结果：当前网络可能无法连接中转服务器，请检查网络或代理设置。".to_string(),
        });
    };

    let nat_status = match report.mapping_varies_by_dest_ipv4 {
        Some(false) => "对直连友好（打洞成功率较高）".to_string(),
        Some(true) => "端口随目标变化（跨网可能改走 IPv6 或中转）".to_string(),
        None => "未知".to_string(),
    };
    let masked_global_v4 = report.global_v4.map(|a| mask_ip(IpAddr::V4(*a.ip())));
    let masked_global_v6 = report.global_v6.map(|a| mask_ip(IpAddr::V6(*a.ip())));
    let captive_portal = report.captive_portal == Some(true);
    let preferred_str = report.preferred_relay.as_ref().map(|u| u.to_string());

    let mut table: BTreeMap<String, [Option<u64>; 3]> = BTreeMap::new();
    for (probe, url, latency) in report.relay_latency.iter() {
        let col = match probe {
            Probe::Https => 0,
            Probe::QadIpv4 => 1,
            Probe::QadIpv6 => 2,
            _ => continue,
        };
        let ms = latency.as_millis() as u64;
        let slot = &mut table.entry(url.to_string()).or_default()[col];
        *slot = Some(slot.map_or(ms, |old| old.min(ms)));
    }

    let relays: Vec<RelayLatencyItem> = table
        .into_iter()
        .map(|(url_str, cols)| {
            let preferred = preferred_str.as_deref() == Some(url_str.as_str());
            let host = url_str
                .trim_start_matches("https://")
                .trim_start_matches("http://")
                .trim_end_matches('/')
                .trim_end_matches('.')
                .to_string();
            RelayLatencyItem {
                relay_host: host,
                https_ms: cols[0],
                ipv4_ms: cols[1],
                ipv6_ms: cols[2],
                preferred,
            }
        })
        .collect();

    let summary = if relay_online && (report.udp_v4 || report.udp_v6) {
        "网络连接正常：可以建立连接并尝试打洞直连。".to_string()
    } else if relay_online {
        "已连上中转服务器，但 UDP 被限制，跨网可能主要通过中转通信。".to_string()
    } else {
        "未能连上中转服务器：同局域网仍可直连，跨网络建议检查防火墙或配置自建中转。".to_string()
    };

    Ok(NetcheckReport {
        checked_at_ms,
        relay_online,
        udp_v4: report.udp_v4,
        udp_v6: report.udp_v6,
        nat_status,
        masked_global_v4,
        masked_global_v6,
        captive_portal,
        relays,
        summary,
    })
}

pub fn write_diagnostic_bundle(
    dest_parent: &Path,
    summary: &DiagnosticSummary,
    runtime_logs: &[String],
    audits: &[AuditEntry],
) -> Result<PathBuf, CoreError> {
    let stamp = Local::now().format("%Y%m%d-%H%M%S").to_string();
    let bundle_dir = dest_parent.join(format!("电脑共享-诊断包-{stamp}"));
    fs::create_dir_all(&bundle_dir)?;

    write_json_atomic(&bundle_dir.join("summary.json"), summary)?;
    write_json_atomic(&bundle_dir.join("audit_recent.json"), &audits)?;

    let mut log_file = fs::File::create(bundle_dir.join("runtime_recent.log"))?;
    for line in runtime_logs {
        writeln!(log_file, "{line}")?;
    }
    log_file.sync_all()?;

    Ok(bundle_dir)
}

fn mask_ip(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            format!("{}.{}.*.*", o[0], o[1])
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            format!("{:x}:{:x}:*:*", s[0], s[1])
        }
    }
}
