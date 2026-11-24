//! 菜单子进程：在这里**构建并显示**原生菜单。
//!
//! 为什么放到子进程：第三方外壳扩展（网盘 / 同步 / 杀毒）会在
//! `QueryContextMenu` 或子菜单展开里永远不返回。以前它卡的是 Rove 自己的
//! 菜单线程 —— 线程杀不掉，只能丢弃，丢着丢着进程就被拖坏到「新线程一行代码
//! 都跑不到」，用户看到的就是右键彻底没反应。
//!
//! 换成子进程之后：卡的是这个进程，父进程一行都没跑过扩展代码；而且进程能
//! `TerminateProcess`，卡多少条都能杀干净，不留残渣。
//!
//! 子进程的活法：从 stdin 读一条请求 → 建菜单 → 往 stdout 打 `shown` →
//! `TrackPopupMenuEx` 阻塞等用户 → 把「用户选了哪一项」一行一行打回去 → 退出。

use std::cell::RefCell;
use std::collections::HashMap;
use std::mem::ManuallyDrop;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows::core::{Interface, PCSTR, PCWSTR, PSTR, Result as WinResult};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Ole::OleInitialize;
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    IContextMenu, IContextMenu2, IContextMenu3, IShellFolder, IShellView, SHBindToParent,
    SHCreateDefaultContextMenu, SHGetDesktopFolder, SHParseDisplayName, CMF_CANRENAME, CMF_EXPLORE,
    CMF_EXTENDEDVERBS, CMF_NORMAL, CMIC_MASK_CONTROL_DOWN, CMIC_MASK_PTINVOKE,
    CMIC_MASK_SHIFT_DOWN, CMINVOKECOMMANDINFO, CMINVOKECOMMANDINFOEX, DEFCONTEXTMENU, GCS_VERBW,
    SVGIO_BACKGROUND,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DeleteMenu, DestroyMenu,
    GetCursorPos, GetForegroundWindow, GetMenuItemCount, GetMenuItemID, GetMenuState,
    GetMenuStringW, GetSubMenu, IsWindow, RegisterClassW, SetForegroundWindow, SetWindowPos,
    ShowWindow, TrackPopupMenuEx, HMENU, MF_BYPOSITION, MF_CHECKED, MF_GRAYED, MF_POPUP,
    MF_SEPARATOR, MF_STRING, SWP_NOACTIVATE, SWP_NOZORDER, SW_SHOWNOACTIVATE,
    TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_DRAWITEM, WM_INITMENUPOPUP, WM_MEASUREITEM, WM_MENUCHAR,
    WNDCLASSW, WS_EX_TOOLWINDOW, WS_POPUP,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, SetActiveWindow, SetFocus, VK_CONTROL, VK_SHIFT,
};

use super::dark;
use super::wire::{self, Request};
use super::{MenuNode, Options, Target};

/// Shell 命令 id 区间（与 `QueryContextMenu` 的入参一致）。
const SHELL_FIRST: u32 = 1;
const SHELL_LAST: u32 = 0x6FFF;
/// 应用命令 id 起点。
const APP_FIRST: u32 = 0x7000;
/// `CMINVOKECOMMANDINFOEX` 的 `fMask` 位：`lpVerbW` 才是宽字符动词。
const CMIC_MASK_UNICODE: u32 = 0x4000;
/// `AllowSetForegroundWindow` 的「任何进程都可以抢前台」。
const ASFW_ANY: u32 = 0xFFFF_FFFF;

thread_local! {
    /// 当前这个线程上正在 Track 的菜单：四条菜单消息要转发给它。
    static ACTIVE: RefCell<Option<IContextMenu>> = const { RefCell::new(None) };
    /// 当前菜单的裁剪选项。子菜单是**展开那一刻**才被 Shell 填内容的，
    /// 所以 `WM_INITMENUPOPUP` 里必须再裁一次。
    static ACTIVE_OPTS: RefCell<Option<Options>> = const { RefCell::new(None) };
    /// 背景菜单取自视图时，视图对象必须活到菜单关掉为止。
    static ACTIVE_VIEW: RefCell<Option<IShellView>> = const { RefCell::new(None) };
    /// 根弹出菜单的句柄。它也会收到 `WM_INITMENUPOPUP`，但那个时刻 Rove
    /// 自己的菜单项已经追加进去了；根菜单在构建阶段（那时还只有系统项）
    /// 已经裁过一遍，所以补过滤要跳过它。
    static ACTIVE_ROOT: RefCell<isize> = const { RefCell::new(0) };
    /// Rove 自己建的子菜单句柄。关键词过滤只针对系统菜单项，
    /// 这些子菜单连同里面的内容一律不碰。
    static APP_POPUPS: RefCell<Vec<isize>> = const { RefCell::new(Vec::new()) };
}

/// 子进程入口（`--menu-host`）。返回即退出进程。
pub(crate) fn run() -> ! {
    let code = host_main();
    // 不跑任何清理钩子：菜单已经关掉了，剩下的就是退出（父进程也不等我们）。
    std::process::exit(code);
}

