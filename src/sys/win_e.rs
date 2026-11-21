//! Win+E 接管：低级键盘钩子（`WH_KEYBOARD_LL`）。
//!
//! 注册表那一层在 [`crate::sys::integration`]：把 Explorer 窗口 CLSID 的
//! `opennewwindow` 谓词指向 `rove.exe --win-e`。这里是第二层，解决注册表
//! 做不到的两件事：
//!
//! 1. **立即响应**：系统关联要先解析 CLSID 再起进程，钩子直接把按键吞掉、
//!    拉起一个 `--win-e` 实例（它自己会转发给已经在跑的守护进程）。
//! 2. **关联坏掉也能用**：关联指向的 exe 被删/改名后，按 Win+E 会弹
//!    「该文件没有与之关联的应用」（用户实测过）。钩子不依赖关联，照常工作。
//!
//! 三处必须守住，否则表现很怪：
//!
//! - 钩子回调里**绝不**碰 I/O、不建进程：低级钩子超时（默认 300 ms）会被
//!   Windows 静默摘掉，之后 Win+E 就悄悄失效。启动进程交给工作线程。
//! - 按下 Win+E 后必须「消费」这次 Windows 键：注入一个未分配的虚拟键
//!   `0xE8` 点一下，否则 Win 抬起会被系统当成单击开始菜单（开始菜单跟着弹）。
//! - Win 抬起时不能留下「裸露的 Win 抬起」：注入的三件事必须打成同一批
//!   输入（`0xE8 down` / `Win up` / `0xE8 up`），前台窗口才不会看到它。
//!
//! 注入的事件都带 [`TAG`]（`dwExtraInfo`），自己的钩子收到直接放行，
//! 否则会自己吞掉自己。

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::Mutex;
use std::thread::JoinHandle;

use crate::logln;

/// 注入事件的自证标记，自己的钩子靠它跳过。
#[cfg(windows)]
const TAG: usize = 0x5A53_4445_4d; // "ZSDEM"

/// 未分配的虚拟键：只用来「点一下」消费按住状态，本身没有含义。
const UNASSIGNED_VK: u16 = 0xE8;
/// `WM_MOUSELEAVE` 之外唯一关心的事：钩子是否还该生效。
static ENABLED: AtomicBool = AtomicBool::new(false);

struct HookThread {
    thread_id: u32,
    thread: JoinHandle<()>,
    requests: SyncSender<()>,
}

static HOOK: Mutex<Option<HookThread>> = Mutex::new(None);

/// 按「接管资源管理器」开关装 / 卸钩子。可重复调用。
pub fn sync(enabled: bool) {
    if enabled {
        start();
    } else {
        stop();
    }
}

#[cfg(windows)]
fn start() {
    let mut slot = HOOK.lock().unwrap_or_else(|e| e.into_inner());
    if slot.is_some() {
        ENABLED.store(true, Ordering::Release);
        return;
    }
    let (requests, incoming) = mpsc::sync_channel::<()>(4);
    let (ready, started) = mpsc::sync_channel::<Result<u32, String>>(1);
    let hook_requests = requests.clone();
    let thread = match std::thread::Builder::new()
        .name("rove-win-e-hook".into())
        .stack_size(256 * 1024)
        .spawn(move || hook_thread(incoming, hook_requests, ready))
    {
        Ok(thread) => thread,
        Err(e) => {
            logln!("!! Win+E 接管：钩子线程起不来（{e}）");
            return;
        }
    };
    // 钩子装不上（权限/别的软件）也不算致命错误：注册表那一层还在。
    match started.recv_timeout(std::time::Duration::from_millis(500)) {
        Ok(Ok(thread_id)) => {
            ENABLED.store(true, Ordering::Release);
            *slot = Some(HookThread {
                thread_id,
                thread,
                requests,
            });
            logln!("[win-e] 键盘钩子已就绪");
        }
        Ok(Err(e)) => {
            logln!("!! Win+E 接管：钩子安装失败（{e}）");
            let _ = thread.join();
        }
        Err(_) => {
            logln!("!! Win+E 接管：钩子线程未就绪");
        }
    }
}

#[cfg(not(windows))]
fn start() {}

