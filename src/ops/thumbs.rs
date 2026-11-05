//! 缩略图管线（Phase 5，6.2）：Win32 `IShellItemImageFactory`。
//!
//! - 图片 / 视频文件与文件夹统一走系统合成（文件夹返回封面卡片）；
//! - 输出 32bpp BGRA 位图 → RGBA `SharedPixelBuffer`，供 Slint 直接显示；
//! - 调用方需在 worker 线程执行（本模块负责该线程的 COM 初始化）。

use std::path::Path;

/// 缩略图边长（与列表图标区域一致）。
pub const THUMB_SIZE: u32 = 44;

/// 是否值得为该路径取缩略图：
#[allow(dead_code)] // Linux 宿主测试构建不引用（Windows 侧使用）
/// - 图片 / 视频扩展名；
/// - 文件夹（系统会为含媒体的文件夹合成封面卡片，Explorer 同款）。
pub fn is_thumbnail_candidate(path: &Path) -> bool {
    if path.is_dir() {
        // 系统目录封面合成开销大且无意义。
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        return !matches!(
            name.as_str(),
            "windows" | "program files" | "program files (x86)" | "$recycle.bin"
                | "system volume information" | "appdata"
        );
    }
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    matches!(
        ext.as_str(),
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "tif" | "tiff" | "mp4" | "mkv"
            | "avi" | "mov" | "webm" | "wmv" | "m4v"
    )
}

/// 每线程一次的 COM 初始化（CoInitializeEx 幂等：已初始化返回 S_FALSE 也算成功）。
fn ensure_com() -> Result<(), String> {
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
    unsafe {
        // S_OK(0)/S_FALSE(1) 都算可用；RPC_E_CHANGED_MODE 时改用多线程模型重试。
        let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok();
        let _ = hr;
    }
    Ok(())
}

/// 取缩略图，返回 (宽, 高, RGBA 字节)。
/// COM 路径失败（受保护目录/文件访问被拒）时回退到系统图标，
/// 保证任何条目都有 Windows 图标可显示。
pub fn get_thumbnail(path: &Path, size: u32) -> Result<(u32, u32, Vec<u8>), String> {
    // 瞬时错误限次重试：启动初期 Shell 缩略图缓存尚未就绪，GetImage 会返回
    // 0x8000000A（E_PENDING，“完成该操作所需的数据还不可使用”）。这是可自愈的，
    // 重试几次即可成功。退避 250ms 起步、每次加倍（250/500/1000/2000/4000ms）。
    // 注意：重试必须放在这里（thumbs 模块）而非调用方，否则驱动器这类「永远
    // 拿不到 264px 缩略图」的条目会在调用方空转十几秒才落定，期间一直空白。
    const MAX_ATTEMPTS: u32 = 6;
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        match get_thumbnail_impl(path, size) {
            Ok(r) => {
                crate::logv!(
                    "[thumb] ok {} req_size={} attempt={} -> {}x{}",
                    path.display(),
                    size,
                    attempt,
                    r.0,
                    r.1
                );
                return Ok(r);
            }
            Err(e) => {
                let transient = e.contains("0x8000000A")
                    || e.contains("E_PENDING")
                    || e.contains("数据还不可使用");
                if transient && attempt < MAX_ATTEMPTS {
                    let wait = 250u64.saturating_mul(1 << (attempt - 1).min(5));
                    crate::logln!(
                        "[thumb] transient E_PENDING (attempt {}/{}) {} size={}: {} -> retry in {}ms",
                        attempt,
                        MAX_ATTEMPTS,
                        path.display(),
                        size,
                        e,
                        wait
                    );
                    std::thread::sleep(std::time::Duration::from_millis(wait));
                    continue;
                }
                crate::logln!(
                    "[thumb] impl FAILED {} req_size={} attempt={}: {}",
                    path.display(),
                    size,
                    attempt,
                    e
                );
                match get_system_icon(path) {
                    Some(r) => {
                        // 核心修复：如果降级返回的图标尺寸远小于请求尺寸
                        // （例如请求 264 返回 32），强行塞进大容器会因透明/缩放产生
                        // 不可见的白块。直接返回 Err，让上层走彩色方块（fallback）。
                        if r.0 * 2 < size || r.1 * 2 < size {
                            crate::logln!(
                                "[thumb] fallback system icon too small ({}x{}) for req_size={}, rejecting -> UI colored block",
                                r.0, r.1, size
                            );
                            return Err(e);
                        }
                        crate::logln!(
                            "[thumb] fallback system icon {} req_size={} -> {}x{}",
                            path.display(),
                            size,
                            r.0,
                            r.1
                        );
                        return Ok(r);
                    }
                    None => {
                        crate::logln!("[thumb] fallback system icon FAILED {}", path.display());
                        return Err(e);
                    }
                }
            }
        }
    }
}

