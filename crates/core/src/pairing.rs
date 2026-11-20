//! 设备配对流程状态机（任务 1.6 与总体方案 7.2 节）：
//! - 6 位数字配对码，5 分钟有效期
//! - 输错 5 次立即作废（常量时间比较）
//! - 校验通过后必须等待被控端用户人工点“允许”才写入已配对名单

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use chrono::Utc;
use constant_time_eq::constant_time_eq;
use iroh::{EndpointId, SecretKey};
use pcshare_proto::{ErrorCode, RpcError};
use tokio::sync::oneshot;
use url::Url;

use crate::error::CoreError;
use crate::events::{PairingDecision, PairingRequestPrompt, PairingSessionTicket};

const PAIRING_TTL: Duration = Duration::from_secs(5 * 60);
const MAX_FAILED_ATTEMPTS: u32 = 5;
const MAX_PENDING_APPROVALS: usize = 32;

/// 使用操作系统密码学安全随机源生成 32 字节随机数（复用 `iroh::SecretKey::generate` 底层 CSPRNG）。
pub(crate) fn csprng_bytes_32() -> [u8; 32] {
    SecretKey::generate().to_bytes()
}

/// 生成 256-bit（64 位小写十六进制）密码学安全随机令牌。
pub(crate) fn csprng_token_hex_64() -> String {
    let bytes = csprng_bytes_32();
    let mut out = String::with_capacity(64);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

/// 生成 6 位数字配对码（OS CSPRNG + 拒绝采样消除取模偏差）。
fn generate_pairing_code() -> String {
    const BOUND: u32 = 1_000_000;
    const LIMIT: u32 = u32::MAX - (u32::MAX % BOUND);
    loop {
        let bytes = csprng_bytes_32();
        for &chunk in bytes.as_chunks::<4>().0 {
            let v = u32::from_le_bytes(chunk);
            if v < LIMIT {
                return format!("{:06}", v % BOUND);
            }
        }
    }
}

fn generate_request_id() -> String {
    let bytes = csprng_bytes_32();
    let id = u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ]);
    format!("pair-{id:016x}")
}

struct PendingApproval {
    tx: oneshot::Sender<PairingDecision>,
    expires_at: Instant,
}

struct ActivePairingSession {
    code: String,
    expires_at: Instant,
    expires_at_ms: u64,
    failed_attempts: u32,
}

#[derive(Debug, Clone)]
pub struct ParsedInvite {
    pub endpoint_id: EndpointId,
    pub code: Option<String>,
    pub relay_url: Option<String>,
    pub direct_addrs: Vec<SocketAddr>,
}

/// 解析邀请码（`pcshare://<id>?code=123456&relay=...&addr=...` 或 `pcshare:<id>:<code>`）或纯 64 位身份码。
pub fn parse_invite_or_id(input: &str) -> Result<ParsedInvite, CoreError> {
    let trimmed = input.trim();
    if trimmed.starts_with("pcshare://") {
        let parsed = Url::parse(trimmed)
            .map_err(|e| CoreError::InvalidArgument(format!("邀请码格式不正确：{e}")))?;
        let id_str = parsed.host_str().unwrap_or("");
        let endpoint_id = id_str
            .parse::<EndpointId>()
            .map_err(|_| CoreError::InvalidArgument(format!("邀请码中的身份码无效：{id_str}")))?;
        let mut code = None;
        let mut relay_url = None;
        let mut direct_addrs = Vec::new();
        for (k, v) in parsed.query_pairs() {
            match k.as_ref() {
                "code" => code = Some(v.into_owned()),
                "relay" => relay_url = Some(v.into_owned()),
                "addr" => {
                    if let Ok(a) = v.parse::<SocketAddr>() {
                        direct_addrs.push(a);
                    }
                }
                _ => {}
            }
        }
        Ok(ParsedInvite {
            endpoint_id,
            code,
            relay_url,
            direct_addrs,
        })
    } else if let Some(rest) = trimmed.strip_prefix("pcshare:") {
        let mut parts = rest.split(':');
        let id_str = parts.next().unwrap_or("");
        let code_str = parts.next().filter(|s| !s.is_empty()).map(ToOwned::to_owned);
        let endpoint_id = id_str
            .parse::<EndpointId>()
            .map_err(|_| CoreError::InvalidArgument(format!("邀请码中的身份码无效：{id_str}")))?;
        Ok(ParsedInvite {
            endpoint_id,
            code: code_str,
            relay_url: None,
            direct_addrs: Vec::new(),
        })
    } else {
        let endpoint_id = trimmed
            .parse::<EndpointId>()
            .map_err(|_| CoreError::InvalidArgument(format!("身份码格式不正确（应为 64 位字符）：{trimmed}")))?;
        Ok(ParsedInvite {
            endpoint_id,
            code: None,
            relay_url: None,
            direct_addrs: Vec::new(),
        })
    }
}

pub fn format_invite_code(
    endpoint_id: EndpointId,
    code: &str,
    relay_url: Option<&str>,
    direct_addrs: &[SocketAddr],
) -> String {
    let mut url = Url::parse(&format!("pcshare://{endpoint_id}")).expect("valid invite url");
    {
        let mut qp = url.query_pairs_mut();
        qp.append_pair("code", code);
        if let Some(relay) = relay_url {
            qp.append_pair("relay", relay);
        }
        for addr in direct_addrs.iter().take(4) {
            qp.append_pair("addr", &addr.to_string());
        }
    }
    url.to_string()
}

