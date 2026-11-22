use super::*;

pub(super) fn filetime_to_u64(ft: FILETIME) -> u64 {
    ((ft.dwHighDateTime as u64) << 32) | (ft.dwLowDateTime as u64)
}

pub(super) fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub(super) fn is_critical_process_name(pid: u32, name: &str, self_pid: u32) -> bool {
    if pid <= 4 || pid == self_pid {
        return true;
    }
    matches!(
        name.to_ascii_lowercase().as_str(),
        "system"
            | "registry"
            | "smss.exe"
            | "csrss.exe"
            | "wininit.exe"
            | "services.exe"
            | "lsass.exe"
            | "winlogon.exe"
            | "dwm.exe"
            | "fontdrvhost.exe"
            | "memory compression"
    )
}

pub(super) unsafe extern "system" fn collect_visible_window(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let list = unsafe { &mut *(lparam.0 as *mut Vec<WindowSummary>) };
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() {
            return BOOL(1);
        }
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return BOOL(1);
        }
        let mut buf = vec![0u16; len as usize + 1];
        let copied = GetWindowTextW(hwnd, &mut buf);
        if copied <= 0 {
            return BOOL(1);
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        list.push(WindowSummary {
            hwnd: hwnd.0 as usize,
            pid,
            title: String::from_utf16_lossy(&buf[..copied as usize]),
        });
    }
    BOOL(1)
}

pub(super) fn known_folder_path(folder_id: &GUID) -> Option<PathBuf> {
    unsafe {
        let pwstr = SHGetKnownFolderPath(folder_id, KF_FLAG_DEFAULT, None).ok()?;
        let path_str = pwstr_to_string_and_free(pwstr);
        if path_str.is_empty() {
            None
        } else {
            Some(PathBuf::from(path_str))
        }
    }
}

pub(super) fn shell_execute_blocking(target: String, args: Option<String>) -> Result<(), PlatformError> {
    let op = wide_null("open");
    let file = wide_null(&target);
    let params = args.filter(|a| !a.is_empty()).map(|a| wide_null(&a));
    let params_pcwstr = params
        .as_ref()
        .map_or(PCWSTR::null(), |p| PCWSTR(p.as_ptr()));

    let _com = ComGuard::init_sta();
    let instance = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(op.as_ptr()),
            PCWSTR(file.as_ptr()),
            params_pcwstr,
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };

    let code = instance.0 as isize;
    if code > 32 {
        return Ok(());
    }
    match code {
        2 => Err(PlatformError::NotFound(format!(
            "找不到这个程序或文件：{target}"
        ))),
        3 => Err(PlatformError::NotFound(format!(
            "找不到这个路径：{target}"
        ))),
        5 => Err(PlatformError::ElevationRequired(
            "打开该程序需要管理员权限，或系统拒绝访问".to_string(),
        )),
        8 => Err(PlatformError::System(
            "对方电脑内存不足，无法启动程序".to_string(),
        )),
        26 | 28 | 29 | 30 => Err(PlatformError::Busy("系统正忙，暂时打不开".to_string())),
        27 | 31 => Err(PlatformError::InvalidInput(
            "这种文件在对方电脑上没有关联的打开方式".to_string(),
        )),
        32 => Err(PlatformError::Busy(
            "目标文件或组件正被占用，暂时无法打开".to_string(),
        )),
        _ => Err(PlatformError::System(format!(
            "系统拒绝了这次打开（错误码 {code}）"
        ))),
    }
}

