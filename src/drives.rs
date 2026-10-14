//! 侧边栏数据源：快速访问、磁盘枚举（卷标 + 容量）、回收站计数。
//!
//! 仅 Windows 实现；其它平台返回空数据（供跨平台编译 / 测试）。

#![cfg(windows)]

use std::path::PathBuf;

use windows::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
};
use windows::Win32::UI::Shell::{SHQUERYRBINFO, SHQueryRecycleBinW};

use crate::wstr::{pcw, wide};

/// DRIVE_FIXED
const DRIVE_FIXED: u32 = 3;

/// 一个磁盘条目
#[derive(Debug, Clone)]
pub struct DriveInfo {
    /// 盘符，如 "C:"
    pub letter: String,
    /// 卷标（为空时用 "本地磁盘"）
    pub label: String,
    /// 已用字节
    pub used: u64,
    /// 总字节
    pub total: u64,
}

impl DriveInfo {
    /// 已用百分比（0-100）
    pub fn used_percent(&self) -> f32 {
        if self.total == 0 {
            return 0.0;
        }
        (self.used as f64 / self.total as f64 * 100.0) as f32
    }

    /// "113G / 477G"
    pub fn capacity_label(&self) -> String {
        format!("{} / {}", format_gb(self.used), format_gb(self.total))
    }
}

/// 快速访问项
#[derive(Debug, Clone)]
pub struct QuickAccess {
    pub desktop: Option<PathBuf>,
    pub downloads: Option<PathBuf>,
    /// 回收站（shell 路径）
    pub recycle_bin: String,
    /// 回收站项目数（查询失败为 None）
    pub recycle_count: Option<u64>,
}

/// 枚举固定磁盘及其容量
pub fn fixed_drives() -> Vec<DriveInfo> {
    let mask = unsafe { GetLogicalDrives() };
    let mut drives = Vec::new();
    for index in 0..26u32 {
        if mask & (1 << index) == 0 {
            continue;
        }
        let letter = (b'A' + index as u8) as char;
        let root = format!("{letter}:");
        let root_wide = wide(&format!("{root}\\"));
        if unsafe { GetDriveTypeW(pcw(&root_wide)) } != DRIVE_FIXED {
            continue;
        }

        let mut label_buffer = [0u16; 64];
        let label = unsafe {
            GetVolumeInformationW(
                pcw(&root_wide),
                Some(&mut label_buffer),
                None,
                None,
                None,
                None,
            )
        }
        .map(|_| {
            let end = label_buffer
                .iter()
                .position(|c| *c == 0)
                .unwrap_or(label_buffer.len());
            String::from_utf16_lossy(&label_buffer[..end])
        })
        .unwrap_or_default();

        let mut free_bytes: u64 = 0;
        let mut total_bytes: u64 = 0;
        let _ = unsafe {
            GetDiskFreeSpaceExW(
                pcw(&root_wide),
                None,
                Some(&mut total_bytes as *mut u64),
                Some(&mut free_bytes as *mut u64),
            )
        };

        drives.push(DriveInfo {
            letter: root,
            label: if label.trim().is_empty() {
                "本地磁盘".to_string()
            } else {
                label
            },
            used: total_bytes.saturating_sub(free_bytes),
            total: total_bytes,
        });
    }
    drives
}

/// 快速访问（桌面 / 下载 / 回收站）
pub fn quick_access() -> QuickAccess {
    QuickAccess {
        desktop: dirs::desktop_dir(),
        downloads: dirs::download_dir(),
        recycle_bin: "::{645FF040-5081-101B-9F08-00AA002F954E}".to_string(),
        recycle_count: recycle_bin_count(),
    }
}

/// 回收站项目数
pub fn recycle_bin_count() -> Option<u64> {
    let mut info = SHQUERYRBINFO {
        cbSize: std::mem::size_of::<SHQUERYRBINFO>() as u32,
        ..unsafe { std::mem::zeroed() }
    };
    match unsafe { SHQueryRecycleBinW(None, &mut info) } {
        Ok(()) => Some(info.i64NumItems.max(0) as u64),
        Err(_) => None,
    }
}

fn format_gb(bytes: u64) -> String {
    const GB: u64 = 1024 * 1024 * 1024;
    if bytes >= GB {
        format!("{}G", bytes / GB)
    } else {
        format!("{}M", bytes / (1024 * 1024))
    }
}
