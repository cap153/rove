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

/// 创建托盘图标（守护生命周期）。失败仅记录，不影响主流程。
pub fn create() -> Option<TrayIcon> {
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
                        let _ = slint::invoke_from_event_loop(|| {
                            destroy_session_public();
                            let _ = slint::quit_event_loop();
                        });
                    }
                    _ => {
                        // 打开 Rove。
                        let _ = slint::invoke_from_event_loop(|| wake_session_public(None));
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
                    let _ = slint::invoke_from_event_loop(|| wake_session_public(None));
                }
            }
        })
        .ok();
}

// 由 app.rs 注入的两个入口（避免 daemon ↔ app 循环依赖）。
pub use crate::app::{destroy_session as destroy_session_public, wake_session as wake_session_public};

// logln 由 app.rs 的宏导出（macro_export）。
use crate::logln;
