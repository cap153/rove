//! 目录枚举与排序。

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use super::entry::FileEntry;

/// 调试日志：经项目既有的 `log_write` 写入 exe 同目录的 `rove.log`（前缀 `[rove:dirsize]`），
/// 与项目其它诊断日志同源、同一 `ROVE_LOG` 开关（存在即启用），不再走 stderr。
#[cfg(windows)]
fn dirsize_trace(msg: String) {
    if crate::app::log_enabled(2) {
        crate::app::log_write(&format!("[rove:dirsize] {msg}"));
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SortKey {
    Name,
    Size,
    Modified,
    Type,
    Path,
}

#[derive(Clone, Copy, Debug)]
pub struct SortState {
    pub key: SortKey,
    pub ascending: bool,
}

impl Default for SortState {
    fn default() -> Self {
        Self {
            key: SortKey::Name,
            ascending: true,
        }
    }
}

/// 枚举目录内容（不递归）。读取失败（权限/不存在）时返回空表。
/// 此函数运行在工作线程，可以放心阻塞。
pub fn list_directory(path: &Path) -> Vec<FileEntry> {
    let Ok(read_dir) = std::fs::read_dir(path) else {
        return Vec::new();
    };
    let mut entries = Vec::new();
    for entry in read_dir.flatten() {
        let path = entry.path();
        // symlink_metadata：不跟随符号链接，避免环与重复统计。
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        let is_dir = meta.is_dir();
        let size = if is_dir { 0 } else { meta.len() };
        let modified = meta.modified().ok();
        let name = entry.file_name().to_string_lossy().into_owned();

        // Windows 隐藏属性（FILE_ATTRIBUTE_HIDDEN = 2）或以 '.' 开头的 Unix 隐藏文件。
        #[cfg(windows)]
        let is_hidden = {
            use std::os::windows::fs::MetadataExt;
            (meta.file_attributes() & 2) != 0 || name.starts_with('.')
        };
        #[cfg(not(windows))]
        let is_hidden = name.starts_with('.');

        entries.push(FileEntry::from_path(&path, is_dir, size, modified, is_hidden));
    }
    entries
}

/// 排序：目录恒在前，其后按当前键比较。
pub fn sort_entries(entries: &mut [FileEntry], sort: SortState) {
    entries.sort_by(|a, b| {
        // 目录优先。
        match (a.is_dir, b.is_dir) {
            (true, false) => return std::cmp::Ordering::Less,
            (false, true) => return std::cmp::Ordering::Greater,
            _ => {}
        }
        let ord = match sort.key {
            SortKey::Name => natural_cmp(&a.name, &b.name),
            SortKey::Size => a.size.cmp(&b.size),
            SortKey::Modified => a.modified.cmp(&b.modified),
            SortKey::Type => a.type_text.cmp(&b.type_text),
            SortKey::Path => a.path.cmp(&b.path),
        };
        if sort.ascending { ord } else { ord.reverse() }
    });
}

/// 自然序比较：数字段按数值比较（"file2" < "file10"），大小写不敏感。
fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let mut ai = a.chars().peekable();
    let mut bi = b.chars().peekable();
    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return std::cmp::Ordering::Equal,
            (None, Some(_)) => return std::cmp::Ordering::Less,
            (Some(_), None) => return std::cmp::Ordering::Greater,
            (Some(ac), Some(bc)) => {
                if ac.is_ascii_digit() && bc.is_ascii_digit() {
                    let mut an = String::new();
                    let mut bn = String::new();
                    while let Some(c) = ai.peek().copied() {
                        if c.is_ascii_digit() {
                            an.push(c);
                            ai.next();
                        } else {
                            break;
                        }
                    }
                    while let Some(c) = bi.peek().copied() {
                        if c.is_ascii_digit() {
                            bn.push(c);
                            bi.next();
                        } else {
                            break;
                        }
                    }
                    let av: u128 = an.parse().unwrap_or(0);
                    let bv: u128 = bn.parse().unwrap_or(0);
                    match av.cmp(&bv) {
                        std::cmp::Ordering::Equal => continue,
                        other => return other,
                    }
                }
                let al = ac.to_lowercase().next().unwrap_or(ac);
                let bl = bc.to_lowercase().next().unwrap_or(bc);
                match al.cmp(&bl) {
                    std::cmp::Ordering::Equal => {
                        ai.next();
                        bi.next();
                    }
                    other => return other,
                }
            }
        }
    }
}

/// 按文件名过滤（大小写不敏感的子串匹配）。当前由 PaneState::rebuild_display
/// 内联实现，保留作单元测试与复用入口。
#[allow(dead_code)]
pub fn filter_entries(entries: &[FileEntry], filter: &str) -> Vec<usize> {
    if filter.trim().is_empty() {
        return (0..entries.len()).collect();
    }
    let needle = filter.trim().to_lowercase();
    entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.name.to_lowercase().contains(&needle))
        .map(|(i, _)| i)
        .collect()
}

