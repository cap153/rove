//! 把 Rove 的每个「正在显示目录」的窗格登记成 **Shell 窗口**。
//!
//! 为什么需要它：浏览器、下载器、微信的「打开文件所在位置」调的是
//! `SHOpenFolderAndSelectItems`。这个 API 的选中信息只走 pidl、**不进命令行**
//! （所以命令行里根本抓不到 `/select,`），Shell 的做法是：
//!
//! 1. 在 `IShellWindows` 里找一个「正显示该目录」的窗口；
//! 2. 找到就通过该窗口 `Document` 的 `IServiceProvider` 拿到 `IShellView`，
//!    调 `IShellView::SelectItem(子 pidl, SVSIF)`；
//! 3. 找不到才超时后自己拉起资源管理器（用户看到的就是「半分钟后冒出一个
//!    资源管理器窗口」）。
//!
//! 所以只要我们自己往 `IShellWindows` 登记，第 2 步就会直接调回我们 ——
//! 不用注入别人的进程，也不用去偷资源管理器的窗口。
//!
//! 结构与 Pulse（`src/app/shell_window_registry.{h,cpp}`）一致：
//!
//! - 每个窗格一套 `IWebBrowserApp`：只有 `HWND` / `LocationURL` /
//!   `LocationName` / `Document` 有意义，其余全部 `E_NOTIMPL`；
//! - `Document` 是 `IDispatch` + `IServiceProvider`，`QueryService` 认
//!   `IID_IShellView`（也顺带认 `IID_IFolderView`，不同调用方问的不一样）；
//! - `IShellView` 里**只有 `SelectItem` 有实现**：把子 pidl 和目录 pidl
//!   拼成完整路径，交给界面线程去选中；
//! - 同一条目录按**两种 pidl 形态**各登记一次（`ILCreateFromPathW` 的
//!   「此电脑\C:\…」和 `SHParseDisplayName` 的「下载\…」）。Shell 逐字节比较
//!   pidl，Chromium/Electron 用后者，别的程序用前者，只登记一种会有一半程序
//!   找不到窗口。
//!
//! 线程模型：所有 `IShellWindows` 调用都在本模块自己的 STA 线程上；界面线程
//! 只调 [`publish`] 交一份「哪些窗格该登记」的快照（路径没变就不会唤醒后台
//! 线程），从不等待 Shell。

use std::path::PathBuf;

/// 一个「该登记成 Shell 窗口」的窗格。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaneRegistration {
    /// 窗格身份：标签号 + 栏号，换目录时复用同一个 key。
    pub key: u64,
    /// 窗格正在显示的目录（只放真实文件系统路径）。
    pub path: PathBuf,
    /// 承载它的顶层窗口（`HWND` 的整数值）；Shell 用它来激活窗口。
    pub hwnd: isize,
}

/// 界面线程：发布当前应该登记的窗格集合。
///
/// 与上次发布的内容相同就直接返回；不同才唤醒后台线程。
pub fn publish(panes: Vec<PaneRegistration>) {
    imp::publish(panes);
}

/// 在窗口还没建出来之前，先向 Shell「预定」一个位置。
///
/// 「打开文件所在位置」的调用方（浏览器/IDM/微信）只等很短一会儿就要么成功、
/// 要么自己拉资源管理器。冷启动时我们光是把进程、窗口、会话建起来就要几百
/// 毫秒，Shell 很可能已经放弃（表现为选不中，或者半分钟后冒出资源管理器）。
///
/// `RegisterPending` 就是给这种情况准备的：它只要「目录 pidl + 将来承载窗口的
/// 线程 id」，不需要窗口存在，等于告诉 Shell「这个目录的窗口马上就来，等我」。
/// 目录已知、窗口还没有的那一刻调用它（见 `run()` 里的 `--shell-open`）。
pub fn reserve(path: PathBuf) {
    imp::reserve(path);
}

/// 设置「Shell 要求选中某个条目」的投递口：参数是条目的完整路径。
///
/// 会在后台线程（可能还是 Shell 的线程）被调用，实现方必须立刻返回、
/// 不能在这里碰界面状态。
pub fn set_select_sink(sink: impl Fn(PathBuf) + Send + Sync + 'static) {
    imp::set_select_sink(sink);
}

