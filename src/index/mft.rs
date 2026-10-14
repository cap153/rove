//! $MFT 直读解析（参考 pulse `index_mft.cpp`）。
//!
//! * 打开 `\\.\C:`，`FSCTL_GET_NTFS_VOLUME_DATA` 取卷参数；
//! * 读 record 0 的 `$DATA(0x80)` run list 定位 $MFT 数据范围；
//! * 逐记录解析：USA fixup、flags、`$FILE_NAME(0x30)`、`$DATA(0x80)`；
//! * 按 namespace 优先级选主名，DOS 名丢弃。

use std::io;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_MODE, ReadFile, SetFilePointerEx,
    OPEN_EXISTING, FILE_BEGIN,
};
use windows::Win32::System::Ioctl::{FSCTL_GET_NTFS_VOLUME_DATA, NTFS_VOLUME_DATA_BUFFER};
use windows::Win32::System::IO::DeviceIoControl;

use super::tree::{Node, Tree};

/// NTFS 属性类型
const ATTRIBUTE_FILE_NAME: u32 = 0x30;
const ATTRIBUTE_DATA: u32 = 0x80;

/// $FILE_NAME namespace
const NS_POSIX: u8 = 0;
const NS_WIN32: u8 = 1;
const NS_DOS: u8 = 2;
const NS_WIN32_DOS: u8 = 3;

/// 卷参数与 $MFT 位置
pub struct VolumeInfo {
    #[allow(dead_code)]
    pub drive: String,
    pub bytes_per_file_record: u32,
    pub bytes_per_cluster: u32,
    /// $MFT 的字节区间（偏移, 长度）
    pub mft_runs: Vec<(u64, u64)>,
}

/// 尝试为当前进程启用 `SeManageVolumePrivilege`。
/// 管理员登录时该权限存在但默认禁用，启用后无需以管理员身份重启即可读取裸卷。
pub fn enable_manage_volume_privilege() -> bool {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{
        AdjustTokenPrivileges, LookupPrivilegeValueW, SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES,
        TOKEN_PRIVILEGES, TOKEN_QUERY,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    unsafe {
        let mut token: HANDLE = HANDLE::default();
        if OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        )
        .is_err()
        {
            return false;
        }

        let mut privileges = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            ..std::mem::zeroed()
        };
        let name = crate::wstr::wide("SeManageVolumePrivilege");
        if LookupPrivilegeValueW(None, crate::wstr::pcw(&name), &mut privileges.Privileges[0].Luid)
            .is_err()
        {
            let _ = CloseHandle(token);
            return false;
        }
        privileges.Privileges[0].Attributes = SE_PRIVILEGE_ENABLED;

        let ok = AdjustTokenPrivileges(token, false, Some(&privileges as *const _), 0, None, None)
            .is_ok();
        let _ = CloseHandle(token);
        ok
    }
}

