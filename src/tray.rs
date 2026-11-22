//! 守护常驻托盘：UI 销毁后仍留在系统托盘。
//!
//! - 左键单击托盘图标 → 唤醒/重建主窗口。
//! - 右键菜单：`打开 Rove` / `退出`。
//! - 事件经全局通道由独立线程轮询，UI 操作一律
//!   `invoke_from_event_loop` 投递回事件循环线程。

use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

/// 托盘菜单动作 ID。
const ID_OPEN: &str = "rove-open";
const ID_QUIT: &str = "rove-quit";

/// 在专用线程上创建托盘图标，并跑该线程的消息循环。
///
/// **不能**在 UI 线程上建：`tray-icon` 把图标的隐藏窗口建在**调用线程**上，
/// 右键菜单的 `TrackPopupMenu` 也是在这个线程里跑**模态循环**
/// （见 tray-icon-0.21.3 `platform_impl/windows/mod.rs` 的 `show_tray_menu`）。
/// 一旦这个线程是 Slint 的事件循环线程，右键菜单一弹就会把定时器和
/// `invoke_from_event_loop` 全部冻住 —— 表现是「托盘点了没反应、窗口不显示，
/// 日志里只有 `[tray] 左键点击` 而没有 `[wake]`」。
pub fn spawn() {
    let spawned = std::thread::Builder::new()
        .name("rove-tray-host".into())
        .spawn(|| {
            // 图标句柄一直在本线程栈上：线程结束（= 进程结束）才移除。
            let _tray = create();
            run_message_loop();
        });
    if let Err(e) = spawned {
        logln!("tray host thread SPAWN FAILED: {e}");
    }
}

/// 本线程的消息循环：托盘的隐藏窗口与右键菜单都靠它。
#[cfg(windows)]
fn run_message_loop() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetMessageW, TranslateMessage, MSG,
    };
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

#[cfg(not(windows))]
fn run_message_loop() {}

/// 创建托盘图标（守护生命周期）。失败仅记录，不影响主流程。
fn create() -> Option<TrayIcon> {
    let menu = Menu::new();
    let _ = menu.append_items(&[
        &MenuItem::with_id(ID_OPEN, "打开 Rove", true, None),
        &MenuItem::with_id(ID_QUIT, "退出", true, None),
    ]);

    let icon = load_icon();
    let mut builder = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("Rove 文件管理器")
        .with_menu_on_left_click(false);
    if let Ok(icon) = icon {
        builder = builder.with_icon(icon);
    }
    match builder.build() {
        Ok(tray) => {
            // 隐藏窗口刚刚建出来：把 Rove 的深浅主题补挂上去（`apply_theme` 早于
            // 本线程时那次调用找不到窗口，只能由这边补一次），否则托盘菜单会
            // 退回「跟随系统」，与 Rove 的主题对不上。
            crate::sys::menu::apply_tray_theme();
            start_event_forwarder();
            Some(tray)
        }
        Err(e) => {
            logln!("tray build failed: {e}");
            None
        }
    }
}

/// 从 assets/tray.rgba（w,h + RGBA）加载托盘图标。
fn load_icon() -> Result<tray_icon::Icon, String> {
    const DATA: &[u8] = include_bytes!("../assets/tray.rgba");
    if DATA.len() < 8 {
        return Err("tray.rgba truncated".into());
    }
    let w = u32::from_le_bytes(DATA[0..4].try_into().unwrap());
    let h = u32::from_le_bytes(DATA[4..8].try_into().unwrap());
    let pixels = &DATA[8..];
    if pixels.len() != (w * h * 4) as usize {
        return Err("tray.rgba size mismatch".into());
    }
    tray_icon::Icon::from_rgba(pixels.to_vec(), w, h).map_err(|e| e.to_string())
}

/// 事件转发线程：菜单/点击 → 事件循环线程。
fn start_event_forwarder() {
    // 菜单事件（阻塞接收）。
    std::thread::Builder::new()
        .name("rove-tray-menu".into())
        .spawn(move || {
            let rx = MenuEvent::receiver();
            while let Ok(ev) = rx.recv() {
                match ev.id.as_ref() {
                    ID_QUIT => {
                        logv!("[tray] 菜单退出");
                        if let Err(e) = slint::invoke_from_event_loop(|| {
                            // 退出是最终决定：先打标记，免得销毁会话时又拉起接班进程。
                            crate::app::note_quitting();
                            destroy_session_public();
                            let _ = slint::quit_event_loop();
                        }) {
                            // 事件循环已经停了：这次点击没有任何人能处理。
                            logal!("[tray] !! 菜单退出投递失败：{e}（进程可能已僵死）");
                        }
                    }
                    _ => {
                        // 打开 Rove。
                        logv!("[tray] 菜单打开 (pid={})", std::process::id());
                        wake_on_ui_thread();
                    }
                }
            }
        })
        .ok();
    // 托盘图标点击（阻塞接收）。
    std::thread::Builder::new()
        .name("rove-tray-click".into())
        .spawn(move || {
            let rx = TrayIconEvent::receiver();
            while let Ok(ev) = rx.recv() {
                if let TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } = ev
                {
                    logv!("[tray] 左键点击 -> 唤起窗口 (pid={})", std::process::id());
                    wake_on_ui_thread();
                }
            }
        })
        .ok();
}

/// 把「唤起窗口」投递给事件循环线程。
///
/// 投递失败意味着事件循环已经停了（或从未启动）：这时点击托盘**永远**没有反应，
/// 而日志里也只剩这条 `[tray]` 记录 —— 一定要把失败原因写下来，否则
/// "点了没反应但日志有输出" 无从判断到底是投递丢了还是窗口没显出来。
fn wake_on_ui_thread() {
    match slint::invoke_from_event_loop(|| wake_session_public(TRAY_WAKE.clone())) {
        // 投递成功、但事件循环没跑起来时，这条日志后面就不会再出现 `[wake]`。
        // 「投递成功 + 没有 [wake]」= 事件循环收不到活，问题不在托盘这一侧。
        Ok(()) => logv!("[tray] 唤起已投递 (pid={})", std::process::id()),
        Err(e) => logal!("[tray] !! 唤起请求投递失败：{e}（事件循环未运行）"),
    }
}

/// 托盘「打开 Rove」：只唤起窗口，不导航、不新开标签。
const TRAY_WAKE: crate::daemon::WakeRequest = crate::daemon::WakeRequest {
    path: None,
    new_tab: false,
    select: None,
};

// 由 app.rs 注入的两个入口（避免 daemon ↔ app 循环依赖）。
pub use crate::app::{destroy_session as destroy_session_public, wake_session as wake_session_public};

// logln/logv 由 app.rs 的宏导出（macro_export）。
use crate::{logal, logln, logv};