pub(super) fn system_and_proc_times() -> (u64, HashMap<u32, (String, u64, u64)>) {
    let mut sys_total = 0u64;
    unsafe {
        let mut idle = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        if GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)).is_ok() {
            sys_total = filetime_to_u64(kernel).saturating_add(filetime_to_u64(user));
        }
    }

    let mut procs = HashMap::new();
    unsafe {
        let Ok(snapshot_raw) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return (sys_total, procs);
        };
        let Some(snapshot) = OwnedHandle::new(snapshot_raw) else {
            return (sys_total, procs);
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snapshot.raw(), &mut entry).is_ok() {
            loop {
                let pid = entry.th32ProcessID;
                let name_len = entry
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(entry.szExeFile.len());
                let name = String::from_utf16_lossy(&entry.szExeFile[..name_len]);

                let mut mem_bytes = 0u64;
                let mut proc_time = 0u64;
                if pid > 0
                    && let Ok(handle_raw) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)
                    && let Some(handle) = OwnedHandle::new(handle_raw)
                {
                    let mut pmc = PROCESS_MEMORY_COUNTERS {
                        cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
                        ..Default::default()
                    };
                    if K32GetProcessMemoryInfo(
                        handle.raw(),
                        &mut pmc,
                        std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
                    )
                    .as_bool()
                    {
                        mem_bytes = pmc.WorkingSetSize as u64;
                    }

                    let mut create = FILETIME::default();
                    let mut exit = FILETIME::default();
                    let mut ktime = FILETIME::default();
                    let mut utime = FILETIME::default();
                    if GetProcessTimes(
                        handle.raw(),
                        &mut create,
                        &mut exit,
                        &mut ktime,
                        &mut utime,
                    )
                    .is_ok()
                    {
                        proc_time = filetime_to_u64(ktime).saturating_add(filetime_to_u64(utime));
                    }
                }
                procs.insert(pid, (name, mem_bytes, proc_time));

                if Process32NextW(snapshot.raw(), &mut entry).is_err() {
                    break;
                }
            }
        }
    }
    (sys_total, procs)
}

/// 在同一进程句柄上通过 `QueryFullProcessImageNameW` 查询可执行文件名，消除 PID 复用 TOCTOU。
pub(super) fn query_process_exe_name(handle: HANDLE, pid: u32) -> Result<String, PlatformError> {
    let mut cap = 512usize;
    loop {
        let mut buf = vec![0u16; cap];
        let mut len = buf.len() as u32;
        match unsafe {
            QueryFullProcessImageNameW(
                handle,
                PROCESS_NAME_WIN32,
                PWSTR(buf.as_mut_ptr()),
                &mut len,
            )
        } {
            Ok(()) => {
                let full = String::from_utf16_lossy(&buf[..len as usize]);
                let name = full
                    .rsplit(['\\', '/'])
                    .next()
                    .filter(|s| !s.is_empty())
                    .unwrap_or(&full)
                    .to_string();
                return Ok(name);
            }
            Err(err) if is_win32_error(&err, ERROR_INSUFFICIENT_BUFFER) && cap < 4096 => {
                cap *= 2;
            }
            Err(err) if is_win32_error(&err, ERROR_GEN_FAILURE) => {
                return Err(PlatformError::NotFound(format!(
                    "找不到进程（PID {pid}），它可能已经退出"
                )));
            }
            Err(err) => {
                return Err(PlatformError::System(format!(
                    "查询目标进程名称失败：{err}"
                )));
            }
        }
    }
}

pub(super) fn enumerate_start_apps_native_sta() -> Result<Vec<InstalledAppInfo>, PlatformError> {
    let _com = ComGuard::init_sta();
    unsafe {
        let folder: IShellItem =
            SHGetKnownFolderItem(&FOLDERID_AppsFolder, KF_FLAG_DEFAULT, None)
                .map_err(|e| PlatformError::System(format!("无法打开系统应用程序目录：{e}")))?;
        let enumerator: IEnumShellItems = folder
            .BindToHandler(None::<&IBindCtx>, &BHID_EnumItems)
            .map_err(|e| PlatformError::System(format!("无法枚举已安装软件：{e}")))?;

        let mut seen = std::collections::HashSet::new();
        let mut apps = Vec::with_capacity(256);
        loop {
            let mut batch: [Option<IShellItem>; 32] = Default::default();
            let mut fetched = 0u32;
            let _ = enumerator.Next(&mut batch, Some(&mut fetched));
            if fetched == 0 {
                break;
            }
            for item in batch.into_iter().take(fetched as usize).flatten() {
                let name = match item.GetDisplayName(SIGDN_NORMALDISPLAY) {
                    Ok(pw) => pwstr_to_string_and_free(pw),
                    Err(_) => String::new(),
                };
                let app_id = match item.GetDisplayName(SIGDN_PARENTRELATIVEPARSING) {
                    Ok(pw) => pwstr_to_string_and_free(pw),
                    Err(_) => String::new(),
                };
                let name_trim = name.trim();
                let id_trim = app_id.trim();
                if !name_trim.is_empty()
                    && !id_trim.is_empty()
                    && seen.insert(id_trim.to_string())
                {
                    apps.push(InstalledAppInfo {
                        name: name_trim.to_string(),
                        app_id: id_trim.to_string(),
                    });
                }
            }
        }
        apps.sort_by_key(|a| a.name.to_lowercase());
        Ok(apps)
    }
}

