//! Windows 原生 Shell 设施：原生文件操作（复制/移动/删除）。

#[cfg(windows)]
mod imp {
    use std::path::{Path, PathBuf};
    use windows_sys::Win32::Foundation::HWND;
    use std::sync::atomic::{AtomicIsize, Ordering};
    use windows_sys::Win32::UI::Shell::{
        SHFileOperationW, FOF_ALLOWUNDO, FOF_NOCONFIRMMKDIR, FOF_WANTNUKEWARNING, FO_COPY, FO_DELETE,
        FO_MOVE, SHFILEOPSTRUCTW,
    };

    /// 主窗口 HWND（UI 线程写入，后台 Shell 操作线程读取）。
    /// 必须传给 SHFileOperationW：否则确认/进度对话框没有宿主窗口，
    /// 我们主窗不会被禁用——对话框里的回车会同时落回 Rove，
    /// 导致“永久删除后回车把删掉的文件夹打开”这类跨窗跳键。
    static OWNER_HWND: AtomicIsize = AtomicIsize::new(0);

    /// 登记主窗口 HWND（获取到后调用一次即可，也允许重复调用）。
    pub fn set_owner_hwnd(hwnd: isize) {
        if hwnd != 0 {
            OWNER_HWND.store(hwnd, Ordering::Relaxed);
        }
    }

    /// 正在显示模态系统对话框（确认框 / 清空回收站进度）。
    ///
    /// 这类对话框会把 UI 线程堵住，看门狗会误报「UI 线程无响应」并打 `!!`；
    /// 用户正在按确认框的时候不该记异常，所以给它一个豁免标记。
    static MODAL_DEPTH: AtomicIsize = AtomicIsize::new(0);

    pub fn modal_active() -> bool {
        MODAL_DEPTH.load(Ordering::Relaxed) > 0
    }

    /// RAII 标记：析构时自动减一，中途 return / panic 都不会漏掉。
    pub struct ModalGuard;

    impl Drop for ModalGuard {
        fn drop(&mut self) {
            MODAL_DEPTH.fetch_sub(1, Ordering::Relaxed);
        }
    }

    pub fn modal_guard() -> ModalGuard {
        MODAL_DEPTH.fetch_add(1, Ordering::Relaxed);
        ModalGuard
    }

    /// 取登记过的主窗口 HWND（0 = 还没登记）。
    pub fn current_owner_hwnd() -> isize {
        OWNER_HWND.load(Ordering::Relaxed)
    }

    /// 彻底删除回收站里的一组条目。
    ///
    /// 走回收站文件夹自己的 `delete` 动词，确认框由 Shell 弹——和资源管理器
    /// 右键「删除」完全同一条路，所以对话框长得一样。实现见 `menu::purge_recycle_items`。
    pub fn purge_items(paths: &[PathBuf], hwnd: isize) -> bool {
        if paths.is_empty() {
            return false;
        }
        // 确认框会把 UI 线程堵住（资源管理器也一样），先给看门狗打豁免。
        let _modal = modal_guard();
        crate::sys::menu::purge_recycle_items(paths, hwnd) > 0
    }

    /// 取当前可用宿主窗口：优先传入值，否则用登记过的全局值。
    fn owner_hwnd(hwnd: isize) -> HWND {
        if hwnd != 0 {
            return hwnd as HWND;
        }
        OWNER_HWND.load(Ordering::Relaxed) as HWND
    }

    /// 将路径列表转换为 Windows Shell 要求的多字符串双空结尾 (double null-terminated) 宽字符流。
    fn to_double_null_wide(paths: &[PathBuf]) -> Vec<u16> {
        let mut out = Vec::new();
        for p in paths {
            let s = p.to_string_lossy().replace('/', "\\");
            out.extend(s.encode_utf16());
            out.push(0); // 单项结尾 null
        }
        out.push(0); // 整体列表结尾 double-null
        out
    }

    /// 调用 Windows 原生 Shell 执行文件操作（弹窗进度条、同名覆盖确认、UAC 提权、支持 Ctrl+Z）。
    fn run_shell_op(
        op: u32,
        from: &[PathBuf],
        to: Option<&Path>,
        flags: u32,
        hwnd: isize,
    ) -> Result<bool, String> {
        if from.is_empty() {
            return Ok(true);
        }
        let from_wide = to_double_null_wide(from);
        let to_wide = to.map(|p| to_double_null_wide(&[p.to_path_buf()]));

        let pto: *const u16 = match to_wide {
            Some(ref v) => v.as_ptr(),
            None => std::ptr::null(),
        };

        let mut sh_op = SHFILEOPSTRUCTW {
            hwnd: owner_hwnd(hwnd),
            wFunc: op,
            pFrom: from_wide.as_ptr(),
            pTo: pto,
            fFlags: flags as u16,
            fAnyOperationsAborted: 0,
            hNameMappings: std::ptr::null_mut(),
            lpszProgressTitle: std::ptr::null(),
        };

        let ret = unsafe { SHFileOperationW(&mut sh_op) };
        if ret != 0 {
            // 1223 = ERROR_CANCELLED：用户在确认框里点了「否」。
            // 这不是失败，不该在状态栏报错——资源管理器也不报。
            if ret == 1223 {
                return Ok(true);
            }
            return Err(format!("Shell 操作失败，错误代码: {ret}"));
        }
        // 返回是否被用户中途取消
        Ok(sh_op.fAnyOperationsAborted != 0)
    }

