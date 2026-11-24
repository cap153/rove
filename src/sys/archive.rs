//! 压缩软件探测（WinRAR / 7-Zip / Bandizip）。
//!
//! 只认这三款，探测方法是**扫开始菜单快捷方式**再解析 `.lnk` 的目标：
//! 用户把软件装在哪个盘、哪个目录都能找到（注册表里的安装路径经常过期）。
//! 多款同时安装时按 WinRAR > 7-Zip > Bandizip 的优先级只取一款。
//!
//! 探测会读盘 + 起 COM，**不能放在界面线程上**：启动首扫走
//! `seed_on_first_run`，用户点「更新」走 `scan_async` + 主循环轮询
//! `take_result`。

use std::path::{Path, PathBuf};

/// 三款工具的识别结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolKind {
    WinRar,
    SevenZip,
    Bandizip,
}

impl ToolKind {
    /// 写进 `actions.toml` 的 `tool` 字段值（也是重扫时认回自己的标记）。
    /// 反向查：配置文件里记的 `tool` 字段。
    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            "winrar" => Some(ToolKind::WinRar),
            "7zip" => Some(ToolKind::SevenZip),
            "bandizip" => Some(ToolKind::Bandizip),
            _ => None,
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            ToolKind::WinRar => "winrar",
            ToolKind::SevenZip => "7zip",
            ToolKind::Bandizip => "bandizip",
        }
    }

    /// 界面上显示的名字。
    pub fn name(self) -> &'static str {
        match self {
            ToolKind::WinRar => "WinRAR",
            ToolKind::SevenZip => "7-Zip",
            ToolKind::Bandizip => "Bandizip",
        }
    }

    fn priority(self) -> u8 {
        match self {
            ToolKind::WinRar => 0,
            ToolKind::SevenZip => 1,
            ToolKind::Bandizip => 2,
        }
    }
}

/// 探测到的工具：类型 + 主程序路径。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tool {
    pub kind: ToolKind,
    pub program: PathBuf,
}