fn host_main() -> i32 {
    // 先报一声「我起来了」：父进程那 2.5 秒的看门狗从这一刻才开始算
    // （冷启动、杀毒扫 exe 都不该被当成「扩展卡死」）。
    wire::emit(wire::EV_READY);
    // 必须在建窗口之前：菜单的 DPI 在那一刻就定下来了。
    become_dpi_aware();
    let Some(request) = read_request() else {
        crate::logal!("!! [menu] 菜单子进程没读到请求，退出");
        return 2;
    };
    unsafe {
        // OleInitialize = STA COM + OLE（剪贴板延迟渲染靠它）。
        let hr = OleInitialize(None);
        crate::logv!("[menu] child: ole hr={:?}", hr);
    }
    // 必须在**第一个窗口建出来之前**设：`SetPreferredAppMode` 对已经创建过的
    // 窗口不追溯，晚一步菜单就还是浅色。
    let _ = dark::set_preferred_mode(request.dark);
    let Some(hwnd) = create_host_window() else {
        crate::logal!("!! [menu] 菜单子进程建窗口失败");
        return 3;
    };
    dark::apply_dark_flag(hwnd, request.dark);
    if show_menu(hwnd, &request) {
        // 交给了 Shell 去干活（复制 / 移动 / 删除 / 发送到…）：那些活跑在
        // **本进程**里，现在退出会把它们砍断。
        linger_for_shell_work(hwnd);
    }
    0
}

/// 声明本进程的 DPI 感知，和主进程保持一致。
///
/// 主进程的 DPI 感知是 Slint/winit 建事件循环时顺手设的（它自己会调
/// `SetProcessDpiAwarenessContext` 到 per-monitor v2）。菜单子进程在跑到
/// Slint 之前就分叉出去了，从来没设过 —— 于是 Windows 把整个进程当成
/// 「不知道缩放」的老程序，**把窗口和菜单位图拉伸**再显示：125% / 150% 的
/// 屏幕上菜单字发虚、边框糊成一片。
fn become_dpi_aware() {
    use windows_sys::Win32::UI::HiDpi::{
        SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE,
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    };
    unsafe {
        // 与 winit 的优先级一致：v2 → v1 → Win7 的老接口。
        if SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) != 0 {
            crate::logv!("[menu] child dpi aware = per-monitor v2");
            return;
        }
        if SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE) != 0 {
            crate::logv!("[menu] child dpi aware = per-monitor");
            return;
        }
        windows_sys::Win32::UI::WindowsAndMessaging::SetProcessDPIAware();
        crate::logal!("!! [menu] 菜单子进程只拿到旧版 DPI 感知（菜单可能发虚）");
    }
}

fn read_request() -> Option<Request> {
    use std::io::Read;
    let mut input = Vec::new();
    std::io::stdin().read_to_end(&mut input).ok()?;
    if input.is_empty() {
        return None;
    }
    bincode::deserialize(&input).ok()
}

/// 建一个零面积的宿主窗口：`TrackPopupMenuEx` 的 hWnd 必须是调用线程的窗口。
fn create_host_window() -> Option<HWND> {
    let class_name = windows::core::w!("Rove.NativeMenuChild");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(host_proc),
        lpszClassName: class_name,
        ..Default::default()
    };
    unsafe {
        RegisterClassW(&wc);
        CreateWindowExW(
            // 注意：不能加 WS_EX_NOACTIVATE —— 激活不了就拿不到键盘焦点，
            // 菜单的方向键 / 回车 / Esc 全会失效。
            WS_EX_TOOLWINDOW,
            class_name,
            PCWSTR::null(),
            WS_POPUP,
            0,
            0,
            1,
            1,
            None,
            None,
            None,
            None,
        )
        .ok()
    }
}

unsafe extern "system" fn host_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if let Some(result) = forward_menu_message(msg, wp, lp) {
        return result;
    }
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

/// `WM_INITMENUPOPUP` / `WM_DRAWITEM` / `WM_MEASUREITEM` / `WM_MENUCHAR` 的转发。
fn forward_menu_message(msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
    if !matches!(
        msg,
        WM_INITMENUPOPUP | WM_DRAWITEM | WM_MEASUREITEM | WM_MENUCHAR
    ) {
        return None;
    }
    let menu = ACTIVE.with(|active| active.borrow().clone())?;
    let mut result = LRESULT(0);
    let mut handled = false;
    if let Ok(menu3) = menu.cast::<IContextMenu3>() {
        if unsafe { menu3.HandleMenuMsg2(msg, wp, lp, Some(&mut result)) }.is_ok() {
            handled = true;
        }
    }
    if !handled {
        if let Ok(menu2) = menu.cast::<IContextMenu2>() {
            if unsafe { menu2.HandleMenuMsg(msg, wp, lp) }.is_ok() {
                handled = true;
                result = LRESULT(0);
            }
        }
    }
    // 子菜单是**展开那一刻**才被 Shell 填内容的，所以这里必须再裁一次。
    // 只处理子菜单：根弹出菜单在构建阶段已经裁过，那时里面还只有系统项。
    if msg == WM_INITMENUPOPUP {
        let popup = HMENU(wp.0 as *mut _);
        let root = ACTIVE_ROOT.with(|value| *value.borrow());
        if root != 0 && wp.0 as isize == root {
            return handled.then_some(result);
        }
        // IContextMenu3 的聚合会跳过「只实现了 IContextMenu2」的子处理器
        // （典型：授予访问权限 / 共享、部分网盘与同步类扩展）。
        // 结果就是子菜单一直空着，但 `HandleMenuMsg2` 照样返回 S_OK。
        // 发现还是空的就补一次 2 版本的调用。
        if unsafe { GetMenuItemCount(Some(popup)) } <= 0 {
            if let Ok(menu2) = menu.cast::<IContextMenu2>() {
                let _ = unsafe { menu2.HandleMenuMsg(msg, wp, lp) };
            }
        }
        if let Some(options) = ACTIVE_OPTS.with(|opts| opts.borrow().clone()) {
            let filled = unsafe { GetMenuItemCount(Some(popup)) }.max(0);
            filter_hidden(popup, &options);
            let kept = unsafe { GetMenuItemCount(Some(popup)) }.max(0);
            crate::logv!(
                "[menu] popup handled={} filled={} kept={}",
                handled,
                filled,
                kept
            );
        }
    }
    handled.then_some(result)
}

