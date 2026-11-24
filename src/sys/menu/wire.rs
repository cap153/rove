//! 父子进程之间的请求 / 事件格式。
//!
//! 菜单**建立在子进程里**：第三方外壳扩展卡死时，卡的是那个子进程，主程序
//! 毫发无伤，而且进程能 `kill`（线程杀不掉）。父进程只做两件事：把要显示的
//! 内容送过去，把用户点了哪一项收回来。
//!
//! 请求走 stdin 一次性写完整段 bincode，子进程读到 EOF 就开始干活；
//! 事件走 stdout 一行一条纯文本 —— 出错时人能直接看懂，也不用对称的解析器。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{MenuNode, Target};

/// 父进程 → 子进程：这一次要弹什么。
#[derive(Serialize, Deserialize)]
pub(super) struct Request {
    pub target: Target,
    pub nodes: Vec<MenuNode>,
    pub hide_keywords: Vec<String>,
    pub extended: bool,
    pub dark: bool,
    /// Rove 主窗口（Shell 的确认框 / 属性框以它为父窗口）。
    pub owner: isize,
}

/// 子进程 → 父进程：「我这个进程起来了」。
///
/// 2.5 秒的看门狗从这一刻才开始算 —— 冷启动 / 杀毒扫描 exe 不该被当成
/// 「外壳扩展卡死」。子进程一直不报这一声，父进程另有 5 秒硬上限兜着。
pub(super) const EV_READY: &str = "ready";
/// 子进程 → 父进程：菜单已经弹到屏幕上（看门狗据此停止计时）。
pub(super) const EV_SHOWN: &str = "shown";
/// 用户按了 Esc / 点了别处，什么也没选。
pub(super) const EV_DISMISSED: &str = "dismissed";
/// 用户点了 Rove 自己的命令项（命令 key 跟在后面）。
pub(super) const EV_APP: &str = "app\t";
/// 点了 Shell 的「重命名」：改走应用内联重命名。
pub(super) const EV_RENAME: &str = "rename";
/// 点了 Shell 的打开 / 在新窗口中打开，且目标是单个文件夹。
pub(super) const EV_NAVIGATE: &str = "nav\t";
/// Shell 命令开始执行 / 结束（期间要抑制回车穿透）。
pub(super) const EV_BEGIN: &str = "begin";
pub(super) const EV_END: &str = "end";
/// Shell 操作可能改了磁盘：让 UI 刷新当前目录。
pub(super) const EV_REFRESH: &str = "refresh";

/// 事件里夹带路径时先转义。
///
/// 协议是「一行一条」，而 NTFS **允许**文件名里带换行 —— 不转义的话
/// `nav\tO:\\a<换行>b` 会被父进程拆成两行，导航到半截路径。
/// `%` 一起转义，才分得清「文件名里本来就有 %0A」和「换行」。
pub(super) fn escape_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    let mut out = String::with_capacity(text.len() + 8);
    for ch in text.chars() {
        match ch {
            '%' => out.push_str("%25"),
            '\n' => out.push_str("%0A"),
            '\r' => out.push_str("%0D"),
            other => out.push(other),
        }
    }
    out
}

/// `escape_path` 的逆运算。
pub(super) fn unescape_path(text: &str) -> PathBuf {
    if !text.contains('%') {
        return PathBuf::from(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('%') {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        if tail.starts_with("%25") {
            out.push('%');
            rest = &tail[3..];
        } else if tail.starts_with("%0A") {
            out.push('\n');
            rest = &tail[3..];
        } else if tail.starts_with("%0D") {
            out.push('\r');
            rest = &tail[3..];
        } else {
            out.push('%');
            rest = &tail[1..];
        }
    }
    out.push_str(rest);
    PathBuf::from(out)
}

/// 子进程往父进程写一行事件，立刻 flush（父进程的看门狗在等）。
pub(super) fn emit(line: &str) {
    use std::io::Write;
    let mut out = std::io::stdout();
    let _ = out.write_all(line.as_bytes());
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}
