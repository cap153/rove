//! 压缩软件探测器：递归扫描开始菜单快捷方式 + 读取注册表 `App Paths`。
//!
//! 识别 7-Zip / WinRAR / Bandizip，供动作引擎生成"压缩 / 解压"动作。

use std::path::{Path, PathBuf};

#[cfg(windows)]
use windows::Win32::Foundation::ERROR_SUCCESS;
#[cfg(windows)]
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, IPersistFile,
    STGM_READ,
};
#[cfg(windows)]
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RegCloseKey, RegOpenKeyExW, RegQueryValueExW,
    KEY_READ, KEY_WOW64_64KEY,
};
#[cfg(windows)]
use windows::Win32::UI::Shell::IShellLinkW;
#[cfg(windows)]
use windows::core::{GUID, Interface, PCWSTR};

#[cfg(windows)]
use crate::wstr::{pcw, wide};

/// 受支持的压缩工具
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    WinRAR,
    SevenZip,
    Bandizip,
}

impl Tool {
    pub fn id(self) -> &'static str {
        match self {
            Tool::WinRAR => "winrar",
            Tool::SevenZip => "7zip",
            Tool::Bandizip => "bandizip",
        }
    }

    pub fn display(self) -> &'static str {
        match self {
            Tool::WinRAR => "WinRAR",
            Tool::SevenZip => "7-Zip",
            Tool::Bandizip => "Bandizip",
        }
    }

    /// 探测优先级：WinRAR 优先（规范要求主动作默认 WinRAR）
    pub fn preference(self) -> u8 {
        match self {
            Tool::WinRAR => 0,
            Tool::SevenZip => 1,
            Tool::Bandizip => 2,
        }
    }

    /// `App Paths` 下登记的可执行文件名
    fn app_paths_names(self) -> &'static [&'static str] {
        match self {
            Tool::WinRAR => &["WinRAR.exe", "Rar.exe"],
            Tool::SevenZip => &["7zFM.exe", "7z.exe"],
            Tool::Bandizip => &["Bandizip.exe"],
        }
    }

    /// 开始菜单快捷方式指向的可执行文件名
    fn shortcut_names(self) -> &'static [&'static str] {
        match self {
            Tool::WinRAR => &["WinRAR.exe"],
            Tool::SevenZip => &["7zFM.exe", "7z.exe"],
            Tool::Bandizip => &["Bandizip.exe"],
        }
    }

    /// 该工具实际用于命令行的可执行名（7-Zip 用 CLI 而非文件管理器）
    fn cli_name(self) -> &'static str {
        match self {
            Tool::WinRAR => "WinRAR.exe",
            Tool::SevenZip => "7z.exe",
            Tool::Bandizip => "Bandizip.exe",
        }
    }
}

/// 一次探测结果
#[derive(Debug, Clone)]
pub struct Detection {
    pub tool: Tool,
    /// 安装目录下的主程序路径
    pub program: PathBuf,
}

/// 探测已安装的压缩软件（开始菜单快捷方式 + 注册表 App Paths）
#[cfg(windows)]
pub fn detect() -> Vec<Detection> {
    let mut found: Vec<Detection> = Vec::new();

    for tool in [Tool::WinRAR, Tool::SevenZip, Tool::Bandizip] {
        // 1) 注册表 App Paths（最权威）
        if let Some(path) = detect_from_registry(tool) {
            found.push(Detection {
                tool,
                program: cli_program(tool, &path),
            });
            continue;
        }
        // 2) 开始菜单快捷方式
        if let Some(path) = detect_from_start_menu(tool) {
            found.push(Detection {
                tool,
                program: cli_program(tool, &path),
            });
        }
    }

    found
}

/// 选择主动作工具：WinRAR 优先，否则按 7-Zip -> Bandizip
pub fn primary_tool(detected: &[Detection]) -> Option<Tool> {
    detected
        .iter()
        .min_by_key(|entry| entry.tool.preference())
        .map(|entry| entry.tool)
}

/// 主程序 -> 命令行程序（7-Zip 的 FM 与 CLI 同目录）
fn cli_program(tool: Tool, program: &Path) -> PathBuf {
    if tool == Tool::SevenZip {
        if let Some(parent) = program.parent() {
            let cli = parent.join(tool.cli_name());
            if cli.exists() {
                return cli;
            }
        }
    }
    program.to_path_buf()
}

// ---------------------------------------------------------------- 注册表

#[cfg(windows)]
fn detect_from_registry(tool: Tool) -> Option<PathBuf> {
    for name in tool.app_paths_names() {
        let key = format!("SOFTWARE\\Microsoft\\Windows\\Current\\App Paths\\{name}");
        for root in [HKEY_LOCAL_MACHINE, HKEY_CURRENT_USER] {
            if let Some(value) = query_default_value(root, &key) {
                let path = PathBuf::from(value.trim_matches('"'));
                if path.exists() {
                    return Some(path);
                }
            }
        }
    }
    None
}

