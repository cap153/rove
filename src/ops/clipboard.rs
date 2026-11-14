//! Windows 系统剪贴板：CF_HDROP 文件复制/剪切/粘贴。
//!
//! 方案移植自 sigma-file-manager（`src-tauri/src/system_clipboard`）：
//! 直接构造 DROPFILES + 宽字符路径表写入 CF_HDROP，并附带
//! Preferred DropEffect 标记复制/移动语义。粘贴时反向解析。
//! 这保证与资源管理器、以及任何读 CF_HDROP 的程序互通。

/// 剪贴板中的文件内容。
#[derive(Clone, Debug, Default)]
pub struct ClipboardFiles {
    pub paths: Vec<std::path::PathBuf>,
    /// "copy" 或 "move"。
    pub operation: String,
}

#[cfg(windows)]
mod imp {
    use std::iter::once;
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Path, PathBuf};

    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable,
        OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GHND};
    use windows_sys::Win32::System::Ole::{CF_HDROP, CF_UNICODETEXT, DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE};
    use windows_sys::Win32::UI::Shell::{DragQueryFileW, DROPFILES, HDROP};
    use windows_sys::core::w;

    use super::ClipboardFiles;

    /// 进程级剪贴板互斥锁，避免与剪贴板查看器等并发竞争。
    static CLIPBOARD_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());
    static OLE_INITIALIZED: std::sync::Once = std::sync::Once::new();

    fn ensure_ole() {
        use windows_sys::Win32::System::Ole::OleInitialize;
        OLE_INITIALIZED.call_once(|| unsafe {
            let _ = OleInitialize(std::ptr::null_mut());
        });
    }

    /// OpenClipboard 带重试：其他窗口短暂持有时不会立即失败。
    unsafe fn open_clipboard() -> Result<(), String> {
        for attempt in 0..8 {
            if unsafe { OpenClipboard(std::ptr::null_mut()) } != 0 {
                return Ok(());
            }
            if attempt < 7 {
                std::thread::sleep(std::time::Duration::from_millis(15));
            }
        }
        Err("OpenClipboard failed".to_string())
    }

    unsafe fn set_clipboard_bytes(format: u32, bytes: &[u8]) -> Result<(), String> {
        let handle = unsafe { GlobalAlloc(GHND, bytes.len()) };
        if handle.is_null() {
            return Err("GlobalAlloc failed".to_string());
        }
        let ptr = unsafe { GlobalLock(handle) };
        if ptr.is_null() {
            return Err("GlobalLock failed".to_string());
        }
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr as *mut u8, bytes.len());
            let _ = GlobalUnlock(handle);
            if SetClipboardData(format, handle).is_null() {
                return Err("SetClipboardData failed".to_string());
            }
        }
        Ok(())
    }

    /// `\\?\UNC\` / `\\?\` 前缀 → 常规路径；分隔符统一为 `\`。
    fn shell_path(path: &Path) -> PathBuf {
        let mut s = path.to_string_lossy().into_owned();
        if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
            s = format!(r"\\{rest}");
        } else if let Some(rest) = s.strip_prefix(r"\\?\") {
            s = rest.to_string();
        }
        s = s.replace('/', "\\");
        PathBuf::from(s)
    }

    pub fn set_files(paths: &[PathBuf], is_move: bool) -> Result<(), String> {
        if paths.is_empty() {
            return Ok(());
        }
        let _guard = CLIPBOARD_MUTEX
            .lock()
            .map_err(|_| "clipboard mutex poisoned".to_string())?;
        ensure_ole();

        let hdrop_paths: Vec<PathBuf> = paths
            .iter()
            .map(|p| {
                std::fs::canonicalize(p)
                    .map(|c| shell_path(&c))
                    .unwrap_or_else(|_| p.clone())
            })
            .collect();

        unsafe {
            open_clipboard()?;
            let result = (|| {
                if EmptyClipboard() == 0 {
                    return Err("EmptyClipboard failed".to_string());
                }

                let header_size = std::mem::size_of::<DROPFILES>();
                let mut wide: Vec<u16> = Vec::new();
                for path in &hdrop_paths {
                    let encoded: Vec<u16> = path.as_os_str().encode_wide().chain(once(0)).collect();
                    wide.extend(encoded);
                }
                wide.push(0);

                let allocation = header_size + wide.len() * 2;
                let handle = GlobalAlloc(GHND, allocation);
                if handle.is_null() {
                    return Err("GlobalAlloc failed".to_string());
                }
                let ptr = GlobalLock(handle);
                if ptr.is_null() {
                    return Err("GlobalLock failed".to_string());
                }
                let dropfiles = DROPFILES {
                    pFiles: header_size as u32,
                    pt: POINT { x: 0, y: 0 },
                    fNC: 0,
                    fWide: 1,
                };
                *(ptr as *mut DROPFILES) = dropfiles;
                std::ptr::copy_nonoverlapping(
                    wide.as_ptr(),
                    ptr.add(header_size) as *mut u16,
                    wide.len(),
                );
                let _ = GlobalUnlock(handle);

                if SetClipboardData(CF_HDROP as u32, handle).is_null() {
                    return Err("SetClipboardData CF_HDROP failed".to_string());
                }

                // 首路径的 FileNameW：某些旧程序会读该格式。
                if let Some(first) = hdrop_paths.first() {
                    let name_w: Vec<u16> = first.as_os_str().encode_wide().chain(once(0)).collect();
                    let fmt = RegisterClipboardFormatW(w!("FileNameW"));
                    let bytes =
                        std::slice::from_raw_parts(name_w.as_ptr() as *const u8, name_w.len() * 2);
                    set_clipboard_bytes(fmt, bytes)?;
                }

                let effect_fmt = RegisterClipboardFormatW(w!("Preferred DropEffect"));
                let effect: u32 = if is_move {
                    DROPEFFECT_MOVE
                } else {
                    DROPEFFECT_COPY | DROPEFFECT_LINK
                };
                set_clipboard_bytes(effect_fmt, &effect.to_ne_bytes())?;
                Ok(())
            })();
            let _ = CloseClipboard();
            result
        }
    }

    /// 把纯文本写进剪贴板（CF_UNICODETEXT）。
    ///
    /// 会被 EmptyClipboard 清掉原有的 CF_HDROP：这是预期的，因为 Rove 只会在
    /// 用户显式点「复制路径」时调它。不附结尾换行（否则粘进对话框会多一行）。
    pub fn set_text(text: &str) -> Result<(), String> {
        let _guard = CLIPBOARD_MUTEX
            .lock()
            .map_err(|_| "clipboard mutex poisoned".to_string())?;
        ensure_ole();
        let mut wide: Vec<u16> = text.encode_utf16().collect();
        wide.push(0);
        let bytes = unsafe {
            std::slice::from_raw_parts(wide.as_ptr() as *const u8, wide.len() * 2)
        };
        unsafe {
            open_clipboard()?;
            let result = (|| {
                if EmptyClipboard() == 0 {
                    return Err("EmptyClipboard failed".to_string());
                }
                set_clipboard_bytes(CF_UNICODETEXT as u32, bytes)
            })();
            let _ = CloseClipboard();
            result
        }
    }

    pub fn clear() -> Result<(), String> {
        let _guard = CLIPBOARD_MUTEX
            .lock()
            .map_err(|_| "clipboard mutex poisoned".to_string())?;
        ensure_ole();
        unsafe {
            open_clipboard()?;
            let result = if EmptyClipboard() == 0 {
                Err("EmptyClipboard failed".to_string())
            } else {
                Ok(())
            };
            let _ = CloseClipboard();
            result
        }
    }

    pub fn read_files() -> ClipboardFiles {
        let _guard = match CLIPBOARD_MUTEX.lock() {
            Ok(g) => g,
            Err(_) => return ClipboardFiles::default(),
        };
        ensure_ole();
        unsafe {
            if open_clipboard().is_err() {
                return ClipboardFiles::default();
            }
            let result = (|| {
                if IsClipboardFormatAvailable(CF_HDROP as u32) == 0 {
                    return ClipboardFiles::default();
                }
                let handle = GetClipboardData(CF_HDROP as u32);
                if handle.is_null() {
                    return ClipboardFiles::default();
                }
                let hdrop = handle as HDROP;
                let count = DragQueryFileW(hdrop, 0xffff_ffff, std::ptr::null_mut(), 0);
                let mut paths = Vec::with_capacity(count as usize);
                for i in 0..count {
                    let needed = DragQueryFileW(hdrop, i, std::ptr::null_mut(), 0);
                    if needed == 0 {
                        continue;
                    }
                    let mut buf = vec![0u16; needed as usize + 1];
                    let copied = DragQueryFileW(hdrop, i, buf.as_mut_ptr(), buf.len() as u32);
                    if copied > 0 {
                        buf.truncate(copied as usize);
                        paths.push(PathBuf::from(String::from_utf16_lossy(&buf)));
                    }
                }
                // Preferred DropEffect 判定复制/移动。
                let mut operation = "copy".to_string();
                let effect_fmt = RegisterClipboardFormatW(w!("Preferred DropEffect"));
                if IsClipboardFormatAvailable(effect_fmt) != 0 {
                    let eh = GetClipboardData(effect_fmt);
                    if !eh.is_null() {
                        let ptr = GlobalLock(eh);
                        if !ptr.is_null() {
                            let effect = *(ptr as *const u32);
                            if effect & DROPEFFECT_MOVE != 0 {
                                operation = "move".to_string();
                            }
                            let _ = GlobalUnlock(eh);
                        }
                    }
                }
                ClipboardFiles { paths, operation }
            })();
            let _ = CloseClipboard();
            result
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use std::path::PathBuf;

    use super::ClipboardFiles;

    pub fn set_files(_paths: &[PathBuf], _is_move: bool) -> Result<(), String> {
        Err("clipboard is only implemented on Windows".to_string())
    }

    pub fn set_text(_text: &str) -> Result<(), String> {
        Err("clipboard is only implemented on Windows".to_string())
    }

    pub fn clear() -> Result<(), String> {
        Err("clipboard is only implemented on Windows".to_string())
    }

    pub fn read_files() -> ClipboardFiles {
        ClipboardFiles::default()
    }
}

/// 写入文件剪贴板（复制/剪切）。
pub fn set_files(paths: &[std::path::PathBuf], is_move: bool) -> Result<(), String> {
    imp::set_files(paths, is_move)
}

/// 把选中项的完整路径（含文件名）拼成多行文本：一项一行，不带引号、不带结尾换行。
pub fn paths_text(paths: &[std::path::PathBuf]) -> String {
    paths
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("\r\n")
}

/// 把纯文本写进剪贴板（复制路径）。
pub fn set_text(text: &str) -> Result<(), String> {
    imp::set_text(text)
}

/// 清空剪贴板（预留给「剪切后取消」等场景）。
#[allow(dead_code)]
pub fn clear() -> Result<(), String> {
    imp::clear()
}

/// 读取文件剪贴板。
pub fn read_files() -> ClipboardFiles {
    imp::read_files()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_text_is_one_path_per_line_without_quotes() {
        let paths = vec![
            std::path::PathBuf::from(r"C:\Users\x\a b.txt"),
            std::path::PathBuf::from(r"D:\重\素晴"),
        ];
        assert_eq!(paths_text(&paths), "C:\\Users\\x\\a b.txt\r\nD:\\重\\素晴");
    }

    #[test]
    fn paths_text_single_has_no_trailing_newline() {
        let paths = vec![std::path::PathBuf::from(r"D:\重")];
        assert_eq!(paths_text(&paths), r"D:\重");
    }
}
