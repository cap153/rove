// Rove 文件管理器入口。
//
// Windows 上以 GUI 子系统构建，避免双击 exe 时弹出控制台窗口。
#![windows_subsystem = "windows"]
// config.json 的序列化用一个很大的 `json!` 字面量拼装整份配置，
// 字段一多就会撞上宏展开的默认递归深度上限（默认 128）。
#![recursion_limit = "512"]

mod app;
mod config;
mod daemon;
mod fs;
mod ops;
mod search;
mod state;
// 每个文件夹自己的视图/排序记忆（独立视图和排序开关）。
mod views;
// 系统级辅助（原生对话框 / 拖放）。
mod sys;
// 静态守卫：禁止「条件里借用 RefCell、块内再 borrow_mut」，只在测试里编进来。
#[cfg(test)]
mod refcell_guard;
// 托盘仅 Windows（Linux 宿主测试无需 GTK 依赖）。
#[cfg(windows)]
mod tray;

fn main() {
    app::run();
}
