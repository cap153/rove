//! 驱动器枚举、回收站统计、网络位置与快速访问目录。

use std::path::PathBuf;

/// 驱动器类型。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[allow(dead_code)] // 变体在 MFT/网络位置阶段启用
pub enum DriveType {
    Removable,
    Fixed,
    Network,
    Cdrom,
    Ramdisk,
    Unknown,
}

impl DriveType {
    pub fn label(self) -> &'static str {
        match self {
            DriveType::Removable => "可移动磁盘",
            DriveType::Fixed => "本地磁盘",
            DriveType::Network => "网络驱动器",
            DriveType::Cdrom => "光盘驱动器",
            DriveType::Ramdisk => "虚拟磁盘",
            DriveType::Unknown => "未知",
        }
    }

    #[cfg(windows)] // 仅 Windows 原生枚举路径使用（MFT 阶段启用）
    fn from_win(dt: u32) -> Self {
        match dt {
            2 => DriveType::Removable,
            3 => DriveType::Fixed,
            4 => DriveType::Network,
            5 => DriveType::Cdrom,
            6 => DriveType::Ramdisk,
            _ => DriveType::Unknown,
        }
    }
}

/// 一个驱动器的完整信息。
#[derive(Clone, Debug)]
pub struct DriveInfo {
    pub letter: char,
    /// 卷标；为空时回落类型标签（"本地磁盘"）。
    pub label: String,
    pub drive_type: DriveType,
    pub total_bytes: u64,
    pub free_bytes: u64,
}

impl DriveInfo {
    /// 侧边栏显示名："Win11 (C:)" / "本地磁盘 (D:)"。
    pub fn display_label(&self) -> String {
        let base = if self.label.is_empty() {
            self.drive_type.label().to_string()
        } else {
            self.label.clone()
        };
        format!("{base} ({}:)", self.letter)
    }

    /// 容量占用比 0..1。
    pub fn usage(&self) -> f64 {
        if self.total_bytes == 0 {
            return 0.0;
        }
        ((self.total_bytes - self.free_bytes) as f64 / self.total_bytes as f64).clamp(0.0, 1.0)
    }
}

/// 回收站统计：条目数与占用体积。
#[derive(Clone, Copy, Debug, Default)]
pub struct RecycleBinStats {
    pub items: i64,
    pub bytes: i64,
}

/// 快速访问目录（按固定顺序）。id 为稳定 ASCII 标识。
pub fn quick_access_dirs() -> Vec<(&'static str, &'static str, Option<PathBuf>)> {
    let home = std::env::var_os("USERPROFILE").map(PathBuf::from);
    let appdata = std::env::var_os("APPDATA").map(PathBuf::from);
    let known = |rel: &str| -> Option<PathBuf> {
        // OneDrive 重定向的桌面/文档/图片位于 %APPDATA%\..\.. 一侧不可靠，
        // 这里按常见相对路径探测，失败则跳过该入口。
        let base = home.clone()?;
        let path = base.join(rel);
        path.exists().then_some(path)
    };
    let one_drive = |rel: &str| -> Option<PathBuf> {
        let base = home.clone()?;
        let path = base.join("OneDrive").join(rel);
        path.exists().then_some(path)
    };
    vec![
        ("桌面", "desktop", known("Desktop").or_else(|| one_drive("Desktop"))),
        ("下载", "downloads", known("Downloads")),
        ("图片", "pictures", known("Pictures").or_else(|| one_drive("Pictures"))),
        ("文档", "documents", known("Documents").or_else(|| one_drive("Documents"))),
        ("音乐", "music", known("Music")),
        ("视频", "videos", known("Videos")),
        (
            "回收站",
            "recycle",
            appdata.as_ref().map(|_| PathBuf::from("recycle://")),
        ),
    ]
}

