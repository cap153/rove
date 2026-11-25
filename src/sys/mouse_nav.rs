//! 鼠标侧键（XButton1 / XButton2）→ 后退 / 前进。
//!
//! Slint 的 `TouchArea` 只有左右中三个键，侧键根本不上报，所以这一层必须走 Win32：
//! 用低级鼠标钩子（`WH_MOUSE_LL`）截 `WM_XBUTTONDOWN` / `WM_XBUTTONUP`。
//!
//! 为什么不用「子类化窗口换 WndProc」：那要拿到 winit 内部的窗口过程并顶替它，
//! 一旦顺序/卸载时机出岔子，整个窗口的消息循环都会受影响。低级钩子只在钩子
//! 线程里跑，跟 Slint 自己的消息循环完全隔离。
//!
//! 两条必须守住的规则：
//!
//! - 钩子回调里**只做判断 + 投递**，不碰 I/O、不借 App：低级钩子超时（默认
//!   300 ms）会被系统静默摘掉，之后侧键就悄悄失效了。真正的导航交给
//!   `slint::invoke_from_event_loop` 回主线程做。
//! - 只有**鼠标正压在主窗口上**才受理（`WindowFromPoint` → `GetAncestor`）。
//!   否则在别的程序上按侧键，我们也会抢着导航。
//!
//! 受理时把 DOWN 与 UP 一起吞掉（返回 1），避免半套 XBUTTON 事件漏给其它处理者。

/// 主窗口句柄（0 = 还没登记）。
static TARGET_HWND: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

/// 钩子是否已经装上（重复 `init` 只更新窗口句柄，不再装第二个钩子）。
static INSTALLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 哪些侧键的「按下」被我们吞掉了（bit 见 [`button_bit`]）：抬起要跟着一起吞。
///
/// 必须记账，不能只看光标：按住侧键**在我们窗口外**松手（DOWN 已经漏给了别的
/// 程序），松开时不存在配对的 DOWN —— 无脑吞掉会害别的程序以为键还按着。
static SWALLOWED_DOWN: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// XBUTTON 编号：1 = 后退，2 = 前进（Windows 默认习惯）。
#[cfg(windows)]
const XBUTTON1: u16 = 0x0001;
#[cfg(windows)]
const XBUTTON2: u16 = 0x0002;

/// 登记主窗口并装钩子。可重复调用（换窗口时更新句柄即可）。
#[cfg(windows)]
pub fn init(hwnd: isize) {
    if hwnd == 0 {
        return;
    }
    TARGET_HWND.store(hwnd, std::sync::atomic::Ordering::Release);
    if INSTALLED.load(std::sync::atomic::Ordering::Acquire) {
        return;
    }
    // 钩子线程活到进程退出为止（卸钩子只会发生在进程结束），句柄不留也没关系。
    let (ready, started) = std::sync::mpsc::sync_channel::<Result<u32, String>>(1);
    let thread = match std::thread::Builder::new()
        .name("rove-mouse-nav".into())
        .stack_size(128 * 1024)
        .spawn(move || hook_thread(ready))
    {
        Ok(thread) => thread,
        Err(e) => {
            crate::logln!("!! 侧键导航：钩子线程起不来（{e}）");
            return;
        }
    };
    match started.recv_timeout(std::time::Duration::from_millis(500)) {
        Ok(Ok(thread_id)) => {
            INSTALLED.store(true, std::sync::atomic::Ordering::Release);
            crate::logln!("[mouse-nav] 侧键钩子已就绪 thread={thread_id}");
        }
        Ok(Err(e)) => {
            crate::logln!("!! 侧键导航：钩子安装失败（{e}）");
            let _ = thread.join();
        }
        Err(_) => crate::logln!("!! 侧键导航：钩子线程未就绪"),
    }
}

/// 非 Windows：没有侧键这回事。
#[cfg(not(windows))]
pub fn init(_hwnd: isize) {}

#[cfg(windows)]
fn hook_thread(ready: std::sync::mpsc::SyncSender<Result<u32, String>>) {
    use windows::Win32::Foundation::HINSTANCE;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetMessageW, PeekMessageW, SetWindowsHookExW, TranslateMessage, MSG,
        PM_NOREMOVE, WH_MOUSE_LL,
    };

    let mut message = MSG::default();
    // 线程消息队列要先建出来，否则 PostThreadMessage(WM_QUIT) 会丢。
    let _ = unsafe { PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE) };
    let hook = unsafe {
        GetModuleHandleW(None).and_then(|module| {
            SetWindowsHookExW(WH_MOUSE_LL, Some(callback), Some(HINSTANCE(module.0)), 0)
        })
    };
    let hook = match hook {
        Ok(hook) => hook,
        Err(e) => {
            let _ = ready.send(Err(e.to_string()));
            return;
        }
    };
    let _ = ready.send(Ok(unsafe {
        windows::Win32::System::Threading::GetCurrentThreadId()
    }));
    while unsafe { GetMessageW(&mut message, None, 0, 0) }.0 > 0 {
        let _ = unsafe { TranslateMessage(&message) };
        unsafe { DispatchMessageW(&message) };
    }
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::UnhookWindowsHookEx(hook);
    }
}

