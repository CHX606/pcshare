//! 电脑共享命令行与无界面服务程序（任务 1.10）。
//!
//! - 不带子命令（或 `serve`）：作为后台被控端运行。
//! - 带子命令：作为控制端执行配对、设备管理、文件夹浏览与操作、上传下载、软件启动与进程管理。

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use pcshare_core::{
    CoreConfig, CoreEvent, CoreNode, LocalStorage, NetworkMode, PairingDecision, PeerRecord,
    default_data_dir, parse_invite_or_id,
};
use pcshare_platform::NativePlatform;
use pcshare_proto::{
    AppLaunchArgs, ConflictPolicy, EntryKind, FsCopyMoveArgs, FsDeleteArgs, FsMkdirArgs,
    FsRenameArgs, LaunchKind, Permissions, ProcKillArgs,
};
use tokio_util::sync::CancellationToken;

struct GlobalOpts {
    data_dir: PathBuf,
    network_mode: NetworkMode,
    name_override: Option<String>,
    json_output: bool,
}

#[tokio::main]
async fn main() {
    // 必须在任何截图、点按之前调用，否则系统缩放下坐标会被虚拟化。
    pcshare_platform::enable_per_monitor_dpi_awareness();

    tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_target(false)
        .with_max_level(tracing::Level::WARN)
        .init();

    if let Err(err) = run().await {
        eprintln!("出错了：{err:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let raw_args: Vec<String> = std::env::args().skip(1).collect();
    let (opts, rest) = extract_global_opts(raw_args)?;

    let Some((subcmd, args)) = rest.split_first() else {
        return cmd_serve(&opts, &[]).await;
    };

    match subcmd.as_str() {
        "serve" => cmd_serve(&opts, args).await,
        "id" => cmd_id(&opts),
        "peers" | "devices" => cmd_peers(&opts),
        "peer-rename" => cmd_peer_rename(&opts, args),
        "peer-remove" => cmd_peer_remove(&opts, args),
        "audits" => cmd_audits(&opts, args),
        "pair" => cmd_pair(&opts, args).await,
        "ping" => cmd_ping(&opts, args).await,
        "info" => cmd_info(&opts, args).await,
        "roots" => cmd_roots(&opts, args).await,
        "ls" => cmd_ls(&opts, args).await,
        "stat" => cmd_stat(&opts, args).await,
        "mkdir" => cmd_mkdir(&opts, args).await,
        "rename" => cmd_rename(&opts, args).await,
        "cp" => cmd_copy_or_move(&opts, args, false).await,
        "mv" => cmd_copy_or_move(&opts, args, true).await,
        "rm" => cmd_rm(&opts, args).await,
        "upload" => cmd_upload(&opts, args).await,
        "download" => cmd_download(&opts, args).await,
        "apps" => cmd_apps(&opts, args).await,
        "launch" => cmd_launch(&opts, args).await,
        "ps" => cmd_ps(&opts, args).await,
        "kill" => cmd_kill(&opts, args).await,
        "help" | "-h" | "--help" | "/?" => {
            print_usage();
            Ok(())
        }
        other => {
            print_usage();
            bail!("不认识的子命令：{other}")
        }
    }
}

fn extract_global_opts(raw: Vec<String>) -> Result<(GlobalOpts, Vec<String>)> {
    let mut data_dir = default_data_dir();
    let mut network_mode = NetworkMode::PublicOrCustom;
    let mut name_override = None;
    let mut json_output = false;
    let mut rest = Vec::new();

    let mut iter = raw.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--data-dir" => {
                let val = iter.next().context("--data-dir 后面需要指定文件夹路径")?;
                data_dir = PathBuf::from(val);
            }
            "--local" => {
                network_mode = NetworkMode::LocalTest;
            }
            "--name" => {
                let val = iter.next().context("--name 后面需要指定设备名称")?;
                name_override = Some(val);
            }
            "--json" => {
                json_output = true;
            }
            _ => rest.push(arg),
        }
    }

    Ok((
        GlobalOpts {
            data_dir,
            network_mode,
            name_override,
            json_output,
        },
        rest,
    ))
}

