//! 电脑共享 · 技术验证工具（第 0 阶段）
//!
//! 一个程序，两种身份：
//!
//! - 被控端：`pcshare-probe serve --allow <控制端身份码>`
//!   直接双击运行时，读取同目录下的 `allow.txt` 作为允许名单。
//! - 控制端：`pcshare-probe <指令> <被控端身份码> [参数]`
//!
//! 只用来回答两个问题：远程打开的软件有没有窗口；不同网络的电脑能不能直连。
//! 正式版的核心库会重写，这里的代码不追求完整。

mod win;

use std::collections::{BTreeMap, HashSet};
use std::io::Write as _;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use iroh::endpoint::{Connection, RecvStream, SendStream, presets};
use iroh::unstable_net_report::Probe;
use iroh::{Endpoint, EndpointId, RelayUrl, SecretKey, TransportAddr, Watcher};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::time::timeout;

/// 协议名。两端必须一致，握手时会核对。
const ALPN: &[u8] = b"pcshare/probe/0";
/// 单条指令或回复的最大字节数。
const MAX_FRAME: usize = 16 * 1024 * 1024;
/// 测速时每次读写的块大小。
const CHUNK: usize = 64 * 1024;
/// 单次测速最多传多少字节，防止写错参数把流量跑光。
const MAX_TRANSFER: u64 = 1024 * 1024 * 1024;
const CLOSE_OK: u32 = 0;
const CLOSE_UNAUTHORIZED: u32 = 401;

// ───────────────────────────── 两端之间的约定 ─────────────────────────────

/// 控制端发给被控端的指令。每条指令占用一条独立的双向数据流。
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
enum Request {
    Ping,
    Info,
    /// 列出文件夹；`path` 为空表示列出盘符。
    Ls { path: String },
    /// 用系统的“打开”动作启动程序、文件或快捷方式。
    Open { target: String, args: Option<String> },
    /// 列出开始菜单里的软件。
    Apps,
    /// 按名字打开开始菜单里的软件。
    OpenApp { name: String },
    /// 被控端向控制端发送数据，用于测下载速度。
    Download { bytes: u64, max_millis: u64 },
    /// 控制端向被控端发送数据，用于测上传速度。
    Upload,
}

/// 回复：成功带数据，失败带一句给人看的原因。
type Reply<T> = std::result::Result<T, String>;

fn ok<T>(value: T) -> Reply<T> {
    Ok(value)
}

/// 被控端所处的登录环境，决定了打开的软件有没有窗口。
#[derive(Debug, Serialize, Deserialize)]
struct Desktop {
    session_id: u32,
    console_session_id: u32,
    elevated: bool,
    input_desktop: Option<String>,
}

impl Desktop {
    fn current() -> Self {
        let info = win::session_info();
        Self {
            session_id: info.session_id,
            console_session_id: info.console_session_id,
            elevated: info.elevated,
            input_desktop: info.input_desktop,
        }
    }

