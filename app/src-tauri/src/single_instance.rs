//! 单实例保护与窗口唤醒（任务 1.14）：
//! 同一数据目录下只允许运行一个桌面实例；重复启动时唤醒已有主窗口并立即退出。

use std::fs;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

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
    /// - 若已有实例在运行：向已有实例发送唤醒信号并返回 `None`。
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
            use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE};
            use windows::Win32::System::Threading::CreateMutexW;
            use windows::core::PCWSTR;

            let name = format!("Local\\PcShareDesktop_{key_hex}");
            let wide: Vec<u16> = OsStr::new(&name).encode_wide().chain(Some(0)).collect();
            unsafe {
                let Ok(handle) = CreateMutexW(None, false, PCWSTR(wide.as_ptr())) else {
                    return None;
                };
                if GetLastError() == ERROR_ALREADY_EXISTS {
                    let _ = CloseHandle(handle);
                    notify_existing_instance(&port_file);
                    return None;
                }
                let raw: HANDLE = handle;
                raw.0 as isize
            }
        };

        #[cfg(not(windows))]
        if port_file.exists() && notify_existing_instance(&port_file) {
            return None;
        }

        let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).ok()?;
        let wake_addr = listener.local_addr().ok()?;
        let _ = fs::write(&port_file, wake_addr.port().to_string());

        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop_clone = stop_flag.clone();
        let cb = Arc::new(on_wakeup);

        let join_handle = std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                if stop_clone.load(Ordering::SeqCst) {
                    break;
                }
                let _ = stream.set_read_timeout(Some(Duration::from_millis(300)));
                let mut buf = [0u8; 16];
                let mut s = stream;
                if let Ok(n) = s.read(&mut buf)
                    && n > 0
                    && buf[..n].starts_with(b"SHOW")
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

fn notify_existing_instance(port_file: &Path) -> bool {
    for _ in 0..8 {
        if let Ok(text) = fs::read_to_string(port_file)
            && let Ok(port) = text.trim().parse::<u16>()
        {
            let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port));
            if let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(400)) {
                let _ = stream.set_write_timeout(Some(Duration::from_millis(400)));
                let _ = stream.write_all(b"SHOW\n");
                let _ = stream.flush();
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(40));
    }
    false
}
