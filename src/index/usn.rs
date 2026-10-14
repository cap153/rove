//! USN Journal 监听（参考 pulse `usn_*` / `index_engine.cpp`）。
//!
//! * 启动时 `FSCTL_QUERY_USN_JOURNAL`，失败则 `FSCTL_CREATE_USN_JOURNAL`；
//! * `FSCTL_READ_USN_JOURNAL` 带 `BytesToWaitFor=1` 阻塞等待新记录（事件循环，非轮询）；
//! * 按 Reason 应用变更到内存树：新建 / 删除 / 重命名（含移动）；
//! * journal ID 变化或 `ERROR_JOURNAL_ENTRY_DELETED` 时重置游标。

use std::io;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Ioctl::{
    CREATE_USN_JOURNAL_DATA, FSCTL_CREATE_USN_JOURNAL, FSCTL_QUERY_USN_JOURNAL,
    FSCTL_READ_USN_JOURNAL, READ_USN_JOURNAL_DATA_V0, USN_JOURNAL_DATA_V0,
};
use windows::Win32::System::IO::DeviceIoControl;

use super::tree::{Node, Tree};

const USN_REASON_FILE_CREATE: u32 = 0x0000_0100;
const USN_REASON_FILE_DELETE: u32 = 0x0000_0200;
const USN_REASON_RENAME_NEW_NAME: u32 = 0x0000_2000;
const USN_REASON_RENAME_OLD_NAME: u32 = 0x0000_1000;
const USN_REASON_CLOSE: u32 = 0x8000_0000;

/// USN Journal 游标
#[derive(Debug, Clone, Copy)]
pub struct Journal {
    pub journal_id: u64,
    pub next_usn: i64,
}

/// 查询（必要时创建）USN Journal
pub fn open_journal(handle: HANDLE) -> io::Result<Journal> {
    if let Ok(journal) = query_journal(handle) {
        return Ok(journal);
    }
    let create = CREATE_USN_JOURNAL_DATA {
        MaximumSize: 32 * 1024 * 1024,
        AllocationDelta: 4 * 1024 * 1024,
    };
    let mut returned: u32 = 0;
    unsafe {
        DeviceIoControl(
            handle,
            FSCTL_CREATE_USN_JOURNAL,
            Some(&create as *const _ as *const core::ffi::c_void),
            std::mem::size_of::<CREATE_USN_JOURNAL_DATA>() as u32,
            None,
            0,
            Some(&mut returned as *mut u32),
            None,
        )
    }
    .map_err(|e| io::Error::other(e.to_string()))?;
    query_journal(handle)
}

fn query_journal(handle: HANDLE) -> io::Result<Journal> {
    let mut data: USN_JOURNAL_DATA_V0 = unsafe { std::mem::zeroed() };
    let mut returned: u32 = 0;
    unsafe {
        DeviceIoControl(
            handle,
            FSCTL_QUERY_USN_JOURNAL,
            None,
            0,
            Some(&mut data as *mut _ as *mut core::ffi::c_void),
            std::mem::size_of::<USN_JOURNAL_DATA_V0>() as u32,
            Some(&mut returned as *mut u32),
            None,
        )
    }
    .map_err(|e| io::Error::other(e.to_string()))?;
    Ok(Journal {
        journal_id: data.UsnJournalID,
        next_usn: data.NextUsn,
    })
}

/// 读取一批 USN 记录并应用到内存树；返回应用的记录数
pub fn read_and_apply(handle: HANDLE, journal: &mut Journal, tree: &mut Tree) -> io::Result<usize> {
    let request = READ_USN_JOURNAL_DATA_V0 {
        StartUsn: journal.next_usn,
        ReasonMask: 0xFFFF_FFFF,
        ReturnOnlyOnClose: 0,
        Timeout: 0,
        // 至少等到 1 条记录再返回（阻塞式事件循环）
        BytesToWaitFor: 1,
        UsnJournalID: journal.journal_id,
    };

    let mut buffer = vec![0u8; 64 * 1024];
    let mut returned: u32 = 0;
    let result = unsafe {
        DeviceIoControl(
            handle,
            FSCTL_READ_USN_JOURNAL,
            Some(&request as *const _ as *const core::ffi::c_void),
            std::mem::size_of::<READ_USN_JOURNAL_DATA_V0>() as u32,
            Some(buffer.as_mut_ptr() as *mut core::ffi::c_void),
            buffer.len() as u32,
            Some(&mut returned as *mut u32),
            None,
        )
    };

    if let Err(error) = result {
        let code = error.code().0 as u32;
        // 记录被清除 / journal 失效：重置游标，交由上层重建
        if code == 0x80070057 || code == 0xC0000000 + 982 || code == 0x800705AA {
            journal.journal_id = 0;
            journal.next_usn = 0;
            return Err(io::Error::other("usn journal invalidated"));
        }
        return Err(io::Error::other(error.to_string()));
    }

    // 输出结构前 8 字节是新的 StartUsn
    if returned >= 8 {
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&buffer[..8]);
        journal.next_usn = i64::from_le_bytes(bytes);
    }

    let mut applied = 0usize;
    let mut offset = 8usize;
    while offset + 0x3C <= returned as usize {
        let record_length = u32::from_le_bytes([
            buffer[offset],
            buffer[offset + 1],
            buffer[offset + 2],
            buffer[offset + 3],
        ]) as usize;
        if record_length < 0x3C || offset + record_length > returned as usize {
            break;
        }
        if apply_record(&buffer[offset..offset + record_length], tree) {
            applied += 1;
        }
        offset += record_length;
    }
    Ok(applied)
}

/// 应用单条 USN 记录
fn apply_record(record: &[u8], tree: &mut Tree) -> bool {
    let frn = read_u64(record, 0x08);
    let parent = read_u64(record, 0x10);
    let reason = read_u32(record, 0x28);
    let name_length = read_u16(record, 0x38) as usize;
    let name_offset = read_u16(record, 0x3A) as usize;
    let attributes = read_u32(record, 0x34);
    let is_dir = attributes & 0x10 != 0;

    let name = if name_offset + name_length * 2 <= record.len() {
        decode_utf16(&record[name_offset..name_offset + name_length * 2])
    } else {
        String::new()
    };

    if reason & USN_REASON_FILE_DELETE != 0 {
        tree.remove(frn);
        return true;
    }
    if reason & USN_REASON_RENAME_NEW_NAME != 0 || reason & USN_REASON_FILE_CREATE != 0 {
        if name.is_empty() {
            return false;
        }
        // RENAME_OLD_NAME 记录只带旧名，忽略（新名记录已包含最终状态）
        if reason & USN_REASON_RENAME_OLD_NAME != 0 && reason & USN_REASON_RENAME_NEW_NAME == 0 {
            return false;
        }
        if let Some(existing) = tree.get(frn) {
            if existing.parent != parent || existing.name != name {
                tree.rename(frn, parent, name);
            }
        } else {
            tree.upsert(
                frn,
                Node {
                    parent,
                    name,
                    is_dir,
                    hidden: false,
                    size: 0,
                },
            );
        }
        return true;
    }
    false
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

#[allow(dead_code)]
fn _keep_reason(_: u32) {
    let _ = USN_REASON_CLOSE;
}