fn print_usage() {
    println!(
        "电脑共享 · 命令行与后台程序 (pcshare-agent)

全局选项：
  --data-dir <路径>    指定配置与身份存储目录（默认 %APPDATA%\\pcshare）
  --local              本地直连测试模式（不连接公网服务器）
  --name <名称>        指定本机显示名称
  --json               以 JSON 输出查询结果

被控端模式：
  pcshare-agent [serve] [--pair] [--auto-approve]
      不带参数运行时默认以被控端启动；加 --pair 同时生成 5 分钟有效配对码

控制端命令（<设备> 可填设备名称、完整 64 位身份码或身份码后 8 位）：
  pcshare-agent id                                      显示本机身份码
  pcshare-agent pair <邀请码或身份码> [6位配对码]        与对方电脑配对
  pcshare-agent peers                                   查看已配对设备列表
  pcshare-agent peer-rename <设备> <新名称>              修改设备备注名称
  pcshare-agent peer-remove <设备>                      移除已配对设备
  pcshare-agent ping <设备>                             测试线路与往返延迟
  pcshare-agent info <设备>                             查看对方电脑信息与桌面状态
  pcshare-agent roots <设备>                            列出盘符与常用文件夹
  pcshare-agent ls <设备> [远端目录路径]                 列出目录内容（省略路径则列盘符）
  pcshare-agent stat <设备> <远端路径>                   查看单个文件或文件夹详情
  pcshare-agent mkdir <设备> <远端路径> [-p]             新建文件夹
  pcshare-agent rename <设备> <远端路径> <新文件名>      重命名文件或文件夹
  pcshare-agent cp <设备> <源路径> <目标目录> [--conflict fail|overwrite|skip|rename]
  pcshare-agent mv <设备> <源路径> <目标目录> [--conflict fail|overwrite|skip|rename]
  pcshare-agent rm <设备> <远端路径> [--permanent]       删除（默认进回收站）
  pcshare-agent upload <设备> <本地文件> <远端文件> [--conflict fail|overwrite|skip|rename]
  pcshare-agent download <设备> <远端文件> <本地文件> [--conflict fail|overwrite|skip|rename]
  pcshare-agent apps <设备> [关键字] [--refresh]         列出对方已装软件
  pcshare-agent launch <设备> <目标> [--by-name|--by-id|--by-path] [--args <参数>]
  pcshare-agent ps <设备> [关键字]                       查看对方正在运行的进程
  pcshare-agent kill <设备> <PID> --yes                 结束指定进程（需 --yes 确认）
  pcshare-agent audits [条数]                           查看本机最近被控操作记录"
    );
}

async fn start_client_node(opts: &GlobalOpts) -> Result<Arc<CoreNode>> {
    let cfg = CoreConfig {
        data_dir: opts.data_dir.clone(),
        network_mode: opts.network_mode,
        device_name_override: opts.name_override.clone(),
        auto_connect_peers: false,
    };
    let node = CoreNode::start(cfg, Arc::new(NativePlatform::new())).await?;
    for w in node.startup_warnings() {
        eprintln!("提示：{w}");
    }
    Ok(node)
}

fn open_local_storage(opts: &GlobalOpts) -> Result<LocalStorage> {
    let (store, warnings) =
        LocalStorage::open(&opts.data_dir, Arc::new(NativePlatform::new()))?;
    for w in warnings {
        eprintln!("提示：{w}");
    }
    Ok(store)
}

fn resolve_peer_id(peers: &[PeerRecord], query: &str) -> Result<String> {
    let q = query.trim();
    if let Some(exact) = peers
        .iter()
        .find(|p| p.endpoint_id.eq_ignore_ascii_case(q) || p.name == q)
    {
        return Ok(exact.endpoint_id.clone());
    }
    let matches: Vec<&PeerRecord> = peers
        .iter()
        .filter(|p| {
            p.name.to_lowercase().contains(&q.to_lowercase())
                || p.endpoint_id.to_lowercase().ends_with(&q.to_lowercase())
        })
        .collect();
    match matches.as_slice() {
        [] => bail!("已配对设备列表中找不到“{q}”，请先用 `peers` 查看或用 `pair` 配对"),
        [only] => Ok(only.endpoint_id.clone()),
        many => {
            let names: Vec<String> = many
                .iter()
                .map(|p| format!("{} ({})", p.name, &p.endpoint_id[p.endpoint_id.len() - 8..]))
                .collect();
            bail!(
                "“{q}”匹配到多台设备，请写得更具体：{}",
                names.join("、")
            )
        }
    }
}

