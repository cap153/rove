//! 任务 3.2：NTFS `$MFT` 全盘索引（需管理员权限）。
//!
//! 用 `mft` crate 直接解析卷上的 `$MFT`，枚举全部文件记录，
//! 比 `std::fs` 递归遍历快一个数量级（免目录树遍历）。
//!
//! - 仅 NTFS 固定盘可用；需管理员权限以打开 `\\.\X:\$MFT`。
//! - 任意盘打开失败（无权限 / 非 NTFS / 外接盘）即跳过该盘；
//!   **全部**盘都失败则返回 `None`，由调用方回退到原有递归 walker。
//! - 与 walker 保持一致的跳过策略（系统噪声目录）。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use super::index::{is_skipped, IndexEntry, MAX_ENTRIES, SharedIndex};
use super::pinyin_fields;

/// 尝试用 MFT 构建全盘索引；成功索引到条目返回 `Some(条目数)`，
/// 完全无法读取 MFT（无权限）返回 `None`（调用方回退 walker）。
#[cfg(windows)]
pub fn build_via_mft(idx: &SharedIndex, stop: &AtomicBool) -> Option<usize> {
    let mut total = 0usize;
    let mut any_drive = false;
    for drive in super::index::list_roots() {
        if stop.load(Ordering::Relaxed) || total >= MAX_ENTRIES {
            break;
        }
        if let Some(n) = scan_drive_mft(&drive, idx, stop, total) {
            any_drive = true;
            total += n;
        }
    }
    if any_drive {
        Some(total)
    } else {
        None
    }
}

/// 非 Windows 平台无 MFT，直接回退 walker。
#[cfg(not(windows))]
pub fn build_via_mft(_idx: &SharedIndex, _stop: &AtomicBool) -> Option<usize> {
    None
}

/// 解析单个盘的 `$MFT` 并写入索引；该盘不可用时返回 `None`。
fn scan_drive_mft(
    drive: &Path,
    idx: &SharedIndex,
    stop: &AtomicBool,
    start_total: usize,
) -> Option<usize> {
    // 盘符（如 "C"），用于构造 `\\.\C:\$MFT` 并作为路径前缀。
    let letter = drive
        .to_string_lossy()
        .trim_end_matches([':', '\\'])
        .to_string();
    if letter.is_empty() {
        return None;
    }
    let mft_path = format!("\\\\.\\{letter}:\\$MFT");

    // `mft` crate 按需 seek 读取，不会整段占内存。
    let mut parser = match mft::MftParser::from_path(&mft_path) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("MFT: 无法打开 {mft_path}（可能无管理员权限）：{e}");
            return None;
        }
    };

    let entry_count = parser.get_entry_count();
    let mut count = 0usize;
    for i in 0..entry_count {
        if stop.load(Ordering::Relaxed) || start_total + count >= MAX_ENTRIES {
            break;
        }
        let entry = match parser.get_entry(i) {
            Ok(e) => e,
            Err(_) => continue,
        };
        // 仅处理已分配的（未删除）记录。
        if !entry.is_allocated() {
            continue;
        }
        let name_attr = match entry.find_best_name_attribute() {
            Some(n) => n,
            None => continue,
        };
        // 跳过 MFT 元数据文件（如 $MFT、$Bitmap、$LogFile）。
        if name_attr.name.starts_with('$') {
            continue;
        }
        let is_dir = entry.is_dir();
        // 系统噪声目录（Windows / Program Files / AppData / node_modules …）整体跳过。
        if is_dir && is_skipped(&name_attr.name) {
            continue;
        }
        // 解析全路径：mft 给出卷内相对路径（形如 `\Windows\System32`）。
        let rel = parser.get_full_path_for_entry(&entry).ok().flatten();
        let path: PathBuf = match rel {
            Some(r) => {
                let s = r.to_string_lossy();
                let stripped = s.strip_prefix('\\').unwrap_or(&s);
                drive.join(stripped)
            }
            None => drive.join(&name_attr.name),
        };
        // 顶层系统目录整体跳过（与 walker 一致）。
        let depth = path.components().count() - drive.components().count();
        if is_dir && depth <= 1 && is_skipped(&name_attr.name) {
            continue;
        }
        let fields = pinyin_fields(&name_attr.name);
        if let Ok(mut w) = idx.write() {
            w.entries.push(IndexEntry {
                path,
                name: name_attr.name.clone(),
                fields,
                is_dir,
            });
        }
        count += 1;
    }

    if count == 0 {
        None
    } else {
        Some(count)
    }
}
