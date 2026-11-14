//! 文件操作子系统：剪贴板互通、任务化复制/移动/删除、终端唤起。

pub mod actions;
pub mod clipboard;
pub mod jobs;
pub mod keymap;
#[cfg(windows)]
pub mod thumbs;

/// 「启动外部程序失败」的提示，由 UI 线程轮询取走（见 `take_launch_errors`）。
///
/// 终端与自定义右键动作共用：`CreateProcess` 会把 `current_dir` 真的打开
/// 一遍，网络盘 / WSL 共享（`\\wsl.localhost\...`）没响应时这一个调用能卡住
/// 几十秒；跑在 UI 线程上就是整个 Rove 变「未响应」、托盘也点不动。现在这些
/// 启动动作全部丢到后台线程，失败信息走这条队列，由 40ms 轮询取走显示。
fn launch_errors() -> &'static std::sync::Mutex<Vec<String>> {
    static QUEUE: std::sync::OnceLock<std::sync::Mutex<Vec<String>>> = std::sync::OnceLock::new();
    QUEUE.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

pub(crate) fn report_launch_error(message: String) {
    if let Ok(mut queue) = launch_errors().lock() {
        // 连点几十次也只留最近几条，队列不无限增长。
        if queue.len() < 8 {
            queue.push(message);
        }
    }
}

/// UI 线程取走待显示的错误提示（由 40ms 轮询定时器调用）。
pub fn take_launch_errors() -> Vec<String> {
    match launch_errors().lock() {
        Ok(mut queue) => std::mem::take(&mut *queue),
        Err(_) => Vec::new(),
    }
}

/// 在指定目录打开终端（后台线程执行，立即返回）。
///
/// 固定用 `cmd.exe`：Windows Terminal（wt.exe）存在时会按它的**默认配置**
/// 启动，而默认配置通常是 PowerShell —— 用户要的是 cmd，所以不再探测 wt。
pub fn open_terminal(dir: &std::path::Path) {
    let dir = dir.to_path_buf();
    let spawned = std::thread::Builder::new()
        .name("rove-terminal".into())
        .spawn(move || {
            if let Err(e) = launch_terminal(&dir) {
                report_launch_error(e);
            }
        });
    if spawned.is_err() {
        report_launch_error("启动终端失败：无法创建线程".to_string());
    }
}

/// 真正的启动动作（只在后台线程里跑，可以随便阻塞）。
///
/// `target` 可以是目录，也可以是文件（右键「在此处打开终端」允许选中文件，
/// 这时开在它所在的目录）；这些 `is_dir()` 判断放在这里是为了不阻塞界面线程。
fn launch_terminal(target: &std::path::Path) -> Result<(), String> {
    let dir: &std::path::Path = if target.is_dir() {
        target
    } else if let Some(parent) = target.parent().filter(|p| p.is_dir()) {
        parent
    } else {
        // `recycle://` 之类的虚拟路径、刚被删掉的目录都别往下走，
        // 否则 CreateProcess 只会回一句 os error 267。
        return Err(format!("目录不存在或无法访问：{}", target.display()));
    };
    let text = dir.to_string_lossy().into_owned();
    // 只有真正的 UNC（`\\server\share`、`//server/share`）需要 pushd；
    // `\\?\C:\...` 这类 verbatim 本地路径可以直接当工作目录。
    let is_network = crate::fs::dir::is_network_path_text(&text);
    // 优先 %ComSpec%：某些精简/加固系统把 System32 从 PATH 里摘掉了，
    // 但 ComSpec 始终指向命令行解释器。
    let shell = std::env::var_os("ComSpec").unwrap_or_else(|| "cmd.exe".into());
    let mut cmd = std::process::Command::new(shell);
    if is_network {
        // 网络路径不能当工作目录（cmd 会退到 `C:\Windows` 并警告 UNC 不支持），
        // 用 pushd 让它自己映射临时盘符。`pushd` 和路径**分成两个 argv**：
        // Rust 的引号转义和 cmd 的解析规则不是一套，拼成一个带引号的字符串
        // 在路径含空格时会被解析成别的东西。
        cmd.arg("/K").arg("pushd").arg(&text);
    } else {
        // 本地目录直接设子进程工作目录，不拼任何命令行参数，没有解析风险。
        cmd.current_dir(dir);
    }
    cmd.spawn()
        .map(|_| ())
        .map_err(|e| format!("启动 cmd.exe 失败: {e}"))
}

#[cfg(test)]
mod terminal_tests {
    use super::{launch_terminal, report_launch_error, take_launch_errors};

    /// 目录不存在（含 `recycle://` 这类虚拟路径）必须在真正启动进程之前就被挡下，
    /// 否则只会得到一句难以理解的 `os error 267`。
    #[test]
    fn missing_dir_is_reported_before_spawn() {
        let err = launch_terminal(std::path::Path::new("/no/such/dir/for/rove")).unwrap_err();
        assert!(err.contains("目录不存在"), "unexpected message: {err}");
    }

    #[test]
    fn launch_error_queue_is_drained_once() {
        report_launch_error("boom".to_string());
        let first = take_launch_errors();
        assert!(first.iter().any(|m| m == "boom"), "got {first:?}");
        assert!(take_launch_errors().is_empty());
    }
}
