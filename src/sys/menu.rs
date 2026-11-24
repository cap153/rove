//! 原生右键菜单（Shell `IContextMenu` + 追加应用命令项）。
//!
//! 为什么要有这个模块：自己画菜单要自己处理命中、子菜单、键盘、坐标、DPI、
//! 深色主题下的系统扩展图标…… 一处偏差就是一堆 bug。这里直接让 Windows
//! 显示它自己的菜单：`打开方式` `发送到` `新建` `属性` 以及所有第三方扩展项
//! （7-Zip / 网盘 / BitLocker …）全部由 Shell 提供，行为与资源管理器一致。
//!
//! 进程模型（关键）：
//!   * `TrackPopupMenuEx` 是阻塞式 API，内部会跑**嵌套消息循环**。如果放在
//!     Slint 的 UI 线程上，嵌套循环里派发出来的消息会重入 Slint 回调，
//!     而回调里再 `borrow_mut()` App 就会 panic。
//!   * 更重要的是第三方外壳扩展会在 `QueryContextMenu` 里**永远不返回**。
//!     线程杀不掉（只能丢弃，丢几次进程就废了），进程能 `TerminateProcess`。
//!     所以菜单在**子进程**（`rove.exe --menu-host`）里构建并显示：卡住就杀，
//!     主程序一行扩展代码都没跑过，永远不受牵连。
//!   * 子进程自己创建一个隐藏的宿主窗口，`TrackPopupMenuEx` 的 hWnd 必须是
//!     调用线程的窗口。菜单的四条消息（`WM_INITMENUPOPUP` / `WM_DRAWITEM` /
//!     `WM_MEASUREITEM` / `WM_MENUCHAR`）也必须由这个窗口过程转发给
//!     `IContextMenu2/3`，否则「新建 / 发送到 / 打开方式」这些懒加载子菜单
//!     是**空的**。
//!   * UI 线程只派子进程、收事件，全程不阻塞。
//!
//! id 约定（与 Shell 一致）：`1..=0x6FFF` 归 Shell 命令，`0x7000+` 归应用命令。

use std::path::PathBuf;
use std::sync::Arc;

/// 菜单里的一项（应用自己追加的命令）。
///
/// 菜单在子进程里构建，所以这些内容要序列化过去。
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
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
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
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
    /// 菜单已经关掉了（子进程画的那张可能把前台权带走过）：把焦点收回主窗口。
    Closed,
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

/// 弹原生菜单。`nodes` = 带系统项时该显示的应用项，`fallback` = 一旦系统菜单
/// 卡死要立刻退成的「只含 Rove 自己项」的那一套（由 UI 线程一并算好，
/// 退的时候不用再回头问它）。`owner` 是主窗口 HWND，用于菜单关掉后归还前台权。
/// 返回 false 表示菜单子进程根本没起来。
#[cfg(windows)]
pub(crate) use imp::{
    apply_tray_theme, busy_for_ms, open, purge_recycle_items, recently_active, set_dark,
    shell_unhealthy, shutdown, tick,
};

/// `--menu-host` 子进程入口：构建并显示菜单，然后退出进程。
#[cfg(windows)]
pub(crate) use child::run as run_host_child;

/// 非 Windows：从来就没有原生菜单，谈不上「刚动过」。
#[cfg(not(windows))]
pub(crate) fn recently_active(_ms: u64) -> bool {
    false
}

/// 非 Windows：没有菜单线程，永远不忙。
#[cfg(not(windows))]
pub(crate) fn busy_for_ms() -> Option<u64> {
    None
}

/// 非 Windows：没有系统菜单，也就谈不上「刚卡过」。
#[cfg(not(windows))]
pub(crate) fn shell_unhealthy() -> bool {
    false
}

#[cfg(not(windows))]
pub(crate) fn tick() {}

#[cfg(not(windows))]
pub(crate) fn shutdown() {}

#[cfg(not(windows))]
pub fn open(
    _: Target,
    _: Vec<MenuNode>,
    _: Vec<MenuNode>,
    _: Options,
    _: isize,
    _: Sink,
) -> bool {
    false
}

/// 非 Windows：菜单由系统绘制，没有深浅可切。
#[cfg(not(windows))]
pub fn set_dark(_: bool) {}

/// 非 Windows：没有托盘菜单要上色。
#[cfg(not(windows))]
pub(crate) fn apply_tray_theme() {}

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

/// 菜单标签结尾有没有省略号（`…` / `...`）。
///
/// 系统菜单里的「打开方式」有两种写法：带点的（弹系统选择器）和不带点的
/// （展开「程序」子菜单）。[`normalize_label`] 会把结尾的点吃掉，两者归一化
/// 之后一模一样，所以精确匹配还得回头看原始标签有没有点。
pub(crate) fn has_trailing_dots(label: &str) -> bool {
    let s = label.trim_end();
    s.ends_with('…') || s.ends_with('.')
}