/// 枚举所有逻辑驱动器。
#[cfg(windows)]
pub fn list_drives() -> Vec<DriveInfo> {
    use windows_sys::Win32::Storage::FileSystem::{
        GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
    };
    let mut drives = Vec::new();
    let mask = unsafe { GetLogicalDrives() };
    if mask == 0 {
        return drives;
    }
    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = (b'A' + i as u8) as char;
        let root: Vec<u16> = format!("{letter}:\\")
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();

        let drive_type = unsafe { GetDriveTypeW(root.as_ptr()) };
        if drive_type == 1 {
            // DRIVE_NO_ROOT_DIR：无效路径。
            continue;
        }

        let mut volume_name = [0u16; 260];
        let mut fs_name = [0u16; 260];
        let mut serial = 0u32;
        let mut max_comp_len = 0u32;
        let mut fs_flags = 0u32;
        let got_volume = unsafe {
            GetVolumeInformationW(
                root.as_ptr(),
                volume_name.as_mut_ptr(),
                volume_name.len() as u32,
                &mut serial,
                &mut max_comp_len,
                &mut fs_flags,
                fs_name.as_mut_ptr(),
                fs_name.len() as u32,
            )
        };
        let label = if got_volume == 0 {
            String::new()
        } else {
            let end = volume_name
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(volume_name.len());
            String::from_utf16_lossy(&volume_name[..end])
        };

        let mut free_bytes_available = 0u64;
        let mut total_bytes = 0u64;
        let mut total_free_bytes = 0u64;
        let got_space = unsafe {
            GetDiskFreeSpaceExW(
                root.as_ptr(),
                &mut free_bytes_available,
                &mut total_bytes,
                &mut total_free_bytes,
            )
        };
        let (total, free) = if got_space == 0 {
            (0, 0)
        } else {
            (total_bytes, free_bytes_available)
        };

        drives.push(DriveInfo {
            letter,
            label,
            drive_type: DriveType::from_win(drive_type),
            total_bytes: total,
            free_bytes: free,
        });
    }
    drives
}

#[cfg(not(windows))]
pub fn list_drives() -> Vec<DriveInfo> {
    Vec::new()
}

/// 查询回收站条目数与占用体积。
#[cfg(windows)]
pub fn recycle_bin_stats() -> RecycleBinStats {
    use windows_sys::Win32::UI::Shell::{SHQueryRecycleBinW, SHQUERYRBINFO};

    let mut info = SHQUERYRBINFO {
        cbSize: std::mem::size_of::<SHQUERYRBINFO>() as u32,
        i64Size: 0,
        i64NumItems: 0,
    };
    let hr = unsafe { SHQueryRecycleBinW(std::ptr::null(), &mut info) };
    if hr < 0 {
        return RecycleBinStats::default();
    }
    RecycleBinStats {
        items: info.i64NumItems,
        bytes: info.i64Size,
    }
}

#[cfg(not(windows))]
pub fn recycle_bin_stats() -> RecycleBinStats {
    RecycleBinStats::default()
}

/// 虚拟回收站路径（站内浏览用，不对应真实目录）。
pub fn recycle_virtual_path() -> PathBuf {
    PathBuf::from("recycle://")
}

/// 列举回收站内的条目（原始路径 + 删除时间 + 体积），供站内浏览。
pub fn list_recycle_entries() -> Vec<super::entry::FileEntry> {
    use std::time::{Duration, UNIX_EPOCH};

    let Ok(items) = trash::os_limited::list() else {
        return Vec::new();
    };
    let mut entries = Vec::with_capacity(items.len());
    for item in items {
        let original = item.original_path();
        let name = item.name.to_string_lossy().into_owned();
        let modified = (item.time_deleted > 0)
            .then(|| UNIX_EPOCH + Duration::from_secs(item.time_deleted as u64));
        // 体积通过回收站元数据读取；失败时置 0（显示为空）。
        let size = trash::os_limited::metadata(&item)
            .map(|meta| match meta.size {
                trash::TrashItemSize::Bytes(bytes) => bytes,
                _ => 0,
            })
            .unwrap_or(0);
        // 原路径已不在磁盘上，目录/文件只能按扩展名推断。
        let is_dir = item.name.to_string_lossy().ends_with('/');
        let mut entry = super::entry::FileEntry::from_path(&original, is_dir, size, modified);
        if is_dir {
            entry.name = name.trim_end_matches('/').to_string();
        }
        entries.push(entry);
    }
    entries
}

