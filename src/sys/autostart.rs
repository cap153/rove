//! 「开机自启动」：登录 Windows 时后台常驻（托盘 + 文件对话框吸附 + 索引预热）。
//!
//! 实现方式是写当前用户的 Run 键：
//!
//! ```text
//! HKCU\Software\Microsoft\Windows\CurrentVersion\Run
//!   Rove = "<当前 exe 绝对路径>" --background
//! ```
//!
//! 只动 `HKEY_CURRENT_USER`，不需要管理员权限，也不碰 HKLM。
//!
//! 与「接管资源管理器」不同，这里**注册表就是唯一真源**，不额外存配置项：
//! 用户可以直接在 任务管理器 → 启动 里停用，也可以在 regedit 里删掉，开关
//! 每次进设置页都按实况显示，不会「配置说开着、系统其实没启动」。
//!
//! 两条安全规则：
//! - `Rove` 这个值属于别的程序 → 一律不覆盖，返回错误；
//! - 是我们写的但 exe 路径变了（便携版换了目录、改了名字）→ 静默改写成当前路径。
//!
//! 归属判定的完整口径见 [`command_is_ours`]。

use std::path::Path;

use crate::logv;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
/// 任务管理器「启动」页把停用状态记在这里（值为 12 字节 REG_BINARY）。
const APPROVED_KEY: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";
const VALUE_NAME: &str = "Rove";
const BACKGROUND_ARG: &str = "--background";
const REG_SZ: u32 = 1;

/// 写进 Run 键的命令行：带引号的 exe + 后台参数。
pub fn command_for(exe: &Path) -> String {
    format!("\"{}\" {}", exe.display(), BACKGROUND_ARG)
}

fn current_exe() -> Option<std::path::PathBuf> {
    std::env::current_exe().ok()
}

/// 把命令行拆成（可执行文件, 参数列表）。只做 Windows 命令行最基本的两条规则：
/// 首段可能有引号；其余按空白切分。够用来判断「是不是我们写的」。
fn split_command(value: &str) -> Option<(String, Vec<String>)> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let (exe, rest) = if let Some(stripped) = value.strip_prefix('"') {
        let end = stripped.find('"')?;
        (stripped[..end].to_string(), &stripped[end + 1..])
    } else {
        match value.find(char::is_whitespace) {
            Some(at) => (value[..at].to_string(), &value[at..]),
            None => (value.to_string(), ""),
        }
    };
    let args: Vec<String> = rest.split_whitespace().map(|s| s.to_string()).collect();
    Some((exe, args))
}

/// 这条 Run 值是不是我们写的。
///
/// 必须恰好只带一个 `--background` 参数，且 exe 满足以下任一条：
/// 1. 路径**就是当前正在运行的这个程序** —— 重命名的副本（`rove - 副本.exe`）
///    也算我们，否则用户会在设置里看到「注册表明明有、开关却没勾上」；
/// 2. 文件名与当前 exe 同名 —— 便携版换了目录（名字没变）仍认得；
/// 3. 文件名以 `rove` 开头 —— 换名字（`rove.exe` ↔ `rove - 副本.exe`）仍认得，
///    由 [`heal`] 静默改写成当前路径。
///
/// 三条都不成立才算「别的程序的」，那才不允许动。
///
/// 为什么不用配置里记「写入时的名字」：注册表是唯一真源（用户可能在 regedit
/// 里删掉、在任务管理器里停用、或换成别的程序的值），多一份存盘记录就多一处
/// 可能说谎的地方 —— 配置说「是我们写的」而实际已被别人改写时，就会去覆盖别人的
/// 条目。而「当前 exe 路径/文件名」无需存盘、随手可得，且永远与正在跑的程序一致。
fn command_is_ours(value: &str, current: Option<&Path>) -> bool {
    let Some((exe, args)) = split_command(value) else {
        return false;
    };
    if args.len() != 1 || !args[0].eq_ignore_ascii_case(BACKGROUND_ARG) {
        return false;
    }
    let exe = Path::new(&exe);
    if let Some(current) = current {
        // 1) 路径就是当前进程自己：Windows 路径大小写不敏感。
        if exe.to_string_lossy().eq_ignore_ascii_case(&current.to_string_lossy()) {
            return true;
        }
        // 2) 同名文件（便携版换目录）。
        let same_name = match (exe.file_name(), current.file_name()) {
            (Some(a), Some(b)) => a.to_string_lossy().eq_ignore_ascii_case(&b.to_string_lossy()),
            _ => false,
        };
        if same_name {
            return true;
        }
    }
    // 3) 文件名以 rove 开头（rove.exe / rove_debug.exe / rove - 副本.exe …）。
    exe.file_stem()
        .and_then(|n| n.to_str())
        .is_some_and(|stem| stem.to_lowercase().starts_with("rove"))
}