/// 读取注册表项默认值（REG_SZ）
#[cfg(windows)]
fn query_default_value(root: HKEY, subkey: &str) -> Option<String> {
    let subkey_wide = wide(subkey);
    let mut handle = HKEY::default();
    let status = unsafe {
        RegOpenKeyExW(
            root,
            pcw(&subkey_wide),
            None,
            KEY_READ | KEY_WOW64_64KEY,
            &mut handle,
        )
    };
    if status != ERROR_SUCCESS {
        return None;
    }

    // 第一次查询取长度
    let mut size: u32 = 0;
    let status = unsafe {
        RegQueryValueExW(
            handle,
            PCWSTR::null(),
            None,
            None,
            None,
            Some(&mut size as *mut u32),
        )
    };
    if status != ERROR_SUCCESS || size == 0 {
        let _ = unsafe { RegCloseKey(handle) };
        return None;
    }

    let mut buffer = vec![0u8; size as usize];
    let status = unsafe {
        RegQueryValueExW(
            handle,
            PCWSTR::null(),
            None,
            None,
            Some(buffer.as_mut_ptr()),
            Some(&mut size as *mut u32),
        )
    };
    let _ = unsafe { RegCloseKey(handle) };
    if status != ERROR_SUCCESS {
        return None;
    }

    let wide: Vec<u16> = buffer
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .take_while(|value| *value != 0)
        .collect();
    Some(String::from_utf16_lossy(&wide))
}

// ---------------------------------------------------------------- 开始菜单

/// 开始菜单快捷方式目录（用户 + 所有用户）
fn start_menu_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(config) = dirs::config_dir() {
        dirs.push(
            config
                .join("Microsoft")
                .join("Windows")
                .join("Start Menu")
                .join("Programs"),
        );
    }
    let program_data =
        std::env::var("PROGRAMDATA").unwrap_or_else(|_| "C:\\ProgramData".to_string());
    dirs.push(
        PathBuf::from(program_data)
            .join("Microsoft")
            .join("Windows")
            .join("Start Menu")
            .join("Programs"),
    );
    dirs
}

#[cfg(windows)]
fn detect_from_start_menu(tool: Tool) -> Option<PathBuf> {
    for dir in start_menu_dirs() {
        if let Some(path) = scan_dir(&dir, tool, 0) {
            return Some(path);
        }
    }
    None
}

/// 递归扫描开始菜单目录，解析 `.lnk` 目标
#[cfg(windows)]
fn scan_dir(dir: &Path, tool: Tool, depth: u32) -> Option<PathBuf> {
    if depth > 4 {
        return None;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return None;
    };
    let mut sub_dirs = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            sub_dirs.push(path);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("lnk") {
            continue;
        }
        let Some(target) = resolve_shortcut(&path) else {
            continue;
        };
        let Some(name) = target.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if tool
            .shortcut_names()
            .iter()
            .any(|candidate| name.eq_ignore_ascii_case(candidate))
        {
            return Some(target);
        }
    }

    for sub in sub_dirs {
        if let Some(path) = scan_dir(&sub, tool, depth + 1) {
            return Some(path);
        }
    }
    None
}

/// 解析 `.lnk` 指向的目标路径（IPersistFile::Load + IShellLinkW::GetPath）
#[cfg(windows)]
pub fn resolve_shortcut(link: &Path) -> Option<PathBuf> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

        // CLSID_ShellLink
        let clsid = GUID::from_u128(0x0002_1401_0000_0000_C000_0000_0000_0046);
        let shell_link: IShellLinkW =
            CoCreateInstance(&clsid, None, CLSCTX_INPROC_SERVER).ok()?;

        // 通过 IPersistFile 加载快捷方式文件
        let file: IPersistFile = shell_link.cast().ok()?;
        let path_wide = wide(&link.display().to_string());
        file.Load(pcw(&path_wide), STGM_READ).ok()?;

        let mut buffer = [0u16; 260];
        let mut data = windows::Win32::Storage::FileSystem::WIN32_FIND_DATAW::default();
        shell_link.GetPath(&mut buffer, &mut data, 0).ok()?;
        let end = buffer
            .iter()
            .position(|c| *c == 0)
            .unwrap_or(buffer.len());
        let text = String::from_utf16_lossy(&buffer[..end]);
        if text.is_empty() {
            return None;
        }
        Some(PathBuf::from(text))
    }
}

/// 非 Windows 平台无压缩软件可探测
#[cfg(not(windows))]
pub fn detect() -> Vec<Detection> {
    Vec::new()
}

/// 非 Windows 平台无法解析 .lnk
#[cfg(not(windows))]
pub fn resolve_shortcut(_link: &Path) -> Option<PathBuf> {
    None
}
