//! 文件操作子系统：剪贴板互通、任务化复制/移动/删除、快捷方式生成、
//! 终端唤起。

pub mod actions;
pub mod clipboard;
pub mod jobs;
pub mod keymap;
pub mod shortcut;
#[cfg(windows)]
pub mod thumbs;

/// 在指定目录打开终端（优先 Windows Terminal，回落 PowerShell）。
pub fn open_terminal(dir: &std::path::Path) -> Result<(), String> {
    let wt = which_in_path("wt.exe");
    if let Some(wt) = wt {
        return std::process::Command::new(wt)
            .arg("-d")
            .arg(dir)
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("启动 wt.exe 失败: {e}"));
    }
    std::process::Command::new("powershell.exe")
        .arg("-NoExit")
        .arg("-Command")
        .arg(format!("Set-Location -LiteralPath '{}'", dir.display()))
        .current_dir(dir)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("启动 powershell.exe 失败: {e}"))
}

fn which_in_path(name: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}