/// 测试辅助：构造一个条目。
#[cfg(test)]
pub fn test_entry(name: &str, is_dir: bool, size: u64) -> FileEntry {
    FileEntry {
        name: name.to_string(),
        path: Path::new(name).to_path_buf(),
        is_dir,
        size,
        modified: None,
        ext: String::new(),
        style: super::entry::IconStyle::Generic,
        type_text: String::new(),
        extra: String::new(),
        is_hidden: false,
        dir_size_loaded: true,
        dir_size_partial: false,
        dir_size_deferred: false,
    }
}

/// 递归计算文件夹总大小（跳过 symlink/Junction 重解析点，支持外部协作式中断）。
/// 递归计算文件夹总大小（跳过 symlink/Junction 重解析点/云占位，支持外部中断）。
/// 返回 (总字节数, 是否为不完全统计下界)。
pub fn calculate_dir_size(dir: &Path, stop: &AtomicBool) -> (u64, bool) {
    // Windows 云按需文件/占位目录属性（OneDrive / iCloud / Dropbox 等）
    // 0x00040000: FILE_ATTRIBUTE_RECALL_ON_OPEN
    // 0x00400000: FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS
    #[cfg(windows)]
    const FILE_ATTRIBUTE_RECALL: u32 = 0x00040000 | 0x00400000;

    if stop.load(Ordering::Relaxed) {
        return (0, true);
    }

    // 根节点防御：若目标自身即为 Junction/Reparse Point（如自环入口），直接跳过
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if let Ok(meta) = std::fs::symlink_metadata(dir) {
            if (meta.file_attributes() & 0x400) != 0 {
                dirsize_trace(format!("skip reparse ROOT: {}", dir.display()));
                return (0, true);
            }
        }
    }

    let Ok(read_dir) = std::fs::read_dir(dir) else {
        // 无法读取（如 Access Denied 无权限、设备忙等）-> 标为 partial 下界
        return (0, true);
    };

    let mut total: u64 = 0;
    let mut is_partial = false;

    for entry in read_dir.flatten() {
        if stop.load(Ordering::Relaxed) {
            return (total, true);
        }
        let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
            is_partial = true;
            continue;
        };

        if meta.is_dir() {
            // 仅目录需跳过 Junction 重解析点（防 Application Data 等自环死循环）；
            // 普通文件即使带虚拟化重解析点标记（Office C2R / WindowsApps 的
            // exe/dll）也应正常计入体积，故文件不在跳过之列。
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                if (meta.file_attributes() & (0x400 | FILE_ATTRIBUTE_RECALL)) != 0 {
                    dirsize_trace(format!(
                        "skip reparse/cloud placeholder: {}",
                        entry.path().display()
                    ));
                    is_partial = true; // 存在未统计的子树，整体标记为下界
                    continue;
                }
            }
            #[cfg(not(windows))]
            {
                if meta.file_type().is_symlink() {
                    is_partial = true;
                    continue;
                }
            }
            let (child_sz, child_partial) = calculate_dir_size(&entry.path(), stop);
            total = total.saturating_add(child_sz);
            if child_partial {
                is_partial = true;
            }
        } else {
            total = total.saturating_add(meta.len());
        }
    }
    (total, is_partial)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    #[test]
    fn dirs_sort_first_then_by_name() {
        let mut entries = vec![
            test_entry("b.txt", false, 10),
            test_entry("a.txt", false, 20),
            test_entry("zeta", true, 0),
            test_entry("alpha", true, 0),
        ];
        sort_entries(&mut entries, SortState::default());
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "zeta", "a.txt", "b.txt"]);
    }

    #[test]
    fn descending_reverses_within_group() {
        let mut entries = vec![test_entry("a", false, 1), test_entry("b", false, 2)];
        sort_entries(
            &mut entries,
            SortState {
                key: SortKey::Size,
                ascending: false,
            },
        );
        assert_eq!(entries[0].name, "b");
    }

    #[test]
    fn natural_order_puts_numbers_in_order() {
        let mut entries = vec![
            test_entry("file10.txt", false, 1),
            test_entry("file2.txt", false, 1),
            test_entry("file1.txt", false, 1),
        ];
        sort_entries(&mut entries, SortState::default());
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["file1.txt", "file2.txt", "file10.txt"]);
    }

    #[test]
    fn filter_is_case_insensitive_substring() {
        let entries = vec![
            test_entry("Readme.md", false, 1),
            test_entry("main.rs", false, 1),
        ];
        assert_eq!(filter_entries(&entries, "MD"), vec![0]);
        assert_eq!(filter_entries(&entries, "  "), vec![0, 1]);
    }

    #[test]
    fn systemtime_roundtrip_smoke() {
        let t = SystemTime::now();
        let _ = super::super::time::format_local(t);
    }
}