pub(super) fn compute_installed_apps_fingerprint() -> u64 {
    fn hash_dir_shallow(h: &mut u64, dir: &Path, depth: u8) {
        let Ok(meta) = std::fs::metadata(dir) else {
            return;
        };
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        *h = h
            .wrapping_mul(1099511628211)
            .wrapping_add(mtime ^ 0x9e3779b97f4a7c15);
        if depth == 0 {
            return;
        }
        if let Ok(rd) = std::fs::read_dir(dir) {
            let mut count = 0u64;
            for entry in rd.flatten() {
                count += 1;
                if depth > 1
                    && let Ok(ft) = entry.file_type()
                    && ft.is_dir()
                {
                    hash_dir_shallow(h, &entry.path(), depth - 1);
                }
            }
            *h = h.wrapping_mul(1099511628211).wrapping_add(count);
        }
    }

    let mut fp = 14695981039346656037u64;
    if let Some(pd) = std::env::var_os("ProgramData") {
        hash_dir_shallow(
            &mut fp,
            &PathBuf::from(pd).join(r"Microsoft\Windows\Start Menu\Programs"),
            2,
        );
    }
    if let Some(ad) = std::env::var_os("APPDATA") {
        hash_dir_shallow(
            &mut fp,
            &PathBuf::from(ad).join(r"Microsoft\Windows\Start Menu\Programs"),
            2,
        );
    }
    if let Some(lad) = std::env::var_os("LOCALAPPDATA") {
        hash_dir_shallow(&mut fp, &PathBuf::from(lad).join("Packages"), 1);
    }
    fp
}

#[derive(Serialize, Deserialize)]
pub(super) struct PersistedAppsIndex {
    fingerprint: u64,
    #[serde(default)]
    updated_at_unix_secs: u64,
    apps: Vec<InstalledAppInfo>,
}

pub(super) fn apps_index_cache_path() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("PcShare")
        .join("apps_index_v2.json")
}

pub(super) fn read_persisted_apps_index(
    cache_path: &Path,
    expected_fp: u64,
    now_secs: u64,
) -> Option<Vec<InstalledAppInfo>> {
    let raw = std::fs::read_to_string(cache_path).ok()?;
    let parsed = serde_json::from_str::<PersistedAppsIndex>(&raw).ok()?;
    let fresh = parsed.updated_at_unix_secs > 0
        && now_secs.saturating_sub(parsed.updated_at_unix_secs) < APP_CACHE_TTL.as_secs();
    if parsed.fingerprint == expected_fp && fresh && !parsed.apps.is_empty() {
        Some(parsed.apps)
    } else {
        None
    }
}

pub(super) fn write_atomic_bytes(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    std::fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(
        "{}.{}.{}.tmp",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("cache"),
        std::process::id(),
        fastrand::u64(..)
    ));
    let write_res = (|| -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)
    })();
    if write_res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    write_res
}

