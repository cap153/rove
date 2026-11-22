//! 只读文件映射：把索引文件"映射"进地址空间，用到哪页读哪页。
//!
//! 这是"索引再大也不占内存"的关键。整份数据留在磁盘上，Windows 只在查询
//! 真正碰到某一页时才把它送进物理内存，内存紧张时可以随时回收 —— 常驻的
//! 只有最近查过的一小撮页，且计在系统文件缓存名下，不是进程的私有内存。
//!
//! 非 Windows 平台（交叉编译检查 / 测试）退化为整读进内存：那些平台没有
//! 我们关心的内存问题，保持实现简单。

use std::path::Path;

/// 一份只读映射。
pub struct Mapped {
    bytes: Storage,
    len: usize,
}

enum Storage {
    /// 映射视图基址。`MEMORY_MAPPED_VIEW_ADDRESS` 内部是裸指针（非 Send/Sync），
    /// 这里只存地址，解除映射时再装回句柄结构。
    #[cfg(windows)]
    View(usize),
    #[cfg(not(windows))]
    Heap(Vec<u8>),
}

// SAFETY: 视图是只读的，且一页一行都不会被写；多线程同时读同一张只读视图是
// 安全的（`&[u8]` 切片的共享访问）。解除映射只发生在本对象 Drop 时，此时不再
// 有其它引用（`View` 被 `RwLock` 独占）。
unsafe impl Send for Mapped {}
unsafe impl Sync for Mapped {}

impl Mapped {
    /// 映射（或读取）整个文件。
    pub fn open(path: &Path) -> std::io::Result<Option<Self>> {
        Self::open_inner(path).map_err(|e| {
            std::io::Error::new(e.kind(), format!("map {}: {e}", path.display()))
        })
    }

    #[cfg(windows)]
    fn open_inner(path: &Path) -> std::io::Result<Option<Self>> {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::Foundation::{CloseHandle, HANDLE};
        use windows::Win32::System::Memory::{
            CreateFileMappingW, MapViewOfFile, FILE_MAP_READ, PAGE_READONLY,
        };

        let file = std::fs::File::open(path)?;
        let len = file.metadata()?.len() as usize;
        if len == 0 {
            return Ok(None);
        }
        // SAFETY: 句柄只在本次调用期间使用；映射成功后我们持有视图，直到 Drop
        // 才解除，期间文件对象可以关闭（视图自己持有引用）。
        unsafe {
            let mapping = match CreateFileMappingW(
                HANDLE(file.as_raw_handle() as _),
                None,
                PAGE_READONLY,
                0,
                0,
                None,
            ) {
                Ok(m) => m,
                Err(_) => return Ok(None),
            };
            let view = MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, 0);
            let _ = CloseHandle(mapping);
            if view.Value.is_null() {
                return Ok(None);
            }
            Ok(Some(Self {
                bytes: Storage::View(view.Value as usize),
                len,
            }))
        }
    }

    #[cfg(not(windows))]
    fn open_inner(path: &Path) -> std::io::Result<Option<Self>> {
        let bytes = std::fs::read(path)?;
        if bytes.is_empty() {
            return Ok(None);
        }
        let len = bytes.len();
        Ok(Some(Self { bytes: Storage::Heap(bytes), len }))
    }

    /// 映射区字节（长度与文件一致）。
    pub fn bytes(&self) -> &[u8] {
        match &self.bytes {
            #[cfg(windows)]
            // SAFETY: 视图覆盖 `len` 字节且在本对象存活期间有效；只读访问。
            Storage::View(base) => unsafe {
                std::slice::from_raw_parts(*base as *const u8, self.len)
            },
            #[cfg(not(windows))]
            Storage::Heap(bytes) => bytes,
        }
    }
}

impl Drop for Mapped {
    fn drop(&mut self) {
        #[cfg(windows)]
        let Storage::View(base) = self.bytes;
            use windows::Win32::System::Memory::{UnmapViewOfFile, MEMORY_MAPPED_VIEW_ADDRESS};
            // SAFETY: 这张视图是本对象创建、且只解除一次。
        unsafe {
            let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                Value: base as *mut core::ffi::c_void,
            });
        }
    }
}