/// 用当前进程路径做归属判定。
fn is_ours(value: &str) -> bool {
    command_is_ours(value, current_exe().as_deref())
}

/// 当前 Run 键里的 `Rove` 值（None = 不存在）。
fn read_command() -> Option<String> {
    let (kind, data) = super::integration::reg::peek_value(RUN_KEY, Some(VALUE_NAME))
        .ok()
        .flatten()?;
    if kind != REG_SZ {
        return Some(String::new());
    }
    Some(super::integration::bytes_to_string(&data))
}

/// 值存在但不是我们写的（返回原命令，供报错文案使用）。
fn foreign_command() -> Option<String> {
    read_command().filter(|value| !is_ours(value))
}

/// 报错文案里的「别人的那条」，类型异常/空值时给个可读的占位。
fn foreign_label(command: &str) -> &str {
    if command.trim().is_empty() {
        "（内容不是文本）"
    } else {
        command
    }
}

/// 任务管理器 → 启动 里是否被停用。
///
/// 该值的首个 DWORD 为 2 或 6 表示启用，3 表示停用（后面的 QWORD 是停用时间）；
/// 值不存在也表示启用（`reg add` 手动加进去的项根本不会有这条记录）。
fn disabled_by_task_manager() -> bool {
    let Ok(Some((kind, data))) =
        super::integration::reg::peek_value(APPROVED_KEY, Some(VALUE_NAME))
    else {
        return false;
    };
    // REG_BINARY = 3；其他类型（含被人手动写成字符串）一律按「没停用」处理。
    if kind != 3 || data.len() < 4 {
        return false;
    }
    !approved_flags_enabled(u32::from_le_bytes([data[0], data[1], data[2], data[3]]))
}

/// `StartupApproved` 首 DWORD 的含义：2 / 6 = 启用，3 = 停用（其余值按停用处理）。
fn approved_flags_enabled(flags: u32) -> bool {
    flags == 2 || flags == 6
}

/// 真实状态：值存在、是我们写的、且没被任务管理器停用。
pub fn enabled() -> bool {
    let Some(command) = read_command() else {
        return false;
    };
    is_ours(&command) && !disabled_by_task_manager()
}

/// 便携版换了目录后把 Run 值改写到当前 exe 路径（启动时调一次）。
pub fn heal() {
    let Some(exe) = current_exe() else { return };
    let expected = command_for(&exe);
    let Some(current) = read_command() else { return };
    if !is_ours(&current) || current == expected {
        return;
    }
    // 被任务管理器停用的条目也照样改写：值名没变，停用记录仍然有效，
    // 这样用户哪天在任务管理器里重新启用，指向的就是当前路径。
    match super::integration::reg::write_string(RUN_KEY, Some(VALUE_NAME), &expected) {
        Ok(()) => logv!("[autostart] 路径已更新: {current} -> {expected}"),
        Err(err) => crate::logln!("!! [autostart] 更新登录启动路径失败: {err}"),
    }
}