pub(super) fn extract_app_icon_png_sta(app_id: &str) -> Option<Vec<u8>> {
    let _com = ComGuard::init_sta();
    unsafe {
        let parsing = wide_null(&format!(r"shell:AppsFolder\{app_id}"));
        let factory: IShellItemImageFactory =
            SHCreateItemFromParsingName(PCWSTR(parsing.as_ptr()), None::<&IBindCtx>).ok()?;
        let hbm = factory
            .GetImage(
                SIZE { cx: 32, cy: 32 },
                SIIGBF_ICONONLY | SIIGBF_BIGGERSIZEOK,
            )
            .ok()?;

        let mut bm = BITMAP::default();
        let got_obj = GetObjectW(
            HGDIOBJ(hbm.0),
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut bm as *mut _ as *mut c_void),
        );
        if got_obj == 0 || bm.bmWidth <= 0 || bm.bmHeight <= 0 {
            let _ = DeleteObject(HGDIOBJ(hbm.0));
            return None;
        }
        let w = bm.bmWidth as usize;
        let h = bm.bmHeight.unsigned_abs() as usize;
        let byte_len = w.checked_mul(h).and_then(|px| px.checked_mul(4))?;

        let hdc = CreateCompatibleDC(None);
        if hdc.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(hbm.0));
            return None;
        }

        let mut bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w as i32,
                biHeight: -(h as i32), // top-down DIB
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut pixels = vec![0u8; byte_len];
        let lines = GetDIBits(
            hdc,
            hbm,
            0,
            h as u32,
            Some(pixels.as_mut_ptr() as *mut c_void),
            &mut bmi,
            DIB_RGB_COLORS,
        );
        let _ = DeleteDC(hdc);
        let _ = DeleteObject(HGDIOBJ(hbm.0));
        if lines == 0 {
            return None;
        }

        // BGRA -> RGBA（若全图 alpha 均为 0，说明是传统 24 位图标转成 32 位，补不透明 alpha=255）
        let any_alpha = pixels.as_chunks::<4>().0.iter().any(|px| px[3] != 0);
        for px in pixels.as_chunks_mut::<4>().0 {
            px.swap(0, 2);
            if !any_alpha {
                px[3] = 255;
            }
        }

        let mut png_bytes = Vec::with_capacity(2048);
        {
            let mut encoder = png::Encoder::new(&mut png_bytes, w as u32, h as u32);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().ok()?;
            writer.write_image_data(&pixels).ok()?;
        }
        Some(png_bytes)
    }
}

pub(super) async fn query_start_apps(
    expected_fp: u64,
    allow_disk_index: bool,
) -> Result<Vec<InstalledAppInfo>, PlatformError> {
    let cache_path = apps_index_cache_path();
    if allow_disk_index {
        let path_for_read = cache_path.clone();
        let now_secs = now_unix_secs();
        if let Ok(Some(cached)) = tokio::task::spawn_blocking(move || {
            read_persisted_apps_index(&path_for_read, expected_fp, now_secs)
        })
        .await
        {
            return Ok(cached);
        }
    }

    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let _ = tx.send(enumerate_start_apps_native_sta());
    });
    let apps = rx
        .await
        .map_err(|_| PlatformError::System("枚举已安装软件的后台线程意外中止".to_string()))??;

    let apps_for_disk = apps.clone();
    let _ = tokio::task::spawn_blocking(move || {
        if let Ok(json) = serde_json::to_vec(&PersistedAppsIndex {
            fingerprint: expected_fp,
            updated_at_unix_secs: now_unix_secs(),
            apps: apps_for_disk,
        }) {
            let _ = write_atomic_bytes(&cache_path, &json);
        }
    })
    .await;

    Ok(apps)
}

