//! 平台相关能力：默认程序打开、终端唤起等。

use std::path::Path;
use std::process::Command;

/// 用系统默认程序打开文件或文件夹
#[cfg(windows)]
pub fn shell_open(path: &Path) -> std::io::Result<()> {
    shell_execute(path, "open")
}

/// 打开系统“打开方式”对话框
#[cfg(windows)]
pub fn shell_open_as(path: &Path) -> std::io::Result<()> {
    shell_execute(path, "openas")
}

#[cfg(windows)]
fn shell_execute(path: &Path, verb: &str) -> std::io::Result<()> {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let operation = crate::wstr::wide(verb);
    let file = crate::wstr::wide(&path.display().to_string());
    let result = unsafe {
        ShellExecuteW(
            None,
            crate::wstr::pcw(&operation),
            crate::wstr::pcw(&file),
            None,
            None,
            SW_SHOWNORMAL,
        )
    };
    // ShellExecuteW 用 HINSTANCE 传回错误码，<= 32 表示失败
    if result.0 as isize > 32 {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "ShellExecuteW({verb}) failed for {}",
            path.display()
        )))
    }
}

#[cfg(not(windows))]
pub fn shell_open(path: &Path) -> std::io::Result<()> {
    Command::new("xdg-open").arg(path).spawn().map(|_| ())
}

#[cfg(not(windows))]
pub fn shell_open_as(_path: &Path) -> std::io::Result<()> {
    Err(std::io::Error::other("openas is only supported on Windows"))
}

/// 在指定目录唤起 Windows Terminal（回退 PowerShell）
#[cfg(windows)]
pub fn open_terminal(dir: &Path) -> std::io::Result<()> {
    let wt = Command::new("wt.exe")
        .arg("-d")
        .arg(dir)
        .spawn();
    if wt.is_ok() {
        return Ok(());
    }
    Command::new("powershell.exe")
        .arg("-NoExit")
        .arg("-Command")
        .arg(format!("Set-Location -LiteralPath '{}'", dir.display()))
        .spawn()
        .map(|_| ())
}

#[cfg(not(windows))]
pub fn open_terminal(dir: &Path) -> std::io::Result<()> {
    Command::new("x-terminal-emulator").arg(dir).spawn().map(|_| ())
}