fn get_thumbnail_impl(path: &Path, size: u32) -> Result<(u32, u32, Vec<u8>), String> {
    ensure_com()?;
    use windows::core::{PCWSTR, Interface};
    use windows::Win32::UI::Shell::{
        SHCreateItemFromParsingName, IShellItemImageFactory, SIIGBF_RESIZETOFIT,
    };
    use windows::Win32::Graphics::Gdi::{
        DeleteObject, GetDC, GetDIBits, ReleaseDC, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS,
    };

    let wide: Vec<u16> = path
        .as_os_str()
        .to_string_lossy()
        .encode_utf16()
        .chain([0])
        .collect();
    unsafe {
        let item: windows::Win32::UI::Shell::IShellItem =
            SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None)
                .map_err(|e| format!("SHCreateItemFromParsingName: {e}"))?;
        let factory: IShellItemImageFactory = item
            .cast()
            .map_err(|e| format!("cast to ImageFactory: {e}"))?;

        let size_h = windows::Win32::Foundation::SIZE {
            cx: size as i32,
            cy: size as i32,
        };
        let hbitmap = factory
            .GetImage(size_h, SIIGBF_RESIZETOFIT)
            .map_err(|e| format!("GetImage: {e}"))?;

        // 查询位图实际尺寸（SIIGBF 可能返回不同于请求的尺寸）。
        let mut bmp = windows::Win32::Graphics::Gdi::BITMAP::default();
        let got = windows::Win32::Graphics::Gdi::GetObjectW(
            hbitmap.into(),
            std::mem::size_of::<windows::Win32::Graphics::Gdi::BITMAP>() as i32,
            Some(&mut bmp as *mut _ as *mut _),
        );
        if got == 0 || bmp.bmWidth <= 0 || bmp.bmHeight <= 0 {
            let _ = DeleteObject(hbitmap.into());
            return Err("GetObjectW failed".into());
        }
        let (bw, bh) = (bmp.bmWidth as u32, bmp.bmHeight as u32);

        // 提取 32bpp 像素（top-down）。
        let hdc = GetDC(None);
        let mut bmi = BITMAPINFO::default();
        bmi.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: bw as i32,
            biHeight: -(bh as i32), // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: 0, // BI_RGB
            ..Default::default()
        };
        let mut buf = vec![0u8; (bw * bh * 4) as usize];
        let lines = GetDIBits(
            hdc,
            hbitmap,
            0,
            bh,
            Some(buf.as_mut_ptr().cast()),
            &mut bmi,
            DIB_RGB_COLORS,
        );
        ReleaseDC(None, hdc);
        let _ = DeleteObject(hbitmap.into());
        if lines == 0 {
            return Err("GetDIBits failed".into());
        }
        // BGRA → RGBA。
        for px in buf.chunks_exact_mut(4) {
            px.swap(0, 2);
        }
        drop(item);
        Ok((bw, bh, buf))
    }
}