fn parse_conflict_flag(val: &str) -> Result<ConflictPolicy> {
    match val.to_ascii_lowercase().as_str() {
        "fail" => Ok(ConflictPolicy::Fail),
        "overwrite" => Ok(ConflictPolicy::Overwrite),
        "skip" => Ok(ConflictPolicy::Skip),
        "rename" | "rename_keep_both" => Ok(ConflictPolicy::RenameKeepBoth),
        other => bail!("不认识的冲突处理策略“{other}”（可选：fail、overwrite、skip、rename）"),
    }
}

fn fmt_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut idx = 0usize;
    while v >= 1024.0 && idx < UNITS.len() - 1 {
        v /= 1024.0;
        idx += 1;
    }
    if idx == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[idx])
    }
}

// ────────────────────────────── 子命令实现 ──────────────────────────────

async fn cmd_serve(opts: &GlobalOpts, args: &[String]) -> Result<()> {
    let mut open_pair = false;
    let mut auto_approve = false;
    for arg in args {
        match arg.as_str() {
            "--pair" => open_pair = true,
            "--auto-approve" => auto_approve = true,
            other => bail!("serve 不认识参数：{other}"),
        }
    }

    let cfg = CoreConfig {
        data_dir: opts.data_dir.clone(),
        network_mode: opts.network_mode,
        device_name_override: opts.name_override.clone(),
        auto_connect_peers: true,
    };
    let node = CoreNode::start(cfg, Arc::new(NativePlatform::new())).await?;
    for w in node.startup_warnings() {
        eprintln!("提示：{w}");
    }

    println!("=== 电脑共享 · 被控端已启动 ===");
    println!("本机名称：{}", node.settings().device_name);
    println!("本机身份码：{}", node.endpoint_id());
    println!(
        "本机桌面状态：{}",
        node.platform().desktop_environment().summary()
    );
    println!("已配对设备数：{}", node.list_peers().len());

    if opts.network_mode == NetworkMode::PublicOrCustom {
        let _ = tokio::time::timeout(Duration::from_secs(6), node.endpoint().online()).await;
    }

    if open_pair {
        let ticket = node.start_pairing_session();
        println!("\n--- 配对已开启（5 分钟内有效）---");
        println!("6 位配对码：{}", ticket.code);
        println!("一键邀请码：{}", ticket.invite_code);
    }
    let _ = io::stdout().flush();

    let mut events_rx = node.subscribe_events();
    let node_for_events = node.clone();
    tokio::spawn(async move {
        while let Ok(ev) = events_rx.recv().await {
            match ev {
                CoreEvent::PairingApprovalNeeded(prompt) => {
                    let _ = writeln!(
                        io::stdout(),
                        "\n收到配对请求：来自“{}”（系统：{}，身份码尾号：{}）",
                        prompt.peer_name,
                        prompt.peer_os,
                        prompt.peer_short_id
                    );
                    if auto_approve {
                        let _ = writeln!(io::stdout(), "已自动允许配对（--auto-approve）。");
                        let _ = io::stdout().flush();
                        let _ = node_for_events.respond_pairing(
                            &prompt.request_id,
                            PairingDecision::Approve {
                                granted_permissions: Permissions::default(),
                                accept_mutual: true,
                            },
                        );
                    } else {
                        let req_id = prompt.request_id.clone();
                        let node_c = node_for_events.clone();
                        tokio::task::spawn_blocking(move || {
                            let _ = write!(io::stdout(), "是否允许该设备连接并操作本机？[y/N]: ");
                            let _ = io::stdout().flush();
                            let mut line = String::new();
                            let approved = io::stdin().read_line(&mut line).is_ok()
                                && matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes");
                            let decision = if approved {
                                let _ = writeln!(io::stdout(), "已允许配对。");
                                PairingDecision::Approve {
                                    granted_permissions: Permissions::default(),
                                    accept_mutual: true,
                                }
                            } else {
                                let _ = writeln!(io::stdout(), "已拒绝配对。");
                                PairingDecision::Reject
                            };
                            let _ = node_c.respond_pairing(&req_id, decision);
                        });
                    }
                }
                CoreEvent::DeviceStateChanged { peer_id, state } => {
                    let short = &peer_id[peer_id.len().saturating_sub(8)..];
                    let _ = writeln!(io::stdout(), "[状态] 设备 ..{short} -> {state:?}");
                }
                _ => {}
            }
        }
    });

    tokio::signal::ctrl_c().await?;
    println!("\n正在停止服务……");
    node.shutdown().await;
    Ok(())
}