/// 按原始路径还原回收站条目，成功返回还原数量。
pub fn restore_recycle_entry(original: &std::path::Path) -> usize {
    let Ok(items) = trash::os_limited::list() else {
        return 0;
    };
    let targets: Vec<_> = items
        .into_iter()
        .filter(|it| it.original_path() == original)
        .collect();
    let count = targets.len();
    if count > 0 && trash::os_limited::restore_all(targets).is_ok() {
        count
    } else {
        0
    }
}

/// 清空回收站，返回是否成功。
pub fn empty_recycle_bin() -> bool {
    let Ok(items) = trash::os_limited::list() else {
        return false;
    };
    trash::os_limited::purge_all(items).is_ok()
}

/// 彻底删除回收站中指定原始路径的条目，返回成功数量。
pub fn purge_recycle_entries(originals: &[std::path::PathBuf]) -> usize {
    let Ok(items) = trash::os_limited::list() else {
        return 0;
    };
    let targets: Vec<_> = items
        .into_iter()
        .filter(|it| originals.iter().any(|p| p == &it.original_path()))
        .collect();
    if targets.is_empty() {
        return 0;
    }
    let count = targets.len();
    if trash::os_limited::purge_all(targets).is_ok() {
        count
    } else {
        0
    }
}

/// 枚举已连接的网络位置（映射盘与 UNC 共享）。
#[cfg(windows)]
pub fn list_network_locations() -> Vec<String> {
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::NetworkManagement::WNet::{
        NETRESOURCEW, RESOURCETYPE_ANY, RESOURCE_CONNECTED, WNetCloseEnum, WNetEnumResourceW,
        WNetOpenEnumW,
    };
    use windows_sys::Win32::Foundation::ERROR_MORE_DATA;

    let mut locations = Vec::new();
    let mut enum_handle: HANDLE = std::ptr::null_mut();
    let rc = unsafe {
        WNetOpenEnumW(
            RESOURCE_CONNECTED,
            RESOURCETYPE_ANY,
            0,
            std::ptr::null(),
            &mut enum_handle,
        )
    };
    if rc != 0 {
        return locations;
    }

    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let mut count: u32 = u32::MAX; // 尽量多取
        let mut buffer_size = buffer.len() as u32;
        let rc = unsafe {
            WNetEnumResourceW(
                enum_handle,
                &mut count,
                buffer.as_mut_ptr() as *mut _,
                &mut buffer_size,
            )
        };
        if rc == ERROR_MORE_DATA {
            buffer.resize(buffer_size as usize + 16 * 1024, 0);
            continue;
        }
        if rc != 0 {
            break;
        }
        let entries = unsafe {
            std::slice::from_raw_parts(buffer.as_ptr() as *const NETRESOURCEW, count as usize)
        };
        for entry in entries {
            if !entry.lpRemoteName.is_null() {
                let len = unsafe { wide_len(entry.lpRemoteName) };
                let name = String::from_utf16_lossy(unsafe {
                    std::slice::from_raw_parts(entry.lpRemoteName, len)
                });
                if !name.is_empty() {
                    locations.push(name);
                }
            }
        }
        if count < u32::MAX as u32 && buffer_size as usize >= buffer.len() {
            break;
        }
        // count == -1（全部返回）时已取完。
        break;
    }
    unsafe { WNetCloseEnum(enum_handle) };
    locations
}

#[cfg(windows)]
unsafe fn wide_len(ptr: *const u16) -> usize {
    let mut len = 0usize;
    while unsafe { *ptr.add(len) } != 0 {
        len += 1;
    }
    len
}

#[cfg(not(windows))]
pub fn list_network_locations() -> Vec<String> {
    Vec::new()
}
