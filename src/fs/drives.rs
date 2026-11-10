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
        // from_path 只负责「显示值」：名称 / 扩展名 / 图标风格 / 类型文本。
        let mut entry = super::entry::FileEntry::from_path(&original, is_dir, size, modified, false);
        if is_dir {
            entry.name = name.trim_end_matches('/').to_string();
        }
        // 身份键必须唯一：同一目录、不同时间删除的同名文件 original_path 完全
        // 相同，拿它当 path 会让选择集 / 高亮 / 缩略图缓存把它们当成同一项。
        // trash 的 id 是 Shell 条目的 desktop-absolute parsing name，逐条唯一。
        // 原始路径保留下来供还原、彻底删除和「路径」列使用。
        entry.original_path = Some(original);
        entry.path = std::path::PathBuf::from(item.id.clone());
        entries.push(entry);
    }
    entries
}

/// 回收站条目的匹配键：唯一 Shell id 优先，也兼容仍传原始路径的调用
/// （剪切→粘贴时剪贴板里保存的是原始路径）。
fn trash_item_matches(item: &trash::TrashItem, key: &std::path::Path) -> bool {
    std::path::Path::new(&item.id) == key || item.original_path().as_path() == key
}

/// 查回收站条目的删除前原始路径（还原前调用，用于判断是否还需要移到目标目录）。
pub fn recycle_original_path(key: &std::path::Path) -> Option<std::path::PathBuf> {
    trash::os_limited::list()
        .ok()?
        .iter()
        .find(|it| trash_item_matches(it, key))
        .map(|it| it.original_path())
}