/// 打开 / 关闭登录启动。真源是注册表，成功后调用方按 [`enabled`] 回填开关。
pub fn set_enabled(enable: bool) -> Result<(), String> {
    let Some(exe) = current_exe() else {
        return Err("无法确定 Rove 的程序路径".into());
    };
    if enable {
        // 只在「开启」时检查占用：关闭一个不属于自己的条目没有意义，
        // 也不该报错（那种情况下开关本来就显示为关）。
        if let Some(foreign) = foreign_command() {
            return Err(format!(
                "登录启动项 Rove 已被其它程序占用，未做修改：{}",
                foreign_label(&foreign)
            ));
        }
        // 用户在任务管理器里停用过：清掉那条停用记录，否则写回 Run 值也不会启动。
        let _ = super::integration::reg::delete_value(APPROVED_KEY, VALUE_NAME);
        let expected = command_for(&exe);
        super::integration::reg::write_string(RUN_KEY, Some(VALUE_NAME), &expected)
            .map_err(|err| format!("写入登录启动项失败：{err}"))?;
        // 回读确认：避免「开关显示开了、其实没写进去」。
        if read_command().as_deref() != Some(expected.as_str()) {
            return Err("登录启动项写入后校验不一致".into());
        }
        Ok(())
    } else {
        super::integration::reg::delete_value(RUN_KEY, VALUE_NAME)
            .map_err(|err| format!("删除登录启动项失败：{err}"))?;
        // 顺手清掉停用记录：只删条目却留着记录，下次再加回来会被当成停用。
        let _ = super::integration::reg::delete_value(APPROVED_KEY, VALUE_NAME);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_own_autostart_commands() {
        assert!(is_ours(r#""C:\Users\a\Downloads\rove.exe" --background"#));
        // 项目名大小写与路径无关。
        assert!(is_ours(r#""D:\tools\ROVE.EXE" --background"#));
        assert!(is_ours(r#""C:\x\rove.exe" --BACKGROUND"#));
    }

    /// 重命名的副本（`rove - 副本.exe`）也是我们自己：
    /// 否则注册表里明明有这条，设置页的开关却显示为关。
    #[test]
    fn recognizes_renamed_copies_as_ours() {
        let value = r#""C:\Users\captain\Downloads\rove - 副本.exe" --background"#;
        // 文件名以 rove 开头 → 认得；路径相同（正在跑的就是副本）也认得。
        assert!(command_is_ours(value, None));
        assert!(command_is_ours(value, Some(Path::new(r"C:\Users\captain\Downloads\rove - 副本.exe"))));
        // 大小写不敏感。
        assert!(command_is_ours(
            r#""C:\A\ROVE - 副本.EXE" --background"#,
            Some(Path::new(r"c:\a\rove - 副本.exe"))
        ));
        // 路径完全吻合即算我们，即使文件名不以 rove 开头。
        assert!(command_is_ours(
            r#""C:\x\fm.exe" --background"#,
            Some(Path::new(r"C:\x\fm.exe"))
        ));
        // 同名文件但换了目录（便携版移动）也算我们。
        assert!(command_is_ours(
            r#""D:\old\fm.exe" --background"#,
            Some(Path::new(r"C:\new\fm.exe"))
        ));
        // 但别人的程序（不同路径、不同名、不以 rove 开头）→ 依然不是我们的。
        assert!(!command_is_ours(
            r#""C:\x\fm.exe" --background"#,
            Some(Path::new(r"C:\y\rove.exe"))
        ));
        // 参数不匹配时，即使同名也不算。
        assert!(!command_is_ours(
            r#""C:\x\rove.exe" --background --profile p"#,
            Some(Path::new(r"C:\x\rove.exe"))
        ));
    }

    #[test]
    fn rejects_foreign_or_extra_arguments() {
        // 别的程序。
        assert!(!is_ours(r#""C:\Program Files\Other\other.exe" --background"#));
        // 我们的程序但多了参数（不是我们写的那条）。
        assert!(!is_ours(
            r#""C:\x\rove.exe" --background --profile "C:\p""#
        ));
        // 少了后台参数：那是「打开主窗口」的旧写法，不该被当成开机自启。
        assert!(!is_ours(r#""C:\x\rove.exe""#));
        assert!(!is_ours(""));
    }

    #[test]
    fn command_quotes_the_executable_path() {
        let path = Path::new(r"C:\Users\captain\Downloads\rove.exe");
        assert_eq!(
            command_for(path),
            r#""C:\Users\captain\Downloads\rove.exe" --background"#
        );
        // 带空格与中文也必须整段引号包住。
        let path = Path::new(r"D:\我的 工具\rove.exe");
        assert_eq!(command_for(path), r#""D:\我的 工具\rove.exe" --background"#);
    }

    #[test]
    fn task_manager_flags_decide_enabled_state() {
        // 任务管理器：2 / 6 = 启用，3 = 停用（秒杀这类值按停用处理更安全）。
        assert!(approved_flags_enabled(2));
        assert!(approved_flags_enabled(6));
        assert!(!approved_flags_enabled(3));
        assert!(!approved_flags_enabled(0));
    }

    #[test]
    fn split_command_handles_quoted_and_bare_paths() {
        assert_eq!(
            split_command(r#""C:\a b\rove.exe" --background"#),
            Some((r"C:\a b\rove.exe".into(), vec!["--background".into()]))
        );
        assert_eq!(
            split_command(r"C:\a\rove.exe --background"),
            Some((r"C:\a\rove.exe".into(), vec!["--background".into()]))
        );
        assert_eq!(split_command(""), None);
        // 引号没闭合：解析失败，判不出归属（按「不是我们的」处理，绝不覆盖）。
        assert_eq!(split_command(r#""C:\a\rove.exe --background"#), None);
    }
}