#[cfg(windows)]
fn stop() {
    ENABLED.store(false, Ordering::Release);
    let taken = HOOK.lock().unwrap_or_else(|e| e.into_inner()).take();
    let Some(hook) = taken else { return };
    unsafe {
        windows::Win32::UI::WindowsAndMessaging::PostThreadMessageW(
            hook.thread_id,
            windows::Win32::UI::WindowsAndMessaging::WM_QUIT,
            windows::Win32::Foundation::WPARAM(0),
            windows::Win32::Foundation::LPARAM(0),
        )
        .ok();
    }
    // 关掉钩子就停一个纯消息循环线程，正常在 1 ms 内返回。
    let _ = hook.thread.join();
    drop(hook.requests);
    logln!("[win-e] 键盘钩子已卸下");
}

#[cfg(not(windows))]
fn stop() {}

/// 这次按键归我们管吗。
#[derive(Default)]
struct Chord {
    /// 已经吞下并受理过 Win+E：重复按下与抬起都要继续吞，否则一次按键会开两个窗口。
    captured: bool,
    /// 已经「消费」过的 Windows 键（bit0 左 Win，bit1 右 Win），抬起时要遮罩。
    masked_win: u8,
}

#[derive(Debug, PartialEq, Eq)]
enum Decision {
    Pass,
    Swallow,
    Launch,
}

impl Chord {
    fn mask_win(&mut self, left: bool, right: bool) {
        self.masked_win |= u8::from(left) | (u8::from(right) << 1);
    }

    /// 新的 Win 键按下：上一次的抬起如果被别的钩子吞掉，这里补齐，
    /// 否则之后每一次 Win 抬起都会被换成遮罩注入（表现为松手不再弹开始菜单，
    /// 而且每次多出一次无关按键）。只有「抬起丢失」时才走到这里。
    fn observe_win_down(&mut self, key: u32) {
        let bit = match key {
            0x5B => 1,
            0x5C => 2,
            _ => 0,
        };
        self.masked_win &= !bit;
    }

    /// Win 键抬起且已被消费 → 返回 true，调用方负责换成遮罩注入。
    fn take_masked_release(&mut self, key: u32, down: bool) -> bool {
        let bit = match key {
            0x5B => 1,
            0x5C => 2,
            _ => 0,
        };
        if down || bit == 0 || self.masked_win & bit == 0 {
            return false;
        }
        self.masked_win &= !bit;
        true
    }

    fn input(
        &mut self,
        key: u32,
        down: bool,
        win: bool,
        other_modifier: bool,
        enabled: bool,
    ) -> Decision {
        if key != 0x45 {
            return Decision::Pass;
        }
        if !down {
            return if std::mem::take(&mut self.captured) {
                Decision::Swallow
            } else {
                Decision::Pass
            };
        }
        if self.captured {
            return Decision::Swallow;
        }
        if win && !other_modifier && enabled {
            self.captured = true;
            Decision::Launch
        } else {
            Decision::Pass
        }
    }
}

struct Context {
    chord: Chord,
    requests: SyncSender<()>,
}

thread_local! {
    static CONTEXT: RefCell<Option<Context>> = const { RefCell::new(None) };
}

#[cfg(windows)]
fn hook_thread(incoming: mpsc::Receiver<()>, requests: SyncSender<()>, ready: SyncSender<Result<u32, String>>) {
    use windows::Win32::Foundation::HINSTANCE;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetMessageW, PeekMessageW, SetWindowsHookExW, TranslateMessage,
        UnhookWindowsHookEx, MSG, PM_NOREMOVE, WH_KEYBOARD_LL,
    };

    let spawned = std::thread::Builder::new()
        .name("rove-win-e-launch".into())
        .stack_size(256 * 1024)
        .spawn(move || {
            while incoming.recv().is_ok() {
                launch();
            }
        });
    if let Err(e) = spawned {
        logln!("!! Win+E 接管：启动线程起不来（{e}）");
        let _ = ready.send(Err(e.to_string()));
        return;
    }

    let mut message = MSG::default();
    // 线程消息队列要先建出来，否则 PostThreadMessage(WM_QUIT) 会丢。
    let _ = unsafe { PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE) };
    let hook = unsafe {
        GetModuleHandleW(None).and_then(|module| {
            SetWindowsHookExW(
                WH_KEYBOARD_LL,
                Some(callback),
                Some(HINSTANCE(module.0)),
                0,
            )
        })
    };
    let hook = match hook {
        Ok(hook) => hook,
        Err(e) => {
            let _ = ready.send(Err(e.to_string()));
            return;
        }
    };
    CONTEXT.with(|slot| {
        *slot.borrow_mut() = Some(Context {
            chord: Chord::default(),
            requests,
        })
    });
    let _ = ready.send(Ok(unsafe {
        windows::Win32::System::Threading::GetCurrentThreadId()
    }));
    while unsafe { GetMessageW(&mut message, None, 0, 0) }.0 > 0 {
        let _ = unsafe { TranslateMessage(&message) };
        unsafe { DispatchMessageW(&message) };
    }
    unsafe {
        let _ = UnhookWindowsHookEx(hook);
    }
    CONTEXT.with(|slot| *slot.borrow_mut() = None);
}