fn cmd_id(opts: &GlobalOpts) -> Result<()> {
    let store = open_local_storage(opts)?;
    if opts.json_output {
        println!(
            "{}",
            serde_json::json!({ "endpoint_id": store.endpoint_id().to_string() })
        );
    } else {
        println!("{}", store.endpoint_id());
    }
    Ok(())
}

fn cmd_peers(opts: &GlobalOpts) -> Result<()> {
    let store = open_local_storage(opts)?;
    let peers = store.list_peers();
    if opts.json_output {
        println!("{}", serde_json::to_string_pretty(&peers)?);
        return Ok(());
    }
    if peers.is_empty() {
        println!("当前还没有已配对的设备。");
        return Ok(());
    }
    println!("已配对设备（共 {} 台）：", peers.len());
    for p in peers {
        println!(
            "  - {} | 身份码：{} | 读文件:{} 写文件:{} 开软件:{} 管进程:{}",
            p.name,
            p.endpoint_id,
            if p.permissions.fs_read { "开" } else { "关" },
            if p.permissions.fs_write { "开" } else { "关" },
            if p.permissions.app_launch { "开" } else { "关" },
            if p.permissions.proc_manage { "开" } else { "关" },
        );
    }
    Ok(())
}

fn cmd_peer_rename(opts: &GlobalOpts, args: &[String]) -> Result<()> {
    let target = args.first().context("缺少设备名称或身份码")?;
    let new_name = args.get(1).context("缺少新设备名称")?.trim().to_string();
    if new_name.is_empty() {
        bail!("新名称不能为空");
    }
    let store = open_local_storage(opts)?;
    let peer_id = resolve_peer_id(&store.list_peers(), target)?;
    store.rename_peer(&peer_id, new_name.clone())?;
    println!("已将设备重命名为：{new_name}");
    Ok(())
}

fn cmd_peer_remove(opts: &GlobalOpts, args: &[String]) -> Result<()> {
    let target = args.first().context("缺少要移除的设备名称或身份码")?;
    let store = open_local_storage(opts)?;
    let peer_id = resolve_peer_id(&store.list_peers(), target)?;
    store.remove_peer(&peer_id)?;
    println!("已移除设备：{peer_id}");
    Ok(())
}

fn cmd_audits(opts: &GlobalOpts, args: &[String]) -> Result<()> {
    let limit = match args.first() {
        Some(s) => s.parse::<usize>().context("条数需为正整数")?,
        None => 30,
    };
    let store = open_local_storage(opts)?;
    let entries = store.read_recent_audits(limit)?;
    if opts.json_output {
        println!("{}", serde_json::to_string_pretty(&entries)?);
        return Ok(());
    }
    if entries.is_empty() {
        println!("暂无操作记录。");
        return Ok(());
    }
    for e in entries {
        println!(
            "[{}] {} ({}) | {} | {} | {}",
            e.timestamp_ms,
            e.peer_name,
            &e.peer_id[e.peer_id.len().saturating_sub(8)..],
            e.action,
            e.detail,
            if e.ok { "成功" } else { "拒绝/失败" }
        );
    }
    Ok(())
}

async fn cmd_pair(opts: &GlobalOpts, args: &[String]) -> Result<()> {
    let first = args.first().context("缺少对方的邀请码或身份码")?;
    let parsed = parse_invite_or_id(first)?;
    let mut code_opt = parsed.code;
    let mut mutual = true;

    for extra in &args[1..] {
        if extra == "--one-way" {
            mutual = false;
        } else if code_opt.is_none() {
            code_opt = Some(extra.clone());
        } else {
            bail!("pair 不认识多余参数：{extra}");
        }
    }
    let code = code_opt.context("缺少 6 位配对码（请提供完整邀请码，或在身份码后附上 6 位配对码）")?;

    let node = start_client_node(opts).await?;
    println!("正在连接对方并发起配对，请在对方电脑上点击“允许”……");
    let res = node
        .pair_with_peer(
            parsed.endpoint_id,
            parsed.direct_addrs,
            parsed.relay_url,
            &code,
            mutual,
            Permissions::default(),
        )
        .await;
    node.shutdown().await;
    let record = res.map_err(|e| anyhow!(e.to_user_message()))?;
    println!("配对成功！已添加设备：{} ({})", record.name, record.endpoint_id);
    Ok(())
}

