//! 路径规范化、开放目录边界检查与 Windows 超长路径处理（任务 1.7 与总体方案 7.4 节）。

use std::path::{Component, Path, PathBuf};

use pcshare_proto::{ErrorCode, Permissions, RpcError};

/// 清理展示用的长路径前缀（去掉 `\\?\`）。
pub fn to_display_path(path: &Path) -> String {
    let s = path.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        s.into_owned()
    }
}

/// 在 Windows 上为超过 240 个字符的绝对路径加上 `\\?\` 扩展前缀，确保超过 260 字符的路径可正常读写。
#[cfg(windows)]
pub fn to_fs_path(clean_abs_path: &Path) -> PathBuf {
    let raw = to_display_path(clean_abs_path);
    if raw.chars().count() < 240 {
        return PathBuf::from(raw);
    }
    if let Some(unc) = raw.strip_prefix(r"\\") {
        PathBuf::from(format!(r"\\?\UNC\{unc}"))
    } else {
        PathBuf::from(format!(r"\\?\{raw}"))
    }
}

#[cfg(not(windows))]
pub fn to_fs_path(clean_abs_path: &Path) -> PathBuf {
    clean_abs_path.to_path_buf()
}

/// 词法规范化绝对路径（不依赖目标文件已存在），消除 `.` 和 `..`，防止越出根目录。
pub fn normalize_lexical_absolute(raw: &str) -> Result<PathBuf, RpcError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(RpcError::new(ErrorCode::Invalid, "路径不能为空"));
    }
    let clean_str = if let Some(rest) = trimmed.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = trimmed.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        trimmed.to_string()
    };

    // Windows 盘符简写补全：例如 "C:" -> "C:\"
    #[cfg(windows)]
    let clean_str = if clean_str.len() == 2
        && clean_str.as_bytes()[0].is_ascii_alphabetic()
        && clean_str.as_bytes()[1] == b':'
    {
        format!("{clean_str}\\")
    } else {
        clean_str
    };

    let input = Path::new(&clean_str);
    if !input.is_absolute() {
        return Err(RpcError::new(
            ErrorCode::Invalid,
            format!("必须提供完整绝对路径：{trimmed}"),
        ));
    }

    let mut normalized = PathBuf::new();
    let mut normal_depth = 0usize;
    for comp in input.components() {
        match comp {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(comp.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if normal_depth == 0 {
                    return Err(RpcError::new(
                        ErrorCode::Forbidden,
                        "路径不允许通过 .. 越出根目录范围",
                    ));
                }
                normalized.pop();
                normal_depth -= 1;
            }
            Component::Normal(seg) => {
                normalized.push(seg);
                normal_depth += 1;
            }
        }
    }
    Ok(normalized)
}

/// 校验并规范化受控路径，确保不越出 `permissions.allowed_roots`（如果设置了限定目录）。
/// 返回 `(干净展示绝对路径, 可直接传给 std::fs 的系统路径)`。
pub fn resolve_authorized_path(
    raw: &str,
    permissions: &Permissions,
) -> Result<(PathBuf, PathBuf), RpcError> {
    let lexical = normalize_lexical_absolute(raw)?;

    if !permissions.allowed_roots.is_empty() {
        // 若路径本身或其已有祖先存在符号链接（含悬空符号链接），用实际解析路径再次核验，防止软链逃逸
        let effective_check_path = resolve_existing_ancestors(&lexical)?;
        let allowed = permissions.allowed_roots.iter().any(|root_str| {
            if let Ok(root_norm) = normalize_lexical_absolute(root_str)
                && let Ok(root_real) = resolve_existing_ancestors(&root_norm)
            {
                is_subpath_of(&lexical, &root_norm)
                    && is_subpath_of(&effective_check_path, &root_real)
            } else {
                false
            }
        });
        if !allowed {
            return Err(RpcError::new(
                ErrorCode::Forbidden,
                "对方没有授权读取或操作这个文件夹",
            ));
        }
    }

    let fs_path = to_fs_path(&lexical);
    Ok((lexical, fs_path))
}

const MAX_SYMLINK_HOPS: u8 = 16;

fn clean_link_target(target: &Path) -> PathBuf {
    let s = target.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\??\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = s.strip_prefix(r"\??\") {
        PathBuf::from(rest)
    } else if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        target.to_path_buf()
    }
}

fn resolve_existing_ancestors(path: &Path) -> Result<PathBuf, RpcError> {
    resolve_existing_ancestors_depth(path, 0)
}