/// 在独立 STA 线程中调用现代 `IFileOperation` 移入回收站，兼容超长路径与损坏符号链接。
pub(super) fn move_to_trash_sta(paths: Vec<PathBuf>) -> Result<(), PlatformError> {
    let _com = ComGuard::init_sta();
    unsafe {
        let op: IFileOperation = CoCreateInstance(&FileOperation, None, CLSCTX_INPROC_SERVER)
            .map_err(|e| PlatformError::System(format!("初始化系统回收站组件失败：{e}")))?;
        op.SetOperationFlags(
            FOF_ALLOWUNDO
                | FOF_NOCONFIRMATION
                | FOF_NOERRORUI
                | FOF_SILENT
                | FOFX_RECYCLEONDELETE
                | FOFX_ADDUNDORECORD,
        )
        .map_err(|e| PlatformError::System(format!("配置系统回收站选项失败：{e}")))?;

        for p in &paths {
            let item = create_shell_item_for_trash(p)?;
            op.DeleteItem(&item, None::<&IFileOperationProgressSink>)
                .map_err(|e| {
                    PlatformError::System(format!(
                        "添加待回收项目失败（{}）：{e}",
                        p.display()
                    ))
                })?;
        }

        op.PerformOperations()
            .map_err(|e| PlatformError::System(format!("无法将文件移入回收站：{e}")))?;
        if op
            .GetAnyOperationsAborted()
            .map(|b| b.as_bool())
            .unwrap_or(false)
        {
            return Err(PlatformError::System("移入回收站的操作被系统中止".to_string()));
        }
        Ok(())
    }
}

pub(super) unsafe fn create_shell_item_for_trash(path: &Path) -> Result<IShellItem, PlatformError> {
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let clean = strip_verbatim_prefix(&abs);
    let clean_w = wide_null(&clean);
    if let Ok(item) = unsafe {
        SHCreateItemFromParsingName::<_, _, IShellItem>(PCWSTR(clean_w.as_ptr()), None::<&IBindCtx>)
    } {
        return Ok(item);
    }

    // 超长路径（>260 字符）：先尝试 verbatim 前缀，再尝试 8.3 短路径
    let verbatim = to_verbatim_path(&abs);
    let verbatim_w = wide_null(&verbatim);
    if let Ok(item) = unsafe {
        SHCreateItemFromParsingName::<_, _, IShellItem>(
            PCWSTR(verbatim_w.as_ptr()),
            None::<&IBindCtx>,
        )
    } {
        return Ok(item);
    }

    let needed = unsafe { GetShortPathNameW(PCWSTR(verbatim_w.as_ptr()), None) };
    if needed > 0 {
        let mut short_buf = vec![0u16; needed as usize + 1];
        let written =
            unsafe { GetShortPathNameW(PCWSTR(verbatim_w.as_ptr()), Some(&mut short_buf)) };
        if written > 0 && (written as usize) < short_buf.len() {
            let short_str = String::from_utf16_lossy(&short_buf[..written as usize]);
            let short_clean = strip_verbatim_prefix(Path::new(&short_str));
            let short_w = wide_null(&short_clean);
            if let Ok(item) = unsafe {
                SHCreateItemFromParsingName::<_, _, IShellItem>(
                    PCWSTR(short_w.as_ptr()),
                    None::<&IBindCtx>,
                )
            } {
                return Ok(item);
            }
        }
    }

    Err(PlatformError::System(format!(
        "无法为路径创建系统回收站外壳项：{}",
        path.display()
    )))
}

pub(super) fn schtask_exists() -> bool {
    use std::os::windows::process::CommandExt;
    std::process::Command::new("schtasks.exe")
        .args(["/Query", "/TN", SCHTASK_NAME])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .is_ok_and(|out| out.status.success())
}

