//! 基础文件操作：复制、移动（剪切）、重命名、删除（回收到回收站）。
//!
//! 复制 / 移动 / 重命名为纯 std 实现，可跨平台；“回收到回收站”依赖
//! Windows `SHFileOperationW`，其它平台返回错误。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Windows 文件名非法字符
const INVALID_CHARS: [char; 9] = ['\\', '/', ':', '*', '?', '"', '<', '>', '|'];

/// 目标已存在时生成 "name (2).ext" 形式的唯一路径
pub fn unique_destination(dest_dir: &Path, name: &str) -> PathBuf {
    let candidate = dest_dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let path = Path::new(name);
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| name.to_string());
    let extension = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();

    for index in 2..1000 {
        let candidate = dest_dir.join(format!("{stem} ({index}){extension}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    dest_dir.join(format!("{stem} (copy){extension}"))
}

/// 复制到目标目录（递归），返回处理项数
pub fn copy(sources: &[PathBuf], dest_dir: &Path) -> io::Result<usize> {
    fs::create_dir_all(dest_dir)?;
    let mut count = 0;
    for source in sources {
        let Some(name) = source.file_name() else {
            continue;
        };
        let dest = unique_destination(dest_dir, &name.to_string_lossy());
        copy_recursive(source, &dest)?;
        count += 1;
    }
    Ok(count)
}

fn copy_recursive(source: &Path, dest: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(source)?;
    if meta.is_dir() {
        fs::create_dir_all(dest)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            let target = dest.join(entry.file_name());
            copy_recursive(&entry.path(), &target)?;
        }
    } else {
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(source, dest)?;
    }
    Ok(())
}

/// 移动到目标目录：优先 rename，跨卷时回退为复制后删除
pub fn move_to(sources: &[PathBuf], dest_dir: &Path) -> io::Result<usize> {
    fs::create_dir_all(dest_dir)?;
    let mut count = 0;
    for source in sources {
        let Some(name) = source.file_name() else {
            continue;
        };
        let dest = unique_destination(dest_dir, &name.to_string_lossy());
        if fs::rename(source, &dest).is_err() {
            copy_recursive(source, &dest)?;
            remove_recursive(source)?;
        }
        count += 1;
    }
    Ok(count)
}

/// 重命名（仅改文件名，校验非法字符）
pub fn rename(path: &Path, new_name: &str) -> io::Result<PathBuf> {
    let new_name = new_name.trim();
    if new_name.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty name"));
    }
    if new_name.chars().any(|c| INVALID_CHARS.contains(&c)) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "name contains invalid characters",
        ));
    }
    if new_name == "." || new_name == ".." {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "reserved name"));
    }
    let Some(parent) = path.parent() else {
        return Err(io::Error::other("no parent directory"));
    };
    let dest = parent.join(new_name);
    if dest == path {
        return Ok(dest);
    }
    fs::rename(path, &dest)?;
    Ok(dest)
}

/// 删除到回收站（带撤销）
#[cfg(windows)]
pub fn recycle(paths: &[PathBuf]) -> io::Result<()> {
    use windows::Win32::UI::Shell::{
        FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, SHFILEOPSTRUCTW,
        SHFileOperationW,
    };
    use windows::core::PCWSTR;

    use crate::wstr::pcw;

    if paths.is_empty() {
        return Ok(());
    }
    // pFrom 需要双 NUL 结尾的宽字符列表
    let mut from: Vec<u16> = Vec::new();
    for path in paths {
        from.extend(path.display().to_string().encode_utf16());
        from.push(0);
    }
    from.push(0);

    let mut operation = SHFILEOPSTRUCTW {
        wFunc: 3, // FO_DELETE
        pFrom: pcw(&from),
        pTo: PCWSTR::null(),
        fFlags: (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_NOERRORUI | FOF_SILENT).0 as u16,
        ..unsafe { std::mem::zeroed() }
    };
    let result = unsafe { SHFileOperationW(&mut operation) };
    if result != 0 {
        return Err(io::Error::other(format!("recycle failed (code {result})")));
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn recycle(_paths: &[PathBuf]) -> io::Result<()> {
    Err(io::Error::other("recycle bin is only supported on Windows"))
}

fn remove_recursive(path: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if meta.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}