pub struct PairingManager {
    session: Mutex<Option<ActivePairingSession>>,
    pending_approvals: Mutex<HashMap<String, PendingApproval>>,
}

impl Default for PairingManager {
    fn default() -> Self {
        Self::new()
    }
}

impl PairingManager {
    pub fn new() -> Self {
        Self {
            session: Mutex::new(None),
            pending_approvals: Mutex::new(HashMap::new()),
        }
    }

    pub fn start_session(
        &self,
        endpoint_id: EndpointId,
        relay_url: Option<&str>,
        direct_addrs: &[SocketAddr],
    ) -> PairingSessionTicket {
        // 重新开启配对会话时作废旧会话遗留的待审批项
        self.clear_pending_approvals();

        let code = generate_pairing_code();
        let expires_at = Instant::now() + PAIRING_TTL;
        let expires_at_ms = (Utc::now().timestamp_millis() as u64)
            .saturating_add(PAIRING_TTL.as_millis() as u64);
        let invite_code = format_invite_code(endpoint_id, &code, relay_url, direct_addrs);

        let mut guard = self.session.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(ActivePairingSession {
            code: code.clone(),
            expires_at,
            expires_at_ms,
            failed_attempts: 0,
        });

        PairingSessionTicket {
            endpoint_id: endpoint_id.to_string(),
            code,
            expires_at_ms,
            invite_code,
        }
    }

    /// 核验对端发来的 6 位配对码。如果正确，返回人工审批提示与等待通道；
    /// 如果错误累计达 5 次，立即作废当前配对码。
    pub fn verify_code_and_prepare_approval(
        &self,
        peer_id: EndpointId,
        peer_name: String,
        peer_os: String,
        mutual: bool,
        submitted_code: &str,
    ) -> Result<(PairingRequestPrompt, oneshot::Receiver<PairingDecision>), RpcError> {
        let (expires_at, expires_at_ms) = {
            let mut guard = self.session.lock().unwrap_or_else(|e| e.into_inner());
            let Some(active) = guard.as_mut() else {
                return Err(RpcError::new(
                    ErrorCode::Unauthorized,
                    "对方尚未开启配对模式，请在被控电脑上点击“添加设备”生成配对码",
                ));
            };

            if Instant::now() >= active.expires_at {
                *guard = None;
                return Err(RpcError::new(
                    ErrorCode::Unauthorized,
                    "配对码已超过 5 分钟有效期，请在被控电脑上重新生成",
                ));
            }

            let trimmed = submitted_code.trim();
            if !constant_time_eq(active.code.as_bytes(), trimmed.as_bytes()) {
                active.failed_attempts += 1;
                if active.failed_attempts >= MAX_FAILED_ATTEMPTS {
                    *guard = None;
                    return Err(RpcError::new(
                        ErrorCode::Unauthorized,
                        "配对码连续输错 5 次，已立即作废，请在被控电脑上重新生成",
                    ));
                }
                let left = MAX_FAILED_ATTEMPTS - active.failed_attempts;
                return Err(RpcError::new(
                    ErrorCode::Unauthorized,
                    format!("配对码不正确，还剩 {left} 次尝试机会"),
                ));
            }

            (active.expires_at, active.expires_at_ms)
        };

        let peer_hex = peer_id.to_string();
        let short_id = if peer_hex.len() >= 8 {
            peer_hex[peer_hex.len() - 8..].to_string()
        } else {
            peer_hex.clone()
        };
        let request_id = generate_request_id();
        let prompt = PairingRequestPrompt {
            request_id: request_id.clone(),
            peer_id: peer_hex,
            peer_short_id: short_id,
            peer_name,
            peer_os,
            mutual,
            expires_at_ms,
        };

        let (tx, rx) = oneshot::channel();
        {
            let now = Instant::now();
            let mut pending = self
                .pending_approvals
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            pending.retain(|_, p| p.expires_at > now && !p.tx.is_closed());
            if pending.len() >= MAX_PENDING_APPROVALS {
                return Err(RpcError::new(
                    ErrorCode::Busy,
                    "当前待审批的配对请求过多，请稍后再试",
                ));
            }
            pending.insert(request_id, PendingApproval { tx, expires_at });
        }

        Ok((prompt, rx))
    }

    pub fn remove_pending(&self, request_id: &str) {
        self.pending_approvals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(request_id);
    }

    fn clear_pending_approvals(&self) {
        let old = {
            let mut pending = self
                .pending_approvals
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut *pending)
        };
        for (_, item) in old {
            let _ = item.tx.send(PairingDecision::Reject);
        }
    }

    pub fn consume_session_on_approved(&self) {
        let mut guard = self.session.lock().unwrap_or_else(|e| e.into_inner());
        *guard = None;
    }

    pub fn cancel_session(&self) {
        {
            let mut guard = self.session.lock().unwrap_or_else(|e| e.into_inner());
            *guard = None;
        }
        self.clear_pending_approvals();
    }

    pub fn respond(&self, request_id: &str, decision: PairingDecision) -> Result<(), CoreError> {
        let now = Instant::now();
        let entry = {
            let mut pending = self
                .pending_approvals
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            pending.retain(|k, p| k == request_id || (p.expires_at > now && !p.tx.is_closed()));
            pending.remove(request_id)
        }
        .ok_or_else(|| {
            CoreError::InvalidArgument(format!("配对请求已过期或不存在：{request_id}"))
        })?;
        if now >= entry.expires_at {
            return Err(CoreError::InvalidArgument(format!(
                "配对请求已超时过期：{request_id}"
            )));
        }
        let _ = entry.tx.send(decision);
        Ok(())
    }
}
