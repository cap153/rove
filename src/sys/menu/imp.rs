//! Windows 实现：`IContextMenu` 构建 + 专用 STA 菜单线程。
//!
//! 对外类型与入口见父模块 `super`。

use std::cell::RefCell;
use std::collections::HashMap;
use std::mem::ManuallyDrop;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use super::{MenuAction, MenuNode, Options, Sink, Target};

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
    AppendMenuW, AllowSetForegroundWindow, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
    DeleteMenu, DestroyMenu, DispatchMessageW, GetCursorPos, GetForegroundWindow,
    GetMenuItemCount, GetMenuItemID, GetMenuState, GetMenuStringW, GetMessageW, GetSubMenu,
    IsWindow, PostMessageW, RegisterClassW, SendMessageTimeoutW, SetForegroundWindow,
    SetWindowPos, ShowWindow,
    TranslateMessage, TrackPopupMenuEx, HMENU, MSG, MF_BYPOSITION, MF_CHECKED, MF_GRAYED, MF_POPUP,
    MF_SEPARATOR, MF_STRING, SWP_NOACTIVATE, SWP_NOZORDER, SW_SHOWNOACTIVATE, TPM_RETURNCMD,
    GetClassNameW, SMTO_ABORTIFHUNG, SMTO_BLOCK, TPM_RIGHTBUTTON, WM_APP, WM_DRAWITEM, WM_INITMENUPOPUP, WM_MEASUREITEM, WM_MENUCHAR, WNDCLASSW,
    WS_EX_TOOLWINDOW, WS_POPUP,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, SetActiveWindow, SetFocus, VK_CONTROL, VK_SHIFT,
};

/// Shell 命令 id 区间（与 `QueryContextMenu` 的入参一致）。
const SHELL_FIRST: u32 = 1;
const SHELL_LAST: u32 = 0x6FFF;
/// 应用命令 id 起点。
const APP_FIRST: u32 = 0x7000;
/// `CMINVOKECOMMANDINFOEX` 的 `fMask` 位：`lpVerbW` 才是宽字符动词。
const CMIC_MASK_UNICODE: u32 = 0x4000;

/// 菜单线程的宿主窗口消息：有新的菜单请求。
const WM_APP_OPEN_MENU: u32 = WM_APP + 0x51;
/// 宿主窗口消息：存活探测。宿主窗口过程直接回 1，用来区分「忙」和「死了」。
const WM_APP_PING: u32 = WM_APP + 0x52;
/// 宿主窗口消息：切换菜单深浅色（wparam 1 = 深色 / 0 = 浅色）。
const WM_APP_SET_DARK: u32 = WM_APP + 0x53;
/// 菜单是否要跟随深色（由 UI 线程按当前主题写入）。
static DARK: AtomicBool = AtomicBool::new(false);
/// 宿主窗口是否已经挂上深色主题（避免每次弹菜单都重复调用）。
static HOST_DARK: AtomicBool = AtomicBool::new(false);
/// 进程级偏好当前是深还是浅（与 [`DARK`] 比对，避免重复调 `SetPreferredAppMode`）。
static MODE_DARK: AtomicBool = AtomicBool::new(false);

/// 进程级偏好是否**已经显式设过**。`SetPreferredAppMode` 的默认值是
/// 「跟随系统」——只要没设过，Shell 弹的任何菜单都按系统走，而不是按 Rove 的主题。
static MODE_SET: AtomicBool = AtomicBool::new(false);

/// `tray-icon` 那个隐藏窗口的句柄（类名固定 `tray_icon_app`）。托盘菜单的配色
/// 只认它自己的窗口，缓存下来免得每次 `FindWindowW`。
static TRAY_HWND: AtomicIsize = AtomicIsize::new(0);
/// 菜单**已经弹在屏幕上**却挂着：超过它才值得探测一次（正常操作不会超过）。
const MENU_STALL_MS: u64 = 1_500;
/// 构建阶段（菜单还没弹出来）的忍耐时限。
///
/// 正常建菜单几十毫秒。用户右键一次没反应，本能就是「再点一下」——那一下
/// 就是「卡住了」的信号，必须立刻换线程给他弹出来，而不是让他干等。
/// 以前这里写 20 秒：第一条右键被第三方扩展卡住后，之后二十秒里每次右键都
/// 被静默忽略，用户只会以为右键坏了，转头去任务管理器杀进程。
const MENU_BUILD_GRACE_MS: u64 = 800;
/// 探测超时：正常线程几毫秒就回，等 400ms 还没回就是真卡死了。
const PING_TIMEOUT_MS: u32 = 400;

// ---------------------------------------------------------------------------
// 菜单请求
// ---------------------------------------------------------------------------
struct Job {
    /// 投递这次请求时 BUSY 的代数。宿主线程被放弃重建后，老线程恢复也不许
    /// 复位新线程的 BUSY。
    gen: u64,
    target: Target,
    nodes: Vec<MenuNode>,
    options: Options,
    owner: isize,
    sink: Sink,
}

// ---------------------------------------------------------------------------
// 线程间状态
// ---------------------------------------------------------------------------

