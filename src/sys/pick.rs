//! 原生文件选择对话框（Win32 `IFileDialog`），用于「浏览…」选择可执行程序。

/// 弹出标准打开文件对话框，返回选中的绝对路径；用户取消或失败时返回 None。
///
/// 注意：本函数会开启嵌套消息循环，调用方应确保不在高频路径中调用。
#[cfg(windows)]
pub fn pick_program_path() -> Option<String> {
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};

    unsafe {
        // COM 可能已被（托盘 / Shell 扩展）初始化；此处只需保证本线程可用。
        let uninit = CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok();
        let out = run_dialog();
        if uninit {
            CoUninitialize();
        }
        out
    }
}

#[cfg(windows)]
unsafe fn run_dialog() -> Option<String> {
    use windows::core::PCWSTR;
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};
    use windows::Win32::UI::Shell::{Common::COMDLG_FILTERSPEC, FileOpenDialog, IFileDialog};
    use windows::Win32::UI::Shell::{FOS_FILEMUSTEXIST, FOS_PATHMUSTEXIST, SIGDN_FILESYSPATH};

    let dialog: IFileDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_ALL).ok()?;

    let name: Vec<u16> = "程序\0".encode_utf16().collect();
    let spec: Vec<u16> = "*.exe;*.bat;*.cmd;*.com\0".encode_utf16().collect();
    let types = [COMDLG_FILTERSPEC {
        pszName: PCWSTR(name.as_ptr()),
        pszSpec: PCWSTR(spec.as_ptr()),
    }];
    let _ = dialog.SetFileTypes(&types);

    let ext: Vec<u16> = "exe\0".encode_utf16().collect();
    let _ = dialog.SetDefaultExtension(PCWSTR(ext.as_ptr()));
    let _ = dialog.SetOptions(FOS_FILEMUSTEXIST | FOS_PATHMUSTEXIST);

    // 父窗口省略（HWND 0）：对话框独立于主窗口，避免嵌套模态与 Slint 事件循环冲突。
    if dialog.Show(None).is_err() {
        return None;
    }

    let item = dialog.GetResult().ok()?;
    let path = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
    let s = path.to_string().ok()?;
    Some(s)
}

/// 非 Windows 平台（交叉编译期）的空实现。
#[cfg(not(windows))]
pub fn pick_program_path() -> Option<String> {
    None
}