async fn cmd_ping(opts: &GlobalOpts, args: &[String]) -> Result<()> {
    let target = args.first().context("缺少目标设备")?;
    let node = start_client_node(opts).await?;
    let peer_id = resolve_peer_id(&node.list_peers(), target)?;
    let res = node.remote_ping(&peer_id).await;
    let state = node.peer_state(&peer_id);
    node.shutdown().await;
    let (_reply, rtt) = res.map_err(|e| anyhow!(e.to_user_message()))?;
    if opts.json_output {
        println!(
            "{}",
            serde_json::json!({
                "peer_id": peer_id,
                "rtt_ms": rtt.as_secs_f64() * 1000.0,
                "state": state,
            })
        );
    } else {
        println!("往返延迟：{:.2} 毫秒", rtt.as_secs_f64() * 1000.0);
    }
    Ok(())
}

async fn cmd_info(opts: &GlobalOpts, args: &[String]) -> Result<()> {
    let target = args.first().context("缺少目标设备")?;
    let node = start_client_node(opts).await?;
    let peer_id = resolve_peer_id(&node.list_peers(), target)?;
    let res = node.remote_info(&peer_id).await;
    node.shutdown().await;
    let info = res.map_err(|e| anyhow!(e.to_user_message()))?;
    if opts.json_output {
        println!("{}", serde_json::to_string_pretty(&info)?);
    } else {
        println!("电脑名称：{}（用户：{}）", info.computer_name, info.user_name);
        println!("系统类型：{}（程序版本 {}）", info.os, info.app_version);
        println!("桌面状态：{}", info.desktop.summary);
        println!(
            "物理内存：可用 {} / 总计 {}",
            fmt_bytes(info.memory.available_bytes),
            fmt_bytes(info.memory.total_bytes)
        );
        for d in info.disks {
            println!(
                "磁盘 {} ({})：可用 {} / 总计 {}",
                d.mount_point,
                d.label,
                fmt_bytes(d.available_bytes),
                fmt_bytes(d.total_bytes)
            );
        }
    }
    Ok(())
}

async fn cmd_roots(opts: &GlobalOpts, args: &[String]) -> Result<()> {
    let target = args.first().context("缺少目标设备")?;
    let node = start_client_node(opts).await?;
    let peer_id = resolve_peer_id(&node.list_peers(), target)?;
    let res = node.remote_roots(&peer_id).await;
    node.shutdown().await;
    let roots = res.map_err(|e| anyhow!(e.to_user_message()))?;
    if opts.json_output {
        println!("{}", serde_json::to_string_pretty(&roots)?);
        return Ok(());
    }
    if !roots.drives.is_empty() {
        println!("盘符：");
        for d in roots.drives {
            println!(
                "  {: <16} {}（可用 {}）",
                d.path,
                d.name,
                fmt_bytes(d.available_bytes.unwrap_or(0))
            );
        }
    }
    if !roots.special_folders.is_empty() {
        println!("常用文件夹：");
        for f in roots.special_folders {
            println!("  {: <8} {}", f.name, f.path);
        }
    }
    Ok(())
}

async fn cmd_ls(opts: &GlobalOpts, args: &[String]) -> Result<()> {
    let target = args.first().context("缺少目标设备")?;
    let Some(path) = args.get(1) else {
        return cmd_roots(opts, args).await;
    };
    let node = start_client_node(opts).await?;
    let peer_id = resolve_peer_id(&node.list_peers(), target)?;
    let res = node.remote_list(&peer_id, path).await;
    node.shutdown().await;
    let entries = res.map_err(|e| anyhow!(e.to_user_message()))?;
    if opts.json_output {
        println!("{}", serde_json::to_string_pretty(&entries)?);
        return Ok(());
    }
    println!("{path}（共 {} 项）：", entries.len());
    for item in entries {
        match item.kind {
            EntryKind::Dir => println!("  [文件夹]    {}", item.name),
            EntryKind::Symlink => println!("  [链接]      {}", item.name),
            EntryKind::File => println!("  {:>10}  {}", fmt_bytes(item.size), item.name),
        }
    }
    Ok(())
}

async fn cmd_stat(opts: &GlobalOpts, args: &[String]) -> Result<()> {
    let target = args.first().context("缺少目标设备")?;
    let path = args.get(1).context("缺少远端文件或文件夹路径")?;
    let node = start_client_node(opts).await?;
    let peer_id = resolve_peer_id(&node.list_peers(), target)?;
    let res = node.remote_stat(&peer_id, path).await;
    node.shutdown().await;
    let stat = res.map_err(|e| anyhow!(e.to_user_message()))?;
    if opts.json_output {
        println!("{}", serde_json::to_string_pretty(&stat.entry)?);
    } else {
        println!("名称：{}", stat.entry.name);
        println!("路径：{}", stat.entry.path);
        println!("类型：{:?}", stat.entry.kind);
        println!("大小：{}", fmt_bytes(stat.entry.size));
        println!("只读：{}", if stat.entry.readonly { "是" } else { "否" });
    }
    Ok(())
}