static JOBS: Mutex<Vec<Job>> = Mutex::new(Vec::new());
static HOST: AtomicIsize = AtomicIsize::new(0);
static START_LOCK: Mutex<()> = Mutex::new(());
/// 是否已经有菜单在 Track 中。第二轮右键不能排队——否则用户点外面关掉菜单后，
/// 排着的那份会迟一步跳出来。
static BUSY: AtomicBool = AtomicBool::new(false);
/// 本轮 BUSY 是哪一代菜单线程持有的。被放弃的那条老线程恢复后不许再改 BUSY，
/// 否则它会把新线程正在用的标记清掉（右键就会叠出第二个菜单）。
static BUSY_GEN: AtomicU64 = AtomicU64::new(0);
/// BUSY 是什么时候置上的（毫秒，进程启动起算）。用来判断「忙」还是「死了」。
static BUSY_SINCE_MS: AtomicU64 = AtomicU64::new(0);
/// 菜单线程代数：老线程恢复时不许覆盖 `HOST`。
static HOST_GEN: AtomicU64 = AtomicU64::new(0);
/// 已经放弃过多少条卡死的宿主线程。
///
/// 卡在第三方扩展里的线程**无法回收**（没有 API 能安全杀掉一个卡在原生调用里
/// 的线程），它会一直挂着：一根栈（保留 2MB 虚拟空间）+ 一个隐藏窗口 + 扩展
/// 自己持有的对象。CPU 占用为 0（它在等待），所以代价是内存不是速度。
/// 但也不能无限重建，否则反复右键会把它们攒起来 —— 超过上限就彻底停手，
/// 让用户重启 Rove。
static ABANDONED: AtomicU32 = AtomicU32::new(0);
/// 最多放弃几条卡死的宿主线程。
///
/// 现在判死很激进（0.8 秒），网络盘上「慢」会被当成「死」多放弃几条，
/// 所以留 6 条余量；每条只占 2MB 虚拟空间，用户重启一次就清零。
const MAX_ABANDONED: u32 = 6;
/// 菜单是否已经**弹到屏幕上**（`TrackPopupMenuEx` 已进入）。
///
/// 用来区分两种「没回应」：构建阶段被第三方扩展拖住（还没弹出来，稍后可能自己好）
/// 和菜单真的在屏幕上（这时线程在跑消息循环，探测一定能回）。判活/放弃前者要看
/// 更长的时限，否则会把「慢」当成「死」，然后把一个稍后自己弹出来的菜单丢掉。
static SHOWN: AtomicBool = AtomicBool::new(false);
/// 已经彻底停手（右键菜单不再可用）。UI 侧据此决定要不要找个时机静默重启进程。
static GAVE_UP: AtomicBool = AtomicBool::new(false);
/// 被放弃的那一代菜单任务。卡在第三方扩展里的线程**迟早会返回**，然后照样
/// 把菜单弹出来 —— 那会是一个没人要的幽灵菜单（用户点它还有效，更糟）。
/// 判死时把代数记在这儿，`run_menu` 弹出前对一下，被放弃的一律不弹。
static ABORTED_GEN: AtomicU64 = AtomicU64::new(0);

/// 进程启动起的毫秒数。
fn now_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START
        .get_or_init(Instant::now)
        .elapsed()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

/// 菜单线程当前是否还活着（挡住第三方菜单扩展把宿主线程拖死的情况）。
fn host_responsive(host: isize) -> bool {
    if host == 0 {
        return false;
    }
    let mut result = 0usize;
    let ok = unsafe {
        SendMessageTimeoutW(
            HWND(host as *mut _),
            WM_APP_PING,
            WPARAM(0),
            LPARAM(0),
            SMTO_ABORTIFHUNG | SMTO_BLOCK,
            PING_TIMEOUT_MS,
            Some(&mut result),
        )
    };
    ok.0 != 0
}

/// 只复位自己那一代：被放弃的老线程恢复后不许动新线程的标记。
fn end_busy(gen: u64) {
    if BUSY_GEN.load(Ordering::Acquire) == gen {
        BUSY.store(false, Ordering::Release);
    }
}

/// 右键菜单是不是已经彻底放弃了（卡死线程到上限）。只有这时才值得换个进程。
pub(crate) fn exhausted() -> bool {
    GAVE_UP.load(Ordering::Acquire)
}

/// UI 线程侧只读诊断：菜单挂着多久了（毫秒）。给看门狗用。
pub(crate) fn busy_for_ms() -> Option<u64> {
    BUSY.load(Ordering::Acquire)
        .then(|| now_ms().saturating_sub(BUSY_SINCE_MS.load(Ordering::Acquire)))
}

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

