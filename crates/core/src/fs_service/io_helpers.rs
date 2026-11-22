use super::*;

pub fn deterministic_part_name(file_name: &str) -> String {
    format!(".{file_name}.pcshare.part")
}

pub fn sanitize_rel_subpath(rel: &str) -> Result<PathBuf, RpcError> {
    let trimmed = rel.trim().replace('/', "\\");
    if trimmed.is_empty() {
        return Err(RpcError::new(ErrorCode::Invalid, "相对路径不能为空"));
    }
    let mut out = PathBuf::new();
    for part in trimmed.split('\\').filter(|s| !s.is_empty()) {
        validate_single_name(part)?;
        out.push(part);
    }
    if out.as_os_str().is_empty() {
        return Err(RpcError::new(ErrorCode::Invalid, "无效的相对子路径"));
    }
    Ok(out)
}

#[cfg(windows)]
const WIN_FILE_FLAG_SEQUENTIAL_SCAN: u32 = 0x0800_0000;

pub fn open_sequential_read(fs_path: &Path) -> std::io::Result<File> {
    let mut opts = fs::OpenOptions::new();
    opts.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        opts.custom_flags(WIN_FILE_FLAG_SEQUENTIAL_SCAN);
    }
    opts.open(fs_path)
}

pub fn create_sequential_write(fs_path: &Path) -> std::io::Result<File> {
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        opts.custom_flags(WIN_FILE_FLAG_SEQUENTIAL_SCAN);
    }
    opts.open(fs_path)
}

pub fn open_sequential_rw(fs_path: &Path) -> std::io::Result<File> {
    let mut opts = fs::OpenOptions::new();
    opts.read(true).write(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        opts.custom_flags(WIN_FILE_FLAG_SEQUENTIAL_SCAN);
    }
    opts.open(fs_path)
}

pub fn open_or_create_sequential_rw(fs_path: &Path) -> std::io::Result<File> {
    let mut opts = fs::OpenOptions::new();
    opts.read(true).write(true).create(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        opts.custom_flags(WIN_FILE_FLAG_SEQUENTIAL_SCAN);
    }
    opts.open(fs_path)
}

pub(crate) fn write_all_at_offset(
    file: &mut File,
    mut data: &[u8],
    mut offset: u64,
) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        while !data.is_empty() {
            let n = file.seek_write(data, offset)?;
            if n == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "分片随机偏移写入返回 0 字节",
                ));
            }
            data = &data[n..];
            offset += n as u64;
        }
        Ok(())
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.write_all_at(data, offset)
    }
    #[cfg(not(any(windows, unix)))]
    {
        use std::io::{Seek, SeekFrom, Write};
        file.seek(SeekFrom::Start(offset))?;
        file.write_all(data)
    }
}

pub fn hash_file_prefix_sync(fs_path: &Path, limit: u64) -> std::io::Result<String> {
    use std::io::Read;
    let mut f = open_sequential_read(fs_path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; TRANSFER_CHUNK_SIZE];
    let mut left = limit;
    while left > 0 {
        let want = (left as usize).min(buf.len());
        let n = f.read(&mut buf[..want])?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        left -= n as u64;
    }
    Ok(hasher.finalize().to_hex().to_string())
}