    fn summary(&self) -> String {
        let place = if self.session_id == 0 {
            "系统服务环境（这里打开的软件不会有窗口）"
        } else if self.input_desktop.is_none() {
            "已登录，但当前是锁屏或权限确认界面"
        } else if self.session_id != self.console_session_id {
            "已登录，但不是接显示器的那个会话（可能是远程桌面）"
        } else {
            "正常桌面"
        };
        let rights = if self.elevated { "管理员权限" } else { "普通权限" };
        format!("{place}，{rights}")
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct InfoReply {
    computer: String,
    user: String,
    desktop: Desktop,
    version: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct Entry {
    name: String,
    dir: bool,
    size: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct LsReply {
    path: String,
    entries: Vec<Entry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct OpenReply {
    /// 打开之后新出现的窗口标题。
    new_windows: Vec<String>,
    desktop: Desktop,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct App {
    name: String,
    id: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct OpenAppReply {
    name: String,
    opened: OpenReply,
}

#[derive(Debug, Serialize, Deserialize)]
struct UploadReply {
    bytes: u64,
}

async fn write_frame<T: Serialize>(send: &mut SendStream, value: &T) -> Result<()> {
    let body = serde_json::to_vec(value)?;
    send.write_all(&(body.len() as u32).to_be_bytes()).await?;
    send.write_all(&body).await?;
    Ok(())
}

async fn read_frame<T: DeserializeOwned>(recv: &mut RecvStream) -> Result<T> {
    let mut len = [0u8; 4];
    recv.read_exact(&mut len).await?;
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME {
        bail!("对方发来的数据过大（{len} 字节）");
    }
    let mut body = vec![0u8; len];
    recv.read_exact(&mut body).await?;
    Ok(serde_json::from_slice(&body)?)
}

// ───────────────────────────────── 入口 ─────────────────────────────────

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // 双击运行时没有参数，结束前要停一下，不然窗口一闪就没了。
    let double_clicked = args.is_empty();
    let result = run(args).await;
    if let Err(err) = &result {
        eprintln!("\n出错了：{err:#}");
    }
    if double_clicked {
        pause();
    }
    if result.is_err() {
        std::process::exit(1);
    }
}

fn pause() {
    print!("\n按回车键关闭窗口……");
    flush();
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
}

fn flush() {
    let _ = std::io::stdout().flush();
}

async fn run(args: Vec<String>) -> Result<()> {
    let Some((command, rest)) = args.split_first() else {
        // 双击运行：有允许名单就当被控端，没有就显示用法。
        let allow = read_allow_file()?;
        if allow.is_empty() {
            print_usage();
            return Ok(());
        }
        return serve(allow).await;
    };
    match command.as_str() {
        "serve" => {
            let mut allow = read_allow_file()?;
            for id in parse_allow_args(rest)? {
                if !allow.contains(&id) {
                    allow.push(id);
                }
            }
            serve(allow).await
        }
        "id" => {
            println!("{}", load_or_create_key()?.public());
            Ok(())
        }
        "netcheck" => netcheck().await,
        "ping" | "info" | "ls" | "open" | "app" | "apps" | "speed" | "test" => {
            client(command, rest).await
        }
        "help" | "-h" | "--help" | "/?" => {
            print_usage();
            Ok(())
        }
        other => {
            print_usage();
            bail!("不认识的指令：{other}")
        }
    }
}

fn print_usage() {
    println!(
        "电脑共享 · 技术验证工具

被控端（让别的电脑来操作这台）：
  pcshare-probe serve --allow <对方身份码>
  也可以把对方身份码写进同目录的 allow.txt，然后直接双击本程序

控制端（操作别的电脑）：
  pcshare-probe id                           显示本机身份码
  pcshare-probe netcheck                     本机网络体检
  pcshare-probe test  <对方身份码>            连通性和速度测试
  pcshare-probe ping  <对方身份码>            线路和延迟
  pcshare-probe info  <对方身份码>            对方电脑的基本情况
  pcshare-probe ls    <对方身份码> [文件夹]    不写文件夹就列出盘符
  pcshare-probe apps  <对方身份码> [关键字]    列出对方已装的软件
  pcshare-probe app   <对方身份码> <软件名>    按名字打开对方的软件
  pcshare-probe open  <对方身份码> <程序或文件> [参数]
  pcshare-probe speed <对方身份码> [多少MB]"
    );
}

// ──────────────────────────── 身份码和允许名单 ────────────────────────────

fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(|dir| dir.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// 身份文件的位置：默认在程序旁边；环境变量 `PCSHARE_PROBE_KEY` 可以改到别处，
/// 方便在同一台电脑上同时扮演两个设备做自测。
fn key_path() -> PathBuf {
    std::env::var_os("PCSHARE_PROBE_KEY")
        .map(PathBuf::from)
        .unwrap_or_else(|| exe_dir().join("probe.key"))
}

/// 读取本机的密钥，没有就新建一个。密钥对应的公开部分就是“身份码”。
fn load_or_create_key() -> Result<SecretKey> {
    let path = key_path();
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            let bytes = hex_decode(text.trim()).with_context(|| {
                format!("身份文件 {} 已损坏，删掉它会重新生成", path.display())
            })?;
            Ok(SecretKey::from_bytes(&bytes))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let key = SecretKey::generate();
            std::fs::write(&path, hex_encode(&key.to_bytes()))
                .with_context(|| format!("无法写入身份文件 {}", path.display()))?;
            Ok(key)
        }
        Err(err) => Err(err).with_context(|| format!("无法读取身份文件 {}", path.display())),
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn hex_decode(text: &str) -> Result<[u8; 32]> {
    if text.len() != 64 || !text.is_ascii() {
        bail!("长度应该是 64 个字符");
    }
    let mut out = [0u8; 32];
    for (i, pair) in text.as_bytes().chunks(2).enumerate() {
        out[i] = u8::from_str_radix(std::str::from_utf8(pair)?, 16)?;
    }
    Ok(out)
}

fn parse_id(text: &str) -> Result<EndpointId> {
    text.trim()
        .parse::<EndpointId>()
        .map_err(|_| anyhow!("身份码格式不对：{text}"))
}

/// 读取程序旁边的 allow.txt：每行一个身份码，`#` 之后是注释。
fn read_allow_file() -> Result<Vec<EndpointId>> {
    let path = exe_dir().join("allow.txt");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).with_context(|| format!("无法读取 {}", path.display())),
    };
    let mut allow = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim_start_matches('\u{feff}');
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let id = parse_id(line).with_context(|| format!("allow.txt 第 {} 行", index + 1))?;
        if !allow.contains(&id) {
            allow.push(id);
        }
    }
    Ok(allow)
}

fn parse_allow_args(args: &[String]) -> Result<Vec<EndpointId>> {
    let mut allow = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--allow" => {
                let value = iter.next().context("--allow 后面要跟对方的身份码")?;
                allow.push(parse_id(value)?);
            }
            other => bail!("serve 不认识这个参数：{other}"),
        }
    }
    Ok(allow)
}

// ───────────────────────────────── 联网 ─────────────────────────────────

async fn bind_endpoint(accept_connections: bool) -> Result<Endpoint> {
    let key = load_or_create_key()?;
    let mut builder = Endpoint::builder(presets::N0).secret_key(key);
    if accept_connections {
        builder = builder.alpns(vec![ALPN.to_vec()]);
    }
    builder
        .bind()
        .await
        .map_err(|err| anyhow!("启动网络失败：{err}"))
}

/// 把中转服务器的网址变成好认的名字。
fn relay_label(url: &RelayUrl) -> String {
    let text = url.to_string();
    let host = text
        .trim_start_matches("https://")
        .trim_end_matches('/')
        .trim_end_matches('.');
    let region = match host.get(..4) {
        Some("use1") => "北美东部",
        Some("usw1") => "北美西部",
        Some("euc1") => "欧洲",
        Some("aps1") => "亚太",
        _ => "",
    };
    if region.is_empty() {
        host.to_string()
    } else {
        format!("{region} {host}")
    }
}

fn ip_kind(ip: IpAddr) -> &'static str {
    match ip.to_canonical() {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            if v4.is_loopback() {
                "本机"
            } else if v4.is_private() || v4.is_link_local() {
                "局域网 IPv4"
            } else if o[0] == 100 && (o[1] & 0xC0) == 64 {
                "运营商内网或组网 IPv4"
            } else if o[0] == 198 && (o[1] & 0xFE) == 18 {
                "代理软件的虚拟网卡"
            } else {
                "公网 IPv4"
            }
        }
        IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            if v6.is_loopback() {
                "本机"
            } else if (first & 0xffc0) == 0xfe80 {
                "局域网 IPv6"
            } else if (first & 0xfe00) == 0xfc00 {
                "内网 IPv6"
            } else {
                "公网 IPv6"
            }
        }
    }
}