#[cfg(not(windows))]
fn hook_thread(_: mpsc::Receiver<()>, _: SyncSender<()>, _: SyncSender<Result<u32, String>>) {}

#[cfg(windows)]
fn key_input(vk: u16, flags: u32) -> windows::Win32::UI::Input::KeyboardAndMouse::INPUT {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        KEYBDINPUT, KEYEVENTF_KEYUP, INPUT, INPUT_0, INPUT_KEYBOARD, VIRTUAL_KEY,
    };
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                dwFlags: if flags == 0 {
                    Default::default()
                } else {
                    KEYEVENTF_KEYUP
                },
                dwExtraInfo: TAG,
                ..Default::default()
            },
        },
    }
}

#[cfg(windows)]
fn send(events: &[windows::Win32::UI::Input::KeyboardAndMouse::INPUT]) -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::{SendInput, INPUT};
    unsafe { SendInput(events, std::mem::size_of::<INPUT>() as i32) == events.len() as u32 }
}

/// 消费掉这次 Windows 键的按住状态，让系统不把 Win 抬起当成单击开始菜单。
#[cfg(windows)]
fn tap_unassigned() {
    let _ = send(&[key_input(UNASSIGNED_VK, 0), key_input(UNASSIGNED_VK, 1)]);
}

/// Win 抬起：同一批输入里先按下 0xE8，再抬起真的 Win 键，最后放开 0xE8。
#[cfg(windows)]
fn mask_win_release(key: u32) -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::KEYEVENTF_EXTENDEDKEY;
    let mut win_up = key_input(key as u16, 1);
    unsafe {
        win_up.Anonymous.ki.dwFlags |= KEYEVENTF_EXTENDEDKEY;
    }
    send(&[key_input(UNASSIGNED_VK, 0), win_up, key_input(UNASSIGNED_VK, 1)])
}

#[cfg(windows)]
unsafe extern "system" fn callback(
    code: i32,
    message: windows::Win32::Foundation::WPARAM,
    data: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::Foundation::LRESULT;
    use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, KBDLLHOOKSTRUCT, WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
    };

    let next = || unsafe { CallNextHookEx(None, code, message, data) };
    if code < 0 || data.0 == 0 {
        return next();
    }
    let id = message.0 as u32;
    let down = matches!(id, WM_KEYDOWN | WM_SYSKEYDOWN);
    if !down && !matches!(id, WM_KEYUP | WM_SYSKEYUP) {
        return next();
    }
    let key = unsafe { &*(data.0 as *const KBDLLHOOKSTRUCT) };
    if key.dwExtraInfo == TAG {
        return next();
    }
    let vk = key.vkCode;
    let masked = CONTEXT.with(|slot| {
        slot.try_borrow_mut()
            .ok()
            .and_then(|mut slot| slot.as_mut().map(|ctx| ctx.chord.take_masked_release(vk, down)))
            .unwrap_or(false)
    });
    if masked {
        // 注入失败绝不能把修饰键留在「物理按下」状态：放行原始抬起。
        return if mask_win_release(vk) {
            LRESULT(1)
        } else {
            next()
        };
    }
    if vk != 0x45 {
        if down && (vk == 0x5B || vk == 0x5C) {
            CONTEXT.with(|slot| {
                if let Ok(mut slot) = slot.try_borrow_mut() {
                    if let Some(ctx) = slot.as_mut() {
                        ctx.chord.observe_win_down(vk);
                    }
                }
            });
        }
        return next();
    }
    let pressed = |vk: i32| unsafe { GetAsyncKeyState(vk) < 0 };
    let win = pressed(0x5B) || pressed(0x5C);
    let other = pressed(0x10) || pressed(0x11) || pressed(0x12);
    let decision = CONTEXT.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(ctx) = slot.as_mut() else {
            return Decision::Pass;
        };
        let decision = ctx
            .chord
            .input(vk, down, win, other, ENABLED.load(Ordering::Acquire));
        if decision == Decision::Launch {
            // 受理不了就别吞：宁可让系统按原样处理，也不能吃掉按键什么也不做。
            if ctx.requests.try_send(()).is_err() {
                ctx.chord.captured = false;
                return Decision::Pass;
            }
            ctx.chord.mask_win(pressed(0x5B), pressed(0x5C));
        }
        decision
    });
    if decision == Decision::Launch {
        tap_unassigned();
    }
    if decision == Decision::Pass {
        next()
    } else {
        LRESULT(1)
    }
}