/// 系统图标兜底：`SHGetFileInfoW`（不打开文件，受保护路径也能拿到图标）。
/// 返回 (宽, 高, RGBA)。尺寸为系统大图标 32px，显示端按需拉伸。
pub fn get_system_icon(path: &Path) -> Option<(u32, u32, Vec<u8>)> {
    use windows::core::PCWSTR;
    use windows::Win32::Graphics::Gdi::{
        DeleteObject, GetDC, GetDIBits, ReleaseDC, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS,
    };
    use windows::Win32::UI::Shell::{
        SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHGFI_USEFILEATTRIBUTES,
    };
    use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, ICONINFO};

    ensure_com().ok()?;
    let wide: Vec<u16> = path
        .as_os_str()
        .to_string_lossy()
        .encode_utf16()
        .chain([0])
        .collect();
    let size = 32u32;
    // 目录 / 盘根：不要传 FILE_ATTRIBUTE_NORMAL（那是“普通文件”），
    // 用真实属性 + SHGFI_USEFILEATTRIBUTES，才能拿到文件夹/驱动器图标；
    // 并且 SHGetFileInfoW 在 Shell 初始化早期也可能瞬时失败，做限次重试。
    let is_dir = std::fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false);
    let file_attrs = if is_dir {
        windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY
    } else {
        windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL
    };
    unsafe {
        let mut info = SHFILEINFOW::default();
        let mut ret = 0;
        // 最多 5 次，250ms 起步指数退避（覆盖 Shell 就绪前的窗口期）。
        for attempt in 0..5u32 {
            ret = SHGetFileInfoW(
                PCWSTR(wide.as_ptr()),
                file_attrs,
                Some(&mut info as *mut _ as *mut _),
                std::mem::size_of::<SHFILEINFOW>() as u32,
                SHGFI_ICON | SHGFI_LARGEICON | SHGFI_USEFILEATTRIBUTES,
            );
            if ret != 0 && !info.hIcon.is_invalid() {
                if attempt > 0 {
                    crate::logln!(
                        "[sysicon] retry OK on attempt {} {}",
                        attempt + 1,
                        path.display()
                    );
                }
                break;
            }
            if attempt < 4 {
                crate::logln!(
                    "[sysicon] attempt {} failed for {} -> retry",
                    attempt + 1,
                    path.display()
                );
                std::thread::sleep(std::time::Duration::from_millis(250 << attempt));
            }
        }
        if ret == 0 || info.hIcon.is_invalid() {
            return None;
        }
        let hicon = info.hIcon;

        let mut ii = ICONINFO::default();
        if GetIconInfo(hicon, &mut ii).is_err() {
            let _ = DestroyIcon(hicon);
            return None;
        }
        let hbm = ii.hbmColor;
        if hbm.is_invalid() {
            let _ = DestroyIcon(hicon);
            let _ = DeleteObject(ii.hbmMask.into());
            return None;
        }

        let hdc = GetDC(None);
        let mut bmi = BITMAPINFO::default();
        bmi.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: size as i32,
            biHeight: -(size as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: 0,
            ..Default::default()
        };
        let mut buf = vec![0u8; (size * size * 4) as usize];
        let lines = GetDIBits(
            hdc,
            hbm,
            0,
            size,
            Some(buf.as_mut_ptr().cast()),
            &mut bmi,
            DIB_RGB_COLORS,
        );
        ReleaseDC(None, hdc);
        let _ = DeleteObject(hbm.into());
        let _ = DeleteObject(ii.hbmMask.into());
        let _ = DestroyIcon(hicon);
        if lines == 0 {
            return None;
        }
        // BGRA→RGBA + 反预乘（HICON 位图为预乘 alpha）。
        for px in buf.chunks_exact_mut(4) {
            let a = px[3] as u32;
            if a > 0 && a < 255 {
                px[0] = ((px[0] as u32 * 255) / a).min(255) as u8;
                px[1] = ((px[1] as u32 * 255) / a).min(255) as u8;
                px[2] = ((px[2] as u32 * 255) / a).min(255) as u8;
            }
            px.swap(0, 2);
        }
        Some((size, size, buf))
    }
}