async fn cmd_mkdir(opts: &GlobalOpts, args: &[String]) -> Result<()> {
    let target = args.first().context("缺少目标设备")?;
    let path = args.get(1).context("缺少要创建的远端文件夹路径")?;
    let parents = args[2..].iter().any(|a| a == "-p" || a == "--parents");
    let node = start_client_node(opts).await?;
    let peer_id = resolve_peer_id(&node.list_peers(), target)?;
    let res = node
        .remote_mkdir(
            &peer_id,
            FsMkdirArgs {
                path: path.clone(),
                parents,
            },
        )
        .await;
    node.shutdown().await;
    let reply = res.map_err(|e| anyhow!(e.to_user_message()))?;
    println!("已创建文件夹：{}", reply.path);
    Ok(())
}

async fn cmd_rename(opts: &GlobalOpts, args: &[String]) -> Result<()> {
    let target = args.first().context("缺少目标设备")?;
    let path = args.get(1).context("缺少原路径")?;
    let new_name = args.get(2).context("缺少新名称")?;
    let node = start_client_node(opts).await?;
    let peer_id = resolve_peer_id(&node.list_peers(), target)?;
    let res = node
        .remote_rename(
            &peer_id,
            FsRenameArgs {
                path: path.clone(),
                new_name: new_name.clone(),
            },
        )
        .await;
    node.shutdown().await;
    let reply = res.map_err(|e| anyhow!(e.to_user_message()))?;
    println!("已重命名：{} -> {}", reply.old_path, reply.new_path);
    Ok(())
}

async fn cmd_copy_or_move(opts: &GlobalOpts, args: &[String], is_move: bool) -> Result<()> {
    let target = args.first().context("缺少目标设备")?;
    let src = args.get(1).context("缺少源路径")?;
    let dest_dir = args.get(2).context("缺少目标目录")?;
    let mut conflict = ConflictPolicy::Fail;
    let mut iter = args[3..].iter();
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--conflict" => {
                let val = iter.next().context("--conflict 后面缺少策略值")?;
                conflict = parse_conflict_flag(val)?;
            }
            other => bail!("不认识的参数：{other}"),
        }
    }

    let node = start_client_node(opts).await?;
    let peer_id = resolve_peer_id(&node.list_peers(), target)?;
    let req = FsCopyMoveArgs {
        sources: vec![src.clone()],
        dest_dir: dest_dir.clone(),
        conflict,
    };
    let task_res = if is_move {
        node.remote_move(&peer_id, req).await
    } else {
        node.remote_copy(&peer_id, req).await
    };
    let task = match task_res {
        Ok(t) => t,
        Err(e) => {
            node.shutdown().await;
            bail!(e.to_user_message());
        }
    };

    let wait_res = node
        .wait_remote_task(&peer_id, &task.task_id, Duration::from_secs(3600))
        .await;
    node.shutdown().await;
    let done = wait_res.map_err(|e| anyhow!(e.to_user_message()))?;
    println!(
        "{}完成（处理 {} 字节）。",
        if is_move { "移动" } else { "复制" },
        done.processed_bytes
    );
    Ok(())
}

async fn cmd_rm(opts: &GlobalOpts, args: &[String]) -> Result<()> {
    let target = args.first().context("缺少目标设备")?;
    let path = args.get(1).context("缺少要删除的远端路径")?;
    let permanent = args[2..].iter().any(|a| a == "--permanent");

    let node = start_client_node(opts).await?;
    let peer_id = resolve_peer_id(&node.list_peers(), target)?;
    let res = node
        .remote_delete(
            &peer_id,
            FsDeleteArgs {
                paths: vec![path.clone()],
                permanent,
            },
        )
        .await;
    node.shutdown().await;
    let reply = res.map_err(|e| anyhow!(e.to_user_message()))?;
    println!(
        "已{} {} 项。",
        if permanent { "永久删除" } else { "移入对方回收站" },
        reply.deleted_count
    );
    Ok(())
}

