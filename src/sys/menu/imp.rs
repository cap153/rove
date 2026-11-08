//! Windows 实现：`IContextMenu` 构建 + 专用 STA 菜单线程。
//!
//! 对外类型与入口见父模块 `super`。

use std::cell::RefCell;
use std::collections::HashMap;
use std::mem::ManuallyDrop;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use super::{MenuAction, MenuNode, Sink, Target};

use windows::core::{Interface, PCSTR, PCWSTR, PSTR, Result as WinResult};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Ole::OleInitialize;
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    IContextMenu, IContextMenu2, IContextMenu3, IShellFolder, SHBindToParent,
    SHCreateDefaultContextMenu, SHGetDesktopFolder, SHParseDisplayName, CMF_CANRENAME, CMF_EXPLORE,
    CMF_EXTENDEDVERBS, CMF_NORMAL, CMIC_MASK_CONTROL_DOWN, CMIC_MASK_PTINVOKE,
    CMIC_MASK_SHIFT_DOWN, CMINVOKECOMMANDINFO, CMINVOKECOMMANDINFOEX, DEFCONTEXTMENU, GCS_VERBW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, AllowSetForegroundWindow, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
    DeleteMenu, DestroyMenu, DispatchMessageW, GetCursorPos, GetForegroundWindow,
    GetMenuItemCount, GetMenuItemID, GetMenuState, GetMenuStringW, GetMessageW, GetSubMenu,
    IsWindow, PostMessageW, RegisterClassW, SetForegroundWindow, SetWindowPos, ShowWindow,
    TranslateMessage, TrackPopupMenuEx, HMENU, MSG, MF_BYPOSITION, MF_CHECKED, MF_GRAYED, MF_POPUP,
    MF_SEPARATOR, MF_STRING, SWP_NOACTIVATE, SWP_NOZORDER, SW_SHOWNOACTIVATE, TPM_RETURNCMD,
    TPM_RIGHTBUTTON, WM_APP, WM_DRAWITEM, WM_INITMENUPOPUP, WM_MEASUREITEM, WM_MENUCHAR, WNDCLASSW,
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