pub(super) fn collect_walk_entries_sync(
    root_clean: &Path,
    root_fs: &Path,
    out: &mut Vec<WalkEntry>,
) -> Result<(), RpcError> {
    let root_disp = to_display_path(root_clean);
    let meta = fs::symlink_metadata(root_fs).map_err(|e| map_io_to_rpc(&e, Some(&root_disp)))?;
    let base_name = root_clean
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "root".to_string());

    if is_symlink_or_junction(root_fs, &meta) {
        return Ok(());
    }
    if !meta.is_dir() {
        out.push(WalkEntry {
            rel_path: base_name,
            abs_path: root_disp,
            dir: false,
            size: meta.len(),
            modified_ms: meta.modified().ok().and_then(system_time_to_ms),
        });
        return Ok(());
    }

    let mut stack: Vec<(PathBuf, PathBuf, String)> =
        vec![(root_clean.to_path_buf(), root_fs.to_path_buf(), base_name)];
    while let Some((cur_clean, cur_fs, cur_rel)) = stack.pop() {
        let cur_disp = to_display_path(&cur_clean);
        let cur_meta = fs::symlink_metadata(&cur_fs).ok();
        out.push(WalkEntry {
            rel_path: cur_rel.clone(),
            abs_path: cur_disp.clone(),
            dir: true,
            size: 0,
            modified_ms: cur_meta
                .and_then(|m| m.modified().ok())
                .and_then(system_time_to_ms),
        });

        let rd = fs::read_dir(&cur_fs).map_err(|e| map_io_to_rpc(&e, Some(&cur_disp)))?;
        let mut sub_dirs = Vec::new();
        for item_res in rd {
            let Ok(item) = item_res else { continue };
            let Ok(ft) = item.file_type() else { continue };
            if ft.is_symlink() {
                continue;
            }
            let item_path = item.path();
            let Ok(m) = fs::symlink_metadata(&item_path) else {
                continue;
            };
            if is_symlink_or_junction(&item_path, &m) {
                continue;
            }
            let name = item.file_name().to_string_lossy().into_owned();
            let child_clean = cur_clean.join(&name);
            let child_rel = format!(r"{cur_rel}\{name}");
            if m.is_dir() {
                sub_dirs.push((child_clean, item_path, child_rel));
            } else {
                out.push(WalkEntry {
                    rel_path: child_rel,
                    abs_path: to_display_path(&child_clean),
                    dir: false,
                    size: m.len(),
                    modified_ms: m.modified().ok().and_then(system_time_to_ms),
                });
            }
        }
        sub_dirs.reverse();
        stack.extend(sub_dirs);
    }
    Ok(())
}

pub fn pick_unique_target(clean_dest: &Path) -> (PathBuf, PathBuf) {
    let fs_initial = to_fs_path(clean_dest);
    if !fs_initial.exists() {
        return (clean_dest.to_path_buf(), fs_initial);
    }
    let parent = clean_dest.parent().unwrap_or_else(|| Path::new(""));
    let stem = clean_dest
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    let ext = clean_dest
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();

    for idx in 1..100_000u32 {
        let candidate = parent.join(format!("{stem} ({idx}){ext}"));
        let candidate_fs = to_fs_path(&candidate);
        if !candidate_fs.exists() {
            return (candidate, candidate_fs);
        }
    }
    let fallback = parent.join(format!("{stem} ({}){ext}", fastrand::u64(..)));
    let fallback_fs = to_fs_path(&fallback);
    (fallback, fallback_fs)
}

pub fn replace_file_atomically(src_fs: &Path, dst_fs: &Path) -> std::io::Result<()> {
    match fs::rename(src_fs, dst_fs) {
        Ok(()) => Ok(()),
        Err(_err) if dst_fs.exists() => {
            fs::remove_file(dst_fs)?;
            fs::rename(src_fs, dst_fs)
        }
        Err(err) => Err(err),
    }
}

pub fn system_time_to_ms(t: SystemTime) -> Option<u64> {
    t.duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as u64)
}

pub(super) fn make_dir_prefix(disp_dir: &str) -> String {
    if disp_dir.ends_with('\\') || disp_dir.ends_with('/') {
        disp_dir.to_string()
    } else {
        let mut p = String::with_capacity(disp_dir.len() + 1);
        p.push_str(disp_dir);
        p.push('\\');
        p
    }
}

pub(super) fn convert_os_dir_entry(prefix: &str, item: fs::DirEntry) -> DirEntry {
    let name = item.file_name().to_string_lossy().into_owned();
    let mut full_path = String::with_capacity(prefix.len() + name.len());
    full_path.push_str(prefix);
    full_path.push_str(&name);
    // Windows 上 DirEntry::metadata() 直接使用 FindNextFileW 缓存的数据，不产生额外系统调用
    let (kind, size, modified_ms, created_ms, readonly) = match item.metadata() {
        Ok(meta) => {
            let ft = meta.file_type();
            let kind = if ft.is_symlink() {
                EntryKind::Symlink
            } else if ft.is_dir() {
                EntryKind::Dir
            } else {
                EntryKind::File
            };
            (
                kind,
                if ft.is_dir() { 0 } else { meta.len() },
                meta.modified().ok().and_then(system_time_to_ms),
                meta.created().ok().and_then(system_time_to_ms),
                meta.permissions().readonly(),
            )
        }
        Err(_) => (EntryKind::File, 0, None, None, false),
    };
    DirEntry {
        name,
        path: full_path,
        kind,
        size,
        modified_ms,
        created_ms,
        readonly,
    }
}

