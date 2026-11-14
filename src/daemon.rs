//! 单实例守护：基于命名管道 `\\.\pipe\rove_ipc`。
//!
//! - 首个实例成为守护：创建管道服务端并在后台线程循环 accept；
//!   收到唤醒消息后经回调投递给 UI 线程。
//! - 后续实例作为客户端：把启动参数（目标路径）写入管道后立即退出，
//!   由守护进程接管显示。
//!
//! 消息格式：单条 JSON `{"paths":["C:\\a"],"new_tab":false}`（message 模式管道）。
//! 无连接时服务端阻塞在 ConnectNamedPipe，CPU 占用为 0。

use std::path::PathBuf;

/// IPC 管道名。
#[allow(dead_code)]
pub const PIPE_NAME: &str = "\\\\.\\pipe\\rove_ipc";

/// 一次打开请求：去哪里 + 是否必须新开标签。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WakeRequest {
    /// `None` = 此电脑（列举驱动器）。
    pub path: Option<PathBuf>,
    /// 资源管理器转来的请求新开标签，不覆盖用户当前正在看的目录。
    pub new_tab: bool,
}

/// 解析启动参数。
///
/// `--shell-open <路径>`（文件夹/磁盘）与 `--this-pc`（此电脑）是资源管理器
/// 转来的打开请求，一律新开标签；其他参数沿用旧行为：取第一个存在的目录，
/// 在当前标签里导航。
pub fn parse_args(args: &[String]) -> WakeRequest {
    if args.iter().any(|a| a == "--this-pc") {
        return WakeRequest {
            path: None,
            new_tab: true,
        };
    }
    if let Some(at) = args.iter().position(|a| a == "--shell-open") {
        // 没有后续参数（或后面紧跟另一个开关）说明参数不完整：这是坏请求，
        // 当成「什么都不打开」处理，绝不能退化成「打开此电脑」——那会在用户
        // 双击文件夹时凭空弹出一个此电脑标签。
        let path = args.get(at + 1).filter(|p| !p.starts_with("--"));
        return match path {
            Some(path) => WakeRequest {
                path: Some(PathBuf::from(path)),
                new_tab: true,
            },
            None => WakeRequest::default(),
        };
    }
    WakeRequest {
        path: args
            .iter()
            .map(PathBuf::from)
            .find(|p| p.is_dir()),
        new_tab: false,
    }
}

/// 把「抢前台」的许可移交给守护进程。
///
/// 资源管理器拉起本进程时，本进程带着前台权限；真正要显示窗口的是守护进程，
/// 而 Windows 默认不允许非前台进程 SetForegroundWindow（拒绝后任务栏按钮只会
/// 闪黄）。必须在转发之前由本进程（有权限的那个）把许可交出去。
#[cfg(windows)]
pub fn hand_over_foreground() {
    unsafe {
        // u32::MAX = ASFW_ANY：允许任何进程设置前台窗口。
        windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(u32::MAX);
    }
}

#[cfg(not(windows))]
pub fn hand_over_foreground() {}

/// 尝试把启动请求转发给已运行的守护进程。
/// 返回 `true` 表示转发成功（调用方应立即退出进程）。
pub fn try_forward(request: &WakeRequest) -> bool {
    let message = WakeMessage {
        paths: request
            .path
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
        new_tab: request.new_tab,
    };
    let Ok(payload) = serde_json::to_vec(&message) else {
        return false;
    };
    send_payload(&payload)
}

/// 唤醒消息。
#[derive(serde::Serialize, serde::Deserialize)]
struct WakeMessage {
    #[serde(default)]
    paths: Vec<String>,
    /// 旧版客户端不带这个字段，缺省按「在当前标签导航」处理。
    #[serde(default)]
    new_tab: bool,
}

/// 守护模式：启动管道服务线程。每收到一条消息回调一次 `on_wake`。
/// 注意：回调在管道线程执行，UI 操作须经 `slint::invoke_from_event_loop`。
pub fn spawn_server(on_wake: impl Fn(WakeRequest) + Send + 'static) -> bool {
    spawn_server_impl(on_wake)
}

