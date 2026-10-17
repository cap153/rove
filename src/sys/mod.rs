//! 系统级辅助设施（`src/sys`）。
//!
//! 目前包含：原生文件选择对话框；后续 Sprint 2 的拖放（dnd）也放在此模块内，
//! 与 Slint 事件循环隔离。

pub mod pick;
pub mod dnd;