/// 按唯一 id（或兼容的原始路径）还原回收站条目，成功返回还原数量。
/// 按 id 命中只还原那一条，不会把同名条目一起还原。
pub fn restore_recycle_entry(key: &std::path::Path) -> usize {
    let Ok(items) = trash::os_limited::list() else {
        return 0;
    };
    let targets: Vec<_> = items
        .into_iter()
        .filter(|it| trash_item_matches(it, key))
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

/// 彻底删除回收站中指定的条目，返回成功数量。
pub fn purge_recycle_entries(keys: &[std::path::PathBuf]) -> usize {
    let Ok(items) = trash::os_limited::list() else {
        return 0;
    };
    let targets: Vec<_> = items
        .into_iter()
        .filter(|it| keys.iter().any(|p| trash_item_matches(it, p.as_path())))
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
    use windows_sys::Win32::NetworkManagement::WNet::{RESOURCE_CONNECTED, RESOURCE_REMEMBERED};

    let mut locations = enumerate_wnet(RESOURCE_CONNECTED);
    // 断开但记住的映射（资源管理器里依然显示）也要列出来。
    for remote in enumerate_wnet(RESOURCE_REMEMBERED) {
        if !locations
            .iter()
            .any(|known| known.eq_ignore_ascii_case(&remote))
        {
            locations.push(remote);
        }
    }
    if !locations.is_empty() {
        crate::logv!("[net] wnet={:?}", locations);
    }
    locations
}

#[cfg(windows)]
fn enumerate_wnet(scope: windows_sys::Win32::NetworkManagement::WNet::NET_RESOURCE_SCOPE) -> Vec<String> {
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::NetworkManagement::WNet::{
        NETRESOURCEW, RESOURCETYPE_ANY, WNetCloseEnum, WNetEnumResourceW, WNetOpenEnumW,
    };
    use windows_sys::Win32::Foundation::ERROR_MORE_DATA;

    let mut locations = Vec::new();
    let mut enum_handle: HANDLE = std::ptr::null_mut();
    let rc = unsafe {
        WNetOpenEnumW(
            scope,
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

/// `%APPDATA%\Microsoft\Windows\Network Shortcuts`：资源管理器「添加一个
/// 网络位置」向导把结果存成这里的 `.lnk` 快捷方式，**不会**建立网络连接。
#[cfg(windows)]
fn network_shortcuts_dir() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("APPDATA")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .map(|p| std::path::PathBuf::from(p).join("AppData/Roaming"))
        })?;
    Some(base.join("Microsoft").join("Windows").join("Network Shortcuts"))
}

/// 资源管理器里的「网络位置」条目。
///
/// 这些东西**不是网络连接**（`WNet` 枚举不到），只是
/// `%APPDATA%\Microsoft\Windows\Network Shortcuts` 下的快捷方式文件。
/// 这里直接读文件字节解 `LinkInfo`：纯本地、微秒级，不会像 shell 枚举那样
/// 去探网络（`IShellFolder::EnumObjects` 在本机上卡过 28 秒）。
#[cfg(windows)]
pub fn list_network_places() -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let Some(dir) = network_shortcuts_dir() else {
        crate::logln!("[net] shortcuts dir: APPDATA/USERPROFILE missing");
        return out;
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        crate::logln!("[net] shortcuts dir missing {}", dir.display());
        return out;
    };
    let mut seen: Vec<String> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    let mut dir_errors = 0usize;
    for entry in entries {
        let Ok(entry) = entry else {
            dir_errors += 1;
            continue;
        };
        let path = entry.path();
        // 向导生成的条目未必带 `.lnk` 扩展名，所以不能按扩展名筛；
        // 也不能无条件用 `file_stem`：名字里带普通点（“Samba 4.22.4”）会被吃掉半截。
        let Some(name) = path.file_name().map(|s| s.to_string_lossy().to_string()) else {
            continue;
        };
        // 条目类型写进日志：绕过 is_file / 扩展名两道猜测。
        let kind = match entry.file_type() {
            Ok(ft) => format!(
                "{}{}{}",
                if ft.is_file() { "file" } else { "-file" },
                if ft.is_dir() { " dir" } else { "" },
                if ft.is_symlink() { " symlink" } else { "" }
            ),
            Err(err) => format!("file_type err: {err}"),
        };
        let label = if path
            .extension()
            .map(|ext| ext.eq_ignore_ascii_case("lnk"))
            .unwrap_or(false)
        {
            path.file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| name.clone())
        } else {
            name.clone()
        };
        if label.is_empty() {
            continue;
        }
        seen.push(format!("{name} [{kind}]"));
        let file_type = entry.file_type().ok();
        // 情况一：重解析点/符号链接，目标是 UNC 就直接用。
        if file_type.map(|ft| ft.is_symlink()).unwrap_or(false) {
            match std::fs::read_link(&path) {
                Ok(link) => {
                    let raw = link.to_string_lossy().to_string();
                    match normalize_network_path(&raw) {
                        Some(target) => out.push((label, target)),
                        None => skipped.push(format!("{label} -> {raw} (link, not unc)")),
                    }
                }
                Err(err) => skipped.push(format!("{label} (read_link err: {err})")),
            }
            continue;
        }
        // 情况二：目录版的网络位置（资源管理器就是这么存的），进目录找 UNC。
        if file_type.map(|ft| ft.is_dir()).unwrap_or(false) {
            let children = dir_children(&path);
            match dir_network_target(&path) {
                Some(target) => out.push((label, target)),
                None => skipped.push(format!("{label} (dir, no unc inside; children={children:?})")),
            }
            continue;
        }
        // 情况三：普通 `.lnk` 文件，解 LinkInfo。
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) => {
                skipped.push(format!("{label} (read err: {err})"));
                continue;
            }
        };
        match lnk_target_from_bytes(&bytes) {
            Some(target) => match normalize_network_path(&target) {
                Some(target) => out.push((label, target)),
                None => skipped.push(format!("{label} -> {target} (not unc)")),
            },
            None => skipped.push(format!("{label} (no target, {} bytes)", bytes.len())),
        }
    }
    crate::logv!(
        "[net] shortcuts dir={} errors={} files={:?} places={:?} skipped={:?}",
        dir.display(),
        dir_errors,
        seen,
        out,
        skipped
    );
    // 只有“看到了但没解析出”的才算异常，升级到常规日志。
    if !skipped.is_empty() {
        crate::logln!("[net] unparsed shortcuts: {skipped:?}");
    }
    out
}

/// 目录版「网络位置」：读目录里的每个文件（`desktop.ini` / `target.lnk` ...），
/// 先试 `.lnk` 的 `LinkInfo`，再退到全字节搜 UNC 字串。
fn dir_network_target(dir: &std::path::Path) -> Option<String> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut found: Option<String> = None;
    for entry in entries.flatten() {
        if !entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
            continue;
        }
        let Ok(bytes) = std::fs::read(entry.path()) else {
            continue;
        };
        if let Some(target) = lnk_target_from_bytes(&bytes).and_then(|t| normalize_network_path(&t)) {
            return Some(target);
        }
        if let Some(target) = scan_bytes_for_unc(&bytes) {
            found = found.or(Some(target));
        }
    }
    found
}

/// 目录里的文件名（只用于日志排查）。
fn dir_children(dir: &std::path::Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| {
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            format!("{} ({size}B)", entry.file_name().to_string_lossy())
        })
        .collect()
}

