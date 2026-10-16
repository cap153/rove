//! 纯 Rust 生成 Windows 快捷方式（.lnk）。
//!
//! 依据 MS-SHLINKS 手工序列化最小可用结构：ShellLinkHeader +
//! LinkInfo（VolumeID + LocalBasePath）+ StringData（工作目录）。
//! 全程 Unicode（IsUnicode），中文路径无损。无需 COM / 外部进程。

use std::path::{Path, PathBuf};

/// ShellLink CLSID {00021401-0000-0000-C000-000000000046}（小端字节序）。
const LINK_CLSID: [u8; 16] = [
    0x01, 0x14, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46,
];

const HAS_LINK_INFO: u32 = 0x0000_0002;
const HAS_WORKING_DIR: u32 = 0x0000_0010;
const IS_UNICODE: u32 = 0x0000_0080;

const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x20;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const SW_SHOWNORMAL: u32 = 1;

/// 为 `target` 生成 .lnk 字节流。
pub fn build_shortcut(target: &Path) -> Vec<u8> {
    let is_dir = std::fs::metadata(target).map(|m| m.is_dir()).unwrap_or(false);
    let size = if is_dir {
        0
    } else {
        std::fs::metadata(target).map(|m| m.len()).unwrap_or(0)
    };
    let working_dir = target
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();

    let target_path = target.to_string_lossy().into_owned();
    let target_utf16 = encode_utf16z(&target_path);
    let workdir_utf16 = encode_utf16z(&working_dir);
    let volume_label_utf16 = encode_utf16z("");

    // ---- VolumeID ----
    let volume_id_size = 0x14 + volume_label_utf16.len() * 2;
    let mut volume_id = Vec::with_capacity(volume_id_size);
    volume_id.extend_from_slice(&(volume_id_size as u32).to_le_bytes()); // VolumeIDSize
    let drive_type: u32 = if target_path.starts_with(r"\\") { 4 } else { 3 };
    volume_id.extend_from_slice(&drive_type.to_le_bytes()); // DriveType
    volume_id.extend_from_slice(&0u32.to_le_bytes()); // DriveSerialNumber
    volume_id.extend_from_slice(&0u32.to_le_bytes()); // VolumeLabelOffset（无 ANSI 标签）
    volume_id.extend_from_slice(&0x14u32.to_le_bytes()); // VolumeLabelOffsetUnicode
    for unit in &volume_label_utf16 {
        volume_id.extend_from_slice(&unit.to_le_bytes());
    }

    // ---- LinkInfo ----
    let local_base_offset = 0x1C + volume_id.len();
    let suffix_offset = local_base_offset + target_utf16.len() * 2;
    let link_info_size = suffix_offset + 2; // 空 CommonPathSuffix（仅 null 终止符）
    let mut link_info = Vec::with_capacity(link_info_size);
    link_info.extend_from_slice(&(link_info_size as u32).to_le_bytes()); // LinkInfoSize
    link_info.extend_from_slice(&0x1Cu32.to_le_bytes()); // LinkInfoHeaderSize
    link_info.extend_from_slice(&0x01u32.to_le_bytes()); // LinkInfoFlags: VolumeIDAndLocalBasePath
    link_info.extend_from_slice(&0x1Cu32.to_le_bytes()); // VolumeIDOffset
    link_info.extend_from_slice(&(local_base_offset as u32).to_le_bytes()); // LocalBasePathOffset
    link_info.extend_from_slice(&0u32.to_le_bytes()); // CommonNetworkRelativeLinkOffset
    link_info.extend_from_slice(&(suffix_offset as u32).to_le_bytes()); // CommonPathSuffixOffset
    link_info.extend_from_slice(&volume_id);
    for unit in &target_utf16 {
        link_info.extend_from_slice(&unit.to_le_bytes());
    }
    link_info.extend_from_slice(&0u16.to_le_bytes()); // CommonPathSuffix = ""

    // ---- ShellLinkHeader ----
    let mut out = Vec::with_capacity(0x4C + link_info.len() + workdir_utf16.len() * 2);
    out.extend_from_slice(&0x4Cu32.to_le_bytes()); // HeaderSize
    out.extend_from_slice(&LINK_CLSID);
    out.extend_from_slice(&(HAS_LINK_INFO | HAS_WORKING_DIR | IS_UNICODE).to_le_bytes());
    out.extend_from_slice(
        &(if is_dir {
            FILE_ATTRIBUTE_DIRECTORY
        } else {
            FILE_ATTRIBUTE_ARCHIVE
        })
        .to_le_bytes(),
    );
    out.extend_from_slice(&0u64.to_le_bytes()); // CreationTime
    out.extend_from_slice(&0u64.to_le_bytes()); // AccessTime
    out.extend_from_slice(&0u64.to_le_bytes()); // WriteTime
    out.extend_from_slice(&(size as u32).to_le_bytes()); // FileSize
    out.extend_from_slice(&0i32.to_le_bytes()); // IconIndex
    out.extend_from_slice(&SW_SHOWNORMAL.to_le_bytes()); // ShowCommand
    out.extend_from_slice(&0u16.to_le_bytes()); // HotKey
    out.extend_from_slice(&0u16.to_le_bytes()); // Reserved1
    out.extend_from_slice(&0u32.to_le_bytes()); // Reserved2
    out.extend_from_slice(&0u32.to_le_bytes()); // Reserved3

    out.extend_from_slice(&link_info);

    // ---- StringData：WORKING_DIR（Unicode，null 结尾） ----
    for unit in &workdir_utf16 {
        out.extend_from_slice(&unit.to_le_bytes());
    }

    out
}

