//! Windows 原生 Shell 设施：属性对话框、原生文件操作（复制/移动/删除）。

#[cfg(windows)]
mod imp {
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use windows_sys::core::w;
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::UI::Shell::{
        ShellExecuteExW, SHFileOperationW, FOF_ALLOWUNDO, FOF_NOCONFIRMMKDIR, FO_COPY, FO_DELETE,
        FO_MOVE, SEE_MASK_INVOKEIDLIST, SHELLEXECUTEINFOW, SHFILEOPSTRUCTW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOW;

    /// 将路径列表转换为 Windows Shell 要求的多字符串双空结尾 (double null-terminated) 宽字符流。
    fn to_double_null_wide(paths: &[PathBuf]) -> Vec<u16> {
        let mut out = Vec::new();
        for p in paths {
            let s = p.to_string_lossy().replace('/', "\\");
            out.extend(s.encode_utf16());
            out.push(0); // 单项结尾 null
        }
        out.push(0); // 整体列表结尾 double-null
        out
    }

    /// 弹出 Windows 原生文件/文件夹属性对话框。
    pub fn show_properties(paths: &[PathBuf], hwnd: isize) {
        if paths.is_empty() {
            return;
        }
        for path in paths {
            let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
            wide.push(0);

            let mut sei: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
            sei.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
            sei.fMask = SEE_MASK_INVOKEIDLIST;
            sei.hwnd = hwnd as HWND;
            sei.lpVerb = w!("properties");
            sei.lpFile = wide.as_ptr();
            sei.nShow = SW_SHOW as i32;

            unsafe {
                ShellExecuteExW(&mut sei);
            }
        }
    }

    /// 调用 Windows 原生 Shell 执行文件操作（弹窗进度条、同名覆盖确认、UAC 提权、支持 Ctrl+Z）。
    fn run_shell_op(
        op: u32,
        from: &[PathBuf],
        to: Option<&Path>,
        flags: u32,
        hwnd: isize,
    ) -> Result<bool, String> {
        if from.is_empty() {
            return Ok(true);
        }
        let from_wide = to_double_null_wide(from);
        let to_wide = to.map(|p| to_double_null_wide(&[p.to_path_buf()]));

        let pto: *const u16 = match to_wide {
            Some(ref v) => v.as_ptr(),
            None => std::ptr::null(),
        };

        let mut sh_op = SHFILEOPSTRUCTW {
            hwnd: hwnd as HWND,
            wFunc: op,
            pFrom: from_wide.as_ptr(),
            pTo: pto,
            fFlags: flags as u16,
            fAnyOperationsAborted: 0,
            hNameMappings: std::ptr::null_mut(),
            lpszProgressTitle: std::ptr::null(),
        };

        let ret = unsafe { SHFileOperationW(&mut sh_op) };
        if ret != 0 {
            return Err(format!("Shell 操作失败，错误代码: {ret}"));
        }
        // 返回是否被用户中途取消
        Ok(sh_op.fAnyOperationsAborted != 0)
    }

    pub fn shell_copy(from: &[PathBuf], dest_dir: &Path, hwnd: isize) -> Result<bool, String> {
        run_shell_op(
            FO_COPY,
            from,
            Some(dest_dir),
            FOF_ALLOWUNDO | FOF_NOCONFIRMMKDIR,
            hwnd,
        )
    }

    pub fn shell_move(from: &[PathBuf], dest_dir: &Path, hwnd: isize) -> Result<bool, String> {
        run_shell_op(
            FO_MOVE,
            from,
            Some(dest_dir),
            FOF_ALLOWUNDO | FOF_NOCONFIRMMKDIR,
            hwnd,
        )
    }

    pub fn shell_delete(from: &[PathBuf], to_trash: bool, hwnd: isize) -> Result<bool, String> {
        let mut flags = 0u32;
        if to_trash {
            flags |= FOF_ALLOWUNDO;
        }
        run_shell_op(FO_DELETE, from, None, flags, hwnd)
    }
}

#[cfg(windows)]
pub use imp::*;

#[cfg(not(windows))]
#[allow(dead_code)]
mod imp {
    use std::path::{Path, PathBuf};

    pub fn show_properties(_paths: &[PathBuf], _hwnd: isize) {}
    pub fn shell_copy(
        _from: &[PathBuf],
        _dest_dir: &Path,
        _hwnd: isize,
    ) -> Result<bool, String> {
        Err("原生 Shell 操作仅在 Windows 支持".into())
    }
    pub fn shell_move(
        _from: &[PathBuf],
        _dest_dir: &Path,
        _hwnd: isize,
    ) -> Result<bool, String> {
        Err("原生 Shell 操作仅在 Windows 支持".into())
    }
    pub fn shell_delete(
        _from: &[PathBuf],
        _to_trash: bool,
        _hwnd: isize,
    ) -> Result<bool, String> {
        Err("原生 Shell 操作仅在 Windows 支持".into())
    }
}