/// 只显示地址的前一小段，够判断属于哪家网络，又不把完整地址打出来。
fn mask_ip(ip: IpAddr) -> String {
    match ip.to_canonical() {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            format!("{}.{}.*.*", o[0], o[1])
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            format!("{:x}:{:x}:*", s[0], s[1])
        }
    }
}

struct PathView {
    label: String,
    rtt: Duration,
    selected: bool,
    direct: bool,
}

/// 当前连接上的所有线路。通常有一条走中转，打洞成功后多一条直连。
fn snapshot_paths(conn: &Connection) -> Vec<PathView> {
    conn.paths()
        .iter()
        .map(|path| {
            let label = match path.remote_addr() {
                TransportAddr::Relay(url) => format!("中转（{}）", relay_label(url)),
                TransportAddr::Ip(addr) => format!("直连（{}）", ip_kind(addr.ip())),
                _ => "其他线路".to_string(),
            };
            PathView {
                label,
                rtt: path.rtt(),
                selected: path.is_selected(),
                direct: path.is_ip(),
            }
        })
        .collect()
}

fn print_paths(paths: &[PathView]) {
    if paths.is_empty() {
        println!("  （没有可用线路）");
    }
    for path in paths {
        let mark = if path.selected { "  ← 正在使用" } else { "" };
        println!("  {}，延迟 {}{mark}", path.label, fmt_duration(path.rtt));
    }
}

fn using_direct(paths: &[PathView]) -> bool {
    paths.iter().any(|path| path.direct && path.selected)
}

fn fmt_duration(duration: Duration) -> String {
    let millis = duration.as_secs_f64() * 1000.0;
    if millis < 10.0 {
        format!("{millis:.1} 毫秒")
    } else if millis < 1000.0 {
        format!("{millis:.0} 毫秒")
    } else {
        format!("{:.1} 秒", millis / 1000.0)
    }
}

fn fmt_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn fmt_speed(bytes: u64, elapsed: Duration) -> String {
    let secs = elapsed.as_secs_f64().max(0.001);
    let mb_per_sec = bytes as f64 / secs / 1_000_000.0;
    format!(
        "{:.1} MB/秒（约 {:.0} 兆宽带），共传 {}，用时 {}",
        mb_per_sec,
        mb_per_sec * 8.0,
        fmt_size(bytes),
        fmt_duration(elapsed)
    )
}

// ───────────────────────────────── 被控端 ─────────────────────────────────

async fn serve(allow: Vec<EndpointId>) -> Result<()> {
    println!("=== 电脑共享 · 技术验证（被控端）===");
    if allow.is_empty() {
        bail!(
            "没有允许名单。请用 --allow 指定控制端的身份码，或者写进同目录的 allow.txt。\n为了安全，被控端不接受名单之外的任何设备"
        );
    }
    let endpoint = bind_endpoint(true).await?;
    println!("本机身份码：{}", endpoint.id());
    println!("允许连接的设备：{} 个", allow.len());
    println!("本机桌面状态：{}", Desktop::current().summary());

    print!("正在连接公共服务器……");
    flush();
    match timeout(Duration::from_secs(15), endpoint.online()).await {
        Ok(()) => {
            let mut status = endpoint.home_relay_status();
            let names: Vec<String> = status
                .get()
                .iter()
                .filter(|relay| relay.is_connected())
                .map(|relay| relay_label(relay.url()))
                .collect();
            println!("已上线（{}）", names.join("、"));
        }
        Err(_) => println!("15 秒内没连上。会继续尝试，但对方可能暂时找不到这台电脑。"),
    }
    println!("等待控制端连接。测试结束后直接关掉这个窗口即可。\n");

    let allow = Arc::new(allow);
    while let Some(incoming) = endpoint.accept().await {
        let allow = allow.clone();
        tokio::spawn(async move {
            match incoming.await {
                Ok(conn) => handle_connection(conn, &allow).await,
                Err(err) => println!("[{}] 有一次连接握手失败：{err}", win::local_time_hms()),
            }
        });
    }
    Ok(())
}

