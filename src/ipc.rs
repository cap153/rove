//! Windows 命名管道单实例机制。
//!
//! * 客户端（后续唤醒）：向管道发送目标路径并立即退出。
//! * 守护进程（首次启动）：常驻监听，把外部请求转发到 Slint 主线程。

use std::path::PathBuf;

use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FlushFileBuffers, ReadFile, WriteFile, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_MODE,
    OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_MESSAGE,
    PIPE_TYPE_MESSAGE, PIPE_WAIT, WaitNamedPipeW,
};

use crate::wstr::{pcw, wide};

pub const PIPE_NAME: &str = r"\\.\pipe\rove.daemon.v1";

/// 客户端 -> 守护进程 的请求
#[derive(Debug, Clone)]
pub enum Request {
    /// 打开（或激活）指向该路径的标签页
    Open(PathBuf),
    /// 显示主窗口
    Show,
    /// 打开设置
    Settings,
    /// 彻底退出守护进程
    Quit,
}

impl Request {
    pub fn encode(&self) -> String {
        match self {
            Request::Open(p) => format!("open\t{}", p.display()),
            Request::Show => "show".to_string(),
            Request::Settings => "settings".to_string(),
            Request::Quit => "quit".to_string(),
        }
    }

    pub fn decode(line: &str) -> Option<Request> {
        let line = line.trim();
        if line.is_empty() {
            return None;
        }
        let (verb, arg) = match line.split_once('\t') {
            Some((v, a)) => (v, Some(a)),
            None => (line, None),
        };
        match verb {
            "open" => arg
                .filter(|a| !a.is_empty())
                .map(|a| Request::Open(PathBuf::from(a))),
            "show" => Some(Request::Show),
            "settings" => Some(Request::Settings),
            "quit" => Some(Request::Quit),
            _ => None,
        }
    }

    /// 解析命令行参数，返回要发送给守护进程的请求
    pub fn from_args<I, S>(args: I) -> Option<Request>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut path: Option<PathBuf> = None;
        let mut flag: Option<Request> = None;
        for arg in args {
            match arg.as_ref() {
                "--quit" | "-q" => flag = Some(Request::Quit),
                "--settings" | "-s" => flag = Some(Request::Settings),
                other => {
                    let candidate = PathBuf::from(other);
                    if !candidate.as_os_str().is_empty() {
                        path = Some(candidate);
                    }
                }
            }
        }
        path.map(Request::Open).or(flag)
    }
}

/// 尝试以客户端身份把请求发给已运行的守护进程。
/// 返回 `true` 表示已有实例接管（调用方应立即退出）。
pub fn try_send(request: &Request) -> bool {
    let name = wide(PIPE_NAME);
    if !unsafe { WaitNamedPipeW(pcw(&name), 300) }.as_bool() {
        return false;
    }
    let handle = match unsafe {
        CreateFileW(
            pcw(&name),
            GENERIC_READ.0 | GENERIC_WRITE.0,
            FILE_SHARE_MODE(0),
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
    } {
        Ok(h) => h,
        Err(_) => return false,
    };
    let mut payload = request.encode().into_bytes();
    payload.push(b'\n');
    let mut written: u32 = 0;
    let ok = unsafe {
        WriteFile(
            handle,
            Some(payload.as_mut_slice()),
            Some(&mut written as *mut u32),
            None,
        )
    };
    let _ = unsafe { CloseHandle(handle) };
    ok.is_ok() && written as usize == payload.len()
}

/// 守护进程侧：常驻管道监听，把请求转交给 `handler`（在 IPC 线程调用）。
pub fn serve<F>(handler: F) -> !
where
    F: Fn(Request) + Send + 'static,
{
    let name = wide(PIPE_NAME);
    loop {
        let pipe = unsafe {
            let handle = CreateNamedPipeW(
                pcw(&name),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT,
                1,
                1024,
                1024,
                0,
                None,
            );
            if handle.is_invalid() {
                Err(windows::core::Error::from_thread())
            } else {
                Ok(handle)
            }
        };
        let pipe = match pipe {
            Ok(handle) => handle,
            Err(e) => {
                eprintln!("[rove] CreateNamedPipeW failed: {e}");
                std::thread::sleep(std::time::Duration::from_millis(500));
                continue;
            }
        };

        // 客户端在 CreateNamedPipe 与 ConnectNamedPipe 之间连入时返回
        // ERROR_PIPE_CONNECTED，属正常情况，直接忽略错误。
        let _ = unsafe { ConnectNamedPipe(pipe, None) };

        let mut buf = [0u8; 4096];
        let mut read: u32 = 0;
        let read_ok = unsafe {
            ReadFile(
                pipe,
                Some(buf.as_mut_slice()),
                Some(&mut read as *mut u32),
                None,
            )
        };
        if read_ok.is_ok() && read > 0 {
            let text = String::from_utf8_lossy(&buf[..read as usize]);
            for line in text.lines() {
                if let Some(request) = Request::decode(line) {
                    handler(request);
                }
            }
            let mut ack = *b"ok\n";
            let mut written: u32 = 0;
            unsafe {
                let _ = WriteFile(
                    pipe,
                    Some(ack.as_mut_slice()),
                    Some(&mut written as *mut u32),
                    None,
                );
                let _ = FlushFileBuffers(pipe);
            }
        }

        let _ = unsafe { DisconnectNamedPipe(pipe) };
        let _ = unsafe { CloseHandle(pipe) };
    }
}
