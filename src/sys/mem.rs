//! 进程自身的内存用量，仅供 `ROVE_LOG=2` 的 `[mem]` 诊断日志使用。
//!
//! 为什么需要它：`[mem]` 里能数出来的只是「Rove 自己的缓存」（索引 / 缩略图 /
//! 目录条目），而任务管理器显示的数字还包含渲染后端缓冲、系统 DLL 映射、
//! 分配器碎片等一大块。把进程总量也打出来，才能一眼看出
//! 「缓存还有多少可省」和「已经到了框架地板」的区别。

/// 返回 `(工作集, 私有字节)`，取不到时 `None`。
///
/// 判断「这个程序到底占了多少」应看**工作集**（和任务管理器同一列）。
/// 私有字节（提交大小）会把历史上的峰值也算进来：实测 `Vec::with_capacity(200MB)`
/// 一个字节都没写，私有字节已经涨到 212 MB，工作集只有 7.6 MB —— 拿它衡量
/// 真实占用会得出完全错误的结论。
#[cfg(windows)]
pub fn usage() -> Option<(u64, u64)> {
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    // 纯 POD 结构，全零即合法初值（cb 随后写入）。
    let mut counters: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
    counters.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    let cb = counters.cb;
    let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, cb) };
    (ok != 0).then(|| {
        (
            counters.WorkingSetSize as u64,
            counters.PagefileUsage as u64,
        )
    })
}

#[cfg(not(windows))]
pub fn usage() -> Option<(u64, u64)> {
    None
}