async fn handle_connection(conn: Connection, allow: &[EndpointId]) {
    let peer = conn.remote_id();
    if !allow.contains(&peer) {
        println!("[{}] 拒绝了不在名单里的设备：{peer}", win::local_time_hms());
        // 先告诉对方原因，再断开。
        if let Ok(Ok((mut send, _recv))) = timeout(Duration::from_secs(5), conn.accept_bi()).await {
            let reply: Reply<()> =
                Err("对方没有把这台电脑加入允许名单（要把本机身份码告诉对方）".to_string());
            let _ = write_frame(&mut send, &reply).await;
            let _ = send.finish();
            let _ = timeout(Duration::from_secs(2), send.stopped()).await;
        }
        conn.close(CLOSE_UNAUTHORIZED.into(), b"unauthorized");
        return;
    }

    println!("[{}] 设备已连接：{}", win::local_time_hms(), peer.fmt_short());
    // 过几秒报告一下线路，让坐在被控端前面的人也能看到是不是直连。
    let watched = conn.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(5)).await;
        if watched.close_reason().is_none() {
            let paths = snapshot_paths(&watched);
            if let Some(path) = paths.iter().find(|path| path.selected) {
                println!(
                    "[{}] 当前线路：{}，延迟 {}",
                    win::local_time_hms(),
                    path.label,
                    fmt_duration(path.rtt)
                );
            }
        }
    });

    while let Ok((send, recv)) = conn.accept_bi().await {
        tokio::spawn(async move {
            if let Err(err) = handle_stream(send, recv).await {
                println!("[{}] 处理指令出错：{err:#}", win::local_time_hms());
            }
        });
    }
    println!("[{}] 设备已断开：{}", win::local_time_hms(), peer.fmt_short());
}

async fn handle_stream(mut send: SendStream, mut recv: RecvStream) -> Result<()> {
    let request: Request = read_frame(&mut recv).await?;
    let now = win::local_time_hms();
    match request {
        Request::Ping => write_frame(&mut send, &ok(())).await?,
        Request::Info => {
            println!("[{now}] 查询本机情况");
            write_frame(&mut send, &ok(local_info())).await?;
        }
        Request::Ls { path } => {
            println!(
                "[{now}] 列出文件夹：{}",
                if path.is_empty() { "（所有盘符）" } else { &path }
            );
            let reply = tokio::task::spawn_blocking(move || list_dir(&path))
                .await
                .unwrap_or_else(|err| Err(format!("内部错误：{err}")));
            write_frame(&mut send, &reply).await?;
        }
        Request::Open { target, args } => {
            println!("[{now}] 打开：{target} {}", args.as_deref().unwrap_or(""));
            let reply = launch_and_watch(target, args).await;
            report_open(&reply);
            write_frame(&mut send, &reply).await?;
        }
        Request::Apps => {
            println!("[{now}] 列出已装软件");
            write_frame(&mut send, &list_apps().await).await?;
        }
        Request::OpenApp { name } => {
            println!("[{now}] 按名字打开软件：{name}");
            let reply = open_app(&name).await;
            report_open(&reply.as_ref().map(|opened| &opened.opened).map_err(Clone::clone));
            write_frame(&mut send, &reply).await?;
        }
        Request::Download { bytes, max_millis } => {
            println!("[{now}] 测速：向对方发送数据");
            write_frame(&mut send, &ok(())).await?;
            let chunk = vec![0u8; CHUNK];
            let deadline = Instant::now() + Duration::from_millis(max_millis);
            let mut left = bytes.min(MAX_TRANSFER);
            while left > 0 && Instant::now() < deadline {
                let n = left.min(CHUNK as u64) as usize;
                send.write_all(&chunk[..n]).await?;
                left -= n as u64;
            }
        }
        Request::Upload => {
            println!("[{now}] 测速：接收对方发来的数据");
            let mut buf = vec![0u8; CHUNK];
            let mut total = 0u64;
            while let Some(n) = recv.read(&mut buf).await? {
                total += n as u64;
            }
            write_frame(&mut send, &ok(UploadReply { bytes: total })).await?;
        }
    }
    send.finish()?;
    Ok(())
}

fn report_open<T: std::borrow::Borrow<OpenReply>>(reply: &Reply<T>) {
    match reply {
        Ok(opened) => {
            let windows = &opened.borrow().new_windows;
            if windows.is_empty() {
                println!("    已交给系统打开，暂时没看到新窗口");
            } else {
                println!("    新窗口：{}", windows.join("；"));
            }
        }
        Err(reason) => println!("    没打开：{reason}"),
    }
}