/// 在任意字节里找第一个 `\\host\share`：UTF-16LE 和 ANSI/UTF-8 两种写法都试。
fn scan_bytes_for_unc(data: &[u8]) -> Option<String> {
    let mut i = 0usize;
    while i + 3 < data.len() {
        // UTF-16LE: 5C 00 5C 00
        if data[i] == b'\\' && data[i + 1] == 0 && data[i + 2] == b'\\' && data[i + 3] == 0 {
            let mut units: Vec<u16> = Vec::new();
            let mut j = i;
            while j + 1 < data.len() {
                let unit = u16::from_le_bytes([data[j], data[j + 1]]);
                if unit == 0 || unit < 0x20 {
                    break;
                }
                units.push(unit);
                j += 2;
            }
            if let Some(target) = normalize_network_path(&String::from_utf16_lossy(&units)) {
                return Some(target);
            }
        }
        // ANSI / UTF-8: 5C 5C
        if data[i] == b'\\' && data[i + 1] == b'\\' {
            let mut bytes: Vec<u8> = Vec::new();
            let mut j = i;
            while j < data.len() {
                let byte = data[j];
                if byte < 0x20 {
                    break;
                }
                bytes.push(byte);
                j += 1;
            }
            if let Some(target) = normalize_network_path(&String::from_utf8_lossy(&bytes)) {
                return Some(target);
            }
        }
        i += 1;
    }
    None
}

/// 把各种写法归一成 `\\host\share`；不是 UNC 就返回 `None`。
///
/// 兼容 `\\?\UNC\host\share`、`\\?\host\share` 和正斜杠写法。
fn normalize_network_path(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let slashed = trimmed.replace('/', "\\");
    let body = if let Some(rest) = slashed.strip_prefix("\\\\?\\UNC\\") {
        format!("\\\\{rest}")
    } else if let Some(rest) = slashed.strip_prefix("\\\\?\\") {
        rest.to_string()
    } else {
        slashed
    };
    let body = body.trim_end_matches('\\');
    body.starts_with("\\\\").then(|| body.to_string())
}

/// 从 `.lnk` 字节里取目标路径。
///
/// 只看 `LinkInfo`（MS-SHLLINK 2.3）：其中的 `LocalBasePath` 就是
/// `\\server\share`；Unicode 版本优先，因为 ANSI 版会被 MAX_PATH 截断。
/// 最后再上一个彻底兜底：全文件搜第一个 UTF-16 的 `\\`。
fn lnk_target_from_bytes(data: &[u8]) -> Option<String> {
    if data.len() < 0x4C + 4 {
        return None;
    }
    let u32_at = |off: usize| -> Option<u32> {
        Some(u32::from_le_bytes(data.get(off..off + 4)?.try_into().ok()?))
    };
    let u16_at = |off: usize| -> Option<u16> {
        Some(u16::from_le_bytes(data.get(off..off + 2)?.try_into().ok()?))
    };
    let wide_at = |off: usize| -> Option<String> {
        let mut units: Vec<u16> = Vec::new();
        let mut i = off;
        while i + 1 < data.len() {
            let unit = u16::from_le_bytes([data[i], data[i + 1]]);
            if unit == 0 {
                break;
            }
            units.push(unit);
            i += 2;
        }
        (!units.is_empty()).then(|| String::from_utf16_lossy(&units))
    };
    let ansi_at = |off: usize| -> Option<String> {
        let mut bytes: Vec<u8> = Vec::new();
        let mut i = off;
        while i < data.len() {
            if data[i] == 0 {
                break;
            }
            bytes.push(data[i]);
            i += 1;
        }
        (!bytes.is_empty()).then(|| String::from_utf8_lossy(&bytes).into_owned())
    };

    // 头部 0x14 处的 LinkFlags：bit0 = 有 LinkTargetIDList，bit1 = 有 LinkInfo。
    let flags = u32_at(0x14)?;
    let mut off = 0x4Cusize;
    if flags & 0x1 != 0 {
        let size = u16_at(off)? as usize;
        off += 2 + size;
    }
    let join = |mut base: String, suffix: Option<String>| -> String {
        if let Some(suffix) = suffix {
            if !suffix.is_empty() {
                base.push_str(suffix.trim_start_matches('\\'));
                return base;
            }
        }
        base
    };
    if flags & 0x2 != 0 {
        let li = off;
        let li_size = u32_at(li)? as usize;
        let header = u16_at(li + 4)? as usize;
        let li_flags = u32_at(li + 8)?;
        let suffix_a = u32_at(li + 24).unwrap_or(0) as usize;
        let (local_u, suffix_u) = if header >= 0x24 {
            (
                u32_at(li + 28).unwrap_or(0) as usize,
                u32_at(li + 32).unwrap_or(0) as usize,
            )
        } else {
            (0, 0)
        };
        if li_size >= 0x1C && li_flags & 0x1 != 0 {
            if local_u != 0 && local_u < li_size {
                if let Some(base) = wide_at(li + local_u) {
                    let suffix = (suffix_u != 0 && suffix_u < li_size)
                        .then(|| wide_at(li + suffix_u))
                        .flatten();
                    let target = join(base, suffix);
                    if !target.is_empty() {
                        return Some(target);
                    }
                }
            }
            let local = u32_at(li + 16).unwrap_or(0) as usize;
            if local != 0 && local < li_size {
                if let Some(base) = ansi_at(li + local) {
                    let suffix = (suffix_a != 0 && suffix_a < li_size)
                        .then(|| ansi_at(li + suffix_a))
                        .flatten();
                    let target = join(base, suffix);
                    if !target.is_empty() {
                        return Some(target);
                    }
                }
            }
        }
    }
    // 兜底：全文件搜第一个 UNC 字串。
    scan_bytes_for_unc(data)
}