/// 弹出原生菜单。`owner` 是主窗口 HWND（用于菜单关闭后归还前台权）；
/// 菜单本身由内部专用线程的零面积窗口持有。返回 false 表示菜单线程没起来。
pub(crate) fn open(
    target: Target,
    nodes: Vec<MenuNode>,
    options: Options,
    owner: isize,
    sink: Sink,
) -> bool {
    // 上一份菜单还在 Track 中？正常情况直接忽略这次右键（连点两下不能叠出两个菜单）。
    // 但第三方菜单扩展（网盘 / 同步 / 杀毒）有时会在 `QueryContextMenu` 或子菜单
    // 展开里卡死，把整个宿主线程拖住——那时 BUSY 永远不会复位，右键从此打不开。
    // 所以挂得太久的要探测一次：没回应就放弃这条线程，换一条新的。
    if BUSY.load(Ordering::Acquire) {
        let age = now_ms().saturating_sub(BUSY_SINCE_MS.load(Ordering::Acquire));
        // 构建阶段（还没弹出来）被拖住是常见的「慢」，要更长的沉默才算死；
        // 有菜单在屏幕上时线程在跑消息循环，探测能回，根本走不到这里。
        let shown = SHOWN.load(Ordering::Acquire);
        let limit = if shown { MENU_STALL_MS } else { MENU_BUILD_GRACE_MS };
        // 构建阶段超时**不必再问 ping**：不少第三方扩展自己会泵消息，
        // `SendMessageTimeout` 照样能立刻回「我还活着」，于是「死了没」永远判成
        // 「活着」→ BUSY 永远不复位 → 之后每次右键都被静默忽略（真机日志：
        // menu_busy 从 21 秒一路涨到 76 秒，右键从此没反应，关窗口也销毁不掉）。
        let stuck_building = !shown;
        if age > limit && (stuck_building || !host_responsive(HOST.load(Ordering::Acquire))) {
            let abandoned = ABANDONED.fetch_add(1, Ordering::AcqRel) + 1;
            ABORTED_GEN.store(BUSY_GEN.load(Ordering::Acquire), Ordering::Release);
            // `SHOWN` 是进程级的：卡死在 `TrackPopupMenuEx` 里的线程永远不会复位
            // 它，留着 true 就会让后面每次右键都按「菜单已在屏幕上」的时限判活。
            SHOWN.store(false, Ordering::Release);
            HOST.store(0, Ordering::Release);
            BUSY.store(false, Ordering::Release);
            if abandoned > MAX_ABANDONED {
                // 卡死的线程回收不了，攒多了只会白占内存：彻底停手，别再造新的。
                GAVE_UP.store(true, Ordering::Release);
                crate::logal!(
                    "[menu] 已放弃 {abandoned} 条卡死的菜单线程，不再重建（右键菜单暂停，关窗时自动重启可恢复）"
                );
                return false;
            }
            crate::logal!(
                "[menu] 菜单线程无响应 {age}ms（多半是第三方菜单扩展卡死），重建线程（第 {abandoned} 条）"
            );
        } else {
            return true;
        }
    }
    // 必须在 UI 线程就把 BUSY 置上：否则连点两次右键会在菜单线程开工前
    // 排进去两个请求，关掉第一个后第二个又自己跳出来。
    if BUSY.swap(true, Ordering::AcqRel) {
        return true;
    }
    let gen = BUSY_GEN.fetch_add(1, Ordering::AcqRel) + 1;
    BUSY_SINCE_MS.store(now_ms(), Ordering::Release);
    let Some(host) = ensure_host() else {
        crate::logln!("[menu] 原生菜单线程不可用，本次右键被忽略");
        end_busy(gen);
        return false;
    };
    JOBS.lock().unwrap_or_else(|e| e.into_inner()).push(Job {
        gen,
        target,
        nodes,
        options,
        owner,
        sink,
    });
    let ok = unsafe {
        PostMessageW(
            Some(HWND(host as *mut _)),
            WM_APP_OPEN_MENU,
            WPARAM(0),
            LPARAM(0),
        )
    };
    if ok.is_err() {
        end_busy(gen);
        return false;
    }
    true
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
            return if handled { Some(result) } else { None };
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
    if handled {
        Some(result)
    } else {
        None
    }
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
#[cfg(windows)]
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
        fn GetFolder(
            &self,
            _riid: *const GUID,
            _ppv: *mut *mut c_void,
        ) -> WinResult<()> {
            Err(windows::core::Error::from_hresult(E_NOTIMPL))
        }
        fn Item(&self, _itemindex: i32) -> WinResult<*mut ITEMIDLIST> {
            Err(windows::core::Error::from_hresult(E_NOTIMPL))
        }
        fn ItemCount(&self, _flags: &_SVGIO) -> WinResult<i32> {
            Err(windows::core::Error::from_hresult(E_NOTIMPL))
        }
        fn Items(
            &self,
            _flags: &_SVGIO,
            riid: *const GUID,
            ppv: *mut *mut c_void,
        ) -> WinResult<()> {
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

    pub(super) fn attach(menu: &windows::Win32::UI::Shell::IContextMenu, hwnd: HWND, dir: &Path) -> Option<SiteGuard> {
        let pidl = super::path_pidl(dir).ok()?;
        let guard = super::Pidls(vec![pidl]);
        let item: IShellItem = unsafe { SHCreateItemFromIDList(guard.0[0]) }.ok()?;
        let unknown: IUnknown = MenuSite {
            hwnd,
            item,
        }
        .into();
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

// ---------------------------------------------------------------------------
// 菜单线程
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// 深色菜单（uxtheme 的未公开序号；任何一步拿不到就整体退回浅色）
// ---------------------------------------------------------------------------

/// 取 `uxtheme.dll` 里的未公开导出（按序号）。
///
/// 序号：133 `AllowDarkModeForWindow` / 135 `SetPreferredAppMode` /
/// 136 `FlushMenuThemes`。这几个接口微软从 Win10 1809 起就一直有，但从未
/// 公开——所以全程尽力而为：取不到就是浅色，绝不让菜单因此弹不出来。
fn uxtheme_export(ordinal: usize) -> Option<unsafe extern "system" fn() -> isize> {
    use windows::core::{w, PCSTR};
    use windows::Win32::System::LibraryLoader::{
        GetModuleHandleW, GetProcAddress, LoadLibraryW,
    };
    unsafe {
        // 先找已加载的模块；没有就主动加载（uxtheme 通常早就被通用控件带进来了）。
        let module = match GetModuleHandleW(w!("uxtheme.dll")) {
            Ok(module) => module,
            Err(_) => LoadLibraryW(w!("uxtheme.dll")).ok()?,
        };
        GetProcAddress(module, PCSTR(ordinal as *const u8))
    }
}

/// 进程级深浅偏好（`SetPreferredAppMode`，0=Default 1=AllowDark 2=ForceDark 3=ForceLight）。
fn set_preferred_mode(dark: bool) -> bool {
    let Some(export) = uxtheme_export(135) else {
        return false;
    };
    let set_mode: unsafe extern "system" fn(i32) -> i32 = unsafe { std::mem::transmute(export) };
    unsafe { set_mode(if dark { 2 } else { 3 }) };
    MODE_DARK.store(dark, Ordering::Release);
    MODE_SET.store(true, Ordering::Release);
    flush_menu_themes();
    true
}

/// 让菜单主题缓存失效，下一次弹出才吃新设置。
fn flush_menu_themes() {
    if let Some(export) = uxtheme_export(136) {
        let flush: unsafe extern "system" fn() = unsafe { std::mem::transmute(export) };
        unsafe { flush() };
    }
}

/// 给任意窗口挂上（或摘掉）深色主题（不改 Rove 自己的宿主状态）。
fn apply_dark_flag(hwnd: HWND, dark: bool) {
    unsafe {
        if let Some(export) = uxtheme_export(133) {
            let allow: unsafe extern "system" fn(HWND, i32) -> i32 =
                std::mem::transmute(export);
            allow(hwnd, dark as i32);
        }
        // Win10 认 "DarkMode_Explorer"；空串 = 恢复系统默认主题。
        let theme = if dark {
            windows::core::w!("DarkMode_Explorer")
        } else {
            windows::core::PCWSTR::null()
        };
        let _ = windows::Win32::UI::Controls::SetWindowTheme(
            hwnd,
            theme,
            windows::core::PCWSTR::null(),
        );
        flush_menu_themes();
    }
}

/// 给 Rove 自己的菜单宿主窗口挂主题，并记下状态。
fn theme_host(hwnd: HWND, dark: bool) {
    apply_dark_flag(hwnd, dark);
    HOST_DARK.store(dark, Ordering::Release);
}

/// 让托盘右键菜单跟随 Rove 的主题。
///
/// 托盘菜单是 `tray-icon` / `muda` 用普通 `TrackPopupMenu` 弹的，配色取决于两件事：
/// 进程级 `SetPreferredAppMode`，以及**拥有窗口**（`tray_icon_app` 隐藏窗口）的
/// 深色允许位。后者它自己从不设，所以要么继承进程状态，要么跟着 Windows 走。
pub fn apply_tray_theme() {
    let dark = DARK.load(Ordering::Acquire);
    let mut hwnd = TRAY_HWND.load(Ordering::Acquire);
    if hwnd == 0 {
        // 托盘图标还没建出来（`apply_theme` 早于 `tray::spawn`）：托管给托盘线程，
        // 它建完图标会自己调一次本函数。
        match find_own_tray_window() {
            Some(found) => {
                hwnd = found;
                TRAY_HWND.store(hwnd, Ordering::Release);
            }
            None => return,
        }
    }
    apply_dark_flag(HWND(hwnd as _), dark);
}

/// 找 `tray-icon` 的隐藏窗口，**必须是本进程的**。
///
/// 类名 `tray_icon_app` 是 `tray-icon` 库写死的，同一个桌面上别的程序
/// （很多托盘程序也用这个库）可能也有同名窗口。`FindWindowW` 会返回第一个
/// 命中的，可能不是我们的 —— 那就会去给别人的窗口改主题，自己的托盘菜单
/// 反而没变。所以枚举顶层窗口，按进程号过滤。
fn find_own_tray_window() -> Option<isize> {
    use windows::Win32::Foundation::BOOL;
    use windows::Win32::System::Threading::GetCurrentProcessId;
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowThreadProcessId, IsWindowVisible,
    };

    struct Ctx {
        class: Vec<u16>,
        pid: u32,
        found: isize,
    }
    let mut class: Vec<u16> = "tray_icon_app".encode_utf16().collect();
    class.push(0);
    let mut ctx = Ctx {
        class,
        pid: unsafe { GetCurrentProcessId() },
        found: 0,
    };
    unsafe extern "system" fn each(hwnd: HWND, lp: LPARAM) -> BOOL {
        let ctx = unsafe { &mut *(lp.0 as *mut Ctx) };
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        if pid != ctx.pid {
            return BOOL(1);
        }
        let want = ctx.class.len() - 1;
        let mut buf = [0u16; 32];
        let n = unsafe { GetClassNameW(hwnd, &mut buf) } as usize;
        if n > 0 && n == want && buf[..want] == ctx.class[..want] {
            // 托盘窗口是隐藏的：不看可见性，有句柄就够。
            let _ = unsafe { IsWindowVisible(hwnd) };
            ctx.found = hwnd.0 as isize;
            return BOOL(0);
        }
        BOOL(1)
    }
    unsafe { EnumWindows(Some(each), LPARAM(&mut ctx as *mut Ctx as isize)) }.ok();
    (ctx.found != 0).then_some(ctx.found)
}

/// 让原生菜单跟随主题（UI 线程调用；真正的绘制在菜单线程上）。
///
/// 应急退路：设置环境变量 `ROVE_NO_DARK_MENU=1` 后一律浅色菜单。
pub fn set_dark(dark: bool) {
    let dark = dark && std::env::var_os("ROVE_NO_DARK_MENU").is_none();
    // 第一次即使值等于默认的 `false` 也不能提前返回：进程默认是「跟随系统」，
    // 用户「Rove 亮色 + Windows 深色」时不设就一直是深色菜单。
    let first = !MODE_SET.swap(true, Ordering::AcqRel);
    crate::logv!("[menu] set_dark dark={} first={}", dark, first);
    if DARK.swap(dark, Ordering::AcqRel) == dark && !first {
        return;
    }
    if first {
        let _ = set_preferred_mode(dark);
        apply_tray_theme();
        return;
    }
    // 还没弹过菜单：宿主窗口建起来时会自己按 DARK 决定，不必在这里做什么。
    let hwnd = HOST.load(Ordering::Acquire);
    if hwnd != 0 {
        unsafe {
            let _ = PostMessageW(
                Some(HWND(hwnd as _)),
                WM_APP_SET_DARK,
                WPARAM(dark as usize),
                LPARAM(0),
            );
        }
    }
    apply_tray_theme();
}

fn ensure_host() -> Option<isize> {
    let existing = HOST.load(Ordering::Acquire);
    if existing != 0 {
        return Some(existing);
    }
    let _guard = START_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let existing = HOST.load(Ordering::Acquire);
    if existing != 0 {
        return Some(existing);
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let gen = HOST_GEN.fetch_add(1, Ordering::AcqRel) + 1;
    std::thread::Builder::new()
        .name("rove-menu".into())
        .spawn(move || host_thread(tx, gen))
        .ok()?;
    rx.recv_timeout(Duration::from_secs(5)).ok().flatten()
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

fn host_thread(ready: std::sync::mpsc::Sender<Option<isize>>, gen: u64) {
    unsafe {
        // OleInitialize = STA COM + OLE（剪贴板延迟渲染靠它）。
        let _ = OleInitialize(None);
        let class_name = windows::core::w!("Rove.NativeMenuHost");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(host_proc),
            lpszClassName: class_name,
            ..Default::default()
        };
        // 重建线程的情况：类名已被（卡死的那条）老线程注册过，注册会失败，
        // 但类是全进程有效的，照样能 CreateWindowExW。
        let atom = RegisterClassW(&wc);
        let class_ok = atom != 0
            || windows::Win32::Foundation::GetLastError()
                == windows::Win32::Foundation::ERROR_CLASS_ALREADY_EXISTS;
        // 必须在**这条线程的第一个窗口建出来之前**设：`SetPreferredAppMode`
        // 对已经创建过的窗口不追溯，晚一步菜单就还是浅色。
        //
        // 注意浅色时也要显式设成 ForceLight：进程默认值是「跟随系统」，
        // 用户把 Rove 钉成亮色而 Windows 是深色时，不设就还是深色菜单。
        let want_dark = DARK.load(Ordering::Acquire);
        let _ = set_preferred_mode(want_dark);
        let hwnd = if class_ok {
            CreateWindowExW(
                // 注意：不能加 WS_EX_NOACTIVATE —— 激活不了就拿不到键盘焦点，
                // 菜单的方向键 / 回车 / Esc 全会失效。零面积弹出窗 + 菜单
                // 关闭后归还前台权（见 run_menu）。
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
        } else {
            None
        };
        let Some(hwnd) = hwnd else {
            let _ = ready.send(None);
            return;
        };
        theme_host(hwnd, want_dark);
        if HOST_GEN.load(Ordering::Acquire) == gen {
            HOST.store(hwnd.0 as isize, Ordering::Release);
        }
        let _ = ready.send(Some(hwnd.0 as isize));

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

unsafe extern "system" fn host_proc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
) -> LRESULT {
    if let Some(result) = forward_menu_message(msg, wp, lp) {
        return result;
    }
    if msg == WM_APP_PING {
        // 回非 0：`SendMessageTimeoutW` 靠这个区分「活着但忙」和「超时没回」。
        return LRESULT(1);
    }
    if msg == WM_APP_SET_DARK {
        let dark = wp.0 != 0;
        let _ = set_preferred_mode(dark);
        theme_host(hwnd, dark);
        return LRESULT(0);
    }
    if msg == WM_APP_OPEN_MENU {
        // 一次性取走全部入队请求，只执行最新那一条（旧的在 BUSY 竞态下
        // 可能残留，不能让它延迟弹出来）。
        let job = JOBS.lock().unwrap_or_else(|e| e.into_inner()).drain(..).last();
        if let Some(job) = job {
            run_menu(hwnd, job);
        } else {
            BUSY.store(false, Ordering::Release);
        }
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
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
    // 父文件夹相同 = 绝对 PIDL 的父前缀字节完全一致。
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

/// 回收站里的「彻底删除」：在**回收站命名空间**上执行 Shell 的 `delete` 动词。
///
/// 为什么不能在 `C:\$Recycle.Bin\...` 的物理路径上调 `SHFileOperationW` /
/// `IFileOperation`：那样 Shell 把条目当普通文件处理，一声不响就抹掉了，
/// 资源管理器那句「确实要永久删除此文件吗？」根本不会出现。真正弹确认框的是
/// 回收站文件夹自己的删除处理器，所以要拿到 `FOLDERID_RecycleBinFolder` 的
/// `IShellFolder`，把命中的子项 PIDL 交给它取 `IContextMenu` 再触发 `delete`。
///
/// `paths` 里既可能是 `$R` 物理路径（`trash` 库给的 id），也可能是命名空间解析名。
/// 匹配先按 `$R` 文件名（每条唯一），退而按显示名（原名）。
///
/// 返回投递给 Shell 的条目数；用户点「否」时命令返回失败，所以结果仍以
/// 「条目还在不在」为准。
pub(crate) fn purge_recycle_items(paths: &[PathBuf], owner: isize) -> usize {
    use windows::Win32::UI::Shell::{
        IShellItem, IShellFolder, IEnumIDList, SHCreateItemWithParent, SHGetKnownFolderItem,
        BHID_SFObject, FOLDERID_RecycleBinFolder, KF_FLAG_DEFAULT, SHCONTF_FOLDERS,
        SHCONTF_NONFOLDERS, SIGDN_DESKTOPABSOLUTEPARSING, SIGDN_NORMALDISPLAY,
    };
    if paths.is_empty() {
        return 0;
    }
    let folder_item: IShellItem =
        match unsafe { SHGetKnownFolderItem(&FOLDERID_RecycleBinFolder, KF_FLAG_DEFAULT, None) } {
            Ok(item) => item,
            Err(e) => {
                crate::logal!("!! menu: 打不开回收站 {:?}", e.code());
                return 0;
            }
        };
    let folder: IShellFolder = match unsafe { folder_item.BindToHandler(None, &BHID_SFObject) } {
        Ok(folder) => folder,
        Err(e) => {
            crate::logal!("!! menu: 回收站不是 IShellFolder {:?}", e.code());
            return 0;
        }
    };
    let host = HWND(owner as *mut core::ffi::c_void);
    let mut list: Option<IEnumIDList> = None;
    if unsafe { folder.EnumObjects(host, (SHCONTF_FOLDERS.0 | SHCONTF_NONFOLDERS.0) as u32, &mut list) }.is_err()
    {
        return 0;
    }
    let Some(list) = list else {
        return 0;
    };

    // (子 PIDL, 解析名, 显示名)
    //
    // `IEnumIDList::Next` 给的子项 PIDL 是 `CoTaskMemAlloc` 出来的，调用方必须
    // 自己还 —— 这里交给守卫，提前 return 也不会漏。
    let mut owned = ChildPidls(Vec::new());
    let mut children: Vec<(*mut ITEMIDLIST, String, String)> = Vec::new();
    loop {
        let mut buf: [*mut ITEMIDLIST; 1] = [std::ptr::null_mut()];
        let mut fetched = 0u32;
        if unsafe { list.Next(&mut buf, Some(&mut fetched)) }.is_err() || fetched == 0 {
            break;
        }
        let child = buf[0];
        if child.is_null() {
            break;
        }
        owned.0.push(child);
        let names = unsafe { SHCreateItemWithParent::<_, IShellItem>(None, &folder, child) }
            .ok()
            .and_then(|item| {
                let parsing = shell_item_name(&item, SIGDN_DESKTOPABSOLUTEPARSING)?;
                let display = shell_item_name(&item, SIGDN_NORMALDISPLAY)?;
                Some((parsing, display))
            });
        match names {
            Some((parsing, display)) => children.push((child, parsing, display)),
            None => children.push((child, String::new(), String::new())),
        }
    }

    let tail = |path: &Path| path.file_name().map(|n| n.to_string_lossy().to_string());
    let mut hit: Vec<*const ITEMIDLIST> = Vec::new();
    for path in paths {
        let Some(name) = tail(path) else { continue };
        // 先按 `$R` 文件名（唯一）匹配；同名副本再多也不会误伤。
        let found = children
            .iter()
            .find(|(_, parsing, _)| parsing.rsplit('\\').next() == Some(name.as_str()))
            .or_else(|| {
                children
                    .iter()
                    .find(|(_, _, display)| display.eq_ignore_ascii_case(&name))
            });
        if let Some((pidl, _, _)) = found {
            let pidl = *pidl as *const ITEMIDLIST;
            // 两个键命中同一条（显示名回退撞车）时只能交一次，否则 Shell 会
            // 把同一条删两遍、真正想要的那条反而留下。
            if !hit.contains(&pidl) {
                hit.push(pidl);
            }
        }
    }
    if hit.is_empty() {
        crate::logal!("!! menu: 回收站里没匹配到要删除的条目");
        return 0;
    }

    let menu: IContextMenu = match unsafe { folder.GetUIObjectOf(host, &hit, None) } {
        Ok(menu) => menu,
        Err(e) => {
            crate::logal!("!! menu: 回收站取不到 IContextMenu {:?}", e.code());
            return 0;
        }
    };
    let verb = "delete";
    let ansi: Vec<u8> = verb.bytes().chain(Some(0)).collect();
    let wide: Vec<u16> = verb.encode_utf16().chain(Some(0)).collect();
    let info = CMINVOKECOMMANDINFOEX {
        cbSize: std::mem::size_of::<CMINVOKECOMMANDINFOEX>() as u32,
        fMask: CMIC_MASK_UNICODE,
        hwnd: host,
        lpVerb: PCSTR(ansi.as_ptr()),
        nShow: 1, // SW_SHOWNORMAL
        lpVerbW: PCWSTR(wide.as_ptr()),
        ..Default::default()
    };
    let result = unsafe {
        menu.InvokeCommand((&info as *const CMINVOKECOMMANDINFOEX).cast::<CMINVOKECOMMANDINFO>())
    };
    crate::logv!(
        "[menu] purge_recycle n={} hr={:?}",
        hit.len(),
        result.as_ref().err().map(|e| e.code().0)
    );
    hit.len()
}

/// 一批由 Shell 分配的子项 PIDL，析构时统一还给 `CoTaskMemFree`。
struct ChildPidls(Vec<*mut ITEMIDLIST>);

impl Drop for ChildPidls {
    fn drop(&mut self) {
        for pidl in self.0.drain(..) {
            if !pidl.is_null() {
                unsafe { CoTaskMemFree(Some(pidl as *const core::ffi::c_void)) };
            }
        }
    }
}

/// `IShellItem` 显示名（调用方负责释放 `CoTaskMem` 那块内存）。
fn shell_item_name(item: &windows::Win32::UI::Shell::IShellItem, kind: windows::Win32::UI::Shell::SIGDN) -> Option<String> {
    let raw = unsafe { item.GetDisplayName(kind) }.ok()?;
    let text = unsafe { raw.to_string() }.ok();
    unsafe { CoTaskMemFree(Some(raw.0 as *const core::ffi::c_void)) };
    text
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
    // 视图的背景菜单对象才会转发，所以优先走视图（不需要真建窗口，
    // 跟 zsdesk 借背景菜单的做法一致）。
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
///
/// 只看菜单文字：不再为每一项发 `GetCommandString` 跨进程询问，
/// 系统自带的复制/剪切/粘贴/刷新也靠默认关键词藏掉。
/// 应用项（`0x7000+`）不动。
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
        let label = super::normalize_label(&menu_label(hmenu, index));
        if !label.is_empty() && super::keyword_hit(&label, &options.hide_keywords) {
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

/// 菜单线程的 OLE 剪贴板延迟渲染落盘（仅当剪贴板属主是本线程）。
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
    hmenu: windows::Win32::UI::WindowsAndMessaging::HMENU,
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
fn trim_separators(hmenu: windows::Win32::UI::WindowsAndMessaging::HMENU) {
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

fn run_menu(host: HWND, job: Job) {
    let _busy = BusyGuard(job.gen);
    if ABORTED_GEN.load(Ordering::Acquire) == job.gen {
        // 这条已经被判死、宿主线程也被换掉了：别再弹（幽灵菜单）。
        crate::logv!("[menu] gen={} 已放弃，跳过弹出", job.gen);
        return;
    }
    // 深浅色兜底对齐：UI 线程切主题的那一刻宿主窗口可能还没建好（`HOST` 还是 0），
    // 那条切换消息就丢了。每次弹菜单前对一次，代价只是一次原子比较。
    let want_dark = DARK.load(Ordering::Acquire);
    if MODE_DARK.load(Ordering::Acquire) != want_dark {
        let _ = set_preferred_mode(want_dark);
    }
    if HOST_DARK.load(Ordering::Acquire) != want_dark {
        theme_host(host, want_dark);
    }
    // `owner`（Rove 主窗口）只用来在菜单关闭后归还前台权。
    // Shell 菜单本身的 hwnd 一律用宿主窗口：Shell 弹的属性/确认框都归属于它，
    // 而宿主窗口被摆到鼠标位置，对话框就落在鼠标附近而不是屏幕左上角。
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
        Err(_) => return,
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
            // 这样「打开方式 / 发送到 / 属性 / 扫描 / 共享 / 网盘同步」这些由父文件夹
            // 聚合出来的项才会出现，且不会把文件误当成命名空间项（那样会冒出「管理」）。
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
        if job.options.extended {
            flags |= CMF_EXTENDEDVERBS;
        }
        let _ = unsafe { active.QueryContextMenu(hmenu, 0, SHELL_FIRST, SHELL_LAST, flags) };
        filter_hidden(hmenu, &job.options);
    }

    // 3. 追加应用命令
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

    // 判死只要 0.8 秒，而慢扩展往往就慢在 `QueryContextMenu` 之后 —— 弹之前
    // 确认一次：这中间用户又右键了、这条已经被放弃，就别把它弹出来（幽灵菜单）。
    // 必须放在「挂 ACTIVE / 显示宿主窗口」之前：提前 return 会漏掉 `DestroyMenu`
    // 和 ACTIVE 复位（弹出菜单句柄泄漏，宿主窗口还会被摆到鼠标位置）。
    if ABORTED_GEN.load(Ordering::Acquire) == job.gen {
        crate::logv!("[menu] gen={} 建好时已被放弃，不弹", job.gen);
        let _ = unsafe { DestroyMenu(hmenu) };
        return;
    }

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
    ACTIVE_OPTS.with(|opts| *opts.borrow_mut() = Some(job.options.clone()));
    ACTIVE_ROOT.with(|root| *root.borrow_mut() = hmenu.0 as isize);
    let flags = TPM_RETURNCMD.0 | TPM_RIGHTBUTTON.0;
    SHOWN.store(true, Ordering::Release);
    let chosen = unsafe { TrackPopupMenuEx(hmenu, flags, point.x, point.y, host, None) }.0 as u32;
    SHOWN.store(false, Ordering::Release);
    ACTIVE_OPTS.with(|opts| *opts.borrow_mut() = None);
    ACTIVE.with(|active| *active.borrow_mut() = None);
    ACTIVE_VIEW.with(|slot| *slot.borrow_mut() = None);
    ACTIVE_ROOT.with(|root| *root.borrow_mut() = 0);
    APP_POPUPS.with(|list| list.borrow_mut().clear());
    drop(site_guard.take());
    crate::logv!(
        "[menu] shell={} count={} app={} keywords={} chosen={}",
        menu.is_some(),
        unsafe { GetMenuItemCount(Some(hmenu)) },
        map.len(),
        job.options.hide_keywords.len(),
        chosen
    );


    // 5. 分发结果
    if chosen != 0 {
        // Shell 在自己的进程里弹确认/进度框；不先放行的话它们拿不到前台权（会藏在后面）。
        let _ = unsafe { AllowSetForegroundWindow(0xFFFF_FFFF) };
        if let Some(key) = map.get(&chosen) {
            (job.sink)(MenuAction::App(key.clone()));
        } else if let Some(active) = &menu {
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
                Some(v) if v.eq_ignore_ascii_case("rename") => (job.sink)(MenuAction::Rename),
                Some(v)
                    if v.eq_ignore_ascii_case("open") || v.eq_ignore_ascii_case("explore") =>
                {
                    match &single_dir {
                        // 文件夹的默认动作交回 Rove 自己导航（否则会开资源管理器）。
                        Some(dir) => (job.sink)(MenuAction::Navigate(dir.clone())),
                        None => {
                            (job.sink)(MenuAction::ShellOpBegin);
                            let _ = invoke_shell(active, host, chosen, point, shell_dir.as_deref());
                            (job.sink)(MenuAction::ShellOpEnd);
                            (job.sink)(MenuAction::Refresh);
                        }
                    }
                }
                _ => {
                    (job.sink)(MenuAction::ShellOpBegin);
                    let _ = invoke_shell(active, host, chosen, point, shell_dir.as_deref());
                    (job.sink)(MenuAction::ShellOpEnd);
                    (job.sink)(MenuAction::Refresh);
                }
            }
        }
    }

    let _ = unsafe { DestroyMenu(hmenu) };
    drop(menu);

    // 剪切/复制是 OLE 延迟渲染：数据只活在本线程的 STA 里，必须 flush 一次，
    // 别的进程（包括 Rove 自己的粘贴）才读得到 CF_HDROP。
    flush_ole_clipboard();

    // 前台权还给 Rove，否则关菜单后键盘事件无人接收。
    // （必须在 invoke 之后：Shell 的确认/进度框要靠我们当前持有前台权才能弹到前面。）
    unsafe {
        if GetForegroundWindow() == host && IsWindow(Some(owner)).as_bool() {
            let _ = SetForegroundWindow(owner);
        }
    }
}

/// `BUSY` 标记的守卫：任何返回路径都会复位。
struct BusyGuard(u64);
impl Drop for BusyGuard {
    fn drop(&mut self) {
        end_busy(self.0);
    }
}