fn local_info() -> InfoReply {
    InfoReply {
        computer: std::env::var("COMPUTERNAME").unwrap_or_default(),
        user: std::env::var("USERNAME").unwrap_or_default(),
        desktop: Desktop::current(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    }
}

fn list_dir(path: &str) -> Reply<LsReply> {
    if path.is_empty() {
        let entries = win::logical_drives()
            .into_iter()
            .map(|drive| Entry {
                name: drive,
                dir: true,
                size: 0,
            })
            .collect();
        return Ok(LsReply {
            path: String::new(),
            entries,
        });
    }
    let reader = std::fs::read_dir(path).map_err(|err| format!("打不开文件夹 {path}：{err}"))?;
    let mut entries = Vec::new();
    for item in reader.flatten() {
        // Windows 在列目录时已经带回了类型和大小，这里不会再多一次磁盘访问。
        let meta = item.metadata().ok();
        entries.push(Entry {
            name: item.file_name().to_string_lossy().into_owned(),
            dir: meta.as_ref().is_some_and(|m| m.is_dir()),
            size: meta.as_ref().map_or(0, |m| m.len()),
        });
    }
    entries.sort_by(|a, b| {
        b.dir
            .cmp(&a.dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(LsReply {
        path: path.to_string(),
        entries,
    })
}

/// 打开目标，并观察桌面上有没有新窗口冒出来。
async fn launch_and_watch(target: String, args: Option<String>) -> Reply<OpenReply> {
    let before: HashSet<usize> = win::visible_windows()
        .into_iter()
        .map(|window| window.hwnd)
        .collect();

    tokio::task::spawn_blocking(move || win::shell_open(&target, args.as_deref()))
        .await
        .map_err(|err| format!("内部错误：{err}"))??;

    // 最多等 6 秒；看到新窗口后再多等一小会儿，把同时弹出的窗口收全。
    let deadline = Instant::now() + Duration::from_secs(6);
    let mut new_windows = Vec::new();
    while Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(200)).await;
        if !new_titles(&before).is_empty() {
            tokio::time::sleep(Duration::from_millis(400)).await;
            new_windows = new_titles(&before);
            break;
        }
    }
    Ok(OpenReply {
        new_windows,
        desktop: Desktop::current(),
    })
}

fn new_titles(before: &HashSet<usize>) -> Vec<String> {
    win::visible_windows()
        .into_iter()
        .filter(|window| !before.contains(&window.hwnd))
        .map(|window| window.title)
        .collect()
}

/// 开始菜单里的所有软件（含应用商店软件）。借用系统自带的脚本工具来查，
/// 正式版会换成直接调用系统接口。
async fn list_apps() -> Reply<Vec<App>> {
    #[derive(Deserialize)]
    struct Raw {
        #[serde(rename = "Name")]
        name: String,
        #[serde(rename = "AppID")]
        app_id: String,
    }
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let script = "[Console]::OutputEncoding=[Text.Encoding]::UTF8; \
                  ConvertTo-Json -Compress -InputObject @(Get-StartApps | Select-Object Name,AppID)";
    let output = tokio::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .await
        .map_err(|err| format!("无法运行系统自带的脚本工具：{err}"))?;
    if !output.status.success() {
        return Err(format!(
            "查询已装软件失败：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let raw: Vec<Raw> = serde_json::from_str(text.trim().trim_start_matches('\u{feff}'))
        .map_err(|err| format!("看不懂系统返回的软件列表：{err}"))?;
    let mut apps: Vec<App> = raw
        .into_iter()
        .map(|item| App {
            name: item.name,
            id: item.app_id,
        })
        .collect();
    apps.sort_by_key(|app| app.name.to_lowercase());
    Ok(apps)
}

async fn open_app(name: &str) -> Reply<OpenAppReply> {
    let apps = list_apps().await?;
    let needle = name.to_lowercase();
    let exact = apps.iter().find(|app| app.name.to_lowercase() == needle);
    let app = match exact {
        Some(app) => app,
        None => {
            let matches: Vec<&App> = apps
                .iter()
                .filter(|app| app.name.to_lowercase().contains(&needle))
                .collect();
            match matches.as_slice() {
                [] => return Err(format!("对方的开始菜单里没有名字包含“{name}”的软件")),
                [only] => *only,
                many => {
                    let names: Vec<&str> =
                        many.iter().take(8).map(|app| app.name.as_str()).collect();
                    return Err(format!(
                        "有 {} 个软件的名字都包含“{name}”，请写得更具体：{}",
                        many.len(),
                        names.join("、")
                    ));
                }
            }
        }
    };
    let opened = launch_and_watch(format!("shell:AppsFolder\\{}", app.id), None).await?;
    Ok(OpenAppReply {
        name: app.name.clone(),
        opened,
    })
}

// ───────────────────────────────── 控制端 ─────────────────────────────────

async fn client(command: &str, rest: &[String]) -> Result<()> {
    let target = parse_id(rest.first().context("缺少对方的身份码")?)?;
    let params = &rest[1..];

    let endpoint = bind_endpoint(false).await?;
    let started = Instant::now();
    let conn = timeout(Duration::from_secs(30), endpoint.connect(target, ALPN))
        .await
        .map_err(|_| anyhow!("30 秒内没连上对方。请确认对方的被控端正在运行，两边都能上网"))?
        .map_err(|err| anyhow!("连接对方失败：{err}"))?;
    let connect_time = started.elapsed();

    // 先发一条最简单的指令：如果对方没有授权本机，这里就会得到明确的原因。
    let outcome = match request::<()>(&conn, &Request::Ping).await {
        Ok(()) => run_client_command(command, params, &conn, connect_time).await,
        Err(err) => Err(err),
    };
    conn.close(CLOSE_OK.into(), b"bye");
    endpoint.close().await;
    outcome
}

async fn run_client_command(
    command: &str,
    params: &[String],
    conn: &Connection,
    connect_time: Duration,
) -> Result<()> {
    match command {
        "ping" => cmd_ping(conn, connect_time).await,
        "info" => cmd_info(conn).await,
        "ls" => cmd_ls(conn, params.first().map_or("", String::as_str)).await,
        "open" => {
            let target = params.first().context("缺少要打开的程序或文件")?;
            let args = (params.len() > 1).then(|| params[1..].join(" "));
            cmd_open(conn, target, args).await
        }
        "app" => {
            if params.is_empty() {
                bail!("缺少软件名");
            }
            cmd_app(conn, &params.join(" ")).await
        }
        "apps" => cmd_apps(conn, params.first().map(String::as_str)).await,
        "speed" => {
            let megabytes = match params.first() {
                Some(text) => text
                    .parse::<u64>()
                    .map_err(|_| anyhow!("大小要写整数，单位是 MB"))?,
                None => 32,
            };
            cmd_speed(conn, megabytes).await
        }
        "test" => cmd_test(conn, connect_time).await,
        other => bail!("不认识的指令：{other}"),
    }
}

/// 发一条指令，等对方的回复。
async fn request<T: DeserializeOwned>(conn: &Connection, req: &Request) -> Result<T> {
    let (mut send, mut recv) = conn
        .open_bi()
        .await
        .map_err(|err| anyhow!("和对方的连接断了：{err}"))?;
    write_frame(&mut send, req).await?;
    send.finish()?;
    let reply: Reply<T> = read_frame(&mut recv)
        .await
        .context("没有收到对方的回复")?;
    reply.map_err(|reason| anyhow!(reason))
}

async fn ping_once(conn: &Connection) -> Result<Duration> {
    let started = Instant::now();
    request::<()>(conn, &Request::Ping).await?;
    Ok(started.elapsed())
}

/// 等直连建立。期间不断发小指令，保持线路上有数据。
async fn wait_for_direct(conn: &Connection, max_wait: Duration) -> Result<Vec<PathView>> {
    let deadline = Instant::now() + max_wait;
    loop {
        let paths = snapshot_paths(conn);
        if using_direct(&paths) || Instant::now() >= deadline {
            return Ok(paths);
        }
        ping_once(conn).await?;
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

struct PingStats {
    min: Duration,
    median: Duration,
    max: Duration,
}

async fn ping_rounds(conn: &Connection, rounds: usize) -> Result<PingStats> {
    let mut samples = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        samples.push(ping_once(conn).await?);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    samples.sort();
    Ok(PingStats {
        min: samples[0],
        median: samples[samples.len() / 2],
        max: samples[samples.len() - 1],
    })
}

async fn cmd_ping(conn: &Connection, connect_time: Duration) -> Result<()> {
    println!("已连接，用时 {}", fmt_duration(connect_time));
    println!("等待直连建立（最多 10 秒）……");
    let paths = wait_for_direct(conn, Duration::from_secs(10)).await?;
    println!("线路：");
    print_paths(&paths);
    let stats = ping_rounds(conn, 10).await?;
    println!(
        "指令往返延迟（10 次）：最小 {} / 中位 {} / 最大 {}",
        fmt_duration(stats.min),
        fmt_duration(stats.median),
        fmt_duration(stats.max)
    );
    Ok(())
}

async fn cmd_info(conn: &Connection) -> Result<()> {
    let info: InfoReply = request(conn, &Request::Info).await?;
    println!("对方电脑：{}（登录用户 {}）", info.computer, info.user);
    println!("桌面状态：{}", info.desktop.summary());
    println!("验证工具版本：{}", info.version);
    Ok(())
}

async fn cmd_ls(conn: &Connection, path: &str) -> Result<()> {
    let started = Instant::now();
    let reply: LsReply = request(
        conn,
        &Request::Ls {
            path: path.to_string(),
        },
    )
    .await?;
    let elapsed = started.elapsed();
    if reply.path.is_empty() {
        println!("对方电脑的盘符（用时 {}）：", fmt_duration(elapsed));
    } else {
        println!(
            "{} 下共 {} 项（用时 {}）：",
            reply.path,
            reply.entries.len(),
            fmt_duration(elapsed)
        );
    }
    const SHOW: usize = 40;
    for entry in reply.entries.iter().take(SHOW) {
        if entry.dir {
            println!("  [文件夹]    {}", entry.name);
        } else {
            println!("  {:>10}  {}", fmt_size(entry.size), entry.name);
        }
    }
    if reply.entries.len() > SHOW {
        println!("  ……还有 {} 项没显示", reply.entries.len() - SHOW);
    }
    Ok(())
}

fn print_opened(opened: &OpenReply) {
    if opened.new_windows.is_empty() {
        println!("6 秒内没有检测到新窗口。软件可能本来就开着，或者启动比较慢。");
    } else {
        println!("对方桌面上新出现的窗口：");
        for title in &opened.new_windows {
            println!("  {title}");
        }
    }
    println!("对方桌面状态：{}", opened.desktop.summary());
}

async fn cmd_open(conn: &Connection, target: &str, args: Option<String>) -> Result<()> {
    let started = Instant::now();
    let opened: OpenReply = request(
        conn,
        &Request::Open {
            target: target.to_string(),
            args,
        },
    )
    .await?;
    println!(
        "对方系统已接受打开指令（含等窗口的时间共 {}）。",
        fmt_duration(started.elapsed())
    );
    print_opened(&opened);
    Ok(())
}

async fn cmd_app(conn: &Connection, name: &str) -> Result<()> {
    let reply: OpenAppReply = request(
        conn,
        &Request::OpenApp {
            name: name.to_string(),
        },
    )
    .await?;
    println!("已在对方电脑上打开：{}", reply.name);
    print_opened(&reply.opened);
    Ok(())
}

async fn cmd_apps(conn: &Connection, keyword: Option<&str>) -> Result<()> {
    let started = Instant::now();
    let apps: Vec<App> = request(conn, &Request::Apps).await?;
    let elapsed = started.elapsed();
    let needle = keyword.map(str::to_lowercase);
    let shown: Vec<&App> = apps
        .iter()
        .filter(|app| {
            needle
                .as_ref()
                .is_none_or(|needle| app.name.to_lowercase().contains(needle))
        })
        .collect();
    println!(
        "对方开始菜单里共 {} 个软件（用时 {}）{}：",
        apps.len(),
        fmt_duration(elapsed),
        match keyword {
            Some(word) => format!("，名字包含“{word}”的有 {} 个", shown.len()),
            None => String::new(),
        }
    );
    const SHOW: usize = 60;
    for app in shown.iter().take(SHOW) {
        println!("  {}", app.name);
    }
    if shown.len() > SHOW {
        println!("  ……还有 {} 个没显示，可以加关键字筛选", shown.len() - SHOW);
    }
    Ok(())
}

async fn download_test(conn: &Connection, bytes: u64, max_time: Duration) -> Result<(u64, Duration)> {
    let (mut send, mut recv) = conn.open_bi().await?;
    write_frame(
        &mut send,
        &Request::Download {
            bytes,
            max_millis: max_time.as_millis() as u64,
        },
    )
    .await?;
    send.finish()?;
    let reply: Reply<()> = read_frame(&mut recv).await?;
    reply.map_err(|reason| anyhow!(reason))?;

    let started = Instant::now();
    let mut buf = vec![0u8; CHUNK];
    let mut total = 0u64;
    while let Some(n) = recv.read(&mut buf).await? {
        total += n as u64;
    }
    Ok((total, started.elapsed()))
}

async fn upload_test(conn: &Connection, bytes: u64, max_time: Duration) -> Result<(u64, Duration)> {
    let (mut send, mut recv) = conn.open_bi().await?;
    write_frame(&mut send, &Request::Upload).await?;

    let started = Instant::now();
    let chunk = vec![0u8; CHUNK];
    let mut left = bytes.min(MAX_TRANSFER);
    while left > 0 && started.elapsed() < max_time {
        let n = left.min(CHUNK as u64) as usize;
        send.write_all(&chunk[..n]).await?;
        left -= n as u64;
    }
    send.finish()?;
    // 对方全部收完才会回复，所以这里的用时包含了数据真正送达的时间。
    let reply: Reply<UploadReply> = read_frame(&mut recv).await?;
    let received = reply.map_err(|reason| anyhow!(reason))?.bytes;
    Ok((received, started.elapsed()))
}

async fn cmd_speed(conn: &Connection, megabytes: u64) -> Result<()> {
    let bytes = megabytes * 1024 * 1024;
    let limit = Duration::from_secs(15);
    println!("线路：");
    print_paths(&snapshot_paths(conn));
    let (down, down_time) = download_test(conn, bytes, limit).await?;
    println!("从对方取数据：{}", fmt_speed(down, down_time));
    let (up, up_time) = upload_test(conn, bytes, limit).await?;
    println!("向对方送数据：{}", fmt_speed(up, up_time));
    Ok(())
}

async fn cmd_test(conn: &Connection, connect_time: Duration) -> Result<()> {
    println!("[1/5] 连接对方：成功，用时 {}", fmt_duration(connect_time));

    println!("[2/5] 等待直连建立（最多 10 秒）……");
    let paths = wait_for_direct(conn, Duration::from_secs(10)).await?;
    print_paths(&paths);
    let direct = using_direct(&paths);

    let stats = ping_rounds(conn, 10).await?;
    println!(
        "[3/5] 指令往返延迟（10 次）：最小 {} / 中位 {} / 最大 {}",
        fmt_duration(stats.min),
        fmt_duration(stats.median),
        fmt_duration(stats.max)
    );

    let info: InfoReply = request(conn, &Request::Info).await?;
    let drives: LsReply = request(
        conn,
        &Request::Ls {
            path: String::new(),
        },
    )
    .await?;
    let drive_names: Vec<&str> = drives.entries.iter().map(|e| e.name.as_str()).collect();
    println!(
        "[4/5] 对方电脑：{}（登录用户 {}），{}；盘符：{}",
        info.computer,
        info.user,
        info.desktop.summary(),
        drive_names.join(" ")
    );

    // 中转服务器是公共资源，有限速，只传一点点；直连就多传一些测准一点。
    let (megabytes, limit) = if direct {
        (64, Duration::from_secs(8))
    } else {
        (2, Duration::from_secs(8))
    };
    let bytes = megabytes * 1024 * 1024;
    let (down, down_time) = download_test(conn, bytes, limit).await?;
    println!("[5/5] 从对方取数据：{}", fmt_speed(down, down_time));
    let (up, up_time) = upload_test(conn, bytes, limit).await?;
    println!("      向对方送数据：{}", fmt_speed(up, up_time));

    println!();
    // 测速期间线路可能已经变化，最后再看一眼。
    let final_paths = snapshot_paths(conn);
    if using_direct(&final_paths) {
        println!("结论：两台电脑已经直连，数据不经过中转服务器。");
    } else {
        println!("结论：没能直连，目前靠公共中转服务器转发，速度和延迟都受它限制。");
    }
    Ok(())
}

// ──────────────────────────────── 网络体检 ────────────────────────────────

fn yes_no(value: bool) -> &'static str {
    if value { "通" } else { "不通" }
}

fn nat_kind(varies: Option<bool>) -> &'static str {
    match varies {
        Some(false) => "对直连友好（对外端口固定）",
        Some(true) => "对直连不友好（每连一个目标就换端口）",
        None => "没能判断",
    }
}

async fn netcheck() -> Result<()> {
    println!("=== 本机网络体检 ===");
    let endpoint = bind_endpoint(false).await?;
    print!("正在探测（最多 20 秒）……");
    flush();
    let mut watcher = endpoint.net_report();
    let report = timeout(Duration::from_secs(20), watcher.initialized())
        .await
        .ok();
    let online = timeout(Duration::from_secs(10), endpoint.online())
        .await
        .is_ok();
    println!("完成\n");

    match report {
        None => println!("没有拿到探测结果：这台电脑可能连不上公共服务器。"),
        Some(report) => {
            println!("IPv4 收发数据：{}", yes_no(report.udp_v4));
            println!("IPv6 收发数据：{}", yes_no(report.udp_v6));
            println!(
                "IPv4 路由器类型：{}",
                nat_kind(report.mapping_varies_by_dest_ipv4)
            );
            if let Some(addr) = report.global_v4 {
                println!("对外的 IPv4 地址：{}", mask_ip(IpAddr::V4(*addr.ip())));
            }
            if let Some(addr) = report.global_v6 {
                println!("对外的 IPv6 地址：{}", mask_ip(IpAddr::V6(*addr.ip())));
            }
            if report.captive_portal == Some(true) {
                println!("注意：这个网络需要先在网页上登录（比如酒店、校园网）。");
            }

            // 每台中转服务器三种探测方式的延迟：网页通道 / IPv4 / IPv6。
            let mut table: BTreeMap<String, [Option<Duration>; 3]> = BTreeMap::new();
            for (probe, url, latency) in report.relay_latency.iter() {
                let column = match probe {
                    Probe::Https => 0,
                    Probe::QadIpv4 => 1,
                    Probe::QadIpv6 => 2,
                    _ => continue,
                };
                table.entry(relay_label(url)).or_default()[column] = Some(latency);
            }
            println!("\n到各中转服务器的延迟（网页通道 / IPv4 / IPv6）：");
            if table.is_empty() {
                println!("  一台都没探测到");
            }
            for (name, columns) in &table {
                let cells: Vec<String> = columns
                    .iter()
                    .map(|cell| cell.map_or("不通".to_string(), fmt_duration))
                    .collect();
                println!("  {name}：{}", cells.join(" / "));
            }
            if let Some(url) = &report.preferred_relay {
                println!("首选中转服务器：{}", relay_label(url));
            }
        }
    }

    println!(
        "\n公共服务器在线状态：{}",
        if online { "已连上" } else { "没连上" }
    );
    let mut status = endpoint.home_relay_status();
    for relay in status.get() {
        if let Some(err) = relay.last_error() {
            println!("  {} 最近一次出错：{err}", relay_label(relay.url()));
        }
    }

    let addr = endpoint.addr();
    let mut kinds: BTreeMap<&str, usize> = BTreeMap::new();
    for ip in addr.ip_addrs() {
        *kinds.entry(ip_kind(ip.ip())).or_default() += 1;
    }
    let summary: Vec<String> = kinds
        .iter()
        .map(|(kind, count)| format!("{kind} × {count}"))
        .collect();
    println!(
        "本机可用于直连的地址：{}",
        if summary.is_empty() {
            "没有".to_string()
        } else {
            summary.join("，")
        }
    );

    endpoint.close().await;
    Ok(())
}
