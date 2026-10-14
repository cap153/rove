//! 快捷方式（.lnk）创建：IShellLinkW + IPersistFile。
//!
//! 仅 Windows 支持；其它平台 `create` 返回错误（`link_name_for` 仍可用）。

use std::io;
use std::path::Path;

/// 在 `link_path` 创建指向 `target` 的快捷方式
#[cfg(windows)]
pub fn create(target: &Path, link_path: &Path, description: &str) -> io::Result<()> {
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
        IPersistFile,
    };
    use windows::Win32::UI::Shell::IShellLinkW;
    use windows::core::{GUID, Interface};

    use crate::wstr::{pcw, wide};

    /// CLSID_ShellLink
    const CLSID_SHELL_LINK: GUID = GUID::from_u128(0x0002_1401_0000_0000_C000_0000_0000_0046);

    if let Some(parent) = link_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    unsafe {
        // 线程可能已以其它模型初始化，失败可忽略
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

        let link: IShellLinkW = CoCreateInstance(&CLSID_SHELL_LINK, None, CLSCTX_INPROC_SERVER)
            .map_err(|e| io::Error::other(e.to_string()))?;

        let target_wide = wide(&target.display().to_string());
        link.SetPath(pcw(&target_wide))
            .map_err(|e| io::Error::other(e.to_string()))?;

        if let Some(workdir) = target.parent() {
            let workdir_wide = wide(&workdir.display().to_string());
            let _ = link.SetWorkingDirectory(pcw(&workdir_wide));
        }

        if !description.is_empty() {
            let description_wide = wide(description);
            let _ = link.SetDescription(pcw(&description_wide));
        }

        let file: IPersistFile = link
            .cast()
            .map_err(|e| io::Error::other(e.to_string()))?;
        let link_wide = wide(&link_path.display().to_string());
        file.Save(pcw(&link_wide), true)
            .map_err(|e| io::Error::other(e.to_string()))?;
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn create(_target: &Path, _link_path: &Path, _description: &str) -> io::Result<()> {
    Err(io::Error::other("shortcuts are only supported on Windows"))
}

/// 快捷方式文件名：`<名称>.lnk`
pub fn link_name_for(target: &Path) -> String {
    let stem = target
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "shortcut".to_string());
    format!("{stem}.lnk")
}