    pub fn shell_copy(from: &[PathBuf], dest_dir: &Path, hwnd: isize) -> Result<bool, String> {
        run_shell_op(
            FO_COPY,
            from,
            Some(dest_dir),
            FOF_ALLOWUNDO | FOF_NOCONFIRMMKDIR,
            hwnd,
        )
    }

    pub fn shell_move(from: &[PathBuf], dest_dir: &Path, hwnd: isize) -> Result<bool, String> {
        run_shell_op(
            FO_MOVE,
            from,
            Some(dest_dir),
            FOF_ALLOWUNDO | FOF_NOCONFIRMMKDIR,
            hwnd,
        )
    }

    pub fn shell_delete(from: &[PathBuf], to_trash: bool, hwnd: isize) -> Result<bool, String> {
        let mut flags = 0u32;
        if to_trash {
            flags |= FOF_ALLOWUNDO;
        } else {
            // Shift+Delete：不设这个标志，系统会「一声不响」直接抹掉。
            // 设了才会弹「确实要永久删除此文件吗？」（和资源管理器一致）。
            flags |= FOF_WANTNUKEWARNING;
        }
        run_shell_op(FO_DELETE, from, None, flags, hwnd)
    }

    /// 用默认程序打开一个文件 / 文件夹 / 脚本。
    ///
    /// 不用 `open` crate：它只检查「powershell 子进程有没有创建成功」，
    /// 真正执行 ShellExecute 的那一步失败（没有关联程序、需要提权、
    /// 路径在 UNC 上、被策略拦住……）一律被吞掉，表现就是「双击没反应」。
    /// 这里直接拿 `ShellExecuteExW` 的返回值 + `GetLastError`，失败必有话说。
    pub fn shell_open(path: &Path) -> Result<(), String> {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::UI::Shell::{
            ShellExecuteExW, SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SHELLEXECUTEINFOW,
        };
        use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

        let mut file: Vec<u16> = path.as_os_str().encode_wide().collect();
        file.push(0);
        let verb: Vec<u16> = "open".encode_utf16().chain([0]).collect();
        // 工作目录必须是**文件自己所在的那个文件夹**。资源管理器就是这么做的：
        // 脚本里的 `.\venv\Scripts\activate`、`.\python.exe xxx.py` 这些相对路径
        // 全靠它。留空的话子进程继承 Rove 自己的当前目录（一般是装的目录），
        // 相对路径全指错地方，脚本一声不响地失败——表现就是「双击没反应」。
        let mut workdir: Vec<u16> = path
            .parent()
            .map(|d| d.as_os_str().encode_wide().chain([0]).collect())
            .unwrap_or_default();

        let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
        info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        // NOASYNC：调用线程有消息循环（UI 线程），Shell 不会把工作丢到后台
        // 然后立刻返回成功——那样错误码就永远拿不到了。
        // NO_UI：失败时不要系统模态框（它会卡住调用线程），错误交给状态栏。
        info.fMask = SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI;
        info.hwnd = owner_hwnd(0);
        info.lpVerb = verb.as_ptr();
        info.lpFile = file.as_ptr();
        if !workdir.is_empty() {
            info.lpDirectory = workdir.as_mut_ptr();
        }
        info.nShow = SW_SHOWNORMAL;
        let ok = unsafe { ShellExecuteExW(&mut info) };
        if ok != 0 {
            return Ok(());
        }
        let code = unsafe { windows_sys::Win32::Foundation::GetLastError() };
        Err(describe_shell_error(code))
    }

    /// 显示 Shell 条目的「属性」对话框。
    ///
    /// 回收站条目传入的是 `SIGDN_DESKTOPABSOLUTEPARSING`（Shell parsing name），
    /// 不是磁盘上存在的文件路径：先 `SHParseDisplayName` 换成绝对 PIDL，再用
    /// `SEE_MASK_INVOKEIDLIST` 调用 `properties` 动词。这样由回收站自己的 Shell
    /// 扩展提供属性页（原位置 / 删除时间），而不是显示 `$R` 临时文件的属性。
    pub fn show_properties(parsing_name: &Path) -> Result<(), String> {
        use std::os::windows::ffi::OsStrExt;
        // COM 用项目已启用 feature 的 `windows` crate；Shell 结构体用 `windows-sys`
        // 的 POD 定义（windows-sys 未启用 Win32_System_Com）。
        use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_APARTMENTTHREADED};
        use windows_sys::Win32::UI::Shell::{
            Common::ITEMIDLIST, ShellExecuteExW, SHParseDisplayName, SEE_MASK_INVOKEIDLIST,
            SHELLEXECUTEINFOW,
        };
        use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

