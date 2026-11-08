//! 原生右键菜单（Shell `IContextMenu` + 追加应用命令项）。
//!
//! 为什么要有这个模块：自己画菜单要自己处理命中、子菜单、键盘、坐标、DPI、
//! 深色主题下的系统扩展图标…… 一处偏差就是一堆 bug。这里直接让 Windows
//! 显示它自己的菜单：`打开方式` `发送到` `新建` `属性` 以及所有第三方扩展项
//! （7-Zip / 网盘 / BitLocker …）全部由 Shell 提供，行为与资源管理器一致。
//!
//! 线程模型（关键）：
//!   * `TrackPopupMenuEx` 是阻塞式 API，内部会跑**嵌套消息循环**。如果放在
//!     Slint 的 UI 线程上，嵌套循环里派发出来的消息会重入 Slint 回调，
//!     而回调里再 `borrow_mut()` App 就会 panic。所以菜单跑在**专用 STA 线程**。
//!   * 该线程自己创建一个隐藏的宿主窗口，`TrackPopupMenuEx` 的 hWnd 必须是
//!     调用线程的窗口。菜单的四条消息（`WM_INITMENUPOPUP` / `WM_DRAWITEM` /
//!     `WM_MEASUREITEM` / `WM_MENUCHAR`）也必须由这个线程的窗口过程转发给
//!     `IContextMenu2/3`，否则「新建 / 发送到 / 打开方式」这些懒加载子菜单
//!     是**空的**。
//!   * UI 线程只投递请求、接收结果，全程不阻塞。
//!
//! id 约定（与 Shell 一致）：`1..=0x6FFF` 归 Shell 命令，`0x7000+` 归应用命令。

use std::path::PathBuf;
use std::sync::Arc;

/// 菜单里的一项（应用自己追加的命令）。
#[derive(Clone, Debug)]
pub enum MenuNode {
    Separator,
    Item {
        /// 选中后回传给 UI 线程的命令标识（沿用 `run_menu_action` 的字符串 id）。
        key: String,
        label: String,
        enabled: bool,
        checked: bool,
    },
    Submenu {
        label: String,
        enabled: bool,
        children: Vec<MenuNode>,
    },
}

/// 菜单目标：选中项 / 当前目录空白处 / 纯应用菜单（不挂 Shell 项）。
#[derive(Clone, Debug)]
pub enum Target {
    Items(Vec<PathBuf>),
    Background(PathBuf),
    AppOnly,
}

/// 菜单结果，跨线程回投给 UI 线程。
#[derive(Clone, Debug)]
pub enum MenuAction {
    /// 用户点了应用自己的命令。
    App(String),
    /// 用户点了 Shell 的「重命名」：改走应用内联重命名。
    Rename,
    /// 用户点了 Shell 的「打开 / 在新窗口中打开」且目标是单个文件夹：应用内导航。
    Navigate(PathBuf),
    /// Shell 命令开始执行（期间要抑制回车穿透，复用 `shell_op_running`）。
    ShellOpBegin,
    /// Shell 命令结束。
    ShellOpEnd,
    /// Shell 操作可能改了磁盘：让 UI 刷新当前目录。
    Refresh,
}

pub type Sink = Arc<dyn Fn(MenuAction) + Send + Sync>;

/// 弹出原生菜单。`owner` 是主窗口 HWND（用于菜单关闭后归还前台权）；
/// `hidden_verbs` 是要从 Shell 菜单里摘掉的命令（canonical verb，如 `copy` / `openas`），
/// 由设置页的开关决定。菜单本身由内部专用线程的零面积窗口持有。
/// 返回 false 表示菜单线程不可用（调用方记日志并放弃本次菜单）。
#[cfg(windows)]
pub(crate) use imp::open;

#[cfg(not(windows))]
pub fn open(
    _: Target,
    _: Vec<MenuNode>,
    _: Vec<String>,
    _: bool,
    _: isize,
    _: Sink,
) -> bool {
    false
}

#[cfg(windows)]
mod imp;
