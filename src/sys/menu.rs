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

/// 背景菜单取自视图对象时，会多出 Explorer 视图自己的一套菜单项。
/// Rove 的列表不是原生视图，这些项点下去无效，而 Rove 自己已经有对应的
/// 「视图 / 排序 / 粘贴」菜单，所以固定剔除。
/// 与用户关键词同一个语法（`!` = 精确相等），不进入设置页。
/// 只能写带加速键的系统原文：根弹出菜单也会再过滤一次，那时 Rove 自己的
/// 菜单项已经在里面了（`排序方式` 正是它自己的子菜单标题）。
pub(crate) const VIEW_LEVEL_LABELS: &[&str] = &[
    "!查看(V)",
    "!View",
    "!排序方式(O)",
    "!Sort by",
    "!分组依据(P)",
    "!Group by",
    "!刷新(E)",
    "!Refresh",
    "!粘贴(P)",
    "!Paste",
    "!粘贴快捷方式(S)",
    "!Paste shortcut",
    "!撤消删除(U)",
    "!撤消(U)",
    "!Undo Delete",
    "!Undo",
    "!在终端中打开",
    "!Open in Terminal",
    "!自定义文件夹(F)…",
    "!Customize this folder…",
];

/// 菜单裁剪选项。
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// 要隐藏的菜单项关键词：菜单文字**包含**它就隐藏；`!` 开头表示**精确相等**。
    /// 由设置页「隐藏的系统菜单项」决定（含默认藏掉的系统复制/剪切/粘贴/刷新）。
    pub hide_keywords: Vec<String>,
    /// Shift + 右键 = 扩展动词（「以管理员身份运行」这类）。
    pub extended: bool,
}

/// 弹出原生菜单。`owner` 是主窗口 HWND（用于菜单关闭后归还前台权）；
/// 菜单本身由内部专用线程的零面积窗口持有。
/// 返回 false 表示菜单线程不可用（调用方记日志并放弃本次菜单）。
#[cfg(windows)]
pub(crate) use imp::open;

#[cfg(not(windows))]
pub fn open(_: Target, _: Vec<MenuNode>, _: Options, _: isize, _: Sink) -> bool {
    false
}

/// 菜单项文字归一化：去掉助记符 `&`、小写化、丢掉 `\t` 之后的热键说明、
/// 全角标点转半角、去掉尾部 `.` / `…`。两边都走它，才能跨语言 / 跨输入法比较。
pub(crate) fn normalize_label(label: &str) -> String {
    let mut s = label.replace('&', "").to_lowercase();
    if let Some(head) = s.split('\t').next() {
        s = head.to_string();
    }
    let s: String = s
        .chars()
        .map(|c| match c {
            '“' | '”' | '＂' => '"',
            '‘' | '’' | '＇' => '\'',
            '（' => '(',
            '）' => ')',
            '：' => ':',
            '，' => ',',
            _ => c,
        })
        .collect();
    s.trim().trim_end_matches('.').trim_end_matches('…').trim().to_string()
}

/// 关键词是否命中菜单项。`label` 必须已经过 [`normalize_label`]。
/// 普通关键词 = **包含**即命中；`!` 开头 = **精确相等**（`!` 本身不参与比较）。
pub(crate) fn keyword_hit(label: &str, keywords: &[String]) -> bool {
    if label.is_empty() {
        return false;
    }
    keywords.iter().any(|keyword| {
        let keyword = keyword.trim();
        if keyword.is_empty() {
            return false;
        }
        // 全角的「！」也当 `!` 用：中文输入法下很容易打出来。
        match keyword.strip_prefix('!').or_else(|| keyword.strip_prefix('！')) {
            Some(exact) => normalize_label(exact) == label,
            None => label.contains(&normalize_label(keyword)),
        }
    })
}

#[cfg(windows)]
mod imp;

#[cfg(test)]
mod tests {
    use super::{keyword_hit, normalize_label};

    fn hit(label: &str, keywords: &[&str]) -> bool {
        let keywords: Vec<String> = keywords.iter().map(|k| k.to_string()).collect();
        keyword_hit(&normalize_label(label), &keywords)
    }

    #[test]
    fn normalize_strips_accel_and_ellipsis() {
        assert_eq!(normalize_label("&Open"), "open");
        assert_eq!(normalize_label("Scan with Microsoft Defender..."), "scan with microsoft defender");
        assert_eq!(normalize_label("Scan with Microsoft Defender…"), "scan with microsoft defender");
        assert_eq!(normalize_label("Open\tCtrl+O"), "open");
    }

    #[test]
    fn normalize_folds_full_width_punctuation() {
        assert_eq!(normalize_label("添加到 “重.rar”"), normalize_label("添加到 \"重.rar\""));
        assert_eq!(normalize_label("打开（新窗口）"), "打开(新窗口)");
    }

    #[test]
    fn substring_keyword_matches() {
        assert!(hit("用手机打开", &["用手机打开"]));
        assert!(hit("上传到百度网盘", &["百度"]));
        assert!(hit("pin to quick access", &["Pin to Quick Access"]));
        // 带文件名的动态项：填固定片段即可
        assert!(hit("添加到 \"重.rar\"", &["添加到 \""]));
        // 全角引号手打也能命中
        assert!(hit("添加到 \"重.rar\"", &["添加到 “"]));
    }

    #[test]
    fn bang_prefix_is_exact_match() {
        assert!(hit("Open", &["!Open"]));
        assert!(!hit("Open in new window", &["!Open"]));
        assert!(!hit("Open", &["!open with"]));
        // `!` 后的内容同样做归一化
        assert!(hit("&Open", &["!open"]));
        // 中文输入法打出的全角「！」同效
        assert!(hit("Open", &["！Open"]));
        assert!(!hit("Open in new window", &["！Open"]));
    }

    #[test]
    fn exact_match_is_literal() {
        // `!` = 完全相等：带了热键尾巴就是带尾巴，不帮用户“猜”。
        assert!(hit("复制(C)", &["!复制(C)"]));
        assert!(!hit("复制(C)", &["!复制"]));
        assert!(!hit("复制", &["!复制(C)"]));
        assert!(!hit("Open (new window)", &["!Open"]));
    }

    #[test]
    fn blank_keywords_are_ignored() {
        assert!(!hit("Open", &[""]));
        assert!(!hit("Open", &["   "]));
        assert!(!hit("Open", &[]));
    }

    #[test]
    fn default_keywords_hide_only_the_standard_four() {
        let kw = crate::config::default_context_menu_hide();
        for label in [
            "Copy",
            "Cut",
            "Paste",
            "Refresh",
            "复制(C)",
            "剪切(T)",
            "粘贴(P)",
            "刷新(E)",
            "Pin to Quick access",
            "固定到快速访问",
            "!Folder",
            "!文件夹(F)",
        ] {
            assert!(
                keyword_hit(&normalize_label(label), &kw),
                "默认项应当藏掉 {label}"
            );
        }
        // 精确匹配：变体项不能跟着消失。
        for label in [
            "Copy to folder",
            "复制到文件夹",
            "Paste shortcut",
            "刷新视图",
            "Unpin from Quick access",
            "取消固定到快速访问",
        ] {
            assert!(
                !keyword_hit(&normalize_label(label), &kw),
                "默认项不能误伤 {label}"
            );
        }
    }
}