#[cfg(not(windows))]
pub fn list_network_places() -> Vec<(String, String)> {
    Vec::new()
}
#[cfg(test)]
mod tests {
    use super::{lnk_target_from_bytes, normalize_network_path, scan_bytes_for_unc};

    fn put_u32(buf: &mut [u8], at: usize, value: u32) {
        buf[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn put_u16(buf: &mut [u8], at: usize, value: u16) {
        buf[at..at + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn utf16(text: &str) -> Vec<u8> {
        text.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
    }

    /// 最小可用的 `.lnk`：只有 LinkInfo，Unicode 版 LocalBasePath。
    fn sample(with_idlist: bool, path: &str) -> Vec<u8> {
        const LI_HEADER: usize = 0x24;
        let mut buf = vec![0u8; 0x4C];
        let mut flags = 0x2u32; // HasLinkInfo
        let mut idlist: Vec<u8> = Vec::new();
        if with_idlist {
            flags |= 0x1; // HasLinkTargetIDList
            idlist.extend_from_slice(&4u16.to_le_bytes());
            idlist.extend_from_slice(&[0, 0, 0, 0]);
        }
        let mut link_info = vec![0u8; LI_HEADER];
        link_info.extend_from_slice(&utf16(path));
        link_info.extend_from_slice(&[0, 0]);
        let size = link_info.len() as u32;
        put_u32(&mut link_info, 0, size);
        put_u16(&mut link_info, 4, LI_HEADER as u16);
        put_u32(&mut link_info, 8, 0x1); // VolumeIDAndLocalBasePath
        put_u32(&mut link_info, 12, 0x1C);
        put_u32(&mut link_info, 28, LI_HEADER as u32); // 路径紧跟在头后面
        buf.extend_from_slice(&idlist);
        buf.extend_from_slice(&link_info);
        put_u32(&mut buf, 0x14, flags);
        buf
    }

    #[test]
    fn lnk_parses_unicode_local_base_path() {
        let bytes = sample(false, r"\\192.168.1.5\Video");
        assert_eq!(
            lnk_target_from_bytes(&bytes).as_deref(),
            Some(r"\\192.168.1.5\Video")
        );
    }

    #[test]
    fn lnk_parses_with_idlist_prefix() {
        let bytes = sample(true, r"\\nas\share");
        assert_eq!(lnk_target_from_bytes(&bytes).as_deref(), Some(r"\\nas\share"));
    }

    #[test]
    fn lnk_rejects_garbage() {
        assert_eq!(lnk_target_from_bytes(b"not a link"), None);
    }

    #[test]
    fn scans_unc_from_utf16_and_ansi() {
        let utf16: Vec<u8> = r"\\192.168.1.5\Video"
            .encode_utf16()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        let mut bytes = b"junk".to_vec();
        bytes.extend_from_slice(&utf16);
        assert_eq!(
            scan_bytes_for_unc(&bytes).as_deref(),
            Some(r"\\192.168.1.5\Video")
        );

        let ansi = b"NetworkLocation=\\\\nas\\share\r\n";
        assert_eq!(scan_bytes_for_unc(ansi).as_deref(), Some(r"\\nas\share"));
    }

    #[test]
    fn normalize_handles_extended_prefixes() {
        assert_eq!(
            normalize_network_path(r"\\?\UNC\192.168.1.5\Video\").as_deref(),
            Some(r"\\192.168.1.5\Video")
        );
        assert_eq!(
            normalize_network_path("//nas/share").as_deref(),
            Some(r"\\nas\share")
        );
        assert_eq!(normalize_network_path("ftp://nas/share"), None);
    }
}