        unsafe {
            // UI 线程可能已初始化 COM；重复调用返回 S_FALSE / RPC_E_CHANGED_MODE，
            // 都不影响 ShellExecuteEx 按当前线程模型工作。
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok();
        }
        let mut name: Vec<u16> = parsing_name.as_os_str().encode_wide().collect();
        name.push(0);
        let verb: Vec<u16> = "properties".encode_utf16().chain([0]).collect();
        let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
        let mut attrs: u32 = 0;
        unsafe {
            let hr = SHParseDisplayName(
                name.as_ptr(),
                std::ptr::null_mut(),
                &mut pidl,
                0,
                &mut attrs,
            );
            if hr != 0 || pidl.is_null() {
                return Err(format!("无法解析 Shell 条目（HRESULT 0x{:08X}）", hr as u32));
            }
            let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
            info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
            info.fMask = SEE_MASK_INVOKEIDLIST;
            info.hwnd = owner_hwnd(0);
            info.lpVerb = verb.as_ptr();
            info.lpIDList = pidl.cast();
            info.nShow = SW_SHOWNORMAL;
            let ok = ShellExecuteExW(&mut info);
            CoTaskMemFree(Some(pidl as *const std::ffi::c_void));
            if ok == 0 {
                return Err("Shell 未能打开属性对话框".to_string());
            }
        }
        Ok(())
    }

    /// `ShellExecuteExW` 的错误码 → 人话。
    ///
    /// 这些码值来自 WinError.h。刻意不把原始数字塞给用户，
    /// 只在后面附上，方便排查。
    fn describe_shell_error(code: u32) -> String {
        let reason = match code {
            2 => "找不到这个文件（可能刚被移动或删除）",
            3 => "找不到这个路径（上级文件夹可能已不存在）",
            5 => "拒绝访问：权限不够，或文件被占用",
            15 => "驱动器不存在（U 盘拔了 / 网络位置掉线）",
            31 => "设备未就绪",
            32 => "文件被其他程序占用",
            1155 => "这种文件没有默认打开方式，请先设置文件关联",
            1223 => "操作已取消",
            740 => "这个操作需要管理员权限",
            _ => "系统没能执行这个操作",
        };
        format!("{reason}（错误 {code}）")
    }
}

#[cfg(windows)]
pub use imp::*;

#[cfg(not(windows))]
#[allow(dead_code)]
mod imp {
    use std::path::{Path, PathBuf};

    pub fn set_owner_hwnd(_hwnd: isize) {}

    pub fn current_owner_hwnd() -> isize {
        0
    }

    /// 非 Windows：退回 `open` crate（只有宿主机 check/test 会走到）。
    pub fn shell_open(path: &Path) -> Result<(), String> {
        open::that_detached(path).map_err(|e| e.to_string())
    }

    pub fn purge_items(paths: &[PathBuf], _hwnd: isize) -> bool {
        let Ok(items) = trash::os_limited::list() else {
            return false;
        };
        let hit: Vec<_> = items
            .into_iter()
            .filter(|it| {
                paths.iter().any(|p| {
                    std::path::Path::new(&it.id) == p.as_path()
                        || it.original_path().as_path() == p.as_path()
                })
            })
            .collect();
        if hit.is_empty() {
            return true;
        }
        trash::os_limited::purge_all(hit).is_ok()
    }

    pub fn modal_active() -> bool {
        false
    }


    pub struct ModalGuard;

    pub fn modal_guard() -> ModalGuard {
        ModalGuard
    }
    pub fn shell_copy(
        _from: &[PathBuf],
        _dest_dir: &Path,
        _hwnd: isize,
    ) -> Result<bool, String> {
        Err("原生 Shell 操作仅在 Windows 支持".into())
    }
    pub fn shell_move(
        _from: &[PathBuf],
        _dest_dir: &Path,
        _hwnd: isize,
    ) -> Result<bool, String> {
        Err("原生 Shell 操作仅在 Windows 支持".into())
    }
    pub fn shell_delete(
        _from: &[PathBuf],
        _to_trash: bool,
        _hwnd: isize,
    ) -> Result<bool, String> {
        Err("原生 Shell 操作仅在 Windows 支持".into())
    }
    pub fn show_properties(_parsing_name: &Path) -> Result<(), String> {
        Err("属性对话框仅在 Windows 支持".into())
    }
}

#[cfg(not(windows))]
pub use imp::*;