fn resolve_existing_ancestors_depth(path: &Path, depth: u8) -> Result<PathBuf, RpcError> {
    if depth > MAX_SYMLINK_HOPS {
        return Err(RpcError::new(
            ErrorCode::Forbidden,
            "路径包含过深或循环符号链接，已拒绝访问",
        ));
    }

    let mut tail = Vec::new();
    let mut cursor = Some(path);
    while let Some(curr) = cursor {
        let fs_curr = to_fs_path(curr);
        if let Ok(canon) = std::fs::canonicalize(&fs_curr) {
            let mut base = PathBuf::from(to_display_path(&canon));
            for seg in tail.into_iter().rev() {
                base.push(seg);
            }
            return Ok(base);
        }

        // 若目标或其祖先是悬空符号链接/联接点，canonicalize 会失败；
        // 绝不能退回父目录放行，必须解析链接目标并递归核验其最终落点。
        if let Ok(meta) = std::fs::symlink_metadata(&fs_curr) {
            let link_target = std::fs::read_link(&fs_curr).ok();
            if meta.file_type().is_symlink() || link_target.is_some() {
                let raw_target = link_target.ok_or_else(|| {
                    RpcError::new(
                        ErrorCode::Forbidden,
                        "无法解析符号链接目标，为防止越界已拒绝访问",
                    )
                })?;
                let cleaned_target = clean_link_target(&raw_target);
                let mut combined = if cleaned_target.is_absolute() {
                    cleaned_target
                } else {
                    let parent = curr.parent().ok_or_else(|| {
                        RpcError::new(ErrorCode::Forbidden, "符号链接缺少父目录，已拒绝访问")
                    })?;
                    parent.join(&cleaned_target)
                };
                for seg in tail.into_iter().rev() {
                    combined.push(seg);
                }
                let next_lexical = normalize_lexical_absolute(&combined.to_string_lossy())
                    .map_err(|_| {
                        RpcError::new(
                            ErrorCode::Forbidden,
                            "符号链接指向不受允许或无效的路径，已拒绝访问",
                        )
                    })?;
                return resolve_existing_ancestors_depth(&next_lexical, depth + 1);
            }
        }

        if let Some(name) = curr.file_name() {
            tail.push(name.to_os_string());
            cursor = curr.parent();
        } else {
            break;
        }
    }
    Ok(path.to_path_buf())
}

/// 检查 `candidate` 是否位于 `root` 之下（含等于 `root`），供路径边界及移动/复制祖先防递归校验复用。
pub fn is_subpath_of(candidate: &Path, root: &Path) -> bool {
    let cand_comps: Vec<String> = candidate
        .components()
        .map(|c| normalize_component(&c))
        .collect();
    let root_comps: Vec<String> = root.components().map(|c| normalize_component(&c)).collect();
    if cand_comps.len() < root_comps.len() {
        return false;
    }
    cand_comps[..root_comps.len()] == root_comps[..]
}

/// 比较两个词法规范化路径是否指向同一路径（Windows 下忽略大小写）。
pub fn paths_equal(a: &Path, b: &Path) -> bool {
    let a_comps: Vec<String> = a.components().map(|c| normalize_component(&c)).collect();
    let b_comps: Vec<String> = b.components().map(|c| normalize_component(&c)).collect();
    a_comps == b_comps
}

fn normalize_component(comp: &Component<'_>) -> String {
    let s = comp.as_os_str().to_string_lossy();
    #[cfg(windows)]
    {
        s.to_lowercase()
    }
    #[cfg(not(windows))]
    {
        s.into_owned()
    }
}

fn is_windows_reserved_device_name(name: &str) -> bool {
    let base = name.split('.').next().unwrap_or(name).trim_end_matches(' ');
    let upper = base.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

/// 校验单个文件名（用于 `fs.rename`、上传/批传段名等），不允许包含路径分隔符、控制字符、末尾点号/空格或系统保留设备名。
pub fn validate_single_name(name: &str) -> Result<&str, RpcError> {
    if name.chars().any(|c| c.is_control()) {
        return Err(RpcError::new(
            ErrorCode::Invalid,
            "文件名不能包含控制字符",
        ));
    }
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        return Err(RpcError::new(
            ErrorCode::Invalid,
            "新名称不能为空，也不能是 . 或 ..",
        ));
    }
    if name.ends_with(['.', ' ']) || trimmed.ends_with('.') {
        return Err(RpcError::new(
            ErrorCode::Invalid,
            "文件名不能以点号或空格结尾",
        ));
    }
    if trimmed.contains(['/', '\\', ':', '*', '?', '"', '<', '>', '|']) {
        return Err(RpcError::new(
            ErrorCode::Invalid,
            "文件名不能包含 \\ / : * ? \" < > | 等特殊字符",
        ));
    }
    if is_windows_reserved_device_name(trimmed) {
        return Err(RpcError::new(
            ErrorCode::Invalid,
            format!("文件名不能使用系统保留名称：{trimmed}"),
        ));
    }
    Ok(trimmed)
}