pub(super) fn delete_schtask_if_exists() -> Result<(), PlatformError> {
    use std::os::windows::process::CommandExt;
    if !schtask_exists() {
        return Ok(());
    }
    let out = std::process::Command::new("schtasks.exe")
        .args(["/Delete", "/TN", SCHTASK_NAME, "/F"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|err| PlatformError::System(format!("无法调用系统计划任务工具：{err}")))?;
    if !out.status.success() {
        return Err(PlatformError::PermissionDenied(
            "取消或降级“以管理员权限开机自启”需要先以管理员身份运行本程序".to_string(),
        ));
    }
    Ok(())
}

pub(super) fn remove_run_reg_value() {
    if let Ok(key) = windows_registry::CURRENT_USER.create(RUN_REG_PATH) {
        let _ = key.remove_value(RUN_REG_VALUE);
    }
}

pub(super) fn shell_item_to_path(item: &IShellItem) -> Option<PathBuf> {
    unsafe {
        let pwstr = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let text = pwstr_to_string_and_free(pwstr);
        if text.is_empty() {
            None
        } else {
            Some(PathBuf::from(text))
        }
    }
}

pub(super) fn run_file_dialog_sta(pick_folder: bool) -> Result<Vec<PathBuf>, PlatformError> {
    const HRESULT_CANCELLED: i32 = 0x800704C7u32 as i32; // ERROR_CANCELLED (1223)

    // RAII 守卫确保任何提前 `?` 返回都会先释放 COM 接口再调用 CoUninitialize
    let _com = ComGuard::init_sta();
    unsafe {
        let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)
            .map_err(|e| PlatformError::System(format!("创建文件选择对话框失败：{e}")))?;

        let mut opts = dialog
            .GetOptions()
            .map_err(|e| PlatformError::System(format!("读取对话框选项失败：{e}")))?;
        opts |= FOS_FORCEFILESYSTEM | FOS_FILEMUSTEXIST;
        if pick_folder {
            opts |= FOS_PICKFOLDERS;
        } else {
            opts |= FOS_ALLOWMULTISELECT;
        }
        dialog
            .SetOptions(opts)
            .map_err(|e| PlatformError::System(format!("设置对话框选项失败：{e}")))?;

        match dialog.Show(None) {
            Ok(()) => {
                let mut out = Vec::new();
                if pick_folder {
                    if let Ok(item) = dialog.GetResult()
                        && let Some(p) = shell_item_to_path(&item)
                    {
                        out.push(p);
                    }
                } else {
                    let items = dialog
                        .GetResults()
                        .map_err(|e| PlatformError::System(format!("读取已选文件失败：{e}")))?;
                    let count = items.GetCount().unwrap_or(0);
                    for idx in 0..count {
                        if let Ok(item) = items.GetItemAt(idx)
                            && let Some(p) = shell_item_to_path(&item)
                        {
                            out.push(p);
                        }
                    }
                }
                Ok(out)
            }
            Err(err) if err.code().0 == HRESULT_CANCELLED => Ok(Vec::new()),
            Err(err) => Err(PlatformError::System(format!("显示文件选择框失败：{err}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_cache_enforces_capacity_limit_and_evicts_oldest() {
        let mut cache = IconCache::default();
        for i in 0..(MAX_ICON_CACHE_ENTRIES + 10) {
            cache.insert(format!("app-{i}"), vec![i as u8]);
        }
        assert_eq!(cache.entries.len(), MAX_ICON_CACHE_ENTRIES);
        assert!(cache.get("app-0").is_none(), "oldest entry should be evicted");
        assert!(
            cache.get(&format!("app-{}", MAX_ICON_CACHE_ENTRIES + 9)).is_some(),
            "newest entry should remain cached"
        );
    }

    #[test]
    fn persisted_apps_index_checks_ttl_and_atomic_write() {
        let dir = std::env::temp_dir().join(format!(
            "pcshare_apps_cache_test_{}_{}",
            std::process::id(),
            fastrand::u64(..)
        ));
        let path = dir.join("apps_index_v2.json");
        let now = 1_700_000_000u64;
        let sample = vec![InstalledAppInfo {
            name: "记事本".to_string(),
            app_id: "notepad".to_string(),
        }];

        let fresh_json = serde_json::to_vec(&PersistedAppsIndex {
            fingerprint: 42,
            updated_at_unix_secs: now - 10,
            apps: sample.clone(),
        })
        .unwrap();
        write_atomic_bytes(&path, &fresh_json).expect("atomic write should succeed");
        assert_eq!(
            read_persisted_apps_index(&path, 42, now),
            Some(sample.clone())
        );
        // 超过 5 分钟 TTL 必须失效
        assert_eq!(
            read_persisted_apps_index(&path, 42, now + APP_CACHE_TTL.as_secs()),
            None
        );
        // 指纹不一致必须失效
        assert_eq!(read_persisted_apps_index(&path, 99, now), None);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