async fn cmd_upload(opts: &GlobalOpts, args: &[String]) -> Result<()> {
    let target = args.first().context("缺少目标设备")?;
    let local_str = args.get(1).context("缺少本地文件路径")?;
    let remote_str = args.get(2).context("缺少远端保存路径")?;
    let mut conflict = ConflictPolicy::Fail;
    let mut iter = args[3..].iter();
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--conflict" => {
                let v = iter.next().context("--conflict 后面缺少策略值")?;
                conflict = parse_conflict_flag(v)?;
            }
            other => bail!("upload 不认识参数：{other}"),
        }
    }

    let local_abs = std::fs::canonicalize(local_str)
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_default().join(local_str));
    let node = start_client_node(opts).await?;
    let peer_id = resolve_peer_id(&node.list_peers(), target)?;

    let cancel = CancellationToken::new();
    let c_sig = cancel.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            c_sig.cancel();
        }
    });

    let res = node
        .upload_file(
            &peer_id,
            &local_abs,
            remote_str,
            conflict,
            cancel,
            Some(Arc::new(|done, total, speed| {
                let pct = if total > 0 {
                    (done as f64 / total as f64) * 100.0
                } else {
                    100.0
                };
                eprint!(
                    "\r上传进度：{pct:5.1}% ({} / {}，{:.1} MB/s)   ",
                    fmt_bytes(done),
                    fmt_bytes(total),
                    speed / 1_048_576.0
                );
            })),
        )
        .await;
    eprintln!();
    node.shutdown().await;
    let done = res.map_err(|e| anyhow!(e.to_user_message()))?;
    println!(
        "上传完成：{}（写入 {}）",
        done.target_path,
        fmt_bytes(done.bytes_written)
    );
    Ok(())
}

async fn cmd_download(opts: &GlobalOpts, args: &[String]) -> Result<()> {
    let target = args.first().context("缺少目标设备")?;
    let remote_str = args.get(1).context("缺少远端文件路径")?;
    let local_str = args.get(2).context("缺少本地保存路径")?;
    let mut conflict = ConflictPolicy::Fail;
    let mut iter = args[3..].iter();
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--conflict" => {
                let v = iter.next().context("--conflict 后面缺少策略值")?;
                conflict = parse_conflict_flag(v)?;
            }
            other => bail!("download 不认识参数：{other}"),
        }
    }

    let local_path = PathBuf::from(local_str);
    let local_abs = if local_path.is_absolute() {
        local_path
    } else {
        std::env::current_dir()?.join(local_path)
    };

    let node = start_client_node(opts).await?;
    let peer_id = resolve_peer_id(&node.list_peers(), target)?;

    let cancel = CancellationToken::new();
    let c_sig = cancel.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            c_sig.cancel();
        }
    });

    let res = node
        .download_file(
            &peer_id,
            remote_str,
            &local_abs,
            conflict,
            cancel,
            Some(Arc::new(|done, total, speed| {
                let pct = if total > 0 {
                    (done as f64 / total as f64) * 100.0
                } else {
                    100.0
                };
                eprint!(
                    "\r下载进度：{pct:5.1}% ({} / {}，{:.1} MB/s)   ",
                    fmt_bytes(done),
                    fmt_bytes(total),
                    speed / 1_048_576.0
                );
            })),
        )
        .await;
    eprintln!();
    node.shutdown().await;
    let done = res.map_err(|e| anyhow!(e.to_user_message()))?;
    println!(
        "下载完成：{}（共 {}）",
        done.target_path,
        fmt_bytes(done.bytes_written)
    );
    Ok(())
}

async fn cmd_apps(opts: &GlobalOpts, args: &[String]) -> Result<()> {
    let target = args.first().context("缺少目标设备")?;
    let mut refresh = false;
    let mut keyword = None;
    for arg in &args[1..] {
        if arg == "--refresh" {
            refresh = true;
        } else if keyword.is_none() {
            keyword = Some(arg.to_lowercase());
        }
    }

    let node = start_client_node(opts).await?;
    let peer_id = resolve_peer_id(&node.list_peers(), target)?;
    let res = node.remote_apps(&peer_id, refresh).await;
    node.shutdown().await;
    let reply = res.map_err(|e| anyhow!(e.to_user_message()))?;

    let filtered: Vec<_> = reply
        .apps
        .into_iter()
        .filter(|a| {
            keyword
                .as_ref()
                .is_none_or(|kw| a.name.to_lowercase().contains(kw))
        })
        .collect();

    if opts.json_output {
        println!("{}", serde_json::to_string_pretty(&filtered)?);
        return Ok(());
    }
    println!("对方已装软件（匹配 {} 个）：", filtered.len());
    for app in filtered {
        println!("  {}  [{}]", app.name, app.app_id);
    }
    Ok(())
}