// ---------------------------------------------------------------------------
// Windows 实现
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod imp {
    use super::{WakeMessage, PIPE_NAME};
    use std::path::PathBuf;

    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_MORE_DATA, ERROR_PIPE_CONNECTED, HANDLE,
        INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FlushFileBuffers, ReadFile, WriteFile, FILE_FLAG_FIRST_PIPE_INSTANCE,
        FILE_SHARE_NONE, OPEN_EXISTING, PIPE_ACCESS_INBOUND,
    };
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_MESSAGE,
        PIPE_TYPE_MESSAGE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    };

    const GENERIC_WRITE: u32 = 0x4000_0000;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    /// 创建一个服务端管道实例（首实例带 FILE_FLAG_FIRST_PIPE_INSTANCE，
    /// 若已存在同名管道则失败，用于单实例竞态裁决）。
    pub fn create_server_instance(first: bool) -> Result<HANDLE, ()> {
        let mut flags = PIPE_ACCESS_INBOUND;
        if first {
            flags |= FILE_FLAG_FIRST_PIPE_INSTANCE;
        }
        let name = wide(PIPE_NAME);
        let handle = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                flags,
                PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT,
                PIPE_UNLIMITED_INSTANCES,
                4096,
                4096,
                0,
                std::ptr::null(),
            )
        };
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return Err(());
        }
        Ok(handle)
    }

    /// 客户端：连接守护管道并写入一条消息。
    /// 服务端为 PIPE_ACCESS_INBOUND（只收不写），客户端必须只申请写权限。
    pub fn send_payload(payload: &[u8]) -> bool {
        let name = wide(PIPE_NAME);
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_WRITE,
                FILE_SHARE_NONE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return false;
        }
        let mut written = 0u32;
        let ok = unsafe {
            WriteFile(handle, payload.as_ptr(), payload.len() as u32, &mut written, std::ptr::null_mut())
        };
        if ok != 0 {
            unsafe {
                FlushFileBuffers(handle);
            }
        }
        unsafe {
            CloseHandle(handle);
        }
        ok != 0
    }

    /// 服务端：阻塞读取一条完整消息（message 模式，可能分多次到达）。
    fn read_message(handle: HANDLE) -> Option<Vec<u8>> {
        let mut buf = vec![0u8; 64 * 1024];
        let mut message = Vec::new();
        loop {
            let mut read = 0u32;
            let ok = unsafe {
                ReadFile(handle, buf.as_mut_ptr(), buf.len() as u32, &mut read, std::ptr::null_mut())
            };
            if ok != 0 {
                message.extend_from_slice(&buf[..read as usize]);
                break;
            }
            if unsafe { GetLastError() } == ERROR_MORE_DATA {
                message.extend_from_slice(&buf[..read as usize]);
                continue;
            }
            return None;
        }
        Some(message)
    }

    /// 跨线程传递的原始句柄（HANDLE 本身非 Send）。
    pub struct SendHandle(pub HANDLE);
    unsafe impl Send for SendHandle {}

    /// 服务端主循环。`first_handle` 为带 FIRST_PIPE_INSTANCE 标志的
    /// 首个实例（竞态裁决后直接复用，避免窗口期被抢占）。
    pub fn server_loop(
        mut first_handle: Option<SendHandle>,
        on_wake: Box<dyn Fn(super::WakeRequest) + Send>,
    ) {
        loop {
            let handle = match first_handle.take() {
                Some(h) => h.0,
                None => match create_server_instance(false) {
                    Ok(h) => h,
                    Err(()) => return, // 无法绑定（异常情况）时退出线程。
                },
            };
            // 阻塞等待客户端连接。
            let connected = unsafe { ConnectNamedPipe(handle, std::ptr::null_mut()) };
            if connected == 0 {
                let err = unsafe { GetLastError() };
                if err != ERROR_PIPE_CONNECTED {
                    unsafe {
                        CloseHandle(handle);
                    }
                    continue;
                }
            }
            if let Some(message) = read_message(handle) {
                if let Ok(msg) = serde_json::from_slice::<WakeMessage>(&message) {
                    let path = msg.paths.iter().map(PathBuf::from).find(|p| p.is_dir());
                    on_wake(super::WakeRequest {
                        path,
                        new_tab: msg.new_tab,
                    });
                }
            }
            unsafe {
                DisconnectNamedPipe(handle);
                CloseHandle(handle);
            }
        }
    }
}

#[cfg(windows)]
fn send_payload(payload: &[u8]) -> bool {
    imp::send_payload(payload)
}

#[cfg(windows)]
fn spawn_server_impl(on_wake: impl Fn(WakeRequest) + Send + 'static) -> bool {
    // 竞态裁决：首个实例创建带 FILE_FLAG_FIRST_PIPE_INSTANCE 的管道；
    // 失败说明守护已存在，由调用方转为客户端。
    let probe = match imp::create_server_instance(true) {
        Ok(h) => imp::SendHandle(h),
        Err(()) => return false,
    };
    let on_wake: Box<dyn Fn(WakeRequest) + Send> = Box::new(on_wake);
    std::thread::Builder::new()
        .name("rove-ipc".into())
        .spawn(move || {
            imp::server_loop(Some(probe), on_wake);
        })
        .expect("spawn rove-ipc");
    true
}

// ---------------------------------------------------------------------------
// 非 Windows 平台桩（开发/测试用）
// ---------------------------------------------------------------------------

#[cfg(not(windows))]
fn send_payload(_payload: &[u8]) -> bool {
    false
}

#[cfg(not(windows))]
fn spawn_server_impl(_on_wake: impl Fn(WakeRequest) + Send + 'static) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn shell_open_always_asks_for_a_new_tab() {
        let req = parse_args(&args(&["--shell-open", r"D:\重"]));
        assert_eq!(req.path, Some(PathBuf::from(r"D:\重")));
        assert!(req.new_tab);
    }

    #[test]
    fn this_pc_opens_a_new_tab_without_path() {
        let req = parse_args(&args(&["--this-pc"]));
        assert_eq!(req.path, None);
        assert!(req.new_tab);
    }

    #[test]
    fn malformed_shell_open_opens_nothing_instead_of_this_pc() {
        // 参数被截断时不能变成「打开此电脑」（那会在用户双击文件夹时凭空弹标签）。
        let req = parse_args(&args(&["--shell-open"]));
        assert_eq!(req, WakeRequest::default());
        let req = parse_args(&args(&["--shell-open", "--some-flag"]));
        assert_eq!(req, WakeRequest::default());
        // `--this-pc` 优先（它自带语义），不会被 --shell-open 吞掉。
        let req = parse_args(&args(&["--shell-open", "--this-pc"]));
        assert_eq!(
            req,
            WakeRequest {
                path: None,
                new_tab: true
            }
        );
    }

    #[test]
    fn plain_path_keeps_navigating_the_current_tab() {
        // 普通启动参数：只有确实存在的目录才算，且不新开标签。
        let here = std::env::temp_dir();
        let req = parse_args(&args(&[here.to_string_lossy().as_ref(), "--flag"]));
        assert_eq!(req.path.as_deref(), Some(here.as_path()));
        assert!(!req.new_tab);
        assert!(!parse_args(&args(&["--flag"])).new_tab);
    }
}
