//! 深色菜单（uxtheme 的未公开序号；任何一步拿不到就整体退回浅色）。
//!
//! 父进程（托盘菜单）与菜单子进程（原生菜单）都要用，所以单独放一个模块。
//! 两边各有一份自己的原子量：深色偏好是**进程级**的，子进程必须自己设一遍。

use std::sync::atomic::{AtomicBool, Ordering};

use windows::core::PCSTR;
use windows::Win32::Foundation::HWND;

/// 上一次设给 uxtheme 的深浅（避免重复调 `SetPreferredAppMode`）。
static MODE_DARK: AtomicBool = AtomicBool::new(false);
/// `SetPreferredAppMode` 是否已经设过。它的默认值是「跟随系统」——
/// 只要没设过，Shell 弹的任何菜单都按系统走，而不是按 Rove 的主题。
static MODE_SET: AtomicBool = AtomicBool::new(false);

/// 取 `uxtheme.dll` 里的未公开导出（按序号）。
///
/// 序号：133 `AllowDarkModeForWindow` / 135 `SetPreferredAppMode` /
/// 136 `FlushMenuThemes`。这几个接口微软从 Win10 1809 起就一直有，但从未
/// 公开——所以全程尽力而为：取不到就是浅色，绝不让菜单因此弹不出来。
fn uxtheme_export(ordinal: usize) -> Option<unsafe extern "system" fn() -> isize> {
    use windows::core::w;
    use windows::Win32::System::LibraryLoader::{
        GetModuleHandleW, GetProcAddress, LoadLibraryW,
    };
    unsafe {
        let module = match GetModuleHandleW(w!("uxtheme.dll")) {
            Ok(module) => module,
            Err(_) => LoadLibraryW(w!("uxtheme.dll")).ok()?,
        };
        GetProcAddress(module, PCSTR(ordinal as *const u8))
    }
}

/// 进程级深浅偏好（`SetPreferredAppMode`，0=Default 1=AllowDark 2=ForceDark 3=ForceLight）。
pub(super) fn set_preferred_mode(dark: bool) -> bool {
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

/// `SetPreferredAppMode` 是否已经显式设过。
pub(super) fn mode_set() -> bool {
    MODE_SET.load(Ordering::Acquire)
}

/// 让菜单主题缓存失效，下一次弹出才吃新设置。
pub(super) fn flush_menu_themes() {
    if let Some(export) = uxtheme_export(136) {
        let flush: unsafe extern "system" fn() = unsafe { std::mem::transmute(export) };
        unsafe { flush() };
    }
}

/// 给任意窗口挂上（或摘掉）深色主题。
pub(super) fn apply_dark_flag(hwnd: HWND, dark: bool) {
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
