#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod actions;
mod app;
mod archive_sync;
mod archivers;
mod index;
mod browse;
mod diff;
mod drives;
mod matcher;
mod ops;
mod preview;
mod platform;
mod settings;
mod shortcut;
mod staging;
mod thumbnails;
mod wstr;

#[cfg(windows)]
mod ipc;
#[cfg(windows)]
mod tray;

fn main() {
    #[cfg(windows)]
    windows_main();

    #[cfg(not(windows))]
    {
        // 非 Windows 平台仅做开发期编译校验，直接拉起界面
        app::handle(app::Command::ShowWindow);
        slint::run_event_loop_until_quit().expect("event loop error");
    }
}

/// Windows 入口：单实例判定 -> 守护进程 -> UI 生命周期
#[cfg(windows)]
fn windows_main() {
    // 启动校验：探测压缩软件并同步 actions.toml（保留排版与注释）
    archive_sync::bootstrap();

    let args: Vec<String> = std::env::args().skip(1).collect();

    // 已有守护进程时作为客户端：发送请求后立即退出
    if let Some(request) = ipc::Request::from_args(&args) {
        if ipc::try_send(&request) {
            return;
        }
    }

    // 首次启动：成为守护进程
    if settings::load().show_tray {
        tray::spawn();
    }

    std::thread::spawn(|| {
        ipc::serve(|request| {
            let command = match request {
                ipc::Request::Open(path) => app::Command::Open(path),
                ipc::Request::Show => app::Command::ShowWindow,
                ipc::Request::Settings => app::Command::ShowSettings,
                ipc::Request::Quit => app::Command::Quit,
            };
            app::post(command);
        });
    });

    app::handle(app::Command::ShowWindow);
    slint::run_event_loop_until_quit().expect("event loop error");
}