// ---------------------------------------------------------------------------
// 菜单请求
// ---------------------------------------------------------------------------
struct Job {
    target: Target,
    nodes: Vec<MenuNode>,
    hidden_verbs: Vec<String>,
    extended: bool,
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
static BUSY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

thread_local! {
    /// 当前这个线程上正在 Track 的菜单：四条菜单消息要转发给它。
    static ACTIVE: RefCell<Option<IContextMenu>> = const { RefCell::new(None) };
}

/// 弹出原生菜单。`owner` 是主窗口 HWND（用于菜单关闭后归还前台权）；
/// 菜单本身由内部专用线程的零面积窗口持有。返回 false 表示菜单线程没起来。
pub(crate) fn open(
    target: Target,
    nodes: Vec<MenuNode>,
    hidden_verbs: Vec<String>,
    extended: bool,
    owner: isize,
    sink: Sink,
) -> bool {
    // 必须在 UI 线程就把 BUSY 置上：否则连点两次右键会在菜单线程开工前
    // 排进去两个请求，关掉第一个后第二个又自己跳出来。
    if BUSY.swap(true, Ordering::AcqRel) {
        return true;
    }
    let Some(host) = ensure_host() else {
        crate::logln!("[menu] 原生菜单线程不可用，本次右键被忽略");
        BUSY.store(false, Ordering::Release);
        return false;
    };
    JOBS.lock().unwrap_or_else(|e| e.into_inner()).push(Job {
        target,
        nodes,
        hidden_verbs,
        extended,
        owner,
        sink,
    });
    let ok = unsafe { PostMessageW(Some(HWND(host as *mut _)), WM_APP_OPEN_MENU, WPARAM(0), LPARAM(0)) };
    if ok.is_err() {
        BUSY.store(false, Ordering::Release);
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
    if let Ok(menu3) = menu.cast::<IContextMenu3>() {
        let mut result = LRESULT(0);
        if unsafe { menu3.HandleMenuMsg2(msg, wp, lp, Some(&mut result)) }.is_ok() {
            return Some(result);
        }
    }
    if let Ok(menu2) = menu.cast::<IContextMenu2>() {
        if unsafe { menu2.HandleMenuMsg(msg, wp, lp) }.is_ok() {
            return Some(LRESULT(0));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// 菜单线程
// ---------------------------------------------------------------------------

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
    std::thread::Builder::new()
        .name("rove-menu".into())
        .spawn(move || host_thread(tx))
        .ok()?;
    rx.recv_timeout(Duration::from_secs(5)).ok().flatten()
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

fn host_thread(ready: std::sync::mpsc::Sender<Option<isize>>) {
    unsafe {
        // OleInitialize = STA COM + OLE（剪贴板延迟渲染靠它）。
        let _ = OleInitialize(None);
        let class_name = windows::core::w!("Rove.NativeMenuHost");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(host_proc),
            lpszClassName: class_name,
            ..Default::default()
        };
        let atom = RegisterClassW(&wc);
        let hwnd = if atom != 0 {
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
        HOST.store(hwnd.0 as isize, Ordering::Release);
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

/// 目录空白处 → `IContextMenu`。
/// 必须绑定到该目录**本身**（`BindToObject`）再 `CreateViewObject`。
/// 用 `SHBindToParent` 会拿到它的父目录，结果是上一层的空白菜单（「新建」会建到上一级）。
fn folder_context_menu(host: HWND, dir: &Path) -> Option<IContextMenu> {
    let pidl = path_pidl(dir).ok()?;
    let pidls = Pidls(vec![pidl]);
    let desktop = unsafe { SHGetDesktopFolder() }.ok()?;
    let folder: IShellFolder = unsafe { desktop.BindToObject(pidls.0[0], None) }.ok()?;
    unsafe { folder.CreateViewObject::<IContextMenu>(host) }.ok()
}

/// 按用户开关删除 Shell 菜单项。
/// Shell 项只有 `1..=0x6FFF` 的才有命令字符串，应用项（`0x7000+`）不动。
fn filter_hidden(hmenu: HMENU, ctx: Option<&IContextMenu>, hidden: &[String]) {
    if hidden.is_empty() {
        return;
    }
    let count = unsafe { GetMenuItemCount(Some(hmenu)) }.max(0);
    for index in (0..count).rev() {
        let id = unsafe { GetMenuItemID(hmenu, index) };
        if id >= APP_FIRST && id != u32::MAX {
            continue;
        }
        let label = menu_label(hmenu, index);
        let verb = ctx.and_then(|c| canonical_verb(c, id));
        if should_hide(verb.as_deref(), &label, hidden) {
            let _ = unsafe { DeleteMenu(hmenu, index as u32, MF_BYPOSITION) };
            continue;
        }
        let sub = unsafe { GetSubMenu(hmenu, index) };
        if !sub.is_invalid() {
            filter_hidden(sub, ctx, hidden);
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

fn should_hide(verb: Option<&str>, label: &str, hidden: &[String]) -> bool {
    if let Some(verb) = verb {
        let verb = verb.to_ascii_lowercase();
        if hidden.iter().any(|h| *h == verb) {
            return true;
        }
    }
    // 子菜单（发送到 / 打开方式）取不到命令字符串，只能按标签兜底。
    let label = normalize_label(label);
    if label.is_empty() {
        return false;
    }
    hidden
        .iter()
        .any(|h| label_aliases(h).iter().any(|alias| *alias == label))
}

/// 去掉助记符与省略号，小写化，便于跨语言比较。
fn normalize_label(label: &str) -> String {
    let mut s = label.replace('&', "").to_lowercase();
    if let Some(head) = s.split('\t').next() {
        s = head.to_string();
    }
    let s = s.trim().trim_end_matches('.').trim_end_matches('…').trim();
    s.to_string()
}

/// 取不到命令字符串的项（子菜单）用它认人。
fn label_aliases(verb: &str) -> &'static [&'static str] {
    match verb {
        "sendto" => &["send to", "发送到"],
        "openas" => &["open with", "打开方式"],
        _ => &[],
    }
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

fn invoke_shell(menu: &IContextMenu, owner: HWND, id: u32, point: POINT) -> WinResult<()> {
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
    let info = CMINVOKECOMMANDINFOEX {
        cbSize: std::mem::size_of::<CMINVOKECOMMANDINFOEX>() as u32,
        fMask: mask,
        hwnd: owner,
        lpVerb: PCSTR(offset as *const u8),
        nShow: 1, // SW_SHOWNORMAL
        lpVerbW: PCWSTR(offset as *const u16),
        ptInvoke: point,
        ..Default::default()
    };
    unsafe {
        menu.InvokeCommand((&info as *const CMINVOKECOMMANDINFOEX).cast::<CMINVOKECOMMANDINFO>())
    }
}

fn run_menu(host: HWND, job: Job) {
    let _busy = BusyGuard;
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
        let _ = unsafe { active.QueryContextMenu(hmenu, 0, SHELL_FIRST, SHELL_LAST, flags) };
        filter_hidden(hmenu, Some(active), &job.hidden_verbs);
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
    let flags = TPM_RETURNCMD.0 | TPM_RIGHTBUTTON.0;
    let chosen = unsafe { TrackPopupMenuEx(hmenu, flags, point.x, point.y, host, None) }.0 as u32;
    ACTIVE.with(|active| *active.borrow_mut() = None);
    crate::logv!(
        "[menu] shell={} count={} app={} hidden={} chosen={}",
        menu.is_some(),
        unsafe { GetMenuItemCount(Some(hmenu)) },
        map.len(),
        job.hidden_verbs.join(","),
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
                            let _ = invoke_shell(active, host, chosen, point);
                            (job.sink)(MenuAction::ShellOpEnd);
                            (job.sink)(MenuAction::Refresh);
                        }
                    }
                }
                _ => {
                    (job.sink)(MenuAction::ShellOpBegin);
                    let _ = invoke_shell(active, host, chosen, point);
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
struct BusyGuard;
impl Drop for BusyGuard {
    fn drop(&mut self) {
        BUSY.store(false, Ordering::Release);
    }
}
