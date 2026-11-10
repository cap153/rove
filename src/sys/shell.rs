//! Windows 原生 Shell 设施：原生文件操作（复制/移动/删除）。

#[cfg(windows)]
mod imp {
    use std::path::{Path, PathBuf};
    use windows_sys::Win32::Foundation::HWND;
    use std::sync::atomic::{AtomicIsize, Ordering};
    use windows_sys::Win32::UI::Shell::{
        SHFileOperationW, FOF_ALLOWUNDO, FOF_NOCONFIRMMKDIR, FO_COPY, FO_DELETE, FO_MOVE,
        SHFILEOPSTRUCTW,
    };

    /// 主窗口 HWND（UI 线程写入，后台 Shell 操作线程读取）。
    /// 必须传给 SHFileOperationW：否则确认/进度对话框没有宿主窗口，
    /// 我们主窗不会被禁用——对话框里的回车会同时落回 Rove，
    /// 导致“永久删除后回车把删掉的文件夹打开”这类跨窗跳键。
    static OWNER_HWND: AtomicIsize = AtomicIsize::new(0);

    /// 登记主窗口 HWND（获取到后调用一次即可，也允许重复调用）。
    pub fn set_owner_hwnd(hwnd: isize) {
        if hwnd != 0 {
            OWNER_HWND.store(hwnd, Ordering::Relaxed);
        }
    }

    /// 取当前可用宿主窗口：优先传入值，否则用登记过的全局值。
    fn owner_hwnd(hwnd: isize) -> HWND {
        if hwnd != 0 {
            return hwnd as HWND;
        }
        OWNER_HWND.load(Ordering::Relaxed) as HWND
    }

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
            hwnd: owner_hwnd(hwnd),
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

    /// 显示 Shell 条目的「属性」对话框。
    ///
    /// 回收站条目传入的是 `SIGDN_DESKTOPABSOLUTEPARSING`（Shell parsing name），
    /// 不是磁盘上存在的文件路径：先 `SHParseDisplayName` 换成绝对 PIDL，再用
    /// `SEE_MASK_INVOKEIDLIST` 调用 `properties` 动词。这样由回收站自己的 Shell
    /// 扩展提供属性页（原位置 / 删除时间），而不是显示 `$R` 临时文件的属性。
    pub fn show_properties(parsing_name: &Path) -> Result<(), String> {
        use std::os::windows::ffi::OsStrExt;
        // COM 用项目已启用 feature 的 `windows` crate；Shell 结构体用 `windows-sys`
        // 的 POD 定义（windows-sys 未启用 Win32_System_Com）。
        use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_APARTMENTTHREADED};
        use windows_sys::Win32::UI::Shell::{
            Common::ITEMIDLIST, ShellExecuteExW, SHParseDisplayName, SEE_MASK_INVOKEIDLIST,
            SHELLEXECUTEINFOW,
        };
        use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

        unsafe {
            // UI 线程可能已初始化 COM；重复调用返回 S_FALSE / RPC_E_CHANGED_MODE，
            // 都不影响 ShellExecuteEx 按当前线程模型工作。
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok();
        }
        let mut name: Vec<u16> = parsing_name.as_os_str().encode_wide().collect();
        name.push(0);
        let verb: Vec<u16> = "properties".encode_utf16().chain([0]).collect();
        let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
        let mut attrs: u32 = 0;
        unsafe {
            let hr = SHParseDisplayName(
                name.as_ptr(),
                std::ptr::null_mut(),
                &mut pidl,
                0,
                &mut attrs,
            );
            if hr != 0 || pidl.is_null() {
                return Err(format!("无法解析 Shell 条目（HRESULT 0x{:08X}）", hr as u32));
            }
            let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
            info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
            info.fMask = SEE_MASK_INVOKEIDLIST;
            info.hwnd = owner_hwnd(0);
            info.lpVerb = verb.as_ptr();
            info.lpIDList = pidl.cast();
            info.nShow = SW_SHOWNORMAL;
            let ok = ShellExecuteExW(&mut info);
            CoTaskMemFree(Some(pidl as *const std::ffi::c_void));
            if ok == 0 {
                return Err("Shell 未能打开属性对话框".to_string());
            }
        }
        Ok(())
    }
}

#[cfg(windows)]
pub use imp::*;

#[cfg(not(windows))]
#[allow(dead_code)]
mod imp {
    use std::path::{Path, PathBuf};

    pub fn set_owner_hwnd(_hwnd: isize) {}
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
    pub fn show_properties(_parsing_name: &Path) -> Result<(), String> {
        Err("属性对话框仅在 Windows 支持".into())
    }
}

#[cfg(not(windows))]
pub use imp::*;
