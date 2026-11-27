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

    /// 一条还原请求：回收站里的条目 + 它删除前所在的目录和名字。
    pub struct RestoreRequest {
        /// 条目在回收站里的 desktop-absolute parsing name（`trash` 的 `id`）。
        pub item: PathBuf,
        /// 删除前所在的目录。
        pub parent: PathBuf,
        /// 删除前的名字。
        pub name: std::ffi::OsString,
    }

    /// 还原回收站条目：走 `IFileOperation`（资源管理器「还原」内部就是它），
    /// 关键是**不设 `FOF_NO_UI`**。
    ///
    /// `trash` crate 的 `restore_all` 写死了 `FOF_NO_UI | FOFX_EARLYFAILURE`，
    /// 原位置有同名文件时只会静默失败；这里放开 UI，多个条目共用一个还原进度框，
    /// 冲突时由 Windows 弹「替换 / 跳过 / 比较文件信息」。
    ///
    /// **会阻塞**（进度框是模态的），调用方必须在后台线程上跑。
    /// 返回值只反映「有没有排队成功 / 整批操作有没有报错」，逐条的真实结果由
    /// 调用方按「条目还在不在回收站里」复核。
    /// 返回 `(用户在复制引擎里点了取消, 逐条结果)`。
    pub fn restore_recycle_items(
        requests: &[RestoreRequest],
        hwnd: isize,
    ) -> (bool, Vec<(PathBuf, Result<(), String>)>) {
        use std::os::windows::ffi::OsStrExt;
        use windows::core::PCWSTR;
        use windows::Win32::Foundation::HWND;
        use windows::Win32::System::Com::{
            CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_APARTMENTTHREADED,
        };
        use windows::Win32::UI::Shell::{
            FileOperation, IFileOperation, IShellItem, SHCreateItemFromParsingName,
            FOF_NOCONFIRMMKDIR,
        };

        // 后台线程：自己初始化成 STA（Shell 的进度框 / 复制引擎要求）。
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok();
        }

        let wide = |text: &std::ffi::OsStr| -> Vec<u16> {
            text.encode_wide().chain(Some(0)).collect()
        };

        let op: IFileOperation = match unsafe { CoCreateInstance(&FileOperation, None, CLSCTX_ALL) }
        {
            Ok(op) => op,
            Err(e) => {
                let msg = format!("创建文件操作失败（0x{:08X}）", e.code().0 as u32);
                return (
                    false,
                    requests.iter().map(|r| (r.item.clone(), Err(msg.clone()))).collect(),
                );
            }
        };
        // 只关掉「是否新建文件夹」的确认框（原目录被删掉时才有；资源管理器也
        // 是不问的）。进度框、冲突框（替换 / 跳过 / 比较文件信息）一概保留。
        unsafe {
            let _ = op.SetOperationFlags(FOF_NOCONFIRMMKDIR);
            let _ = op.SetOwnerWindow(HWND(hwnd as *mut core::ffi::c_void));
        }

        let mut out: Vec<(PathBuf, Result<(), String>)> = Vec::with_capacity(requests.len());
        for request in requests {
            let queued = (|| -> windows::core::Result<()> {
                let item_wide = wide(request.item.as_os_str());
                let parent_wide = wide(request.parent.as_os_str());
                let name_wide = wide(&request.name);
                let src: IShellItem =
                    unsafe { SHCreateItemFromParsingName(PCWSTR(item_wide.as_ptr()), None) }?;
                let dest: IShellItem =
                    unsafe { SHCreateItemFromParsingName(PCWSTR(parent_wide.as_ptr()), None) }?;
                unsafe { op.MoveItem(&src, &dest, PCWSTR(name_wide.as_ptr()), None) }
            })();
            match queued {
                Ok(()) => out.push((request.item.clone(), Ok(()))),
                Err(e) => {
                    let msg = format!("排队还原失败（0x{:08X}）", e.code().0 as u32);
                    crate::logal!("!! [restore] {} 失败：{msg}", request.item.display());
                    out.push((request.item.clone(), Err(msg)));
                }
            }
        }

        let mut aborted = false;
        if out.iter().any(|(_, result)| result.is_ok()) {
            match unsafe { op.PerformOperations() } {
                Ok(()) => crate::logv!("[restore] Shell 还原完成 n={}", requests.len()),
                Err(e) => {
                    // 用户在进度框里点「取消」也走这里（HRESULT 是复制引擎的错误）。
                    let msg = format!("还原被中止（0x{:08X}）", e.code().0 as u32);
                    crate::logal!("!! [restore] {msg}");
                    for (_, result) in out.iter_mut() {
                        if result.is_ok() {
                            *result = Err(msg.clone());
                        }
                    }
                }
            }
            // 复制引擎自己的「用户取消」标记：只有它能区分「取消」和「失败」，
            // 两者的后果完全不同 —— 取消要把历史记录留着，失败要扔掉。
            aborted = matches!(unsafe { op.GetAnyOperationsAborted() }, Ok(flag) if flag.as_bool());
            if aborted {
                crate::logv!("[restore] 用户在还原进度框里点了取消");
            }
        }
        (aborted, out)
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
        invoke_verb_on_idlist(parsing_name, "properties", "属性")
    }

    /// 弹系统「打开方式…」选择器。
    ///
    /// 走 Shell 的 `openas` 动词：跟资源管理器右键「打开方式…」拉起来的是同一个
    /// 对话框（Win10/11 是 `OpenWith.exe`），能选已注册程序，也能「在电脑上查找
    /// 其他应用」。**默认关联改不了**：Windows 禁止程序写 UserChoice，「始终使用」
    /// 那个勾选只能由系统对话框自己落盘。
    pub fn show_open_with(path: &Path) -> Result<(), String> {
        invoke_verb_on_idlist(path, "openas", "「打开方式」")
    }

    /// 解出快捷方式（`.lnk`）指向的目标路径。
    ///
    /// 会碰磁盘 + COM（网络盘上的 .lnk 解析还能卡住），调用方必须放到后台线程上。
    /// 目标不存在时**不报错**：`GetPath` 照样能拿到记录的路径，由调用方去判断
    /// 「到底还在不在」，这样坏快捷方式也能给用户一句「目标已不存在」而不是
    /// 一个 COM 错误码。
    pub fn shortcut_target(link_path: &Path) -> Result<PathBuf, String> {
        use std::os::windows::ffi::OsStrExt;
        use windows::core::Interface as _;
        use windows::core::PCWSTR;
        use windows::Win32::System::Com::{
            CoCreateInstance, CoInitializeEx, CoUninitialize, IPersistFile, CLSCTX_INPROC_SERVER,
            COINIT_APARTMENTTHREADED,
        };
        use windows::Win32::UI::Shell::{IShellLinkW, ShellLink, SLGP_RAWPATH};

        let initialized = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok();
        let result = (|| -> Result<PathBuf, String> {
            let link: IShellLinkW =
                unsafe { CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER) }
                    .map_err(|e| format!("创建 Shell 链接失败（0x{:08X}）", e.code().0 as u32))?;
            let file: IPersistFile = link
                .cast()
                .map_err(|_| "Shell 链接不支持 IPersistFile".to_string())?;
            let wide: Vec<u16> = link_path.as_os_str().encode_wide().chain([0]).collect();
            unsafe { file.Load(PCWSTR(wide.as_ptr()), windows::Win32::System::Com::STGM_READ) }
                .map_err(|e| format!("读取快捷方式失败（0x{:08X}）", e.code().0 as u32))?;
            let mut buf = [0u16; 1024];
            let mut find = unsafe { std::mem::zeroed() };
            // RAWPATH：拿记录里的原路径，不去做「按名字在磁盘上搜一遍」的解析，
            // 免得在慢盘上卡好几秒；存不存在由调用方判断。
            // 参数是 u32（SLGP_FLAGS 只是指定位掩码的枚举）。
            unsafe { link.GetPath(&mut buf, &mut find, SLGP_RAWPATH.0 as u32) }
                .map_err(|e| format!("解析快捷方式目标失败（0x{:08X}）", e.code().0 as u32))?;
            let len = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
            if len == 0 {
                return Err("这个快捷方式没有记录目标".to_string());
            }
            let text = String::from_utf16_lossy(&buf[..len]);
            // 有些 .lnk 存的是带环境变量的形式（%SystemRoot%\…），这里展开它。
            let expanded = expand_env(&text);
            Ok(PathBuf::from(expanded.unwrap_or(text)))
        })();
        if initialized {
            unsafe { CoUninitialize() };
        }
        result
    }

    /// 展开 `%VAR%` 形式的路径（不进注册表，只查进程环境变量）。
    fn expand_env(text: &str) -> Option<String> {
        if !text.contains('%') {
            return None;
        }
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(start) = rest.find('%') {
            out.push_str(&rest[..start]);
            let after = &rest[start + 1..];
            let Some(end) = after.find('%') else {
                out.push_str(&rest[start..]);
                return Some(out);
            };
            let name = &after[..end];
            match std::env::var(name) {
                Ok(value) => out.push_str(&value),
                // 认不出来的变量原样保留，别把路径拼坏。
                Err(_) => {
                    out.push('%');
                    out.push_str(name);
                    out.push('%');
                }
            }
            rest = &after[end + 1..];
        }
        out.push_str(rest);
        Some(out)
    }

    /// 把路径解析成绝对 PIDL，再用 `SEE_MASK_INVOKEIDLIST` 调一个 Shell 动词。
    ///
    /// 走 IDList 而不是 `lpFile`：回收站的条目只有 parsing name（`$R` 临时文件
    /// 的真实路径不存在），只有 PIDL 这条路能让回收站自己的 Shell 扩展接住。
    fn invoke_verb_on_idlist(parsing_name: &Path, verb_name: &str, what: &str) -> Result<(), String> {
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
        let verb: Vec<u16> = verb_name.encode_utf16().chain([0]).collect();
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
                return Err(format!("Shell 未能打开{what}对话框"));
            }
        }
        Ok(())
    }

    /// 给一组目标建快捷方式（`.lnk`）。
    ///
    /// `to_desktop = true` → 全建到桌面；否则建在各自所在目录。
    /// `suffix` 是文件名里的那半截（「<名字> - 快捷方式.lnk」），跟界面语言走。
    /// 会碰磁盘 + COM，调用方必须放到后台线程上跑。
    pub fn create_shortcuts(
        paths: &[PathBuf],
        to_desktop: bool,
        suffix: &str,
    ) -> Result<usize, String> {
        use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};

        if paths.is_empty() {
            return Err("没有选中项".into());
        }
        let desktop = if to_desktop {
            Some(desktop_dir()?)
        } else {
            None
        };
        // 线程上可能已经初始化过 COM（S_FALSE 也算成功，要配对 CoUninitialize）；
        // 只有真的初始化成功才反初始化，否则会把别人的引用计数弄乱。
        let initialized = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok();
        let mut made = 0usize;
        let mut last_err = String::new();
        for target in paths {
            match unsafe { make_one_shortcut(target, desktop.as_deref(), suffix) } {
                Ok(_) => made += 1,
                Err(e) => last_err = e,
            }
        }
        if initialized {
            unsafe { CoUninitialize() };
        }
        if made == 0 {
            return Err(if last_err.is_empty() {
                "没有可创建的目标".to_string()
            } else {
                last_err
            });
        }
        Ok(made)
    }

    /// 桌面目录的真实文件路径（不是虚拟命名空间）。
    fn desktop_dir() -> Result<PathBuf, String> {
        use windows::Win32::System::Com::CoTaskMemFree;
        use windows::Win32::UI::Shell::{SHGetKnownFolderPath, FOLDERID_Desktop};

        unsafe {
            let raw = SHGetKnownFolderPath(&FOLDERID_Desktop, Default::default(), None)
                .map_err(|e| format!("拿不到桌面路径（0x{:08X}）", e.code().0 as u32))?;
            let text = raw.to_string().unwrap_or_default();
            CoTaskMemFree(Some(raw.0 as *const std::ffi::c_void));
            if text.is_empty() {
                return Err("拿不到桌面路径".to_string());
            }
            Ok(PathBuf::from(text))
        }
    }

    /// 建一个 `.lnk`，返回落盘路径。
    unsafe fn make_one_shortcut(
        target: &Path,
        desktop: Option<&Path>,
        suffix: &str,
    ) -> Result<PathBuf, String> {
        use windows::core::{Interface, PCWSTR};
        use windows::Win32::System::Com::{CoCreateInstance, IPersistFile, CLSCTX_INPROC_SERVER};
        use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};

        fn wide(p: &Path) -> Vec<u16> {
            use std::os::windows::ffi::OsStrExt as _;
            p.as_os_str().encode_wide().chain([0]).collect()
        }

        let dir = desktop
            .map(|d| d.to_path_buf())
            .or_else(|| target.parent().map(|p| p.to_path_buf()))
            .ok_or_else(|| "找不到目标所在文件夹".to_string())?;
        let name = target
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| target.to_string_lossy().into_owned());
        // 盘符根（`C:\`）没有 file_name，整串里有 `:` `\` 这些非法字符，
        // 不换掉的话 IPersistFile::Save 会直接失败。换成 `_` 至少能建出来。
        let name: String = name
            .chars()
            .map(|c| if "\\/:*?\"<>|".contains(c) { '_' } else { c })
            .collect();
        // 资源管理器的命名规则：「<原名> - 快捷方式.lnk」，重名就加「(2)」。
        let stem = format!("{name} - {suffix}");
        let mut link_path = dir.join(format!("{stem}.lnk"));
        let mut n = 2u32;
        while link_path.exists() {
            if n > 999 {
                return Err("同名快捷方式太多了".to_string());
            }
            link_path = dir.join(format!("{stem} ({n}).lnk"));
            n += 1;
        }

        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)
            .map_err(|e| format!("创建 Shell 链接失败（0x{:08X}）", e.code().0 as u32))?;
        let target_w = wide(target);
        link.SetPath(PCWSTR(target_w.as_ptr()))
            .map_err(|e| format!("设置快捷方式目标失败（0x{:08X}）", e.code().0 as u32))?;
        // 可执行文件双击时资源管理器会把工作目录设成它所在文件夹，这里跟着一致
        // （否则程序会以 %SystemRoot%\System32 之类的目录为工作目录启动）。
        if !target.is_dir() {
            if let Some(parent) = target.parent() {
                let parent_w = wide(parent);
                let _ = link.SetWorkingDirectory(PCWSTR(parent_w.as_ptr()));
            }
        }
        let file: IPersistFile = link
            .cast()
            .map_err(|_| "Shell 链接不支持 IPersistFile".to_string())?;
        let link_w = wide(&link_path);
        file.Save(PCWSTR(link_w.as_ptr()), true)
            .map_err(|e| format!("写入快捷方式失败（0x{:08X}）", e.code().0 as u32))?;
        Ok(link_path)
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
    /// 非 Windows：只为让上层代码能编译（真正的还原只有 Windows 有）。
    pub struct RestoreRequest {
        pub item: PathBuf,
        pub parent: PathBuf,
        pub name: std::ffi::OsString,
    }

    pub fn restore_recycle_items(
        requests: &[RestoreRequest],
        _hwnd: isize,
    ) -> (bool, Vec<(PathBuf, Result<(), String>)>) {
        (
            false,
            requests
                .iter()
                .map(|r| (r.item.clone(), Err("当前平台不支持还原".to_string())))
                .collect(),
        )
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
    pub fn show_open_with(_path: &Path) -> Result<(), String> {
        Err("「打开方式」对话框仅在 Windows 支持".into())
    }
    pub fn create_shortcuts(
        _paths: &[PathBuf],
        _to_desktop: bool,
        _suffix: &str,
    ) -> Result<usize, String> {
        Err("创建快捷方式仅在 Windows 支持".into())
    }
    pub fn shortcut_target(_link_path: &Path) -> Result<PathBuf, String> {
        Err("解析快捷方式仅在 Windows 支持".into())
    }
}