// ---------------------------------------------------------------------------
// 菜单站点
// ---------------------------------------------------------------------------

/// 「授予访问权限」这类扩展不是从 `IShellExtInit` 的 pidl / 数据对象里拿目标的，
/// 而是向菜单的 **site**（`IObjectWithSite`）要一个 `IFolderView`，
/// 再调 `IFolderView::Items(SVGIO_SELECTION, IID_IShellItemArray)` 取当前目标。
/// 不给 site 时它们不报错、也不填子菜单——就是我们现在遇到的现象。
///
/// 这里给一个只实现必要几项的最小 site：
/// `IOleWindow::GetWindow` 给宿主窗口，`IFolderView::Items` 给文件夹自身的
/// `IShellItemArray`，其余全部 `E_NOTIMPL`。
mod site {
    use std::cell::RefCell;
    use std::ffi::c_void;
    use std::path::Path;

    use windows::core::{implement, GUID, IUnknown, Interface, Result as WinResult};
    use windows::Win32::Foundation::{BOOL, E_NOTIMPL, E_NOINTERFACE, HWND, POINT};
    use windows::Win32::System::Com::{IServiceProvider, IServiceProvider_Impl};
    use windows::Win32::System::Ole::{IObjectWithSite, IOleWindow, IOleWindow_Impl};
    use windows::Win32::UI::Shell::Common::ITEMIDLIST;
    use windows::Win32::UI::Shell::{
        IFolderView, IFolderView_Impl, IShellItem, IShellItemArray, SHCreateItemFromIDList,
        SHCreateShellItemArrayFromShellItem, _SVGIO,
    };

    thread_local! {
        /// `QueryService` 要把自己 QI 成 `IFolderView`，但 `_Impl` 方法里
        /// 只有 `&self`，拿不到外层 COM 指针，所以存一份自己。
        static SELF_UNK: RefCell<Option<IUnknown>> = const { RefCell::new(None) };
    }

    #[implement(IServiceProvider, IOleWindow, IFolderView)]
    struct MenuSite {
        hwnd: HWND,
        item: IShellItem,
    }

    impl IServiceProvider_Impl for MenuSite_Impl {
        fn QueryService(
            &self,
            _guidservice: *const GUID,
            riid: *const GUID,
            ppvobject: *mut *mut c_void,
        ) -> WinResult<()> {
            let unknown = SELF_UNK.with(|slot| slot.borrow().clone());
            let Some(unknown) = unknown else {
                return Err(windows::core::Error::from_hresult(E_NOINTERFACE));
            };
            unsafe { unknown.query(riid, ppvobject) }.ok()
        }
    }

    impl IOleWindow_Impl for MenuSite_Impl {
        fn GetWindow(&self) -> WinResult<HWND> {
            Ok(self.hwnd)
        }
        fn ContextSensitiveHelp(&self, _entermode: BOOL) -> WinResult<()> {
            Err(windows::core::Error::from_hresult(E_NOTIMPL))
        }
    }

    impl IFolderView_Impl for MenuSite_Impl {
        fn GetCurrentViewMode(&self) -> WinResult<u32> {
            Err(windows::core::Error::from_hresult(E_NOTIMPL))
        }
        fn SetCurrentViewMode(&self, _viewmode: u32) -> WinResult<()> {
            Err(windows::core::Error::from_hresult(E_NOTIMPL))
        }
        fn GetFolder(&self, _riid: *const GUID, _ppv: *mut *mut c_void) -> WinResult<()> {
            Err(windows::core::Error::from_hresult(E_NOTIMPL))
        }
        fn Item(&self, _itemindex: i32) -> WinResult<*mut ITEMIDLIST> {
            Err(windows::core::Error::from_hresult(E_NOTIMPL))
        }
        fn ItemCount(&self, _flags: &_SVGIO) -> WinResult<i32> {
            Err(windows::core::Error::from_hresult(E_NOTIMPL))
        }
        fn Items(&self, _flags: &_SVGIO, riid: *const GUID, ppv: *mut *mut c_void) -> WinResult<()> {
            let array: IShellItemArray =
                unsafe { SHCreateShellItemArrayFromShellItem(&self.item) }?;
            unsafe { array.query(riid, ppv) }.ok()
        }
        fn GetSelectionMarkedItem(&self) -> WinResult<i32> {
            Err(windows::core::Error::from_hresult(E_NOTIMPL))
        }
        fn GetFocusedItem(&self) -> WinResult<i32> {
            Err(windows::core::Error::from_hresult(E_NOTIMPL))
        }
        fn GetItemPosition(&self, _pidl: *const ITEMIDLIST) -> WinResult<POINT> {
            Err(windows::core::Error::from_hresult(E_NOTIMPL))
        }
        fn GetSpacing(&self, _ppt: *mut POINT) -> WinResult<()> {
            Err(windows::core::Error::from_hresult(E_NOTIMPL))
        }
        fn GetDefaultSpacing(&self) -> WinResult<POINT> {
            Err(windows::core::Error::from_hresult(E_NOTIMPL))
        }
        fn GetAutoArrange(&self) -> WinResult<()> {
            Err(windows::core::Error::from_hresult(E_NOTIMPL))
        }
        fn SelectItem(&self, _item: i32, _flags: u32) -> WinResult<()> {
            Err(windows::core::Error::from_hresult(E_NOTIMPL))
        }
        fn SelectAndPositionItems(
            &self,
            _cidl: u32,
            _apidl: *const *const ITEMIDLIST,
            _apt: *const POINT,
            _flags: u32,
        ) -> WinResult<()> {
            Err(windows::core::Error::from_hresult(E_NOTIMPL))
        }
    }