/// 关键词是否命中菜单项。`label` 传**原始标签**，内部自己归一化。
/// 普通关键词 = **包含**即命中；`!` 开头 = **精确相等**（`!` 本身不参与比较），
/// 且省略号也必须一致：`!Open with...` 只藏带点的那条，不带点的子菜单项留着。
pub(crate) fn keyword_hit(label: &str, keywords: &[String]) -> bool {
    let label_text = normalize_label(label);
    if label_text.is_empty() {
        return false;
    }
    let label_dots = has_trailing_dots(label);
    keywords.iter().any(|keyword| {
        let keyword = keyword.trim();
        if keyword.is_empty() {
            return false;
        }
        // 全角的「！」也当 `!` 用：中文输入法下很容易打出来。
        match keyword.strip_prefix('!').or_else(|| keyword.strip_prefix('！')) {
            Some(exact) => {
                normalize_label(exact) == label_text && has_trailing_dots(exact) == label_dots
            }
            None => label_text.contains(&normalize_label(keyword)),
        }
    })
}

#[cfg(windows)]
mod child;
#[cfg(windows)]
mod dark;
#[cfg(windows)]
mod imp;
#[cfg(windows)]
mod wire;

#[cfg(test)]
mod tests {
    use super::{keyword_hit, normalize_label};

    fn hit(label: &str, keywords: &[&str]) -> bool {
        let keywords: Vec<String> = keywords.iter().map(|k| k.to_string()).collect();
        keyword_hit(label, &keywords)
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

    /// 属性 / 重命名 / 创建快捷方式 / 桌面快捷方式的默认隐藏关键词
    /// 必须能真的命中系统菜单的实际文字（带热键尾巴的按带尾巴写）。
    #[test]
    fn default_hide_keywords_hit_real_shell_labels() {
        let hide = crate::config::default_context_menu_hide();
        let hide: Vec<&str> = hide.iter().map(|s| s.as_str()).collect();
        assert!(hit("属性(R)", &hide));
        assert!(hit("重命名(M)", &hide));
        assert!(hit("创建快捷方式(S)", &hide));
        assert!(hit("桌面快捷方式", &hide));
        assert!(hit("Properties", &hide));
        assert!(hit("Rename", &hide));
        assert!(hit("Create shortcut", &hide));
        // 英文系统把「桌面快捷方式」写作带括号的长名。
        assert!(hit("Desktop (create shortcut)", &hide));
        // 不能误伤：Rove 自己那些项没有热键尾巴。
        assert!(!hit("新建文件夹", &hide));
    }

    /// 「打开方式」在系统菜单里有两种写法：带省略号的弹原生选择器（Rove 自己
    /// 已经有同名项，默认藏掉），不带省略号的会展开「程序」子菜单（保留）。
    /// 两者归一化后文字一模一样，只能靠结尾的省略号区分。
    #[test]
    fn ellipsis_distinguishes_the_two_open_with_items() {
        let hide = crate::config::default_context_menu_hide();
        let hide: Vec<&str> = hide.iter().map(|s| s.as_str()).collect();
        assert!(hit("打开方式(H)...", &hide));
        assert!(hit("Open with...", &hide));
        // 有的系统版本不带热键尾巴，带点的仍然要能命中。
        assert!(hit("打开方式...", &hide));
        // 不带点的那条：展开子菜单，必须留着。
        assert!(!hit("打开方式(H)", &hide));
        assert!(!hit("Open with", &hide));
        assert!(!hit("打开方式", &hide));
        // 精确匹配只认这几种写法，别的变体不受影响。
        assert!(!hit("用其他应用打开", &hide));
    }

    #[test]
    fn blank_keywords_are_ignored() {
        assert!(!hit("Open", &[""]));
        assert!(!hit("Open", &["   "]));
        assert!(!hit("Open", &[]));
    }

    /// Shell 的撤消项文字跟着上一次操作变，只能靠包含匹配全藏掉；
    /// 同时不能误伤名字里带「撤消」字样的第三方项以外的东西 —— 这里只钉
    /// 「必须藏掉」的那几种写法。
    #[test]
    fn default_keywords_hide_every_shell_undo_variant() {
        let kw = crate::config::default_context_menu_hide();
        for label in [
            "Undo",
            "Undo Copy",
            "Undo Move",
            "Undo Delete",
            "Undo Rename",
            "撤消",
            "撤消复制",
            "撤消移动",
            "撤消删除",
            "撤消重命名",
        ] {
            assert!(
                keyword_hit(label, &kw),
                "默认项应当藏掉撤消变体 {label}"
            );
        }
    }

    /// 系统菜单的删除项走 Explorer 的删除，Rove 记不了账，默认精确藏掉
    /// （Rove 自己的「删除」排在剪切下面，不受影响）。
    #[test]
    fn default_keywords_hide_the_shell_delete_item() {
        let kw = crate::config::default_context_menu_hide();
        for label in ["删除(D)", "Delete"] {
            assert!(
                keyword_hit(label, &kw),
                "默认项应当藏掉 {label}"
            );
        }
        // 精确匹配：只是文字里含「删除」的项不能被连带藏掉。
        assert!(!hit("删除到文件夹", &["!删除(D)"]));
    }

    /// 重做项跟撤消项一样按包含匹配：Shell 的文字会跟着上一次操作变。
    #[test]
    fn default_keywords_hide_every_shell_redo_variant() {
        let kw = crate::config::default_context_menu_hide();
        for label in ["Redo", "Redo Copy", "Redo Move", "Redo Delete", "恢复", "恢复复制", "恢复移动"] {
            assert!(
                keyword_hit(label, &kw),
                "默认项应当藏掉重做变体 {label}"
            );
        }
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
            "Folder",
            "文件夹(F)",
        ] {
            assert!(
                keyword_hit(label, &kw),
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
                !keyword_hit(label, &kw),
                "默认项不能误伤 {label}"
            );
        }
    }
}