#[cfg(not(windows))]
pub use imp::*;

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    /// 真建一个 `.lnk` 再解回目标路径：验证 COM 那条链是通的。
    #[test]
    fn shortcut_target_round_trips() {
        let dir = std::env::temp_dir().join(format!("rove-lnk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let target = dir.join("target-file.txt");
        std::fs::write(&target, b"x").expect("写目标文件");
        // 建快捷方式不带扩展名参数：让它按默认后缀自己落到目录里。
        create_shortcuts(&[target.clone()], false, " - 快捷方式").expect("建快捷方式");
        let link = std::fs::read_dir(&dir)
            .expect("读目录")
            .flatten()
            .map(|e| e.path())
            .find(|p| {
                p.extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("lnk"))
            })
            .expect("找不到生成的 .lnk");
        let got = shortcut_target(&link).expect("解析快捷方式");
        assert_eq!(
            got.to_string_lossy().to_lowercase(),
            target.to_string_lossy().to_lowercase()
        );
        // 目标删掉之后仍然要能解析出路径（由上层判断「已不存在」）。
        std::fs::remove_file(&target).expect("删目标");
        let gone = shortcut_target(&link).expect("目标不在也要解析出来");
        assert_eq!(
            gone.to_string_lossy().to_lowercase(),
            target.to_string_lossy().to_lowercase()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