/// 打开卷（`\\.\C:`）
pub fn open_volume(drive: &str) -> io::Result<HANDLE> {
    let path = crate::wstr::wide(&format!("\\\\.\\{drive}"));
    unsafe {
        CreateFileW(
            crate::wstr::pcw(&path),
            0x8000_0000 | 0x4000_0000, // GENERIC_READ | GENERIC_WRITE
            FILE_SHARE_MODE(0x0000_0001 | 0x0000_0002),     // READ | WRITE
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
    }
    .map_err(|e| io::Error::other(e.to_string()))
}

pub fn close_volume(handle: HANDLE) {
    unsafe {
        let _ = CloseHandle(handle);
    }
}

/// 读取卷参数并解析 record 0 得到 $MFT run list
pub fn read_volume_info(handle: HANDLE, drive: &str) -> io::Result<VolumeInfo> {
    let mut data: NTFS_VOLUME_DATA_BUFFER = unsafe { std::mem::zeroed() };
    let mut returned: u32 = 0;
    unsafe {
        DeviceIoControl(
            handle,
            FSCTL_GET_NTFS_VOLUME_DATA,
            None,
            0,
            Some(&mut data as *mut _ as *mut core::ffi::c_void),
            std::mem::size_of::<NTFS_VOLUME_DATA_BUFFER>() as u32,
            Some(&mut returned as *mut u32),
            None,
        )
    }
    .map_err(|e| io::Error::other(e.to_string()))?;

    let record_size = if data.BytesPerFileRecordSegment > 0 {
        data.BytesPerFileRecordSegment
    } else {
        1024
    };

    // record 0 位于 MFT 起点
    let mft_offset = (data.MftStartLcn as u64) * data.BytesPerCluster as u64;
    let record = read_at(handle, mft_offset, record_size as usize)?;
    let runs = parse_record0_runs(&record).unwrap_or_else(|| {
        // 回退：用 MftValidDataLength 覆盖整个 MFT
        vec![(mft_offset, data.MftValidDataLength.max(0) as u64)]
    });

    Ok(VolumeInfo {
        drive: drive.to_string(),
        bytes_per_file_record: record_size,
        bytes_per_cluster: data.BytesPerCluster,
        mft_runs: runs,
    })
}

/// 从卷句柄读取指定偏移的数据
fn read_at(handle: HANDLE, offset: u64, length: usize) -> io::Result<Vec<u8>> {
    let mut buffer = vec![0u8; length];
    let mut read: u32 = 0;
    unsafe {
        let _ = SetFilePointerEx(handle, offset as i64, None, FILE_BEGIN);
        ReadFile(
            handle,
            Some(buffer.as_mut_slice()),
            Some(&mut read as *mut u32),
            None,
        )
    }
    .map_err(|e| io::Error::other(e.to_string()))?;
    buffer.truncate(read as usize);
    Ok(buffer)
}

/// 解析 record 0 中 $MFT 自身 `$DATA(0x80)` 的 run list
fn parse_record0_runs(record: &[u8]) -> Option<Vec<(u64, u64)>> {
    if record.len() < 0x38 || &record[0..4] != b"FILE" {
        return None;
    }
    let attr_offset = read_u16(record, 0x14) as usize;
    let bytes_used = read_u32(record, 0x18) as usize;
    let end = bytes_used.min(record.len());
    let mut offset = attr_offset;

    while offset + 0x40 <= end {
        let attr_type = read_u32(record, offset);
        if attr_type == 0xFFFF_FFFF {
            break;
        }
        let attr_len = read_u32(record, offset + 4) as usize;
        if attr_len < 0x18 || offset + attr_len > record.len() {
            break;
        }
        let non_resident = record[offset + 8] != 0;
        if attr_type == ATTRIBUTE_DATA && non_resident {
            let runlist_offset = read_u16(record, offset + 0x20) as usize;
            if let Some(runs) = decode_runs(&record[offset + runlist_offset..]) {
                return Some(runs);
            }
        }
        offset += attr_len;
    }
    None
}

/// 解码 NTFS run list 为 [(LCN, 簇数)]
fn decode_runs(data: &[u8]) -> Option<Vec<(u64, u64)>> {
    let mut runs = Vec::new();
    let mut offset = 0usize;
    let mut lcn: i64 = 0;

    while offset < data.len() {
        let header = data[offset];
        if header == 0 {
            break;
        }
        let len_size = (header & 0x0F) as usize;
        let off_size = (header >> 4) as usize;
        offset += 1;
        if len_size == 0 || offset + len_size + off_size > data.len() {
            break;
        }
        let mut run_length: u64 = 0;
        for i in 0..len_size {
            run_length |= (data[offset + i] as u64) << (8 * i);
        }
        offset += len_size;

        if off_size == 0 {
            continue; // sparse run
        }
        let mut delta: i64 = 0;
        for i in 0..off_size {
            delta |= (data[offset + i] as i64) << (8 * i);
        }
        let sign_bit = 1i64 << (8 * off_size - 1);
        if delta & sign_bit != 0 {
            delta -= 1i64 << (8 * off_size);
        }
        offset += off_size;
        lcn += delta;
        runs.push((lcn.max(0) as u64, run_length));
    }

    if runs.is_empty() {
        None
    } else {
        Some(runs)
    }
}

/// 全量扫描 $MFT 填充目录树；返回节点数
pub fn scan_mft(handle: HANDLE, info: &VolumeInfo, tree: &mut Tree) -> io::Result<usize> {
    let record_size = info.bytes_per_file_record as usize;
    let cluster = info.bytes_per_cluster as u64;
    let chunk: usize = 8 * 1024 * 1024;
    let mut total = 0usize;

    for (lcn, clusters) in &info.mft_runs {
        let start = lcn * cluster;
        let length = clusters * cluster;
        let mut position = 0u64;
        while position < length {
            let to_read = ((length - position) as usize).min(chunk);
            let Ok(buffer) = read_at(handle, start + position, to_read) else {
                break;
            };
            position += to_read as u64;

            let mut offset = 0usize;
            while offset + record_size <= buffer.len() {
                let record = &buffer[offset..offset + record_size];
                offset += record_size;
                if let Some(node) = parse_file_record(record) {
                    let frn = record_frn(record);
                    tree.upsert(frn, node);
                    total += 1;
                }
            }
        }
    }
    Ok(total)
}

/// MFT 记录号 + sequence -> FRN
fn record_frn(record: &[u8]) -> u64 {
    let index = read_u32(record, 0x2C) as u64;
    let sequence = read_u16(record, 0x10) as u64;
    (sequence << 48) | (index & 0x0000_FFFF_FFFF_FFFF)
}

/// 解析一条 FILE 记录为树节点
fn parse_file_record(record: &[u8]) -> Option<Node> {
    if record.len() < 0x40 || &record[0..4] != b"FILE" {
        return None;
    }
    let flags = read_u16(record, 0x16);
    if flags & 0x0001 == 0 {
        return None; // 未在使用
    }
    if read_u64(record, 0x20) != 0 {
        return None; // 扩展记录
    }

    let attr_offset = read_u16(record, 0x14) as usize;
    let bytes_used = read_u32(record, 0x18) as usize;
    let end = bytes_used.min(record.len());
    let mut offset = attr_offset;

    // (namespace 优先级, parent FRN, 名称)
    let mut best: Option<(u8, u64, String)> = None;
    let mut size: u64 = 0;

    while offset + 0x18 <= end {
        let attr_type = read_u32(record, offset);
        if attr_type == 0xFFFF_FFFF {
            break;
        }
        let attr_len = read_u32(record, offset + 4) as usize;
        if attr_len < 0x18 || offset + attr_len > record.len() {
            break;
        }
        let non_resident = record[offset + 8] != 0;

        if attr_type == ATTRIBUTE_FILE_NAME && !non_resident {
            let value_length = read_u32(record, offset + 0x10) as usize;
            let value_offset = read_u16(record, offset + 0x14) as usize;
            let start = offset + value_offset;
            let value = record.get(start..start + value_length)?;
            if value.len() >= 66 {
                let parent = read_u64(value, 0);
                let real_size = read_u64(value, 48);
                let name_length = value[64] as usize;
                let namespace = value[65];
                if 66 + name_length * 2 <= value.len() {
                    let name = decode_utf16(&value[66..66 + name_length * 2]);
                    if !name.is_empty() && namespace != NS_DOS {
                        let rank = if namespace == NS_WIN32 || namespace == NS_POSIX {
                            0
                        } else if namespace == NS_WIN32_DOS {
                            1
                        } else {
                            2
                        };
                        if best.as_ref().is_none_or(|(current, ..)| rank < *current) {
                            best = Some((rank, parent, name));
                        }
                        if size == 0 {
                            size = real_size;
                        }
                    }
                }
            }
        } else if attr_type == ATTRIBUTE_DATA && non_resident {
            let real_size = read_u64(record, offset + 0x30);
            if size == 0 {
                size = real_size;
            }
        }

        offset += attr_len;
    }

    let (_, parent, name) = best?;
    let is_dir = flags & 0x0002 != 0;
    let hidden = is_noise(&name);
    Some(Node {
        parent,
        name,
        is_dir,
        hidden,
        size,
    })
}

/// 噪声过滤（参考 pulse ShouldSkipName / IsNtfsRootMetafileName）
fn is_noise(name: &str) -> bool {
    const SKIP: [&str; 8] = [
        ".",
        "..",
        "$Recycle.Bin",
        "System Volume Information",
        "WinSxS",
        "servicing",
        "node_modules",
        "$Extend",
    ];
    if SKIP.contains(&name) {
        return true;
    }
    // NTFS 根元文件（$MFT、$LogFile、$Bitmap 等）
    name.starts_with('$')
        && name.len() > 1
        && name.chars().nth(1).is_some_and(|c| c.is_ascii_uppercase())
}

// ---------------------------------------------------------------- 小工具

fn read_u16(data: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([data[offset], data[offset + 1]])
}

fn read_u32(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
    ])
}

fn read_u64(data: &[u8], offset: usize) -> u64 {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&data[offset..offset + 8]);
    u64::from_le_bytes(bytes)
}

fn decode_utf16(bytes: &[u8]) -> String {
    let wide: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    String::from_utf16_lossy(&wide)
}