/// 拉起 `rove.exe --win-e`；失败就退回系统自己的「此电脑」窗口。
#[cfg(windows)]
fn launch() {
    use std::process::Command;

    // 守护进程就在本进程里时直接走管道：省掉一次进程启动（约 100 ms），
    // 窗口也就不会被「先起进程再转发」的顺序拖慢。管道是单实例的，服务端
    // 正忙着处理上一条时连不上 —— 这种情况得多试几次，否则会误判成
    // 「没有守护进程」而起一个新进程，结果跑出第二个实例（两个托盘、两个钩子）。
    let request = crate::daemon::WakeRequest {
        path: None,
        new_tab: true,
        select: None,
    };
    for attempt in 0..3 {
        if crate::daemon::try_forward(&request) {
            return;
        }
        if attempt < 2 {
            std::thread::sleep(std::time::Duration::from_millis(40));
        }
    }
    let spawned = std::env::current_exe().and_then(|exe| {
        Command::new(exe)
            .arg("--win-e")
            .spawn()
            .map(|_| ())
    });
    if spawned.is_ok() {
        return;
    }
    logln!("!! Win+E 打开 Rove 失败，退回系统资源管理器");
    let explorer = std::env::var("SystemRoot")
        .map(|root| format!("{root}\\explorer.exe"))
        .unwrap_or_else(|_| "explorer.exe".to_string());
    let _ = Command::new(explorer)
        .arg("/n,/e,::{20D04FE0-3AEA-1069-A2D8-08002B30309D}")
        .spawn();
}

#[cfg(not(windows))]
fn launch() {}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    fn armed() -> Chord {
        Chord::default()
    }

    #[test]
    fn win_e_launches_once_and_swallows_repeat_and_release() {
        let mut chord = armed();
        assert_eq!(chord.input(0x45, true, true, false, true), Decision::Launch);
        assert_eq!(chord.input(0x45, true, true, false, true), Decision::Swallow);
        assert_eq!(chord.input(0x45, false, false, false, true), Decision::Swallow);
        assert_eq!(chord.input(0x45, false, false, false, true), Decision::Pass);
        assert_eq!(chord.input(0x45, true, true, false, true), Decision::Launch);
    }

    #[test]
    fn other_chords_and_plain_e_are_left_alone() {
        let mut chord = armed();
        assert_eq!(chord.input(0x44, true, true, false, true), Decision::Pass);
        assert_eq!(chord.input(0x45, true, false, false, true), Decision::Pass);
        assert_eq!(chord.input(0x45, true, true, true, true), Decision::Pass);
        assert_eq!(chord.input(0x45, false, false, false, true), Decision::Pass);
    }

    #[test]
    fn disabled_flag_passes_everything() {
        let mut chord = armed();
        assert_eq!(chord.input(0x45, true, true, false, false), Decision::Pass);
        assert_eq!(chord.input(0x45, false, false, false, false), Decision::Pass);
    }

    #[test]
    fn a_lost_windows_release_is_forgiven_on_the_next_press() {
        let mut chord = armed();
        assert_eq!(chord.input(0x45, true, true, false, true), Decision::Launch);
        chord.mask_win(true, false);
        // 抬起被别的钩子吞掉：遮罩位留在那里。
        chord.observe_win_down(0x5B);
        assert!(!chord.take_masked_release(0x5B, false));
    }

    #[test]
    fn windows_release_is_masked_once_per_key() {
        let mut chord = armed();
        assert_eq!(chord.input(0x45, true, true, false, true), Decision::Launch);
        chord.mask_win(true, false);
        assert!(!chord.take_masked_release(0x5B, true));
        assert!(chord.take_masked_release(0x5B, false));
        assert!(!chord.take_masked_release(0x5B, false));
        assert!(!chord.take_masked_release(0x5C, false));
    }
}