// ---------------------------------------------------------------------------
// Windows 实现
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod imp {
    use super::PaneRegistration;
    // ---- 诊断 ----
    //
    // 两级：`sw!` 是「有事发生」（选中请求往来、异常），`swv!` 是例行流量
    // （登记、Shell 查询往来），后者只在 `ROVE_LOG=2` 时记。
    //
    // 曾经 `sw!` 是无条件写盘的：接管进程不继承 `ROVE_LOG`，冷启动那种
    // 「父进程是浏览器」的场景开日志很麻烦。现在那个需求由 exe 同目录的
    // `rove.verbose` 标记文件承担（见 `app::log_level`），常态保持安静。
    //
    /// 需要看一眼才记的事：选中请求的来去、异常。
    macro_rules! sw {
        ($($arg:tt)*) => {
            crate::logln!("[shell-windows] {}", format_args!($($arg)*))
        };
    }

    /// 常规流量（登记/查询往来）：只在 `ROVE_LOG=2` 时记。
    macro_rules! swv {
        ($($arg:tt)*) => {
            crate::logv!("[shell-windows] {}", format_args!($($arg)*))
        };
    }

    use std::ffi::c_void;
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, OnceLock};

    use windows::core::{implement, BSTR, GUID, HRESULT, PCWSTR, PWSTR, Ref};
    use windows::Win32::Foundation::{
        BOOL, DISP_E_BADINDEX, DISP_E_MEMBERNOTFOUND, DISP_E_UNKNOWNNAME, E_FAIL, E_INVALIDARG,
        E_NOINTERFACE, E_NOTIMPL, E_OUTOFMEMORY, E_POINTER, HWND, S_FALSE, SHANDLE_PTR, VARIANT_BOOL,
        VARIANT_TRUE,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CoTaskMemFree, IDispatch, IDispatch_Impl,
        IServiceProvider, IServiceProvider_Impl, ITypeInfo, CLSCTX_ALL, COINIT_APARTMENTTHREADED,
        DISPATCH_FLAGS, DISPPARAMS, EXCEPINFO,
    };
    use windows::Win32::System::Ole::IOleWindow_Impl;
    use windows::Win32::System::Variant::{InitVariantFromBuffer, VariantClear, VARIANT};
    use windows::Win32::UI::Shell::{
        ILCreateFromPathW, ILCombine, ILGetSize, IShellBrowser, IShellBrowser_Impl, IShellView,
        IShellView_Impl, IShellWindows, IWebBrowserApp, IWebBrowserApp_Impl, IWebBrowser_Impl,
        SHGetNameFromIDList, SHParseDisplayName, ShellWindows, UrlCreateFromPathW, FOLDERSETTINGS,
        SIGDN, SIGDN_DESKTOPABSOLUTEPARSING, SWC_BROWSER,
    };
    use windows::Win32::UI::Shell::Common::ITEMIDLIST;
    use windows::Win32::System::Threading::{CreateEventW, GetCurrentThreadId, SetEvent};
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetWindowThreadProcessId, IsWindowVisible,
        MsgWaitForMultipleObjectsEx, PeekMessageW, TranslateMessage, MSG, MWMO_INPUTAVAILABLE,
        PM_REMOVE, QS_ALLINPUT,
    };

    /// Shell 传来的「请选中这个条目」出口。
    type Sink = Box<dyn Fn(PathBuf) + Send + Sync>;
    static SINK: OnceLock<Sink> = OnceLock::new();

    pub fn set_select_sink(sink: impl Fn(PathBuf) + Send + Sync + 'static) {
        // 多次会话复用同一个进程：后来者不覆盖（模板一致，语义相同）。
        let _ = SINK.set(Box::new(sink));
    }

    fn deliver_select(path: PathBuf) {
        match SINK.get() {
            Some(sink) => sink(path),
            None => sw!("!! 收到选中请求但没有投递口：{path:?}"),
        }
    }

    // ---- 发布快照 + 后台线程 ----

    static DESIRED: Mutex<Vec<PaneRegistration>> = Mutex::new(Vec::new());
    /// 快照版本号：内容变了才 +1，后台线程据此决定要不要重新登记
    /// （否则 2 秒一次的超时唤醒会把整张表重扫一遍）。
    static REVISION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    /// 自动重置事件：发布新快照后 Set，后台线程立刻看一眼。
    static WAKE: Mutex<Option<isize>> = Mutex::new(None);
    static STARTED: OnceLock<()> = OnceLock::new();

    /// 待预定的目录 + 将来承载窗口的线程 id（见 [`reserve`]）。
    static RESERVE: Mutex<Option<(PathBuf, u32)>> = Mutex::new(None);
    /// 已经 `RegisterPending`、还没真正 `Register` 的位置：
    /// (目录, 是否 alias 形态, cookie, 登记时刻)。
    static PENDING: Mutex<Vec<(PathBuf, bool, i32, std::time::Instant)>> = Mutex::new(Vec::new());

    /// 冷启动时「马上就有窗口」的预定（界面线程调用，真正执行在 STA 线程）。
    pub fn reserve(path: PathBuf) {
        if path.as_os_str().is_empty() {
            return;
        }
        // 预定要带上「窗口将出现在哪个线程」——此刻窗口还不存在，就是界面线程。
        let thread_id = unsafe { GetCurrentThreadId() };
        if let Ok(mut slot) = RESERVE.lock() {
            *slot = Some((path, thread_id));
        }
        ensure_worker();
        wake();
    }

    fn wake() {
        if let Ok(slot) = WAKE.lock() {
            if let Some(event) = slot.as_ref() {
                unsafe {
                    let _ = SetEvent(windows::Win32::Foundation::HANDLE(*event as *mut c_void));
                }
            }
        }
    }

    pub fn publish(panes: Vec<PaneRegistration>) {
        {
            let mut desired = match DESIRED.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            if *desired == panes {
                return;
            }
            *desired = panes;
        }
        REVISION.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        ensure_worker();
        wake();
    }

    fn ensure_worker() {
        if STARTED.get().is_some() {
            return;
        }
        let event = unsafe { CreateEventW(None, false, false, None) };
        if event.is_err() {
            sw!("!! 建事件失败，登记功能停用");
            return;
        }
        let event = event.unwrap();
        if let Ok(mut slot) = WAKE.lock() {
            *slot = Some(event.0 as isize);
        }
        let handle = event.0 as isize;
        let spawned = std::thread::Builder::new()
            .name("rove-shell-windows".into())
            .spawn(move || worker(windows::Win32::Foundation::HANDLE(handle as *mut c_void)));
        match spawned {
            Ok(_) => {
                let _ = STARTED.set(());
            }
            Err(err) => {
                sw!("!! 后台线程启动失败：{err}");
                if let Ok(mut slot) = WAKE.lock() {
                    *slot = None;
                }
            }
        }
    }

    fn current_desired() -> Vec<PaneRegistration> {
        match DESIRED.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    fn worker(event: windows::Win32::Foundation::HANDLE) {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        }
        let windows: Option<IShellWindows> = unsafe {
            CoCreateInstance(&ShellWindows, None, CLSCTX_ALL).ok()
        };
        let Some(windows) = windows else {
            sw!("!! 取不到 IShellWindows，登记功能停用");
            unsafe { CoUninitialize() };
            return;
        };
        swv!("后台线程就绪（STA + IShellWindows）");
        let mut registered: Vec<Registered> = Vec::new();
        let mut applied = 0u64;
        let mut first_sync = true;
        // 先按一次快照再等唤醒：窗口建好之前就发布的窗格不会漏掉。
        // 这个线程与进程同寿（没有退出路径），所以不用清理。
        loop {
            // 「马上就有窗口」的预定排在最前面：它就是为了赶在 Shell 放弃之前。
            if let Some((path, thread_id)) = RESERVE.lock().ok().and_then(|mut slot| slot.take()) {
                reserve_pending(&windows, &path, thread_id);
            }
            expire_pending(&windows);
            let revision = REVISION.load(std::sync::atomic::Ordering::SeqCst);
            if revision != applied {
                applied = revision;
                let desired = current_desired();
                apply(&windows, &desired, &mut registered);
                // 无条件记一行「第一次同步的结果」：冷启动排查全靠它 ——
                // 它出现说明登记链路跑到了，条数为 0 说明 Shell 还没认我们，
                // 而它完全不出现说明登记根本没被触发。
                // 只在真的开始登记时才记这一行：窗格还没建出来（0 个）时
                // 它只会误导人。
                if first_sync && !desired.is_empty() {
                    first_sync = false;
                    sw!(
                        "[first-sync] 窗格 {} 个，登记成功 {} 条",
                        desired.len(),
                        registered.len()
                    );
                }
            }
            // 必须泵消息：Shell 把 `IShellView::SelectItem` 通过跨公寓调用
            // 投递到本线程，本线程不泵就会一直挂着（浏览器那边表现为卡住）。
            unsafe {
                MsgWaitForMultipleObjectsEx(
                    Some(&[event]),
                    2_000,
                    QS_ALLINPUT,
                    MWMO_INPUTAVAILABLE,
                );
                let mut msg = MSG::default();
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        }
    }

    /// 一条已登记的记录（一个窗格的一种 pidl 形态）。
    struct Registered {
        key: u64,
        path: PathBuf,
        /// 承载窗口。窗口被销毁重建后句柄会变，必须跟着换（否则 Shell 激活
        /// 的是已经不存在的那一个）。
        hwnd: isize,
        /// 这条记录用的 pidl 形态（primary / alias），见 [`folder_pidl`]。
        alias: bool,
        cookie: i32,
        /// 持有对象本身：`Revoke` 之后才能释放，否则 Shell 手里可能还拿着指针。
        app: IWebBrowserApp,
    }

    /// 真的去「预定」：两种 pidl 形态各一条（Shell 逐字节比较 IDList，
    /// 不同调用方给的形态不一样，见 [`folder_pidl`]）。
    fn reserve_pending(windows: &IShellWindows, path: &Path, thread_id: u32) {
        for alias in [false, true] {
            let Some(pidl) = folder_pidl(path, alias) else {
                continue;
            };
            let mut location =
                match unsafe { InitVariantFromBuffer(pidl as *const c_void, ILGetSize(Some(pidl))) } {
                    Ok(variant) => variant,
                    Err(err) => {
                        sw!("!! 预定：目录变体生成失败 {err} path={path:?}");
                        unsafe { CoTaskMemFree(Some(pidl as *const c_void)) };
                        continue;
                    }
                };
            unsafe { CoTaskMemFree(Some(pidl as *const c_void)) };
            let root = VARIANT::default();
            match unsafe { windows.RegisterPending(thread_id as i32, &location, &root, SWC_BROWSER) } {
                Ok(cookie) => {
                    if let Ok(mut slot) = PENDING.lock() {
                        slot.push((path.to_path_buf(), alias, cookie, std::time::Instant::now()));
                    }
                    swv!("预定 path={path:?} alias={alias} cookie={cookie}");
                }
                Err(err) => sw!("!! 预定失败 path={path:?} alias={alias} err={err}"),
            }
            let _ = unsafe { VariantClear(&mut location) };
        }
    }

    /// 拿走与 `(path, alias)` 对应的预定 cookie：有就复用它作为「预定位置」，
    /// 不再重复预定一次（重复预定会在 Shell 里留下一条永远等不到的空位）。
    fn take_pending(path: &Path, alias: bool) -> Option<i32> {
        let mut slot = PENDING.lock().ok()?;
        let index = slot.iter().position(|(other, other_alias, _, _)| {
            *other_alias == alias && crate::fs::dir::same_path_loose(other, path)
        })?;
        Some(slot.remove(index).2)
    }

    /// 预定出去 15 秒还没被真正登记用上就撤掉（会话可能最终打开了别的目录），
    /// 否则 Shell 会一直以为「那个目录的窗口马上就来」。
    ///
    /// 注意「先在锁内摘出来、再放锁去 Revoke」：`Revoke` 是跨进程的 COM 调用
    /// （会进 Shell 的消息循环），不该在持有 `PENDING` 的情况下做。
    fn expire_pending(windows: &IShellWindows) {
        let expired: Vec<(PathBuf, bool, i32)> = {
            let Ok(mut slot) = PENDING.lock() else {
                return;
            };
            let now = std::time::Instant::now();
            let mut keep = Vec::with_capacity(slot.len());
            let mut expired = Vec::new();
            for entry in slot.drain(..) {
                if now.duration_since(entry.3).as_secs() >= 15 {
                    expired.push((entry.0, entry.1, entry.2));
                } else {
                    keep.push(entry);
                }
            }
            *slot = keep;
            expired
        };
        for (path, alias, cookie) in expired {
            if let Err(err) = unsafe { windows.Revoke(cookie) } {
                sw!("!! 撤消过期预定失败 cookie={cookie}: {err}");
            }
            swv!("预定过期撤消 path={path:?} alias={alias}");
        }
    }

    /// 让 Shell 里登记的内容与 `desired` 一致。
    fn apply(windows: &IShellWindows, desired: &[PaneRegistration], registered: &mut Vec<Registered>) {
        swv!(
            "同步快照: 应有 {} 个窗格，已登记 {} 条",
            desired.len(),
            registered.len()
        );
        // 撤掉不再需要的：窗格没了、换目录了、或者这条形态记录多余。
        let mut index = 0;
        while index < registered.len() {
            let still_wanted = desired.iter().any(|pane| {
                pane.key == registered[index].key
                    && pane.path == registered[index].path
                    && pane.hwnd == registered[index].hwnd
            });
            if still_wanted {
                index += 1;
            } else {
                let slot = registered.remove(index);
                revoke(windows, slot);
            }
        }
        // 补齐新窗格 / 换过目录的窗格，按两种 pidl 形态各登记一次。
        for pane in desired {
            for alias in [false, true] {
                if registered.iter().any(|slot| {
                    slot.key == pane.key
                        && slot.path == pane.path
                        && slot.hwnd == pane.hwnd
                        && slot.alias == alias
                }) {
                    continue;
                }
                if let Some(slot) = register(windows, pane, alias) {
                    registered.push(slot);
                }
            }
        }
    }

    fn register(windows: &IShellWindows, pane: &PaneRegistration, alias: bool) -> Option<Registered> {
        if pane.path.as_os_str().is_empty() {
            return None;
        }
        let pidl = folder_pidl(&pane.path, alias)?;
        unsafe {
            // 目录用 pidl（不是字符串）描述，Shell 逐字节比较它来找窗口。
            let mut location = match InitVariantFromBuffer(pidl as *const c_void, ILGetSize(Some(pidl)))
            {
                Ok(variant) => variant,
                Err(err) => {
                    sw!("!! 目录变体生成失败 {err}");
                    CoTaskMemFree(Some(pidl as *const c_void));
                    return None;
                }
            };
            CoTaskMemFree(Some(pidl as *const c_void));
            let root = VARIANT::default();
            let thread_id = GetWindowThreadProcessId(HWND(pane.hwnd as *mut c_void), None) as i32;
            // 先「预定」一个位置：这样 Shell 在等我们绑定时不会超时后自己
            // 拉起资源管理器（就是用户看到的「半分钟后冒出资源管理器」）。
            // 冷启动时 `reserve` 已经替这个目录预定过位置：直接复用它，
            // Shell 那边因此是「等到了」而不是「等超时」。
            let pending = match take_pending(&pane.path, alias) {
                Some(cookie) => {
                    swv!("复用预定 key={} path={:?} alias={alias} cookie={cookie}", pane.key, pane.path);
                    cookie
                }
                None => match windows.RegisterPending(thread_id, &location, &root, SWC_BROWSER) {
                    Ok(cookie) => cookie,
                    Err(err) => {
                        sw!(
                            "!! RegisterPending 失败 key={} path={:?} alias={} err={err}",
                            pane.key,
                            pane.path,
                            alias
                        );
                        let _ = VariantClear(&mut location);
                        return None;
                    }
                },
            };
            let view: IShellView = View::new(pane.hwnd, pane.path.display().to_string()).into();
            let browser: IShellBrowser = Browser::new(pane.hwnd, &view).into();
            let document: IDispatch = Document::new(&view, &browser).into();
            let app: IWebBrowserApp = BrowserApp::new(
                pane.hwnd,
                file_url(&pane.path),
                pane.path.display().to_string(),
                document,
            )
            .into();
            let cookie = match windows.Register(&app, pane.hwnd as i32, SWC_BROWSER) {
                Ok(cookie) => cookie,
                Err(err) => {
                    sw!(
                        "!! Register 失败 key={} path={:?} alias={} err={err}",
                        pane.key,
                        pane.path,
                        alias
                    );
                    let _ = windows.Revoke(pending);
                    let _ = VariantClear(&mut location);
                    return None;
                }
            };
            if cookie != pending {
                // 实测 Shell 不会把 `Register` 的 cookie 给成预定的那一个（它自己
                // 按自己的序号发号），所以这条分支在「提前预定」场景里是常态。
                // 关键动作是撤掉那条没用上的预定：否则 Shell 会一直以为「那个
                // 目录的窗口马上就来」，白等到超时。
                swv!(
                    "预定未绑定，撤消 pending={pending} cookie={cookie} path={:?}",
                    pane.path
                );
                let _ = windows.Revoke(pending);
            }
            if let Err(err) = windows.OnNavigate(cookie, &location) {
                sw!(
                    "!! OnNavigate 失败 key={} path={:?} alias={} err={}",
                    pane.key,
                    pane.path,
                    alias,
                    err
                );
                let _ = windows.Revoke(cookie);
                let _ = VariantClear(&mut location);
                return None;
            }
            let _ = VariantClear(&mut location);
            swv!(
                "登记 key={} path={:?} alias={} cookie={}",
                pane.key,
                pane.path,
                alias,
                cookie
            );
            Some(Registered {
                key: pane.key,
                path: pane.path.clone(),
                hwnd: pane.hwnd,
                alias,
                cookie,
                app,
            })
        }
    }

    fn revoke(windows: &IShellWindows, slot: Registered) {
        if let Err(err) = unsafe { windows.Revoke(slot.cookie) } {
            sw!("!! Revoke 失败 cookie={}: {err}", slot.cookie);
        }
        drop(slot.app);
    }

    /// 目录的 pidl：`ILCreateFromPathW`（「此电脑\C:\…」）与
    /// `SHParseDisplayName`（已知文件夹形态「下载\…」）两种。
    ///
    /// 两种形态字节不同，Shell 又逐字节比较，所以调用方按同一条目录登记两次。
    fn folder_pidl(path: &Path, alias: bool) -> Option<*mut ITEMIDLIST> {
        let wide: Vec<u16> = path.as_os_str().encode_wide_nt();
        unsafe {
            if !alias {
                let pidl = ILCreateFromPathW(PCWSTR(wide.as_ptr()));
                return (!pidl.is_null()).then_some(pidl);
            }
            let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
            match SHParseDisplayName(PCWSTR(wide.as_ptr()), None, &mut pidl, 0, None) {
                Ok(()) if !pidl.is_null() => Some(pidl),
                _ => None,
            }
        }
    }

    fn file_url(path: &Path) -> String {
        let wide: Vec<u16> = path.as_os_str().encode_wide_nt();
        let mut buffer = [0u16; 2084];
        let mut length = buffer.len() as u32;
        unsafe {
            match UrlCreateFromPathW(PCWSTR(wide.as_ptr()), PWSTR(buffer.as_mut_ptr()), &mut length, 0)
            {
                Ok(()) => String::from_utf16_lossy(&buffer[..(length as usize).min(buffer.len())])
                    .trim_end_matches('\0')
                    .to_string(),
                Err(_) => String::new(),
            }
        }
    }

    fn pidl_name(pidl: *const ITEMIDLIST, kind: SIGDN) -> String {
        unsafe {
            match SHGetNameFromIDList(pidl, kind) {
                Ok(name) => {
                    let text = name.to_string().unwrap_or_default();
                    CoTaskMemFree(Some(name.0 as *const c_void));
                    text
                }
                Err(_) => String::new(),
            }
        }
    }

    /// `OsStr` → 以 NUL 结尾的 UTF-16。
    trait WideNt {
        fn encode_wide_nt(&self) -> Vec<u16>;
    }

    impl WideNt for std::ffi::OsStr {
        fn encode_wide_nt(&self) -> Vec<u16> {
            use std::os::windows::ffi::OsStrExt;
            self.encode_wide().chain(std::iter::once(0)).collect()
        }
    }
    use windows::core::Interface as _;

    fn guid_name(guid: &GUID) -> String {
        format!("{guid:?}")
    }

    // ---- IShellView：只实现 SelectItem ----

    /// 一个窗格的 `IShellView`。
    ///
    /// 只有 `SelectItem` 会被 Shell 真正调用：它带着「相对目录的子 pidl」，
    /// 拼成完整 pidl 取出绝对路径，再交给界面线程。
    #[implement(IShellView)]
    struct View {
        hwnd: isize,
        folder: String,
    }

    impl View {
        fn new(hwnd: isize, folder: String) -> Self {
            Self { hwnd, folder }
        }
    }

    impl IOleWindow_Impl for View_Impl {
        fn GetWindow(&self) -> windows::core::Result<HWND> {
            Ok(HWND(self.hwnd as *mut c_void))
        }

        fn ContextSensitiveHelp(&self, _enter: BOOL) -> windows::core::Result<()> {
            Err(E_NOTIMPL.into())
        }
    }

    impl IShellView_Impl for View_Impl {
        fn TranslateAccelerator(&self, _msg: *const MSG) -> HRESULT {
            S_FALSE
        }

        fn EnableModeless(&self, _enable: BOOL) -> windows::core::Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn UIActivate(&self, _state: u32) -> windows::core::Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn Refresh(&self) -> windows::core::Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn CreateViewWindow(
            &self,
            _previous: Ref<'_, IShellView>,
            _settings: *const FOLDERSETTINGS,
            _browser: Ref<'_, IShellBrowser>,
            _rect: *const windows::Win32::Foundation::RECT,
        ) -> windows::core::Result<HWND> {
            Err(E_NOTIMPL.into())
        }

        fn DestroyViewWindow(&self) -> windows::core::Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn GetCurrentInfo(&self) -> windows::core::Result<FOLDERSETTINGS> {
            Err(E_NOTIMPL.into())
        }

        fn AddPropertySheetPages(
            &self,
            _reserved: u32,
            _page: windows::Win32::UI::Controls::LPFNSVADDPROPSHEETPAGE,
            _lparam: windows::Win32::Foundation::LPARAM,
        ) -> windows::core::Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn SaveViewState(&self) -> windows::core::Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn SelectItem(&self, item: *const ITEMIDLIST, flags: u32) -> windows::core::Result<()> {
            swv!("SelectItem folder={:?} flags=0x{flags:x}", self.folder);
            // SVSI_DESELECT(0) 单独来一次没有可显示的东西。
            const INTERESTING: u32 = 1 | 3 | 8 | 16; // SELECT | EDIT | ENSUREVISIBLE | FOCUSED
            if flags & INTERESTING == 0 {
                swv!("SelectItem: 没有任何「要显示选中」的标志，忽略");
                return Ok(());
            }
            if item.is_null() || self.folder.is_empty() {
                sw!("!! SelectItem: 参数不完整（item 空或目录空）");
                return Err(E_INVALIDARG.into());
            }
            let wide: Vec<u16> = std::ffi::OsStr::new(&self.folder).encode_wide_nt();
            unsafe {
                let folder = ILCreateFromPathW(PCWSTR(wide.as_ptr()));
                if folder.is_null() {
                    sw!("!! SelectItem: 目录 pidl 生成失败 {:?}", self.folder);
                    return Err(E_FAIL.into());
                }
                let full = ILCombine(Some(folder), Some(item));
                CoTaskMemFree(Some(folder as *const c_void));
                if full.is_null() {
                    sw!("!! SelectItem: ILCombine 失败");
                    return Err(E_OUTOFMEMORY.into());
                }
                let text = pidl_name(full, SIGDN_DESKTOPABSOLUTEPARSING);
                CoTaskMemFree(Some(full as *const c_void));
                if text.is_empty() {
                    sw!("!! SelectItem: 拼出的 pidl 取不到路径");
                    return Err(E_FAIL.into());
                }
                sw!("SelectItem -> 选中 {text}");
                deliver_select(PathBuf::from(text));
            }
            Ok(())
        }

        fn GetItemObject(
            &self,
            _item: &windows::Win32::UI::Shell::_SVGIO,
            _riid: *const GUID,
            _out: *mut *mut c_void,
        ) -> windows::core::Result<()> {
            Err(E_NOTIMPL.into())
        }
    }

    // ---- IShellBrowser：只实现 QueryActiveShellView ----

    /// Shell 有两条路找「正在显示该目录的视图」：
    ///
    /// 1. `Document` 的 `IServiceProvider::QueryService(IID_IShellView/IFolderView)`
    ///    → 直接要视图；
    /// 2. `QueryService(SID_SShellBrowser, IID_IShellBrowser)` → 要浏览器对象，
    ///    再 `QueryActiveShellView()` 拿视图（`SHOpenFolderAndSelectItems` 就是这条）。
    ///
    /// 两条都接住，不同 Windows 版本/调用方走的不一样。
    #[implement(IShellBrowser)]
    struct Browser {
        hwnd: isize,
        view: IShellView,
    }

    impl Browser {
        fn new(hwnd: isize, view: &IShellView) -> Self {
            Self {
                hwnd,
                view: view.clone(),
            }
        }
    }

    impl IOleWindow_Impl for Browser_Impl {
        fn GetWindow(&self) -> windows::core::Result<HWND> {
            Ok(HWND(self.hwnd as *mut c_void))
        }

        fn ContextSensitiveHelp(&self, _enter: BOOL) -> windows::core::Result<()> {
            Err(E_NOTIMPL.into())
        }
    }

    impl IShellBrowser_Impl for Browser_Impl {
        fn InsertMenusSB(
            &self,
            _shared: windows::Win32::UI::WindowsAndMessaging::HMENU,
            _widths: *mut windows::Win32::System::Ole::OLEMENUGROUPWIDTHS,
        ) -> windows::core::Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn SetMenuSB(
            &self,
            _shared: windows::Win32::UI::WindowsAndMessaging::HMENU,
            _ole: isize,
            _active: HWND,
        ) -> windows::core::Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn RemoveMenusSB(
            &self,
            _shared: windows::Win32::UI::WindowsAndMessaging::HMENU,
        ) -> windows::core::Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn SetStatusTextSB(&self, _text: &PCWSTR) -> windows::core::Result<()> {
            Ok(())
        }

        fn EnableModelessSB(&self, _enable: BOOL) -> windows::core::Result<()> {
            Ok(())
        }

        fn TranslateAcceleratorSB(&self, _msg: *const MSG, _id: u16) -> windows::core::Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn BrowseObject(&self, _pidl: *const ITEMIDLIST, _flags: u32) -> windows::core::Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn GetViewStateStream(
            &self,
            _mode: u32,
        ) -> windows::core::Result<windows::Win32::System::Com::IStream> {
            Err(E_NOTIMPL.into())
        }

        fn GetControlWindow(&self, _id: u32) -> windows::core::Result<HWND> {
            Err(E_NOTIMPL.into())
        }

        fn SendControlMsg(
            &self,
            _id: u32,
            _msg: u32,
            _wparam: windows::Win32::Foundation::WPARAM,
            _lparam: windows::Win32::Foundation::LPARAM,
            _result: *mut windows::Win32::Foundation::LRESULT,
        ) -> windows::core::Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn QueryActiveShellView(&self) -> windows::core::Result<IShellView> {
            swv!("QueryActiveShellView -> 有");
            Ok(self.view.clone())
        }

        fn OnViewWindowActive(&self, _view: Ref<'_, IShellView>) -> windows::core::Result<()> {
            Ok(())
        }

        fn SetToolbarItems(
            &self,
            _buttons: *const windows::Win32::UI::Controls::TBBUTTON,
            _count: u32,
            _flags: u32,
        ) -> windows::core::Result<()> {
            Err(E_NOTIMPL.into())
        }
    }

    // ---- Document：IDispatch + IServiceProvider ----

    /// `IWebBrowserApp::Document` 的返回值。Shell 只拿它 `QueryInterface` 成
    /// `IServiceProvider`，再从里面要 `IShellView` / `IShellBrowser`。
    #[implement(IDispatch, IServiceProvider)]
    struct Document {
        view: IShellView,
        browser: IShellBrowser,
    }

    impl Document {
        fn new(view: &IShellView, browser: &IShellBrowser) -> Self {
            Self {
                view: view.clone(),
                browser: browser.clone(),
            }
        }
    }

    impl IDispatch_Impl for Document_Impl {
        fn GetTypeInfoCount(&self) -> windows::core::Result<u32> {
            Ok(0)
        }

        fn GetTypeInfo(&self, _index: u32, _lcid: u32) -> windows::core::Result<ITypeInfo> {
            Err(E_NOTIMPL.into())
        }

        fn GetIDsOfNames(
            &self,
            _riid: *const GUID,
            _names: *const PCWSTR,
            _count: u32,
            _lcid: u32,
            _ids: *mut i32,
        ) -> windows::core::Result<()> {
            Err(DISP_E_UNKNOWNNAME.into())
        }

        fn Invoke(
            &self,
            _member: i32,
            _riid: *const GUID,
            _lcid: u32,
            _flags: DISPATCH_FLAGS,
            _params: *const DISPPARAMS,
            _result: *mut VARIANT,
            _info: *mut EXCEPINFO,
            _arg_error: *mut u32,
        ) -> windows::core::Result<()> {
            Err(DISP_E_MEMBERNOTFOUND.into())
        }
    }

    impl IServiceProvider_Impl for Document_Impl {
        fn QueryService(
            &self,
            service: *const GUID,
            riid: *const GUID,
            out: *mut *mut c_void,
        ) -> windows::core::Result<()> {
            if service.is_null() || riid.is_null() || out.is_null() {
                return Err(E_POINTER.into());
            }
            let service = unsafe { *service };
            let riid = unsafe { *riid };
            swv!(
                "QueryService service={} riid={}",
                guid_name(&service),
                guid_name(&riid)
            );
            // 只看 `riid`：`service` 只是个提示，不同 Windows 版本/调用方给的
            // 服务 GUID 不一样（实测 `SHOpenFolderAndSelectItems` 给的是
            // IFolderView 的 GUID 却要 IShellView），按服务名分派会漏。
            unsafe {
                if riid == IShellView::IID {
                    give(&self.view, &riid, out)
                } else if riid == IID_SHELL_BROWSER {
                    give(&self.browser, &riid, out)
                } else {
                    sw!(
                        "!! QueryService 不支持的 riid={}（service={}）",
                        guid_name(&riid),
                        guid_name(&service)
                    );
                    Err(E_NOINTERFACE.into())
                }
            }
        }
    }

    /// `{000214e2-0000-0000-c000-000000000046}`：`IID_IShellBrowser`，
    /// 也是 `SID_SShellBrowser`（两者同一个 GUID）。
    const IID_SHELL_BROWSER: GUID = GUID::from_u128(0x000214e2_0000_0000_c000_000000000046);

    /// 把 `object` 按 `riid` 交给调用方（自己做 `QueryInterface` + AddRef）。
    unsafe fn give<T: windows::core::Interface>(
        object: &T,
        riid: &GUID,
        out: *mut *mut c_void,
    ) -> windows::core::Result<()> {
        unsafe {
            let _ = object.query(riid, out);
        }
        if unsafe { (*out).is_null() } {
            return Err(E_NOINTERFACE.into());
        }
        Ok(())
    }

    // ---- BrowserApp：IWebBrowserApp ----

    /// `IShellWindows` 里的一条记录。只有 HWND / 位置 / Document 有用。
    #[implement(IWebBrowserApp)]
    struct BrowserApp {
        hwnd: isize,
        url: String,
        name: String,
        document: IDispatch,
    }

    impl BrowserApp {
        fn new(hwnd: isize, url: String, name: String, document: IDispatch) -> Self {
            Self {
                hwnd,
                url,
                name,
                document,
            }
        }
    }

    impl IDispatch_Impl for BrowserApp_Impl {
        fn GetTypeInfoCount(&self) -> windows::core::Result<u32> {
            Ok(0)
        }

        fn GetTypeInfo(&self, _index: u32, _lcid: u32) -> windows::core::Result<ITypeInfo> {
            Err(DISP_E_BADINDEX.into())
        }

        fn GetIDsOfNames(
            &self,
            _riid: *const GUID,
            _names: *const PCWSTR,
            _count: u32,
            _lcid: u32,
            _ids: *mut i32,
        ) -> windows::core::Result<()> {
            Err(DISP_E_UNKNOWNNAME.into())
        }

        fn Invoke(
            &self,
            _member: i32,
            _riid: *const GUID,
            _lcid: u32,
            _flags: DISPATCH_FLAGS,
            _params: *const DISPPARAMS,
            _result: *mut VARIANT,
            _info: *mut EXCEPINFO,
            _arg_error: *mut u32,
        ) -> windows::core::Result<()> {
            Err(DISP_E_MEMBERNOTFOUND.into())
        }
    }

    impl IWebBrowser_Impl for BrowserApp_Impl {
        fn GoBack(&self) -> windows::core::Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn GoForward(&self) -> windows::core::Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn GoHome(&self) -> windows::core::Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn GoSearch(&self) -> windows::core::Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn Navigate(
            &self,
            _url: &BSTR,
            _flags: *const VARIANT,
            _target: *const VARIANT,
            _post: *const VARIANT,
            _headers: *const VARIANT,
        ) -> windows::core::Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn Refresh(&self) -> windows::core::Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn Refresh2(&self, _level: *const VARIANT) -> windows::core::Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn Stop(&self) -> windows::core::Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn Application(&self) -> windows::core::Result<IDispatch> {
            Err(E_NOTIMPL.into())
        }

        fn Parent(&self) -> windows::core::Result<IDispatch> {
            Err(E_NOTIMPL.into())
        }

        fn Container(&self) -> windows::core::Result<IDispatch> {
            Err(E_NOTIMPL.into())
        }

        fn Document(&self) -> windows::core::Result<IDispatch> {
            Ok(self.document.clone())
        }

        fn TopLevelContainer(&self) -> windows::core::Result<VARIANT_BOOL> {
            Ok(VARIANT_TRUE)
        }

        fn Type(&self) -> windows::core::Result<BSTR> {
            Err(E_NOTIMPL.into())
        }

        fn Left(&self) -> windows::core::Result<i32> {
            Err(E_NOTIMPL.into())
        }

        fn SetLeft(&self, _left: i32) -> windows::core::Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn Top(&self) -> windows::core::Result<i32> {
            Err(E_NOTIMPL.into())
        }

        fn SetTop(&self, _top: i32) -> windows::core::Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn Width(&self) -> windows::core::Result<i32> {
            Err(E_NOTIMPL.into())
        }

        fn SetWidth(&self, _width: i32) -> windows::core::Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn Height(&self) -> windows::core::Result<i32> {
            Err(E_NOTIMPL.into())
        }

        fn SetHeight(&self, _height: i32) -> windows::core::Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn LocationName(&self) -> windows::core::Result<BSTR> {
            Ok(BSTR::from(&self.name))
        }

        fn LocationURL(&self) -> windows::core::Result<BSTR> {
            Ok(BSTR::from(&self.url))
        }

        fn Busy(&self) -> windows::core::Result<VARIANT_BOOL> {
            Ok(VARIANT_BOOL(0))
        }
    }

    impl IWebBrowserApp_Impl for BrowserApp_Impl {
        fn Quit(&self) -> windows::core::Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn ClientToWindow(&self, _cx: *mut i32, _cy: *mut i32) -> windows::core::Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn PutProperty(&self, _name: &BSTR, _value: &VARIANT) -> windows::core::Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn GetProperty(&self, _name: &BSTR) -> windows::core::Result<VARIANT> {
            Err(E_NOTIMPL.into())
        }

        fn Name(&self) -> windows::core::Result<BSTR> {
            Ok(BSTR::from("Rove"))
        }

        fn HWND(&self) -> windows::core::Result<SHANDLE_PTR> {
            Ok(SHANDLE_PTR(self.hwnd))
        }

        fn FullName(&self) -> windows::core::Result<BSTR> {
            Err(E_NOTIMPL.into())
        }

        fn Path(&self) -> windows::core::Result<BSTR> {
            Err(E_NOTIMPL.into())
        }

        fn Visible(&self) -> windows::core::Result<VARIANT_BOOL> {
            let visible = unsafe { IsWindowVisible(HWND(self.hwnd as *mut c_void)) }.as_bool();
            Ok(if visible { VARIANT_TRUE } else { VARIANT_BOOL(0) })
        }

        fn SetVisible(&self, _value: VARIANT_BOOL) -> windows::core::Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn StatusBar(&self) -> windows::core::Result<VARIANT_BOOL> {
            Err(E_NOTIMPL.into())
        }

        fn SetStatusBar(&self, _value: VARIANT_BOOL) -> windows::core::Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn StatusText(&self) -> windows::core::Result<BSTR> {
            Err(E_NOTIMPL.into())
        }

        fn SetStatusText(&self, _text: &BSTR) -> windows::core::Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn ToolBar(&self) -> windows::core::Result<i32> {
            Err(E_NOTIMPL.into())
        }

        fn SetToolBar(&self, _value: i32) -> windows::core::Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn MenuBar(&self) -> windows::core::Result<VARIANT_BOOL> {
            Err(E_NOTIMPL.into())
        }

        fn SetMenuBar(&self, _value: VARIANT_BOOL) -> windows::core::Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn FullScreen(&self) -> windows::core::Result<VARIANT_BOOL> {
            Err(E_NOTIMPL.into())
        }

        fn SetFullScreen(&self, _value: VARIANT_BOOL) -> windows::core::Result<()> {
            Err(E_NOTIMPL.into())
        }
    }
}

// ---------------------------------------------------------------------------
// 非 Windows 平台：什么也不做（宿主上只编译，不运行）
// ---------------------------------------------------------------------------

#[cfg(not(windows))]
mod imp {
    use super::PaneRegistration;
    use std::path::PathBuf;

    pub fn publish(_panes: Vec<PaneRegistration>) {}

    pub fn reserve(_path: PathBuf) {}

    pub fn set_select_sink(_sink: impl Fn(PathBuf) + Send + Sync + 'static) {}
}
