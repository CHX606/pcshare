use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

fn agent_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_pcshare-agent"))
}

fn temp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "pcshare_agent_e2e_{}_{}_{}",
        tag,
        std::process::id(),
        fastrand::u64(..)
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn run_cli(data_dir: &Path, args: &[&str]) -> (bool, String, String) {
    let out = Command::new(agent_bin())
        .arg("--local")
        .arg("--data-dir")
        .arg(data_dir)
        .args(args)
        .output()
        .expect("spawn pcshare-agent");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

#[test]
fn stage1_full_workflow_via_agent_cli() {
    let host_data = temp_dir("host_data");
    let client_data = temp_dir("client_data");
    let workspace = temp_dir("workspace");

    // 1. Check `id` command works and persists across runs
    let (ok1, id_out1, err1) = run_cli(&client_data, &["id"]);
    assert!(ok1, "id failed: {err1}");
    let (ok2, id_out2, _) = run_cli(&client_data, &["id"]);
    assert!(ok2);
    assert_eq!(id_out1.trim(), id_out2.trim());
    assert_eq!(id_out1.trim().len(), 64);

    // 2. Start host daemon in background with `--pair --auto-approve`
    let mut host_proc = Command::new(agent_bin())
        .arg("--local")
        .arg("--data-dir")
        .arg(&host_data)
        .args(["serve", "--pair", "--auto-approve"])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn host serve");

    // Read invite code line from host stdout while keeping pipe drained in background
    let stdout = host_proc.stdout.take().expect("host stdout");
    let (invite_tx, invite_rx) = std::sync::mpsc::channel();
    let stdout_drain = std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        let mut sent = false;
        for line in reader.lines().map_while(Result::ok) {
            if !sent
                && let Some(idx) = line.find("pcshare://")
            {
                let _ = invite_tx.send(line[idx..].trim().to_string());
                sent = true;
            }
        }
    });
    let invite = invite_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("host must print pcshare:// invite code");

    // 3. Pair from client CLI using invite code
    let (ok_pair, pair_out, pair_err) = run_cli(&client_data, &["pair", &invite]);
    assert!(ok_pair, "pair failed: {pair_out} / {pair_err}");

    // 4. List paired devices (`peers`) and rename host to "测试被控机"
    let (ok_peers, peers_out, _) = run_cli(&client_data, &["peers"]);
    assert!(ok_peers);
    assert!(!peers_out.trim().is_empty());

    // Extract host 64-char id from invite
    let host_id = invite
        .strip_prefix("pcshare://")
        .unwrap()
        .split('?')
        .next()
        .unwrap();
    let (ok_ren, _, ren_err) = run_cli(&client_data, &["peer-rename", host_id, "测试被控机"]);
    assert!(ok_ren, "peer-rename failed: {ren_err}");

    // 5. Browse roots & mkdir & upload & ls & download & rm using friendly device name "测试被控机"!
    let (ok_roots, roots_out, roots_err) = run_cli(&client_data, &["roots", "测试被控机"]);
    assert!(ok_roots, "roots failed: {roots_err}");
    assert!(roots_out.contains(":\\"));

    let remote_dir = workspace.join("cli_remote_dir");
    let (ok_mk, _, mk_err) = run_cli(
        &client_data,
        &["mkdir", "测试被控机", &remote_dir.to_string_lossy(), "-p"],
    );
    assert!(ok_mk, "mkdir failed: {mk_err}");

    let local_src = workspace.join("hello.txt");
    let content = "通过命令行上传与下载的完整验证内容 1234567890";
    std::fs::write(&local_src, content).unwrap();

    let remote_file = remote_dir.join("uploaded.txt");
    let (ok_up, _, up_err) = run_cli(
        &client_data,
        &[
            "upload",
            "测试被控机",
            &local_src.to_string_lossy(),
            &remote_file.to_string_lossy(),
            "--conflict",
            "overwrite",
        ],
    );
    assert!(ok_up, "upload failed: {up_err}");

    let (ok_ls, ls_out, ls_err) = run_cli(
        &client_data,
        &["ls", "测试被控机", &remote_dir.to_string_lossy()],
    );
    assert!(ok_ls, "ls failed: {ls_err}");
    assert!(ls_out.contains("uploaded.txt"));

    let local_dl = workspace.join("downloaded.txt");
    let (ok_dl, _, dl_err) = run_cli(
        &client_data,
        &[
            "download",
            "测试被控机",
            &remote_file.to_string_lossy(),
            &local_dl.to_string_lossy(),
            "--conflict",
            "overwrite",
        ],
    );
    assert!(ok_dl, "download failed: {dl_err}");
    assert_eq!(std::fs::read_to_string(&local_dl).unwrap(), content);

    // 6. List apps & processes & kill spawned child process via CLI
    let (ok_apps, apps_out, apps_err) = run_cli(&client_data, &["apps", "测试被控机"]);
    assert!(ok_apps, "apps failed: {apps_err}");
    assert!(!apps_out.trim().is_empty());

    let (ok_ps, ps_out, ps_err) = run_cli(&client_data, &["ps", "测试被控机"]);
    assert!(ok_ps, "ps failed: {ps_err}");
    assert!(ps_out.contains("pcshare-agent"));

    let mut dummy = Command::new("ping.exe")
        .args(["127.0.0.1", "-n", "30"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let dummy_pid = dummy.id().to_string();
    let (ok_kill, _, kill_err) =
        run_cli(&client_data, &["kill", "测试被控机", &dummy_pid, "--yes"]);
    assert!(ok_kill, "kill failed: {kill_err}");
    let _ = dummy.wait();

    // 7. Delete remote file (moves to recycle bin by default)
    let (ok_rm, _, rm_err) = run_cli(
        &client_data,
        &["rm", "测试被控机", &remote_file.to_string_lossy()],
    );
    assert!(ok_rm, "rm failed: {rm_err}");
    assert!(!remote_file.exists());

    let _ = host_proc.kill();
    let _ = host_proc.wait();
    let _ = stdout_drain.join();

    // 8. Check host recorded audit logs for remote actions
    let (ok_aud, aud_out, aud_err) = run_cli(&host_data, &["audits"]);
    assert!(ok_aud, "audits failed: {aud_err}");
    assert!(aud_out.contains("fs.write"));

    let _ = std::fs::remove_dir_all(host_data);
    let _ = std::fs::remove_dir_all(client_data);
    let _ = std::fs::remove_dir_all(workspace);
}