pub(super) fn stream_dir_batches(
    disp_dir: &str,
    fs_dir: &Path,
    first_batch_cap: usize,
    tx: tokio::sync::mpsc::Sender<Result<FsListBatch, RpcError>>,
) {
    let reader = match fs::read_dir(fs_dir) {
        Ok(r) => r,
        Err(e) => {
            let _ = tx.blocking_send(Err(map_io_to_rpc(&e, Some(disp_dir))));
            return;
        }
    };
    let prefix = make_dir_prefix(disp_dir);
    let mut iter = reader.flatten();
    let mut current_cap = first_batch_cap.max(1);
    let next_cap = DEFAULT_LIST_BATCH_SIZE.max(current_cap);
    let mut chunk = Vec::with_capacity(current_cap);
    let mut peeked = iter.next();

    if peeked.is_none() {
        let _ = tx.blocking_send(Ok(FsListBatch {
            path: disp_dir.to_string(),
            entries: Vec::new(),
            has_more: false,
            total_hint: Some(0),
        }));
        return;
    }

    while let Some(item) = peeked {
        chunk.push(convert_os_dir_entry(&prefix, item));
        peeked = iter.next();
        if chunk.len() >= current_cap || peeked.is_none() {
            let has_more = peeked.is_some();
            let outgoing = std::mem::replace(
                &mut chunk,
                if has_more {
                    Vec::with_capacity(next_cap)
                } else {
                    Vec::new()
                },
            );
            if tx
                .blocking_send(Ok(FsListBatch {
                    path: disp_dir.to_string(),
                    entries: outgoing,
                    has_more,
                    total_hint: None,
                }))
                .is_err()
            {
                return;
            }
            current_cap = next_cap;
        }
    }
}

pub(super) fn read_dir_fast(clean_dir: &Path, fs_dir: &Path) -> Result<Vec<DirEntry>, RpcError> {
    let disp_dir = to_display_path(clean_dir);
    let reader = fs::read_dir(fs_dir).map_err(|e| map_io_to_rpc(&e, Some(&disp_dir)))?;
    let prefix = make_dir_prefix(&disp_dir);
    let mut entries = Vec::new();
    for item in reader.flatten() {
        entries.push(convert_os_dir_entry(&prefix, item));
    }

    // 文件夹优先，再按名称不区分大小写排序（零额外堆分配比较）
    entries.sort_by(|a, b| {
        let a_is_dir = a.kind == EntryKind::Dir;
        let b_is_dir = b.kind == EntryKind::Dir;
        b_is_dir
            .cmp(&a_is_dir)
            .then_with(|| cmp_case_insensitive(&a.name, &b.name))
    });
    Ok(entries)
}

pub(super) fn cmp_case_insensitive(a: &str, b: &str) -> std::cmp::Ordering {
    if a.is_ascii() && b.is_ascii() {
        return a
            .bytes()
            .map(|c| c.to_ascii_lowercase())
            .cmp(b.bytes().map(|c| c.to_ascii_lowercase()))
            .then_with(|| a.cmp(b));
    }
    let mut ia = a.chars().flat_map(char::to_lowercase);
    let mut ib = b.chars().flat_map(char::to_lowercase);
    loop {
        match (ia.next(), ib.next()) {
            (Some(ca), Some(cb)) => match ca.cmp(&cb) {
                std::cmp::Ordering::Equal => continue,
                non_eq => return non_eq,
            },
            (None, Some(_)) => return std::cmp::Ordering::Less,
            (Some(_), None) => return std::cmp::Ordering::Greater,
            (None, None) => return a.cmp(b),
        }
    }
}

pub(super) fn stat_one(clean: &Path, fs_path: &Path) -> Result<DirEntry, RpcError> {
    let disp = to_display_path(clean);
    let meta = fs::symlink_metadata(fs_path).map_err(|e| map_io_to_rpc(&e, Some(&disp)))?;
    let ft = meta.file_type();
    let kind = if ft.is_symlink() {
        EntryKind::Symlink
    } else if ft.is_dir() {
        EntryKind::Dir
    } else {
        EntryKind::File
    };
    let name = clean
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| disp.clone());
    Ok(DirEntry {
        name,
        path: disp,
        kind,
        size: if ft.is_dir() { 0 } else { meta.len() },
        modified_ms: meta.modified().ok().and_then(system_time_to_ms),
        created_ms: meta.created().ok().and_then(system_time_to_ms),
        readonly: meta.permissions().readonly(),
    })
}
