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

    // 若路径本身或其已有祖先存在符号链接，用实际解析路径再次核验，防止软链逃逸
    let effective_check_path = resolve_existing_ancestors(&lexical);

    if !permissions.allowed_roots.is_empty() {
        let allowed = permissions.allowed_roots.iter().any(|root_str| {
            if let Ok(root_norm) = normalize_lexical_absolute(root_str) {
                let root_real = resolve_existing_ancestors(&root_norm);
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

fn resolve_existing_ancestors(path: &Path) -> PathBuf {
    let mut tail = Vec::new();
    let mut cursor = Some(path);
    while let Some(curr) = cursor {
        let fs_curr = to_fs_path(curr);
        if let Ok(canon) = std::fs::canonicalize(&fs_curr) {
            let mut base = PathBuf::from(to_display_path(&canon));
            for seg in tail.into_iter().rev() {
                base.push(seg);
            }
            return base;
        }
        if let Some(name) = curr.file_name() {
            tail.push(name.to_os_string());
            cursor = curr.parent();
        } else {
            break;
        }
    }
    path.to_path_buf()
}

fn is_subpath_of(candidate: &Path, root: &Path) -> bool {
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

/// 校验单个文件名（用于 `fs.rename`），不允许包含路径分隔符或保留名称。
pub fn validate_single_name(name: &str) -> Result<&str, RpcError> {
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        return Err(RpcError::new(ErrorCode::Invalid, "新名称不能为空，也不能是 . 或 .."));
    }
    if trimmed.contains(['/', '\\', ':', '*', '?', '"', '<', '>', '|']) {
        return Err(RpcError::new(
            ErrorCode::Invalid,
            "文件名不能包含 \\ / : * ? \" < > | 等特殊字符",
        ));
    }
    Ok(trimmed)
}
