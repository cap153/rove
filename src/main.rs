// Rove 文件管理器入口。
//
// Windows 上以 GUI 子系统构建，避免双击 exe 时弹出控制台窗口。
#![windows_subsystem = "windows"]

mod app;
mod config;
mod daemon;
mod fs;
mod ops;
mod search;
mod state;
// 托盘仅 Windows（Linux 宿主测试无需 GTK 依赖）。
#[cfg(windows)]
mod tray;

fn main() {
    app::run();
}
