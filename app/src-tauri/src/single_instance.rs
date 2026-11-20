//! 单实例保护与窗口唤醒（任务 1.14）：
//! 同一数据目录下只允许运行一个桌面实例；重复启动时唤醒已有主窗口并立即退出。

use std::fs;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const PORT_FILE_NAME: &str = "ui_instance.port";

pub struct SingleInstanceGuard {
    port_file: PathBuf,
    wake_addr: SocketAddr,
    stop_flag: Arc<AtomicBool>,
    join_handle: Option<std::thread::JoinHandle<()>>,
    #[cfg(windows)]
    mutex_handle: isize,
}

impl SingleInstanceGuard {
    /// 尝试获取当前数据目录的单实例锁。
    /// - 若本进程为首个实例：返回 `Some(guard)`，并在后台监听重复启动发来的唤醒信号调用 `on_wakeup`。
    /// - 若已有实例在运行：向已有实例发送携带随机唤醒令牌的唤醒信号并返回 `None`。
    pub fn try_acquire<F>(data_dir: &Path, on_wakeup: F) -> Option<Self>
    where
        F: Fn() + Send + Sync + 'static,
    {
        let _ = fs::create_dir_all(data_dir);
        let canonical = fs::canonicalize(data_dir).unwrap_or_else(|_| data_dir.to_path_buf());
        let key_hex = {
            let lowered = canonical.to_string_lossy().to_lowercase();
            let h = blake3::hash(lowered.as_bytes()).to_hex();
            h[..24].to_string()
        };
        let port_file = data_dir.join(PORT_FILE_NAME);

        #[cfg(windows)]
        let mutex_handle = {
            use std::ffi::OsStr;
            use std::os::windows::ffi::OsStrExt;
            use windows::Win32::Foundation::{
                CloseHandle, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, GetLastError, HANDLE,
            };
            use windows::Win32::System::Threading::CreateMutexW;
            use windows::core::{HRESULT, PCWSTR};

            let name = format!("Local\\PcShareDesktop_{key_hex}");
            let wide: Vec<u16> = OsStr::new(&name).encode_wide().chain(Some(0)).collect();
            unsafe {
                match CreateMutexW(None, false, PCWSTR(wide.as_ptr())) {
                    Ok(handle) => {
                        if GetLastError() == ERROR_ALREADY_EXISTS {
                            let _ = CloseHandle(handle);
                            notify_existing_instance(&port_file);
                            return None;
                        }
                        let raw: HANDLE = handle;
                        raw.0 as isize
                    }
                    Err(err) => {
                        // 已有管理员权限实例持有该命名互斥锁时，普通权限调用 CreateMutexW 会报 ERROR_ACCESS_DENIED，
                        // 此时仍需通过端口文件与唤醒令牌唤出已有主窗口。
                        if err.code() == HRESULT::from_win32(ERROR_ACCESS_DENIED.0)
                            || GetLastError() == ERROR_ACCESS_DENIED
                        {
                            notify_existing_instance(&port_file);
                        }
                        return None;
                    }
                }
            }
        };

        #[cfg(not(windows))]
        if port_file.exists() && notify_existing_instance(&port_file) {
            return None;
        }

        let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).ok()?;
        let wake_addr = listener.local_addr().ok()?;
        let wake_token = generate_wake_token();
        write_port_file_atomic(&port_file, wake_addr.port(), &wake_token);

        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop_clone = stop_flag.clone();
        let cb = Arc::new(on_wakeup);
        let expected_token = wake_token.into_bytes();

        let join_handle = std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                if stop_clone.load(Ordering::SeqCst) {
                    break;
                }
                let _ = stream.set_read_timeout(Some(Duration::from_millis(300)));
                let mut buf = [0u8; 128];
                let mut s = stream;
                if let Ok(n) = s.read(&mut buf)
                    && n > 0
                    && is_valid_wake_message(&buf[..n], &expected_token)
                {
                    cb();
                }
            }
        });

        Some(Self {
            port_file,
            wake_addr,
            stop_flag,
            join_handle: Some(join_handle),
            #[cfg(windows)]
            mutex_handle,
        })
    }
}

impl Drop for SingleInstanceGuard {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect_timeout(&self.wake_addr, Duration::from_millis(200));
        if let Some(h) = self.join_handle.take() {
            let _ = h.join();
        }
        let _ = fs::remove_file(&self.port_file);
        #[cfg(windows)]
        unsafe {
            use std::ffi::c_void;
            use windows::Win32::Foundation::{CloseHandle, HANDLE};
            let _ = CloseHandle(HANDLE(self.mutex_handle as *mut c_void));
        }
    }
}

fn generate_wake_token() -> String {
    let mut raw = [0u8; 16];
    let mut filled = false;

    #[cfg(windows)]
    {
        use windows::Win32::Security::Cryptography::{
            BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom,
        };
        let status = unsafe { BCryptGenRandom(None, &mut raw, BCRYPT_USE_SYSTEM_PREFERRED_RNG) };
        if status.0 >= 0 {
            filled = true;
        }
    }

    #[cfg(not(windows))]
    {
        if let Ok(mut f) = fs::File::open("/dev/urandom")
            && f.read_exact(&mut raw).is_ok()
        {
            filled = true;
        }
    }

    if !filled {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&std::process::id().to_le_bytes());
        if let Ok(dur) = SystemTime::now().duration_since(UNIX_EPOCH) {
            hasher.update(&dur.as_nanos().to_le_bytes());
        }
        for _ in 0..4 {
            hasher.update(&fastrand::u64(..).to_le_bytes());
        }
        let digest = hasher.finalize();
        raw.copy_from_slice(&digest.as_bytes()[..16]);
    }

    let mut hex = String::with_capacity(32);
    for b in raw {
        use std::fmt::Write as _;
        let _ = write!(&mut hex, "{b:02x}");
    }
    hex
}

fn write_port_file_atomic(port_file: &Path, port: u16, token: &str) {
    let content = format!("{port} {token}\n");
    let tmp = port_file.with_extension(format!("port.{}.tmp", std::process::id()));
    if fs::write(&tmp, &content).is_ok() {
        if fs::rename(&tmp, port_file).is_err() {
            let _ = fs::remove_file(&tmp);
            let _ = fs::write(port_file, &content);
        }
    } else {
        let _ = fs::write(port_file, &content);
    }
}

fn is_valid_wake_message(raw: &[u8], expected_token: &[u8]) -> bool {
    let trimmed = raw.trim_ascii();
    let Some(rest) = trimmed.strip_prefix(b"SHOW ") else {
        return false;
    };
    ct_eq(rest, expected_token)
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() || a.is_empty() {
        return false;
    }
    let mut diff = 0u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn parse_port_file(text: &str) -> Option<(u16, &str)> {
    let mut parts = text.split_whitespace();
    let port = parts.next()?.parse::<u16>().ok()?;
    let token = parts.next()?;
    if token.is_empty() || parts.next().is_some() {
        return None;
    }
    Some((port, token))
}

fn notify_existing_instance(port_file: &Path) -> bool {
    for _ in 0..8 {
        if let Ok(text) = fs::read_to_string(port_file)
            && let Some((port, token)) = parse_port_file(&text)
        {
            let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port));
            if let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(400)) {
                let _ = stream.set_write_timeout(Some(Duration::from_millis(400)));
                let payload = format!("SHOW {token}\n");
                let _ = stream.write_all(payload.as_bytes());
                let _ = stream.flush();
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(40));
    }
    false
}