/// `MSLLHOOKSTRUCT.mouseData` → 方向（`Some(true)` = 前进）。
///
/// 侧键编号在**高 16 位**，低 16 位是保留位（滚轮量走的是别的消息）。
#[cfg(windows)]
fn button_direction(mouse_data: u32) -> Option<bool> {
    match ((mouse_data >> 16) & 0xFFFF) as u16 {
        XBUTTON1 => Some(false),
        XBUTTON2 => Some(true),
        _ => None,
    }
}

/// 同一个编号在 [`SWALLOWED_DOWN`] 里的位（0 = 不是侧键）。
#[cfg(windows)]
fn button_bit(mouse_data: u32) -> u8 {
    match ((mouse_data >> 16) & 0xFFFF) as u16 {
        XBUTTON1 => 1,
        XBUTTON2 => 2,
        _ => 0,
    }
}

#[cfg(windows)]
unsafe extern "system" fn callback(
    code: i32,
    message: windows::Win32::Foundation::WPARAM,
    data: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::Foundation::{HWND, LRESULT, POINT};
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, GetAncestor, MSLLHOOKSTRUCT, WindowFromPoint, GA_ROOT, WM_XBUTTONDBLCLK,
        WM_XBUTTONDOWN, WM_XBUTTONUP,
    };

    use std::sync::atomic::Ordering;

    let next = || unsafe { CallNextHookEx(None, code, message, data) };
    if code < 0 || data.0 == 0 {
        return next();
    }
    let id = message.0 as u32;
    // 连按两次侧键时，第二次 Windows 发的是 DBLCLK 而不是 DOWN：一样当按下处理，
    // 否则「快速按两下后退」只走一格。
    let press = id == WM_XBUTTONDOWN || id == WM_XBUTTONDBLCLK;
    if !press && id != WM_XBUTTONUP {
        return next();
    }
    let info = unsafe { &*(data.0 as *const MSLLHOOKSTRUCT) };
    let bit = button_bit(info.mouseData);
    if bit == 0 {
        return next();
    }
    if !press {
        // 抬起：只吞「按下被我们吞过」的那些，且不看光标位置（按下和松手之间
        // 完全可能已经移到窗口外）。
        return if SWALLOWED_DOWN.fetch_and(!bit, Ordering::AcqRel) & bit != 0 {
            LRESULT(1)
        } else {
            next()
        };
    }
    let target = TARGET_HWND.load(Ordering::Acquire);
    if target == 0 {
        return next();
    }
    let pt: POINT = info.pt;
    let root = unsafe { GetAncestor(WindowFromPoint(pt), GA_ROOT) };
    if root != HWND(target as *mut core::ffi::c_void) {
        return next();
    }
    let Some(forward) = button_direction(info.mouseData) else {
        return next();
    };
    SWALLOWED_DOWN.fetch_or(bit, Ordering::AcqRel);
    crate::app::mouse_side_nav(forward);
    LRESULT(1)
}

#[cfg(all(test, windows))]
mod tests {
    use super::{button_bit, button_direction};

    /// 侧键编号在高 16 位：XButton1 = 后退，XButton2 = 前进，其它一律不受理。
    #[test]
    fn side_buttons_map_to_back_and_forward() {
        assert_eq!(button_direction(0x0001_0000), Some(false));
        assert_eq!(button_direction(0x0002_0000), Some(true));
        assert_eq!(button_direction(0x0000_0001), None);
        assert_eq!(button_direction(0), None);
    }

    /// 两个侧键的记账位必须互不相同、且非侧键不占位（否则会互相清掉配额）。
    #[test]
    fn side_button_bits_are_distinct() {
        assert_eq!(button_bit(0x0001_0000), 1);
        assert_eq!(button_bit(0x0002_0000), 2);
        assert_ne!(button_bit(0x0001_0000), button_bit(0x0002_0000));
        assert_eq!(button_bit(0), 0);
        assert_eq!(button_bit(0x0003_0000), 0);
    }
}