async fn cmd_launch(opts: &GlobalOpts, args: &[String]) -> Result<()> {
    let target_peer = args.first().context("缺少目标设备")?;
    let target_app = args.get(1).context("缺少要打开的软件名称、标识或文件路径")?;
    let mut kind = LaunchKind::AppName;
    let mut launch_args = None;

    let mut iter = args[2..].iter();
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--by-name" => kind = LaunchKind::AppName,
            "--by-id" => kind = LaunchKind::AppId,
            "--by-path" => kind = LaunchKind::PathOrTarget,
            "--args" => {
                let v = iter.next().context("--args 后面缺少启动参数")?;
                launch_args = Some(v.clone());
            }
            other => bail!("launch 不认识参数：{other}"),
        }
    }

    let node = start_client_node(opts).await?;
    let peer_id = resolve_peer_id(&node.list_peers(), target_peer)?;
    let res = node
        .remote_launch(
            &peer_id,
            AppLaunchArgs {
                target: target_app.clone(),
                kind,
                args: launch_args,
                skip_window_wait: false,
            },
        )
        .await;
    node.shutdown().await;
    let reply = res.map_err(|e| anyhow!(e.to_user_message()))?;

    if opts.json_output {
        println!("{}", serde_json::to_string_pretty(&reply)?);
        return Ok(());
    }
    println!(
        "已在对方电脑启动：{}",
        reply.resolved_name.as_deref().unwrap_or(target_app)
    );
    if !reply.new_windows.is_empty() {
        println!("新出现的窗口：{}", reply.new_windows.join("；"));
    }
    if let Some(hint) = reply.hint {
        println!("提示：{hint}");
    }
    println!("对方桌面状态：{}", reply.desktop.summary);
    Ok(())
}

async fn cmd_ps(opts: &GlobalOpts, args: &[String]) -> Result<()> {
    let target = args.first().context("缺少目标设备")?;
    let keyword = args.get(1).map(|s| s.to_lowercase());

    let node = start_client_node(opts).await?;
    let peer_id = resolve_peer_id(&node.list_peers(), target)?;
    let res = node.remote_processes(&peer_id).await;
    node.shutdown().await;
    let reply = res.map_err(|e| anyhow!(e.to_user_message()))?;

    let filtered: Vec<_> = reply
        .processes
        .into_iter()
        .filter(|p| {
            keyword.as_ref().is_none_or(|kw| {
                p.name.to_lowercase().contains(kw)
                    || p.window_title
                        .as_ref()
                        .is_some_and(|t| t.to_lowercase().contains(kw))
            })
        })
        .collect();

    if opts.json_output {
        println!("{}", serde_json::to_string_pretty(&filtered)?);
        return Ok(());
    }
    println!(
        "{:>8}  {:>10}  {:>6}  {: <28}  窗口标题",
        "PID", "内存", "CPU", "进程名"
    );
    for p in filtered {
        println!(
            "{:>8}  {:>10}  {:>5.1}%  {: <28}  {}",
            p.pid,
            fmt_bytes(p.memory_bytes),
            p.cpu_percent,
            p.name,
            p.window_title.as_deref().unwrap_or("")
        );
    }
    Ok(())
}

async fn cmd_kill(opts: &GlobalOpts, args: &[String]) -> Result<()> {
    let target = args.first().context("缺少目标设备")?;
    let pid_str = args.get(1).context("缺少要结束的进程 PID")?;
    let pid: u32 = pid_str.parse().context("PID 必须是正整数")?;
    let confirmed = args[2..].iter().any(|a| a == "--yes" || a == "-y");
    if !confirmed {
        bail!("结束进程可能导致对方电脑未保存的数据丢失，请附加 --yes 确认后再执行");
    }

    let node = start_client_node(opts).await?;
    let peer_id = resolve_peer_id(&node.list_peers(), target)?;
    let res = node
        .remote_kill_process(&peer_id, ProcKillArgs { pid, confirmed })
        .await;
    node.shutdown().await;
    let reply = res.map_err(|e| anyhow!(e.to_user_message()))?;
    println!("已结束进程：{} (PID {})", reply.name, reply.pid);
    Ok(())
}