    /// 挂上 site；菜单关掉后 `Drop` 会自动摘下来。
    pub(super) struct SiteGuard {
        shell_hook: IObjectWithSite,
        _provider: IServiceProvider,
    }

    impl Drop for SiteGuard {
        fn drop(&mut self) {
            let _ = unsafe { self.shell_hook.SetSite(None) };
            SELF_UNK.with(|slot| *slot.borrow_mut() = None);
        }
    }

    pub(super) fn attach(
        menu: &windows::Win32::UI::Shell::IContextMenu,
        hwnd: HWND,
        dir: &Path,
    ) -> Option<SiteGuard> {
        let pidl = super::path_pidl(dir).ok()?;
        let guard = super::Pidls(vec![pidl]);
        let item: IShellItem = unsafe { SHCreateItemFromIDList(guard.0[0]) }.ok()?;
        let unknown: IUnknown = MenuSite { hwnd, item }.into();
        SELF_UNK.with(|slot| *slot.borrow_mut() = Some(unknown.clone()));
        let provider: IServiceProvider = match unknown.cast() {
            Ok(provider) => provider,
            Err(_) => {
                SELF_UNK.with(|slot| *slot.borrow_mut() = None);
                return None;
            }
        };
        let shell_hook: IObjectWithSite = match menu.cast() {
            Ok(hook) => hook,
            Err(_) => {
                SELF_UNK.with(|slot| *slot.borrow_mut() = None);
                return None;
            }
        };
        if unsafe { shell_hook.SetSite(Some(&unknown)) }.is_err() {
            SELF_UNK.with(|slot| *slot.borrow_mut() = None);
            return None;
        }
        crate::logv!("[menu] site attached");
        Some(SiteGuard {
            shell_hook,
            _provider: provider,
        })
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

/// 取路径的绝对 PIDL（调用方负责 `CoTaskMemFree`）。
fn path_pidl(path: &Path) -> WinResult<*mut ITEMIDLIST> {
    let mut name: Vec<u16> = path.as_os_str().encode_wide().collect();
    name.push(0);
    let mut pidl = std::ptr::null_mut();
    unsafe { SHParseDisplayName(PCWSTR(name.as_ptr()), None, &mut pidl, 0, None)? };
    if pidl.is_null() {
        return Err(windows::core::Error::from_win32());
    }
    Ok(pidl)
}

struct Pidls(Vec<*mut ITEMIDLIST>);
impl Drop for Pidls {
    fn drop(&mut self) {
        for pidl in self.0.drain(..) {
            unsafe { CoTaskMemFree(Some(pidl.cast())) };
        }
    }
}

/// 绝对 PIDL 里最后一个 `SHITEMID` 的字节偏移。
/// 从该偏移开始就是「相对父文件夹的子项 PIDL」。
unsafe fn last_item_offset(pidl: *const ITEMIDLIST) -> Option<usize> {
    let mut at = pidl as *const u8;
    let mut offset = 0usize;
    let mut last = 0usize;
    loop {
        let cb = std::ptr::read_unaligned(at as *const u16) as usize;
        if cb == 0 {
            break;
        }
        last = offset;
        offset += cb;
        at = at.add(cb);
        if offset > 0x10000 {
            return None;
        }
    }
    Some(last)
}

/// 选中项 → `IContextMenu`。
/// 同一父文件夹时走 `SHBindToParent` + `IShellFolder::GetUIObjectOf`（与资源管理器一致）；
/// 横跨多个文件夹时返回 None，由调用方回退到 `SHCreateDefaultContextMenu`。
fn items_context_menu(host: HWND, pidls: &[*mut ITEMIDLIST]) -> Option<IContextMenu> {
    let first = *pidls.first()?;
    let offsets: Vec<usize> = pidls
        .iter()
        .map(|p| unsafe { last_item_offset(*p) })
        .collect::<Option<Vec<_>>>()?;
    let prefix_len = offsets[0];
    let prefix = unsafe { std::slice::from_raw_parts(first as *const u8, prefix_len) };
    for (pidl, offset) in pidls.iter().zip(&offsets) {
        if *offset != prefix_len {
            return None;
        }
        let other = unsafe { std::slice::from_raw_parts(*pidl as *const u8, *offset) };
        if other != prefix {
            return None;
        }
    }
    let children: Vec<*const ITEMIDLIST> = pidls
        .iter()
        .zip(&offsets)
        .map(|(pidl, offset)| unsafe { (*pidl as *const u8).add(*offset) as *const ITEMIDLIST })
        .collect();
    let parent: IShellFolder = unsafe { SHBindToParent(first, None) }.ok()?;
    unsafe { parent.GetUIObjectOf(host, &children, None) }.ok()
}

/// 目录空白处 → `IContextMenu`。
/// 必须绑定到该目录**本身**（`BindToObject`）再 `CreateViewObject`。
/// 用 `SHBindToParent` 会拿到它的父目录，结果是上一层的空白菜单（「新建」会建到上一级）。
fn folder_context_menu(host: HWND, dir: &Path) -> Option<IContextMenu> {
    let pidl = path_pidl(dir).ok()?;
    let pidls = Pidls(vec![pidl]);
    let desktop = unsafe { SHGetDesktopFolder() }.ok()?;
    let folder: IShellFolder = unsafe { desktop.BindToObject(pidls.0[0], None) }.ok()?;
    // 「授予访问权限」这类子菜单是靠 Shell 扩展在展开那一刻填的。
    // 文件夹自己的菜单对象（`CreateViewObject(IID_IContextMenu)`）虽然也会
    // 画出那一条，但它转发不到扩展里，子菜单永远是空的。
    // 视图的背景菜单对象才会转发，所以优先走视图。
    if let Ok(view) = unsafe { folder.CreateViewObject::<IShellView>(host) } {
        if let Ok(menu) = unsafe { view.GetItemObject::<IContextMenu>(SVGIO_BACKGROUND) } {
            crate::logv!("[menu] background source=view");
            ACTIVE_VIEW.with(|slot| *slot.borrow_mut() = Some(view));
            return Some(menu);
        }
    }
    crate::logv!("[menu] background source=folder-object");
    unsafe { folder.CreateViewObject::<IContextMenu>(host) }.ok()
}

/// 按用户关键词删除 Shell 菜单项（`!` 开头 = 精确匹配，否则包含即隐藏）。
fn filter_hidden(hmenu: HMENU, options: &Options) {
    if options.hide_keywords.is_empty() {
        return;
    }
    if APP_POPUPS.with(|list| list.borrow().iter().any(|h| *h == hmenu.0 as isize)) {
        return;
    }
    let count = unsafe { GetMenuItemCount(Some(hmenu)) }.max(0);
    for index in (0..count).rev() {
        let id = unsafe { GetMenuItemID(hmenu, index) };
        if id >= APP_FIRST && id != u32::MAX {
            continue;
        }
        // 子菜单项没有 id，只能看句柄归属。
        if id == u32::MAX {
            let sub = unsafe { GetSubMenu(hmenu, index) };
            if !sub.is_invalid()
                && APP_POPUPS.with(|list| list.borrow().iter().any(|h| *h == sub.0 as isize))
            {
                continue;
            }
        }
        // 原始标签直接交给 keyword_hit：精确匹配要靠结尾的省略号区分
        // 「打开方式」的两种写法（带点 = 弹选择器，不带点 = 展开子菜单）。
        let label = menu_label(hmenu, index);
        if !label.trim().is_empty() && super::keyword_hit(&label, &options.hide_keywords) {
            let _ = unsafe { DeleteMenu(hmenu, index as u32, MF_BYPOSITION) };
            continue;
        }
        let sub = unsafe { GetSubMenu(hmenu, index) };
        if !sub.is_invalid() {
            filter_hidden(sub, options);
        }
    }
    trim_separators(hmenu);
}

fn menu_label(hmenu: HMENU, index: i32) -> String {
    let mut buffer = [0u16; 512];
    let length = unsafe { GetMenuStringW(hmenu, index as u32, Some(&mut buffer), MF_BYPOSITION) };
    let length = length.max(0) as usize;
    String::from_utf16_lossy(&buffer[..length.min(buffer.len())])
}

/// OLE 剪贴板延迟渲染落盘（仅当剪贴板属主是本线程）。
///
/// 剪切/复制是延迟渲染：数据只活在本进程的 STA 里，必须 flush 一次，别的
/// 进程（包括 Rove 自己的粘贴）才读得到 `CF_HDROP`。flush 之后数据由剪贴板
/// 持有，本进程退出也不会丢。
fn flush_ole_clipboard() {
    use windows_sys::Win32::System::DataExchange::GetClipboardOwner;
    use windows_sys::Win32::System::Ole::OleFlushClipboard;
    use windows_sys::Win32::System::Threading::GetCurrentThreadId;
    use windows_sys::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
    unsafe {
        let owner = GetClipboardOwner();
        if owner.is_null() {
            return;
        }
        if GetWindowThreadProcessId(owner, std::ptr::null_mut()) != GetCurrentThreadId() {
            return;
        }
        let _ = OleFlushClipboard();
    }
}

fn append_nodes(
    hmenu: HMENU,
    nodes: &[MenuNode],
    next_id: &mut u32,
    map: &mut HashMap<u32, String>,
) -> WinResult<()> {
    for node in nodes {
        match node {
            MenuNode::Separator => unsafe {
                AppendMenuW(hmenu, MF_SEPARATOR, 0, PCWSTR::null())?;
            },
            MenuNode::Item {
                key,
                label,
                enabled,
                checked,
            } => {
                let id = *next_id;
                *next_id = next_id.wrapping_add(1);
                map.insert(id, key.clone());
                let mut flags = MF_STRING;
                if !enabled {
                    flags |= MF_GRAYED;
                }
                if *checked {
                    flags |= MF_CHECKED;
                }
                let text = wide(label);
                unsafe { AppendMenuW(hmenu, flags, id as usize, PCWSTR(text.as_ptr()))? };
            }
            MenuNode::Submenu {
                label,
                enabled,
                children,
            } => {
                let sub = unsafe { CreatePopupMenu()? };
                APP_POPUPS.with(|list| list.borrow_mut().push(sub.0 as isize));
                append_nodes(sub, children, next_id, map)?;
                let mut flags = MF_POPUP | MF_STRING;
                if !enabled {
                    flags |= MF_GRAYED;
                }
                let text = wide(label);
                unsafe {
                    AppendMenuW(hmenu, flags, sub.0 as usize, PCWSTR(text.as_ptr()))?;
                }
            }
        }
    }
    Ok(())
}

/// 去掉连续 / 首尾分隔线：Shell 菜单按条件吞掉项后经常留下空档。
fn trim_separators(hmenu: HMENU) {
    let mut index = 0u32;
    let mut previous_separator = true;
    loop {
        let count = unsafe { GetMenuItemCount(Some(hmenu)) }.max(0) as u32;
        if index >= count {
            break;
        }
        let flags = unsafe { GetMenuState(hmenu, index, MF_BYPOSITION) };
        let separator = flags != u32::MAX && flags & MF_SEPARATOR.0 != 0;
        if separator && (previous_separator || index + 1 == count) {
            let _ = unsafe { DeleteMenu(hmenu, index, MF_BYPOSITION) };
        } else {
            previous_separator = separator;
            index += 1;
        }
    }
}

/// Shell 命令的规范动词（"open" / "rename" / "delete" …），失败返回 None。
fn canonical_verb(menu: &IContextMenu, id: u32) -> Option<String> {
    if !(SHELL_FIRST..=SHELL_LAST).contains(&id) {
        return None;
    }
    let mut buffer = [0u16; 128];
    unsafe {
        menu.GetCommandString(
            (id - SHELL_FIRST) as usize,
            GCS_VERBW,
            None,
            PSTR(buffer.as_mut_ptr().cast()),
            buffer.len() as u32,
        )
    }
    .ok()?;
    let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    Some(String::from_utf16_lossy(&buffer[..end]))
}

fn invoke_shell(
    menu: &IContextMenu,
    owner: HWND,
    id: u32,
    point: POINT,
    dir: Option<&Path>,
) -> WinResult<()> {
    if !(SHELL_FIRST..=SHELL_LAST).contains(&id) {
        return Ok(());
    }
    let offset = (id - SHELL_FIRST) as usize;
    let shift = unsafe { GetKeyState(VK_SHIFT.0 as i32) } < 0;
    let control = unsafe { GetKeyState(VK_CONTROL.0 as i32) } < 0;
    let mut mask = CMIC_MASK_UNICODE | CMIC_MASK_PTINVOKE;
    if shift {
        mask |= CMIC_MASK_SHIFT_DOWN;
    }
    if control {
        mask |= CMIC_MASK_CONTROL_DOWN;
    }
    // 资源管理器会把“当前文件夹”一起传给处理程序（lpDirectoryW）。
    // 有些扩展靠它拼相对路径 / 定位工作目录，缺了就会自己再猜一个（往往猜错）。
    let dir_wide: Vec<u16> = dir
        .map(|d| d.as_os_str().encode_wide().chain(Some(0)).collect())
        .unwrap_or_default();
    let info = CMINVOKECOMMANDINFOEX {
        cbSize: std::mem::size_of::<CMINVOKECOMMANDINFOEX>() as u32,
        fMask: mask,
        hwnd: owner,
        lpVerb: PCSTR(offset as *const u8),
        nShow: 1, // SW_SHOWNORMAL
        lpVerbW: PCWSTR(offset as *const u16),
        lpDirectoryW: if dir_wide.is_empty() {
            PCWSTR::null()
        } else {
            PCWSTR(dir_wide.as_ptr())
        },
        ptInvoke: point,
        ..Default::default()
    };
    let result = unsafe {
        menu.InvokeCommand((&info as *const CMINVOKECOMMANDINFOEX).cast::<CMINVOKECOMMANDINFO>())
    };
    crate::logv!(
        "[menu] invoke id={} offset={} verb={:?} dir={:?} hr={:?}",
        id,
        offset,
        canonical_verb(menu, id),
        dir.map(|d| d.display().to_string()),
        result.as_ref().err().map(|e| e.code().0)
    );
    result
}

/// 构建并显示菜单，然后把用户的选择一行一行打回父进程。
///
/// 返回 true 表示这一项交给了 Shell 执行（可能是个要在本进程里跑很久的任务）。
fn show_menu(host: HWND, job: &Request) -> bool {
    // `owner`（Rove 主窗口）只用来在菜单关闭后归还前台权，以及当 Shell 那些
    // 确认框的父窗口。菜单本身的 hwnd 一律用本进程的宿主窗口。
    let owner = if job.owner != 0 {
        HWND(job.owner as *mut _)
    } else {
        host
    };
    let mut point = POINT::default();
    if unsafe { GetCursorPos(&mut point) }.is_err() {
        point = POINT { x: 0, y: 0 };
    }

    let hmenu = match unsafe { CreatePopupMenu() } {
        Ok(menu) => menu,
        Err(_) => return false,
    };

    // 1. Shell 部分
    let mut menu: Option<IContextMenu> = None;
    let mut site_guard: Option<site::SiteGuard> = None;
    match &job.target {
        Target::Items(paths) if !paths.is_empty() => {
            let mut pidls = Pidls(Vec::with_capacity(paths.len()));
            for path in paths {
                match path_pidl(path) {
                    Ok(pidl) => pidls.0.push(pidl),
                    Err(_) => continue,
                }
            }
            // 首选：真实父文件夹的 GetUIObjectOf（资源管理器走的就是这条）。
            menu = items_context_menu(host, &pidls.0);
            // 退路：选中项横跨多个文件夹时，交给默认实现拼一个。
            if menu.is_none() {
                if let Ok(desktop) = unsafe { SHGetDesktopFolder() } {
                    let mut list: Vec<*mut ITEMIDLIST> = pidls.0.clone();
                    let mut dcm = DEFCONTEXTMENU {
                        hwnd: host,
                        psf: ManuallyDrop::new(Some(desktop)),
                        cidl: list.len() as u32,
                        apidl: list.as_mut_ptr(),
                        ..Default::default()
                    };
                    menu = unsafe { SHCreateDefaultContextMenu::<IContextMenu>(&dcm) }.ok();
                    unsafe { ManuallyDrop::drop(&mut dcm.psf) };
                }
            }
        }
        Target::Background(dir) => {
            menu = folder_context_menu(host, dir);
            // 背景菜单要挂 site，「授予访问权限」子菜单才会被填内容。
            if let Some(active) = &menu {
                site_guard = site::attach(active, host, dir);
            }
        }
        _ => {}
    }

    // 2. 填充 Shell 命令 + 按用户开关裁掉对应项
    if let Some(active) = &menu {
        let mut flags = CMF_NORMAL | CMF_EXPLORE;
        if matches!(&job.target, Target::Items(p) if p.len() == 1) {
            flags |= CMF_CANRENAME;
        }
        if job.extended {
            flags |= CMF_EXTENDEDVERBS;
        }
        let options = Options {
            hide_keywords: job.hide_keywords.clone(),
            extended: job.extended,
        };
        let _ = unsafe { active.QueryContextMenu(hmenu, 0, SHELL_FIRST, SHELL_LAST, flags) };
        filter_hidden(hmenu, &options);
    }

    // 3. 追加应用命令
    let options = Options {
        hide_keywords: job.hide_keywords.clone(),
        extended: job.extended,
    };
    let mut map: HashMap<u32, String> = HashMap::new();
    let mut next_id = APP_FIRST;
    if !job.nodes.is_empty() {
        // 应用项与系统项之间补一条分隔线，区分归属。
        if unsafe { GetMenuItemCount(Some(hmenu)) } > 0 {
            let _ = unsafe { AppendMenuW(hmenu, MF_SEPARATOR, 0, PCWSTR::null()) };
        }
    }
    let _ = append_nodes(hmenu, &job.nodes, &mut next_id, &mut map);
    trim_separators(hmenu);

    // 4. 显示（阻塞；嵌套消息循环中 host_proc 会转发菜单消息）
    //
    // 菜单的键盘导航（方向键 / 回车 / Esc / 热键）依赖「菜单所属线程的窗口
    // 是前台窗口」。宿主窗口是零面积弹出窗：先摆到菜单锚点、不激活地显示，
    // 再把前台权交给它；菜单关后把前台权还给 Rove。
    unsafe {
        let _ = SetWindowPos(host, None, point.x, point.y, 0, 0, SWP_NOZORDER | SWP_NOACTIVATE);
        let _ = ShowWindow(host, SW_SHOWNOACTIVATE);
        let _ = SetForegroundWindow(host);
        let _ = SetActiveWindow(host);
        let _ = SetFocus(Some(host));
    }
    ACTIVE.with(|active| *active.borrow_mut() = menu.clone());
    ACTIVE_OPTS.with(|opts| *opts.borrow_mut() = Some(options));
    ACTIVE_ROOT.with(|root| *root.borrow_mut() = hmenu.0 as isize);
    let flags = TPM_RETURNCMD.0 | TPM_RIGHTBUTTON.0;
    // 先报「弹出来了」：父进程的看门狗据此停止计时（它只在等这一步）。
    wire::emit(wire::EV_SHOWN);
    let chosen = unsafe { TrackPopupMenuEx(hmenu, flags, point.x, point.y, host, None) }.0 as u32;
    ACTIVE_OPTS.with(|opts| *opts.borrow_mut() = None);
    ACTIVE.with(|active| *active.borrow_mut() = None);
    ACTIVE_VIEW.with(|slot| *slot.borrow_mut() = None);
    ACTIVE_ROOT.with(|root| *root.borrow_mut() = 0);
    APP_POPUPS.with(|list| list.borrow_mut().clear());
    drop(site_guard.take());
    crate::logv!(
        "[menu] child shell={} count={} app={} keywords={} chosen={}",
        menu.is_some(),
        unsafe { GetMenuItemCount(Some(hmenu)) },
        map.len(),
        job.hide_keywords.len(),
        chosen
    );

    // 5. 分发结果
    let mut handed_to_shell = false;
    if chosen == 0 {
        wire::emit(wire::EV_DISMISSED);
    } else if let Some(key) = map.get(&chosen) {
        wire::emit(&format!("{}{key}", wire::EV_APP));
    } else if let Some(active) = &menu {
        // Shell 在自己的进程里弹确认/进度框；不先放行的话它们拿不到前台权（会藏在后面）。
        let _ = unsafe { AllowSetForegroundWindow(ASFW_ANY) };
        let verb = canonical_verb(active, chosen);
        let single_dir = match &job.target {
            Target::Items(paths) if paths.len() == 1 => {
                paths.first().filter(|p| p.is_dir()).cloned()
            }
            _ => None,
        };
        // 交给 Shell 处理时的“当前文件夹”：背景菜单就是该目录，
        // 选中项就是它所在的目录（与资源管理器一致）。
        let shell_dir = match &job.target {
            Target::Background(dir) => Some(dir.clone()),
            Target::Items(paths) => paths.first().map(|p| {
                if p.is_dir() {
                    p.clone()
                } else {
                    p.parent().map(|d| d.to_path_buf()).unwrap_or_else(|| p.clone())
                }
            }),
            _ => None,
        };
        match verb.as_deref() {
            Some(v) if v.eq_ignore_ascii_case("rename") => wire::emit(wire::EV_RENAME),
            Some(v) if v.eq_ignore_ascii_case("open") || v.eq_ignore_ascii_case("explore") => {
                match &single_dir {
                    // 文件夹的默认动作交回 Rove 自己导航（否则会开资源管理器）。
                    Some(dir) => wire::emit(&format!(
                        "{}{}",
                        wire::EV_NAVIGATE,
                        wire::escape_path(dir)
                    )),
                    None => {
                        wire::emit(wire::EV_BEGIN);
                        handed_to_shell = true;
                        let _ = invoke_shell(active, host, chosen, point, shell_dir.as_deref());
                        wire::emit(wire::EV_END);
                        wire::emit(wire::EV_REFRESH);
                    }
                }
            }
            _ => {
                wire::emit(wire::EV_BEGIN);
                handed_to_shell = true;
                let _ = invoke_shell(active, host, chosen, point, shell_dir.as_deref());
                wire::emit(wire::EV_END);
                wire::emit(wire::EV_REFRESH);
            }
        }
    } else {
        wire::emit(wire::EV_DISMISSED);
    }

    let _ = unsafe { DestroyMenu(hmenu) };
    drop(menu);

    // 剪切/复制是 OLE 延迟渲染：数据只活在本进程的 STA 里，必须 flush 一次，
    // 别的进程（包括 Rove 自己的粘贴）才读得到 CF_HDROP。
    flush_ole_clipboard();

    // 前台权还给 Rove，否则关菜单后键盘事件无人接收。
    // （必须在 invoke 之后：Shell 的确认/进度框要靠我们当前持有前台权才能弹到前面。）
    unsafe {
        if GetForegroundWindow() == host && IsWindow(Some(owner)).as_bool() {
            let _ = SetForegroundWindow(owner);
        }
    }
    handed_to_shell
}

/// 干活期间别退出：一直泵消息，直到本进程再没有可见窗口（进度框 / 确认框 /
/// 属性框都关掉了），或者到了硬上限。
///
/// 为什么要留这么一手：「发送到」「复制到」「删除到回收站」这些是 Shell 在
/// **调用方进程**里起的后台任务，进度框也属于它。菜单进程一退出，任务就跟着
/// 被砍断 —— 用户看到的是复制做到一半没了。
fn linger_for_shell_work(host: HWND) {
    use windows::Win32::System::Threading::GetCurrentProcessId;
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, EnumWindows, GetWindowThreadProcessId, IsWindowVisible, PeekMessageW,
        TranslateMessage, MSG, PM_REMOVE,
    };

    struct Scan {
        pid: u32,
        host: isize,
        count: usize,
    }
    unsafe extern "system" fn each_window(
        hwnd: HWND,
        lp: LPARAM,
    ) -> windows::Win32::Foundation::BOOL {
        let scan = unsafe { &mut *(lp.0 as *mut Scan) };
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        if pid == scan.pid
            && hwnd.0 as isize != scan.host
            && unsafe { IsWindowVisible(hwnd).as_bool() }
        {
            scan.count += 1;
        }
        windows::Win32::Foundation::BOOL(1)
    }

    let own_windows = || {
        let mut scan = Scan {
            pid: unsafe { GetCurrentProcessId() },
            host: host.0 as isize,
            count: 0,
        };
        unsafe { EnumWindows(Some(each_window), LPARAM(&mut scan as *mut Scan as isize)) }.ok();
        scan.count
    };

    // Shell 的进度框不是立刻出现的：先给 600ms，别刚 invoke 完就判定「没事」。
    let started = std::time::Instant::now();
    let grace = std::time::Duration::from_millis(600);
    let mut quiet_since: Option<std::time::Instant> = None;
    loop {
        // 泵自己的消息：Shell 的后台线程有时要等调用方的模态循环。
        let mut msg = MSG::default();
        while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() } {
            let _ = unsafe { TranslateMessage(&msg) };
            unsafe { DispatchMessageW(&msg) };
        }
        let busy = started.elapsed() < grace || own_windows() > 0;
        if busy {
            quiet_since = None;
        } else if let Some(since) = quiet_since {
            // 再确认 1.5 秒：进度框常常是「先关掉、再开一个」。
            if since.elapsed() >= std::time::Duration::from_millis(1_500) {
                break;
            }
        } else {
            quiet_since = Some(std::time::Instant::now());
        }
        if started.elapsed() >= std::time::Duration::from_secs(30 * 60) {
            // 硬上限：留一个僵尸不如让它走（正常情况下这里永远走不到）。
            crate::logal!("!! [menu] 菜单子进程滞留超过 30 分钟，退出");
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}
