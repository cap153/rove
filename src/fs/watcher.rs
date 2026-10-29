//! 当前目录树文件变更监听器（基于 Windows ReadDirectoryChangesW）。
//!
//! 监听给定目录及其整棵子树：任何深层文件的增删改都会通过 `tx` 回传其完整路径，
//! 由上层（app.rs）逆向反查出当前视图中的直接子目录并触发容量重算。

use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

#[cfg(windows)]
pub struct DirWatcher {
    handle: windows_sys::Win32::Foundation::HANDLE,
    pub path: PathBuf,
}

#[cfg(not(windows))]
pub struct DirWatcher {
    pub path: PathBuf,
}

impl DirWatcher {
    #[cfg(windows)]
    pub fn start(dir: &Path, tx: Sender<PathBuf>) -> Option<Self> {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileW, ReadDirectoryChangesW, FILE_FLAG_BACKUP_SEMANTICS, FILE_LIST_DIRECTORY,
            FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME,
            FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE, FILE_NOTIFY_INFORMATION,
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
        };

        let mut wide_path: Vec<u16> = dir.as_os_str().encode_wide().collect();
        wide_path.push(0);

        let handle = unsafe {
            CreateFileW(
                wide_path.as_ptr(),
                FILE_LIST_DIRECTORY,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                std::ptr::null_mut(),
            )
        };

        if handle == INVALID_HANDLE_VALUE {
            return None;
        }

        let root = dir.to_path_buf();
        // HANDLE 是裸指针（*mut c_void），不实现 Send；中转成 usize 以满足跨线程要求。
        let thread_handle = handle as usize;
        std::thread::Builder::new()
            .name("rove-dir-watcher".into())
            .spawn(move || {
                let h = thread_handle as windows_sys::Win32::Foundation::HANDLE;
                // 32KB 对齐缓冲区，可承载大量并发文件变动
                let mut buffer = [0u8; 32768];

                loop {
                    let mut bytes_returned = 0u32;
                    let success = unsafe {
                        ReadDirectoryChangesW(
                            h,
                            buffer.as_mut_ptr() as _,
                            buffer.len() as u32,
                            1, // bWatchSubtree = TRUE：递归监听整棵子树
                            FILE_NOTIFY_CHANGE_FILE_NAME
                                | FILE_NOTIFY_CHANGE_DIR_NAME
                                | FILE_NOTIFY_CHANGE_SIZE
                                | FILE_NOTIFY_CHANGE_LAST_WRITE,
                            &mut bytes_returned,
                            std::ptr::null_mut(),
                            None,
                        )
                    };

                    // 句柄被外部 CloseHandle 时 success 为 0，正常退出线程
                    if success == 0 || bytes_returned == 0 {
                        break;
                    }

                    // 解析变长的 FILE_NOTIFY_INFORMATION 结构体链表
                    let mut offset = 0usize;
                    loop {
                        let info = unsafe {
                            &*buffer
                                .as_ptr()
                                .add(offset)
                                .cast::<FILE_NOTIFY_INFORMATION>()
                        };
                        let name_len = (info.FileNameLength / 2) as usize;
                        let slice =
                            unsafe { std::slice::from_raw_parts(info.FileName.as_ptr(), name_len) };
                        let rel_name = String::from_utf16_lossy(slice);
                        let changed_path = root.join(rel_name);

                        if tx.send(changed_path).is_err() {
                            return;
                        }

                        if info.NextEntryOffset == 0 {
                            break;
                        }
                        offset += info.NextEntryOffset as usize;
                        if offset >= buffer.len() {
                            break;
                        }
                    }
                }
            })
            .ok();

        Some(Self {
            handle,
            path: dir.to_path_buf(),
        })
    }

    #[cfg(not(windows))]
    pub fn start(_dir: &Path, _tx: Sender<PathBuf>) -> Option<Self> {
        None
    }
}

#[cfg(windows)]
impl Drop for DirWatcher {
    fn drop(&mut self) {
        let handle = self.handle as usize;
        // 核心修复：严禁在 UI 线程同步调用 CloseHandle！
        // 阻塞中的 ReadDirectoryChangesW 在被 CloseHandle 取消时，内核需撤销挂起的 IRP，
        // 会阻塞调用线程 2~5 秒。移至后台线程异步销毁，UI 线程 0ms 瞬间返回。
        std::thread::spawn(move || unsafe {
            windows_sys::Win32::Foundation::CloseHandle(handle as _);
        });
    }
}