impl Tool {
    /// 同目录下的配套程序（存在才返回），用来挑更合适的入口。
    fn sibling(&self, names: &[&str]) -> Option<PathBuf> {
        let dir = self.program.parent()?;
        for name in names {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        None
    }

    /// 「添加到压缩文件...」用的程序：优先 GUI 入口。
    pub fn add_program(&self) -> PathBuf {
        match self.kind {
            ToolKind::WinRar => self.program.clone(),
            // 7zG 是 7-Zip 的 GUI 前端，弹的是那个「添加到压缩包」对话框。
            ToolKind::SevenZip => self.sibling(&["7zG.exe", "7zFM.exe"]).unwrap_or_else(|| self.program.clone()),
            ToolKind::Bandizip => self.program.clone(),
        }
    }

    /// 「解压到 xxx\」用的程序。
    pub fn extract_program(&self) -> PathBuf {
        match self.kind {
            ToolKind::WinRar => self.sibling(&["WinRAR.exe", "UnRAR.exe"]).unwrap_or_else(|| self.program.clone()),
            ToolKind::SevenZip => self.sibling(&["7zG.exe", "7zFM.exe"]).unwrap_or_else(|| self.program.clone()),
            ToolKind::Bandizip => self.program.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// 异步扫描（「更新」按钮 / 首次启动）
// ---------------------------------------------------------------------------

type ScanResult = Option<Tool>;

#[cfg(windows)]
fn slot() -> &'static std::sync::Mutex<Option<ScanResult>> {
    use std::sync::{Mutex, OnceLock};
    static SLOT: OnceLock<Mutex<Option<ScanResult>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// 后台线程扫一遍；结果留在槽里，由主循环 `take_result` 取走。
///
/// 返回 false 表示线程没起来（此时不会有结果回来，调用方别干等）。
pub fn scan_async() -> bool {
    #[cfg(windows)]
    {
        std::thread::Builder::new()
            .name("rove-archive-scan".into())
            .spawn(|| {
                let found = detect();
                crate::logln!(
                    "[archive] 扫描完成：{}",
                    found
                        .as_ref()
                        .map(|t| format!("{} ({})", t.kind.id(), t.program.display()))
                        .unwrap_or_else(|| "未找到".into())
                );
                if let Ok(mut slot) = slot().lock() {
                    *slot = Some(found);
                }
            })
            .is_ok()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// 取一次扫描结果（主循环里轮询，没有结果返回 None）。
pub fn take_result() -> Option<Option<Tool>> {
    #[cfg(windows)]
    {
        slot().lock().ok().and_then(|mut g| g.take())
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// 同步探测（**只在后台线程 / 启动首扫时调用**）。
#[cfg(windows)]
pub fn detect() -> Option<Tool> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::{Interface, PCWSTR};
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
        COINIT_APARTMENTTHREADED, STGM_READ,
    };
    use windows::Win32::System::Com::IPersistFile;
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};

    /// 认这三款主程序名（开始菜单里可能叫 WinRAR / 7-Zip File Manager / Bandizip）。
    fn kind_of(exe: &Path) -> Option<ToolKind> {
        let name = exe.file_name()?.to_string_lossy().to_lowercase();
        match name.as_str() {
            "winrar.exe" => Some(ToolKind::WinRar),
            "7zfm.exe" | "7zg.exe" | "7z.exe" => Some(ToolKind::SevenZip),
            "bandizip.exe" => Some(ToolKind::Bandizip),
            _ => None,
        }
    }

    /// 解析一个 `.lnk` 的目标路径。
    unsafe fn resolve(path: &Path) -> Option<PathBuf> {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
        let persist: IPersistFile = link.cast().ok()?;
        persist.Load(PCWSTR(wide.as_ptr()), STGM_READ).ok()?;
        let mut buffer = [0u16; 1024];
        link.GetPath(&mut buffer, std::ptr::null_mut(), 0).ok()?;
        let len = buffer.iter().position(|c| *c == 0).unwrap_or(buffer.len());
        if len == 0 {
            return None;
        }
        Some(PathBuf::from(String::from_utf16_lossy(&buffer[..len])))
    }

    fn collect_lnks(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
        if depth > 4 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.file_type() else { continue };
            if meta.is_dir() {
                collect_lnks(&path, out, depth + 1);
                continue;
            }
            // 先按名字过滤，避免把上百个快捷方式都过一遍 COM。
            let name = path.file_name().unwrap_or_default().to_string_lossy().to_lowercase();
            if !name.ends_with(".lnk") {
                continue;
            }
            if name.contains("winrar") || name.contains("7-zip") || name.contains("7zip") || name.contains("bandizip")
            {
                out.push(path);
            }
        }
    }

    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(p) = std::env::var_os("ProgramData") {
        roots.push(PathBuf::from(p).join("Microsoft\\Windows\\Start Menu\\Programs"));
    }
    if let Some(p) = std::env::var_os("APPDATA") {
        roots.push(PathBuf::from(p).join("Microsoft\\Windows\\Start Menu\\Programs"));
    }
    let mut lnks = Vec::new();
    for root in &roots {
        collect_lnks(root, &mut lnks, 0);
    }

    let initialized = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok();
    let mut best: Option<Tool> = None;
    for lnk in &lnks {
        let Some(target) = (unsafe { resolve(lnk) }) else {
            continue;
        };
        let Some(kind) = kind_of(&target) else { continue };
        let tool = Tool {
            kind,
            program: target,
        };
        if best.as_ref().map(|b| kind.priority() < b.kind.priority()).unwrap_or(true) {
            best = Some(tool);
        }
        // 已经拿到最高优先级（WinRAR）就不必再解析剩下的。
        if best.as_ref().map(|b| b.kind.priority()) == Some(0) {
            break;
        }
    }
    if initialized {
        unsafe { CoUninitialize() };
    }
    best
}

#[cfg(not(windows))]
pub fn detect() -> Option<Tool> {
    None
}

/// 启动首扫（同步，调用方负责放到后台线程上）。
pub fn seed_on_first_run() -> Option<Tool> {
    detect()
}
