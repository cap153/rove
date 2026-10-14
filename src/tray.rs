//! Windows 系统托盘：常驻图标与菜单，转发到 Slint 主线程。
//!
//! 两个后台线程：
//! 1. 托盘窗口线程：创建隐藏窗口，注册托盘图标并跑消息循环；
//! 2. 转发线程：把托盘命令投递到 Slint 事件循环（主线程）。

use std::sync::OnceLock;
use std::sync::mpsc::{self, Receiver, Sender};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_SETVERSION,
    NOTIFYICON_VERSION_4, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DispatchMessageW,
    GetCursorPos, GetMessageW, IDI_APPLICATION, LoadIconW, MF_STRING, MSG, RegisterClassW,
    SetForegroundWindow, TPM_BOTTOMALIGN, TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage,
    WINDOW_EX_STYLE, WINDOW_STYLE, WNDCLASSW, WS_OVERLAPPED, WM_APP, WM_COMMAND, WM_CONTEXTMENU,
    WM_LBUTTONDBLCLK, WM_RBUTTONUP,
};

use crate::wstr::{pcw, wide};

const WM_APP_TRAY: u32 = WM_APP + 1;

const ID_TRAY_OPEN: usize = 1001;
const ID_TRAY_SETTINGS: usize = 1002;
const ID_TRAY_QUIT: usize = 1003;

const TRAY_CLASS: &str = "RoveTrayHostClass";

/// 托盘交互产生的应用命令
#[derive(Debug, Clone, Copy)]
pub enum TrayCommand {
    OpenWindow,
    OpenSettings,
    Quit,
}

impl From<TrayCommand> for crate::app::Command {
    fn from(command: TrayCommand) -> Self {
        match command {
            TrayCommand::OpenWindow => crate::app::Command::ShowWindow,
            TrayCommand::OpenSettings => crate::app::Command::ShowSettings,
            TrayCommand::Quit => crate::app::Command::Quit,
        }
    }
}

static TRAY_TX: OnceLock<Sender<TrayCommand>> = OnceLock::new();

pub fn spawn() {
    let (tx, rx) = mpsc::channel::<TrayCommand>();
    let _ = TRAY_TX.set(tx);

    std::thread::spawn(run_tray_thread);
    std::thread::spawn(move || forward_commands(rx));
}

fn send(command: TrayCommand) {
    if let Some(tx) = TRAY_TX.get() {
        let _ = tx.send(command);
    }
}

/// 把托盘命令投递进 Slint 事件循环
fn forward_commands(rx: Receiver<TrayCommand>) {
    for command in rx {
        crate::app::post(command.into());
    }
}

unsafe extern "system" fn tray_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        match msg {
            WM_APP_TRAY => {
                // lParam 低位是鼠标事件
                let event = (lparam.0 as u32) & 0xFFFF;
                match event {
                    WM_LBUTTONDBLCLK => send(TrayCommand::OpenWindow),
                    WM_RBUTTONUP | WM_CONTEXTMENU => show_context_menu(hwnd),
                    _ => {}
                }
                LRESULT(0)
            }
            WM_COMMAND => {
                let id = wparam.0 & 0xFFFF;
                match id {
                    ID_TRAY_OPEN => send(TrayCommand::OpenWindow),
                    ID_TRAY_SETTINGS => send(TrayCommand::OpenSettings),
                    ID_TRAY_QUIT => send(TrayCommand::Quit),
                    _ => {}
                }
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

unsafe fn show_context_menu(hwnd: HWND) {
    unsafe {
        let menu = match CreatePopupMenu() {
            Ok(menu) => menu,
            Err(_) => return,
        };
        let _ = AppendMenuW(menu, MF_STRING, ID_TRAY_OPEN, pcw(&wide("Open main window")));
        let _ = AppendMenuW(menu, MF_STRING, ID_TRAY_SETTINGS, pcw(&wide("Settings")));
        let _ = AppendMenuW(menu, MF_STRING, ID_TRAY_QUIT, pcw(&wide("Exit completely")));

        let mut point = POINT::default();
        let _ = GetCursorPos(&mut point);
        // 必须先让宿主窗口获得前台状态，否则菜单不会自动消失
        let _ = SetForegroundWindow(hwnd);
        let _ = TrackPopupMenu(
            menu,
            TPM_RIGHTBUTTON | TPM_BOTTOMALIGN,
            point.x,
            point.y,
            None,
            hwnd,
            None,
        );
        let _ = DestroyMenu(menu);
    }
}

unsafe fn add_tray_icon(hwnd: HWND) {
    unsafe {
        let mut data: NOTIFYICONDATAW = std::mem::zeroed();
        data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = hwnd;
        data.uID = 1;
        data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        data.uCallbackMessage = WM_APP_TRAY;

        let Ok(icon) = LoadIconW(None, IDI_APPLICATION) else {
            return;
        };
        data.hIcon = icon;

        let tip = wide("Rove");
        let count = tip.len().min(127);
        data.szTip[..count].copy_from_slice(&tip[..count]);

        data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        let pointer = &data as *const NOTIFYICONDATAW;
        let _ = Shell_NotifyIconW(NIM_ADD, pointer);
        let _ = Shell_NotifyIconW(NIM_SETVERSION, pointer);
    }
}

fn run_tray_thread() {
    let Ok(instance) = (unsafe { GetModuleHandleW(None) }) else {
        return;
    };
    let class_name = wide(TRAY_CLASS);

    let window_class = WNDCLASSW {
        lpfnWndProc: Some(tray_wndproc),
        hInstance: instance.into(),
        lpszClassName: pcw(&class_name),
        ..unsafe { std::mem::zeroed() }
    };
    unsafe { RegisterClassW(&window_class) };

    let hwnd = match unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            pcw(&class_name),
            pcw(&wide("Rove")),
            WINDOW_STYLE(WS_OVERLAPPED.0),
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            None,
        )
    } {
        Ok(hwnd) => hwnd,
        Err(_) => return,
    };

    unsafe { add_tray_icon(hwnd) };

    let mut message = MSG::default();
    while unsafe { GetMessageW(&mut message, None, 0, 0) }.as_bool() {
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}
