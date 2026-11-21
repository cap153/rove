//! 系统级辅助设施（`src/sys`）。
//!
//! 目前包含：原生文件选择对话框；后续 Sprint 2 的拖放（dnd）也放在此模块内，
//! 与 Slint 事件循环隔离。

pub mod pick;
pub mod dnd;
pub mod shell;
pub mod menu;
pub mod dialog_jump;
// 接管资源管理器（注册表接管文件夹/磁盘/此电脑的默认打开方式）。
pub mod integration;
// Win+E 接管：低级键盘钩子（注册表那一层在 integration 里）。
pub mod win_e;
// 开机自启动（登录时后台常驻，写 HKCU 的 Run 键）。
pub mod autostart;
// 把窗格登记成 Shell 窗口：浏览器/下载器的「打开文件所在位置」会直接选中条目。
pub mod shell_windows;