fn encode_utf16z(s: &str) -> Vec<u16> {
    let mut v: Vec<u16> = s.encode_utf16().collect();
    v.push(0);
    v
}

/// 用户桌面目录。
pub fn desktop_dir() -> Option<PathBuf> {
    let profile = std::env::var_os("USERPROFILE").map(PathBuf::from)?;
    let direct = profile.join("Desktop");
    if direct.exists() {
        return Some(direct);
    }
    let one_drive = profile.join("OneDrive").join("Desktop");
    one_drive.exists().then_some(one_drive)
}

/// 用户开始菜单 Programs 目录。
pub fn start_menu_programs_dir() -> Option<PathBuf> {
    let appdata = std::env::var_os("APPDATA").map(PathBuf::from)?;
    Some(
        appdata.join("Microsoft").join("Windows").join("Start Menu").join("Programs"),
    )
}

/// 在 `dir` 中创建指向 `target` 的快捷方式；返回 .lnk 路径。
/// 已存在同名文件时追加序号，不覆盖。
pub fn create_shortcut_in(dir: &Path, target: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("创建目录失败: {e}"))?;
    let stem = target
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "快捷方式".to_string());
    let mut path = dir.join(format!("{stem}.lnk"));
    let mut counter = 2u32;
    while path.exists() {
        path = dir.join(format!("{stem} ({counter}).lnk"));
        counter += 1;
    }
    let bytes = build_shortcut(target);
    std::fs::write(&path, bytes).map_err(|e| format!("写入快捷方式失败: {e}"))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_layout_is_76_bytes() {
        let target = Path::new("C:\\Windows\\notepad.exe");
        let bytes = build_shortcut(target);
        // HeaderSize 字段自描述。
        assert_eq!(u32::from_le_bytes(bytes[0..4].try_into().unwrap()), 0x4C);
        assert_eq!(&bytes[4..20], &LINK_CLSID);
        // LinkInfo 紧跟 76 字节头之后，其尺寸自描述。
        let link_info_size = u32::from_le_bytes(bytes[0x4C..0x50].try_into().unwrap()) as usize;
        // WORKING_DIR 长度按生产代码相同的方式推导（跨平台一致：
        // Unix 上反斜杠路径的 parent 为空串，Windows 上为 "C:\Windows"）。
        let workdir = target
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let workdir_bytes = (workdir.encode_utf16().count() + 1) * 2;
        assert_eq!(bytes.len(), 0x4C + link_info_size + workdir_bytes);
        // 顺带验证 LinkInfo 内部偏移：VolumeIDOffset = 0x1C，
        // LocalBasePathOffset = 0x1C + VolumeID 尺寸（20 字节头 + 空标签 2 字节）。
        let volume_id_offset =
            u32::from_le_bytes(bytes[0x4C + 12..0x4C + 16].try_into().unwrap()) as usize;
        let local_base_offset =
            u32::from_le_bytes(bytes[0x4C + 16..0x4C + 20].try_into().unwrap()) as usize;
        assert_eq!(volume_id_offset, 0x1C);
        assert_eq!(local_base_offset, 0x1C + 0x14 + 2);
    }

    #[test]
    fn utf16_encoding_roundtrip() {
        assert_eq!(encode_utf16z("中文"), vec![0x4E2D, 0x6587, 0]);
    }
}
